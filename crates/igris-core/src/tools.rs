//! Dynamic tool registry — the runtime list of tools the LLM may call.
//!
//! The registry is the product; individual tools are its tenants (plan
//! §3.3). Tenants are registered in code by [`seed_registry`]:
//!
//! v1 (2026-11-13):
//! - `get_time`  — current local date/time (read-only)
//! - `calculate` — arithmetic, ported from ZeroClaw's
//!   `zeroclaw-tools/src/calculator.rs` (v1 keeps its 13 most
//!   voice-relevant functions of upstream's 25; extend the enum + match
//!   arm when a use case needs more)
//! - `open_app`  — launch one of a hard allowlist of desktop apps through
//!   `std::process::Command`; no shell, no arbitrary paths (gated)
//!
//! v1.1 batch 1 (2026-11-14) — system & files:
//! - `file_search`   — case-insensitive file-name search under the common
//!   user folders (or an explicit root); read-only, depth/entry/time-capped
//! - `read_file`     — read a text file, truncated to a small output
//!   budget (read-only)
//! - `create_folder` — mkdir -p equivalent under an explicit path
//! - `open_url`      — open http/https URLs in the default browser via
//!   `ShellExecuteW` (scheme allowlist, no shell)
//! - `media_key`     — global media key press (play/next/prev/stop) via
//!   `keybd_event`; hits whatever app owns the media session
//!
//! Dynamism ladder: v1 code-registered → v1.1 `tools.toml` config-defined
//! tools (name/desc/schema + allowlisted command template, no recompile —
//! `register` replaces a same-name tenant to make overrides possible) → v3
//! MCP/WASM servers (ZeroClaw's `zeroclaw-plugins` pattern).
//!
//! Security policy (voice use): no arbitrary shell execution (ZeroClaw's
//! `shell` tool is deliberately not ported); the `open_app` allowlist is a
//! fixed table; `open_url` accepts only http/https so protocol handlers
//! cannot be abused to launch local programs; the file tools are read-only
//! apart from `create_folder` (file writing waits for v2.5's gated
//! actions); every dispatch runs on a worker thread with a timeout, so a
//! hung tool returns an error result instead of deadlocking
//! the voice loop; dispatches are sequential by construction (the caller
//! runs them one at a time between turns).
//!
//! Results are plain `Result<String, String>`: the string is what the model
//! sees inside the tool_result envelope when Phase 4 wires the loop.

use crate::llm::ToolSpec;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE,
    VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Per-tool execution budget (voice UX: never deadlock the loop).
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// One executable tenant of the registry.
pub struct ToolDef {
    pub name: String,
    pub desc: String,
    pub schema: Value,
    pub run: Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>,
}

/// Runtime registry, not a hardcoded table: tools can be added, replaced
/// and (in later ladder steps) loaded from configuration.
#[derive(Default)]
pub struct Registry {
    tools: Vec<ToolDef>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tenant. Re-registering an existing name replaces it (this is
    /// what later lets config-defined tools override built-ins).
    pub fn register(&mut self, tool: ToolDef) {
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name == tool.name) {
            *slot = tool;
        } else {
            self.tools.push(tool);
        }
    }

    /// Prompt-facing view: sorted by name, so the system prompt block is
    /// byte-stable for the llama-server KV cache.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name.clone(),
                description: t.desc.clone(),
                parameters_schema: t.schema.clone(),
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Run a tool and wait for its result (sequential, voice-context).
    pub fn dispatch(&self, name: &str, args: &Value) -> Result<String, String> {
        self.dispatch_with_timeout(name, args, TOOL_TIMEOUT)
    }

    /// Run a tool on a worker thread with a hard budget. Unknown names and
    /// hung/crashed tools are errors, never panics.
    pub fn dispatch_with_timeout(
        &self,
        name: &str,
        args: &Value,
        timeout: Duration,
    ) -> Result<String, String> {
        let Some(def) = self.tools.iter().find(|t| t.name == name) else {
            return Err(format!("unknown tool: {name}"));
        };
        let run = Arc::clone(&def.run);
        let args = args.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            /* A panic inside a tool drops the sender on unwind; the
             * receiver maps that to an error instead of dying. */
            let _ = tx.send(run(&args));
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("tool '{name}' timed out after {timeout:?}"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("tool '{name}' crashed"))
            }
        }
    }
}

