//! Tool-protocol envelope classification: decides whether a JSON blob
//! or text prefix looks like a tool call, a declaration, an example,
//! or a malformed fragment.

use crate::toolparse::formats::map_tool_name_alias;
use crate::toolparse::parser::parse_tool_calls;
use crate::toolparse::protocol::{parse_arguments_value, ParsedToolCall, ToolProtocolEnvelopeKind};
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;


pub(crate) fn parse_tool_call_id(
    root: &serde_json::Value,
    function: Option<&serde_json::Value>,
) -> Option<String> {
    function
        .and_then(|func| func.get("id"))
        .or_else(|| root.get("id"))
        .or_else(|| root.get("tool_call_id"))
        .or_else(|| root.get("call_id"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

#[allow(dead_code)] // Phase 4: schema-guided argument recovery
pub fn canonicalize_json_for_tool_signature(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<String> = map.keys().cloned().collect();
            keys.sort_unstable();
            let mut ordered = serde_json::Map::new();
            for key in keys {
                if let Some(child) = map.get(&key) {
                    ordered.insert(key, canonicalize_json_for_tool_signature(child));
                }
            }
            serde_json::Value::Object(ordered)
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(canonicalize_json_for_tool_signature)
                .collect(),
        ),
        _ => value.clone(),
    }
}

pub(crate) fn parse_tool_call_value(value: &serde_json::Value) -> Option<ParsedToolCall> {
    if let Some(function) = value.get("function") {
        let tool_call_id = parse_tool_call_id(value, Some(function));
        let raw_name = function
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let name = map_tool_name_alias(raw_name).to_string();
        if !name.is_empty() {
            let arguments = parse_arguments_value(
                function
                    .get("arguments")
                    .or_else(|| function.get("parameters")),
            );
            return Some(ParsedToolCall {
                name,
                arguments,
                tool_call_id,
            });
        }
    }

    let tool_call_id = parse_tool_call_id(value, None);
    let raw_name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let name = map_tool_name_alias(raw_name).to_string();

    if name.is_empty() {
        return None;
    }

    let arguments =
        parse_arguments_value(value.get("arguments").or_else(|| value.get("parameters")));
    Some(ParsedToolCall {
        name,
        arguments,
        tool_call_id,
    })
}

pub(crate) fn parse_tool_calls_from_json_value(value: &serde_json::Value) -> Vec<ParsedToolCall> {
    let mut calls = Vec::new();

    if let Some(tool_calls) = value.get("tool_calls").and_then(|v| v.as_array()) {
        for call in tool_calls {
            if let Some(parsed) = parse_tool_call_value(call) {
                calls.push(parsed);
            }
        }

        if !calls.is_empty() {
            return calls;
        }
    }

    if let Some(array) = value.as_array() {
        for item in array {
            if let Some(parsed) = parse_tool_call_value(item) {
                calls.push(parsed);
            }
        }
        return calls;
    }

    if let Some(parsed) = parse_tool_call_value(value) {
        calls.push(parsed);
    }

    calls
}

pub(crate) fn has_non_empty_string(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

pub(crate) fn has_arguments_signal(value: &serde_json::Value) -> bool {
    value.get("arguments").is_some() || value.get("parameters").is_some()
}

pub(crate) fn looks_like_tool_call_object(value: &serde_json::Value) -> bool {
    if let Some(function) = value.get("function").and_then(serde_json::Value::as_object) {
        let function = serde_json::Value::Object(function.clone());
        return has_non_empty_string(&function, "name") && has_arguments_signal(&function);
    }

    has_non_empty_string(value, "name") && has_arguments_signal(value)
}

pub(crate) fn tool_call_array_has_protocol_shape(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| !items.is_empty() && items.iter().any(looks_like_tool_call_object))
}

pub(crate) fn has_tool_protocol_object_signal(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };

    let has_args = has_arguments_signal(value);
    let has_call_id = has_non_empty_string(value, "id")
        || has_non_empty_string(value, "call_id")
        || has_non_empty_string(value, "tool_call_id");

    object
        .get("function")
        .and_then(serde_json::Value::as_object)
        .is_some()
        || (has_non_empty_string(value, "name") && has_args)
        || (has_args && has_call_id)
}

pub(crate) fn tool_call_array_has_malformed_protocol_signal(value: &serde_json::Value, key: &str) -> bool {
    value
        .get(key)
        .and_then(serde_json::Value::as_array)
        .is_some_and(|items| !items.is_empty() && items.iter().any(has_tool_protocol_object_signal))
}

