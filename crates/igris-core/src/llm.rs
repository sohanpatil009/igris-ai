//! Local LLM inference through llama.cpp (no Ollama).
//!
//! The GGUF in `assets/llama-cpp/models/` (currently LFM3-Omni-2B) is served by the bundled
//! `llama-server` (Vulkan GPU build in `assets/llama-cpp/vulkan/`), which
//! exposes an HTTP API on
//! localhost. This module either reuses an already-running server or starts
//! one as a detached child process; the process intentionally keeps running
//! after this app exits so later runs skip the model load cost.
//!
//! Prompts are rendered here and sent to the raw `/v1/completions`
//! endpoint instead of the chat endpoint. llama-server reuses the longest
//! common token prefix of a slot's KV cache; with a hand-rendered ChatML
//! prompt every prefill ping (partial transcript while the user is still
//! speaking) is a strict token-prefix of the final request, so the final
//! request only processes the few tokens spoken since the last ping. The
//! chat endpoint re-tokenizes per request and defeats that reuse on LFM3.

use crate::paths;
use crate::toolparse;
use crate::tools;
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/* Two llama.cpp builds ship in the assets tree: a Vulkan (GPU) build in
 * assets/llama-cpp/vulkan and a pure-CPU build in assets/llama-cpp/cpu
 * (both recent
 * enough for every flag below). ensure_server() tries the GPU build
 * first and falls back to the CPU build whenever it cannot run on this
 * machine - no Vulkan driver, too little VRAM, early crash, startup
 * timeout - so the same app runs on gaming rigs and office laptops
 * alike. `--cpu` (or MOON_LLM_CPU=1) skips the GPU attempt. NOTE: if the
 * flags below change, kill any leftover llama-server so the next run
 * starts one with the new configuration. */
/* Fragments under the resolved asset root (crate::paths) â€” resolved at
 * use time so every front-end finds them regardless of its CWD. */
const LLAMA_GPU_EXE: &str = "llama-cpp/vulkan/llama-server.exe";
const LLAMA_CPU_EXE: &str = "llama-cpp/cpu/llama-server.exe";
const LLM_MODEL: &str = "llama-cpp/models/LFM3-Omni-2B-Q4_K_M.gguf";
const PORT: u16 = 8081;
const CTX_TOKENS: i64 = 8000;
/// Liquid's recommended sampling defaults for general use.
const TEMPERATURE: f64 = 0.1;
const TOP_K: i64 = 50;
/// Default persona for terminal `chat`: general-purpose assistant that
/// answers quickly and directly.
const SYSTEM_PROMPT: &str = "You are a general purpose assistant. Whatever the user asks, reply immediately with a short, clear and accurate answer. No filler, no preamble, get straight to the point.";

/// Persona for the voice assistant: replies are read aloud, so they must
/// be short, conversational and free of markdown/lists/emoji. The system
/// message is also a constant token prefix, so it caches after the first
/// exchange.
pub const VOICE_SYSTEM_PROMPT: &str = "You are a helpful voice assistant. Reply like you are speaking out loud: short, conversational and direct, usually one or two sentences. Never use markdown, bullet lists, headings or emoji.";

/* ================================================================ */
/* ChatML rendering (LFM3)                                          */
/* ================================================================ */

/* LFM3's template is ChatML-style; llama-server renders prompts exactly
 * like this (verified against --log-prompts-dir dumps). Rendering here and
 * calling /v1/completions is what makes KV-prefix reuse work: a prefill
 * ping is a strict token-prefix of the final request, while the chat
 * endpoint re-tokenizes per request and defeats the reuse on this model.
 *
 * Assistant history turns must carry the generation prompt too (an empty
 * think block, thinking disabled), so the rendered text matches the tokens
 * the server cached while generating that reply. */

const IM_START: &str = "<|im_start|>";
const IM_END: &str = "<|im_end|>";
const GEN_PROMPT: &str = "<think></think>";

/* Render `messages` as ChatML. Every turn becomes
 * `<|im_start|>role\ncontent<|im_end|>\n`; assistant turns additionally
 * carry the empty think block after the role line. The array must end
 * with the user message to answer.
 *
 * `final_request = false` (prefill ping): the trailing user turn stays
 * open (no <|im_end|>), so the text-so-far is a strict token-prefix of
 * the final request. `final_request = true` closes it and appends the
 * assistant generation prompt. */
fn render_prompt(messages: &[Value], final_request: bool) -> String {
    let mut s = String::from("<|startoftext|>");
    let n = messages.len();
    for (i, m) in messages.iter().enumerate() {
        let role = m["role"].as_str().unwrap_or("user");
        let content = m["content"].as_str().unwrap_or("");
        s.push_str(IM_START);
        s.push_str(role);
        s.push('\n');
        if role == "assistant" {
            s.push_str(GEN_PROMPT);
            s.push('\n');
        }
        s.push_str(content);
        let last = i + 1 == n;
        if !(!final_request && last) {
            s.push_str(IM_END);
            s.push('\n');
        }
    }
    if final_request {
        s.push_str(IM_START);
        s.push_str("assistant\n");
        s.push_str(GEN_PROMPT);
        s.push('\n');
    }
    s
}

/* ================================================================ */
/* tool-calling prompt protocol (Phase 2)                           */
/* ================================================================ */

/// A tool as advertised to the model in the system prompt.
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters_schema: Value,
}

/// Verbatim port of ZeroClaw's `TOOL_CALL_PROTOCOL_INSTRUCTIONS`
/// (zeroclaw-runtime/src/agent/tool_call_format.rs:39-58), including the
/// trailing blank line. Tag literals are referenced from the vendored
/// parser's constants so no raw tag text lives in this source file; the
/// pinned test asserts the exact bytes.
fn tool_call_protocol_instructions() -> String {
    format!(
        "## Tool Use Protocol\n\n\
         To use a tool, wrap a JSON object in {open}{close} tags:\n\n\
         ```\n{open}\n\
         {{\"name\": \"tool_name\", \"arguments\": {{\"param\": \"value\"}}}}\n\
         {close}\n\
         ```\n\n\
         CRITICAL: Output actual {open} tags\u{2014}never describe steps or give examples.\n\n\
         Example: User says something that needs a tool. You MUST respond with a real call naming one of your available tools:\n\
         {open}\n\
         {{\"name\": \"tool_name\", \"arguments\": {{\"param\": \"value\"}}}}\n\
         {close}\n\n\
         You may use multiple tool calls in a single response. After tool execution, results appear in {result} tags. Continue reasoning with the results until you can give a final answer.\n\n\
         Persistent memory: personal facts the user has shared (preferences, plans, dates, passwords, notes) are saved in your memory and survive between sessions. When asked about anything personal, call memory_recall first â€” never claim you don't know without checking.\n\n\
         Gated tools: their first call only validates the action and announces the plan â€” it does NOT execute, and the result starts with NOT EXECUTED â€” CONFIRMATION REQUIRED. Tell the user exactly what will happen and wait for a clear yes; only after the user confirms, call the same tool again with identical arguments and it will execute. If the user declines or changes the request, do not call it again.\n\n",
        open = toolparse::TOOL_CALL_OPEN,
        close = toolparse::TOOL_CALL_CLOSE,
        result = toolparse::TOOL_RESULT_OPEN,
    )
}

/// The tool-calling prompt block: the ZeroClaw protocol instructions
/// (verbatim port of `TOOL_CALL_PROTOCOL_INSTRUCTIONS`,
/// zeroclaw-runtime/src/agent/tool_call_format.rs:39-58) plus two rules
/// of our own appended after it â€” persistent memory (v1.2) and gated
/// tools (v2.5) â€” then the `### Available Tools` listing (ZeroClaw's
/// `build_tool_instructions` shape, zeroclaw-runtime/src/agent/loop_.rs:
/// 1358-1383), with specs sorted by name so the system prompt is
/// byte-stable for the KV cache.
pub fn tools_protocol_block(specs: &[ToolSpec]) -> String {
    use std::fmt::Write as _; // String takes fmt::Write, not the io::Write in scope above
    if specs.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<&ToolSpec> = specs.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut s = String::from("\n");
    s.push_str(&tool_call_protocol_instructions());
    s.push_str("### Available Tools\n\n");
    for spec in sorted {
        let _ = writeln!(
            s,
            "**{}**: {}\nCall arguments: `{}`\n",
            spec.name,
            spec.description,
            compact_args_example(&spec.parameters_schema)
        );
    }
    s
}

/// One compact example of a tool's call arguments, rendered from its
/// JSON schema. Live evidence (benchmark 2026-11-15/16): the model reads
/// a raw schema dump as DATA to echo back ("properties.app") and burns
/// 2-4 rounds repairing the shape; a single canonical example removes
/// the ambiguity. Required keys come first (schema/BTreeMap order among
/// equals), then the optional ones.
fn compact_args_example(schema: &Value) -> String {
    let Some(props) = schema.get("properties").and_then(Value::as_object) else {
        return "{}".to_string();
    };
    let required: std::collections::HashSet<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut entries: Vec<(bool, usize, &String)> = props
        .keys()
        .enumerate()
        .map(|(i, k)| (required.contains(k.as_str()), i, k))
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let parts: Vec<String> = entries
        .into_iter()
        .map(|(_, _, k)| format!("\"{k}\": {}", placeholder_for(&props[k])))
        .collect();
    format!("{{{}}}", parts.join(", "))
}

/// The example value for one schema property: enum values joined with
/// '|' inside a quoted string, strings as "...", numbers as 0,
/// booleans as false, arrays as [], objects as {}. Unknown/absent types
/// fall back to a string placeholder (the least surprising example for
/// an LLM).
fn placeholder_for(schema: &Value) -> String {
    if let Some(vals) = schema.get("enum").and_then(Value::as_array) {
        let joined: Vec<&str> = vals.iter().filter_map(Value::as_str).collect();
        if !joined.is_empty() {
            return format!("\"{}\"", joined.join("|"));
        }
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("integer") | Some("number") => "0".to_string(),
        Some("boolean") => "false".to_string(),
        Some("array") => "[]".to_string(),
        Some("object") => "{}".to_string(),
        _ => "\"...\"".to_string(),
    }
}

/// Tools are always on (open decision 4, resolved 2026-11-13); the
/// `--no-tools` flag or `MOON_NO_TOOLS=1` drops the protocol block so the
/// assistant behaves exactly as before tool calling existed.
pub fn tools_enabled(no_tools_flag: bool) -> bool {
    !no_tools_flag && std::env::var("MOON_NO_TOOLS").is_err()
}

/// System prompt for a history seed: the `base` persona plus the tool
/// protocol block when tools are on. Called once at seed time â€” the system
/// turn never changes mid-session, keeping the KV-cache prefix stable.
/// Specs come from the tool registry (src/tools.rs), the single owner of
/// the tool list.
pub fn system_prompt_with_tools(base: &str, tools_on: bool) -> String {
    let mut s = String::from(base);
    if tools_on {
        s.push_str(&tools_protocol_block(&tools::registry().specs()));
    }
    s
}

/* ================================================================ */
/* agent loop (Phase 4): stream -> parse -> dispatch -> results      */
/* ================================================================ */

/// Tool-round budget per turn (ZeroClaw's `DEFAULT_MAX_TOOL_ITERATIONS`).
pub const MAX_TOOL_ITERATIONS: usize = 10;
/// Malformed tool-call retries per turn (ZeroClaw's number).
pub const MAX_MALFORMED_RETRIES: usize = 2;
/// Token-cap continuations per turn (plan P1): a reply the server
/// stopped at `finish_reason == "length"` with zero tool calls is a
/// mid-sentence fragment, not an answer; one corrective continuation
/// is allowed before the fragment becomes the (honest, capped) final
/// reply instead of looping forever.
pub const MAX_LENGTH_CONTINUATIONS: usize = 1;

