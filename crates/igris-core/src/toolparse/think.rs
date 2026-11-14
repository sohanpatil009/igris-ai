//! Think-span stripping (vendored port + LFM3-1.7B extension).


pub(crate) const THINK_OPEN: &str = "<think>";
pub(crate) const THINK_CLOSE: &str = "</think>";
/// LFM3-1.7B via llama.cpp renders its think block using these special-token
/// IDs as literal text (see tool_calling_plan.md §3.1 / §3.5).
pub(crate) const LFM3_THINK_OPEN: &str = "<Token 1437>";
pub(crate) const LFM3_THINK_CLOSE: &str = "<Token 9550>";

/// Remove inline reasoning spans before terminal-response classification or
/// user-visible parsing. Covers both standard `<think>`/`</think>` and the
/// LFM3 `<Token 1437>` ... `<Token 9550>` form.
///
/// An unclosed opening tag suppresses the remainder so partial reasoning never
/// becomes final output.
pub fn strip_think_tags(text: &str) -> String {
    let stripped = strip_think_pairs(text, THINK_OPEN, THINK_CLOSE);
    let stripped = strip_think_pairs(&stripped, LFM3_THINK_OPEN, LFM3_THINK_CLOSE);
    stripped.trim().to_string()
}

/// Upstream `strip_think_tags` loop (api/src/model_provider.rs:237-254),
/// generalized over the open/close delimiters.
pub(crate) fn strip_think_pairs(text: &str, open: &str, close: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut remaining = text;
    loop {
        if let Some(start) = remaining.find(open) {
            result.push_str(&remaining[..start]);
            if let Some(end) = remaining[start..].find(close) {
                remaining = &remaining[start + end + close.len()..];
            } else {
                break;
            }
        } else {
            result.push_str(remaining);
            break;
        }
    }
    result
}
