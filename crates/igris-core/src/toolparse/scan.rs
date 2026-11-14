//! Low-level scanning: open/close tag tables, JSON value extraction,
//! recovered-string decoding, malformed file_write recovery.

use crate::toolparse::envelope::json_fence_body;
use crate::toolparse::formats::map_tool_name_alias;
use crate::toolparse::protocol::ParsedToolCall;


pub(crate) const TOOL_CALL_OPEN_TAGS: [&str; 8] = [
    "<tool_call>",
    "<tool_calls>",
    "<toolcall>",
    "<tool-call>",
    // Hermes-family models sometimes emit the tool *declaration* tag around an
    // invocation. Qwen2.5-Coder-32B does this deterministically: the payload is
    // well-formed Hermes JSON, only the wrapper is wrong.
    "<tools>",
    "<invoke>",
    "<minimax:tool_call>",
    "<minimax:toolcall>",
];

pub(crate) const TOOL_CALL_CLOSE_TAGS: [&str; 8] = [
    "</tool_call>",
    "</tool_calls>",
    "</toolcall>",
    "</tool-call>",
    "</tools>",
    "</invoke>",
    "</minimax:tool_call>",
    "</minimax:toolcall>",
];

pub(crate) fn find_first_tag<'a>(haystack: &str, tags: &'a [&'a str]) -> Option<(usize, &'a str)> {
    tags.iter()
        .filter_map(|tag| haystack.find(tag).map(|idx| (idx, *tag)))
        .min_by_key(|(idx, _)| *idx)
}

pub(crate) fn extract_first_json_value_with_end(input: &str) -> Option<(serde_json::Value, usize)> {
    let trimmed = input.trim_start();
    let trim_offset = input.len().saturating_sub(trimmed.len());

    for (byte_idx, ch) in trimmed.char_indices() {
        if ch != '{' && ch != '[' {
            continue;
        }

        let slice = &trimmed[byte_idx..];
        let mut stream = serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
        if let Some(Ok(value)) = stream.next() {
            let consumed = stream.byte_offset();
            if consumed > 0 {
                return Some((value, trim_offset + byte_idx + consumed));
            }
        }
    }

    None
}

pub(crate) fn strip_leading_close_tags(mut input: &str) -> &str {
    loop {
        let trimmed = input.trim_start();
        if !trimmed.starts_with("</") {
            return trimmed;
        }

        let Some(close_end) = trimmed.find('>') else {
            return "";
        };
        input = &trimmed[close_end + 1..];
    }
}

pub(crate) fn extract_json_values(input: &str) -> Vec<serde_json::Value> {
    let mut values = Vec::new();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return values;
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        values.push(value);
        return values;
    }

    let char_positions: Vec<(usize, char)> = trimmed.char_indices().collect();
    let mut idx = 0;
    while idx < char_positions.len() {
        let (byte_idx, ch) = char_positions[idx];
        if ch == '{' || ch == '[' {
            let slice = &trimmed[byte_idx..];
            let mut stream =
                serde_json::Deserializer::from_str(slice).into_iter::<serde_json::Value>();
            if let Some(Ok(value)) = stream.next() {
                let consumed = stream.byte_offset();
                if consumed > 0 {
                    values.push(value);
                    let next_byte = byte_idx + consumed;
                    while idx < char_positions.len() && char_positions[idx].0 < next_byte {
                        idx += 1;
                    }
                    continue;
                }
            }
        }
        idx += 1;
    }

    values
}

pub(crate) fn skip_json_ws(input: &str, mut idx: usize) -> usize {
    while let Some(ch) = input[idx..].chars().next() {
        if !ch.is_whitespace() {
            break;
        }
        idx += ch.len_utf8();
    }
    idx
}

pub(crate) fn find_json_field_value_start(input: &str, field: &str, start: usize) -> Option<usize> {
    let pattern = format!("\"{field}\"");
    let mut search_start = start;
    while let Some(relative) = input[search_start..].find(&pattern) {
        let key_start = search_start + relative;
        let after_key = key_start + pattern.len();
        let colon = skip_json_ws(input, after_key);
        if input[colon..].starts_with(':') {
            return Some(colon + 1);
        }
        search_start = after_key;
    }
    None
}

pub(crate) fn find_json_string_end(input: &str, quote_start: usize) -> Option<usize> {
    if !input[quote_start..].starts_with('"') {
        return None;
    }

    let mut escaped = false;
    for (relative, ch) in input[quote_start + 1..].char_indices() {
        let idx = quote_start + 1 + relative;
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' => return Some(idx),
            _ => {}
        }
    }

    None
}

pub(crate) fn parse_json_string_field_after(
    input: &str,
    field: &str,
    start: usize,
) -> Option<(String, usize)> {
    let value_start = skip_json_ws(input, find_json_field_value_start(input, field, start)?);
    let value_end = find_json_string_end(input, value_start)?;
    let value = serde_json::from_str::<String>(&input[value_start..=value_end]).ok()?;
    Some((value, value_end + 1))
}

