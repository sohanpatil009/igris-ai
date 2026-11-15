//! Local LLM inference through llama.cpp (no Ollama).
//!
//! The GGUF in `assets/llama-cpp/model/` is served by the bundled
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
/* Fragments under the resolved asset root (crate::paths) — resolved at
 * use time so every front-end finds them regardless of its CWD. */
const LLAMA_GPU_EXE: &str = "llama-cpp/vulkan/llama-server.exe";
const LLAMA_CPU_EXE: &str = "llama-cpp/cpu/llama-server.exe";
const LLM_MODEL: &str = "llama-cpp/model/LFM3-1.7B-Q4_K_M.gguf";
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
         Persistent memory: personal facts the user has shared (preferences, plans, dates, passwords, notes) are saved in your memory and survive between sessions. When asked about anything personal, call memory_recall first — never claim you don't know without checking.\n\n",
        open = toolparse::TOOL_CALL_OPEN,
        close = toolparse::TOOL_CALL_CLOSE,
        result = toolparse::TOOL_RESULT_OPEN,
    )
}

/// The tool-calling prompt block: the verbatim protocol instructions plus
/// the `### Available Tools` listing (ZeroClaw's `build_tool_instructions`
/// shape, zeroclaw-runtime/src/agent/loop_.rs:1358-1383), with specs sorted
/// by name so the system prompt is byte-stable for the KV cache.
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
            "**{}**: {}\nParameters: `{}`\n",
            spec.name, spec.description, spec.parameters_schema
        );
    }
    s
}

/// Tools are always on (open decision 4, resolved 2026-11-13); the
/// `--no-tools` flag or `MOON_NO_TOOLS=1` drops the protocol block so the
/// assistant behaves exactly as before tool calling existed.
pub fn tools_enabled(no_tools_flag: bool) -> bool {
    !no_tools_flag && std::env::var("MOON_NO_TOOLS").is_err()
}

/// System prompt for a history seed: the `base` persona plus the tool
/// protocol block when tools are on. Called once at seed time — the system
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

/// Loop bookkeeping for one agent turn: the iteration budget and the
/// malformed-retry budget, kept pure so the caps are unit-testable
/// without a live server.
pub struct AgentLoopState {
    pub rounds: usize,
    pub retries: usize,
}

impl AgentLoopState {
    pub fn new() -> Self {
        AgentLoopState {
            rounds: 0,
            retries: 0,
        }
    }

