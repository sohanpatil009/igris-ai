//! Tool call parsing for LLM responses.
//!
//! Vendored from zeroclaw-labs/zeroclaw, commit
//! 2c0a8c26354b7fbd52fc8ff52cb1ba81c6fcaf0c (2026-11-12):
//! `crates/zeroclaw-tool-call-parser/src/lib.rs`
//! License: MIT OR Apache-2.0 (upstream workspace license).
//!
//! Vendoring changes vs upstream:
//! - `zeroclaw_api::strip_think_tags` re-export replaced with an in-file port
//!   (api/src/model_provider.rs:237-254) extended for LFM3-1.7B think spans
//!   (`<Token 1437>` ... `<Token 9550>`).
//! - `zeroclaw_log` warn sites (upstream lib.rs 2230 / 2426 + helper 1870)
//!   stubbed with `eprintln!`; the log-capture test was dropped.
//! - `map_tool_name_alias` trimmed to identity (+ dotted-prefix strip).
//! - `TERMINAL_MARKERS` extended with `<|eot_id|>` (llama.cpp leak).
//! - Doc examples keep the upstream crate name (`use
//!   zeroclaw_tool_call_parser::...`); they are inert in a bin target.

mod envelope;
mod formats;
mod parser;
mod protocol;
#[cfg(test)]
mod tests;
mod think;
mod scan;
mod stream;
#[cfg(test)]
mod tools_wrapper_body_boundary_tests;
mod xml;

pub use envelope::{
    canonicalize_json_for_tool_signature, classify_tool_protocol_envelope,
    contains_parseable_tool_call, contains_tool_call_opener, contains_tool_protocol_tag_call,
    looks_like_incomplete_tool_protocol_json, looks_like_malformed_tool_protocol_envelope,
    looks_like_malformed_tool_protocol_envelope_for_known_tools, looks_like_tool_protocol_envelope,
    looks_like_tool_protocol_example, tool_protocol_envelope_mentions_known_tool,
};
pub use parser::{
    build_native_assistant_history_from_parsed_calls, detect_tool_call_parse_issue,
    parse_tool_calls, strip_tool_result_blocks,
};
pub use protocol::{
    DSML_CLOSE, DSML_OPEN, ParsedToolCall, TERMINAL_MARKERS, TOOL_CALL_CLOSE, TOOL_CALL_OPEN,
    TOOL_RESULT_CLOSE, TOOL_RESULT_OPEN, ToolProtocolEnvelopeKind, strip_trailing_terminal_markers,
};
pub use stream::StreamFilter;
pub use think::strip_think_tags;

pub(crate) use envelope::*;
pub(crate) use formats::*;
pub(crate) use scan::*;
pub(crate) use xml::*;
#[cfg(test)]
pub(crate) use parser::*;
#[cfg(test)]
pub(crate) use protocol::*;
#[cfg(test)]
pub(crate) use think::*;
