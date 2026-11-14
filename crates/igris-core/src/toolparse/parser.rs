//! The main dispatching parser: parse_tool_calls and the tools-span
//! machinery, plus result-block stripping, parse-issue detection, and
//! native assistant-history construction.

use regex::Regex;
use std::sync::LazyLock;
use super::*;


/// Is this JSON value an *invocation* rather than a tool *declaration*?
///
/// Only consulted for the `<tools>` wrapper, which is overloaded: in the Hermes
/// prompt format `<tools>` DECLARES the available tools, while `<tool_call>`
/// invokes one. Every other alias in [`TOOL_CALL_OPEN_TAGS`] has exactly one
/// meaning, so this narrowing does not apply to them.
///
/// The distinction cannot be left to [`has_arguments_signal`], which counts
/// `parameters` as an arguments marker: a declaration carries `name` +
/// `parameters` and is therefore indistinguishable from a call by that test.
/// A declaration is rejected here on three signals a real invocation never has
/// -- a JSON array of entries, a `description`, or a `parameters` schema in
/// place of concrete `arguments`.
pub(crate) fn looks_like_tools_wrapper_invocation(value: &serde_json::Value) -> bool {
    // A declaration block is an ARRAY of tool schemas. An invocation is one call.
    let serde_json::Value::Object(map) = value else {
        return false;
    };
    // `description` and `parameters` describe a tool; they never appear in a call.
    if map.contains_key("description") || map.contains_key("parameters") {
        return false;
    }
    // OpenAI-shaped `{"type":"function","function":{...}}` declarations.
    if let Some(inner) = map.get("function").and_then(serde_json::Value::as_object)
        && (inner.contains_key("description") || inner.contains_key("parameters"))
    {
        return false;
    }
    // Only `arguments` is admitted. `args` was accepted here previously, but the
    // canonical parser reads `arguments`/`parameters` and never `args`, so an
    // `args`-only body was admitted as an invocation and then dispatched with
    // EMPTY arguments -- a corrupted call rather than an inert one. Admitting a
    // shape the parser cannot honour is worse than not admitting it: if a model
    // is found to emit `args`, implement it in the canonical parser first.
    //
    // THE ADMITTED VALUE MUST BE THE EXECUTED VALUE. `parse_tool_calls_from_json_value`
    // gives precedence to a nested `function` object, and `tool_calls` can expand one
    // value into several. A body carrying BOTH a benign top level and a nested
    // envelope therefore passed this predicate on the top level while dispatching the
    // nested content:
    //
    //   {"name":"benign","arguments":{},
    //    "function":{"name":"shell","arguments":{"command":"rm -rf /tmp/x"}}}
    //
    // The discriminator has to authorize the same representation that crosses the
    // parser boundary, so an envelope member disqualifies the body outright rather
    // than being validated on a level the parser will ignore.
    if map.contains_key("function") || map.contains_key("tool_calls") {
        return false;
    }
    has_non_empty_string(value, "name") && map.contains_key("arguments")
}

/// The extent of one `<tools>` span, and the single value it is allowed to carry.
///
/// `<tools>` is the only overloaded wrapper in [`TOOL_CALL_OPEN_TAGS`]: in the
/// Hermes prompt format it DECLARES the available tools, while some models also
/// use it to wrap an invocation. Because it is overloaded, its contract is
/// narrower than every other alias:
///
/// 1. The body is delimited STRUCTURALLY -- see [`tools_span`].
/// 2. It carries exactly one canonical JSON invocation, or nothing.
/// 3. Whatever it does not carry is inert, and stays inert everywhere else.
pub(crate) struct ToolsSpan {
    /// Byte offset in `after_open` where the body ends.
    body_end: usize,
    /// Byte length of the close tag that terminated the span; 0 when unclosed.
    close_len: usize,
    /// The one JSON value the body carried, if the body was exactly one value.
    /// `None` means nothing in this span may ever be dispatched.
    value: Option<serde_json::Value>,
}