    /// Start the next streamed round; `false` once the iteration budget
    /// is exhausted.
    pub fn next_round(&mut self) -> bool {
        if self.rounds >= MAX_TOOL_ITERATIONS {
            return false;
        }
        self.rounds += 1;
        true
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
}

/// One executed tool call, as it flows into the results message.
pub struct ToolOutcome {
    pub name: String,
    pub status: String, // "ok" | "error"
    pub output: String,
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
/// executed call. Plain appended text — `render_prompt` needs no change
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
    /// The iteration budget ran out while the model kept calling tools.
    Exhausted,
}

/// One agent turn on `history` (which already holds the system seed and
/// the user message): stream a reply with protocol hold-back, parse it,
/// execute any tool calls against the registry, feed the results back as
/// a user message, and repeat — until a plain final reply, a barge-in, or
/// the iteration budget. History only ever grows (each round appends its
/// raw assistant envelopes and the results message), so every round is a
/// strict prompt-prefix of the next and `cache_prompt` stays effective.
pub fn agent_turn(
    history: &mut Vec<Value>,
    max_tokens: i64,
    sink: &mut StreamSink,
) -> Result<AgentTurnEnd, Box<dyn std::error::Error>> {
    let registry = tools::registry();
    let mut state = AgentLoopState::new();
    while state.next_round() {
        let msgs = history.clone();
        let mut filter = toolparse::StreamFilter::new();
        let mut cut = false;
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
        let (raw, _first_ms) = chat_stream(&msgs, max_tokens, &mut on_tok)?;
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
        let tail = filter.finish();
        if !tail.is_empty() && !(sink.on_released)(&tail) {
            return Ok(AgentTurnEnd::Cut);
        }

        let (_cleaned, calls) = toolparse::parse_tool_calls(&raw);
        if calls.is_empty() {
            if let Some(corrective) = state.malformed_verdict(&raw, &calls) {
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
        let stored = toolparse::strip_trailing_terminal_markers(&raw);
        history.push(json!({"role": "assistant", "content": stored}));
        let names: Vec<String> = calls.iter().map(|c| c.name.clone()).collect();
        (sink.on_tool_round)(&names);
        let mut outcomes: Vec<ToolOutcome> = Vec::new();
        for call in &calls {
            let (status, output) = match registry.dispatch(&call.name, &call.arguments) {
                Ok(out) => ("ok", out),
                Err(e) => ("error", e),
            };
            outcomes.push(ToolOutcome {
                name: call.name.clone(),
                status: status.to_string(),
                output,
            });
        }
        history.push(json!({
            "role": "user",
            "content": tool_results_user_message(&outcomes)
        }));
    }
    Ok(AgentTurnEnd::Exhausted)
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
/// "lfm3-1.7b-q4_k_m.gguf" — the identity a reused server must carry.
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
         * model is up" — a wrong-model or foreign server on the port
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
/// inherits *every* inheritable handle this process holds — std always
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
/// hold. Closing our process handle never kills the child — the server
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
     * [stdin, stdout, stderr]. */
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
        let handles = [h_in, h_out, h_err];
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
            true, /* bInheritHandles — the attribute list is what restricts */
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
     * CreateProcessW — drop our copies, list first. */
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
     * loop below re-reads — no pipe, no reader thread, nothing that
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
pub fn chat_stream(
    messages: &[Value],
    max_tokens: i64,
    on_token: &mut impl FnMut(&str) -> bool,
) -> Result<(String, u128), Box<dyn std::error::Error>> {
    let t0 = Instant::now();
    let body = json!({
        "prompt": render_prompt(messages, true),
        "max_tokens": max_tokens,
        "temperature": TEMPERATURE,
        "top_k": TOP_K,
        "stream": true,
        "cache_prompt": true,
    });
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
    Ok((out, first))
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
        chat_stream(&history, max_tokens, &mut on_tok)?;
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
    if matches!(end, AgentTurnEnd::Exhausted) {
        println!("\n(gave up after {MAX_TOOL_ITERATIONS} tool rounds)");
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
        return stream_persona(&prompt, 512, tools_on);
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
        if let Err(e) = stream_persona(q, 512, tools_on) {
            println!("  error: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The upstream protocol block, transcribed independently through
    /// unicode escapes, pinned byte-for-byte against our runtime
    /// construction so the "verbatim port" claim stays true.
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
Persistent memory: personal facts the user has shared (preferences, plans, dates, passwords, notes) are saved in your memory and survive between sessions. When asked about anything personal, call memory_recall first — never claim you don't know without checking.\n\n";
        assert_eq!(tool_call_protocol_instructions(), expected);
    }

    #[test]
    fn tools_protocol_block_lists_specs_sorted_by_name() {
        let block = tools_protocol_block(&tools::registry().specs());
        assert!(block.starts_with("\n## Tool Use Protocol"));
        assert!(block.contains("### Available Tools"));
        assert!(block.ends_with("\n\n"));
        let c = block.find("**calculate**").unwrap();
        let cs = block.find("**content_search**").unwrap();
        let cf = block.find("**create_folder**").unwrap();
        let cn = block.find("**create_note**").unwrap();
        let df = block.find("**download_file**").unwrap();
        let fs = block.find("**file_search**").unwrap();
        let g = block.find("**get_time**").unwrap();
        let ld = block.find("**list_dir**").unwrap();
        let mk = block.find("**media_key**").unwrap();
        let mr = block.find("**memory_recall**").unwrap();
        let ms = block.find("**memory_save**").unwrap();
        let o = block.find("**open_app**").unwrap();
        let ou = block.find("**open_url**").unwrap();
        let rf = block.find("**read_file**").unwrap();
        let rs = block.find("**reminder_set**").unwrap();
        let rc = block.find("**run_command**").unwrap();
        let sc = block.find("**screenshot**").unwrap();
        let sv = block.find("**set_volume**").unwrap();
        let ss = block.find("**sys_stats**").unwrap();
        let wf = block.find("**web_fetch**").unwrap();
        let ws = block.find("**web_search**").unwrap();
        assert!(
            c < cs
                && cs < cf
                && cf < cn
                && cn < df
                && df < fs
                && fs < g
                && g < ld
                && ld < mk
                && mk < mr
                && mr < ms
                && ms < o
                && o < ou
                && ou < rf
                && rf < rs
                && rs < rc
                && rc < sc
                && sc < sv
                && sv < ss
                && ss < wf
                && wf < ws
        );
        // Every entry carries its schema on a Parameters line.
        assert_eq!(block.matches("Parameters: `").count(), 22);
        // The calculate tenant's structured schema is advertised.
        assert!(block.contains("\"required\":[\"function\"]"));
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
            },
            ToolOutcome {
                name: "nope".to_string(),
                status: "error".to_string(),
                output: "unknown tool: nope".to_string(),
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
        assert_eq!(expected_model_id(), "lfm3-1.7b-q4_k_m.gguf");
    }

    #[test]
    fn props_model_id_extraction_tolerates_build_shapes() {
        /* The live bundled build (b12110): top-level model_path with a
         * full Windows path (observed 2026-11-15 on :8081). */
        let live = r#"{"model_path":"E:\\proj\\assets\\llama-cpp/model/LFM3-1.7B-Q4_K_M.gguf","build_info":"b12110"}"#;
        assert_eq!(
            parse_props_model_id(live).as_deref(),
            Some("lfm3-1.7b-q4_k_m.gguf")
        );
        /* Older spelling: model_alias. */
        let alias = r#"{"model_alias":"/models/LFM3-1.7B-Q4_K_M.gguf"}"#;
        assert_eq!(
            parse_props_model_id(alias).as_deref(),
            Some("lfm3-1.7b-q4_k_m.gguf")
        );
        /* A wrong model is extracted as itself (the caller compares). */
        let wrong = r#"{"model_path":"C:\\other\\Qwen2-0.5B.gguf"}"#;
        assert_eq!(parse_props_model_id(wrong).as_deref(), Some("qwen2-0.5b.gguf"));
        /* Foreign/404/garbage bodies carry no identity. */
        assert_eq!(parse_props_model_id("<html>404</html>"), None);
        assert_eq!(parse_props_model_id(r#"{"total_slots":4}"#), None);
        assert_eq!(parse_props_model_id(""), None);
    }
}