/// Loop bookkeeping for one agent turn: the iteration budget, the
/// malformed-retry budget, the validation-streak escalation (v3
/// hardening), and the task-class budget (plan P3 item 9), kept pure so
/// the caps are unit-testable without a live server.
pub struct AgentLoopState {
    pub rounds: usize,
    pub retries: usize,
    /// Consecutive rounds whose calls were ALL rejected before any
    /// execution (unknown name / wrong-typed args / refused plan).
    /// Domain errors never count â€” a real result must be allowed to end
    /// in a plain-words answer.
    pub invalid_streak: usize,
    /// Whether any round this turn reached dispatch (parsed calls and
    /// ran the tool round). The DSML drift correction only fires before
    /// the first execution: after one, a prose-wrapped envelope echo is
    /// noise (the prose is the answer), before one it is a failed tool
    /// attempt that must not silently become the final reply.
    pub executed_any: bool,
    /// Token-cap continuations used this turn (plan P1): a reply the
    /// server stopped at `finish_reason == "length"` with zero tool
    /// calls is a mid-sentence fragment, not an answer; see
    /// [`AgentLoopState::truncation_verdict`].
    pub continuations: usize,
    /// Task-class budget (plan P3 item 9): caps for rounds / tool
    /// calls / web calls. [`crate::budget::Budget::simple`] is the
    /// historical default, so a turn that never sets it behaves exactly
    /// as before.
    pub budget: crate::budget::Budget,
    /// Tool calls dispatched this turn, parallel fan-out included.
    pub tool_calls_used: usize,
    /// Page-fetching calls (web_fetch + web_search) dispatched this
    /// turn - the web budget's counter.
    pub web_calls_used: usize,
    /// Tools that reported a definitive unsupported-capability error
    /// this turn: further calls get a deterministic refusal without
    /// dispatch (plan P3 item 9's "stop retrying after a definitive
    /// unsupported-capability error").
    pub blocked_tools: Vec<String>,
    /// Delivery-deadline notices already injected (2-rounds-left,
    /// last-round); each fires at most once per turn.
    pub deadline_notices_given: (bool, bool),
}

impl AgentLoopState {
    pub fn new() -> Self {
        AgentLoopState {
            rounds: 0,
            retries: 0,
            invalid_streak: 0,
            executed_any: false,
            continuations: 0,
            budget: crate::budget::Budget::simple(),
            tool_calls_used: 0,
            web_calls_used: 0,
            blocked_tools: Vec::new(),
            deadline_notices_given: (false, false),
        }
    }

    /// Start the next streamed round; `false` once the iteration budget
    /// is exhausted. The cap is the task class's `max_rounds` (the
    /// default equals [`MAX_TOOL_ITERATIONS`]).
    pub fn next_round(&mut self) -> bool {
        if self.rounds >= self.budget.max_rounds {
            return false;
        }
        self.rounds += 1;
        true
    }

    /// The delivery-deadline notice to inject at this round's start, if
    /// any (plan P3 item 9). Live evidence (S3-11C, S3-13C, S5-20C): the
    /// model investigates honestly to the round cap and never delivers.
    /// Two rounds out it is told to start delivering; one round out,
    /// that the next round is its last. Each notice fires at most once.
    pub fn round_start_notice(&mut self) -> Option<String> {
        let remaining = self.budget.max_rounds.saturating_sub(self.rounds);
        if remaining == 2 && !self.deadline_notices_given.0 {
            self.deadline_notices_given.0 = true;
            return Some(
                "BUDGET: 2 tool rounds left for this task. Stop gathering evidence; \
                 start delivering - give the final answer or write the requested \
                 file with what you already have."
                    .to_string(),
            );
        }
        if remaining == 1 && !self.deadline_notices_given.1 {
            self.deadline_notices_given.1 = true;
            return Some(
                "BUDGET: this is your LAST tool round. Deliver the final answer \
                 immediately after it - no new investigations."
                    .to_string(),
            );
        }
        None
    }

    /// Budget decision for one parsed tool call, BEFORE dispatch:
    /// `Err(refusal_text)` = the call must not run (blocked tool, spent
    /// web budget, spent tool-call budget) and the refusal becomes its
    /// tool_result; `Ok(())` = dispatch, and the call is counted.
    pub fn decide_call(&mut self, name: &str) -> Result<(), String> {
        if self.blocked_tools.iter().any(|t| t == name) {
            return Err(format!(
                "{name} reported itself unavailable on this system and was already \
                 reported once. Do not call it again - deliver the answer from \
                 other evidence."
            ));
        }
        let b = self.budget;
        if crate::budget::is_web_tool(name) && self.web_calls_used >= b.max_web_calls {
            return Err(format!(
                "Web budget for this task is spent ({}/{} web calls). Work with \
                 the evidence already collected and deliver the answer now.",
                self.web_calls_used, b.max_web_calls
            ));
        }
        if self.tool_calls_used >= b.max_tool_calls {
            return Err(format!(
                "Tool-call budget for this task is spent ({}/{} calls). Deliver \
                 the final answer now.",
                self.tool_calls_used, b.max_tool_calls
            ));
        }
        self.tool_calls_used += 1;
        if crate::budget::is_web_tool(name) {
            self.web_calls_used += 1;
        }
        Ok(())
    }

    /// After a dispatched round: block every tool whose result was a
    /// definitive unsupported-capability error and return one corrective
    /// line naming them (None when nothing new was blocked). Blocking
    /// takes effect from the NEXT round - a parallel batch already in
    /// flight runs to completion.
    pub fn unsupported_verdict(&mut self, outcomes: &[ToolOutcome]) -> Option<String> {
        let mut newly: Vec<String> = Vec::new();
        for o in outcomes {
            if o.status == "error"
                && o.output.starts_with(crate::budget::UNSUPPORTED_PREFIX)
                && !self.blocked_tools.iter().any(|t| *t == o.name)
                && !newly.iter().any(|t| *t == o.name)
            {
                newly.push(o.name.clone());
            }
        }
        if newly.is_empty() {
            return None;
        }
        self.blocked_tools.extend(newly.iter().cloned());
        let names = newly.join(", ");
        Some(format!(
            "Note: {names} reported this capability as unavailable on this system. \
             Do not call it again this task - answer from other evidence."
        ))
    }

    /// Verdict for a streamed reply that parsed to zero tool calls.
    /// `Some(corrective_user_message)` = append it to history and retry;
    /// `None` = treat the reply as the final plain answer. A reply WITH
    /// calls resets the retry budget (the model found the format again).
    pub fn malformed_verdict(
        &mut self,
        raw: &str,
        calls: &[toolparse::ParsedToolCall],
    ) -> Option<String> {
        if !calls.is_empty() {
            self.retries = 0;
            return None;
        }
        let issue = toolparse::detect_tool_call_parse_issue(raw, calls)?;
        if self.retries >= MAX_MALFORMED_RETRIES {
            return None;
        }
        self.retries += 1;
        Some(format!(
            "Your last response tried to use a tool but was malformed ({issue}). \
             Reply with exactly one JSON object inside one tool-call tag pair, \
             naming one of your available tools, or answer in plain words."
        ))
    }

    /// Verdict for a zero-call reply that carries the model's native DSML
    /// envelope on a round where nothing has executed yet
    /// (`executed_any == false`). Live evidence (2026-11-15): a first
    /// round of prose + a MANGLED DSML envelope parses to zero calls, is
    /// NOT flagged by the issue detector (it only catches DSML when the
    /// response OPENS with the envelope), and silently becomes the final
    /// answer — raw tags streamed to the user, nothing armed. The
    /// prose-echo exemption exists for rounds after a success; before
    /// any execution there is no answer to protect. Shares the malformed
    /// retry budget, and the retry round is grammar-forced by the loop.
    pub fn dsml_drift_verdict(&mut self, raw: &str) -> Option<String> {
        if self.executed_any
            || !raw.contains(toolparse::DSML_OPEN)
            || toolparse::looks_like_tool_protocol_example(raw)
            || self.retries >= MAX_MALFORMED_RETRIES
        {
            return None;
        }
        self.retries += 1;
        Some(
            "Your last response used your native DSML envelope format, which \
             cannot be dispatched here. Reply with exactly one JSON object \
             inside one tool-call tag pair, naming one of your available \
             tools, or answer in plain words."
                .to_string(),
        )
    }

    /// Verdict for a zero-call reply whose stream ended at the token cap
    /// (`finish_reason == "length"`, plan P1). Live evidence (benchmark
    /// 2026-11-16, S5-19 twice): the fragment streamed to the user as if
    /// it were the final answer, mid-sentence, with the requested report
    /// never produced. Checked AFTER the malformed and DSML verdicts in
    /// the zero-calls path — a cut-off envelope is a parse issue first.
    /// One corrective continuation per turn (`MAX_LENGTH_CONTINUATIONS`);
    /// after that the fragment becomes the final reply so the turn stays
    /// bounded.
    pub fn truncation_verdict(&mut self, stop: Option<&str>) -> Option<String> {
        if stop != Some("length") || self.continuations >= MAX_LENGTH_CONTINUATIONS {
            return None;
        }
        self.continuations += 1;
        Some(
            "Your last response was cut off by the length limit before it finished. \
             Continue exactly where it stopped: if a tool call was pending, emit the \
             complete tool-call envelope now; otherwise end the answer in a few words. \
             Do not repeat text you already wrote."
                .to_string(),
        )
    }

    /// What to do after a dispatched round (v3 hardening). Live evidence
    /// (2026-11-15): when a round's calls all fail validation â€” the
    /// schema-echo/alias-mangling defect class, which PARSES so the
    /// malformed path never fires â€” the model, given only the error
    /// results, drifts to a prose question and the turn ends with zero
    /// progress. The escalation ladder:
    ///
    /// streak 1 â†’ one free corrective retry round (unconstrained: the
    /// model may legitimately answer in plain words, e.g. when the user
    /// never said which file to delete);
    /// streak 2 â†’ grammar-force the next round, making echoed/wrong-typed
    /// argument shapes unrepresentable (the tool-call grammar admits only
    /// schema-typed values);
    /// streak â‰¥ 3 â†’ stand down: forcing failed to resolve it, so stop
    /// spending rounds on constraints and let the model speak or the
    /// iteration budget end the turn.
    pub fn validation_verdict(&mut self, outcomes: &[ToolOutcome]) -> ValidationStep {
        let all_invalid = !outcomes.is_empty()
            && outcomes
                .iter()
                .all(|o| o.phase == crate::tools::DispatchPhase::Invalid);
        if !all_invalid {
            /* a mix, an armed announce, or a real execution: the round
             * made progress (or got a real result) â€” reset the streak */
            self.invalid_streak = 0;
            return ValidationStep::None;
        }
        self.invalid_streak += 1;
        match self.invalid_streak {
            1 => {
                let first = outcomes[0].output.clone();
                ValidationStep::Corrective(format!(
                    "Every tool call this round was rejected before anything executed ({first}). \
                     Reply with one corrected tool call in the canonical JSON envelope, or, \
                     if you cannot determine the correct arguments, answer in plain words."
                ))
            }
            2 => ValidationStep::ForceGrammar,
            _ => ValidationStep::None,
        }
    }
}

/// The agent loop's decision after a round whose calls were all
/// rejected before execution â€” see
/// [`AgentLoopState::validation_verdict`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationStep {
    /// Results message alone; the next round is unconstrained.
    None,
    /// Append the corrective note to the results message; the next
    /// round stays unconstrained (the model may answer in plain words).
    Corrective(String),
    /// Grammar-force the next round: shapes that failed twice must
    /// become unrepresentable.
    ForceGrammar,
}

/// One executed tool call, as it flows into the results message.
pub struct ToolOutcome {
    pub name: String,
    pub status: String, // "ok" | "error"
    pub output: String,
    /// Which path the call took (v3 hardening): rounds where every call
    /// died before execution drive the escalation ladder.
    pub phase: crate::tools::DispatchPhase,
}

