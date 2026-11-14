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
//! v1.1 batch 2 (2026-11-14) — system state:
//! - `sys_stats`  — CPU / RAM / system-drive / battery / uptime snapshot
//!   (read-only; CPU is sampled over ~150 ms inside the call)
//! - `set_volume` — master output volume 0-100 through the audio endpoint
//!   COM API (`IAudioEndpointVolume`), read back after setting
//! - `screenshot` — primary-screen capture to a PNG (GDI `BitBlt` into a
//!   32-bpp DIB section), optional viewer launch via `ShellExecuteW`
//!
//! v1.1 `tools.toml` (2026-11-14) — config-defined tools:
//! `[[tool]]` entries in `%APPDATA%\igris\tools.toml` extend the registry
//! without recompiling. Each tenant has either a URL template (opened in
//! the default browser, http/https only, substituted values
//! percent-encoded) or an argv command template (executed directly via
//! `std::process::Command` — no shell, so argument values can never be
//! re-interpreted as syntax). `{placeholder}`s must map to properties the
//! tool's own schema declares. The file is the user's own trust domain,
//! but a broken file is reported on stderr and ignored — bad config
//! never bricks the assistant. Loaded once at CLI startup
//! ([`load_config_tools`]) and folded into the registry at first use;
//! `registry()` itself stays seed-only so tests that build a registry
//! remain hermetic no matter what sits in the user's config.
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
use regex::Regex;
use serde::Deserialize;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{FILETIME, HWND};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDeviceCaps, HGDIOBJ, HORZRES,
    ReleaseDC, SRCCOPY, SelectObject, VERTRES,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{
    GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::GetSystemTimes;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MEDIA_NEXT_TRACK, VK_MEDIA_PLAY_PAUSE,
    VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Per-tool execution budget (voice UX: never deadlock the loop).
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// One executable tenant of the registry.
#[derive(Clone)]
pub struct ToolDef {
    pub name: String,
    pub desc: String,
    pub schema: Value,
    pub run: Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>,
}

/* Manual Debug: the `run` closure is an opaque Arc<dyn Fn>, so a derived
 * impl is impossible; tests use unwrap_err()/unwrap() on tool vectors. */
impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("desc", &self.desc)
            .field("schema", &self.schema)
            .field("run", &"<closure>")
            .finish()
    }
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
        let args = unwrap_schema_confused_args(&def.schema, &args.clone());
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
    r.register(ToolDef {
        name: "sys_stats".to_string(),
        desc: "Get a system status snapshot: CPU usage, RAM usage, free disk space on the \
              system drive, battery level and power source, and uptime. Read-only."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        run: Arc::new(run_sys_stats),
    });
    r.register(ToolDef {
        name: "set_volume".to_string(),
        desc: "Set the system output volume to a percentage (0 = muted, 100 = full). \
              Applies to the default playback device."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "volume": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 100,
                    "description": "Target volume as a percentage (0 = muted, 100 = full)"
                }
            },
            "required": ["volume"]
        }),
        run: Arc::new(run_set_volume),
    });
    r.register(ToolDef {
        name: "screenshot".to_string(),
        desc: "Capture the screen (primary monitor) to a PNG file. By default saves to \
              the Pictures folder with a timestamped name; pass a path to save elsewhere, \
              and open=true to show the image in the default viewer."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Optional file path for the PNG (supports ~ for the \
                        user home); defaults to the Pictures folder"
                },
                "open": {
                    "type": "boolean",
                    "description": "Open the saved image in the default viewer (default false)"
                }
            },
            "required": []
        }),
        run: Arc::new(run_screenshot),
    });
    r
}

/// The process-wide registry, seeded once on first use. Both prompt surfaces
/// (the voice history seed in assist.rs and `chat` in llm.rs) and the Phase 4
/// turn loop share this instance, so the specs advertised in the prompt and
/// the executable tools can never drift apart. Config-defined tools staged
/// by `load_config_tools()` before this first use are folded in here
/// (tests never call the loader, so test registries stay hermetic).
pub fn registry() -> &'static Registry {
    static REG: LazyLock<Registry> = LazyLock::new(|| {
        let mut r = seed_registry();
        if let Some(defs) = CONFIG_TOOLS.get() {
            for def in defs {
                r.register(def.clone());
            }
        }
        r
    });
    &REG
}