/// Delimit a `<tools>` span by PARSING it, never by substring search.
///
/// A textual scan for the close tag is not JSON-string aware, so tag-shaped
/// bytes inside argument content act as a delimiter: the wrapper truncates
/// mid-string, the valid call is lost, and the remainder is exposed to the
/// other recovery parsers as if the model had emitted it at top level. Tool
/// arguments routinely carry markup, so quoted tag text must never delimit.
///
/// Parsing first makes the distinction structural: bytes inside the parsed
/// value belong to the value, and only a close alias that FOLLOWS the value can
/// end the span. This holds for the matching close, a foreign close alias, and
/// no close at all -- the three ways a span can end -- so all three share one
/// rule rather than each re-deriving a boundary.
///
/// When the body is not exactly one JSON value it can never be admitted, so the
/// only remaining job is to bound the inert region. That search starts AFTER any
/// value that did parse, which keeps a quoted close from truncating the span.
/// A body that looks like JSON but does not parse gets no textual search at all:
/// a close alias may be quoted inside an unterminated string, and there is no
/// valid call in a malformed body to lose by consuming the remainder.
///
/// The FIRST close alias at or after that point ends the span; bytes beyond it
/// are outside the wrapper and are parsed normally. This is deliberate. A model
/// that echoes its declaration block and then invokes a tool is the common real
/// shape, and swallowing everything to a later close would make that invocation
/// unreachable. It also concedes nothing: content after the close is content the
/// model could have emitted with no wrapper at all, so treating it as ordinary
/// output grants no capability. What the wrapper policy owns is the body it
/// delimits -- that a declaration, an echoed prompt, or a prose example inside
/// the span never becomes a call -- not the whole remainder of the response.
pub(crate) fn tools_span(after_open: &str) -> ToolsSpan {
    let lead = after_open.len() - after_open.trim_start().len();
    let mut de = serde_json::Deserializer::from_str(after_open.trim_start())
        .into_iter::<serde_json::Value>();

    if let Some(Ok(value)) = de.next() {
        let body_end = lead + de.byte_offset();
        if let Some(rest) = after_open.get(body_end..) {
            let ws = rest.len() - rest.trim_start().len();
            let trailing = rest.trim_start();

            // Models mix open/close aliases, so ANY close tag can terminate the
            // wrapper. It still has to follow the value to count.
            if let Some(close) = TOOL_CALL_CLOSE_TAGS
                .iter()
                .find(|tag| trailing.starts_with(**tag))
            {
                return ToolsSpan {
                    body_end: body_end + ws,
                    close_len: close.len(),
                    value: Some(value),
                };
            }

            // Unclosed, but the body IS the complete value. Truncation of the
            // wrapper must not weaken the rule the closed paths enforce, and it
            // must not strengthen it either: a complete invocation still counts.
            if trailing.is_empty() {
                return ToolsSpan {
                    body_end: after_open.len(),
                    close_len: 0,
                    value: Some(value),
                };
            }
        }

        // A value parsed, but the body holds more than that one value (trailing
        // prose, a second value). Not admissible; bound the span from the end of
        // what parsed so quoted tag text inside it cannot act as the delimiter.
        return match find_first_tag(&after_open[body_end..], &TOOL_CALL_CLOSE_TAGS) {
            Some((idx, tag)) => ToolsSpan {
                body_end: body_end + idx,
                close_len: tag.len(),
                value: None,
            },
            None => ToolsSpan {
                body_end: after_open.len(),
                close_len: 0,
                value: None,
            },
        };
    }

    // Nothing parsed. If the body opens like JSON it is malformed JSON, and a
    // close alias could be quoted inside an unterminated string -- so do not
    // trust a textual scan to bound it. Consume the remainder instead; a
    // malformed `<tools>` body has no valid call to lose, and leaving a suffix
    // behind is exactly how refused bytes reach another executable parser.
    let trimmed = after_open.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return ToolsSpan {
            body_end: after_open.len(),
            close_len: 0,
            value: None,
        };
    }

    // Prose or a declaration block: no JSON string for a close alias to hide in,
    // so a textual bound is sound and keeps any following tags parseable.
    match find_first_tag(after_open, &TOOL_CALL_CLOSE_TAGS) {
        Some((idx, tag)) => ToolsSpan {
            body_end: idx,
            close_len: tag.len(),
            value: None,
        },
        None => ToolsSpan {
            body_end: after_open.len(),
            close_len: 0,
            value: None,
        },
    }
}