pub(crate) fn classify_tool_protocol_json_value(
    value: &serde_json::Value,
) -> Option<ToolProtocolEnvelopeKind> {
    if value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|ty| ty == "function_call")
        && has_non_empty_string(value, "name")
        && (has_arguments_signal(value) || has_non_empty_string(value, "call_id"))
    {
        return Some(ToolProtocolEnvelopeKind::ResponsesFunctionCall);
    }

    if tool_call_array_has_protocol_shape(value, "tool_calls") {
        return Some(ToolProtocolEnvelopeKind::ToolCalls);
    }

    if tool_call_array_has_protocol_shape(value, "toolcalls") {
        return Some(ToolProtocolEnvelopeKind::ToolCallsAlias);
    }

    if value
        .get("function_call")
        .is_some_and(looks_like_tool_call_object)
    {
        return Some(ToolProtocolEnvelopeKind::FunctionCall);
    }

    if has_non_empty_string(value, "tool_call_id")
        && (value.get("content").is_some()
            || value.get("result").is_some()
            || value.get("output").is_some())
    {
        return Some(ToolProtocolEnvelopeKind::ToolResult);
    }

    None
}

#[allow(dead_code)] // Phase 4: hallucinated-name detection helper
pub(crate) fn json_value_mentions_known_tool(
    value: &serde_json::Value,
    known_tool_names: &HashSet<String>,
) -> bool {
    if known_tool_names.is_empty() {
        return false;
    }

    let Some(object) = value.as_object() else {
        return value.as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| json_value_mentions_known_tool(item, known_tool_names))
        });
    };

    let name_matches = |candidate: Option<&serde_json::Value>| {
        candidate
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .is_some_and(|name| known_tool_names.contains(&name.to_ascii_lowercase()))
    };

    if name_matches(object.get("name")) {
        return true;
    }

    if let Some(function) = object
        .get("function")
        .and_then(serde_json::Value::as_object)
    {
        let function = serde_json::Value::Object(function.clone());
        if json_value_mentions_known_tool(&function, known_tool_names) {
            return true;
        }
    }

    if let Some(function_call) = object.get("function_call")
        && json_value_mentions_known_tool(function_call, known_tool_names)
    {
        return true;
    }

    ["tool_calls", "toolcalls"].iter().any(|key| {
        object
            .get(*key)
            .and_then(serde_json::Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .any(|item| json_value_mentions_known_tool(item, known_tool_names))
            })
    })
}

#[allow(dead_code)] // Phase 4: envelope-name filtering
pub fn tool_protocol_envelope_mentions_known_tool(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> bool {
    if known_tool_names.is_empty() {
        return false;
    }

    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return tool_protocol_envelope_mentions_known_tool(body, known_tool_names);
    }

    if starts_with_tool_protocol_tag_or_fence(trimmed) || contains_tool_protocol_tag_marker(trimmed)
    {
        let (_, calls) = parse_tool_calls(trimmed);
        if calls
            .iter()
            .any(|call| known_tool_names.contains(&call.name.to_ascii_lowercase()))
        {
            return true;
        }
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .is_ok_and(|value| json_value_mentions_known_tool(&value, known_tool_names))
}

pub(crate) fn has_malformed_tool_protocol_json_signal(value: &serde_json::Value) -> bool {
    // Empty `tool_calls: []` is a valid strict-provider compatibility case;
    // similar business JSON must also carry protocol-shaped fields before it
    // is withheld from user-visible output.
    tool_call_array_has_malformed_protocol_signal(value, "tool_calls")
        || tool_call_array_has_malformed_protocol_signal(value, "toolcalls")
        || value
            .get("function_call")
            .is_some_and(has_tool_protocol_object_signal)
        || (value
            .get("type")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|ty| ty == "function_call")
            && (has_non_empty_string(value, "name")
                || has_non_empty_string(value, "call_id")
                || has_arguments_signal(value)))
        || (has_non_empty_string(value, "tool_call_id")
            && (value.get("content").is_some()
                || value.get("result").is_some()
                || value.get("output").is_some()))
}

