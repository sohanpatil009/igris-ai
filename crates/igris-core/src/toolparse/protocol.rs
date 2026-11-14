//! Core protocol types and constants: ParsedToolCall, envelope kinds,
//! argument normalization, terminal markers, canonical protocol tags.


/// A single parsed tool call extracted from LLM output.
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    pub name: String,
    pub arguments: serde_json::Value,
    pub tool_call_id: Option<String>,
}

/// Internal tool protocol envelope variants that must not be treated as
/// user-visible channel text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolProtocolEnvelopeKind {
    ToolCalls,
    ToolCallsAlias,
    FunctionCall,
    ToolResult,
    ResponsesFunctionCall,
    TaggedToolCall,
}

pub(crate) fn parse_arguments_value(raw: Option<&serde_json::Value>) -> serde_json::Value {
    match raw {
        Some(serde_json::Value::String(s)) => serde_json::from_str::<serde_json::Value>(s)
            .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new())),
        Some(value) => value.clone(),
        None => serde_json::Value::Object(serde_json::Map::new()),
    }
}

/// Canonical vocabulary of terminal markers emitted by providers that must be
/// stripped from final response text. This is the single source of truth for
/// the marker vocabulary, shared by the non-streaming
/// [`strip_trailing_terminal_markers`] helper and the streaming state machine
/// (`zeroclaw-runtime`'s `StreamTerminalMarkerStripper`) so a vocabulary or
/// matching-rule change cannot drift one path.
///
/// Order matters: longer spellings must precede shorter ones so a suffix check
/// matches the most specific marker first.
pub const TERMINAL_MARKERS: [&str; 3] = ["<|eot_id|>", "<|eom|>", "<eom>"];

/// Canonical tool-call protocol tags (the spellings ZeroClaw's
/// tool_call_format protocol block uses), spelled with unicode escapes so
/// this source file carries no raw tag text — safe under tooling that
/// treats tag-like spans specially. Consumers (llm.rs prompt builder)
/// reference these instead of re-spelling the tags.
pub const TOOL_CALL_OPEN: &str = "\u{3c}tool_call\u{3e}";
pub const TOOL_CALL_CLOSE: &str = "\u{3c}/tool_call\u{3e}";
/// Results wrapper named inside the protocol instructions text.
pub const TOOL_RESULT_OPEN: &str = "\u{3c}tool_result\u{3e}";
/// Close of the results wrapper (stream filter + carrier blocks).
pub const TOOL_RESULT_CLOSE: &str = "\u{3c}/tool_result\u{3e}";

/// LFM3's native DSML tool-call envelope, observed live 2026-11-14: opener
/// token, then a `function_calls>` label, then a JSON object with
/// `name`/`arguments`, then the closer. The fullwidth bars (U+FF5C) are
/// part of the model's special-token spelling. Spelled with unicode escapes
/// like the other tag consts so this file carries no raw tag text.
///
/// Policy (evidence-based, 2026-11-14 debug e2e): this envelope is NOT a
/// parse family — every DSML body the model emitted was schema-confused
/// (the JSON schema echoed as the arguments value), so parsing it would
/// dispatch garbage and the resulting "missing parameter" error reliably
/// sent the model into a hallucination loop (fake inline tool results,
/// nested garbled spans, repeat presses). The envelope is instead:
/// (1) held back by the StreamFilter so it never reaches TTS/terminal, and
/// (2) flagged by `detect_tool_call_parse_issue` so the agent loop issues
/// a corrective verdict that re-anchors the model to the protocol-block
/// format, whose argument shapes actually dispatch.
pub const DSML_OPEN: &str = "\u{3c}\u{FF5C}DSML\u{FF5C}\u{3e}";
/// Close of the LFM3 native DSML tool-call envelope.
pub const DSML_CLOSE: &str = "\u{3c}/\u{FF5C}DSML\u{FF5C}\u{3e}";

/// Strip trailing terminal markers (`<eom>`, `<|eom|>`) from a response string.
/// Handles stacked markers with arbitrary whitespace between them.
///
/// Terminal markers are protocol metadata that should not appear in user-visible text.
/// This function iteratively removes trailing markers and whitespace until none remain.
///
/// # Examples
///
/// ```
/// use igris_core::toolparse::strip_trailing_terminal_markers;
///
/// assert_eq!(strip_trailing_terminal_markers("Summary<eom>"), "Summary");
/// assert_eq!(strip_trailing_terminal_markers("Summary<|eom|>"), "Summary");
/// assert_eq!(strip_trailing_terminal_markers("Summary<eom><|eom|>"), "Summary");
/// assert_eq!(strip_trailing_terminal_markers("Summary<eom>  \n"), "Summary");
/// assert_eq!(strip_trailing_terminal_markers("Summary<eom>           <|eom|>"), "Summary");
/// assert_eq!(strip_trailing_terminal_markers("Text with <eom> inline"), "Text with <eom> inline");
/// ```
pub fn strip_trailing_terminal_markers(text: &str) -> String {
    let mut result = text.to_string();

    loop {
        // Look for a recognized marker at the end of the trailing-whitespace-
        // trimmed tail. When the trimmed tail ends in a marker, remove the
        // marker AND the whitespace that followed it (the marker suffix).
        // Whitespace BEFORE the marker belongs to the response text and is
        // preserved — this matches the streaming stripper, which keeps e.g.
        // `"Answer\n<eom>"` as `"Answer\n"`. If no marker is found, unmarked
        // trailing whitespace (e.g. `"Answer\n"`) is ordinary text and is
        // preserved too, so the two paths share one policy.
        let trimmed = result.trim_end();
        let mut stripped = false;
        for marker in TERMINAL_MARKERS {
            if let Some(prefix) = trimmed.strip_suffix(marker) {
                result = prefix.to_string();
                stripped = true;
                break;
            }
        }
        if !stripped {
            break;
        }
    }

    result
}