/// One tool_result block inside the results user message (the shape the
/// protocol block promises: results appear in tool_result tags).
pub fn tool_result_block(name: &str, status: &str, output: &str) -> String {
    format!(
        "{}\n{}\n{}\n",
        toolparse::TOOL_RESULT_OPEN,
        json!({"name": name, "status": status, "result": output}),
        toolparse::TOOL_RESULT_CLOSE
    )
}

/// The user message that carries tool results back to the model: a
/// stable `[Tool results]` carrier prefix plus one tool_result block per
/// executed call. Plain appended text â€” `render_prompt` needs no change
/// and the KV-cache prefix stays stable.
pub fn tool_results_user_message(outcomes: &[ToolOutcome]) -> String {
    let mut s = String::from("[Tool results]\n");
    for o in outcomes {
        s.push_str(&tool_result_block(&o.name, &o.status, &o.output));
    }
    s
}

/// Collapse a parsed call list down to unique calls: a degenerate model
/// reply can loop its envelope several times, and executing the same
/// call twice would, e.g., open the app twice. Signature = name +
/// canonicalized (key-sorted) arguments, using upstream's canonicalizer.
pub fn dedup_tool_calls(calls: Vec<toolparse::ParsedToolCall>) -> Vec<toolparse::ParsedToolCall> {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut unique = Vec::with_capacity(calls.len());
    for call in calls {
        let sig = (
            call.name.clone(),
            toolparse::canonicalize_json_for_tool_signature(&call.arguments).to_string(),
        );
        if seen.insert(sig) {
            unique.push(call);
        }
    }
    unique
}

/// What to do when a streamed round fails before producing usable text
/// (plan P1 recovery ladder). One bounded recovery each for the two
/// transport-level failure classes; anything else propagates.
enum RoundRetry {
    None,
    /// The prompt outgrew the model context (HTTP 400): shrink the
    /// middle history and retry the same round once.
    Compact,
    /// The connection to llama-server died ("Peer disconnected", etc.,
    /// benchmark 2026-11-16, S2-7/S2-8): re-spawn the server and retry
    /// the same round once.
    Reconnect,
}

/// Transport-level failures between IGRIS and llama-server — the shared
/// server died or the socket dropped. These warrant ONE
/// `ensure_server(false)` retry. Deliberately NOT matched by "timed
/// out": a slow stream is still making progress, and restarting it
/// would replay the generation from scratch.
fn is_transport_error(msg: &str) -> bool {
    let m = msg.to_lowercase();
    [
        "peer disconnected",
        "connection refused",
        "connection reset",
        "broken pipe",
        "os error 10054",
        "os error 10061",
    ]
    .iter()
    .any(|s| m.contains(s))
}

/// The server rejected the request outright (HTTP 400): in practice the
/// rendered prompt outgrew the model context after heavy web rounds
/// (benchmark 2026-11-16, S3-12/S4-16, reproducible). Matched in two
/// shapes: our own formatter's and ureq's Error::StatusCode Display —
/// the latter only if a call site ever bypasses the status checks.
fn is_context_overflow(msg: &str) -> bool {
    msg.starts_with("llama-server error 400") || msg.contains("http status: 400")
}

/// One-shot recovery for a prompt that outgrew the model context (plan
/// P1): keep the system seed (0) and the original user request (1)
/// verbatim, shrink every middle message to a bounded digest, and bound
/// the kept tail too. The tail's tool-result payloads (a parallel
/// read_file batch, a fetched page) are usually the very mass that
/// overflowed, so leaving the last four verbatim made the compacted
/// retry overflow again (benchmark rerun 2026-11-16, S2-7F). Bounded
/// no-op when there is nothing to compress.
pub fn compact_history(history: &mut Vec<Value>) {
    const COMPACT_MIDDLE_CAP: usize = 600;
    const COMPACT_TAIL_CAP: usize = 800;
    let n = history.len();
    if n <= 2 {
        return;
    }
    for i in 2..n {
        /* the last four messages hold the round actually in flight —
         * keep more of them than the middle, but never all of it */
        let cap = if n - i > 4 {
            COMPACT_MIDDLE_CAP
        } else {
            COMPACT_TAIL_CAP
        };
        if let Some(content) = history[i].get("content").and_then(Value::as_str) {
            let shrunk = crate::tools::truncate_chars(content, cap);
            history[i]["content"] = json!(shrunk);
        }
    }
}

/// One streamed round with protocol hold-back: a fresh StreamFilter,
/// tokens forwarded to the sink, and the `finish()` tail flushed after
/// the stream ends (the closure-borrow scope trick: `on_tok` lives in
/// an inner block so `filter` and `sink` are free again for the tail).
/// Returns the raw text plus the server-reported stop reason, and
/// whether the sink cut the stream (barge-in, or a cut tail).
fn run_stream_round(
    msgs: &[Value],
    max_tokens: i64,
    grammar: Option<&str>,
    sink: &mut StreamSink,
) -> (Result<(String, Option<String>), Box<dyn std::error::Error>>, bool) {
    let mut filter = toolparse::StreamFilter::new();
    let mut cut = false;
    let stream = {
        let mut on_tok = |tok: &str| -> bool {
            let released = filter.feed(tok);
            if released.is_empty() {
                return true;
            }
            if (sink.on_released)(&released) {
                true
            } else {
                cut = true;
                false
            }
        };
        chat_stream(msgs, max_tokens, &mut on_tok, grammar)
    };
    /* the first-token latency is terminal-only bookkeeping; the agent
     * loop cares about the text and the stop reason */
    let result = stream.map(|(raw, _first_ms, stop)| (raw, stop));
    let tail_cut = if cut {
        false
    } else {
        match &result {
            Ok(_) => {
                let tail = filter.finish();
                !tail.is_empty() && !(sink.on_released)(&tail)
            }
            Err(_) => false,
        }
    };
    (result, cut || tail_cut)
}

/// Surface callbacks for [`agent_turn`].
pub struct StreamSink<'a> {
    /// Receives only released text (never a tool envelope, think span, or
    /// terminal marker). Return `false` to cut the stream (barge-in).
    pub on_released: &'a mut dyn FnMut(&str) -> bool,
    /// Called before a tool round's calls execute, with the tool names.
    pub on_tool_round: &'a mut dyn FnMut(&[String]),
}

/// How an agent turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentTurnEnd {
    /// A plain (tool-free) final reply completed the turn.
    Final,
    /// Barge-in cut the stream.
    Cut,
    /// The task's tool-round budget ran out while the model kept
    /// calling tools; carries the rounds that ran.
    Exhausted { rounds: usize },
}

/// One agent turn on `history` (which already holds the system seed and
/// the user message): stream a reply with protocol hold-back, parse it,
/// execute any tool calls against the registry, feed the results back as
/// a user message, and repeat â€” until a plain final reply, a barge-in, or
/// the iteration budget. History only ever grows (each round appends its
/// raw assistant envelopes and the results message), so every round is a
/// strict prompt-prefix of the next and `cache_prompt` stays effective.
pub fn agent_turn(
    history: &mut Vec<Value>,
    max_tokens: i64,
    sink: &mut StreamSink,
) -> Result<AgentTurnEnd, Box<dyn std::error::Error>> {
    /* One turn per utterance (the voice loop and the CLI chat both come
     * through here): gated tools need a pending armed in a strictly
     * earlier turn, so a same-turn re-call can never bypass the
     * confirmation (v2.5). */
    crate::gating::begin_turn();
    let registry = tools::registry();
    /* grammar-constrained retries (v3): one grammar per turn, built from
     * the sorted specs so the per-tool args rules are index-stable */
    let grammar = crate::toolgrammar::tool_calls_grammar(&registry.specs());
    /* true from the round after a malformed verdict until a good round */
    let mut constrained = false;
    let timed = std::env::var_os("MOON_TIMING").is_some();
    let mut state = AgentLoopState::new();
    /* Task-class budget (plan P3 item 9): the newest user utterance
     * classifies the turn, and the class shapes the caps plus the
     * delivery-deadline notices. The default (Simple) equals the
     * historical constants, so unclassified-shape turns behave exactly
     * as measured by the benchmark suite. */
    let user_text = history
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(Value::as_str) == Some("user"))
        .and_then(|m| m.get("content").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    state.budget = crate::budget::budget_for(crate::budget::classify(&user_text));
    while state.next_round() {
        /* delivery-deadline notices (plan P3 item 9): their own user
         * message at round start, so the round's prompt ends with the
         * deadline and the model delivers instead of investigating to
         * the wall (the S3-11C/S3-13C/S5-20C failure shape) */
        if let Some(notice) = state.round_start_notice() {
            history.push(json!({"role": "user", "content": notice}));
        }
        let grammar_ref: Option<&str> = if constrained {
            grammar.as_deref()
        } else {
            None
        };
        let t_llm = Instant::now();
        /* Bounded recovery ladder (plan P1): one compaction retry for a
         * context overflow, one re-spawn retry for a transport error,
         * both retrying the SAME round on fresh state. Never a
         * slow-stream restart — a still-generating stream must not be
         * replayed from scratch. */
        let mut compacted = false;
        let mut reconnected = false;
        let outcome = loop {
            let msgs = history.clone();
            let attempt = run_stream_round(&msgs, max_tokens, grammar_ref, sink);
            let retry = match &attempt.0 {
                Err(e) => {
                    let msg = e.to_string();
                    if is_context_overflow(&msg) && !compacted {
                        compacted = true;
                        RoundRetry::Compact
                    } else if is_transport_error(&msg) && !reconnected {
                        reconnected = true;
                        RoundRetry::Reconnect
                    } else {
                        RoundRetry::None
                    }
                }
                Ok(_) => RoundRetry::None,
            };
            match retry {
                RoundRetry::Compact => {
                    if timed {
                        eprintln!(
                            "[timing] round {} context overflow — compacting history, retrying",
                            state.rounds
                        );
                    }
                    compact_history(history);
                }
                RoundRetry::Reconnect => {
                    if timed {
                        eprintln!(
                            "[timing] round {} transport error — re-spawning server, retrying",
                            state.rounds
                        );
                    }
                    let _ = ensure_server(false);
                }
                RoundRetry::None => break attempt,
            }
        };
        let (raw, stop) = outcome.0?;
        let cut = outcome.1;
        if timed {
            eprintln!(
                "[timing] round {} llm {}ms",
                state.rounds,
                t_llm.elapsed().as_millis()
            );
        }
        if std::env::var_os("MOON_DEBUG_RAW").is_some() {
            /* angle brackets escaped so the dump can be read safely in
             * logs/terminals where tag-like text is a parsing hazard */
            let escaped = raw.replace('\u{3c}', "\\x3c");
            eprintln!("[debug] round {} raw: {escaped}", state.rounds);
        }
        if cut {
            /* parity with the plain path: an interrupted reply still
             * lands in history (the model sees its own partial output) */
            let stored = toolparse::strip_trailing_terminal_markers(&raw);
            if !stored.is_empty() {
                history.push(json!({"role": "assistant", "content": stored}));
            }
            return Ok(AgentTurnEnd::Cut);
        }

        let (_cleaned, calls) = toolparse::parse_tool_calls(&raw);
        if calls.is_empty() {
            if let Some(corrective) = state.malformed_verdict(&raw, &calls) {
                /* the NEXT round samples under the tool-call grammar so
                 * malformed syntax cannot repeat (v3) */
                constrained = true;
                history.push(json!({"role": "assistant", "content": raw}));
                history.push(json!({"role": "user", "content": corrective}));
                continue;
            }
            if let Some(corrective) = state.dsml_drift_verdict(&raw) {
                /* first-contact DSML drift: grammar-force the retry so
                 * the native envelope spelling becomes unrepresentable */
                constrained = true;
                history.push(json!({"role": "assistant", "content": raw}));
                history.push(json!({"role": "user", "content": corrective}));
                continue;
            }
            if let Some(corrective) = state.truncation_verdict(stop.as_deref()) {
                /* token-cap fragment (plan P1): a mid-sentence reply is
                 * not an answer — one corrective continuation, checked
                 * after the malformed/DSML verdicts */
                history.push(json!({"role": "assistant", "content": raw}));
                history.push(json!({"role": "user", "content": corrective}));
                continue;
            }
            let stored = toolparse::strip_trailing_terminal_markers(&raw);
            if !stored.is_empty() {
                history.push(json!({"role": "assistant", "content": stored}));
            }
            return Ok(AgentTurnEnd::Final);
        }

        // De-duplicate repeated identical calls before dispatch: a
        // degenerate reply can loop its envelope several times.
        let calls = dedup_tool_calls(calls);
        state.retries = 0;
        state.executed_any = true;
        let stored = toolparse::strip_trailing_terminal_markers(&raw);
        history.push(json!({"role": "assistant", "content": stored}));
        let names: Vec<String> = calls.iter().map(|c| c.name.clone()).collect();
        (sink.on_tool_round)(&names);
        /* one call takes the plain path; several calls execute together
         * on parallel workers (v3, bounded to 4 at a time) with results
         * in call order */
        let t_tools = Instant::now();
        /* Task-class budget (plan P3 item 9): every call is decided
         * before dispatch. Spent web/tool-call budgets and tools blocked
         * for definitive unsupported-capability errors get a
         * deterministic refusal tool_result without running; allowed
         * calls dispatch exactly as before (one on the plain path,
         * several on parallel workers), results stay in call order. */
        let mut decisions: Vec<Result<(), String>> = Vec::with_capacity(calls.len());
        for call in &calls {
            decisions.push(state.decide_call(&call.name));
        }
        let mut slots: Vec<Option<ToolOutcome>> = Vec::with_capacity(calls.len());
        let mut runnable: Vec<(usize, &toolparse::ParsedToolCall)> = Vec::new();
        for (i, (call, decision)) in calls.iter().zip(&decisions).enumerate() {
            match decision {
                Ok(()) => {
                    slots.push(None);
                    runnable.push((i, call));
                }
                Err(refusal) => slots.push(Some(ToolOutcome {
                    name: call.name.clone(),
                    status: "error".to_string(),
                    output: refusal.clone(),
                    phase: crate::tools::DispatchPhase::Invalid,
                })),
            }
        }
        if runnable.len() == 1 {
            let (i, call) = runnable[0];
            let (result, phase) = registry.dispatch_classified(&call.name, &call.arguments);
            let (status, output) = match result {
                Ok(out) => ("ok", out),
                Err(e) => ("error", e),
            };
            slots[i] = Some(ToolOutcome {
                name: call.name.clone(),
                status: status.to_string(),
                output,
                phase,
            });
        } else if runnable.len() > 1 {
            let batch: Vec<(String, Value)> = runnable
                .iter()
                .map(|(_, c)| (c.name.clone(), c.arguments.clone()))
                .collect();
            let results = registry.dispatch_batch(&batch);
            for (i, o) in runnable.iter().map(|(i, _)| *i).zip(results) {
                let (status, output) = match o.result {
                    Ok(out) => ("ok", out),
                    Err(e) => ("error", e),
                };
                slots[i] = Some(ToolOutcome {
                    name: o.name,
                    status: status.to_string(),
                    output,
                    phase: o.phase,
                });
            }
        }
        let outcomes: Vec<ToolOutcome> = slots.into_iter().flatten().collect();
        if timed {
            eprintln!(
                "[timing] round {} tools {}ms over {} call(s){}",
                state.rounds,
                t_tools.elapsed().as_millis(),
                runnable.len(),
                if runnable.len() > 1 { " parallel" } else { "" },
            );
        }
        /* the results message, plus the escalation-ladder verdict (v3
         * hardening) which may append a corrective note or force the
         * next round to sample under the tool-call grammar */
        let mut results_msg = tool_results_user_message(&outcomes);
        /* definitive unsupported-capability results (plan P3 item 9):
         * block the tool for the rest of the turn and tell the model
         * once, so it never spends rounds re-calling a missing
         * capability */
        if let Some(note) = state.unsupported_verdict(&outcomes) {
            results_msg.push('\n');
            results_msg.push_str(&note);
        }
        match state.validation_verdict(&outcomes) {
            ValidationStep::None => constrained = false,
            ValidationStep::Corrective(note) => {
                results_msg.push('\n');
                results_msg.push_str(&note);
                constrained = false;
            }
            ValidationStep::ForceGrammar => constrained = true,
        }
        history.push(json!({
            "role": "user",
            "content": results_msg
        }));
    }
    Ok(AgentTurnEnd::Exhausted { rounds: state.rounds })
}