/// LFM3 sometimes emits the DSML attr-form with the schema name instead of
/// the argument name — `parameter=properties` (or `parameter=parameters`)
/// with the intended arguments object as the body. The parser faithfully
/// keys that body under "properties", so the real arguments end up one
/// level deep: `{"properties": {"volume": 40}}`. When the tool's schema
/// declares no parameter by that name, the wrapper is a formatting
/// artifact: strip one level and dispatch the intended arguments. The
/// honest-error alternative costs the model a recovery round it often
/// cannot find — e2e evidence (2026-11-14): `set_volume` burned 5 rounds
/// on the same confused shape and gave up, while a scalar-string-arg tool
/// (`media_key`) does self-heal in one round. Double-dispatch is not a
/// risk: dedup is per-response and this runs inside a single dispatch.
fn unwrap_schema_confused_args(schema: &Value, args: &Value) -> Value {
    const WRAPPER_KEYS: [&str; 2] = ["properties", "parameters"];
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    if map.len() != 1 {
        return args.clone();
    }
    let (key, value) = map.iter().next().expect("len checked above");
    if !WRAPPER_KEYS.contains(&key.as_str()) || !value.is_object() {
        return args.clone();
    }
    /* A schema that genuinely declares the key keeps it. */
    let declared = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|p| p.contains_key(key));
    if declared != Some(false) {
        return args.clone();
    }
    value.clone()
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

/// ShellExecuteW "open" on a target string (a URL for the default
/// browser, or a file path for its default handler). Shell semantics:
/// instance values <= 32 mean failure.
fn shell_open(target: &str) -> Result<(), String> {
    let verb = to_wide("open");
    let file = to_wide(target);
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
    if (hinst.0 as isize) <= 32 {
        return Err(format!("ShellExecuteW failed for '{target}'"));
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
    shell_open(&url)?;
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

/* ================================================================ */
/* sys_stats — CPU / RAM / disk / battery / uptime snapshot           */
/* ================================================================ */

fn run_sys_stats(_args: &Value) -> Result<String, String> {
    let cpu = cpu_percent();
    let ram = ram_summary()?;
    let disk = disk_summary()?;
    let power = power_summary()?;
    Ok(format!(
        "CPU: {cpu}% | RAM: {ram} | Disk: {disk} | {power} | Uptime: {}",
        uptime_string()
    ))
}

fn filetime_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

/// One (idle, kernel, user) counter snapshot. On Windows the kernel time
/// already includes idle time, so busy = kernel + user - idle.
fn system_times() -> Result<(u64, u64, u64), String> {
    let (mut idle, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default());
    /* SAFETY: three out-pointers to our own locals. */
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }
        .map_err(|e| format!("sys_stats: GetSystemTimes failed: {e}"))?;
    Ok((
        filetime_u64(&idle),
        filetime_u64(&kernel),
        filetime_u64(&user),
    ))
}

/// CPU usage sampled over ~150 ms inside this call (two counter reads).
fn cpu_percent() -> u32 {
    let Ok(t1) = system_times() else {
        return 0;
    };
    std::thread::sleep(Duration::from_millis(150));
    let Ok(t2) = system_times() else {
        return 0;
    };
    /* wrapping_sub: counters never reset while we hold the machine, but a
     * debug-mode panic on u64 underflow is a worse failure than a wrong % */
    let idle = t2.0.wrapping_sub(t1.0);
    let kernel = t2.1.wrapping_sub(t1.1);
    let user = t2.2.wrapping_sub(t1.2);
    let total = kernel + user;
    if total == 0 {
        return 0;
    }
    let busy = total.saturating_sub(idle);
    ((busy as f64) * 100.0 / (total as f64)).round() as u32
}

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn ram_summary() -> Result<String, String> {
    let mut mem = MEMORYSTATUSEX::default();
    mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    /* SAFETY: out-pointer to our own struct, dwLength already set. */
    unsafe { GlobalMemoryStatusEx(&mut mem) }
        .map_err(|e| format!("sys_stats: GlobalMemoryStatusEx failed: {e}"))?;
    let used = gib(mem.ullTotalPhys - mem.ullAvailPhys);
    Ok(format!(
        "{} GB used of {} GB ({}%)",
        used,
        gib(mem.ullTotalPhys),
        mem.dwMemoryLoad
    ))
}

fn disk_summary() -> Result<String, String> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let dir = format!("{drive}\\");
    let wide = to_wide(&dir);
    let (mut free, mut total) = (0u64, 0u64);
    /* SAFETY: NUL-terminated wide string built above; two out u64s. */
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), Some(&mut total), None) }
        .map_err(|e| format!("sys_stats: GetDiskFreeSpaceExW failed: {e}"))?;
    if total == 0 {
        return Ok(format!("{drive} unknown"));
    }
    let used_pct = (((total - free) as f64) * 100.0 / (total as f64)).round() as u32;
    Ok(format!(
        "{drive} {} GB free of {} GB ({used_pct}% used)",
        gib(free),
        gib(total)
    ))
}

