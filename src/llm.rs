//! Local LLM inference through llama.cpp (no Ollama).
//!
//! The GGUF in `models/llm/` is served by the bundled `llama-server`
//! (Vulkan GPU build in `models/llm-gpu/`), which exposes an HTTP API on
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

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/* Two llama.cpp builds ship with the repo: a Vulkan (GPU) build in
 * models/llm-gpu and a pure-CPU build in tools/llama-cpp (both recent
 * enough for every flag below). ensure_server() tries the GPU build
 * first and falls back to the CPU build whenever it cannot run on this
 * machine - no Vulkan driver, too little VRAM, early crash, startup
 * timeout - so the same app runs on gaming rigs and office laptops
 * alike. `--cpu` (or MOON_LLM_CPU=1) skips the GPU attempt. NOTE: if the
 * flags below change, kill any leftover llama-server so the next run
 * starts one with the new configuration. */
const LLAMA_GPU_EXE: &str = "models/llm-gpu/llama-server.exe";
const LLAMA_CPU_EXE: &str = "tools/llama-cpp/llama-server.exe";
const LLM_MODEL: &str = "models/llm/LFM3-1.7B-Q4_K_M.gguf";
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

/// Which llama.cpp build to try. The GPU build is preferred; the CPU
/// build runs on any Windows x64 machine.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Backend {
    Gpu,
    Cpu,
}

impl Backend {
    fn exe(self) -> &'static str {
        match self {
            Backend::Gpu => LLAMA_GPU_EXE,
            Backend::Cpu => LLAMA_CPU_EXE,
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
        println!("  llm       : reusing already-running llama-server (:{PORT})");
        return Ok(());
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

/// Spawn one backend and poll it until healthy. Ok carries the GPU device
/// name when one shows up in the startup log. The child is deliberately
/// not killed on success: it outlives this process and keeps serving for
/// later runs of the app.
fn try_start(backend: Backend) -> Result<Option<String>, String> {
    if !std::path::Path::new(backend.exe()).exists() {
        return Err(format!(
            "{} not found (run from the repo root)",
            backend.exe()
        ));
    }
    let mut cmd = Command::new(backend.exe());
    cmd.arg("-m")
        .arg(LLM_MODEL)
        .arg("-c")
        .arg(CTX_TOKENS.to_string())
        .arg("-fa")
        .arg("on") /* flash attention: ~2x decode speed on this GPU */
        /* LFM3 mixes sliding-window attention layers; the default SWA
         * cache rejects KV-shift reuse ("cache_reuse is not supported by
         * this context"). --swa-full stores the full window and unlocks
         * prompt-prefix reuse. Both builds support it. */
        .arg("--swa-full")
        /* KV-shifting chunk reuse: a new request may also reuse interior
         * cache chunks, not just the leading prefix. */
        .arg("--cache-reuse")
        .arg("1")
        /* Do not pass -np: an explicit slot count disables the unified KV
         * buffer and cache-reuse gets rejected. Default slots (4 here) are
         * picked per request by LCP similarity, which keeps the warm slot
         * in play. */
        .arg("--no-webui") /* terminal-only use; the port stays API-exclusive */
        .arg("--port")
        .arg(PORT.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if backend == Backend::Gpu {
        cmd.arg("-ngl").arg("99"); /* offload every layer (Vulkan) */
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let stderr = child.stderr.take().expect("stderr was piped");
    /* Keep draining the server's stderr in the background: an unread
     * pipe would eventually block the server, and the Vulkan build's
     * failure reasons (no device, no VRAM) only appear there. */
    let lines: Arc<Mutex<Vec<String>>> = Arc::default();
    let failed = Arc::new(AtomicBool::new(false));
    let fail_reason: Arc<Mutex<Option<String>>> = Arc::default();
    let reader_lines = Arc::clone(&lines);
    let reader_failed = Arc::clone(&failed);
    let reader_reason = Arc::clone(&fail_reason);
    let is_gpu = backend == Backend::Gpu;
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if is_gpu {
                let low = line.to_lowercase();
                if !reader_failed.load(Ordering::Relaxed)
                    && GPU_FAIL_HINTS.iter().any(|h| low.contains(h))
                {
                    *reader_reason.lock().expect("fail log") = Some(line.clone());
                    reader_failed.store(true, Ordering::Relaxed);
                }
            }
            reader_lines.lock().expect("stderr log").push(line);
        }
    });

    let started = Instant::now();
    loop {
        if failed.load(Ordering::Relaxed) {
            let _ = child.kill();
            let reason = fail_reason
                .lock()
                .expect("fail log")
                .clone()
                .unwrap_or_else(|| "vulkan unavailable".into());
            return Err(reason);
        }
        if let Ok(Some(_)) = child.try_wait() {
            let _ = child.kill(); /* reap the exited child */
            let reason = lines
                .lock()
                .expect("stderr log")
                .last()
                .cloned()
                .unwrap_or_else(|| "no stderr output".into());
            return Err(format!("exited before becoming healthy: {reason}"));
        }
        if server_ready() {
            return Ok(device_name(&lines));
        }
        if started.elapsed() > backend.startup_timeout() {
            let _ = child.kill();
            return Err(format!("{} did not become healthy in time", backend.exe()));
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Best-effort GPU name from the startup log, e.g. a line like
/// "ggml_vulkan: 0 = NVIDIA GeForce MX350 (NVIDIA) | ..." yields
/// "NVIDIA GeForce MX350".
fn device_name(lines: &Mutex<Vec<String>>) -> Option<String> {
    let v = lines.lock().ok()?;
    v.iter().find_map(|line| {
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
fn stream_persona(prompt: &str, max_tokens: i64) -> Result<(), Box<dyn std::error::Error>> {
    let messages = [
        json!({"role": "system", "content": SYSTEM_PROMPT}),
        json!({"role": "user", "content": prompt}),
    ];
    print!("ai:- ");
    std::io::stdout().flush()?;
    let mut on_tok = |tok: &str| -> bool {
        print!("{tok}");
        let _ = std::io::stdout().flush();
        true
    };
    let (_reply, _first_ms) = chat_stream(&messages, max_tokens, &mut on_tok)?;
    println!();
    Ok(())
}

/// `chat` subcommand: one-shot with arguments, or an interactive loop when
/// called without arguments. Replies stream token-by-token.
pub fn cmd_chat(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut force_cpu = false;
    let mut rest: Vec<&str> = Vec::new();
    for a in args {
        if a == "--cpu" {
            force_cpu = true;
        } else {
            rest.push(a.as_str());
        }
    }
    let prompt = rest.join(" ");
    ensure_server(force_cpu)?;

    if !prompt.is_empty() {
        return stream_persona(&prompt, 512);
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
        if let Err(e) = stream_persona(q, 512) {
            println!("  error: {e}");
        }
    }
    Ok(())
}