/// v1 seed tenants (open decisions 2/3 of 2026-11-13). Later steps swap in
/// the dynamism ladder without touching the call sites.
pub fn seed_registry() -> Registry {
    let mut r = Registry::new();
    r.register(ToolDef {
        name: "calculate".to_string(),
        desc: "Perform arithmetic calculations: add, subtract, multiply, divide, pow, \
              sqrt, abs, modulo, round, sum, average, min, max. Use this tool whenever \
              you need to compute a numeric result instead of guessing."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "function": {
                    "type": "string",
                    "description": "Calculation to perform: add(values), subtract(values), \
                        divide(values), multiply(values), pow(a,b), sqrt(x), abs(x), \
                        modulo(a,b), round(x,decimals), sum(values), average(values), \
                        min(values), max(values)",
                    "enum": [
                        "add", "subtract", "multiply", "divide", "pow", "sqrt", "abs",
                        "modulo", "round", "sum", "average", "min", "max"
                    ]
                },
                "values": {
                    "type": "array",
                    "items": {"type": "number"},
                    "description": "Array of numeric values. Required for: add, subtract, \
                        divide, multiply, sum, average, min, max."
                },
                "a": {"type": "number", "description": "First operand. Required for: pow, modulo."},
                "b": {"type": "number", "description": "Second operand. Required for: pow, modulo."},
                "x": {"type": "number", "description": "Input number. Required for: sqrt, abs, round."},
                "decimals": {"type": "integer", "description": "Decimal places. Required for: round."}
            },
            "required": ["function"]
        }),
        run: Arc::new(run_calculate),
    });
    r.register(ToolDef {
        name: "get_time".to_string(),
        desc: "Get the current local date and time.".to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        run: Arc::new(run_get_time),
    });
    r.register(ToolDef {
        name: "open_app".to_string(),
        desc: "Open one of the allowed desktop applications (notepad, calc, chrome, \
              spotify, vscode)."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "app": {
                    "type": "string",
                    "enum": ["notepad", "calc", "chrome", "spotify", "vscode"],
                    "description": "Which app to open"
                }
            },
            "required": ["app"]
        }),
        run: Arc::new(run_open_app),
    });
    r.register(ToolDef {
        name: "file_search".to_string(),
        desc: "Search for files whose name contains the query text (case-insensitive). \
              Searches the common user folders (Desktop, Documents, Downloads, Pictures, \
              Music, Videos) or an explicit root directory. Returns matching file paths."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text to look for inside file names (case-insensitive)"
                },
                "root": {
                    "type": "string",
                    "description": "Optional absolute directory to search under instead of \
                        the common user folders"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum number of matches to return (default 20, max 100)"
                }
            },
            "required": ["query"]
        }),
        run: Arc::new(run_file_search),
    });
    r.register(ToolDef {
        name: "read_file".to_string(),
        desc: "Read a text file from disk and return its contents (truncated). Use \
              file_search first if the path is unknown. Supports ~ for the user home."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path (absolute, or relative to the working directory)"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum characters to return (default 2000, max 8000)"
                }
            },
            "required": ["path"]
        }),
        run: Arc::new(run_read_file),
    });
    r.register(ToolDef {
        name: "create_folder".to_string(),
        desc: "Create a new folder, creating missing parent folders too (like mkdir -p). \
              Does not overwrite existing files; creating an existing folder is a no-op."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Folder path to create (absolute or ~ based)"
                }
            },
            "required": ["path"]
        }),
        run: Arc::new(run_create_folder),
    });
    r.register(ToolDef {
        name: "open_url".to_string(),
        desc: "Open a web URL (http or https) in the default browser.".to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to open, e.g. https://example.com"
                }
            },
            "required": ["url"]
        }),
        run: Arc::new(run_open_url),
    });
    r.register(ToolDef {
        name: "media_key".to_string(),
        desc: "Press a global media key: play_pause, next_track, previous_track, or stop. \
              Controls whatever media session is active (Spotify, browser video, music \
              apps) — it does not target a specific app."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "key": {
                    "type": "string",
                    "enum": ["play_pause", "next_track", "previous_track", "stop"],
                    "description": "Which media key to press"
                }
            },
            "required": ["key"]
        }),
        run: Arc::new(run_media_key),
    });
    r
}

/// The process-wide registry, seeded once on first use. Both prompt surfaces
/// (the voice history seed in assist.rs and `chat` in llm.rs) and the Phase 4
/// turn loop share this instance, so the specs advertised in the prompt and
/// the executable tools can never drift apart.
pub fn registry() -> &'static Registry {
    static REG: LazyLock<Registry> = LazyLock::new(seed_registry);
    &REG
}

/* ================================================================ */
/* get_time                                                          */
/* ================================================================ */

/// Format stays stable so tests (and the model, next turn) can parse it.
fn run_get_time(_args: &Value) -> Result<String, String> {
    let now = chrono::Local::now();
    Ok(format!(
        "{} on {}",
        now.format("%Y-%m-%d %H:%M:%S"),
        now.format("%A")
    ))
}

/* ================================================================ */
/* calculate — ported from zeroclaw-tools/src/calculator.rs          */
/* (v1 subset; helpers and formatting kept 1:1 with upstream)        */
/* ================================================================ */

const MAX_ROUND_DECIMALS: i64 = 15;
const MAX_VALUES_LEN: usize = 10_000;

fn run_calculate(args: &Value) -> Result<String, String> {
    let function = args
        .get("function")
        .and_then(|v| v.as_str())
        .ok_or("Missing required parameter: function")?;
    match function {
        "add" => calc_add(args),
        "subtract" => calc_subtract(args),
        "multiply" => calc_multiply(args),
        "divide" => calc_divide(args),
        "pow" => calc_pow(args),
        "sqrt" => calc_sqrt(args),
        "abs" => calc_abs(args),
        "modulo" => calc_modulo(args),
        "round" => calc_round(args),
        "sum" => calc_sum(args),
        "average" => calc_average(args),
        "min" => calc_min(args),
        "max" => calc_max(args),
        other => Err(format!("Unknown function: {other}")),
    }
}

fn extract_f64(args: &Value, key: &str, name: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("Missing required parameter: {name}"))
}

fn extract_i64(args: &Value, key: &str, name: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("Missing required parameter: {name}"))
}

