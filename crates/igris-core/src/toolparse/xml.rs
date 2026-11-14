//! XML-family parsers: xml tool calls, the function-attribute form,
//! and MiniMax invoke/parameter blocks.

use crate::toolparse::formats::map_tool_name_alias;
use crate::toolparse::protocol::ParsedToolCall;
use crate::toolparse::scan::extract_json_values;
use regex::Regex;
use std::sync::LazyLock;


pub(crate) fn is_xml_meta_tag(tag: &str) -> bool {
    let normalized = tag.to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "tool_call"
            | "toolcall"
            | "tool-call"
            | "invoke"
            | "thinking"
            | "thought"
            | "analysis"
            | "reasoning"
            | "reflection"
    )
}

/// Match opening XML tags: `<tag_name>`.  Does NOT use backreferences.
pub(crate) static XML_OPEN_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"<([a-zA-Z_][a-zA-Z0-9_-]*)>").expect("XML_OPEN_TAG_RE regex must compile")
});

/// MiniMax XML invoke format:
/// `<invoke name="shell"><parameter name="command">pwd</parameter></invoke>`
pub(crate) static MINIMAX_INVOKE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<invoke\b[^>]*\bname\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*>(.*?)</invoke>"#)
        .expect("MINIMAX_INVOKE_RE regex must compile")
});

pub(crate) static MINIMAX_PARAMETER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<parameter\b[^>]*\bname\s*=\s*(?:"([^"]+)"|'([^']+)')[^>]*>(.*?)</parameter>"#,
    )
    .expect("MINIMAX_PARAMETER_RE regex must compile")
});

/// Extracts all `<tag>…</tag>` pairs from `input`, returning `(tag_name, inner_content)`.
/// Handles matching closing tags without regex backreferences.
pub(crate) fn extract_xml_pairs(input: &str) -> Vec<(&str, &str)> {
    let mut results = Vec::new();
    let mut search_start = 0;
    while let Some(open_cap) = XML_OPEN_TAG_RE.captures(&input[search_start..]) {
        let Some(full_open) = open_cap.get(0) else {
            break;
        };
        let Some(tag_name) = open_cap.get(1) else {
            search_start += full_open.end();
            continue;
        };
        let tag_name = tag_name.as_str();
        let open_end = search_start + full_open.end();

        let closing_tag = format!("</{tag_name}>");
        if let Some(close_pos) = input[open_end..].find(&closing_tag) {
            let inner = &input[open_end..open_end + close_pos];
            results.push((tag_name, inner.trim()));
            search_start = open_end + close_pos + closing_tag.len();
        } else {
            search_start = open_end;
        }
    }
    results
}

/// Parse XML-style tool calls in `<tool_call>` bodies.
/// Supports both nested argument tags and JSON argument payloads:
/// - `<memory_recall><query>...</query></memory_recall>`
/// - `<shell>{"command":"pwd"}</shell>`
pub(crate) fn parse_xml_tool_calls(xml_content: &str) -> Option<Vec<ParsedToolCall>> {
    let mut calls = Vec::new();
    let trimmed = xml_content.trim();

    if !trimmed.starts_with('<') || !trimmed.contains('>') {
        return None;
    }

    for (tool_name_str, inner_content) in extract_xml_pairs(trimmed) {
        let tool_name = tool_name_str.to_string();
        if is_xml_meta_tag(&tool_name) {
            continue;
        }

        if inner_content.is_empty() {
            continue;
        }

        let mut args = serde_json::Map::new();

        if let Some(first_json) = extract_json_values(inner_content).into_iter().next() {
            match first_json {
                serde_json::Value::Object(object_args) => {
                    args = object_args;
                }
                other => {
                    args.insert("value".to_string(), other);
                }
            }
        } else {
            for (key_str, value) in extract_xml_pairs(inner_content) {
                let key = key_str.to_string();
                if is_xml_meta_tag(&key) {
                    continue;
                }
                if !value.is_empty() {
                    args.insert(key, serde_json::Value::String(value.to_string()));
                }
            }

            if args.is_empty() {
                args.insert(
                    "content".to_string(),
                    serde_json::Value::String(inner_content.to_string()),
                );
            }
        }

        calls.push(ParsedToolCall {
            name: map_tool_name_alias(&tool_name).to_string(),
            arguments: serde_json::Value::Object(args),
            tool_call_id: None,
        });
    }

    if calls.is_empty() { None } else { Some(calls) }
}

/* OUR ADDITION (not vendored): LFM3-1.7B prompt-guided mode. The model
 * reliably wraps calls in the tool-call tag pair but writes the body in
 * an XML-attribute style the stock JSON/XML/GLM parsers do not cover:
 *   O: `function=NAME` open + immediate close (parameterless call)
 *   O: `function=NAME` open + `parameter=KEY` children + close
 * Accept it so such a call is not dropped; JSON bodies still take
 * precedence because the stock fallbacks run first. */
pub(crate) static FUNCTION_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\x3cfunction\s*=\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*>(.*?)\x3c/\s*function\s*>"#)
        .expect("FUNCTION_ATTR_RE regex must compile")
});