pub(crate) fn starts_with_tool_protocol_tag_or_fence(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    lower.starts_with("<tool_call")
        || lower.starts_with("<toolcall")
        || lower.starts_with("<tool-call")
        // `<tools>` only, not a `<tools` prefix: the prefix would also swallow
        // `<toolsomething>`, and unlike the aliases above this tag has a second
        // legitimate meaning (the Hermes tool *declaration* block), so it is
        // matched exactly and left to the example-guard below.
        || lower.starts_with("<tools>")
        || lower.starts_with("<invoke")
        || lower.starts_with("<functioncall")
        || lower.starts_with("<function_call")
        || starts_with_tool_protocol_fence_lower(&lower)
        || lower.starts_with("[tool_call]")
}

pub(crate) fn starts_with_tool_protocol_fence(text: &str) -> bool {
    let lower = text.trim_start().to_ascii_lowercase();
    starts_with_tool_protocol_fence_lower(&lower)
}

pub(crate) fn starts_with_tool_protocol_fence_lower(lower: &str) -> bool {
    lower.starts_with("```tool_call")
        || lower.starts_with("```toolcall")
        || lower.starts_with("```tool-call")
        || lower.starts_with("```invoke")
        || starts_with_tool_name_fence_lower(lower)
}

pub(crate) fn starts_with_tool_name_fence_lower(lower: &str) -> bool {
    let Some(rest) = lower.strip_prefix("```tool") else {
        return false;
    };
    matches!(rest.chars().next(), Some(c) if c.is_whitespace() && c != '\n' && c != '\r')
}

pub(crate) fn contains_tool_protocol_tag_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("<tool_call")
        || lower.contains("<toolcall")
        || lower.contains("<tool-call")
        || lower.contains("<tools>")
        || lower.contains("<invoke")
        || lower.contains("<functioncall")
        || lower.contains("<function_call")
        || lower.contains("```tool_call")
        || lower.contains("```toolcall")
        || lower.contains("```tool-call")
        || lower.contains("```invoke")
        || lower.contains("```tool ")
        || lower.contains("[tool_call]")
}

pub fn looks_like_tool_protocol_example(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if let Some((body, visible_text)) = leading_json_fence_body_and_trailing_text(trimmed)
        && classify_tool_protocol_envelope(body).is_some()
        && has_example_context(visible_text)
    {
        return true;
    }

    if starts_with_tool_protocol_fence(trimmed) || contains_tool_protocol_tag_marker(trimmed) {
        let (visible_text, calls) = parse_tool_calls(trimmed);
        if !calls.is_empty() && has_example_context(&visible_text) {
            return true;
        }
    }

    false
}

pub(crate) fn has_example_context(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("example")
        || lower.contains("sample")
        || lower.contains("示例")
        // Common Chinese "for example" / "sample" markers. We keep this list
        // intentionally small to avoid accidentally exempting real protocol leaks.
        || lower.contains("例如")
        || lower.contains("比如")
        || lower.contains("举例")
        || lower.contains("例子")
        || lower.contains("比方说")
        || lower.contains("譬如")
}

pub(crate) fn leading_json_fence_body_and_trailing_text(trimmed: &str) -> Option<(&str, &str)> {
    let rest = trimmed.strip_prefix("```")?;
    let first_newline = rest.find('\n')?;
    let language = rest[..first_newline].trim().trim_end_matches('\r');
    if !language.eq_ignore_ascii_case("json") {
        return None;
    }

    let body_with_close = &rest[first_newline + 1..];
    let close_start = body_with_close.find("```")?;
    let body = body_with_close[..close_start].trim();
    let trailing = body_with_close[close_start + 3..].trim();
    (!body.is_empty() && !trailing.is_empty()).then_some((body, trailing))
}

pub fn contains_tool_protocol_tag_call(text: &str) -> bool {
    if !contains_tool_protocol_tag_marker(text) || looks_like_tool_protocol_example(text) {
        return false;
    }

    let (_, calls) = parse_tool_calls(text);
    !calls.is_empty()
}

/// `text` carries a call `parse_tool_calls` would return, in any syntax
/// the parser supports, and is not a teaching example.
/// [`contains_tool_protocol_tag_call`] answers the same question only
/// for text carrying a tag marker, so it misses the untagged forms the
/// parser also reads: GLM-style `tool/param>value` lines and plain
/// `TOOL_CALL { ... }} /TOOL_CALL` blocks. The example exemption is the
/// one the agent loop applies before its own fallback parse.
pub fn contains_parseable_tool_call(text: &str) -> bool {
    if looks_like_tool_protocol_example(text) {
        return false;
    }

    let (_, calls) = parse_tool_calls(text);
    !calls.is_empty()
}