/// Does a byte RANGE overlap any `<tools>` span that was refused?
///
/// Refusing to admit a body is only half of the boundary. The other half is that
/// the refused bytes are not offered to a second executable parser. Fallbacks
/// that walk `remaining` get this for free, because the `<tools>` handler
/// advances past the span. Fallbacks that re-scan the ORIGINAL response do not,
/// and must consult this instead.
///
/// THE TEST IS OVERLAP, NOT MEMBERSHIP OF THE START OFFSET. Asking only whether
/// a match BEGINS inside a refused span leaves the boundary open from the other
/// side: a fence that opens BEFORE the span and runs through it never starts
/// inside anything, passes the check, and its body -- refused bytes included --
/// is handed to `extract_json_values`, which finds the very object the `<tools>`
/// handler rejected. Start-offset containment is not span containment.
///
/// Two ranges overlap when each begins before the other ends; empty ranges
/// cannot overlap anything.
pub(crate) fn range_hits_rejected_span(rejected: &[std::ops::Range<usize>], start: usize, end: usize) -> bool {
    if start >= end {
        return false;
    }
    rejected
        .iter()
        .any(|span| !span.is_empty() && start < span.end && span.start < end)
}

pub fn parse_tool_calls(response: &str) -> (String, Vec<ParsedToolCall>) {
    // Strip `<think>...</think>` blocks before parsing.  Qwen and other
    // reasoning models embed chain-of-thought inline in the response text;
    // these tags can interfere with `<tool_call>` extraction and must be
    // removed first.
    let cleaned = strip_think_tags(response);
    let response = cleaned.as_str();

    let mut text_parts = Vec::new();
    let mut calls = Vec::new();
    let mut remaining = response;
    // Byte ranges of `<tools>` spans this loop refused. Consulted by the global
    // fallbacks that re-scan `response` instead of walking `remaining`.
    let mut rejected_tools_spans: Vec<std::ops::Range<usize>> = Vec::new();

    // First, try to parse as OpenAI-style JSON response with tool_calls array
    // This handles model_providers like Minimax that return tool_calls in native JSON format
    if let Ok(json_value) = serde_json::from_str::<serde_json::Value>(response.trim()) {
        calls = parse_tool_calls_from_json_value(&json_value);
        if !calls.is_empty() {
            // If we found tool_calls, extract any content field as text
            if let Some(content) = json_value.get("content").and_then(|v| v.as_str())
                && !content.trim().is_empty()
            {
                text_parts.push(content.trim().to_string());
            }
            return (text_parts.join("\n"), calls);
        }
    }
    if let Some(call) = parse_malformed_file_write_call(response.trim()) {
        return (String::new(), vec![call]);
    }

    // This scan searches the WHOLE response for executable `<invoke>` syntax, so
    // running it ahead of the tag loop lets legacy markup nested inside a
    // `<tools>` wrapper execute before the body is ever classified. Gating the
    // wrapper-local sites cannot protect a scan that runs first. When a `<tools>`
    // span is present the tag loop owns the text; responses without one are
    // unaffected.
    if !response.contains("<tools>")
        && let Some((minimax_text, minimax_calls)) = parse_minimax_invoke_calls(response)
        && !minimax_calls.is_empty()
    {
        return (minimax_text, minimax_calls);
    }

    // Fall back to XML-style tool-call tag parsing.
    while let Some((start, open_tag)) = find_first_tag(remaining, &TOOL_CALL_OPEN_TAGS) {
        // Everything before the tag is text
        let before = &remaining[..start];
        if !before.trim().is_empty() {
            text_parts.push(before.trim().to_string());
        }

        // `<tools>` is handled HERE, in full, and never reaches the generic
        // recovery paths below. Those paths exist to rescue malformed output from
        // unambiguous tags: they scan through prose for JSON, retry the body
        // against XML and GLM shorthand, and treat a foreign or missing close as
        // a reason to try harder. Every one of those behaviours is wrong for an
        // overloaded tag that also declares tools, and threading a guard through
        // each of them means the guard can be forgotten at the next path added.
        // Handling the tag once, at the top, makes that structurally impossible.
        if open_tag == "<tools>" {
            let after_open = &remaining[start + open_tag.len()..];
            let span = tools_span(after_open);
            let consumed = span.body_end + span.close_len;

            // Exactly one canonical invocation is the entire admissible set.
            let admitted = span
                .value
                .as_ref()
                .filter(|value| looks_like_tools_wrapper_invocation(value))
                .map(parse_tool_calls_from_json_value)
                .filter(|parsed| !parsed.is_empty());

            if let Some(parsed) = admitted {
                calls.extend(parsed);
            } else {
                // Refused. The body becomes visible text, and the span is
                // recorded so no later parser can execute the bytes this
                // boundary just declined.
                let body = after_open[..span.body_end].trim();
                if !body.is_empty() {
                    text_parts.push(body.to_string());
                }
                let span_start = response.len() - remaining.len() + start;
                rejected_tools_spans.push(span_start..span_start + open_tag.len() + consumed);
            }

            remaining = &after_open[consumed..];
            continue;
        }

        let Some(close_tag) = (match open_tag {
            "<tool_call>" => Some("</tool_call>"),
            "<tool_calls>" => Some("</tool_calls>"),
            "<toolcall>" => Some("</toolcall>"),
            "<tool-call>" => Some("</tool-call>"),
            "<invoke>" => Some("</invoke>"),
            "<minimax:tool_call>" => Some("</minimax:tool_call>"),
            "<minimax:toolcall>" => Some("</minimax:toolcall>"),
            _ => None,
        }) else {
            break;
        };

        let after_open = &remaining[start + open_tag.len()..];
        if let Some(close_idx) = after_open.find(close_tag) {
            let inner = &after_open[..close_idx];
            let mut parsed_any = false;

            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    parsed_any = true;
                    calls.extend(parsed_calls);
                }
            }

            if !parsed_any && let Some(call) = parse_malformed_file_write_call(inner) {
                calls.push(call);
                parsed_any = true;
            }

            // If JSON parsing failed, try XML format (DeepSeek/GLM style)
            if !parsed_any && let Some(xml_calls) = parse_xml_tool_calls(inner) {
                calls.extend(xml_calls);
                parsed_any = true;
            }

            if !parsed_any {
                // GLM-style shortened body: `shell>uname -a` or `shell\ncommand: date`
                if let Some(glm_call) = parse_glm_shortened_body(inner) {
                    calls.push(glm_call);
                    parsed_any = true;
                }
            }

            if !parsed_any {
                // OUR ADDITION: LFM3 function-attribute bodies
                // (parse_function_attr_calls, see block above).
                if let Some(attr_calls) = parse_function_attr_calls(inner) {
                    calls.extend(attr_calls);
                    parsed_any = true;
                }
            }

            if !parsed_any {
                eprintln!(
                    "[toolparse] Malformed <tool_call>: expected tool-call object in tag body (JSON/XML/GLM)"
                );
            }

            remaining = &after_open[close_idx + close_tag.len()..];
        } else {
            // Matching close tag not found — try cross-alias close tags first.
            // Models sometimes mix open/close tag aliases (e.g. <tool_call>...</invoke>).
            let mut resolved = false;
            if let Some((cross_idx, cross_tag)) = find_first_tag(after_open, &TOOL_CALL_CLOSE_TAGS)
            {
                let inner = &after_open[..cross_idx];
                let mut parsed_any = false;

                // Try JSON
                let json_values = extract_json_values(inner);
                for value in json_values {
                    let parsed_calls = parse_tool_calls_from_json_value(&value);
                    if !parsed_calls.is_empty() {
                        parsed_any = true;
                        calls.extend(parsed_calls);
                    }
                }

                if !parsed_any && let Some(call) = parse_malformed_file_write_call(inner) {
                    calls.push(call);
                    parsed_any = true;
                }

                // Try XML
                if !parsed_any && let Some(xml_calls) = parse_xml_tool_calls(inner) {
                    calls.extend(xml_calls);
                    parsed_any = true;
                }

                // Try GLM shortened body
                if !parsed_any && let Some(glm_call) = parse_glm_shortened_body(inner) {
                    calls.push(glm_call);
                    parsed_any = true;
                }

                if parsed_any {
                    remaining = &after_open[cross_idx + cross_tag.len()..];
                    resolved = true;
                }
            }

            if resolved {
                continue;
            }

            // No cross-alias close tag resolved — fall back to JSON recovery
            // from unclosed tags (brace-balancing).
            if let Some(json_end) = find_json_end(after_open)
                && let Ok(value) =
                    serde_json::from_str::<serde_json::Value>(&after_open[..json_end])
            {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    calls.extend(parsed_calls);
                    remaining = strip_leading_close_tags(&after_open[json_end..]);
                    continue;
                }
            }

            if let Some((value, consumed_end)) = extract_first_json_value_with_end(after_open) {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                if !parsed_calls.is_empty() {
                    calls.extend(parsed_calls);
                    remaining = strip_leading_close_tags(&after_open[consumed_end..]);
                    continue;
                }
            }

            if let Some(call) = parse_malformed_file_write_call(after_open) {
                calls.push(call);
                remaining = "";
                continue;
            }

            // Last resort: try GLM shortened body on everything after the open tag.
            // The model may have emitted `<tool_call>shell>ls` with no close tag at all.
            let glm_input = after_open.trim();
            if let Some(glm_call) = parse_glm_shortened_body(glm_input) {
                calls.push(glm_call);
                remaining = "";
                continue;
            }

            remaining = &remaining[start..];
            break;
        }
    }

    // The fallbacks below re-scan the ORIGINAL response rather than walking
    // `remaining`, so the tag loop's consume-on-reject does not reach them: a
    // `<tools>` body this parser already refused is otherwise handed straight to
    // the next executable parser, which has no notion of the wrapper policy.
    // Every match therefore has to be checked against the refused spans. The
    // fallbacks further down operate on `remaining` and are covered already.

    // If XML tags found nothing, try markdown code blocks with tool_call language.
    // Models behind OpenRouter sometimes output ```tool_call ... ``` or hybrid
    // ```tool_call ... </tool_call> instead of structured API calls or XML tags.
    if calls.is_empty() {
        static MD_TOOL_CALL_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(
                r"(?s)```(?:tool[_-]?call|invoke)\s*\n(.*?)(?:```|</tool[_-]?call>|</toolcall>|</invoke>|</minimax:toolcall>)",
            )
            .expect("MD_TOOL_CALL_RE regex must compile")
        });
        let mut md_text_parts: Vec<String> = Vec::new();
        let mut last_end = 0;

        for cap in MD_TOOL_CALL_RE.captures_iter(response) {
            let Some(full_match) = cap.get(0) else {
                continue;
            };
            // Range-aware: a fence that OPENS before a refused span and runs
            // through it must be refused too, not just one that starts inside.
            if range_hits_rejected_span(&rejected_tools_spans, full_match.start(), full_match.end())
            {
                continue;
            }
            let before = &response[last_end..full_match.start()];
            if !before.trim().is_empty() {
                md_text_parts.push(before.trim().to_string());
            }
            let inner = &cap[1];
            let json_values = extract_json_values(inner);
            for value in json_values {
                let parsed_calls = parse_tool_calls_from_json_value(&value);
                calls.extend(parsed_calls);
            }
            if calls.is_empty()
                && let Some(call) = parse_malformed_file_write_call(inner)
            {
                calls.push(call);
            }
            last_end = full_match.end();
        }

        if !calls.is_empty() {
            let after = &response[last_end..];
            if !after.trim().is_empty() {
                md_text_parts.push(after.trim().to_string());
            }
            text_parts = md_text_parts;
            remaining = "";
        }
    }

    // Try ```tool <name> format used by some model_providers (e.g., xAI grok)
    // Example: ```tool file_write\n{"path": "...", "content": "..."}\n```
    if calls.is_empty() {
        static MD_TOOL_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r"(?s)```tool\s+(\w+)\s*\n(.*?)(?:```|$)")
                .expect("MD_TOOL_NAME_RE regex must compile")
        });
        let mut md_text_parts: Vec<String> = Vec::new();
        let mut last_end = 0;

        for cap in MD_TOOL_NAME_RE.captures_iter(response) {
            let Some(full_match) = cap.get(0) else {
                continue;
            };
            // Range-aware: a fence that OPENS before a refused span and runs
            // through it must be refused too, not just one that starts inside.
            if range_hits_rejected_span(&rejected_tools_spans, full_match.start(), full_match.end())
            {
                continue;
            }
            let before = &response[last_end..full_match.start()];
            if !before.trim().is_empty() {
                md_text_parts.push(before.trim().to_string());
            }
            let tool_name = &cap[1];
            let inner = &cap[2];

            // Try to parse the inner content as JSON arguments
            let json_values = extract_json_values(inner);
            if json_values.is_empty() {
                if map_tool_name_alias(tool_name) == "file_write"
                    && let Some(arguments) = parse_malformed_file_write_arguments(inner)
                {
                    calls.push(ParsedToolCall {
                        name: "file_write".to_string(),
                        arguments,
                        tool_call_id: None,
                    });
                } else {
                    // Warn if we found a tool block but couldn't parse arguments
                    eprintln!(
                        "[toolparse] Found ```tool <name> block but could not parse JSON arguments (payload_len={})",
                        inner.len()
                    );
                }
            } else {
                for value in json_values {
                    let arguments = if value.is_object() {
                        value
                    } else {
                        serde_json::Value::Object(serde_json::Map::new())
                    };
                    calls.push(ParsedToolCall {
                        name: map_tool_name_alias(tool_name).to_string(),
                        arguments,
                        tool_call_id: None,
                    });
                }
            }
            last_end = full_match.end();
        }

        if !calls.is_empty() {
            let after = &response[last_end..];
            if !after.trim().is_empty() {
                md_text_parts.push(after.trim().to_string());
            }
            text_parts = md_text_parts;
            remaining = "";
        }
    }

    if calls.is_empty() {
        let xml_calls = parse_xml_attribute_tool_calls(remaining);
        if !xml_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for call in xml_calls {
                calls.push(call);
                // Try to remove the XML from text
                if let Some(start) = cleaned_text.find("<minimax:toolcall>")
                    && let Some(end) = cleaned_text.find("</minimax:toolcall>")
                {
                    let end_pos = end + "</minimax:toolcall>".len();
                    if end_pos <= cleaned_text.len() {
                        cleaned_text =
                            format!("{}{}", &cleaned_text[..start], &cleaned_text[end_pos..]);
                    }
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    if calls.is_empty() {
        let perl_calls = parse_perl_style_tool_calls(remaining);
        if !perl_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for call in perl_calls {
                calls.push(call);
                // Try to remove the TOOL_CALL block from text
                while let Some(start) = cleaned_text.find("TOOL_CALL") {
                    if let Some(end) = cleaned_text.find("/TOOL_CALL") {
                        let end_pos = end + "/TOOL_CALL".len();
                        if end_pos <= cleaned_text.len() {
                            cleaned_text =
                                format!("{}{}", &cleaned_text[..start], &cleaned_text[end_pos..]);
                        }
                    } else {
                        break;
                    }
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // <FunctionCall>
    // file_read
    // <code>path>/Users/...</code>
    // </FunctionCall>
    if calls.is_empty() {
        let func_calls = parse_function_call_tool_calls(remaining);
        if !func_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for call in func_calls {
                calls.push(call);
                // Try to remove the FunctionCall block from text
                while let Some(start) = cleaned_text.find("<FunctionCall>") {
                    if let Some(end) = cleaned_text.find("</FunctionCall>") {
                        let end_pos = end + "</FunctionCall>".len();
                        if end_pos <= cleaned_text.len() {
                            cleaned_text =
                                format!("{}{}", &cleaned_text[..start], &cleaned_text[end_pos..]);
                        }
                    } else {
                        break;
                    }
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // GLM-style tool calls (browser_open/url>https://..., shell/command>ls, etc.)
    if calls.is_empty() {
        let glm_calls = parse_glm_style_tool_calls(remaining);
        if !glm_calls.is_empty() {
            let mut cleaned_text = remaining.to_string();
            for (name, args, raw) in &glm_calls {
                calls.push(ParsedToolCall {
                    name: name.clone(),
                    arguments: args.clone(),
                    tool_call_id: None,
                });
                if let Some(r) = raw {
                    cleaned_text = cleaned_text.replace(r, "");
                }
            }
            if !cleaned_text.trim().is_empty() {
                text_parts.push(cleaned_text.trim().to_string());
            }
            remaining = "";
        }
    }

    // Remaining text after last tool call
    if !remaining.trim().is_empty() {
        text_parts.push(remaining.trim().to_string());
    }

    (text_parts.join("\n"), calls)
}

/// Strip prompt-guided tool artifacts from visible output while preserving
/// raw model text in history for future turns.
pub fn strip_tool_result_blocks(text: &str) -> String {
    static TOOL_RESULT_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<tool_result[^>]*>.*?</tool_result>")
            .expect("TOOL_RESULT_RE regex must compile")
    });
    static THINKING_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<thinking>.*?</thinking>").expect("THINKING_RE regex must compile")
    });
    static THINK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?s)<think>.*?</think>").expect("THINK_RE regex must compile")
    });
    static TOOL_RESULTS_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\[Tool results\]\s*\n?")
            .expect("TOOL_RESULTS_PREFIX_RE regex must compile")
    });
    static EXCESS_BLANK_LINES_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\n{3,}").expect("EXCESS_BLANK_LINES_RE regex must compile"));

    let result = TOOL_RESULT_RE.replace_all(text, "");
    let result = THINKING_RE.replace_all(&result, "");
    let result = THINK_RE.replace_all(&result, "");
    let result = TOOL_RESULTS_PREFIX_RE.replace_all(&result, "");
    let result = EXCESS_BLANK_LINES_RE.replace_all(result.trim(), "\n\n");

    result.trim().to_string()
}