fn extract_values(args: &Value, min_len: usize) -> Result<Vec<f64>, String> {
    let values = args
        .get("values")
        .and_then(|v| v.as_array())
        .ok_or("Missing required parameter: values (array of numbers)")?;
    if values.len() < min_len {
        return Err(format!(
            "Expected at least {min_len} value(s), got {}",
            values.len()
        ));
    }
    if values.len() > MAX_VALUES_LEN {
        return Err(format!(
            "values array must be at most {MAX_VALUES_LEN} entries, got {}",
            values.len()
        ));
    }
    let mut nums = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        match v.as_f64() {
            Some(n) => nums.push(n),
            None => return Err(format!("values[{i}] is not a valid number")),
        }
    }
    Ok(nums)
}

/// Integer-shaped floats print without a decimal tail (upstream parity).
fn format_num(n: f64) -> String {
    if n == n.floor() && n.abs() < 1e15 {
        let rounded = n.round() as i128;
        format!("{rounded}")
    } else {
        format!("{n}")
    }
}

fn calc_add(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    Ok(format_num(values.iter().sum()))
}

fn calc_subtract(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut iter = values.iter();
    let mut result = iter
        .next()
        .copied()
        .ok_or("Subtraction requires at least one value")?;
    for v in iter {
        result -= v;
    }
    Ok(format_num(result))
}

fn calc_multiply(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut result = 1.0;
    for v in &values {
        result *= v;
    }
    Ok(format_num(result))
}

fn calc_divide(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut iter = values.iter();
    let mut result = iter
        .next()
        .copied()
        .ok_or("Division requires at least one value")?;
    for v in iter {
        if *v == 0.0 {
            return Err("Division by zero".to_string());
        }
        result /= v;
    }
    Ok(format_num(result))
}

fn calc_pow(args: &Value) -> Result<String, String> {
    let base = extract_f64(args, "a", "a (base)")?;
    let exp = extract_f64(args, "b", "b (exponent)")?;
    Ok(format_num(base.powf(exp)))
}

fn calc_sqrt(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    if x < 0.0 {
        return Err("Cannot compute square root of a negative number".to_string());
    }
    Ok(format_num(x.sqrt()))
}

fn calc_abs(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    Ok(format_num(x.abs()))
}

fn calc_modulo(args: &Value) -> Result<String, String> {
    let a = extract_f64(args, "a", "a")?;
    let b = extract_f64(args, "b", "b")?;
    if b == 0.0 {
        return Err("Modulo by zero".to_string());
    }
    Ok(format_num(a % b))
}

fn calc_round(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    let decimals = extract_i64(args, "decimals", "decimals")?;
    if !(0..=MAX_ROUND_DECIMALS).contains(&decimals) {
        return Err(format!(
            "decimals must be between 0 and {MAX_ROUND_DECIMALS}"
        ));
    }
    let multiplier = 10_f64.powi(decimals as i32);
    Ok(format_num((x * multiplier).round() / multiplier))
}

fn calc_sum(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    Ok(format_num(values.iter().sum()))
}

fn calc_average(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    Ok(format_num(values.iter().sum::<f64>() / values.len() as f64))
}

fn calc_min(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    let Some(min_val) = values.iter().copied().reduce(f64::min) else {
        return Err("Cannot compute min of an empty array".to_string());
    };
    Ok(format_num(min_val))
}

fn calc_max(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    let Some(max_val) = values.iter().copied().reduce(f64::max) else {
        return Err("Cannot compute max of an empty array".to_string());
    };
    Ok(format_num(max_val))
}

/* ================================================================ */
/* open_app — hard allowlist, no shell, no arbitrary paths           */
/* ================================================================ */

const OPEN_APP_HINT: &str = "Allowed apps: notepad, calc, chrome, spotify, vscode";