/// `text` opens call syntax the parser reads, whether or not the call has
/// closed yet: one of the tag markers the tagged check looks for, or a
/// `TOOL_CALL` / `[TOOL_CALL]` block opener. `TOOL_CALL` is matched
/// case-sensitively, as the parser's own malformed-call check matches it,
/// so the `tool_call_id` key does not count. A span carrying an opener
/// cannot be judged as a quotation on its own, because the closer the
/// call needs may arrive after the span.
pub fn contains_tool_call_opener(text: &str) -> bool {
    contains_tool_protocol_tag_marker(text) || text.contains("TOOL_CALL")
}

pub(crate) fn classify_tagged_tool_protocol_envelope(text: &str) -> Option<ToolProtocolEnvelopeKind> {
    if !starts_with_tool_protocol_tag_or_fence(text) {
        return None;
    }
    if looks_like_tool_protocol_example(text) {
        return None;
    }

    let is_fence = starts_with_tool_protocol_fence(text);
    let (visible_text, calls) = parse_tool_calls(text);
    (!calls.is_empty() && (is_fence || visible_text.trim().is_empty()))
        .then_some(ToolProtocolEnvelopeKind::TaggedToolCall)
}

pub(crate) fn looks_like_malformed_tagged_tool_protocol_envelope(text: &str) -> bool {
    if !starts_with_tool_protocol_tag_or_fence(text) {
        return false;
    }
    if looks_like_tool_protocol_example(text) {
        return false;
    }

    let (visible_text, calls) = parse_tool_calls(text);
    if !calls.is_empty() || !visible_text.trim().is_empty() {
        return false;
    }

    let lower = text.to_ascii_lowercase();
    lower.contains("arguments")
        || lower.contains("parameters")
        || lower.contains("function")
        || lower.contains("name")
        || lower.contains("call_id")
        || lower.contains("tool_call_id")
}

/// JSON keys naming a tool-call container. Business JSON does not carry these.
pub(crate) const TOOL_PROTOCOL_CONTAINER_KEYS: [&str; 3] =
    ["\"tool_calls\"", "\"toolcalls\"", "\"function_call\""];

/// JSON keys carrying a tool call's correlation id.
pub(crate) const TOOL_PROTOCOL_CALL_ID_KEYS: [&str; 2] = ["\"call_id\"", "\"tool_call_id\""];

/// Every key that identifies a payload as tool protocol on its own.
pub(crate) fn tool_protocol_json_identifying_keys() -> impl Iterator<Item = &'static str> {
    TOOL_PROTOCOL_CONTAINER_KEYS
        .into_iter()
        .chain(TOOL_PROTOCOL_CALL_ID_KEYS)
}

pub(crate) fn has_malformed_tool_protocol_text_signal(text: &str) -> bool {
    let trimmed = text.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if !json_like {
        return false;
    }

    // Malformed text cannot be parsed into a Value, so keep the tool-result
    // signal close to the valid-envelope shape to avoid business JSON false positives.
    let has_tool_result_shape = text.contains("\"tool_call_id\"")
        && (text.contains("\"content\"")
            || text.contains("\"result\"")
            || text.contains("\"output\""));
    let has_protocol_container = TOOL_PROTOCOL_CONTAINER_KEYS
        .iter()
        .any(|key| text.contains(key));
    let has_arguments = text.contains("\"arguments\"") || text.contains("\"parameters\"");
    let has_call_id = TOOL_PROTOCOL_CALL_ID_KEYS
        .iter()
        .any(|key| text.contains(key));

    has_tool_result_shape || (has_protocol_container && has_arguments && has_call_id)
}

/// Whether `text` is a tool-protocol JSON payload that has not finished
/// arriving.
///
/// The completed-envelope classifiers need the whole value: they parse it, or
/// they look for a corroborating second key. A streaming consumer cannot wait
/// for either — the frame it is deciding about is on screen now, and the key
/// that gives the payload away may be the only one that has arrived. So this
/// deliberately trips on a *single* protocol key.
///
/// Being eager is the safe direction here and not in the completed case: a
/// held-back partial costs one frame and is re-rendered by the next delta,
/// whereas a rendered protocol envelope stays visible until the turn ends, or
/// indefinitely if the turn fails first. `false` for anything that already
/// parses as a complete JSON value, which the ordinary classifiers then judge
/// on their own terms.
pub fn looks_like_incomplete_tool_protocol_json(text: &str) -> bool {
    let trimmed = text.trim();
    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if !json_like {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_incomplete_tool_protocol_json(body);
    }

    // A complete value is not this function's business.
    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return false;
    }

    tool_protocol_json_identifying_keys().any(|key| trimmed.contains(key))
}