// Narrow recovery for malformed file_write calls whose content string contains
// model-emitted unescaped quotes. This is deliberately not a general JSON
// repair path: content must be the final argument field and the remaining tail
// must only close the surrounding tool-call protocol envelope.
pub(crate) fn decode_recovered_json_string_fragment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();

    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }

        match chars.next() {
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some('b') => out.push('\u{0008}'),
            Some('f') => out.push('\u{000c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let mut value = 0u32;
                let mut valid = true;
                let mut consumed = String::with_capacity(4);
                for _ in 0..4 {
                    let Some(hex) = chars.next() else {
                        valid = false;
                        break;
                    };
                    consumed.push(hex);
                    if let Some(digit) = hex.to_digit(16) {
                        value = (value << 4) | digit;
                    } else {
                        valid = false;
                    }
                }
                if valid && consumed.len() == 4 {
                    if let Some(decoded) = char::from_u32(value) {
                        out.push(decoded);
                    } else {
                        out.push_str("\\u");
                        out.push_str(&consumed);
                    }
                } else {
                    out.push_str("\\u");
                    out.push_str(&consumed);
                }
            }
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }

    out
}

pub(crate) fn file_write_content_tail_is_unambiguous(input: &str, after_quote: usize) -> bool {
    let mut idx = skip_json_ws(input, after_quote);
    if !input[idx..].starts_with('}') {
        return false;
    }
    idx += '}'.len_utf8();
    idx = skip_json_ws(input, idx);

    while let Some(ch) = input[idx..].chars().next() {
        match ch {
            '}' | ']' => {
                idx += ch.len_utf8();
                idx = skip_json_ws(input, idx);
            }
            _ => break,
        }
    }

    let tail = input[idx..].trim_start();
    tail.is_empty()
        || tail.starts_with("</tool_call>")
        || tail.starts_with("</tool_calls>")
        || tail.starts_with("</tools>")
        || tail.starts_with("</toolcall>")
        || tail.starts_with("</tool-call>")
        || tail.starts_with("</invoke>")
        || tail.starts_with("</minimax:tool_call>")
        || tail.starts_with("</minimax:toolcall>")
        || tail.starts_with("```")
}

pub(crate) fn file_write_content_quote_starts_additional_final_field(input: &str, after_quote: usize) -> bool {
    let mut idx = skip_json_ws(input, after_quote);
    if !input[idx..].starts_with(',') {
        return false;
    }

    idx += ','.len_utf8();
    idx = skip_json_ws(input, idx);

    let Some(field_end) = find_json_string_end(input, idx) else {
        return false;
    };

    idx = skip_json_ws(input, field_end + 1);
    if !input[idx..].starts_with(':') {
        return false;
    }

    idx += ':'.len_utf8();
    idx = skip_json_ws(input, idx);

    let mut stream =
        serde_json::Deserializer::from_str(&input[idx..]).into_iter::<serde_json::Value>();
    let Some(Ok(_)) = stream.next() else {
        return false;
    };

    let consumed = stream.byte_offset();
    consumed > 0 && file_write_content_tail_is_unambiguous(input, idx + consumed)
}

pub(crate) fn parse_malformed_file_write_content_after(input: &str, start: usize) -> Option<String> {
    let value_start = skip_json_ws(input, find_json_field_value_start(input, "content", start)?);
    if !input[value_start..].starts_with('"') {
        return None;
    }

    let mut escaped = false;
    for (relative, ch) in input[value_start + 1..].char_indices() {
        let idx = value_start + 1 + relative;
        if escaped {
            escaped = false;
            continue;
        }

        match ch {
            '\\' => escaped = true,
            '"' if file_write_content_tail_is_unambiguous(input, idx + 1) => {
                let raw = &input[value_start + 1..idx];
                return Some(decode_recovered_json_string_fragment(raw));
            }
            '"' if file_write_content_quote_starts_additional_final_field(input, idx + 1) => {
                return None;
            }
            '"' => {}
            _ => {}
        }
    }

    None
}

pub(crate) fn parse_malformed_file_write_arguments(input: &str) -> Option<serde_json::Value> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let object_start = skip_json_ws(trimmed, 0);
    if !trimmed[object_start..].starts_with('{') {
        return None;
    }

    let (path, path_end) = parse_json_string_field_after(trimmed, "path", object_start)?;
    if path.trim().is_empty() {
        return None;
    }

    let content = parse_malformed_file_write_content_after(trimmed, path_end)?;
    Some(serde_json::json!({
        "path": path,
        "content": content,
    }))
}

pub(crate) fn parse_malformed_file_write_call(input: &str) -> Option<ParsedToolCall> {
    let trimmed = input.trim();
    let body = json_fence_body(trimmed).unwrap_or(trimmed).trim();
    if body.is_empty() || !(body.starts_with('{') || body.starts_with('[')) {
        return None;
    }

    let (name, name_end) = parse_json_string_field_after(body, "name", 0)?;
    if map_tool_name_alias(name.trim()) != "file_write" {
        return None;
    }

    let arguments_start = find_json_field_value_start(body, "arguments", name_end)
        .or_else(|| find_json_field_value_start(body, "parameters", name_end))?;
    let arguments = parse_malformed_file_write_arguments(&body[arguments_start..])?;

    Some(ParsedToolCall {
        name: "file_write".to_string(),
        arguments,
        tool_call_id: None,
    })
}

/// Find the end position of a JSON object by tracking balanced braces.
pub(crate) fn find_json_end(input: &str) -> Option<usize> {
    let trimmed = input.trim_start();
    let offset = input.len() - trimmed.len();

    if !trimmed.starts_with('{') {
        return None;
    }

    let mut depth = 0;
    let mut in_string = false;
    let mut escape_next = false;

    for (i, ch) in trimmed.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }

        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset + i + ch.len_utf8());
                }
            }
            _ => {}
        }
    }

    None
}