pub(crate) static PARAM_ATTR_EQ_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\x3cparameter\s*=\s*([a-zA-Z_][a-zA-Z0-9_]*)\s*>(.*?)\x3c/\s*parameter\s*>"#)
        .expect("PARAM_ATTR_EQ_RE regex must compile")
});

/// Parse `function=NAME` attribute-style bodies (see block above).
/// Parameter children are accepted in both `parameter=KEY` and
/// MiniMax `parameter name="KEY"` spellings; values are strings.
pub(crate) fn parse_function_attr_calls(body: &str) -> Option<Vec<ParsedToolCall>> {
    let mut calls = Vec::new();
    for cap in FUNCTION_ATTR_RE.captures_iter(body) {
        let Some(name) = cap.get(1) else {
            continue;
        };
        let name = name.as_str().trim();
        if name.is_empty() {
            continue;
        }
        let inner = cap.get(2).map(|m| m.as_str()).unwrap_or("");
        let mut args = serde_json::Map::new();
        for p in PARAM_ATTR_EQ_RE.captures_iter(inner) {
            let (Some(k), Some(v)) = (p.get(1), p.get(2)) else {
                continue;
            };
            let key = k.as_str().trim();
            let value = v.as_str().trim();
            if !key.is_empty() && !value.is_empty() {
                /* child text that is itself JSON (numbers, arrays,
                 * objects) becomes a real JSON value; anything else
                 * stays a string */
                let val = serde_json::from_str::<serde_json::Value>(value)
                    .unwrap_or_else(|_| serde_json::Value::String(value.to_string()));
                args.insert(key.to_string(), val);
            }
        }
        for p in MINIMAX_PARAMETER_RE.captures_iter(inner) {
            let key = p
                .get(1)
                .or_else(|| p.get(2))
                .map(|m| m.as_str().trim().to_string());
            let value = p.get(3).map(|m| m.as_str().trim().to_string());
            if let (Some(k), Some(v)) = (key, value) {
                if !k.is_empty() && !v.is_empty() {
                    let val = serde_json::from_str::<serde_json::Value>(&v)
                        .unwrap_or_else(|_| serde_json::Value::String(v));
                    args.insert(k, val);
                }
            }
        }
        calls.push(ParsedToolCall {
            name: map_tool_name_alias(name).to_string(),
            arguments: serde_json::Value::Object(args),
            tool_call_id: None,
        });
    }
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// Parse MiniMax-style XML tool calls with attributed invoke/parameter tags.
pub(crate) fn parse_minimax_invoke_calls(response: &str) -> Option<(String, Vec<ParsedToolCall>)> {
    let mut calls = Vec::new();
    let mut text_parts = Vec::new();
    let mut last_end = 0usize;

    for cap in MINIMAX_INVOKE_RE.captures_iter(response) {
        let Some(full_match) = cap.get(0) else {
            continue;
        };

        let before = response[last_end..full_match.start()].trim();
        if !before.is_empty() {
            text_parts.push(before.to_string());
        }

        let name = cap
            .get(1)
            .or_else(|| cap.get(2))
            .map(|m| m.as_str().trim())
            .filter(|v| !v.is_empty());
        let body = cap.get(3).map(|m| m.as_str()).unwrap_or("").trim();
        last_end = full_match.end();

        let Some(name) = name else {
            continue;
        };

        let mut args = serde_json::Map::new();
        for param_cap in MINIMAX_PARAMETER_RE.captures_iter(body) {
            let key = param_cap
                .get(1)
                .or_else(|| param_cap.get(2))
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if key.is_empty() {
                continue;
            }
            let value = param_cap
                .get(3)
                .map(|m| m.as_str().trim())
                .unwrap_or_default();
            if value.is_empty() {
                continue;
            }

            let parsed = extract_json_values(value).into_iter().next();
            args.insert(
                key.to_string(),
                parsed.unwrap_or_else(|| serde_json::Value::String(value.to_string())),
            );
        }

        if args.is_empty() {
            if let Some(first_json) = extract_json_values(body).into_iter().next() {
                match first_json {
                    serde_json::Value::Object(obj) => args = obj,
                    other => {
                        args.insert("value".to_string(), other);
                    }
                }
            } else if !body.is_empty() {
                args.insert(
                    "content".to_string(),
                    serde_json::Value::String(body.to_string()),
                );
            }
        }

        calls.push(ParsedToolCall {
            name: map_tool_name_alias(name).to_string(),
            arguments: serde_json::Value::Object(args),
            tool_call_id: None,
        });
    }

    if calls.is_empty() {
        return None;
    }

    let after = response[last_end..].trim();
    if !after.is_empty() {
        text_parts.push(after.to_string());
    }

    let text = text_parts
        .join("\n")
        .replace("<minimax:tool_call>", "")
        .replace("</minimax:tool_call>", "")
        .replace("<minimax:toolcall>", "")
        .replace("</minimax:toolcall>", "")
        .trim()
        .to_string();

    Some((text, calls))
}