#[allow(dead_code)] // Phase 4: malformed-envelope name detection
pub(crate) fn malformed_text_mentions_known_tool(text: &str, known_tool_names: &HashSet<String>) -> bool {
    if known_tool_names.is_empty() {
        return false;
    }

    static JSON_NAME_FIELD_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#""name"\s*:\s*"([^"]+)""#).expect("JSON_NAME_FIELD_RE regex must compile")
    });

    JSON_NAME_FIELD_RE.captures_iter(text).any(|cap| {
        cap.get(1)
            .map(|name| name.as_str().trim().to_ascii_lowercase())
            .is_some_and(|name| known_tool_names.contains(&name))
    })
}

#[allow(dead_code)] // Phase 4: stream hold-back signal
pub(crate) fn has_malformed_tool_protocol_text_signal_for_known_tools(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> bool {
    if has_malformed_tool_protocol_text_signal(text) {
        return true;
    }

    let trimmed = text.trim_start();
    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if !json_like {
        return false;
    }

    let has_protocol_container = text.contains("\"tool_calls\"")
        || text.contains("\"toolcalls\"")
        || text.contains("\"function_call\"");
    let has_arguments = text.contains("\"arguments\"") || text.contains("\"parameters\"");

    has_protocol_container
        && has_arguments
        && malformed_text_mentions_known_tool(text, known_tool_names)
}

pub(crate) fn json_fence_body(trimmed: &str) -> Option<&str> {
    let rest = trimmed.strip_prefix("```")?;
    let first_newline = rest.find('\n')?;
    let language = rest[..first_newline].trim().trim_end_matches('\r');
    if !language.eq_ignore_ascii_case("json") {
        return None;
    }

    let body_with_close = &rest[first_newline + 1..];
    let close_start = body_with_close.rfind("```")?;
    if !body_with_close[close_start + 3..].trim().is_empty() {
        return None;
    }
    Some(body_with_close[..close_start].trim())
}

pub fn classify_tool_protocol_envelope(text: &str) -> Option<ToolProtocolEnvelopeKind> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(kind) = classify_tagged_tool_protocol_envelope(trimmed) {
        return Some(kind);
    }

    if let Some(body) = json_fence_body(trimmed) {
        return classify_tool_protocol_envelope(body);
    }

    let value = serde_json::from_str::<serde_json::Value>(trimmed).ok()?;
    classify_tool_protocol_json_value(&value)
}

pub fn looks_like_tool_protocol_envelope(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    if classify_tool_protocol_envelope(trimmed).is_some() {
        return true;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_tool_protocol_envelope(body);
    }

    serde_json::from_str::<serde_json::Value>(trimmed)
        .is_ok_and(|value| has_malformed_tool_protocol_json_signal(&value))
}

pub fn looks_like_malformed_tool_protocol_envelope(text: &str) -> bool {
    let trimmed = text.trim();
    if looks_like_tool_protocol_example(trimmed) {
        return false;
    }

    if looks_like_malformed_tagged_tool_protocol_envelope(trimmed) {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if trimmed.is_empty() || !json_like {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_malformed_tool_protocol_envelope(body);
    }

    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return false;
    }

    has_malformed_tool_protocol_text_signal(trimmed)
}

#[allow(dead_code)] // Phase 4: stream hold-back guard
pub fn looks_like_malformed_tool_protocol_envelope_for_known_tools(
    text: &str,
    known_tool_names: &HashSet<String>,
) -> bool {
    let trimmed = text.trim();
    if looks_like_tool_protocol_example(trimmed) {
        return false;
    }

    if looks_like_malformed_tool_protocol_envelope(trimmed) {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    let json_like =
        trimmed.starts_with('{') || trimmed.starts_with('[') || lower.starts_with("```json");
    if trimmed.is_empty() || !json_like {
        return false;
    }

    if let Some(body) = json_fence_body(trimmed) {
        return looks_like_malformed_tool_protocol_envelope_for_known_tools(body, known_tool_names);
    }

    if serde_json::from_str::<serde_json::Value>(trimmed).is_ok() {
        return false;
    }

    has_malformed_tool_protocol_text_signal_for_known_tools(trimmed, known_tool_names)
}