/* ================================================================ */
/* server lifecycle                                                 */
/* ================================================================ */

/// Shared HTTP agent. Global budget is generous because a completion can
/// legitimately run for tens of seconds on CPU.
static AGENT: OnceLock<ureq::Agent> = OnceLock::new();

fn agent() -> &'static ureq::Agent {
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            /* Non-200s must reach chat_stream/complete's own status
             * checks, which format "llama-server error {status}: {body}"
             * — ureq's default Err(Error::StatusCode(n)) hides the body
             * and dodges the recovery ladder's overflow matcher
             * (benchmark 2026-11-16 rerun, S2-7A/S4-15A). */
            .http_status_as_error(false)
            .build()
            .into()
    })
}

fn base_url() -> String {
    format!("http://127.0.0.1:{PORT}")
}

/// True when a llama-server is up and has finished loading the model.
fn server_ready() -> bool {
    matches!(
        agent()
            .get(&format!("{}/health", base_url()))
            .config()
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .call(),
        Ok(resp) if resp.status().as_u16() == 200,
    )
}

/// Lowercased file name of the GGUF this build expects to serve, e.g.
/// "lfm3-1.7b-q4_k_m.gguf" â€” the identity a reused server must carry.
fn expected_model_id() -> &'static str {
    static EXPECTED: OnceLock<String> = OnceLock::new();
    EXPECTED.get_or_init(|| {
        std::path::Path::new(LLM_MODEL)
            .file_name()
            .map(|f| f.to_string_lossy().to_lowercase())
            .unwrap_or_else(|| LLM_MODEL.to_lowercase())
    })
}

/// Extract the GGUF file name from a llama-server /props body. Tolerant
/// across builds: the live bundled build (b12110) reports `model_path`
/// top-level; `model_alias` is the older spelling. None when the body is
/// not props-shaped JSON (a foreign server, a 404 page).
fn parse_props_model_id(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    for key in ["model_path", "model_alias"] {
        if let Some(p) = v.get(key).and_then(|x| x.as_str()) {
            if let Some(name) = std::path::Path::new(p).file_name() {
                return Some(name.to_string_lossy().to_lowercase());
            }
        }
    }
    None
}

/// The running server's own model identity (GGUF file name, lowercased)
/// from its /props endpoint; None when it does not answer with one
/// (foreign server on the port, or a llama-server build without /props).
fn server_model_id() -> Option<String> {
    let mut resp = agent()
        .get(&format!("{}/props", base_url()))
        .config()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .call()
        .ok()?;
    resp.body_mut().read_to_string().ok().and_then(|b| parse_props_model_id(&b))
}

/// Which llama.cpp build to try. The GPU build is preferred; the CPU
/// build runs on any Windows x64 machine.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Backend {
    Gpu,
    Cpu,
}

impl Backend {
    fn exe(self) -> std::path::PathBuf {
        match self {
            Backend::Gpu => paths::asset(LLAMA_GPU_EXE),
            Backend::Cpu => paths::asset(LLAMA_CPU_EXE),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Backend::Gpu => "gpu",
            Backend::Cpu => "cpu",
        }
    }
    /* Generous on purpose: a slow-but-working load must never be killed.
     * Real failures (no driver, no VRAM, crash) surface as a fast-fail
     * hint or an early process exit, not as a timeout. Measured here:
     * gpu 17 s with a cold disk cache, cpu 4 s. */
    fn startup_timeout(self) -> Duration {
        match self {
            Backend::Gpu => Duration::from_secs(90),
            Backend::Cpu => Duration::from_secs(150),
        }
    }
}

/* Lowercase fragments of llama-server stderr that mean "this machine
 * cannot run the Vulkan build"; each one triggers an immediate fallback
 * to the CPU build instead of waiting out the timeout. */
const GPU_FAIL_HINTS: [&str; 5] = [
    "0 vulkan devices",
    "failed to create vulkan",
    "vkcreateinstance",
    "out_of_device_memory",
    "device lost",
];

/// Records the backend preference from the first ensure_server call so
/// later calls (prefill pings during a running session) reuse the same
/// choice instead of re-deriving it.
static FORCE_CPU: OnceLock<bool> = OnceLock::new();

/// Start a llama-server (GPU build first, CPU build as the automatic
/// fallback) unless one is already serving, then wait until the model is
/// fully loaded (health endpoint returns 200). `force_cpu` skips the
/// GPU attempt entirely.
pub fn ensure_server(force_cpu: bool) -> Result<(), Box<dyn std::error::Error>> {
    let force_cpu = *FORCE_CPU.get_or_init(|| force_cpu || std::env::var("MOON_LLM_CPU").is_ok());
    if server_ready() {
        /* BUG-006: /health says "a llama-server is up", not "the RIGHT
         * model is up" â€” a wrong-model or foreign server on the port
         * used to be reused blindly (degraded answers, no error).
         * Identify it via /props before reusing; refuse loudly on
         * mismatch. Fail-closed when /props carries no identity. */
        match server_model_id() {
            Some(id) if id == expected_model_id() => {
                println!("  llm       : reusing already-running llama-server (:{PORT})");
                return Ok(());
            }
            Some(id) => {
                let msg = format!(
                    "port {PORT} is already serving a DIFFERENT model ('{id}', expected \
                     '{}'). Kill that llama-server, then run IGRIS again.",
                    expected_model_id()
                );
                eprintln!("  llm       : {msg}");
                return Err(msg.into());
            }
            None => {
                let msg = format!(
                    "port {PORT} answers /health but is not our llama-server (/props \
                     carries no model identity). Free the port, then run IGRIS again."
                );
                eprintln!("  llm       : {msg}");
                return Err(msg.into());
            }
        }
    }
    let order: &[Backend] = if force_cpu {
        &[Backend::Cpu]
    } else {
        &[Backend::Gpu, Backend::Cpu]
    };
    let mut last_err = String::new();
    for (attempt, &backend) in order.iter().enumerate() {
        match try_start(backend) {
            Ok(device) => {
                let dev = device.map(|d| format!(" ({d})")).unwrap_or_default();
                println!(
                    "  llm       : llama.cpp {} backend ready{}",
                    backend.label(),
                    dev
                );
                return Ok(());
            }
            Err(reason) => {
                eprintln!("  llm       : {} build failed: {reason}", backend.label());
                last_err = reason;
                if attempt + 1 < order.len() {
                    eprintln!("  llm       : falling back to the cpu build");
                }
            }
        }
    }
    Err(format!("no llama-server could be started: {last_err}").into())
}

/* ================================================================ */
/* isolated spawn (BUG-010): the server must inherit NOTHING         */
/* ================================================================ */

/// BUG-010: a llama-server spawned through `std::process::Command`
/// inherits *every* inheritable handle this process holds â€” std always
/// calls CreateProcessW with `bInheritHandles=TRUE` and never builds a
/// `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. When we run under a redirected
/// `cargo run` (or any pipeline), our handle table carries the parent
/// chain's pipe write-ends twice: cargo's original handle plus std's
/// inheritable duplicate made for us. Clearing the inherit flag on our
/// three GetStdHandle entries cannot reach the originals, so the
/// immortal detached server kept the pipe write-ends alive and outer
/// pipelines never saw EOF even after this process had exited (the
/// app-level trace always ran to completion). This manual spawn passes
/// an explicit handle list instead: the child receives exactly its
/// three std handles and nothing else, no matter what we happen to
/// hold. Closing our process handle never kills the child â€” the server
/// outlives this process by design.
struct IsolatedChild {
    process: windows::Win32::Foundation::HANDLE,
}

