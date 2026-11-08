// Offline local LLM helpers.
//
// Primary path: a direct GGUF model via `llama-cli` (lean, no daemon).
// Fallback: a local Ollama instance over HTTP.

use std::process::Command;

pub fn reason_local_once(system: &str, user: &str) -> Result<String, String> {
    // 1. Direct GGUF / llama.cpp path.
    let model = find_gguf_model();
    if let Some(model) = model {
        let bin = std::env::var("IGRIS_LLM_BIN").unwrap_or_else(|_| "llama-cli".to_string());
        let prompt = format!("{system}\nUser: {user}\nAssistant:");
        let args = [
            "-m",
            model.as_str(),
            "-p",
            prompt.as_str(),
            "-n",
            "200",
            "--simple-io",
            "--no-display-prompt",
        ];
        let out = Command::new(&bin)
            .args(args)
            .output()
            .map_err(|e| format!("failed to run '{bin}': {e}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr).to_string();
            // Fall through to Ollama if available.
            let _ = err;
        } else {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let reply = extract_llama_stdout(&stdout);
            if !reply.trim().is_empty() {
                return Ok(reply.trim().to_string());
            }
        }
    }

    // 2. Optional Ollama fallback (HTTP).
    if let Ok(host) = std::env::var("OLLAMA_HOST") {
        if !host.trim().is_empty() {
            return ollama_once(&host, system, user);
        }
    }
    Err("no local GGUF model configured and OLLAMA_HOST not set".to_string())
}

fn find_gguf_model() -> Option<String> {
    if let Ok(m) = std::env::var("IGRIS_GGUF_MODEL") {
        if !m.trim().is_empty() {
            return Some(m.trim().to_string());
        }
    }
    if let Ok(m) = std::env::var("IGRIS_GGUF") {
        if !m.trim().is_empty() {
            return Some(m.trim().to_string());
        }
    }
    // Common local default path.
    let candidate = std::path::PathBuf::from("models").join("local.gguf");
    if candidate.exists() {
        candidate.to_str().map(str::to_string)
    } else {
        None
    }
}

fn extract_llama_stdout(stdout: &str) -> String {
    // llama-cli emits the generated text as continuation; strip trailing token stats.
    let mut lines: Vec<&str> = stdout.lines().collect();
    // Drop lines that look like llama.cpp timing reports.
    lines.retain(|l| !l.trim_start().starts_with('[') && !l.trim_start().starts_with("main:"));
    let joined = lines.join(" ").trim().to_string();
    // Remove any prompt echo if present is not trivial; keep last chunk.
    if let Some(pos) = joined.rfind("Assistant:") {
        joined[pos + "Assistant:".len()..].trim().to_string()
    } else {
        joined
    }
}

fn ollama_once(host: &str, system: &str, user: &str) -> Result<String, String> {
    let model = std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3.2".to_string());
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let url = format!("{}/api/generate", host.trim_end_matches('/'));
    let payload = serde_json::json!({
        "model": model,
        "system": system,
        "prompt": user,
        "stream": false
    });
    let resp = client
        .post(&url)
        .json(&payload)
        .send()
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("ollama HTTP {}", resp.status()));
    }
    let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    Ok(v.get("response")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("no response")
        .trim()
        .to_string())
}
