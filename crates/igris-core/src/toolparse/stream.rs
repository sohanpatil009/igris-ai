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
/* never be part of a tool-call envelope, a think span, a terminal    */
/* marker, or a hallucinated bare tool-result JSON object comes back  */
/* out. Protocol-shaped spans are withheld until complete and then    */
/* dropped, so an envelope is never spoken aloud or printed. A bare   */
/* JSON object whose keys spell the fake-result signature (name +     */
/* result/status) is dropped at its closing brace (BUG-003); other    */
/* JSON objects release verbatim once they close. Parsing still runs  */
/* on the full raw text, so suppression here can never affect         */
/* execution. False positives cost a little latency (text releases a  */
/* chunk late); false negatives would leak protocol text to the       */
/* voice — the filter is conservative.                                */
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
    /* BUG-003: an object start in progress (a `{`, optionally followed
     * by whitespace) must stay pending until its next char arrives and
     * the JSON trigger can decide drop vs release. */
    let trimmed = text.trim_end();
    if trimmed.ends_with('{') {
        max_hold = max_hold.max(text.len() - trimmed.len() + 1);
    } else if trimmed.ends_with("{\u{22}") {
        max_hold = max_hold.max(text.len() - trimmed.len() + 2);
    }
    max_hold
}

/// BUG-003: the trigger that starts a bare-JSON hold — a `{` whose next
/// non-whitespace char is a `"` (a JSON object spell). Prose braces
/// (`{quick}`) and arrays never trigger; a JSON *string* mention does,
/// costing only release latency until the object closes.
pub(crate) fn json_trigger_index(text: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(rel) = text[from..].find('{') {
        let i = from + rel;
        if text[i + 1..].trim_start().starts_with('"') {
            return Some(i);
        }
        from = i + 1;
    }
    None
}

/// Hold state for a bare JSON object that may be a hallucinated inline
/// tool result (BUG-003). The scanner walks the object byte-wise,
/// tracking strings (so braces inside values cannot fake the depth) and
/// the fake-result key spell.
struct JsonHold {
    /// bytes of pending already structurally scanned
    scanned: usize,
    /// first byte of the current JSON string (just after its opening quote)
    str_start: usize,
    /// object/array depth outside strings
    depth: u32,
    in_string: bool,
    escaped: bool,
    saw_name: bool,
    saw_result: bool,
    saw_status: bool,
    /// a protocol span/marker appeared inside the held object: suppress
    /// the whole object at its close regardless of the key spell
    contaminated: bool,
}

impl JsonHold {
    fn new() -> Self {
        JsonHold {
            scanned: 0,
            str_start: 0,
            depth: 0,
            in_string: false,
            escaped: false,
            saw_name: false,
            saw_result: false,
            saw_status: false,
            contaminated: false,
        }
    }

    /// Fake-result signature: the observed hallucination spells
    /// `{"name": ..., "result": ..., "status": "ok"}`; `name` plus
    /// either companion key is machinery-shaped enough to suppress.
    fn has_signature(&self) -> bool {
        self.saw_name && (self.saw_result || self.saw_status)
    }

    fn must_drop(&self) -> bool {
        self.contaminated || self.has_signature()
    }
}

/// One json_step decision on the held object.
enum JsonStep {
    /// object still open: keep holding
    Held,
    /// object finished at this pending index: drop (machinery)
    Drop(usize),
    /// object finished at this pending index: release verbatim
    Release(usize),
}

/// Advance the hold scanner over `pending` and decide at the object's
/// close (or at end-of-stream if it never closes).
fn json_step(h: &mut JsonHold, pending: &str, at_end: bool) -> JsonStep {
    let bytes = pending.as_bytes();
    while h.scanned < pending.len() {
        let b = bytes[h.scanned];
        if h.in_string {
            if h.escaped {
                h.escaped = false;
            } else if b == b'\\' {
                h.escaped = true;
            } else if b == b'"' {
                h.in_string = false;
                match &pending[h.str_start..h.scanned] {
                    "name" => h.saw_name = true,
                    "result" => h.saw_result = true,
                    "status" => h.saw_status = true,
                    _ => {}
                }
            }
        } else {
            match b {
                b'"' => {
                    h.in_string = true;
                    h.str_start = h.scanned + 1;
                }
                b'{' => h.depth += 1,
                b'}' => {
                    h.depth = h.depth.saturating_sub(1);
                    if h.depth == 0 {
                        let end = h.scanned + 1;
                        return if h.must_drop() {
                            JsonStep::Drop(end)
                        } else {
                            JsonStep::Release(end)
                        };
                    }
                }
                _ => {}
            }
        }
        h.scanned += 1;
    }
    if at_end {
        /* stream over before the object closed: machinery-shaped is
         * suppressed; otherwise released verbatim (never lose text) */
        let end = pending.len();
        return if h.must_drop() {
            JsonStep::Drop(end)
        } else {
            JsonStep::Release(end)
        };
    }
    JsonStep::Held
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
    /// BUG-003: while a bare JSON object may be fake tool machinery:
    /// its hold state.
    json_hold: Option<JsonHold>,
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
            json_hold: None,
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
            if let Some(h) = self.json_hold.as_mut() {
                /* protocol spans outrank the JSON hold: an opener INSIDE
                 * the held object marks it contaminated and is suppressed
                 * with the object at its close (a span inside a bare JSON
                 * object is machinery noise either way) */
                if earliest_stream_opener(&self.pending[h.scanned..]).is_some()
                    || earliest_stream_closer(&self.pending[h.scanned..]).is_some()
                {
                    h.contaminated = true;
                }
                match json_step(h, &self.pending, at_end) {
                    JsonStep::Held => break,
                    JsonStep::Drop(end) => {
                        self.pending.drain(..end);
                        self.json_hold = None;
                        continue;
                    }
                    JsonStep::Release(end) => {
                        out.push_str(&self.pending[..end]);
                        self.pending.drain(..end);
                        self.json_hold = None;
                        continue;
                    }
                }
            }
            let opener = earliest_stream_opener(&self.pending);
            /* a stray close without an opener — or before one — is
             * protocol noise (its span was closed in an earlier round and
             * already suppressed): drop it */
            if let Some((ci, cl_len)) = earliest_stream_closer(&self.pending) {
                let precedes_opener = match opener {
                    Some((oi, _, _)) => ci <= oi,
                    None => true,
                };
                if precedes_opener {
                    out.push_str(&self.pending[..ci]);
                    self.pending.drain(..ci + cl_len);
                    continue;
                }
            }
            /* BUG-003: a bare JSON object start holds until the object
             * closes and the fake-result signature decides drop vs
             * release. Never preempt an opener that PRECEDES it — an
             * envelope's arguments object must stay inside the span
             * machinery, not start a JSON hold. */
            if let Some(i) = json_trigger_index(&self.pending) {
                let opener_preempts = match opener {
                    Some((oi, _, _)) => oi < i,
                    None => false,
                };
                if !opener_preempts {
                    out.push_str(&self.pending[..i]);
                    self.pending.drain(..i);
                    self.json_hold = Some(JsonHold::new());
                    continue;
                }
            }
            match opener {
                Some((i, open_len, opener)) => {
                    out.push_str(&self.pending[..i]);
                    if let StreamOpener::Span(close) = opener {
                        self.span_close = Some(close);
                    }
                    self.pending.drain(..i + open_len);
                }
                None => {
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