fn power_summary() -> Result<String, String> {
    let mut status = SYSTEM_POWER_STATUS::default();
    /* SAFETY: out-pointer to our own struct. */
    unsafe { GetSystemPowerStatus(&mut status) }
        .map_err(|e| format!("sys_stats: GetSystemPowerStatus failed: {e}"))?;
    const BATTERY_FLAG_NO_BATTERY: u8 = 128;
    if status.BatteryFlag & BATTERY_FLAG_NO_BATTERY != 0 {
        return Ok("Battery: none (desktop)".to_string());
    }
    let ac = match status.ACLineStatus {
        0 => "on battery",
        1 => "on AC power",
        _ => "power source unknown",
    };
    if status.BatteryLifePercent == 255 {
        return Ok(format!("Battery: unknown ({ac})"));
    }
    Ok(format!("Battery: {}% ({ac})", status.BatteryLifePercent))
}

fn uptime_string() -> String {
    /* SAFETY: parameterless counter read. */
    let secs = unsafe { GetTickCount64() } / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let minutes = rem / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes:02}m")
    } else {
        format!("{hours}h {minutes:02}m")
    }
}

/* ================================================================ */
/* set_volume — COM IAudioEndpointVolume absolute set + read-back     */
/* ================================================================ */

fn run_set_volume(args: &Value) -> Result<String, String> {
    let volume = extract_i64(args, "volume", "set_volume")?;
    if !(0..=100).contains(&volume) {
        return Err(format!("volume must be 0-100, got {volume}"));
    }
    let achieved = set_master_volume(volume as u32)?;
    Ok(format!("System volume set to {achieved}%."))
}

/// Set the master output volume through the default audio endpoint and
/// read back the achieved level. Runs on the dispatch worker thread, so
/// COM is initialized for this thread only and balanced below.
fn set_master_volume(percent: u32) -> Result<u32, String> {
    /* SAFETY: thread-local COM initialization; on a fresh dispatch worker
     * this can only return S_OK/S_FALSE, both of which must be balanced
     * by CoUninitialize, which is exactly what happens at the end. */
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(format!("set_volume: COM initialization failed: {hr}"));
    }
    let result = (|| -> Result<u32, String> {
        let endpoint = audio_endpoint()?;
        let target = percent as f32 / 100.0;
        /* SAFETY: null event-context GUID; endpoint is alive through the
         * call and dropped by the closure scope afterwards. */
        unsafe { endpoint.SetMasterVolumeLevelScalar(target, std::ptr::null()) }
            .map_err(|e| format!("set_volume: SetMasterVolumeLevelScalar failed: {e}"))?;
        let achieved = unsafe { endpoint.GetMasterVolumeLevelScalar() }
            .map_err(|e| format!("set_volume: GetMasterVolumeLevelScalar failed: {e}"))?;
        Ok((achieved * 100.0).round() as u32)
    })();
    /* SAFETY: balances the successful CoInitializeEx above. */
    unsafe { CoUninitialize() };
    result
}

fn audio_endpoint() -> Result<IAudioEndpointVolume, String> {
    /* SAFETY: CoCreateInstance of the standard MMDeviceEnumerator class. */
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("set_volume: COM instantiation failed: {e}"))?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
        .map_err(|e| format!("set_volume: no default audio device: {e}"))?;
    let endpoint: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| format!("set_volume: endpoint activation failed: {e}"))?;
    Ok(endpoint)
}

/* ================================================================ */
/* screenshot — GDI capture of the primary screen into a PNG          */
/* ================================================================ */

