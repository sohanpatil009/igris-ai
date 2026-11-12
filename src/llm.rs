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
use std::io::{BufRead, Read, Write};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/* Vulkan (GPU) build of llama.cpp, installed in models/llm-gpu. Swap this
 * back to "tools/llama-cpp/llama-server.exe" and drop --swa-full for pure
 * CPU. NOTE: if the flags below change, kill any leftover llama-server so
 * the next run starts one with the new configuration. */
const LLAMA_SERVER_EXE: &str = "models/llm-gpu/llama-server.exe";
const LLM_MODEL: &str = "models/llm/LFM3-1.7B-Q4_K_M.gguf";
const PORT: u16 = 8081;
const CTX_TOKENS: i64 = 8000;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(300);
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

/// Start the bundled llama-server if none is running, then wait until the
/// model is fully loaded (health endpoint returns 200).
pub fn ensure_server() -> Result<(), Box<dyn std::error::Error>> {
    if server_ready() {
        return Ok(());
    }
    if !std::path::Path::new(LLAMA_SERVER_EXE).exists() {
        return Err(format!(
            "llama-server not found at {LLAMA_SERVER_EXE} (run from the repo root)"
        )
        .into());
    }
    Command::new(LLAMA_SERVER_EXE)
        .arg("-m")
        .arg(LLM_MODEL)
        .arg("-c")
        .arg(CTX_TOKENS.to_string())
        .arg("-ngl")
        .arg("99") /* offload every layer to the GPU (Vulkan backend) */
        .arg("-fa")
        .arg("on") /* flash attention: ~2x decode speed on this GPU */
        /* LFM3 mixes sliding-window attention layers; the default SWA
         * cache rejects KV-shift reuse ("cache_reuse is not supported by
         * this context"). --swa-full stores the full window and unlocks
         * prompt-prefix reuse. */
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
        .stderr(Stdio::null())
        .spawn()?;
    /* The child outlives this handle on purpose; std does not kill it on
     * drop, so the server keeps serving for the next run of the app. */
    let started = Instant::now();
    loop {
        if server_ready() {
            return Ok(());
        }
        if started.elapsed() > STARTUP_TIMEOUT {
            return Err("llama-server did not become healthy in time".into());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
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
    ensure_server()?;
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
    let prompt = args.join(" ");
    ensure_server()?;

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