impl IsolatedChild {
    /// Has the child exited? Non-blocking.
    fn try_wait(&self) -> bool {
        use windows::Win32::Foundation::WAIT_OBJECT_0;
        use windows::Win32::System::Threading::WaitForSingleObject;
        unsafe { WaitForSingleObject(self.process, 0) == WAIT_OBJECT_0 }
    }

    /// Terminate the child (startup-failure paths) and reap it briefly.
    fn kill(&self) {
        use windows::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};
        unsafe {
            let _ = TerminateProcess(self.process, 1);
            let _ = WaitForSingleObject(self.process, 2000);
        }
    }
}

impl Drop for IsolatedChild {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.process);
        }
    }
}

/// Spawn `exe` with `args`, stdio = NUL / NUL / `stderr_log`, inheriting
/// no handle beyond those three (see [`IsolatedChild`]).
fn spawn_isolated(
    exe: &std::path::Path,
    args: &[String],
    stderr_log: &std::path::Path,
) -> Result<IsolatedChild, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::Foundation::{
        CloseHandle, GENERIC_ACCESS_RIGHTS, GENERIC_READ, GENERIC_WRITE, HANDLE,
        HANDLE_FLAG_INHERIT, SetHandleInformation,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList,
        UpdateProcThreadAttribute, EXTENDED_STARTUPINFO_PRESENT, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROCESS_CREATION_FLAGS, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, STARTUPINFOW,
    };

    /* Child stdio: NUL for stdin/stdout, the startup log for stderr. The
     * log is shared read+write so the poll loop in try_start can read it
     * while the server writes. */
    let nul = |access: GENERIC_ACCESS_RIGHTS| -> Result<HANDLE, String> {
        let mut w: Vec<u16> = "\\\\.\\NUL".encode_utf16().collect();
        w.push(0);
        unsafe {
            CreateFileW(
                PCWSTR(w.as_mut_ptr()),
                access.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
            .map_err(|e| format!("opening NUL failed: {e}"))
        }
    };
    let h_in = nul(GENERIC_READ)?;
    let h_out = nul(GENERIC_WRITE)?;
    let mut path_w: Vec<u16> = stderr_log.as_os_str().encode_wide().collect();
    path_w.push(0);
    let h_err = unsafe {
        CreateFileW(
            PCWSTR(path_w.as_mut_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            CREATE_ALWAYS,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
        .map_err(|e| format!("opening server stderr log failed: {e}"))
    }?;

    /* All three handles must be inheritable to travel through the handle
     * list. */
    for h in [h_in, h_out, h_err] {
        unsafe {
            SetHandleInformation(h, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT)
                .map_err(|e| format!("SetHandleInformation failed: {e}"))?;
        }
    }

    /* One-entry attribute list restricting inherited handles to exactly
     * [stdin, stdout, stderr]. The handle array MUST stay alive until
     * CreateProcessW returns below: Windows reads the list during process
     * creation (dropping it here yields ERROR_INVALID_PARAMETER 0x80070057
     * at spawn time, intermittently per backend). */
    let handles = [h_in, h_out, h_err];
    let mut size = 0usize;
    unsafe {
        /* Probe call: it "fails" by design and leaves the required
         * buffer size in `size`. */
        let _ = InitializeProcThreadAttributeList(None, 1, None, &mut size);
    }
    let mut list_buf = vec![0u8; size];
    let list = LPPROC_THREAD_ATTRIBUTE_LIST(list_buf.as_mut_ptr().cast());
    unsafe {
        InitializeProcThreadAttributeList(Some(list), 1, None, &mut size)
            .map_err(|e| format!("InitializeProcThreadAttributeList failed: {e}"))?;
        UpdateProcThreadAttribute(
            list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            Some(handles.as_ptr().cast()),
            core::mem::size_of::<[HANDLE; 3]>(),
            None,
            None,
        )
        .map_err(|e| format!("UpdateProcThreadAttribute failed: {e}"))?;
    }

    let mut si = STARTUPINFOEXW::default();
    si.StartupInfo.cb = core::mem::size_of::<STARTUPINFOEXW>() as u32;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = h_in;
    si.StartupInfo.hStdOutput = h_out;
    si.StartupInfo.hStdError = h_err;
    si.lpAttributeList = list;

    let mut cmdline = make_command_line(exe, args);
    let mut exe_wide: Vec<u16> = exe.as_os_str().encode_wide().collect();
    exe_wide.push(0);
    let mut pi: PROCESS_INFORMATION = unsafe { core::mem::zeroed() };
    let spawn = unsafe {
        CreateProcessW(
            PCWSTR(exe_wide.as_mut_ptr()),
            Some(PWSTR(cmdline.as_mut_ptr())),
            None,
            None,
            true, /* bInheritHandles â€” the attribute list is what restricts */
            /* EXTENDED_STARTUPINFO_PRESENT is mandatory with a
             * STARTUPINFOEXW: without it CreateProcessW reads the struct
             * as a plain STARTUPINFOW and silently ignores the attribute
             * list (first live run leaked every handle again). */
            PROCESS_CREATION_FLAGS(EXTENDED_STARTUPINFO_PRESENT.0),
            None, /* environment: inherit ours */
            PCWSTR::null(),
            &si as *const STARTUPINFOEXW as *const STARTUPINFOW,
            &mut pi,
        )
        .map_err(|e| format!("CreateProcessW failed: {e}"))
    };
    /* The list and the child-side std handles were consumed by
     * CreateProcessW â€” drop our copies, list first. */
    unsafe {
        DeleteProcThreadAttributeList(list);
        let _ = CloseHandle(h_in);
        let _ = CloseHandle(h_out);
        let _ = CloseHandle(h_err);
    }
    spawn?;
    unsafe {
        let _ = CloseHandle(pi.hThread);
    }
    Ok(IsolatedChild { process: pi.hProcess })
}

/// Compose a Windows command line with the same quoting rules
/// std::process::Command uses (MSVCRT parsing: double every backslash
/// that precedes a quote, 2n+1 backslashes to emit a quote, double
/// trailing backslashes before a closing quote).
fn make_command_line(exe: &std::path::Path, args: &[String]) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut cmd = Vec::new();
    append_arg(&exe.as_os_str().encode_wide().collect::<Vec<_>>(), &mut cmd);
    for a in args {
        cmd.push(b' ' as u16);
        append_arg(&a.encode_utf16().collect::<Vec<_>>(), &mut cmd);
    }
    cmd.push(0);
    cmd
}

fn append_arg(arg: &[u16], cmd: &mut Vec<u16>) {
    let quote = arg.is_empty()
        || arg
            .iter()
            .any(|&c| c == b' ' as u16 || c == b'\t' as u16 || c == b'"' as u16);
    if quote {
        cmd.push(b'"' as u16);
    }
    let mut bs = 0usize;
    for &c in arg {
        if c == b'\\' as u16 {
            bs += 1;
        } else if c == b'"' as u16 {
            for _ in 0..2 * bs + 1 {
                cmd.push(b'\\' as u16);
            }
            cmd.push(b'"' as u16);
            bs = 0;
        } else {
            for _ in 0..bs {
                cmd.push(b'\\' as u16);
            }
            cmd.push(c);
            bs = 0;
        }
    }
    if quote {
        for _ in 0..2 * bs {
            cmd.push(b'\\' as u16);
        }
        cmd.push(b'"' as u16);
    }
}

/// Spawn one backend and poll it until healthy. Ok carries the GPU device
/// name when one shows up in the startup log. The child is deliberately
/// not killed on success: it outlives this process and keeps serving for
/// later runs of the app.
fn try_start(backend: Backend) -> Result<Option<String>, String> {
    let exe = backend.exe();
    if !exe.exists() {
        return Err(format!(
            "{} not found (provision the assets tree, see README)",
            exe.display()
        ));
    }
    let mut args = vec![
        "-m".into(),
        paths::asset(LLM_MODEL).to_string_lossy().into_owned(),
        "-c".into(),
        CTX_TOKENS.to_string(),
        "-fa".into(),
        "on".into(), /* flash attention: ~2x decode speed on this GPU */
        /* LFM3 mixes sliding-window attention layers; the default SWA
         * cache rejects KV-shift reuse ("cache_reuse is not supported by
         * this context"). --swa-full stores the full window and unlocks
         * prompt-prefix reuse. Both builds support it. */
        "--swa-full".into(),
        /* KV-shifting chunk reuse: a new request may also reuse interior
         * cache chunks, not just the leading prefix. */
        "--cache-reuse".into(),
        "1".into(),
        /* Do not pass -np: an explicit slot count disables the unified KV
         * buffer and cache-reuse gets rejected. Default slots (4 here) are
         * picked per request by LCP similarity, which keeps the warm slot
         * in play. */
        "--no-webui".into(), /* terminal-only use; the port stays API-exclusive */
        "--port".into(),
        PORT.to_string(),
    ];
    if backend == Backend::Gpu {
        args.push("-ngl".into());
        args.push("99".into()); /* offload every layer (Vulkan) */
    }
    /* The server's stderr (startup log; the Vulkan build's failure
     * reasons only appear there) is captured to a file that the poll
     * loop below re-reads â€” no pipe, no reader thread, nothing that
     * could block the server or leak into it. */
    let stderr_log = std::env::temp_dir().join("igris-llama-server-stderr.log");
    let child = spawn_isolated(&exe, &args, &stderr_log)
        .map_err(|e| format!("spawn failed: {e}"))?;

    /* Startup-log state: lines seen so far, and the first GPU failure
     * hint (no device, no VRAM) that triggers the CPU fallback
     * immediately instead of waiting out the timeout. */
    let mut lines: Vec<String> = Vec::new();
    let mut fail_reason: Option<String> = None;
    let is_gpu = backend == Backend::Gpu;
    let started = Instant::now();
    loop {
        if let Ok(text) = std::fs::read_to_string(&stderr_log) {
            let fresh: Vec<String> = text.lines().map(str::to_string).collect();
            if is_gpu && fail_reason.is_none() {
                for line in &fresh {
                    let low = line.to_lowercase();
                    if GPU_FAIL_HINTS.iter().any(|h| low.contains(h)) {
                        fail_reason = Some(line.clone());
                        break;
                    }
                }
            }
            lines = fresh;
        }
        if let Some(reason) = fail_reason.clone() {
            child.kill();
            return Err(reason);
        }
        if child.try_wait() {
            let reason = lines
                .last()
                .cloned()
                .unwrap_or_else(|| "no stderr output".into());
            return Err(format!("exited before becoming healthy: {reason}"));
        }
        if server_ready() {
            return Ok(device_name(&lines));
        }
        if started.elapsed() > backend.startup_timeout() {
            child.kill();
            return Err(format!(
                "{} did not become healthy in time",
                backend.exe().display()
            ));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Best-effort GPU name from the startup log, e.g. a line like
/// "ggml_vulkan: 0 = NVIDIA GeForce MX350 (NVIDIA) | ..." yields
/// "NVIDIA GeForce MX350".
fn device_name(lines: &[String]) -> Option<String> {
    lines.iter().find_map(|line| {
        let rest = line.trim().strip_prefix("ggml_vulkan:")?;
        let idx = rest.find(" = ")?;
        let after = &rest[idx + 3..];
        let end = after.find(" (").unwrap_or(after.len());
        let name = after[..end].trim();
        (!name.is_empty()).then(|| name.to_string())
    })
}

/* ================================================================ */
/* raw completions API                                              */
/* ================================================================ */

/* POST the prompt and return the parsed JSON envelope. */
fn complete(body: Value) -> Result<Value, Box<dyn std::error::Error>> {
    let mut resp = agent()
        .post(&format!("{}/v1/completions", base_url()))
        .header("Content-Type", "application/json")
        .send_json(&body)?;
    if resp.status().as_u16() != 200 {
        let status = resp.status();
        let mut raw = Vec::new();
        let _ = resp.into_body().into_reader().read_to_end(&mut raw);
        return Err(format!(
            "llama-server error {status}: {}",
            String::from_utf8_lossy(&raw)
        )
        .into());
    }
    let raw = resp.body_mut().read_to_string()?;
    Ok(serde_json::from_str(&raw)?)
}

/// Warm the server's prompt cache with text the user has spoken so far.
///
/// This is the heart of "live context": while the user is still talking,
/// the partial transcript is pushed to the model as a max_tokens=1
/// request over the raw completions endpoint. llama-server keeps per-slot
/// KV caches and reuses the longest common prefix of every request, and
/// because the ping prompt is rendered open-ended (no closing <|im_end|>)
/// it is a strict token-prefix of the final request, which only has to
/// process the tokens spoken since the last ping -> the answer starts
/// almost instantly.
///
/// Returns the number of prompt tokens served from cache (0 = cold start).
pub fn prefill(messages: &[Value]) -> Result<u64, Box<dyn std::error::Error>> {
    ensure_server(false)?;
    let body = json!({
        "prompt": render_prompt(messages, false),
        "max_tokens": 1,
        "temperature": 0.0,
        "cache_prompt": true,
    });
    let v = complete(body)?;
    Ok(v["timings"]["cache_n"].as_u64().unwrap_or(0))
}

/// Streaming completion: each generated token is passed to `on_token` the
/// moment it arrives (SSE over the raw completions endpoint). The callback
/// returns false to abort the stream early (e.g. push-to-talk released
/// mid-reply); the reply so far is returned and the rest is discarded.
/// Returns the full reply text and the latency to the first token in ms.
/// The prompt is rendered closed-ended so the model answers after the
/// generation prompt; generation stops at the EOS token, so no template
/// junk reaches the caller.
///
/// `grammar` is an optional GBNF string passed to llama-server's
/// `grammar` sampling constraint (see [`crate::toolgrammar`]); callers
/// pass `None` for normal rounds and the tool-call grammar for
/// corrective retries, where the model must emit a well-formed envelope.
pub fn chat_stream(
    messages: &[Value],
    max_tokens: i64,
    on_token: &mut impl FnMut(&str) -> bool,
    grammar: Option<&str>,
) -> Result<(String, u128, Option<String>), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let mut body = json!({
        "prompt": render_prompt(messages, true),
        "max_tokens": max_tokens,
        "temperature": TEMPERATURE,
        "top_k": TOP_K,
        "stream": true,
        "cache_prompt": true,
    });
    /* `grammar` is llama-server's GBNF sampling constraint (native to the
     * /v1/completions endpoint, live-probed 2026-11-15). It changes only
     * the sampler, never the prompt, so the KV cache is unaffected. */
    if let Some(g) = grammar {
        body["grammar"] = json!(g);
    }
    let resp = agent()
        .post(&format!("{}/v1/completions", base_url()))
        .header("Content-Type", "application/json")
        .send_json(&body)?;
    if resp.status().as_u16() != 200 {
        let status = resp.status();
        let mut raw = Vec::new();
        let _ = resp.into_body().into_reader().read_to_end(&mut raw);
        return Err(format!(
            "llama-server error {status}: {}",
            String::from_utf8_lossy(&raw)
        )
        .into());
    }
    /* The body reader is Read but not BufRead, so SSE lines are split
     * manually from an accumulating byte buffer. Raw completions deltas
     * live in choices[0].text (the chat endpoint uses delta.content). */
    let mut reader = resp.into_body().into_reader();
    let mut out = String::new();
    let mut first_ms: Option<u128> = None;
    let mut stop: Option<String> = None;
    let mut buf: Vec<u8> = Vec::new();
    let mut tmp = [0u8; 8192];
    'outer: loop {
        /* handle every complete line currently in the buffer */
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]);
            let t = line.trim();
            if !t.starts_with("data:") {
                continue;
            }
            let data = t["data:".len()..].trim();
            if data == "[DONE]" {
                break 'outer;
            }
            let v: Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            /* capture the terminal stop reason (both field spellings
             * exist across llama.cpp builds) so the agent loop can tell
             * a natural end from a token-cap truncation (plan P1) */
            if let Some(reason) = stop_reason_from_chunk(&v) {
                stop = Some(reason);
            }
            if let Some(tok) = v["choices"][0]["text"].as_str()
                && !tok.is_empty()
            {
                if first_ms.is_none() {
                    first_ms = Some(t0.elapsed().as_millis());
                }
                if !on_token(tok) {
                    break 'outer;
                }
                out.push_str(tok);
            }
        }
        /* refill */
        match reader.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) => return Err(e.into()),
        }
    }
    let first = first_ms.unwrap_or_else(|| t0.elapsed().as_millis());
    Ok((out, first, stop))
}