fn run_screenshot(args: &Value) -> Result<String, String> {
    let path: PathBuf = match args.get("path") {
        None | Some(Value::Null) => default_screenshot_path()?,
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Err("screenshot: path must not be empty".to_string());
            }
            expand_home_path(s).into()
        }
        Some(_) => return Err("screenshot: 'path' must be a string".to_string()),
    };
    let open = match args.get("open") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return Err("screenshot: 'open' must be a boolean".to_string()),
    };
    if path.is_dir() {
        return Err(format!(
            "screenshot: path points to an existing folder: {}",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "screenshot: cannot create folder {}: {e}",
                    parent.display()
                )
            })?;
        }
    }
    let (w, h, rgb) = capture_screen_rgb()?;
    image::save_buffer(&path, &rgb, w, h, image::ColorType::Rgb8)
        .map_err(|e| format!("screenshot: saving {} failed: {e}", path.display()))?;
    let mut msg = format!("Screenshot saved to {} ({}x{}).", path.display(), w, h);
    if open {
        if let Err(e) = launch_default_viewer(&path) {
            msg.push_str(&format!(" (could not open viewer: {e})"));
        }
    }
    Ok(msg)
}

fn default_screenshot_path() -> Result<PathBuf, String> {
    let base = dirs::picture_dir().unwrap_or_else(std::env::temp_dir);
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    Ok(base.join(format!("igris_screenshot_{stamp}.png")))
}

fn launch_default_viewer(path: &Path) -> Result<(), String> {
    shell_open(&path.as_os_str().to_string_lossy())
}

/// Capture the primary monitor as RGB bytes (top-down, row-major).
fn capture_screen_rgb() -> Result<(u32, u32, Vec<u8>), String> {
    /* SAFETY: standard GDI screen capture. Every handle taken below is
     * released on every path before returning. */
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return Err("screenshot: GetDC failed".to_string());
        }
        let w = GetDeviceCaps(Some(hdc), HORZRES);
        let h = GetDeviceCaps(Some(hdc), VERTRES);
        if w <= 0 || h <= 0 {
            ReleaseDC(None, hdc);
            return Err("screenshot: screen dimensions unavailable".to_string());
        }
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, /* negative: rows come top-down */
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = match CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bmp) => bmp,
            Err(e) => {
                ReleaseDC(None, hdc);
                return Err(format!("screenshot: CreateDIBSection failed: {e}"));
            }
        };
        let mem = CreateCompatibleDC(Some(hdc));
        if mem.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            ReleaseDC(None, hdc);
            return Err("screenshot: CreateCompatibleDC failed".to_string());
        }
        let old = SelectObject(mem, HGDIOBJ(hbmp.0));
        let blt = BitBlt(mem, 0, 0, w, h, Some(hdc), 0, 0, SRCCOPY).is_ok();
        let len = (w as usize) * (h as usize) * 4;
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        if blt {
            let px = std::slice::from_raw_parts(bits as *const u8, len);
            for quad in px.chunks_exact(4) {
                rgb.push(quad[2]); /* R (buffer is BGRA) */
                rgb.push(quad[1]); /* G */
                rgb.push(quad[0]); /* B */
            }
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, hdc);
        if !blt {
            return Err("screenshot: BitBlt failed".to_string());
        }
        Ok((w as u32, h as u32, rgb))
    }
}

/* ================================================================ */
/* tools.toml — config-defined tools (dynamism ladder rung)          */
/* ================================================================ */

#[derive(Deserialize)]
struct ToolsFile {
    #[serde(default)]
    tool: Vec<ConfigToolEntry>,
}

#[derive(Deserialize)]
struct ConfigToolEntry {
    name: String,
    description: String,
    /* JSON text of the tool's parameter schema; omitted = no params. */
    schema: Option<String>,
    url: Option<String>,
    command: Option<Vec<String>>,
}

/// `{name}` placeholders — lowercase slug identifiers, so they can only
/// ever reference schema properties (which are declared with the same
/// slug rule), never arbitrary expressions.
static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([a-z][a-z0-9_]*)\}").expect("static regex"));

/// Config-defined tools staged by `load_config_tools()` before the first
/// `registry()` use; the registry folds them in at seed time. Tests never
/// call the loader, so the test registry stays hermetic.
static CONFIG_TOOLS: OnceLock<Vec<ToolDef>> = OnceLock::new();