/// Candidate executables for an allowed app, tried in order (PATH name
/// first, then well-known install locations). Returns None for any name
/// outside the allowlist — that rejection happens before any spawn.
fn app_candidates(app: &str) -> Option<Vec<String>> {
    let path_names: &[&str] = match app {
        "notepad" => &["notepad"],
        "calc" => &["calc"],
        "vscode" => &["code"],
        "chrome" => &["chrome"],
        "spotify" => &["spotify"],
        _ => return None,
    };
    let mut candidates: Vec<String> = path_names.iter().map(|s| s.to_string()).collect();
    /* Well-known install locations for apps that are not on PATH. These
     * are fixed templates, resolved at use time — never caller-supplied. */
    let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let program_files_x86 =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    match app {
        "chrome" => {
            candidates.push(
                std::path::PathBuf::from(&program_files)
                    .join(r"Google\Chrome\Application\chrome.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
            candidates.push(
                std::path::PathBuf::from(&program_files_x86)
                    .join(r"Google\Chrome\Application\chrome.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        "spotify" => {
            if let Ok(appdata) = std::env::var("APPDATA") {
                candidates.push(
                    std::path::PathBuf::from(appdata)
                        .join(r"Spotify\Spotify.exe")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        _ => {}
    }
    Some(candidates)
}

/// Spawns the GUI process detached (the Child handle is dropped, the app
/// keeps running). Note for tests: no unit test dispatches an allowed
/// name — actually launching an app is Phase 5 e2e territory.
fn run_open_app(args: &Value) -> Result<String, String> {
    let app = args
        .get("app")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing required parameter: app. {OPEN_APP_HINT}"))?;
    let Some(candidates) = app_candidates(app) else {
        return Err(format!("'{app}' is not an allowed app. {OPEN_APP_HINT}"));
    };
    let mut last_err = String::new();
    for candidate in candidates {
        match std::process::Command::new(&candidate).spawn() {
            Ok(_child) => return Ok(format!("Launched {app}.")),
            Err(e) => last_err = format!("{candidate}: {e}"),
        }
    }
    Err(format!("Could not launch {app}. {last_err}"))
}

/* ================================================================ */
/* shared path helpers (v1.1 file tools)                             */
/* ================================================================ */

/// The user profile directory ("~"). Falls back to "." so the file tools
/// still behave on exotic setups instead of panicking.
fn home_dir_string() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())
}

/// Expand a leading `~` (or `~/`, `~\`) to the user profile directory.
/// Windows APIs accept `/` separators, so the rest is left untouched.
fn expand_home_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "~" {
        return home_dir_string();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return format!("{}\\{}", home_dir_string(), rest);
    }
    trimmed.to_string()
}

/// Pull a required string argument out of the tool-call JSON.
fn required_str<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing required parameter: {key} (tool {tool})"))
}

/// Pull an optional integer argument with a clamped fallback.
fn optional_u64(args: &Value, key: &str, default: u64, lo: u64, hi: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default)
        .clamp(lo, hi)
}

/* ================================================================ */
/* file_search — bounded name-substring walk                         */
/* ================================================================ */

/// Self-limit ~0.5 s below the 5 s dispatch timeout: partial results beat
/// a timeout error for voice UX.
const SEARCH_TIME_BUDGET: Duration = Duration::from_millis(3_500);
const SEARCH_MAX_ENTRIES: usize = 30_000;
const SEARCH_MAX_DEPTH: usize = 8;
const SEARCH_DEFAULT_LIMIT: u64 = 20;
const SEARCH_MAX_LIMIT: u64 = 100;

/// Directory names never descended into (lowercased). Dependency/build/
/// system trees carry no user documents and would blow the time budget.
const SEARCH_SKIP_DIRS: &[&str] = &[
    "$recycle.bin",
    "appdata",
    "node_modules",
    "system volume information",
    "target",
    "vendor",
    "venv",
    "__pycache__",
];

/// Default roots when the caller gives none: the user's content folders.
/// Missing ones are skipped silently.
fn default_search_roots() -> Vec<PathBuf> {
    let home = home_dir_string();
    ["Desktop", "Documents", "Downloads", "Pictures", "Music", "Videos"]
        .iter()
        .map(|d| Path::new(&home).join(d))
        .filter(|p| p.is_dir())
        .collect()
}

/// Iterative bounded walk. Returns (matches, entries scanned, budget hit).
fn search_walk(roots: &[PathBuf], query_lc: &str, limit: usize) -> (Vec<PathBuf>, usize, bool) {
    let deadline = Instant::now() + SEARCH_TIME_BUDGET;
    let mut matches = Vec::new();
    let mut scanned = 0usize;
    let mut budget_hit = false;
    'roots: for root in roots {
        let mut stack = vec![(root.clone(), 0usize)];
        while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                scanned += 1;
                if Instant::now() >= deadline || scanned >= SEARCH_MAX_ENTRIES {
                    budget_hit = true;
                    break 'roots;
                }
                let path = entry.path();
                let name_lc = entry.file_name().to_string_lossy().to_lowercase();
                if path.is_dir() {
                    if depth < SEARCH_MAX_DEPTH
                        && !name_lc.starts_with('.')
                        && !SEARCH_SKIP_DIRS.contains(&name_lc.as_str())
                    {
                        stack.push((path, depth + 1));
                    }
                } else if name_lc.contains(query_lc) {
                    matches.push(path);
                    if matches.len() >= limit {
                        break 'roots;
                    }
                }
            }
        }
    }
    (matches, scanned, budget_hit)
}

fn run_file_search(args: &Value) -> Result<String, String> {
    let query = required_str(args, "query", "file_search")?;
    let query_lc = query.trim().to_lowercase();
    if query_lc.is_empty() {
        return Err("query is empty".into());
    }
    let limit = optional_u64(args, "limit", SEARCH_DEFAULT_LIMIT, 1, SEARCH_MAX_LIMIT) as usize;
    let roots: Vec<PathBuf> = match args.get("root").and_then(|v| v.as_str()) {
        Some(root) => {
            let expanded = PathBuf::from(expand_home_path(root));
            if !expanded.is_dir() {
                return Err(format!(
                    "root directory does not exist: {}",
                    expanded.display()
                ));
            }
            vec![expanded]
        }
        None => default_search_roots(),
    };
    if roots.is_empty() {
        return Err("no searchable user folders found on this machine".into());
    }
    let (matches, scanned, budget_hit) = search_walk(&roots, &query_lc, limit);
    let budget_note = if budget_hit {
        " (search budget reached; results may be partial)"
    } else {
        ""
    };
    let scope = if roots.len() == 1 {
        roots[0].display().to_string()
    } else {
        "the user's common folders".to_string()
    };
    if matches.is_empty() {
        return Ok(format!(
            "No files matching '{query}'{budget_note} after scanning {scanned} entries \
             under {scope}."
        ));
    }
    let mut out = format!(
        "Found {} file(s) matching '{query}'{budget_note} after scanning {scanned} entries \
         under {scope}:",
        matches.len()
    );
    for m in &matches {
        out.push_str(&format!("\n{}", m.display()));
    }
    Ok(out)
}