pub fn detect_tool_call_parse_issue(
    response: &str,
    parsed_calls: &[ParsedToolCall],
) -> Option<String> {
    if !parsed_calls.is_empty() {
        return None;
    }

    let trimmed = response.trim();
    if trimmed.is_empty() {
        return None;
    }

    // LFM3 native DSML envelope: never a parse family (policy note on
    // DSML_OPEN). A response that OPENS with the envelope is the model
    // drifting to its native call format — issue the corrective verdict so
    // it re-anchors to the protocol-block format. A prose-wrapped echo
    // after a successful round is noise: the prose is the answer, the
    // echo is held by the stream filter, and no corrective round is spent.
    if trimmed.starts_with(DSML_OPEN) && !looks_like_tool_protocol_example(trimmed) {
        return Some(
            "response resembled an internal tool protocol envelope but no valid tool call could be parsed"
                .into(),
        );
    }

    if looks_like_tool_protocol_envelope(trimmed) {
        return Some(
            "response resembled an internal tool protocol envelope but no valid tool call could be parsed"
                .into(),
        );
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return has_malformed_tool_protocol_json_signal(&value).then(|| {
            "response resembled an internal tool protocol envelope but no valid tool call could be parsed"
                .into()
        });
    }

    if has_malformed_tool_protocol_text_signal(trimmed) {
        return Some(
            "response resembled an internal tool protocol envelope but no valid tool call could be parsed"
                .into(),
        );
    }

    let contains_tool_payload_marker = trimmed.contains("<tool_call")
        || trimmed.contains("<toolcall")
        || trimmed.contains("<tool-call")
        || trimmed.contains("```tool_call")
        || trimmed.contains("```toolcall")
        || trimmed.contains("```tool-call")
        || trimmed.contains("```tool file_")
        || trimmed.contains("```tool shell")
        || trimmed.contains("```tool web_")
        || trimmed.contains("```tool memory_")
        || trimmed.contains("```tool ") // Generic ```tool <name> pattern
        || trimmed.contains("TOOL_CALL")
        || trimmed.contains("[TOOL_CALL]")
        || trimmed.contains("<FunctionCall>");

    if contains_tool_payload_marker {
        if looks_like_tool_protocol_example(trimmed) {
            return None;
        }
        if contains_tool_protocol_tag_call(trimmed) {
            return Some(
                "response resembled a tool-call payload but no valid tool call could be parsed"
                    .into(),
            );
        }

        let (visible_text, recovered_calls) = parse_tool_calls(trimmed);
        if !recovered_calls.is_empty() && !visible_text.trim().is_empty() {
            return None;
        }
        if !recovered_calls.is_empty() || visible_text.trim().is_empty() {
            return Some(
                "response resembled a tool-call payload but no valid tool call could be parsed"
                    .into(),
            );
        }
    }

    if looks_like_malformed_tool_protocol_envelope(trimmed) {
        Some("response resembled a tool-call payload but no valid tool call could be parsed".into())
    } else {
        None
    }
}

pub fn build_native_assistant_history_from_parsed_calls(
    text: &str,
    tool_calls: &[ParsedToolCall],
    reasoning_content: Option<&str>,
) -> Option<String> {
    // Strict provider validators (DeepSeek V4, NVIDIA NIM, ...) reject
    // assistant messages that carry `tool_calls: []`. When there are no
    // parsed calls, return None so the caller falls through to a plain
    // text assistant message.
    if tool_calls.is_empty() {
        return None;
    }

    let calls_json = tool_calls
        .iter()
        .map(|tc| {
            Some(serde_json::json!({
                "id": tc.tool_call_id.clone()?,
                "name": tc.name,
                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_else(|_| "{}".to_string()),
            }))
        })
        .collect::<Option<Vec<_>>>()?;

    let content = if text.trim().is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::Value::String(text.trim().to_string())
    };

    let mut obj = serde_json::json!({
        "content": content,
        "tool_calls": calls_json,
    });

    if let Some(rc) = reasoning_content
        && let Some(obj) = obj.as_object_mut()
    {
        obj.insert(
            "reasoning_content".to_string(),
            serde_json::Value::String(rc.to_string()),
        );
    }

    Some(obj.to_string())
}
