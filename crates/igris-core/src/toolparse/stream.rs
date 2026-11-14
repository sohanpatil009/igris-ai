//! Streaming hold-back filter (our addition, not vendored). Withholds
//! protocol-shaped spans from display/TTS; parsing still runs on the
//! full raw text. The section banner below carries the design contract.

use crate::toolparse::protocol::{
    DSML_CLOSE, DSML_OPEN, TERMINAL_MARKERS, TOOL_CALL_CLOSE, TOOL_CALL_OPEN, TOOL_RESULT_CLOSE,
    TOOL_RESULT_OPEN,
};
use crate::toolparse::think::{LFM3_THINK_CLOSE, LFM3_THINK_OPEN, THINK_CLOSE, THINK_OPEN};


/* ================================================================ */
/* streaming hold-back filter — OUR ADDITION (not vendored)          */
/*                                                                   */
/* ZeroClaw's stream_guard concept: tokens go in, only text that can  */
/* never be part of a tool-call envelope, a think span, or a terminal */
/* marker comes back out. Protocol-shaped spans are withheld until    */
/* complete and then dropped, so an envelope is never spoken aloud or */
/* printed. Parsing still runs on the full raw text, so suppression   */
/* here can never affect execution. False positives cost a little     */
/* latency (text releases a chunk late); false negatives would leak   */
/* protocol text to the voice — the filter is conservative.           */
/* ================================================================ */

/// A protocol opener found on the stream.
#[derive(Clone, Copy)]
pub(crate) enum StreamOpener {
    /// Open a dropped span that ends at this closer.
    Span(&'static str),
    /// Bare drop (terminal marker): only the marker bytes vanish.
    Marker,
}

/// Earliest protocol opener in `text`:
/// `(byte index, opener length, opener)`.
pub(crate) fn earliest_stream_opener(text: &str) -> Option<(usize, usize, StreamOpener)> {
    let span_pairs: [(&str, &'static str); 5] = [
        (THINK_OPEN, THINK_CLOSE),
        (LFM3_THINK_OPEN, LFM3_THINK_CLOSE),
        (TOOL_CALL_OPEN, TOOL_CALL_CLOSE),
        (TOOL_RESULT_OPEN, TOOL_RESULT_CLOSE),
        (DSML_OPEN, DSML_CLOSE),
    ];
    let mut best: Option<(usize, usize, StreamOpener)> = None;
    for (open, close) in span_pairs {
        if let Some(i) = text.find(open) {
            let better = match &best {
                Some((bi, _, _)) => i < *bi,
                None => true,
            };
            if better {
                best = Some((i, open.len(), StreamOpener::Span(close)));
            }
        }
    }
    for marker in TERMINAL_MARKERS {
        if let Some(i) = text.find(marker) {
            let better = match &best {
                Some((bi, _, _)) => i < *bi,
                None => true,
            };
            if better {
                best = Some((i, marker.len(), StreamOpener::Marker));
            }
        }
    }
    best
}

/// Length of the longest suffix of `text` that is a strict prefix of any
/// opener tag, close tag, or terminal marker — the bytes held back across
/// chunk boundaries. Every candidate is ASCII except the DSML spellings
/// (their fullwidth bars are multibyte), so only char-boundary prefixes
/// are considered.
pub(crate) fn stream_holdback_len(text: &str) -> usize {
    let candidates: [&str; 13] = [
        THINK_OPEN,
        LFM3_THINK_OPEN,
        TOOL_CALL_OPEN,
        TOOL_RESULT_OPEN,
        THINK_CLOSE,
        LFM3_THINK_CLOSE,
        TOOL_CALL_CLOSE,
        TOOL_RESULT_CLOSE,
        DSML_OPEN,
        DSML_CLOSE,
        TERMINAL_MARKERS[0],
        TERMINAL_MARKERS[1],
        TERMINAL_MARKERS[2],
    ];
    let mut max_hold = 0;
    for tag in candidates {
        for k in 1..tag.len() {
            /* the DSML spellings are multibyte, so skip offsets that are
             * not char boundaries — slicing there would panic */
            if !tag.is_char_boundary(k) {
                continue;
            }
            if text.ends_with(&tag[..k]) {
                max_hold = max_hold.max(k);
            }
        }
    }
    max_hold
}

/// Earliest known close tag in `text` while not inside a span:
/// `(byte index, close-tag length, which close)`. A close without its
/// opener is protocol noise (the opener was in an earlier round whose
/// filter already dropped the unclosed span) — the stream filter skips it.
pub(crate) fn earliest_stream_closer(text: &str) -> Option<(usize, usize)> {
    let closes: [&str; 5] = [
        THINK_CLOSE,
        LFM3_THINK_CLOSE,
        TOOL_CALL_CLOSE,
        TOOL_RESULT_CLOSE,
        DSML_CLOSE,
    ];
    let mut best: Option<(usize, usize)> = None;
    for close in closes {
        if let Some(i) = text.find(close) {
            let better = match &best {
                Some((bi, _)) => i < *bi,
                None => true,
            };
            if better {
                best = Some((i, close.len()));
            }
        }
    }
    best
}

/// Token-in / clean-text-out filter for streamed LLM replies.
pub struct StreamFilter {
    pending: String,
    /// While inside a dropped span: the closer we are waiting for.
    span_close: Option<&'static str>,
}

impl Default for StreamFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFilter {
    pub fn new() -> Self {
        StreamFilter {
            pending: String::new(),
            span_close: None,
        }
    }

    /// Feed one streamed token; returns the text now safe to emit.
    pub fn feed(&mut self, tok: &str) -> String {
        self.pending.push_str(tok);
        self.drain(false)
    }

    /// End of stream: release anything still releasable. A partial
    /// opener/marker tail is released verbatim (never lose text); the
    /// remainder of an unclosed span is dropped (suppression policy).
    pub fn finish(&mut self) -> String {
        let out = self.drain(true);
        self.pending.clear();
        out
    }

    fn drain(&mut self, at_end: bool) -> String {
        let mut out = String::new();
        loop {
            if let Some(close) = self.span_close {
                if let Some(j) = self.pending.find(close) {
                    self.pending.drain(..j + close.len());
                    self.span_close = None;
                    continue;
                }
                /* span continues in later tokens; dropped at finish */
                break;
            }
            match earliest_stream_opener(&self.pending) {
                Some((i, open_len, opener)) => {
                    /* a stray close without an opener comes first: drop
                     * it as protocol noise (its span was closed in an
                     * earlier round and already suppressed) */
                    if let Some((ci, cl_len)) = earliest_stream_closer(&self.pending)
                        && ci <= i
                    {
                        out.push_str(&self.pending[..ci]);
                        self.pending.drain(..ci + cl_len);
                        continue;
                    }
                    out.push_str(&self.pending[..i]);
                    if let StreamOpener::Span(close) = opener {
                        self.span_close = Some(close);
                    }
                    self.pending.drain(..i + open_len);
                }
                None => {
                    if let Some((ci, cl_len)) = earliest_stream_closer(&self.pending) {
                        out.push_str(&self.pending[..ci]);
                        self.pending.drain(..ci + cl_len);
                        continue;
                    }
                    let hold = if at_end {
                        0
                    } else {
                        stream_holdback_len(&self.pending)
                    };
                    let release = self.pending.len() - hold;
                    out.push_str(&self.pending[..release]);
                    self.pending.drain(..release);
                    break;
                }
            }
        }
        out
    }
}