/* ================================================================ */
/* read_file / create_folder                                         */
/* ================================================================ */

/// Refuse to read anything bigger: keeps tool results and the 8k-token
/// context healthy (big files are for dedicated pipelines, not voice).
const READ_FILE_MAX_BYTES: u64 = 10 * 1024 * 1024;
const READ_FILE_DEFAULT_CHARS: u64 = 2_000;
const READ_FILE_MAX_CHARS: u64 = 8_000;

fn run_read_file(args: &Value) -> Result<String, String> {
    let raw = required_str(args, "path", "read_file")?;
    let path = PathBuf::from(expand_home_path(raw));
    let meta = match fs::metadata(&path) {
        Ok(m) => m,
        Err(e) => return Err(format!("Cannot read '{raw}': {e}")),
    };
    if meta.is_dir() {
        return Err(format!("'{raw}' is a directory, not a file"));
    }
    if meta.len() > READ_FILE_MAX_BYTES {
        return Err(format!(
            "'{raw}' is too large to read ({} bytes; limit {READ_FILE_MAX_BYTES})",
            meta.len()
        ));
    }
    let bytes = fs::read(&path).map_err(|e| format!("Cannot read '{raw}': {e}"))?;
    let max_chars = optional_u64(
        args,
        "max_chars",
        READ_FILE_DEFAULT_CHARS,
        1,
        READ_FILE_MAX_CHARS,
    ) as usize;
    let text = String::from_utf8_lossy(&bytes);
    /* Binary sniff: a text file decodes clean; a binary one is full of
     * replacement characters. Refuse those instead of wasting the model's
     * context on RIFF/PE garbage (discovered in e2e: a .wav "read"). */
    let total_chars = text.chars().count();
    let bad_chars = text.chars().filter(|c| *c == '\u{FFFD}').count();
    if total_chars > 0 && bad_chars * 10 > total_chars {
        return Err(format!(
            "'{raw}' looks like a binary file (not readable text)"
        ));
    }
    if total_chars <= max_chars {
        return Ok(text.into_owned());
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(&format!(
        "\n[truncated: showing first {max_chars} of {total_chars} characters]"
    ));
    Ok(out)
}

fn run_create_folder(args: &Value) -> Result<String, String> {
    let raw = required_str(args, "path", "create_folder")?;
    if raw.trim().is_empty() {
        return Err("path is empty".into());
    }
    let path = PathBuf::from(expand_home_path(raw));
    if path.is_dir() {
        return Ok(format!("Folder already exists: {}", path.display()));
    }
    if path.is_file() {
        return Err(format!("'{raw}' exists as a file, cannot create a folder there"));
    }
    fs::create_dir_all(&path)
        .map_err(|e| format!("Could not create '{}': {e}", path.display()))?;
    Ok(format!("Created folder: {}", path.display()))
}

/* ================================================================ */
/* open_url — ShellExecuteW, http/https scheme allowlist             */
/* ================================================================ */

/// Only web URLs: prevents protocol-handler abuse (e.g. `file://` or an
/// installed-app scheme launching a local program) — the browser is the
/// only permitted target.
fn validate_open_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err(format!(
            "Only http:// and https:// URLs are allowed (got '{url}')"
        ));
    }
    if u.len() < 8 {
        return Err("URL is too short".into());
    }
    for c in u.chars() {
        if c.is_control() || matches!(c, ' ' | '"' | '<' | '>' | '\'' | '\\') {
            return Err(format!("URL contains the invalid character '{c}'"));
        }
    }
    Ok(())
}