/// Config file location: `%APPDATA%\igris\tools.toml`.
pub fn config_tools_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("igris").join("tools.toml"))
}

/// Read + validate the user's `tools.toml` and stage the tools for
/// [`registry`]. Call once at CLI startup, before any registry use —
/// later calls cannot affect an already-built registry. A missing file
/// is normal (0 tools); an invalid file is reported on stderr and
/// ignored: bad config must never brick the assistant.
pub fn load_config_tools() -> usize {
    let Some(path) = config_tools_path() else {
        return 0;
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return 0;
    };
    match parse_config_tools(&text) {
        Ok(defs) => {
            let n = defs.len();
            let _ = CONFIG_TOOLS.set(defs);
            n
        }
        Err(e) => {
            eprintln!("tools: ignoring {}: {e}", path.display());
            0
        }
    }
}

/// Parse + validate a `tools.toml` text into registrable tenants. Pure
/// function so tests can exercise it without touching the filesystem.
fn parse_config_tools(text: &str) -> Result<Vec<ToolDef>, String> {
    let file: ToolsFile = toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?;
    let mut defs = Vec::new();
    for (i, entry) in file.tool.iter().enumerate() {
        let label = format!("tool #{}", i + 1);
        let name = entry.name.trim();
        if name.is_empty() {
            return Err(format!("{label}: 'name' is empty"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "{label}: name '{name}' must be a slug: lowercase letters, digits, underscores"
            ));
        }
        if entry.description.trim().is_empty() {
            return Err(format!("{label} ({name}): 'description' is empty"));
        }
        if (entry.url.is_some()) == (entry.command.is_some()) {
            return Err(format!("{label} ({name}): define exactly one of 'url' or 'command'"));
        }
        let schema_text = entry
            .schema
            .as_deref()
            .unwrap_or("{\"type\":\"object\",\"properties\":{},\"required\":[]}");
        let schema: Value = serde_json::from_str(schema_text)
            .map_err(|e| format!("{label} ({name}): 'schema' is not valid JSON: {e}"))?;
        if !schema.is_object() {
            return Err(format!("{label} ({name}): 'schema' must be a JSON object"));
        }
        let props = schema.get("properties").cloned().unwrap_or_else(|| json!({}));
        if !props.is_object() {
            return Err(format!("{label} ({name}): 'schema.properties' must be an object"));
        }
        if let Some(url) = &entry.url {
            validate_url_template(url, &props).map_err(|e| format!("{label} ({name}): {e}"))?;
            let url = url.clone();
            defs.push(ToolDef {
                name: name.to_string(),
                desc: entry.description.trim().to_string(),
                schema,
                run: Arc::new(move |args: &Value| run_config_url(&url, args)),
            });
        } else if let Some(cmd) = &entry.command {
            validate_command_template(cmd, &props).map_err(|e| format!("{label} ({name}): {e}"))?;
            let cmd = cmd.clone();
            defs.push(ToolDef {
                name: name.to_string(),
                desc: entry.description.trim().to_string(),
                schema,
                run: Arc::new(move |args: &Value| run_config_command(&cmd, args)),
            });
        }
    }
    Ok(defs)
}

fn validate_url_template(template: &str, props: &Value) -> Result<(), String> {
    let t = template.trim();
    if !(t.starts_with("http://") || t.starts_with("https://")) {
        return Err("url template must start with http:// or https://".to_string());
    }
    for ph in placeholders_in(t) {
        if props.get(ph.as_str()).is_none() {
            return Err(format!(
                "url placeholder '{ph}' is not declared in the schema properties"
            ));
        }
    }
    Ok(())
}

fn validate_command_template(cmd: &[String], props: &Value) -> Result<(), String> {
    if cmd.is_empty() {
        return Err("command template must have at least a program".to_string());
    }
    if cmd[0].trim().is_empty() {
        return Err("command program is empty".to_string());
    }
    for part in cmd {
        for ph in placeholders_in(part) {
            if props.get(ph.as_str()).is_none() {
                return Err(format!(
                    "command placeholder '{ph}' is not declared in the schema properties"
                ));
            }
        }
    }
    Ok(())
}

fn placeholders_in(template: &str) -> Vec<String> {
    PLACEHOLDER_RE
        .find_iter(template)
        .map(|m| m.as_str()[1..m.len() - 1].to_string())
        .collect()
}