/// finish_reason/stop_reason from one SSE completion chunk: raw
/// /v1/completions chunks spell it `finish_reason`, some builds spell
/// `stop_reason` — capture BOTH so a token-cap cut (`"length"`) is
/// visible to the agent loop. Null/empty/absent means "no reason yet".
fn stop_reason_from_chunk(v: &Value) -> Option<String> {
    let c = &v["choices"][0];
    c["finish_reason"]
        .as_str()
        .or_else(|| c["stop_reason"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/* ================================================================ */
/* terminal chat                                                    */
/* ================================================================ */

/// One persona turn streamed to the terminal under the minimal `ai:-`
/// label; tokens print the moment they arrive.
fn stream_persona(
    prompt: &str,
    max_tokens: i64,
    tools_on: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut history: Vec<Value> = vec![
        json!({
            "role": "system",
            "content": system_prompt_with_tools(SYSTEM_PROMPT, tools_on)
        }),
        json!({"role": "user", "content": prompt}),
    ];
    print!("ai:- ");
    std::io::stdout().flush()?;
    if !tools_on {
        /* escape hatch: exactly the pre-tools behavior */
        let mut on_tok = |tok: &str| -> bool {
            print!("{tok}");
            let _ = std::io::stdout().flush();
            true
        };
        chat_stream(&history, max_tokens, &mut on_tok, None)?;
        println!();
        return Ok(());
    }

    let mut on_released = |released: &str| -> bool {
        print!("{released}");
        let _ = std::io::stdout().flush();
        true
    };
    let mut on_tool_round = |names: &[String]| {
        println!("\n[tools] {}", names.join(", "));
    };
    let mut sink = StreamSink {
        on_released: &mut on_released,
        on_tool_round: &mut on_tool_round,
    };
    let end = agent_turn(&mut history, max_tokens, &mut sink)?;
    if let AgentTurnEnd::Exhausted { rounds } = end {
        println!("\n(gave up after {rounds} tool rounds)");
    } else {
        println!();
    }
    Ok(())
}

/// `chat` subcommand: one-shot with arguments, or an interactive loop when
/// called without arguments. Replies stream token-by-token.
pub fn cmd_chat(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut force_cpu = false;
    let mut no_tools = false;
    let mut rest: Vec<&str> = Vec::new();
    for a in args {
        if a == "--cpu" {
            force_cpu = true;
        } else if a == "--no-tools" {
            no_tools = true;
        } else {
            rest.push(a.as_str());
        }
    }
    let tools_on = tools_enabled(no_tools);
    let prompt = rest.join(" ");
    ensure_server(force_cpu)?;

    if !prompt.is_empty() {
        return stream_persona(&prompt, 1024, tools_on);
    }

    println!("Type a prompt (empty line or 'exit' to quit):");
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        line.clear();
        stdin.lock().read_line(&mut line)?;
        let q = line.trim();
        if q.is_empty() || q.eq_ignore_ascii_case("exit") || q.eq_ignore_ascii_case("quit") {
            break;
        }
        if let Err(e) = stream_persona(q, 1024, tools_on) {
            println!("  error: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ZeroClaw protocol block, transcribed independently through
    /// unicode escapes, pinned byte-for-byte against our runtime
    /// construction so the "verbatim port" claim stays true â€” including
    /// our two appended rules (persistent memory, v1.2; gated tools,
    /// v2.5).
    #[test]
    fn tool_call_protocol_instructions_is_upstream_verbatim() {
        let open = "\u{3c}tool_call\u{3e}";
        let close = "\u{3c}/tool_call\u{3e}";
        let result = "\u{3c}tool_result\u{3e}";
        let expected = "## Tool Use Protocol\n\n\
To use a tool, wrap a JSON object in "
            .to_string()
            + open
            + close
            + " tags:\n\n```\n"
            + open
            + "\n{\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n"
            + close
            + "\n```\n\nCRITICAL: Output actual "
            + open
            + " tags\u{2014}never describe steps or give examples.\n\n\
Example: User says something that needs a tool. You MUST respond with a real call naming one of your available tools:\n"
            + open
            + "\n{\"name\": \"tool_name\", \"arguments\": {\"param\": \"value\"}}\n"
            + close
            + "\n\nYou may use multiple tool calls in a single response. After tool execution, results appear in "
            + result
            + " tags. Continue reasoning with the results until you can give a final answer.\n\n\
Persistent memory: personal facts the user has shared (preferences, plans, dates, passwords, notes) are saved in your memory and survive between sessions. When asked about anything personal, call memory_recall first â€” never claim you don't know without checking.\n\n\
Gated tools: their first call only validates the action and announces the plan â€” it does NOT execute, and the result starts with NOT EXECUTED â€” CONFIRMATION REQUIRED. Tell the user exactly what will happen and wait for a clear yes; only after the user confirms, call the same tool again with identical arguments and it will execute. If the user declines or changes the request, do not call it again.\n\n";
        assert_eq!(tool_call_protocol_instructions(), expected);
    }

    #[test]
    fn tools_protocol_block_lists_specs_sorted_by_name() {
        let block = tools_protocol_block(&tools::registry().specs());
        assert!(block.starts_with("\n## Tool Use Protocol"));
        assert!(block.contains("### Available Tools"));
        assert!(block.ends_with("\n\n"));
        let bclk = block.find("**browser_click**").unwrap();
        let bc = block.find("**browser_close**").unwrap();
        let bo = block.find("**browser_open**").unwrap();
        let boac = block.find("**browser_open_and_click**").unwrap();
        let br = block.find("**browser_read**").unwrap();
        let c = block.find("**calculate**").unwrap();
        let ca = block.find("**close_app**").unwrap();
        let cs = block.find("**content_search**").unwrap();
        let cop = block.find("**copy_files**").unwrap();
        let cf = block.find("**create_folder**").unwrap();
        let cn = block.find("**create_note**").unwrap();
        let df = block.find("**delete_file**").unwrap();
        let di = block.find("**disk_stats**").unwrap();
        let dl = block.find("**download_file**").unwrap();
        let erb = block.find("**empty_recycle_bin**").unwrap();
        let fm = block.find("**file_metadata**").unwrap();
        let fd = block.find("**find_duplicates**").unwrap();
        let fw = block.find("**focus_window**").unwrap();
        let fs = block.find("**file_search**").unwrap();
        let g = block.find("**get_time**").unwrap();
        let gri = block.find("**github_repo_info**").unwrap();
        let ld = block.find("**list_dir**").unwrap();
        let lfi = block.find("**list_files**").unwrap();
        let lwi = block.find("**list_windows**").unwrap();
        let mk = block.find("**media_key**").unwrap();
        let mr = block.find("**memory_recall**").unwrap();
        let ms = block.find("**memory_save**").unwrap();
        let o = block.find("**open_app**").unwrap();
        let ou = block.find("**open_url**").unwrap();
        let pm = block.find("**power_mode**").unwrap();
        let ps = block.find("**process_stats**").unwrap();
        let rf = block.find("**read_file**").unwrap();
        let rb = block.find("**recycle_bin**").unwrap();
        let rs = block.find("**reminder_set**").unwrap();
        let rp = block.find("**replace_file**").unwrap();
        let rc = block.find("**run_command**").unwrap();
        let sc = block.find("**screenshot**").unwrap();
        let sv = block.find("**set_volume**").unwrap();
        let sd = block.find("**shutdown**").unwrap();
        let sda = block.find("**shutdown_abort**").unwrap();
        let ss = block.find("**sys_stats**").unwrap();
        let wf = block.find("**web_fetch**").unwrap();
        let ws = block.find("**web_search**").unwrap();
        let wsi = block.find("**workspace_info**").unwrap();
        let wri = block.find("**write_file**").unwrap();
        assert!(
            bclk < bc
                && bc < bo
                && bo < boac
                && boac < br
                && br < c
                && c < ca
                && ca < cs
                && cs < cop
                && cop < cf
                && cf < cn
                && cn < df
                && df < di
                && di < dl
                && dl < erb
                && erb < fm
                && fm < fs
                && fs < fd
                && fd < fw
                && fw < g
                && g < gri
                && gri < ld
                && ld < lfi
                && lfi < lwi
                && lwi < mk
                && mk < mr
                && mr < ms
                && ms < o
                && o < ou
                && ou < pm
                && pm < ps
                && ps < rf
                && rf < rb
                && rb < rs
                && rs < rp
                && rp < rc
                && rc < sc
                && sc < sv
                && sv < sd
                && sd < sda
                && sda < ss
                && ss < wf
                && wf < ws
                && ws < wsi
                && wsi < wri
        );
        // Every entry carries ONE canonical call-arguments example (plan
        // P0): the raw schema dump made the model echo "properties"
        // shapes as data (the open_app defect class).
        assert_eq!(block.matches("Call arguments: `").count(), 46);
        assert!(!block.contains("\"required\""));
        assert!(!block.contains("properties"));
        // The calculate tenant's function enum is enumerated in its
        // example instead of advertising a schema.
        assert!(block.contains("\"function\": \"add|subtract|multiply"));
    }

    #[test]
    fn tools_protocol_block_empty_specs_is_empty() {
        assert_eq!(tools_protocol_block(&[]), "");
    }

    #[test]
    fn system_prompt_with_tools_disabled_is_exactly_base() {
        assert_eq!(
            system_prompt_with_tools(VOICE_SYSTEM_PROMPT, false),
            VOICE_SYSTEM_PROMPT
        );
    }

    #[test]
    fn system_prompt_with_tools_is_deterministic() {
        let a = system_prompt_with_tools(VOICE_SYSTEM_PROMPT, true);
        let b = system_prompt_with_tools(VOICE_SYSTEM_PROMPT, true);
        assert_eq!(a, b);
        assert!(a.starts_with(VOICE_SYSTEM_PROMPT));
        assert!(a.contains("## Tool Use Protocol"));
    }

    #[test]
    fn registry_schemas_round_trip_through_serde_json() {
        for spec in tools::registry().specs() {
            let rendered = spec.parameters_schema.to_string();
            let back: Value = serde_json::from_str(&rendered)
                .unwrap_or_else(|e| panic!("schema for {} is not valid JSON: {e}", spec.name));
            assert_eq!(back, spec.parameters_schema, "schema of {}", spec.name);
        }
    }

    #[test]
    fn tools_enabled_flag_composition() {
        assert!(!tools_enabled(true));
    }

    /* ---- agent loop glue (Phase 4) ---- */

    #[test]
    fn agent_loop_iteration_budget_caps_at_ten() {
        let mut s = AgentLoopState::new();
        for _ in 0..MAX_TOOL_ITERATIONS {
            assert!(s.next_round());
        }
        assert!(!s.next_round());
        assert_eq!(s.rounds, MAX_TOOL_ITERATIONS);
    }

    #[test]
    fn malformed_retry_budget_caps_at_two() {
        let mut s = AgentLoopState::new();
        // Internal envelope-shaped JSON, no tags: deterministically flagged
        // by detect_tool_call_parse_issue, with nothing recoverable.
        let raw = "{\"tool_calls\": [{\"name\": \"get_time\", \"arguments\": {}}]}";
        assert!(s.malformed_verdict(raw, &[]).is_some());
        assert!(s.malformed_verdict(raw, &[]).is_some());
        assert!(
            s.malformed_verdict(raw, &[]).is_none(),
            "retry budget caps at {MAX_MALFORMED_RETRIES}"
        );
        assert_eq!(s.retries, MAX_MALFORMED_RETRIES);
    }

    #[test]
    fn a_reply_with_tool_calls_resets_the_retry_budget() {
        let mut s = AgentLoopState::new();
        let raw = "{\"tool_calls\": [{\"name\": \"get_time\", \"arguments\": {}}]}";
        assert!(s.malformed_verdict(raw, &[]).is_some());
        let call = toolparse::ParsedToolCall {
            name: "get_time".to_string(),
            arguments: json!({}),
            tool_call_id: None,
        };
        assert!(s.malformed_verdict(raw, &[call]).is_none());
        assert_eq!(s.retries, 0);
        // The budget is spendable again after the reset.
        assert!(s.malformed_verdict(raw, &[]).is_some());
    }

    #[test]
    fn dsml_drift_flags_first_contact_and_spares_post_success() {
        use toolparse::{DSML_CLOSE, DSML_OPEN};
        /* prose-first mangled DSML envelope: the exact live round-1
         * failure (2026-11-15) — parses to zero calls and is NOT flagged
         * by the issue detector, which only catches DSML openers */
        let raw = format!(
            "I'll move that file to the recycle bin.\n{}tool_name delete_file{}",
            DSML_OPEN, DSML_CLOSE
        );
        let mut s = AgentLoopState::new();
        let verdict = s.dsml_drift_verdict(&raw);
        match &verdict {
            Some(note) => assert!(note.contains("DSML"), "{note}"),
            None => panic!("first-contact drift must be corrected"),
        }
        assert_eq!(s.retries, 1, "drift shares the retry budget");

        /* after a round that executed tools, the same shape is the
         * prose answer with a held-back echo — no corrective round */
        let mut s2 = AgentLoopState::new();
        s2.executed_any = true;
        assert!(s2.dsml_drift_verdict(&raw).is_none());
        assert_eq!(s2.retries, 0);

        /* and the drift budget is the shared malformed budget */
        let mut s3 = AgentLoopState::new();
        assert!(s3.dsml_drift_verdict(&raw).is_some());
        assert!(s3.dsml_drift_verdict(&raw).is_some());
        assert!(
            s3.dsml_drift_verdict(&raw).is_none(),
            "drift caps at {MAX_MALFORMED_RETRIES}"
        );
    }

    fn invalid_outcome(name: &str, output: &str) -> ToolOutcome {
        ToolOutcome {
            name: name.to_string(),
            status: "error".to_string(),
            output: output.to_string(),
            phase: crate::tools::DispatchPhase::Invalid,
        }
    }

    #[test]
    fn validation_ladder_corrective_then_grammar_then_standdown() {
        use crate::tools::DispatchPhase;
        let mut s = AgentLoopState::new();
        let boom = [invalid_outcome("delete_file", "cannot plan 'delete_file': missing path")];
        /* streak 1: one free corrective retry, unconstrained */
        let step = s.validation_verdict(&boom);
        match &step {
            ValidationStep::Corrective(note) => {
                assert!(note.contains("canonical JSON envelope"), "{note}");
                assert!(note.contains("missing path"), "{note}");
                assert!(
                    note.contains("answer in plain words"),
                    "the free round must be allowed to ask instead"
                );
            }
            other => panic!("expected Corrective, got {other:?}"),
        }
        /* streak 2: the model flailed again â€” grammar-force */
        let step = s.validation_verdict(&boom);
        assert_eq!(step, ValidationStep::ForceGrammar);
        /* streak 3: stand down â€” stop spending rounds on constraints */
        let step = s.validation_verdict(&boom);
        assert_eq!(step, ValidationStep::None);
        /* and a round with a real execution resets everything */
        let ok = [ToolOutcome {
            name: "get_time".to_string(),
            status: "ok".to_string(),
            output: "14:00".to_string(),
            phase: DispatchPhase::Ran,
        }];
        assert_eq!(s.validation_verdict(&ok), ValidationStep::None);
        assert_eq!(s.invalid_streak, 0);
        /* after the reset the ladder starts from corrective again */
        assert!(matches!(
            s.validation_verdict(&boom),
            ValidationStep::Corrective(_)
        ));
    }

    #[test]
    fn validation_ladder_never_fires_on_progress() {
        use crate::tools::DispatchPhase;
        let mut s = AgentLoopState::new();
        let echo = |phase| ToolOutcome {
            name: "t".to_string(),
            status: "error".to_string(),
            output: "x".to_string(),
            phase,
        };
        /* an armed announce waits on the user â€” not a failure */
        assert_eq!(
            s.validation_verdict(&[echo(DispatchPhase::Armed)]),
            ValidationStep::None
        );
        assert_eq!(s.invalid_streak, 0);
        /* a mix of one real result + one invalid call: made progress */
        let mixed = [
            echo(DispatchPhase::Ran),
            echo(DispatchPhase::Invalid),
        ];
        assert_eq!(s.validation_verdict(&mixed), ValidationStep::None);
        assert_eq!(s.invalid_streak, 0, "mixed rounds must not accumulate");
    }

    /* ---- task-class budget (plan P3 item 9) ---- */

    #[test]
    fn task_budget_shapes_rounds_and_deadline_notices() {
        use crate::budget::{budget_for, TaskClass};
        /* the default (Simple) keeps the historical 10-round budget */
        let mut s = AgentLoopState::new();
        assert_eq!(s.budget.max_rounds, MAX_TOOL_ITERATIONS);
        for _ in 0..MAX_TOOL_ITERATIONS {
            assert!(s.next_round());
        }
        assert!(!s.next_round());

        /* report class: 12 rounds, notices fire once at 2 and 1 left */
        let mut s = AgentLoopState::new();
        s.budget = budget_for(TaskClass::Report);
        for _ in 0..9 {
            assert!(s.next_round());
            assert_eq!(s.round_start_notice(), None);
        }
        assert!(s.next_round()); /* round 10 of 12: two remain */
        let n2 = s.round_start_notice().unwrap();
        assert!(n2.starts_with("BUDGET: 2 tool rounds left"), "{n2}");
        assert_eq!(s.round_start_notice(), None, "each notice fires once");
        assert!(s.next_round()); /* round 11: one remains */
        let n1 = s.round_start_notice().unwrap();
        assert!(n1.starts_with("BUDGET: this is your LAST tool round"), "{n1}");
        assert!(s.next_round()); /* round 12 runs */
        assert!(!s.next_round()); /* 13 is refused */

        /* simple class: notices land at rounds 8 and 9 */
        let mut s = AgentLoopState::new();
        for _ in 0..7 {
            assert!(s.next_round());
            assert_eq!(s.round_start_notice(), None);
        }
        assert!(s.next_round()); /* round 8 of 10 */
        assert!(s
            .round_start_notice()
            .unwrap()
            .starts_with("BUDGET: 2 tool rounds left"));
        assert!(s.next_round()); /* round 9 */
        assert!(s
            .round_start_notice()
            .unwrap()
            .starts_with("BUDGET: this is your LAST tool round"));
    }

    #[test]
    fn task_budget_decides_calls_web_and_totals() {
        use crate::budget::{budget_for, TaskClass};
        let mut s = AgentLoopState::new();
        s.budget = budget_for(TaskClass::Research); /* web 6, calls 24 */
        for i in 0..6 {
            assert!(s.decide_call("web_search").is_ok(), "web call {i}");
        }
        /* web budget spent: refusal, and the refused call is not counted */
        let err = s.decide_call("web_fetch").unwrap_err();
        assert!(err.contains("Web budget for this task is spent (6/6"), "{err}");
        assert!(err.contains("deliver the answer now"), "{err}");
        /* non-web tools still run until the total call budget is spent */
        for _ in 0..18 {
            assert!(s.decide_call("read_file").is_ok());
        }
        assert_eq!(s.tool_calls_used, 24);
        let err = s.decide_call("read_file").unwrap_err();
        assert!(
            err.contains("Tool-call budget for this task is spent (24/24"),
            "{err}"
        );
    }

    #[test]
    fn task_budget_blocks_unsupported_tools_after_first_report() {
        use crate::budget::UNSUPPORTED_PREFIX;
        use crate::tools::DispatchPhase;
        let mut s = AgentLoopState::new();
        let boom = ToolOutcome {
            name: "power_mode".to_string(),
            status: "error".to_string(),
            output: format!(
                "{UNSUPPORTED_PREFIX}the Power saver plan is not configured on this machine"
            ),
            phase: DispatchPhase::Ran,
        };
        let note = s.unsupported_verdict(std::slice::from_ref(&boom)).unwrap();
        assert!(note.contains("power_mode"), "{note}");
        assert!(note.contains("Do not call it again"), "{note}");
        /* idempotent: a second report adds no new note */
        assert_eq!(s.unsupported_verdict(std::slice::from_ref(&boom)), None);
        /* blocked from the next round: deterministic refusal, no dispatch */
        let err = s.decide_call("power_mode").unwrap_err();
        assert!(err.contains("unavailable on this system"), "{err}");
        /* the block does not leak to other tools */
        assert!(s.decide_call("get_time").is_ok());
        /* an ordinary (transient) error never blocks */
        let soft = ToolOutcome {
            name: "web_fetch".to_string(),
            status: "error".to_string(),
            output: "HTTP 503 from example.com".to_string(),
            phase: DispatchPhase::Ran,
        };
        assert_eq!(s.unsupported_verdict(std::slice::from_ref(&soft)), None);
        assert!(s.decide_call("web_fetch").is_ok());
    }

    #[test]
    fn dedup_tool_calls_collapses_repeated_identical_calls() {
        let g = || toolparse::ParsedToolCall {
            name: "get_time".to_string(),
            arguments: json!({}),
            tool_call_id: None,
        };
        let c = toolparse::ParsedToolCall {
            name: "calculate".to_string(),
            arguments: json!({"expression": "2+2"}),
            tool_call_id: None,
        };
        let mut c2 = c.clone();
        c2.arguments = json!({"expression": "7*6"});
        let out = dedup_tool_calls(vec![g(), c, g(), c2, g()]);
        // Two duplicate get_time calls collapse; the distinct calculate
        // calls (different args = different signature) both survive.
        assert_eq!(out.len(), 3, "out: {out:?}");
        assert_eq!(out[0].name, "get_time");
        assert_eq!(out[1].name, "calculate");
        assert_eq!(out[1].arguments["expression"], "2+2");
        assert_eq!(out[2].arguments["expression"], "7*6");
    }

    #[test]
    fn tool_results_user_message_carrier_shape() {
        let outcomes = vec![
            ToolOutcome {
                name: "get_time".to_string(),
                status: "ok".to_string(),
                output: "2026-11-13 14:00:00 on Friday".to_string(),
                phase: crate::tools::DispatchPhase::Ran,
            },
            ToolOutcome {
                name: "nope".to_string(),
                status: "error".to_string(),
                output: "unknown tool: nope".to_string(),
                phase: crate::tools::DispatchPhase::Invalid,
            },
        ];
        let msg = tool_results_user_message(&outcomes);
        assert!(msg.starts_with("[Tool results]\n"));
        let open_line = format!("\n{}\n", toolparse::TOOL_RESULT_OPEN);
        assert_eq!(msg.matches(&open_line).count(), 2);
        assert!(msg.contains("\"name\":\"get_time\""));
        assert!(msg.contains("\"status\":\"ok\""));
        assert!(msg.contains("\"status\":\"error\""));
        assert!(msg.trim_end().ends_with(toolparse::TOOL_RESULT_CLOSE));
    }

    /* ---- BUG-006: server-reuse identity check ---- */

    #[test]
    fn expected_model_id_is_the_bundled_gguf_name() {
        assert_eq!(expected_model_id(), "lfm3-omni-2b-q4_k_m.gguf");
    }

    #[test]
    fn props_model_id_extraction_tolerates_build_shapes() {
        /* The live bundled build (b12110): top-level model_path with a
         * full Windows path (observed 2026-11-15 on :8081). */
        let live = r#"{"model_path":"E:\\proj\\assets\\llama-cpp/models/LFM3-Omni-2B-Q4_K_M.gguf","build_info":"b12110"}"#;
        assert_eq!(
            parse_props_model_id(live).as_deref(),
            Some("lfm3-omni-2b-q4_k_m.gguf")
        );
        /* Older spelling: model_alias. */
        let alias = r#"{"model_alias":"/models/LFM3-Omni-2B-Q4_K_M.gguf"}"#;
        assert_eq!(
            parse_props_model_id(alias).as_deref(),
            Some("lfm3-omni-2b-q4_k_m.gguf")
        );
        /* A wrong model is extracted as itself (the caller compares). */
        let wrong = r#"{"model_path":"C:\\other\\Qwen2-0.5B.gguf"}"#;
        assert_eq!(parse_props_model_id(wrong).as_deref(), Some("qwen2-0.5b.gguf"));
        /* Foreign/404/garbage bodies carry no identity. */
        assert_eq!(parse_props_model_id("\x3chtml\x3e404\x3c/html\x3e"), None);
        assert_eq!(parse_props_model_id(r#"{"total_slots":4}"#), None);
        assert_eq!(parse_props_model_id(""), None);
    }

    /* ---- compact tool catalog + recovery (plan P0/P1) ---- */

    #[test]
    fn compact_args_example_required_first_and_enums() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "description": "optional cap"},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
                "path": {"type": "string"}
            },
            "required": ["path"]
        });
        /* required keys first (BTreeMap order among equals), enums
         * joined with '|', integers as 0, strings as "..." */
        assert_eq!(
            compact_args_example(&schema),
            "{\"path\": \"...\", \"limit\": 0, \"mode\": \"fast|slow\"}"
        );
    }

    #[test]
    fn compact_args_example_handles_empty_and_shapeless_schemas() {
        assert_eq!(
            compact_args_example(&json!({"type": "object", "properties": {}, "required": []})),
            "{}"
        );
        assert_eq!(compact_args_example(&json!({})), "{}");
    }

    #[test]
    fn placeholder_for_covers_the_common_types() {
        assert_eq!(placeholder_for(&json!({"type": "string"})), "\"...\"");
        assert_eq!(placeholder_for(&json!({"type": "integer"})), "0");
        assert_eq!(placeholder_for(&json!({"type": "number"})), "0");
        assert_eq!(placeholder_for(&json!({"type": "boolean"})), "false");
        assert_eq!(placeholder_for(&json!({"type": "array"})), "[]");
        assert_eq!(placeholder_for(&json!({"type": "object"})), "{}");
        assert_eq!(
            placeholder_for(&json!({"type": "string", "enum": ["a", "b"]})),
            "\"a|b\""
        );
        /* a non-string enum falls through to the type spelling */
        assert_eq!(placeholder_for(&json!({"type": "integer", "enum": [1, 2]})), "0");
    }

    #[test]
    fn truncation_verdict_allows_one_continuation_per_turn() {
        let mut s = AgentLoopState::new();
        let note = s
            .truncation_verdict(Some("length"))
            .expect("first token-cap cut must continue");
        assert!(note.contains("cut off"), "{note}");
        /* the budget is one: the second cut becomes the (honest) final
         * reply instead of looping forever */
        assert!(s.truncation_verdict(Some("length")).is_none());
        /* other stop reasons are natural ends, never continuations */
        let mut t = AgentLoopState::new();
        assert!(t.truncation_verdict(Some("stop")).is_none());
        assert!(t.truncation_verdict(Some("eos")).is_none());
        assert!(t.truncation_verdict(None).is_none());
    }

    #[test]
    fn compact_history_shrinks_middle_and_keeps_short_tail_verbatim() {
        let long = "x".repeat(2000);
        let mut history = vec![
            json!({"role": "system", "content": "SYSTEM"}),
            json!({"role": "user", "content": "USER REQUEST"}),
            json!({"role": "assistant", "content": long.clone()}),
            json!({"role": "user", "content": long}),
            json!({"role": "assistant", "content": "tail one"}),
            json!({"role": "user", "content": "tail two"}),
            json!({"role": "assistant", "content": "tail three"}),
            json!({"role": "user", "content": "tail four"}),
        ];
        compact_history(&mut history);
        assert_eq!(history.len(), 8);
        assert_eq!(history[0]["content"], "SYSTEM");
        assert_eq!(history[1]["content"], "USER REQUEST");
        let middle = history[2]["content"].as_str().unwrap();
        assert!(middle.contains("[truncated"), "{middle}");
        assert!(middle.len() < 700, "{}", middle.len());
        assert_eq!(history[4]["content"], "tail one");
        assert_eq!(history[5]["content"], "tail two");
        assert_eq!(history[6]["content"], "tail three");
        assert_eq!(history[7]["content"], "tail four");
    }

    #[test]
    fn compact_history_shrinks_giant_tail_payloads() {
        /* the rerun's S2-7F shape: the round's giant tool-result batch
         * sits in the kept tail — the compaction must bound it, or the
         * retry overflows a second time and dies (StatusCode 400) */
        let mut history = vec![
            json!({"role": "system", "content": "S"}),
            json!({"role": "user", "content": "U"}),
            json!({"role": "user", "content": "x".repeat(5000)}),
        ];
        compact_history(&mut history);
        /* seed and original request stay verbatim even in short
         * histories */
        assert_eq!(history[0]["content"], "S");
        assert_eq!(history[1]["content"], "U");
        let content = history[2]["content"].as_str().unwrap();
        assert!(
            content.contains("[truncated: showing first 800 of 5000"),
            "{content}"
        );
        /* a short history is no longer a blanket exemption: only
         * seed + request are, and they were never longer than the caps */
    }

    #[test]
    fn transport_and_overflow_matchers() {
        assert!(is_transport_error("Peer disconnected"));
        assert!(is_transport_error(
            "error sending request: Connection refused (os error 10061)"
        ));
        assert!(is_transport_error("Broken pipe (os error 109)"));
        /* a slow stream is NOT a transport error: never restart it */
        assert!(!is_transport_error("request timed out"));
        assert!(!is_transport_error("llama-server error 400: too long"));
        assert!(is_context_overflow(
            "llama-server error 400: {\"error\":\"the request exceeds the available context size\"}"
        ));
        /* ureq's own Error::StatusCode Display (the rerun's dying shape
         * when a call site bypasses our status checks) */
        assert!(is_context_overflow("http status: 400"));
        assert!(!is_context_overflow("llama-server error 500: boom"));
        assert!(!is_context_overflow("http status: 502"));
        assert!(!is_context_overflow("Peer disconnected"));
    }

    #[test]
    fn stop_reason_from_chunk_reads_both_field_spellings() {
        assert_eq!(
            stop_reason_from_chunk(&json!({
                "choices": [{"text": "x", "finish_reason": "length"}]
            })),
            Some("length".to_string())
        );
        assert_eq!(
            stop_reason_from_chunk(&json!({
                "choices": [{"text": "x", "stop_reason": "eos"}]
            })),
            Some("eos".to_string())
        );
        assert_eq!(
            stop_reason_from_chunk(&json!({
                "choices": [{"text": "x", "finish_reason": null}]
            })),
            None
        );
        assert_eq!(stop_reason_from_chunk(&json!({"choices": []})), None);
    }
}