/// UTF-16 with a trailing NUL, for `PCWSTR` arguments.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn run_open_url(args: &Value) -> Result<String, String> {
    let url = required_str(args, "url", "open_url")?;
    validate_open_url(url)?;
    let url = url.trim().to_string();
    let verb = to_wide("open");
    let file = to_wide(&url);
    /* SAFETY: ShellExecuteW with NUL-terminated buffers we built above;
     * nothing borrowed outlives the call. */
    let hinst = unsafe {
        ShellExecuteW(
            Some(HWND::default()),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    /* Shell semantics: instance values <= 32 mean failure. */
    if (hinst.0 as isize) <= 32 {
        return Err(format!("Could not open {url} (ShellExecuteW failed)"));
    }
    Ok(format!("Opened {url} in the default browser."))
}

/* ================================================================ */
/* media_key — global keybd_event of four fixed VK_MEDIA_* keys      */
/* ================================================================ */

/// Map the tool's key name to its virtual-key code. Only these four
/// exist — no free-form key injection.
fn media_vk(key: &str) -> Option<u8> {
    match key {
        "play_pause" => Some(VK_MEDIA_PLAY_PAUSE.0 as u8),
        "next_track" => Some(VK_MEDIA_NEXT_TRACK.0 as u8),
        "previous_track" => Some(VK_MEDIA_PREV_TRACK.0 as u8),
        "stop" => Some(VK_MEDIA_STOP.0 as u8),
        _ => None,
    }
}

fn run_media_key(args: &Value) -> Result<String, String> {
    let key = required_str(args, "key", "media_key")?;
    let Some(vk) = media_vk(key) else {
        return Err(format!(
            "Unknown media key '{key}'. Allowed: play_pause, next_track, previous_track, stop"
        ));
    };
    /* SAFETY: a standard global synthetic keypress (down + up) of a fixed
     * VK_MEDIA_* code; no other input state is touched. */
    unsafe {
        keybd_event(vk, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
    }
    Ok(format!("Pressed media key: {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    /// Unique-name counter for test scratch dirs (process id already
    /// separates concurrent test processes).
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn reg() -> Registry {
        seed_registry()
    }

    /// Fresh scratch directory under the system temp dir; callers clean
    /// up with `fs::remove_dir_all` (errors ignored on purpose).
    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "igris_tools_{}_{}_{}_{}",
            tag,
            std::process::id(),
            nanos,
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /* ---- registry mechanics ---- */

    #[test]
    fn specs_are_sorted_by_name() {
        let names: Vec<String> = reg().specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "calculate",
                "create_folder",
                "file_search",
                "get_time",
                "media_key",
                "open_app",
                "open_url",
                "read_file"
            ]
        );
    }

    #[test]
    fn specs_carry_nonempty_desc_and_object_schema() {
        for spec in reg().specs() {
            assert!(!spec.description.is_empty(), "{}", spec.name);
            assert!(
                spec.parameters_schema.is_object(),
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn unknown_tool_dispatch_is_an_error_not_a_panic() {
        let err = reg().dispatch("no_such_tool", &json!({})).unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");
    }

    #[test]
    fn register_replaces_same_name_and_keeps_others() {
        let mut r = reg();
        let before = r.tools.len();
        r.register(ToolDef {
            name: "get_time".to_string(),
            desc: "replacement".to_string(),
            schema: json!({"type": "object"}),
            run: Arc::new(|_a| Ok("replaced".to_string())),
        });
        assert_eq!(r.tools.len(), before, "replacement must not duplicate");
        assert_eq!(r.dispatch("get_time", &json!({})).unwrap(), "replaced");
    }

    /* ---- timeout / crash isolation ---- */

    #[test]
    fn hung_tool_returns_error_within_budget() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "slow".to_string(),
            desc: "test tenant".to_string(),
            schema: json!({"type": "object"}),
            run: Arc::new(|_a| {
                std::thread::sleep(Duration::from_secs(30));
                Ok("never".to_string())
            }),
        });
        let t0 = Instant::now();
        let err = r
            .dispatch_with_timeout("slow", &json!({}), Duration::from_millis(150))
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(5), "must not wait 30s");
    }

    #[test]
    fn panicking_tool_becomes_an_error_result() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "boom".to_string(),
            desc: "test tenant".to_string(),
            schema: json!({"type": "object"}),
            run: Arc::new(|_a| panic!("intentional test panic")),
        });
        let err = r
            .dispatch_with_timeout("boom", &json!({}), Duration::from_secs(2))
            .unwrap_err();
        assert!(err.contains("crashed"), "{err}");
    }

    /* ---- get_time ---- */

    #[test]
    fn get_time_output_is_stable_and_parseable() {
        let out = reg().dispatch("get_time", &json!({})).unwrap();
        let re = regex::Regex::new(
            r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} on (Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday)$",
        )
        .unwrap();
        assert!(re.is_match(&out), "unexpected get_time output: {out}");
    }

    #[test]
    fn get_time_ignores_extra_args() {
        assert!(reg().dispatch("get_time", &json!({"x": 1})).is_ok());
    }

    /* ---- calculate ---- */

    #[test]
    fn calculate_arithmetic_core() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "add", "values": [2.0, 2.0]}))
                .unwrap(),
            "4"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "add", "values": [1.0, 2.0, 3.5]})
            )
            .unwrap(),
            "6.5"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "subtract", "values": [10.0, 3.0, 1.5]})
            )
            .unwrap(),
            "5.5"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "multiply", "values": [3.0, 4.0, 5.0]})
            )
            .unwrap(),
            "60"
        );
    }

    #[test]
    fn calculate_division_and_div_by_zero() {
        let r = reg();
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "divide", "values": [100.0, 4.0]})
            )
            .unwrap(),
            "25"
        );
        let err = r
            .dispatch(
                "calculate",
                &json!({"function": "divide", "values": [10.0, 0.0]}),
            )
            .unwrap_err();
        assert!(err.contains("zero"), "{err}");
    }

    #[test]
    fn calculate_pow_sqrt_modulo_round() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "pow", "a": 2.0, "b": 10.0}))
                .unwrap(),
            "1024"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "sqrt", "x": 144.0}))
                .unwrap(),
            "12"
        );
        assert!(r
            .dispatch("calculate", &json!({"function": "sqrt", "x": -4.0}))
            .is_err());
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "modulo", "a": 17.0, "b": 5.0}))
                .unwrap(),
            "2"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "round", "x": 2.715, "decimals": 2})
            )
            .unwrap(),
            "2.72"
        );
    }

    #[test]
    fn calculate_aggregates_and_unknown_function() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "sum", "values": [1.0, 2.0, 3.0]}))
                .unwrap(),
            "6"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "average", "values": [10.0, 20.0, 30.0]})
            )
            .unwrap(),
            "20"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "min", "values": [5.0, 1.0, 8.0]}))
                .unwrap(),
            "1"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "max", "values": [5.0, 1.0, 8.0]}))
                .unwrap(),
            "8"
        );
        let err = r
            .dispatch(
                "calculate",
                &json!({"function": "variance", "values": [1.0, 2.0]}),
            )
            .unwrap_err();
        assert!(err.contains("Unknown function"), "{err}");
    }

    #[test]
    fn calculate_garbage_input_is_error_not_panic() {
        let r = reg();
        assert!(r.dispatch("calculate", &json!({})).is_err());
        assert!(r.dispatch("calculate", &json!({"function": "add"})).is_err());
        assert!(r
            .dispatch(
                "calculate",
                &json!({"function": "add", "values": ["a", "b"]})
            )
            .is_err());
        assert!(r
            .dispatch("calculate", &json!({"function": "add", "values": [1.0]}))
            .is_err());
        assert!(r.dispatch("calculate", &json!({"function": "sqrt"})).is_err());
    }

    /* ---- open_app ---- */
    /* No unit test dispatches an allowed name: that would really launch a
     * GUI app. Real launches are Phase 5 e2e. */

    #[test]
    fn open_app_rejects_non_allowlisted_names_before_any_spawn() {
        let err = reg()
            .dispatch("open_app", &json!({"app": "terminal"}))
            .unwrap_err();
        assert!(err.contains("not an allowed app"), "{err}");
    }

    #[test]
    fn open_app_empty_or_missing_args_are_errors() {
        let r = reg();
        assert!(r.dispatch("open_app", &json!({})).is_err());
        assert!(r.dispatch("open_app", &json!({"app": 5})).is_err());
    }

    #[test]
    fn open_app_allowlist_covers_the_decision_3_set() {
        for app in ["notepad", "calc", "chrome", "spotify", "vscode"] {
            assert!(app_candidates(app).is_some(), "{app} should be allowed");
        }
        assert!(app_candidates("word").is_none());
    }

    #[test]
    fn open_app_spotify_candidates_include_the_appdata_install_path() {
        let cands = app_candidates("spotify").unwrap();
        assert!(cands.iter().any(|c| c.ends_with("Spotify.exe")));
    }

    /* ---- shared path helpers ---- */

    #[test]
    fn expand_home_path_expands_tilde_only() {
        let home = home_dir_string();
        assert_eq!(expand_home_path("~"), home);
        assert_eq!(expand_home_path("~/Documents"), format!("{}\\Documents", home));
        assert_eq!(expand_home_path("~\\notes"), format!("{}\\notes", home));
        assert_eq!(expand_home_path("C:\\plain\\path"), "C:\\plain\\path");
        assert_eq!(expand_home_path(" relative/x "), "relative/x");
    }

    /* ---- file_search ---- */

    #[test]
    fn file_search_finds_files_case_insensitively() {
        let root = temp_root("fs_hit");
        fs::write(root.join("Quarterly Report.TXT"), "x").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("q-report-old.txt"), "x").unwrap();
        fs::write(root.join("notes.md"), "x").unwrap();
        let out = reg()
            .dispatch(
                "file_search",
                &json!({"query": "REPORT", "root": root.to_string_lossy()}),
            )
            .unwrap();
        assert!(out.contains("Found 2"), "{out}");
        assert!(out.contains("Quarterly Report.TXT"), "{out}");
        assert!(out.contains("q-report-old.txt"), "{out}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn file_search_respects_limit_and_reports_count() {
        let root = temp_root("fs_limit");
        for i in 0..5 {
            fs::write(root.join(format!("song_{i}.mp3")), "x").unwrap();
        }
        let out = reg()
            .dispatch(
                "file_search",
                &json!({"query": "song_", "limit": 2, "root": root.to_string_lossy()}),
            )
            .unwrap();
        assert!(out.contains("Found 2"), "{out}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn file_search_no_match_is_a_clean_message() {
        let root = temp_root("fs_none");
        fs::write(root.join("hello.txt"), "x").unwrap();
        let out = reg()
            .dispatch(
                "file_search",
                &json!({"query": "zzz-nothing", "root": root.to_string_lossy()}),
            )
            .unwrap();
        assert!(out.contains("No files matching"), "{out}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn file_search_skips_hidden_and_listed_dirs() {
        let root = temp_root("fs_skip");
        fs::create_dir_all(root.join(".hiddendir")).unwrap();
        fs::create_dir_all(root.join("AppData")).unwrap();
        fs::create_dir_all(root.join("visible")).unwrap();
        fs::write(root.join(".hiddendir").join("keep.txt"), "x").unwrap();
        fs::write(root.join("AppData").join("keep.txt"), "x").unwrap();
        fs::write(root.join("visible").join("keep.txt"), "x").unwrap();
        let out = reg()
            .dispatch(
                "file_search",
                &json!({"query": "keep", "root": root.to_string_lossy()}),
            )
            .unwrap();
        let visible_hit = out.contains("visible\\keep.txt") || out.contains("visible/keep.txt");
        assert!(visible_hit, "{out}");
        assert!(
            !out.contains("AppData\\keep.txt") && !out.contains("AppData/keep.txt"),
            "{out}"
        );
        assert!(!out.contains(".hiddendir"), "{out}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn file_search_errors_on_bad_args() {
        let r = reg();
        assert!(r.dispatch("file_search", &json!({})).is_err());
        assert!(r.dispatch("file_search", &json!({"query": "  "})).is_err());
        /* a root that exists but has no such subdir is a clean error */
        let base = temp_root("fs_badroot");
        let bad = base.join("no_such_subdir");
        assert!(r
            .dispatch(
                "file_search",
                &json!({"query": "x", "root": bad.to_string_lossy()})
            )
            .is_err());
        fs::remove_dir_all(base).ok();
    }

    /* ---- read_file ---- */

    #[test]
    fn read_file_roundtrip_and_errors() {
        let root = temp_root("rf");
        let f = root.join("hello.txt");
        fs::write(&f, "hello igris").unwrap();
        let out = reg()
            .dispatch("read_file", &json!({"path": f.to_string_lossy()}))
            .unwrap();
        assert_eq!(out, "hello igris");
        assert!(reg()
            .dispatch(
                "read_file",
                &json!({"path": root.join("nope.txt").to_string_lossy()})
            )
            .is_err());
        let dir_err = reg()
            .dispatch("read_file", &json!({"path": root.to_string_lossy()}))
            .unwrap_err();
        assert!(dir_err.contains("directory"), "{dir_err}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn read_file_truncates_with_notice() {
        let root = temp_root("rf_trunc");
        let f = root.join("big.txt");
        fs::write(&f, "x".repeat(3000)).unwrap();
        let out = reg()
            .dispatch(
                "read_file",
                &json!({"path": f.to_string_lossy(), "max_chars": 100}),
            )
            .unwrap();
        assert!(out.starts_with(&"x".repeat(100)), "{out}");
        assert!(
            out.contains("[truncated: showing first 100 of 3000 characters]"),
            "{out}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn read_file_refuses_oversized_files() {
        let root = temp_root("rf_big");
        let f = root.join("huge.bin");
        fs::write(&f, vec![0u8; READ_FILE_MAX_BYTES as usize + 1]).unwrap();
        let err = reg()
            .dispatch("read_file", &json!({"path": f.to_string_lossy()}))
            .unwrap_err();
        assert!(err.contains("too large"), "{err}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn read_file_sniffs_binary_instead_of_returning_garbage() {
        let root = temp_root("rf_bin");
        let f = root.join("blob.bin");
        /* RIFF-ish noise: mostly bytes that do not decode as UTF-8. */
        fs::write(&f, [0xFFu8, 0xFE, 0x00, 0x01, 0xD8, 0x07, 0x91]).unwrap();
        let err = reg()
            .dispatch("read_file", &json!({"path": f.to_string_lossy()}))
            .unwrap_err();
        assert!(err.contains("binary"), "{err}");
        fs::remove_dir_all(root).ok();
    }

    /* ---- create_folder ---- */

    #[test]
    fn create_folder_makes_nested_dirs_and_is_idempotent() {
        let root = temp_root("cf");
        let target = root.join("a/b/c");
        let out1 = reg()
            .dispatch("create_folder", &json!({"path": target.to_string_lossy()}))
            .unwrap();
        assert!(out1.contains("Created folder"), "{out1}");
        assert!(target.is_dir());
        let out2 = reg()
            .dispatch("create_folder", &json!({"path": target.to_string_lossy()}))
            .unwrap();
        assert!(out2.contains("already exists"), "{out2}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn create_folder_rejects_file_paths_and_bad_args() {
        let root = temp_root("cf_file");
        let f = root.join("occupied.txt");
        fs::write(&f, "x").unwrap();
        let err = reg()
            .dispatch("create_folder", &json!({"path": f.to_string_lossy()}))
            .unwrap_err();
        assert!(err.contains("exists as a file"), "{err}");
        assert!(reg().dispatch("create_folder", &json!({})).is_err());
        fs::remove_dir_all(root).ok();
    }

    /* ---- open_url ---- */
    /* Valid URLs are never dispatched in unit tests — that opens a real
     * browser tab. Only validation and arg errors are unit-tested; the
     * ShellExecuteW path is e2e territory. */

    #[test]
    fn open_url_validates_schemes_and_characters() {
        assert!(validate_open_url("https://example.com").is_ok());
        assert!(validate_open_url("http://localhost:8080/x?y=1&z=2").is_ok());
        for bad in [
            "ftp://example.com",
            "file:///C:/Windows/System32/calc.exe",
            "example.com",
            "ms-settings:display",
            "https://exa mple.com",
            "https://ex\"ample.com",
            "https://ex\nample.com",
            "",
        ] {
            assert!(validate_open_url(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn open_url_rejects_missing_args_without_launching() {
        assert!(reg().dispatch("open_url", &json!({})).is_err());
        assert!(reg().dispatch("open_url", &json!({"url": 5})).is_err());
    }

    /* ---- media_key ---- */
    /* Valid presses are never dispatched in unit tests — that toggles the
     * user's real media playback. Mapping + validation are unit-tested;
     * the actual keybd_event path is e2e territory. */

    #[test]
    fn media_key_mapping_covers_the_four_keys() {
        /* Official Winuser.h values (0xB0-0xB3); volume keys sit lower at
         * 0xAD-0xAF, so no name can ever map to a volume key. */
        assert_eq!(media_vk("next_track"), Some(0xB0));
        assert_eq!(media_vk("previous_track"), Some(0xB1));
        assert_eq!(media_vk("stop"), Some(0xB2));
        assert_eq!(media_vk("play_pause"), Some(0xB3));
        assert!(media_vk("volume_up").is_none());
        assert!(media_vk("").is_none());
    }

    #[test]
    fn media_key_rejects_unknown_and_missing_keys() {
        assert!(reg().dispatch("media_key", &json!({})).is_err());
        assert!(reg()
            .dispatch("media_key", &json!({"key": "volume_mute"}))
            .is_err());
    }
}