/// Argument value as a template string: strings verbatim, numbers and
/// bools in their JSON form. An argument the template needs but the
/// model didn't supply is an honest error — better than a mangled URL
/// or a half-formed argv.
fn arg_string(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(Value::Bool(b)) => Ok(b.to_string()),
        _ => Err(format!("Missing required parameter: {key}")),
    }
}

/// Percent-encode bytes that would break a URL. Values are restricted
/// to RFC 3986 unreserved characters — separators like '&' or '='
/// inside a VALUE must be encoded so they cannot split the query
/// string; template punctuation outside the values is never touched.
/// (Everything the encoder removes is also rejected by
/// `validate_open_url`, so the final URL is always browser-safe.)
fn url_encode_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for byte in s.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Substitute `{placeholder}`s from the call arguments. `encode` picks
/// URL-percent-encoding (templates) vs verbatim text (argv entries).
fn expand_template(template: &str, args: &Value, encode: bool) -> Result<String, String> {
    let mut out = String::new();
    let mut last = 0usize;
    for m in PLACEHOLDER_RE.find_iter(template) {
        out.push_str(&template[last..m.start()]);
        let key = &template[m.start() + 1..m.end() - 1];
        let raw = arg_string(args, key)?;
        let piece = if encode {
            url_encode_value(&raw)
        } else {
            raw
        };
        out.push_str(&piece);
        last = m.end();
    }
    out.push_str(&template[last..]);
    Ok(out)
}

fn run_config_url(template: &str, args: &Value) -> Result<String, String> {
    let url = expand_template(template, args, true)?;
    validate_open_url(&url)?;
    shell_open(&url)?;
    Ok(format!("Opened {url}"))
}

fn run_config_command(template: &[String], args: &Value) -> Result<String, String> {
    let mut argv = Vec::with_capacity(template.len());
    for part in template {
        argv.push(expand_template(part, args, false)?);
    }
    /* Direct argv spawn, fire-and-forget (same policy as open_app):
     * values are separate arguments, never shell syntax. */
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .map_err(|e| format!("Could not run '{}': {e}", argv[0]))?;
    Ok(format!("Ran {}.", argv[0]))
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
                "read_file",
                "screenshot",
                "set_volume",
                "sys_stats"
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

    /* ---- sys_stats ---- */
    /* Read-only queries on the live OS, so a full smoke run is hermetic. */

    #[test]
    fn sys_stats_reports_all_sections() {
        let out = run_sys_stats(&json!({})).expect("sys_stats succeeds");
        assert!(out.contains("CPU:") && out.contains("%"), "{out}");
        assert!(out.contains("RAM:") && out.contains("GB"), "{out}");
        assert!(out.contains("Disk:") && out.contains("GB"), "{out}");
        assert!(out.contains("Battery:"), "{out}");
        assert!(out.contains("Uptime:") && out.contains("m"), "{out}");
    }

    /* ---- set_volume ---- */
    /* Validation only in unit tests — real COM calls change the user's
     * actual output volume, so the set path is e2e territory. */

    #[test]
    fn set_volume_rejects_out_of_range_and_bad_args() {
        assert!(run_set_volume(&json!({"volume": 101})).unwrap_err().contains("0-100"));
        assert!(run_set_volume(&json!({"volume": -1})).unwrap_err().contains("0-100"));
        assert!(run_set_volume(&json!({})).unwrap_err().contains("set_volume"));
        assert!(run_set_volume(&json!({"volume": "loud"}))
            .unwrap_err()
            .contains("set_volume"));
    }

    /* ---- screenshot ---- */

    #[test]
    fn screenshot_rejects_bad_args_before_capturing() {
        assert!(run_screenshot(&json!({"path": "   "})).unwrap_err().contains("path"));
        assert!(run_screenshot(&json!({"path": 42})).unwrap_err().contains("path"));
        assert!(run_screenshot(&json!({"open": "yes"})).unwrap_err().contains("open"));
    }

    #[test]
    fn screenshot_smoke_saves_a_real_png() {
        let dir = temp_root("screenshot");
        let path = dir.join("shot.png");
        let out = run_screenshot(&json!({"path": path.to_string_lossy()})).expect("screenshot");
        assert!(out.contains("Screenshot saved"), "{out}");
        let meta = fs::metadata(&path).expect("png exists on disk");
        assert!(meta.len() > 1_000, "png suspiciously small: {}", meta.len());
        use std::io::Read;
        let mut head = [0u8; 4];
        fs::File::open(&path)
            .unwrap()
            .read_exact(&mut head)
            .expect("readable");
        assert_eq!(&head, &[0x89, b'P', b'N', b'G'], "not a PNG signature");
        fs::remove_dir_all(&dir).ok();
    }

    /* ---- schema-confused wrapper unwrapping (dispatch-time repair) ---- */

    #[test]
    fn confused_schema_wrapper_args_are_unwrapped() {
        let schema = reg()
            .specs()
            .into_iter()
            .find(|s| s.name == "set_volume")
            .unwrap()
            .parameters_schema;
        /* The e2e-observed shape: properties wrapper around the real args. */
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"properties": {"volume": 40}})),
            json!({"volume": 40})
        );
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"parameters": {"volume": 5}})),
            json!({"volume": 5})
        );
        /* Clean calls pass through untouched. */
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"volume": 40})),
            json!({"volume": 40})
        );
        /* Extra keys or non-object wrapper values are not repairable. */
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"properties": {"volume": 40}, "x": 1})),
            json!({"properties": {"volume": 40}, "x": 1})
        );
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"properties": 3})),
            json!({"properties": 3})
        );
        /* A tool that legitimately declares the key keeps it. */
        let declared = json!({
            "type": "object",
            "properties": {"properties": {"type": "string"}},
            "required": []
        });
        assert_eq!(
            unwrap_schema_confused_args(&declared, &json!({"properties": {"a": 1}})),
            json!({"properties": {"a": 1}})
        );
        /* Schema without a properties field is left alone (conservative). */
        assert_eq!(
            unwrap_schema_confused_args(&json!({"type": "object"}), &json!({"properties": {}})),
            json!({"properties": {}})
        );
    }

    #[test]
    fn dispatch_unwraps_confused_wrapper_before_running() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "probe".to_string(),
            desc: "probe".to_string(),
            schema: json!({
                "type": "object",
                "properties": {"value": {"type": "integer"}},
                "required": ["value"]
            }),
            run: Arc::new(|args: &Value| -> Result<String, String> {
                Ok(format!(
                    "value={}",
                    args.get("value").and_then(|v| v.as_i64()).unwrap_or(-1)
                ))
            }),
        });
        let out = r.dispatch("probe", &json!({"properties": {"value": 7}})).unwrap();
        assert_eq!(out, "value=7");
    }

    /* ---- tools.toml config-defined tools ---- */

    const PLAY_SONG_TOML: &str = r#"
[[tool]]
name = "play_song"
description = "Search YouTube for a song and open the results page"
schema = '{"type":"object","properties":{"query":{"type":"string","description":"Song or artist to search"}},"required":["query"]}'
url = "https://www.youtube.com/results?search_query={query}"
"#;

    const NOTE_CMD_TOML: &str = r#"
[[tool]]
name = "show_note"
description = "Show a note in notepad"
schema = '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}'
command = ["notepad.exe", "{text}"]
"#;

    #[test]
    fn config_parse_accepts_url_and_command_templates() {
        let defs = parse_config_tools(PLAY_SONG_TOML).expect("url tool parses");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "play_song");
        assert!(defs[0].schema["properties"]["query"].is_object());
        let defs = parse_config_tools(NOTE_CMD_TOML).expect("command tool parses");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "show_note");
    }

    #[test]
    fn config_parse_empty_and_comment_only_files_are_zero_tools() {
        assert_eq!(parse_config_tools("").unwrap().len(), 0);
        assert_eq!(parse_config_tools("# just a comment\n").unwrap().len(), 0);
    }

    #[test]
    fn config_parse_rejects_bad_names_and_missing_descriptions() {
        let bad_name = PLAY_SONG_TOML.replace("play_song", "Play-Song");
        assert!(parse_config_tools(&bad_name).unwrap_err().contains("slug"));
        let empty_name = PLAY_SONG_TOML.replace("play_song", "");
        assert!(parse_config_tools(&empty_name).unwrap_err().contains("'name' is empty"));
        let no_desc = PLAY_SONG_TOML.replace(
            "description = \"Search YouTube for a song and open the results page\"",
            "description = \"\"",
        );
        assert!(parse_config_tools(&no_desc).unwrap_err().contains("'description' is empty"));
    }

    #[test]
    fn config_parse_requires_exactly_one_of_url_or_command() {
        let both = format!("{PLAY_SONG_TOML}command = [\"notepad.exe\"]\n");
        assert!(parse_config_tools(&both).unwrap_err().contains("exactly one"));
        let neither = PLAY_SONG_TOML.replace(
            "url = \"https://www.youtube.com/results?search_query={query}\"\n",
            "",
        );
        assert!(parse_config_tools(&neither).unwrap_err().contains("exactly one"));
    }

    #[test]
    fn config_parse_rejects_unknown_placeholders_and_bad_schemes() {
        let undeclared = PLAY_SONG_TOML.replace("{query}", "{track}");
        let err = parse_config_tools(&undeclared).unwrap_err();
        assert!(err.contains("'track' is not declared"), "{err}");
        let bad_scheme = PLAY_SONG_TOML.replace(
            "https://www.youtube.com/results?search_query={query}",
            "file://C:/Windows/{query}",
        );
        assert!(parse_config_tools(&bad_scheme).unwrap_err().contains("http"));
        let undeclared_cmd = NOTE_CMD_TOML.replace("{text}", "{note}");
        assert!(parse_config_tools(&undeclared_cmd)
            .unwrap_err()
            .contains("'note' is not declared"));
    }

    #[test]
    fn config_parse_rejects_invalid_schemas() {
        let bad_json = PLAY_SONG_TOML.replace(
            "'{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\",\"description\":\"Song or artist to search\"}},\"required\":[\"query\"]}'",
            "'{\"type\": \"object\",'",
        );
        assert!(parse_config_tools(&bad_json).unwrap_err().contains("valid JSON"));
        let non_object = PLAY_SONG_TOML.replace(
            "'{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\",\"description\":\"Song or artist to search\"}},\"required\":[\"query\"]}'",
            "'[1, 2]'",
        );
        assert!(parse_config_tools(&non_object)
            .unwrap_err()
            .contains("JSON object"));
    }

    #[test]
    fn config_url_expansion_encodes_values_but_keeps_template() {
        let args = json!({"query": "shape of you"});
        let url = expand_template(
            "https://www.youtube.com/results?search_query={query}",
            &args,
            true,
        )
        .unwrap();
        assert_eq!(
            url,
            "https://www.youtube.com/results?search_query=shape%20of%20you"
        );
        /* '&' inside a VALUE must not split the query string. */
        let args = json!({"query": "dance & move"});
        let url = expand_template(
            "https://x.example/search?q={query}&safe=true",
            &args,
            true,
        )
        .unwrap();
        assert_eq!(url, "https://x.example/search?q=dance%20%26%20move&safe=true");
        /* A missing argument is an honest error, not a mangled URL. */
        assert!(expand_template(
            "https://www.youtube.com/results?search_query={query}",
            &json!({}),
            true
        )
        .unwrap_err()
        .contains("query"));
    }

    #[test]
    fn config_command_expansion_is_verbatim_argv() {
        let args = json!({"text": "a b \"quoted\""});
        let expanded = expand_template("{text}", &args, false).unwrap();
        assert_eq!(expanded, "a b \"quoted\"");
    }

    #[test]
    fn placeholders_ignore_non_slug_braces() {
        assert_eq!(
            placeholders_in("keep {Bad} {good_2} {x and {unclosed}"),
            vec!["good_2".to_string(), "unclosed".to_string()]
        );
    }

    #[test]
    fn config_tools_override_builtins_and_extend_the_registry() {
        let mut r = seed_registry();
        for def in parse_config_tools(PLAY_SONG_TOML).unwrap() {
            r.register(def);
        }
        let names: Vec<String> = r.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"play_song".to_string()));
        assert_eq!(names.len(), 12); /* 11 built-ins + 1 config tenant */
        /* register() replaces same-name tenants: an override of get_time. */
        let defs = parse_config_tools(
            r#"
[[tool]]
name = "get_time"
description = "Config override of get_time"
url = "https://time.example/{q}"
schema = '{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}'
"#,
        )
        .unwrap();
        r.register(defs.into_iter().next().unwrap());
        let get_time = r.specs().into_iter().find(|s| s.name == "get_time").unwrap();
        assert!(get_time.description.contains("Config override"));
    }
}
