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
//! v1.2 (2026-11-14) — persistent memory (goal.md §7): `memory_save`,
//! `memory_recall`, `create_note`, `activity_log`, `reminder_set` over the
//! file-backed store in [`crate::memory`] (hybrid design: our own sync
//! store + vendored mem0-rust similarity kernels — see that module's doc).
//! Everything lives in `%APPDATA%\igris\memory.json` and survives restart.
//!
//! v2 (2026-11-14) — dev & browser (goal.md §4/§5/§6):
//! - `run_command`     — curated program+subcommand allowlist (cargo dev
//!   subcommands + read-only git), no shell; bare `cargo run` becomes
//!   build-then-launch so the app stays alive after the reply
//! - `list_dir`        — bounded directory listing (folders first, sizes)
//! - `content_search`  — substring grep over text files with line numbers
//!   (breadth-first walk, binary sniff, skips dependency trees AND nested
//!   git clones, time/file budget)
//! - `web_fetch`       — size-capped page reader (HTML → readable text,
//!   JSON passthrough); loopback/private literal hosts refused
//! - `download_file`   — streamed save into Downloads only (the fixed
//!   writable path), 200 MB cap, HTML-page guard
//! - `web_search`      — DuckDuckGo "lite" HTML backend (no API key;
//!   user decision 2026-11-14), redirect unwrapping + entity decode
//!
//! Dev/network tools declare longer time budgets than the 5 s voice
//! default through [`tool_budget`] (one auditable table).
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
use crate::memory::{self, MemoryKind, ScoredMemory};
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

/// Default per-tool execution budget (voice UX: never deadlock the loop).
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Tools that legitimately outlive the 5 s voice budget. One table on
/// purpose: a single glance shows everything allowed to run long, and the
/// dispatch layer stays the only place that knows about budgets. Barge-in
/// still cuts the whole turn, so a slow build never traps the user.
fn tool_budget(name: &str) -> Duration {
    match name {
        // dev commands: a cold `cargo build`/`test` can take minutes
        "run_command" => Duration::from_secs(120),
        // streaming a large file over a slow link
        "download_file" => Duration::from_secs(120),
        // plain page loads / one search round-trip
        "web_fetch" => Duration::from_secs(25),
        "web_search" => Duration::from_secs(20),
        _ => TOOL_TIMEOUT,
    }
}

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
    /// The budget comes from [`tool_budget`]: 5 s for everything except
    /// the v2 dev/network tools that declare a longer one.
    pub fn dispatch(&self, name: &str, args: &Value) -> Result<String, String> {
        self.dispatch_with_timeout(name, args, tool_budget(name))
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
    r.register(ToolDef {
        name: "memory_save".to_string(),
        desc: "Persist a durable fact about the user for future sessions \
              (preferences, plans, people, important dates). Use this whenever \
              the user asks you to remember something or states a durable \
              personal fact worth keeping."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The fact as one short standalone sentence, \
                        e.g. 'My exam is on March 3rd'"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional 1-3 topical keywords, e.g. ['exam', 'college']"
                }
            },
            "required": ["content"]
        }),
        run: Arc::new(run_memory_save),
    });
    r.register(ToolDef {
        name: "memory_recall".to_string(),
        desc: "Search saved memories, notes, activity and reminders. Call this \
              tool for ANY question phrased like 'what is my ...', 'when is \
              my ...', 'do you remember ...', 'what are my reminders/notes', \
              or about the user's preferences, plans, dates, passwords, or \
              past requests. Never say you don't know without calling this \
              tool first — facts may have been saved in an earlier session."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to look for, e.g. 'exam date' or 'wifi password'"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional: restrict to records carrying any of these tags"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results to return (default 5)"
                }
            },
            "required": ["query"]
        }),
        run: Arc::new(run_memory_recall),
    });
    r.register(ToolDef {
        name: "create_note".to_string(),
        desc: "Create a quick note for the user (ideas, lists, drafts, things \
              worth keeping handy). Notes are searchable via memory_recall."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "The note text"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional topical keywords"
                }
            },
            "required": ["content"]
        }),
        run: Arc::new(run_create_note),
    });
    r.register(ToolDef {
        name: "activity_log".to_string(),
        desc: "Record that a notable action or event just happened (opened an \
              app, changed a setting, finished a task). Feeds 'what did you \
              do' questions, which are answered via memory_recall."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "description": "Short verb phrase, e.g. 'set volume to 40'"
                },
                "detail": {
                    "type": "string",
                    "description": "Optional extra context"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional topical keywords"
                }
            },
            "required": ["action"]
        }),
        run: Arc::new(run_activity_log),
    });
    r.register(ToolDef {
        name: "reminder_set".to_string(),
        desc: "Save a reminder with a due time so it can be surfaced later. \
              Use when the user asks to be reminded about something. The \
              reminder is stored persistently; memory_recall can find it."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "description": "What to remind the user about"
                },
                "due_at": {
                    "type": "string",
                    "description": "When it is due — ISO 8601 if possible, \
                        e.g. 2026-11-15T09:00:00, otherwise clear wording \
                        like 'tomorrow 9am'"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional topical keywords"
                }
            },
            "required": ["content", "due_at"]
        }),
        run: Arc::new(run_reminder_set),
    });
    /* v2 — dev & browser (goal.md §4/§5/§6) */
    r.register(ToolDef {
        name: "run_command".to_string(),
        desc: "Run a curated developer command in a project directory and return its \
              output. Allowed programs ONLY: cargo (subcommands check, build, test, run, \
              new, init, add, remove, fmt, clippy, tree) and git (read-only: status, \
              log, diff, show, rev-parse). Bare 'cargo run' builds and launches the app \
              in the background. Use for build/check/test/git requests like 'run cargo \
              check on my project' or 'what is the git status of X'."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "program": {
                    "type": "string",
                    "description": "'cargo' or 'git'"
                },
                "args": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Subcommand and arguments, e.g. ['check'] or \
                        ['status']. First item must be an allowed subcommand."
                },
                "working_dir": {
                    "type": "string",
                    "description": "Optional project directory to run in, e.g. 'E:\\my\\project'"
                }
            },
            "required": ["program", "args"]
        }),
        run: Arc::new(run_run_command),
    });
    r.register(ToolDef {
        name: "list_dir".to_string(),
        desc: "List the contents of a directory — folders first (trailing /), then \
              files with sizes (capped at 200 entries). Use for 'show me the files in \
              X', 'what is in my Downloads folder', 'show the project structure' \
              requests. path defaults to the user's home directory."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory to list; defaults to the user's home directory"
                }
            }
        }),
        run: Arc::new(run_list_dir),
    });
    r.register(ToolDef {
        name: "content_search".to_string(),
        desc: "Search INSIDE text files (code, notes, configs) for a substring and \
              return matching lines as file:line:text. Use for 'find where X is \
              defined', 'search my code for X', 'which files mention X' requests. \
              Skips binaries and dependency/vendor trees; may return partial \
              results on huge folders — if the result says the budget was \
              reached, retry with a narrower root (a subfolder)."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text to find, e.g. 'fn dispatch' or 'todo:'"
                },
                "root": {
                    "type": "string",
                    "description": "Optional directory to search in; defaults to the user's common folders"
                },
                "glob": {
                    "type": "string",
                    "description": "Optional file filter like '*.rs' or '*.md'"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max matches to return (default 25)"
                }
            },
            "required": ["query"]
        }),
        run: Arc::new(run_content_search),
    });
    r.register(ToolDef {
        name: "web_fetch".to_string(),
        desc: "Fetch a public web page and return its readable text (HTML converted \
              to plain text; JSON/other text returned as-is; size-capped). Use for \
              'summarize this URL', 'read this page', or to read a result found by \
              web_search. Binary files are refused — use download_file for those."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The http(s) URL to fetch"
                }
            },
            "required": ["url"]
        }),
        run: Arc::new(run_web_fetch),
    });
    r.register(ToolDef {
        name: "download_file".to_string(),
        desc: "Download a file from a public URL into the user's Downloads folder \
              (the only writable location). Use for 'download this file/pdf/image' \
              requests. Returns the saved path; refuses webpages (HTML) and files \
              over 200 MB."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The http(s) URL of the file"
                },
                "filename": {
                    "type": "string",
                    "description": "Optional name for the saved file; derived from the URL when omitted"
                }
            },
            "required": ["url"]
        }),
        run: Arc::new(run_download_file),
    });
    r.register(ToolDef {
        name: "web_search".to_string(),
        desc: "Search the public web with DuckDuckGo. Call this tool for ANY request \
              phrased like 'search the web for ...', 'look up ... online', 'find ... \
              on the internet', or whenever current/live information beyond your \
              training is needed (news, versions, prices, docs). Returns ranked \
              titles, URLs and snippets; follow up with web_fetch to read a page in \
              full."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "What to search for"
                },
                "max_results": {
                    "type": "integer",
                    "description": "Max results to return (default 8)"
                }
            },
            "required": ["query"]
        }),
        run: Arc::new(run_web_search),
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

/// A directory containing a `.git` entry (folder or worktree file) is a
/// nested repository — dependency-tree semantics: skipped when
/// descending (an explicit root INSIDE the clone still searches it).
fn is_nested_repo(dir: &Path) -> bool {
    dir.join(".git").exists()
}

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
/* v1.2 — persistent memory (hybrid: own sync store + vendored       */
/* mem0-rust kernels; the store itself lives in crate::memory)       */
/* ================================================================ */

/// Memory tools run against the process-wide store. Argument validation
/// happens BEFORE the store is locked, so a bad call can never touch the
/// user's memory file (this also keeps the lib tests hermetic: they only
/// exercise the validation path through `dispatch`, the store logic
/// directly, never a valid memory dispatch in-process).
fn locked_store() -> Result<std::sync::MutexGuard<'static, memory::MemoryStore>, String> {
    memory::global()
        .lock()
        .map_err(|_| "memory: store lock poisoned".to_string())
}

fn arg_tags(args: &Value) -> Result<Vec<String>, String> {
    match args.get("tags") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut tags = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => tags.push(s.clone()),
                    _ => return Err("memory: 'tags' must be an array of strings".to_string()),
                }
            }
            Ok(tags)
        }
        Some(_) => Err("memory: 'tags' must be an array of strings".to_string()),
    }
}

fn arg_limit(args: &Value) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(5),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v @ 1..=50) => Ok(v as usize),
            _ => Err("memory: 'limit' must be an integer between 1 and 50".to_string()),
        },
        Some(_) => Err("memory: 'limit' must be an integer between 1 and 50".to_string()),
    }
}

fn run_memory_save(args: &Value) -> Result<String, String> {
    let content = arg_string(args, "content")?;
    if content.trim().is_empty() {
        return Err("memory: 'content' must not be empty".to_string());
    }
    let tags = arg_tags(args)?;
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Memory, &content, &tags, None)?;
    Ok(if existed {
        format!("Already saved earlier ({}): {}", record.id, record.content)
    } else {
        format!("Memory saved ({}).", record.id)
    })
}

fn run_memory_recall(args: &Value) -> Result<String, String> {
    let query = arg_string(args, "query")?;
    let tags = arg_tags(args)?;
    let limit = arg_limit(args)?;
    let store = locked_store()?;
    let hits = store.recall(&query, &tags, limit);
    Ok(format_recall_hits(&hits))
}

fn run_create_note(args: &Value) -> Result<String, String> {
    let content = arg_string(args, "content")?;
    if content.trim().is_empty() {
        return Err("memory: 'content' must not be empty".to_string());
    }
    let tags = arg_tags(args)?;
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Note, &content, &tags, None)?;
    Ok(if existed {
        format!("An identical note already exists ({}).", record.id)
    } else {
        format!("Note created ({}).", record.id)
    })
}

fn run_activity_log(args: &Value) -> Result<String, String> {
    let action = arg_string(args, "action")?;
    if action.trim().is_empty() {
        return Err("memory: 'action' must not be empty".to_string());
    }
    let detail = match args.get("detail") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(_) => return Err("memory: 'detail' must be a string".to_string()),
    };
    let tags = arg_tags(args)?;
    let content = if detail.is_empty() {
        action
    } else {
        format!("{action} — {detail}")
    };
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Activity, &content, &tags, None)?;
    Ok(if existed {
        format!("Already logged earlier ({}).", record.id)
    } else {
        format!("Logged activity ({}).", record.id)
    })
}

fn run_reminder_set(args: &Value) -> Result<String, String> {
    let content = arg_string(args, "content")?;
    if content.trim().is_empty() {
        return Err("memory: 'content' must not be empty".to_string());
    }
    let due_at = arg_string(args, "due_at")?;
    if due_at.trim().is_empty() {
        return Err("memory: 'due_at' must not be empty".to_string());
    }
    let tags = arg_tags(args)?;
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Reminder, &content, &tags, Some(due_at))?;
    Ok(if existed {
        format!("This reminder already exists ({}), due {}.", record.id, record.due_at.unwrap_or_default())
    } else {
        format!(
            "Reminder saved ({}), due {}.",
            record.id,
            record.due_at.unwrap_or_default()
        )
    })
}

fn format_recall_hits(hits: &[ScoredMemory]) -> String {
    if hits.is_empty() {
        return "No matching records found.".to_string();
    }
    let mut out = String::from("Found:\n");
    for hit in hits {
        let record = &hit.record;
        out.push_str(&format!("- [{}] {}", record.kind.label(), record.content));
        if !record.tags.is_empty() {
            out.push_str(&format!(" ({})", record.tags.join(", ")));
        }
        if let Some(due) = &record.due_at {
            out.push_str(&format!(" — due {due}"));
        }
        out.push_str(&format!(" — saved {}", record.created_at));
        out.push('\n');
    }
    out
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

/* ================================================================ */
/* v2 — dev & browser (goal.md §4/§5/§6)                             */
/* list_dir · content_search · run_command ·                        */
/* web_fetch · download_file · web_search                           */
/*                                                                  */
/* Safety policy: run_command is a curated program+subcommand        */
/* allowlist (no shell, argv direct); download_file writes only into */
/* the user's Downloads folder; web tools block loopback/private     */
/* literal hosts so a fetched page cannot send the assistant after   */
/* local services; every web read is byte-capped.                   */
/* ================================================================ */

/* ---------------------------------------------------------------- */
/* list_dir                                                          */
/* ---------------------------------------------------------------- */

/// Hard entry cap: voice replies never need thousands of lines.
const LIST_DIR_MAX_ENTRIES: usize = 200;

/// Human byte size ("12 B", "3.4 KB", "5.6 MB", "2.1 GB").
fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n < K {
        format!("{n} B")
    } else if n < K * K {
        format!("{:.1} KB", n / K)
    } else if n < K * K * K {
        format!("{:.1} MB", n / (K * K))
    } else {
        format!("{:.1} GB", n / (K * K * K))
    }
}

fn run_list_dir(args: &Value) -> Result<String, String> {
    let raw = args
        .get("path")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let path = if raw.is_empty() {
        PathBuf::from(home_dir_string())
    } else {
        PathBuf::from(expand_home_path(&raw))
    };
    if !path.is_dir() {
        return Err(format!("Not a directory (or not found): {}", path.display()));
    }
    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    let read = fs::read_dir(&path)
        .map_err(|e| format!("Cannot list '{}': {e}", path.display()))?;
    for entry in read.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push((name, meta.is_dir(), meta.len()));
    }
    let total = entries.len();
    /* Folders first, then case-insensitive names — the shape Explorer shows. */
    entries.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });
    let shown = total.min(LIST_DIR_MAX_ENTRIES);
    let mut out = format!("{total} entries in {}:", path.display());
    for (name, is_dir, size) in &entries[..shown] {
        if *is_dir {
            out.push_str(&format!("\n  {name}/"));
        } else {
            out.push_str(&format!("\n  {name} ({})", fmt_bytes(*size)));
        }
    }
    if total > shown {
        out.push_str(&format!("\n  [and {} more entries]", total - shown));
    }
    Ok(out)
}

/* ---------------------------------------------------------------- */
/* content_search — bounded text grep over a directory tree          */
/* ---------------------------------------------------------------- */

/// File cap + time budget: partial results beat a dispatch timeout.
const CONTENT_MAX_FILES: usize = 1_500;
const CONTENT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const CONTENT_DEFAULT_LIMIT: u64 = 25;
const CONTENT_MAX_LIMIT: u64 = 100;
const CONTENT_SNIPPET_CHARS: usize = 200;

/// Cheap text-vs-binary gate: NUL bytes in the first 8 KiB. (read_file
/// sniffs replacement-character ratio after a full decode; here the
/// decision must happen BEFORE decoding, per file, inside the budget.)
fn looks_binary(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = fs::File::open(path) else {
        return true;
    };
    let mut head = [0u8; 8192];
    let mut read = 0usize;
    loop {
        match f.read(&mut head[read..]) {
            Ok(0) => break,
            Ok(n) => {
                read += n;
                if read == head.len() {
                    break;
                }
            }
            Err(_) => return true,
        }
    }
    head[..read].contains(&0)
}

fn run_content_search(args: &Value) -> Result<String, String> {
    let query = required_str(args, "query", "content_search")?;
    let query_lc = query.trim().to_lowercase();
    if query_lc.is_empty() {
        return Err("query is empty".into());
    }
    let limit =
        optional_u64(args, "limit", CONTENT_DEFAULT_LIMIT, 1, CONTENT_MAX_LIMIT) as usize;
    /* glob = "*.rs" style extension filter; stored without the dot. */
    let glob_ext: Option<String> = args
        .get("glob")
        .and_then(|v| v.as_str())
        .and_then(|g| {
            let g = g.trim().trim_start_matches('*').trim_start_matches('.');
            if g.is_empty() {
                None
            } else {
                Some(g.to_lowercase())
            }
        });
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
    let deadline = Instant::now() + SEARCH_TIME_BUDGET;
    let mut hits: Vec<(PathBuf, usize, String)> = Vec::new();
    let mut files_scanned = 0usize;
    let mut budget_hit = false;
    /* Breadth-first on purpose: a DFS pops the alphabetically-LAST
     * top-level subtree first (observed live: a nested clone starved
     * `crates/` entirely at repo-root scope). BFS gives top-level
     * project folders the first turn and dependency-like trees the
     * last. */
    use std::collections::VecDeque;
    'roots: for root in &roots {
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
        queue.push_back((root.clone(), 0usize));
        while let Some((dir, depth)) = queue.pop_front() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if Instant::now() >= deadline || files_scanned >= CONTENT_MAX_FILES {
                    budget_hit = true;
                    break 'roots;
                }
                let path = entry.path();
                if path.is_dir() {
                    let name_lc = entry.file_name().to_string_lossy().to_lowercase();
                    if depth < SEARCH_MAX_DEPTH
                        && !name_lc.starts_with('.')
                        && !SEARCH_SKIP_DIRS.contains(&name_lc.as_str())
                        && !is_nested_repo(&path)
                    {
                        queue.push_back((path, depth + 1));
                    }
                    continue;
                }
                if let Some(ext) = &glob_ext {
                    if path.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase())
                        != Some(ext.clone())
                    {
                        continue;
                    }
                }
                let Ok(meta) = entry.metadata() else { continue };
                if meta.len() > CONTENT_MAX_FILE_BYTES {
                    continue;
                }
                files_scanned += 1;
                if looks_binary(&path) {
                    continue;
                }
                let Ok(bytes) = fs::read(&path) else { continue };
                let text = String::from_utf8_lossy(&bytes);
                for (idx, line) in text.lines().enumerate() {
                    if line.to_lowercase().contains(&query_lc) {
                        let trimmed: String = line
                            .trim_end()
                            .chars()
                            .take(CONTENT_SNIPPET_CHARS)
                            .collect();
                        hits.push((path.clone(), idx + 1, trimmed));
                        if hits.len() >= limit {
                            break 'roots;
                        }
                    }
                }
            }
        }
    }
    let scope = if roots.len() == 1 {
        roots[0].display().to_string()
    } else {
        "the user's common folders".to_string()
    };
    let budget_note = if budget_hit {
        " (search budget reached before every folder was scanned — \
         results may be PARTIAL)"
    } else {
        ""
    };
    if hits.is_empty() {
        return Ok(format!(
            "No matches for '{query}'{budget_note} after scanning {files_scanned} text \
             files under {scope}."
        ));
    }
    let mut out = format!(
        "Found {} match(es) for '{query}'{budget_note} after scanning {files_scanned} \
         text files under {scope}:",
        hits.len()
    );
    for (path, line_no, line) in &hits {
        out.push_str(&format!("\n{}:{line_no}: {line}", path.display()));
    }
    Ok(out)
}

/* ---------------------------------------------------------------- */
/* run_command — curated program+subcommand allowlist                */
/* ---------------------------------------------------------------- */

/// Output ceiling fed to the model (compiler dumps can be enormous).
const RUN_COMMAND_MAX_OUTPUT_CHARS: usize = 4_000;
/// Internal kill deadline. Deliberately BELOW the dispatch budget
/// (`tool_budget("run_command")` = 120 s) so the tool always answers
/// before the dispatcher gives up.
const RUN_COMMAND_INTERNAL_BUDGET: Duration = Duration::from_secs(105);

/// goal.md safety rules: never free-form. Program → permitted first
/// argument (the subcommand). `cargo run` is listed because it gets the
/// special two-phase build-then-launch path below.
const COMMAND_ALLOWLIST: &[(&str, &[&str])] = &[
    (
        "cargo",
        &[
            "check", "build", "test", "run", "new", "init", "add", "remove", "fmt",
            "clippy", "tree",
        ],
    ),
    ("git", &["status", "log", "diff", "show", "rev-parse"]),
];

fn allowed_subcommands(program: &str) -> Option<&'static [&'static str]> {
    COMMAND_ALLOWLIST
        .iter()
        .find(|(p, _)| *p == program)
        .map(|(_, subs)| *subs)
}

fn validate_command(program: &str, args: &[String]) -> Result<(), String> {
    let Some(subs) = allowed_subcommands(program) else {
        return Err(format!(
            "Program '{program}' is not allowed. Allowed: {}",
            COMMAND_ALLOWLIST
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    if args.is_empty() {
        return Err(format!(
            "Missing subcommand for '{program}'. Allowed: {}",
            subs.join(", ")
        ));
    }
    let sub = args[0].as_str();
    if !subs.contains(&sub) {
        return Err(format!(
            "'{program} {sub}' is not allowed. Allowed: {}",
            subs.join(", ")
        ));
    }
    for a in args {
        if a.is_empty() {
            return Err("empty command argument".into());
        }
        /* No NUL (would truncate the argv) and no control bytes (no
         * newline smuggling into a console). Values are otherwise
         * verbatim argv entries — no shell, so no metacharacter risk. */
        if a.bytes().any(|b| b == 0 || b < 0x20) {
            return Err(format!("invalid control character in command argument '{a}'"));
        }
    }
    Ok(())
}

/// Spawn + capture + bounded wait. Returns (exit code, combined output).
/// `CREATE_NO_WINDOW` keeps console flashes off the desktop when the
/// assistant itself runs headless.
fn run_command_output(
    prog: &str,
    args: &[String],
    wd: Option<&Path>,
) -> Result<(i32, String), String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = wd {
        cmd.current_dir(dir);
    }
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start {prog}: {e}"))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if started.elapsed() >= RUN_COMMAND_INTERNAL_BUDGET {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Could not wait for {prog}: {e}")),
        }
    };
    let mut out_bytes = t_out.join().unwrap_or_default();
    let err_bytes = t_err.join().unwrap_or_default();
    if !out_bytes.is_empty() && !err_bytes.is_empty() && !out_bytes.ends_with(b"\n") {
        out_bytes.push(b'\n');
    }
    out_bytes.extend_from_slice(&err_bytes);
    let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
    Ok((exit_code, String::from_utf8_lossy(&out_bytes).into_owned()))
}

/// Shared output ceiling: cap by chars, append a truncation marker.
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(&format!(
        "\n[truncated: showing first {max_chars} of {total} characters]"
    ));
    out
}

/// First `name = "..."` inside the `[package]` section of a Cargo.toml
/// (line scan: comments and later sections ignored).
fn manifest_package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line.eq_ignore_ascii_case("[package]");
            continue;
        }
        if in_package && !line.starts_with('#') && line.starts_with("name") {
            if let Some(v) = line.split('=').nth(1) {
                let v = v.trim().trim_matches('"').trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Two-phase bare `cargo run`: build (captured, bounded), then launch the
/// built executable detached so the app stays alive after the reply —
/// a bounded capture cannot do that (a never-exiting app would hang the
/// turn or get killed). Workspace manifests without a `[package]` section
/// fall back to a plain bounded `cargo run`.
fn cargo_build_then_launch(wd: Option<&Path>) -> Result<String, String> {
    let dir = wd
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let manifest = dir.join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest)
        .map_err(|e| format!("Cannot read {}: {e}", manifest.display()))?;
    let Some(pkg) = manifest_package_name(&manifest_text) else {
        let (code, text) = run_command_output("cargo", &["run".to_string()], Some(&dir))?;
        return Ok(format!(
            "'cargo run' in {} finished with exit code {code}:\n{}",
            dir.display(),
            truncate_chars(&text, RUN_COMMAND_MAX_OUTPUT_CHARS)
        ));
    };
    let (code, text) = run_command_output("cargo", &["build".to_string()], Some(&dir))?;
    if code != 0 {
        return Ok(format!(
            "Build failed (exit code {code}):\n{}",
            truncate_chars(&text, RUN_COMMAND_MAX_OUTPUT_CHARS)
        ));
    }
    /* cargo maps hyphens to underscores in executable names. */
    let exe = dir
        .join("target")
        .join("debug")
        .join(format!("{}.exe", pkg.replace('-', "_")));
    if !exe.is_file() {
        return Err(format!(
            "Build reported success but {} was not found",
            exe.display()
        ));
    }
    let launch = std::process::Command::new(&exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not launch {}: {e}", exe.display()))?;
    let pid = launch.id();
    drop(launch);
    Ok(format!(
        "Build succeeded; launched {} (pid {pid}). It keeps running in the background.",
        exe.display()
    ))
}

fn run_run_command(args: &Value) -> Result<String, String> {
    let program = required_str(args, "program", "run_command")?
        .trim()
        .to_lowercase();
    let cmd_args: Vec<String> = match args.get("args") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => return Err("Missing required parameter: args (array of strings)".into()),
    };
    validate_command(&program, &cmd_args)?;
    let wd: Option<PathBuf> = match args.get("working_dir").and_then(|v| v.as_str()) {
        Some(d) if !d.trim().is_empty() => {
            let p = PathBuf::from(expand_home_path(d));
            if !p.is_dir() {
                return Err(format!("working_dir does not exist: {}", p.display()));
            }
            Some(p)
        }
        _ => None,
    };

    /* Bare `cargo run` gets the build-then-launch treatment. `cargo run
     * -- <appargs>` keeps bounded-capture semantics on purpose. */
    if program == "cargo" && cmd_args.len() == 1 && cmd_args[0] == "run" {
        return cargo_build_then_launch(wd.as_deref());
    }

    let (code, text) = run_command_output(&program, &cmd_args, wd.as_deref())?;
    let wd_note = wd
        .as_ref()
        .map(|p| format!(" in {}", p.display()))
        .unwrap_or_default();
    let out = if text.trim().is_empty() {
        "(no output)".to_string()
    } else {
        truncate_chars(text.trim_end(), RUN_COMMAND_MAX_OUTPUT_CHARS)
    };
    Ok(format!(
        "'{} {}'{wd_note} finished with exit code {code}:\n{out}",
        program,
        cmd_args.join(" ")
    ))
}

/* ---------------------------------------------------------------- */
/* web shared: agent, URL policy, entity decode                      */
/* ---------------------------------------------------------------- */

/// Browser-like UA: many sites (and DDG) 403 unknown clients.
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Web agent for the v2 browser tools — separate from the LLM server
/// agent (different timeout regime); per-request timeouts set at call
/// time so each tool controls its own budget.
static WEB_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .user_agent(WEB_USER_AGENT)
        .build()
        .into()
});

const WEB_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const WEB_SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const WEB_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

fn web_get(
    url: &str,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    WEB_AGENT
        .get(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|e| e.to_string())
}

/// Same scheme/character policy as `validate_open_url`, plus a block on
/// loopback/private LITERAL hosts (SSRF guard: fetched-page content must
/// never be able to send the assistant after local services). Best
/// effort by hostname literal — DNS-resolution checks are out of scope.
fn validate_web_url(url: &str) -> Result<(), String> {
    validate_open_url(url)?;
    let rest = url.trim();
    let rest = rest.split_once("://").map(|(_, r)| r).unwrap_or(rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    /* strip userinfo */
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if is_private_host(host) {
        return Err(format!(
            "Local/private addresses are not allowed for web tools (got '{host}')"
        ));
    }
    Ok(())
}

fn is_private_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_lowercase();
    if h.is_empty()
        || h == "localhost"
        || h == "0.0.0.0"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
    {
        return true;
    }
    /* [IPv6] or [IPv6]:port — a bare suffix(']') strip would miss ports. */
    if let Some(end) = h.find(']') {
        let inner = &h[1..end];
        let v6 = inner.split('%').next().unwrap_or(inner);
        return v6 == "::1"
            || v6.starts_with("fe8")
            || v6.starts_with("fc")
            || v6.starts_with("fd");
    }
    /* Bare host, possibly with a :port suffix — an authority like
     * "127.0.0.1:8018" keeps the port, so the port must go before the
     * IPv4 parse. Only strip an all-digit port when the prefix is a
     * dotted/IPv4-shaped address (a bracket-less IPv6 like ::1 has no
     * port and would be mangled by the split). */
    let bare = if let Some(colon) = h.rfind(':') {
        let (addr, port) = (&h[..colon], &h[colon + 1..]);
        if !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit())
            && (addr.contains('.') || addr.parse::<std::net::Ipv4Addr>().is_ok())
        {
            addr
        } else {
            h.as_str()
        }
    } else {
        h.as_str()
    };
    if let Ok(ip) = bare.parse::<std::net::Ipv4Addr>() {
        return ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_broadcast()
            || ip.is_unspecified();
    }
    /* Bracket-less IPv6 loopback (invalid in a URL authority, but cheap
     * to catch; no legal DNS name contains a colon). */
    bare.starts_with("::1")
}

/// Decode common HTML entities found in page/search text (named + decimal
/// + hex numeric). Unknown or malformed entities pass through unchanged.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest.find(';').filter(|&i| i <= 10).map(|i| &rest[..=i]);
        match entity {
            Some(e) => {
                match e {
                    "&amp;" => out.push('&'),
                    "&lt;" => out.push('\u{3c}'),
                    "&gt;" => out.push('\u{3e}'),
                    "&quot;" => out.push('"'),
                    "&apos;" | "&#39;" => out.push('\''),
                    "&nbsp;" => out.push(' '),
                    other => {
                        if let Some(num) = other.strip_prefix("&#") {
                            /* `other` still carries the terminating ';' */
                            let num = num.strip_suffix(';').unwrap_or(num);
                            let decoded = if let Some(hex) =
                                num.strip_prefix('x').or_else(|| num.strip_prefix('X'))
                            {
                                u32::from_str_radix(hex, 16).ok()
                            } else {
                                num.parse::<u32>().ok()
                            };
                            match decoded.and_then(char::from_u32) {
                                Some(c) => out.push(c),
                                None => out.push_str(other),
                            }
                        } else {
                            out.push_str(other);
                        }
                    }
                }
                rest = &rest[e.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/* ---------------------------------------------------------------- */
/* web_fetch — size-capped readable-text page reader                 */
/* ---------------------------------------------------------------- */

/// Hard byte cap on the response body actually read.
const FETCH_MAX_BYTES: usize = 512 * 1024;
/// Ceiling fed to the model after conversion.
const FETCH_MAX_CHARS: usize = 6_000;

static HTML_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)\x3c!--.*?--\x3e").expect("static regex"));
static HTML_SCRIPT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\x3cscript\b[^>]*>.*?\x3c/script\x3e"#).expect("static regex")
});
static HTML_STYLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\x3cstyle\b[^>]*>.*?\x3c/style\x3e"#).expect("static regex")
});
static HTML_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\x3c(?:br\b[^>]*|/(?:p|div|h[1-6]|li|tr|td|table|ul|ol|blockquote|",
        r"section|article|header|footer|nav|main|pre)\b[^>]*)\x3e"
    ))
    .expect("static regex")
});
static HTML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x3c[^>]*\x3e").expect("static regex"));
static HTML_WS_RUN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(" {2,}").expect("static regex"));

/// Minimal HTML-to-readable-text conversion for model consumption:
/// scripts/styles/comments removed, block boundaries become newlines,
/// remaining tags stripped, entities decoded, whitespace collapsed.
fn html_to_text(html: &str) -> String {
    let s = HTML_COMMENT_RE.replace_all(html, " ");
    let s = HTML_SCRIPT_RE.replace_all(&s, " ");
    let s = HTML_STYLE_RE.replace_all(&s, " ");
    let s = HTML_BLOCK_RE.replace_all(&s, "\n");
    let s = HTML_TAG_RE.replace_all(&s, " ");
    let s = HTML_WS_RUN_RE.replace_all(&s, " ");
    let s = decode_entities(&s);
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn is_binary_content_type(ctype: &str) -> bool {
    let main = ctype.split(';').next().unwrap_or("").trim();
    if main.is_empty() {
        return false; /* unknown: try the text path, sniffed next */
    }
    !(main.starts_with("text/")
        || main.contains("json")
        || main.contains("xml")
        || main.contains("javascript")
        || main.contains("csv"))
}

fn run_web_fetch(args: &Value) -> Result<String, String> {
    use std::io::Read;
    let url = required_str(args, "url", "web_fetch")?;
    validate_web_url(url)?;
    let url = url.trim().to_string();
    let resp = web_get(&url, WEB_FETCH_TIMEOUT)
        .map_err(|e| format!("Could not fetch {url}: {e}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status} from {url}"));
    }
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if is_binary_content_type(&ctype) {
        return Err(format!(
            "'{url}' serves binary content ({ctype}) — web_fetch returns text; use \
             download_file for files."
        ));
    }
    /* take(cap + 1): read at most the cap, and one extra byte flags a
     * bigger page without ever tripping the reader's own limit. */
    let mut reader = resp
        .into_body()
        .into_with_config()
        .limit((FETCH_MAX_BYTES + 1) as u64)
        .reader();
    let mut buf: Vec<u8> = Vec::new();
    (&mut reader)
        .take((FETCH_MAX_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Could not read body of {url}: {e}"))?;
    let truncated = buf.len() > FETCH_MAX_BYTES;
    buf.truncate(FETCH_MAX_BYTES);
    let text = String::from_utf8_lossy(&buf);
    let readable = if ctype.contains("html") || (ctype.is_empty() && text.contains('\u{3c}')) {
        html_to_text(&text)
    } else {
        text.into_owned()
    };
    let mut out = truncate_chars(&readable, FETCH_MAX_CHARS);
    if truncated {
        out.push_str("\n[page larger than the fetch cap; content truncated]");
    }
    Ok(format!("Content of {url}:\n{out}"))
}

/* ---------------------------------------------------------------- */
/* download_file — streamed save into Downloads (fixed writable path) */
/* ---------------------------------------------------------------- */

const DOWNLOAD_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Path-safety: no separators, no traversal, no reserved/control chars,
/// no leading dots, sane length.
fn sanitize_filename(name: &str) -> String {
    let mut out: String = name
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '\u{3c}' | '\u{3e}' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() || out.chars().all(|c| c == '_') {
        return String::new();
    }
    out.chars().take(100).collect()
}

/// Last non-empty path segment of a URL (query/fragment stripped),
/// percent-decoded.
fn download_filename_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let segment = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
    percent_decode(segment)
}

/// Percent-decode %XX escapes (bytes preserved), everything else
/// verbatim. Lossy UTF-8 on the final join.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            /* need bytes[i+1] and bytes[i+2] */
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn run_download_file(args: &Value) -> Result<String, String> {
    use std::io::{Read, Write};
    let url = required_str(args, "url", "download_file")?;
    validate_web_url(url)?;
    let url = url.trim().to_string();
    let dir = dirs::download_dir().ok_or("Could not resolve the user's Downloads folder")?;
    if !dir.is_dir() {
        return Err(format!("Downloads folder not found: {}", dir.display()));
    }
    let name = match args.get("filename").and_then(|v| v.as_str()) {
        Some(f) => {
            let s = sanitize_filename(f);
            if s.is_empty() {
                return Err(format!("Invalid filename '{f}'"));
            }
            s
        }
        None => {
            let derived = sanitize_filename(&download_filename_from_url(&url));
            if derived.is_empty() {
                return Err(
                    "Could not derive a filename from the URL — pass a filename".into()
                );
            }
            derived
        }
    };
    let target = dir.join(&name);
    if target.exists() {
        return Err(format!(
            "'{name}' already exists in Downloads — choose a different filename"
        ));
    }
    let resp = web_get(&url, WEB_DOWNLOAD_TIMEOUT)
        .map_err(|e| format!("Could not fetch {url}: {e}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status} from {url}"));
    }
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if ctype.starts_with("text/html") {
        return Err(format!(
            "'{url}' returned a webpage (HTML), not a downloadable file — it may be an \
             error page or a link to a viewer page."
        ));
    }
    if let Some(len) = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > DOWNLOAD_MAX_BYTES {
            return Err(format!(
                "File is {len} bytes; download limit is {DOWNLOAD_MAX_BYTES}"
            ));
        }
    }
    /* LimitReader at cap+1: any server trying to exceed the cap errors
     * out and the partial file is deleted — never a runaway disk write. */
    let mut reader = resp
        .into_body()
        .into_with_config()
        .limit(DOWNLOAD_MAX_BYTES + 1)
        .reader();
    let mut file = fs::File::create(&target)
        .map_err(|e| format!("Could not create {}: {e}", target.display()))?;
    let mut total: u64 = 0;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                if total > DOWNLOAD_MAX_BYTES {
                    drop(file);
                    let _ = fs::remove_file(&target);
                    return Err(format!(
                        "File exceeds the {DOWNLOAD_MAX_BYTES} byte download limit; aborted"
                    ));
                }
                file.write_all(&chunk[..n])
                    .map_err(|e| format!("Write failed: {e}"))?;
            }
            Err(e) => {
                drop(file);
                let _ = fs::remove_file(&target);
                return Err(format!("Download failed: {e}"));
            }
        }
    }
    let _ = file.flush();
    Ok(format!(
        "Downloaded {name} ({}) to {}",
        fmt_bytes(total),
        target.display()
    ))
}

/* ---------------------------------------------------------------- */
/* web_search — DuckDuckGo "lite" HTML backend (no API key)          */
/* ---------------------------------------------------------------- */

const SEARCH_RESULTS_DEFAULT: u64 = 8;
const SEARCH_RESULTS_MAX: u64 = 15;
const SEARCH_PAGE_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct WebResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Clean a fragment (title/snippet): strip inline tags, decode entities,
/// collapse whitespace.
fn clean_html_fragment(s: &str) -> String {
    let stripped = HTML_TAG_RE.replace_all(s, " ");
    let decoded = decode_entities(&stripped);
    HTML_WS_RUN_RE
        .replace_all(&decoded, " ")
        .trim()
        .to_string()
}

/// `//duckduckgo.com/l/?uddg=<percent-encoded-target>&rut=…` redirect
/// wrappers carry the real target in the uddg parameter.
fn unwrap_ddg_redirect(href: &str) -> String {
    let h = href.trim();
    for prefix in [
        "//duckduckgo.com/l/?uddg=",
        "https://duckduckgo.com/l/?uddg=",
        "http://duckduckgo.com/l/?uddg=",
    ] {
        if let Some(rest) = h.strip_prefix(prefix) {
            let end = rest.find('&').unwrap_or(rest.len());
            return percent_decode(&rest[..end]);
        }
    }
    if let Some(rest) = h.strip_prefix("//") {
        return format!("https://{rest}");
    }
    h.to_string()
}

/// Parse the DuckDuckGo "lite" result page. Markup verified live
/// 2026-11-14 (probe): result titles in `class='result-link'` anchors
/// (href is the redirect wrapper above), snippets in
/// `class='result-snippet'` cells, in result order. Quote style varies,
/// so the regexes accept both. Snippets are zipped by index — a result
/// without a snippet row can shift alignment (accepted trade-off).
fn parse_ddg_lite(html: &str) -> Vec<WebResult> {
    static LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)\x3ca\b[^>]*class\s*=\s*['"]result-link['"][^>]*>(.*?)\x3c/a\x3e"#)
            .expect("static regex")
    });
    static HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)\x3ca\b[^>]*href\s*=\s*['"]([^'"]+)['"]"#).expect("static regex")
    });
    static SNIP_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)\x3ctd\b[^>]*class\s*=\s*['"]result-snippet['"][^>]*>(.*?)\x3c/td\x3e"#)
            .expect("static regex")
    });
    let mut results: Vec<WebResult> = Vec::new();
    for m in LINK_RE.find_iter(html) {
        let tag_and_text = m.as_str();
        let url = HREF_RE
            .captures(tag_and_text)
            .and_then(|c| c.get(1))
            .map(|h| unwrap_ddg_redirect(h.as_str()))
            .unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        let title = LINK_RE
            .captures(tag_and_text)
            .and_then(|c| c.get(1))
            .map(|t| clean_html_fragment(t.as_str()))
            .unwrap_or_default();
        results.push(WebResult {
            title,
            url,
            snippet: String::new(),
        });
    }
    let snippets: Vec<String> = SNIP_RE
        .captures_iter(html)
        .filter_map(|c| c.get(1))
        .map(|c| clean_html_fragment(c.as_str()))
        .collect();
    for (i, r) in results.iter_mut().enumerate() {
        if let Some(s) = snippets.get(i) {
            r.snippet = s.clone();
        }
    }
    results
}

fn run_web_search(args: &Value) -> Result<String, String> {
    let query = required_str(args, "query", "web_search")?;
    let query = query.trim();
    if query.is_empty() {
        return Err("query is empty".into());
    }
    let limit = optional_u64(
        args,
        "max_results",
        SEARCH_RESULTS_DEFAULT,
        1,
        SEARCH_RESULTS_MAX,
    ) as usize;
    let url = format!(
        "https://lite.duckduckgo.com/lite/?q={}",
        url_encode_value(query)
    );
    let resp = web_get(&url, WEB_SEARCH_TIMEOUT)
        .map_err(|e| format!("Search failed: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!(
            "DuckDuckGo returned HTTP {status} — the search backend may be \
             rate-limiting; try again shortly."
        ));
    }
    let html = resp
        .into_body()
        .into_with_config()
        .limit(SEARCH_PAGE_MAX_BYTES as u64)
        .read_to_string()
        .map_err(|e| format!("Could not read search page: {e}"))?;
    let results = parse_ddg_lite(&html);
    if results.is_empty() {
        return Err(
            "DuckDuckGo returned no parseable results (markup change or bot challenge) — \
             try rephrasing the query."
                .into(),
        );
    }
    let shown = results.len().min(limit);
    let mut out = format!("Top {shown} web results for '{query}':");
    for (i, r) in results.iter().take(shown).enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("\n   {}", r.snippet));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryRecord;
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
                "activity_log",
                "calculate",
                "content_search",
                "create_folder",
                "create_note",
                "download_file",
                "file_search",
                "get_time",
                "list_dir",
                "media_key",
                "memory_recall",
                "memory_save",
                "open_app",
                "open_url",
                "read_file",
                "reminder_set",
                "run_command",
                "screenshot",
                "set_volume",
                "sys_stats",
                "web_fetch",
                "web_search"
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

    /* ---- v1.2 memory: validation path only ------------------------ */

    /* Valid memory calls lock the process-wide store (the user's real
     * file), so lib tests exercise only the argument-validation path —
     * every case below errors out before the store lock is taken. The
     * store logic itself is covered in memory.rs, including the
     * survive-restart proof. */

    #[test]
    fn memory_tools_reject_bad_args_before_touching_the_store() {
        let r = seed_registry();
        assert!(r.dispatch("memory_save", &json!({})).unwrap_err().contains("content"));
        assert!(r
            .dispatch("memory_save", &json!({"content": "   "}))
            .unwrap_err()
            .contains("content"));
        assert!(r
            .dispatch("memory_save", &json!({"content": "ok", "tags": "x"}))
            .unwrap_err()
            .contains("tags"));
        assert!(r.dispatch("memory_recall", &json!({})).unwrap_err().contains("query"));
        assert!(r
            .dispatch("memory_recall", &json!({"query": "x", "limit": 0}))
            .unwrap_err()
            .contains("limit"));
        assert!(r
            .dispatch("memory_recall", &json!({"query": "x", "limit": "5"}))
            .unwrap_err()
            .contains("limit"));
        assert!(r.dispatch("create_note", &json!({})).unwrap_err().contains("content"));
        assert!(r
            .dispatch("activity_log", &json!({"action": "x", "detail": 5}))
            .unwrap_err()
            .contains("detail"));
        assert!(r.dispatch("reminder_set", &json!({"content": "call mom"})).unwrap_err().contains("due_at"));
        assert!(r
            .dispatch("reminder_set", &json!({"content": "call mom", "due_at": "   "}))
            .unwrap_err()
            .contains("due_at"));
    }

    #[test]
    fn format_recall_hits_shapes_the_model_reply() {
        assert_eq!(format_recall_hits(&[]), "No matching records found.");
        let hits = vec![
            ScoredMemory {
                record: MemoryRecord {
                    id: "m1".to_string(),
                    kind: MemoryKind::Reminder,
                    content: "Call mom".to_string(),
                    tags: vec!["family".to_string()],
                    created_at: "2026-11-14 12:00:00".to_string(),
                    due_at: Some("tomorrow 9am".to_string()),
                },
                score: 1.0,
                via: "exact",
            },
            ScoredMemory {
                record: MemoryRecord {
                    id: "m2".to_string(),
                    kind: MemoryKind::Memory,
                    content: "Wifi password is hunter2".to_string(),
                    tags: Vec::new(),
                    created_at: "2026-11-14 12:30:00".to_string(),
                    due_at: None,
                },
                score: 0.5,
                via: "similar",
            },
        ];
        let out = format_recall_hits(&hits);
        assert!(out.starts_with("Found:\n"), "{out}");
        assert!(out.contains("- [reminder] Call mom (family) — due tomorrow 9am — saved 2026-11-14 12:00:00"), "{out}");
        assert!(out.contains("- [memory] Wifi password is hunter2 — saved 2026-11-14 12:30:00"), "{out}");
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
        assert_eq!(names.len(), 23); /* 22 built-ins + 1 config tenant */
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

    /* ---- v2: per-tool budgets ---- */

    #[test]
    fn tool_budget_gives_v2_tools_more_than_the_voice_default() {
        assert_eq!(tool_budget("run_command"), Duration::from_secs(120));
        assert_eq!(tool_budget("download_file"), Duration::from_secs(120));
        assert_eq!(tool_budget("web_fetch"), Duration::from_secs(25));
        assert_eq!(tool_budget("web_search"), Duration::from_secs(20));
        /* everything else keeps the 5 s voice budget */
        assert_eq!(tool_budget("get_time"), TOOL_TIMEOUT);
        assert_eq!(tool_budget("no_such_tool"), TOOL_TIMEOUT);
    }

    /* ---- list_dir ---- */

    #[test]
    fn list_dir_sorts_dirs_first_and_shows_sizes() {
        let root = temp_root("listdir");
        fs::create_dir_all(root.join("beta_dir")).unwrap();
        fs::create_dir_all(root.join("gamma_dir")).unwrap();
        fs::write(root.join("alpha_file.txt"), "hello world").unwrap();
        let out = reg()
            .dispatch("list_dir", &json!({"path": root.to_string_lossy()}))
            .unwrap();
        let b = out.find("beta_dir/").unwrap();
        let g = out.find("gamma_dir/").unwrap();
        let a = out.find("alpha_file.txt (11 B)").unwrap();
        assert!(b < g && g < a, "{out}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_dir_caps_at_200_entries_with_overflow_note() {
        let root = temp_root("listdir_cap");
        for i in 0..205 {
            fs::write(root.join(format!("f{i:03}.txt")), "x").unwrap();
        }
        let out = reg()
            .dispatch("list_dir", &json!({"path": root.to_string_lossy()}))
            .unwrap();
        /* count file lines only — the overflow note also starts "\n  " */
        assert_eq!(out.matches("\n  f").count(), 200);
        assert!(out.contains("[and 5 more entries]"), "{out}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn list_dir_errors_on_missing_dir_and_defaults_to_home() {
        assert!(
            reg()
                .dispatch("list_dir", &json!({"path": "Z:\\definitely\\missing"}))
                .is_err()
        );
        /* no path -> the user's home listing (home exists in test envs) */
        let out = reg().dispatch("list_dir", &json!({})).unwrap();
        assert!(out.contains("entries in"), "{out}");
    }

    /* ---- content_search ---- */

    #[test]
    fn content_search_finds_lines_skips_binary_and_skipped_dirs() {
        let root = temp_root("contentsearch");
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(
            root.join("code.rs"),
            "use std;\n\nfn dispatch_here() {}\nlet x = DISPATCH_SIZE;\n",
        )
        .unwrap();
        fs::write(root.join("notes.md"), "DISPATCH means send\n").unwrap();
        fs::write(root.join("node_modules").join("gone.txt"), "dispatch secret\n").unwrap();
        fs::write(root.join("bin.dat"), b"dispatch\x00binary").unwrap();
        let out = reg()
            .dispatch(
                "content_search",
                &json!({"query": "dispatch", "root": root.to_string_lossy()}),
            )
            .unwrap();
        assert!(out.contains("code.rs:3"), "{out}");
        assert!(out.contains("code.rs:4"), "{out}");
        assert!(out.contains("notes.md:1"), "{out}");
        /* binary + dependency trees never surface */
        assert!(!out.contains("bin.dat"), "{out}");
        assert!(!out.contains("node_modules"), "{out}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn content_search_glob_filter_and_bad_root() {
        let root = temp_root("contentsearch_glob");
        fs::write(root.join("code.rs"), "needle here\n").unwrap();
        fs::write(root.join("notes.md"), "needle there\n").unwrap();
        let out = reg()
            .dispatch(
                "content_search",
                &json!({"query": "needle", "root": root.to_string_lossy(), "glob": "*.rs"}),
            )
            .unwrap();
        assert!(out.contains("code.rs:1"), "{out}");
        assert!(!out.contains("notes.md"), "{out}");
        let err = reg()
            .dispatch(
                "content_search",
                &json!({"query": "x", "root": "Z:\\no\\such\\dir"}),
            )
            .unwrap_err();
        assert!(err.contains("does not exist"), "{err}");
        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn content_search_skips_nested_repos_on_descent() {
        let root = temp_root("contentsearch_repo");
        fs::create_dir_all(root.join("nested").join(".git")).unwrap();
        fs::write(root.join("nested").join("hidden.txt"), "needle in a clone\n").unwrap();
        fs::write(root.join("top.txt"), "needle at top\n").unwrap();
        let out = reg()
            .dispatch(
                "content_search",
                &json!({"query": "needle", "root": root.to_string_lossy()}),
            )
            .unwrap();
        assert!(out.contains("top.txt:1"), "{out}");
        assert!(!out.contains("nested"), "{out}");
        /* an explicit root INTO the nested repo still searches it */
        let out = reg()
            .dispatch(
                "content_search",
                &json!({
                    "query": "needle",
                    "root": root.join("nested").to_string_lossy()
                }),
            )
            .unwrap();
        assert!(out.contains("hidden.txt:1"), "{out}");
        fs::remove_dir_all(&root).ok();
    }

    /* ---- run_command ---- */

    #[test]
    fn run_command_allowlist_rejects_free_form() {
        let err = reg()
            .dispatch(
                "run_command",
                &json!({"program": "python", "args": ["-c", "print(1)"]}),
            )
            .unwrap_err();
        assert!(err.contains("is not allowed"), "{err}");
        let err = reg()
            .dispatch("run_command", &json!({"program": "cargo", "args": ["clean"]}))
            .unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
        let err = reg()
            .dispatch("run_command", &json!({"program": "git", "args": ["push", "origin"]}))
            .unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
        let err = reg()
            .dispatch("run_command", &json!({"program": "git", "args": []}))
            .unwrap_err();
        assert!(err.contains("Missing subcommand"), "{err}");
        let err = reg()
            .dispatch(
                "run_command",
                &json!({"program": "git", "args": ["status", "--short\n"]}),
            )
            .unwrap_err();
        assert!(err.contains("control character"), "{err}");
    }

    #[test]
    fn run_command_real_git_status_reports_exit_code() {
        /* Read-only, fast, and proves the whole spawn-capture-wait path. */
        let out = reg()
            .dispatch(
                "run_command",
                &json!({
                    "program": "git",
                    "args": ["status"],
                    "working_dir": env!("CARGO_MANIFEST_DIR")
                }),
            )
            .unwrap();
        assert!(out.contains("finished with exit code 0"), "{out}");
        assert!(out.contains("'git status'"), "{out}");
    }

    #[test]
    fn run_command_output_reports_nonzero_exit_without_failing() {
        let (code, text) = run_command_output(
            "git",
            &["log".to_string(), "--definitely-not-a-flag".to_string()],
            Some(Path::new(env!("CARGO_MANIFEST_DIR"))),
        )
        .unwrap();
        assert_ne!(code, 0);
        assert!(!text.is_empty(), "usage errors print something");
    }

    #[test]
    fn run_command_working_dir_must_exist() {
        let err = reg()
            .dispatch(
                "run_command",
                &json!({"program": "git", "args": ["status"], "working_dir": "Z:\\no\\dir"}),
            )
            .unwrap_err();
        assert!(err.contains("working_dir does not exist"), "{err}");
    }

    #[test]
    fn manifest_package_name_scans_only_the_package_section() {
        let text = "[package]\n# name = \"commented\"\nname = \"my-app\"\nedition = \"2024\"\n\
                    \n[dependencies]\nname = \"should-not-win\"\n";
        assert_eq!(manifest_package_name(text), Some("my-app".to_string()));
        assert_eq!(manifest_package_name("[workspace]\nmembers = []\n"), None);
    }

    /* ---- shared v2 helpers ---- */

    #[test]
    fn percent_decode_sanitize_and_format_helpers() {
        assert_eq!(
            percent_decode("https%3A%2F%2Frust%2Dlang.org%2F"),
            "https://rust-lang.org/"
        );
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E2%82%AC"), "\u{20AC}");
        assert_eq!(percent_decode("100%"), "100%"); /* dangling escape passes through */
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(sanitize_filename("a\u{3c}b\u{3e}:x?.txt"), "a_b__x_.txt");
        assert_eq!(sanitize_filename("../x"), "_x");
        assert_eq!(sanitize_filename("   "), "");
        assert_eq!(sanitize_filename("___"), "");
        assert_eq!(
            download_filename_from_url("https://x.com/files/report%202026.pdf?dl=1"),
            "report 2026.pdf"
        );
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KB");
        assert_eq!(fmt_bytes(5_600_000), "5.3 MB");
        assert_eq!(truncate_chars("hello", 10), "hello");
        let t = truncate_chars("hello world", 5);
        assert!(t.starts_with("hello") && t.contains("[truncated"), "{t}");
    }

    /* ---- web tools: pure helpers + validation (live network is e2e) ---- */

    #[test]
    fn web_urls_block_private_hosts_and_non_http() {
        for bad in [
            "http://127.0.0.1:8018/health",
            "https://localhost/x",
            "http://10.0.0.5/x",
            "http://192.168.1.10/x",
            "http://172.16.0.1/x",
            "http://169.254.169.254/metadata",
            "http://[::1]:8080/x",
            "http://machine.local/x",
            "ftp://example.com/file",
        ] {
            assert!(validate_web_url(bad).is_err(), "must reject {bad}");
        }
        validate_web_url("https://example.com/page?q=1").unwrap();
        validate_web_url("http://example.com").unwrap();
    }

    #[test]
    fn decode_entities_named_decimal_hex_and_unknown() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "\u{3c}tag\u{3e}");
        assert_eq!(decode_entities("&quot;hi&quot;"), "\"hi\"");
        assert_eq!(decode_entities("it&#39;s"), "it's");
        assert_eq!(decode_entities("a&nbsp;b"), "a b");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
        assert_eq!(decode_entities("100% & this"), "100% & this");
    }

    /// Angle tags written as escapes — no raw tag text in source.
    const HTML_FIXTURE: &str = "\x3chtml\x3e\x3chead\x3e\x3ctitle\x3eMy Page\x3c/title\x3e\
\x3cstyle\x3e.x{color:red}\x3c/style\x3e\x3c/head\x3e\
\x3cbody\x3e\x3c!-- hidden --\x3e\x3ch1\x3eHello\x3c/h1\x3e\
\x3cp\x3eFirst&nbsp;paragraph with \x3cb\x3ebold\x3c/b\x3e &amp; entities.\x3c/p\x3e\
\x3cscript\x3ealert('no');\x3c/script\x3e\
\x3cul\x3e\x3cli\x3eone\x3c/li\x3e\x3cli\x3etwo\x3c/li\x3e\x3c/ul\x3e\x3c/body\x3e\x3c/html\x3e";

    #[test]
    fn html_to_text_strips_noise_keeps_readable_text() {
        let text = html_to_text(HTML_FIXTURE);
        assert!(text.contains("My Page"), "{text}");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("First paragraph with bold & entities."), "{text}");
        assert!(text.contains("one") && text.contains("two"), "{text}");
        assert!(!text.contains("alert"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn binary_content_type_classification() {
        assert!(is_binary_content_type("image/png"));
        assert!(is_binary_content_type("application/pdf"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(!is_binary_content_type("text/html; charset=utf-8"));
        assert!(!is_binary_content_type("application/json"));
        assert!(!is_binary_content_type("application/xhtml+xml"));
        assert!(!is_binary_content_type(""));
    }

    /* Real markup shape captured live from lite.duckduckgo.com (probe,
     * 2026-11-14): redirect-wrapped hrefs with percent-encoded targets,
     * single-quoted classes, snippet cells in result order. */
    const DDG_FIXTURE: &str = "\x3ctable\x3e\
\x3ctr\x3e\x3ctd\x3e\x3ca rel='nofollow' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&amp;rut=abc123' class='result-link'\x3eRust \x3cb\x3eProgramming\x3c/b\x3e Language\x3c/a\x3e\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e&nbsp;&nbsp;&nbsp;\x3c/td\x3e\x3ctd class='result-snippet'\x3eA language empowering everyone to build reliable &amp; efficient software.\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e\x3cspan class='link-text'\x3erust-lang.org\x3c/span\x3e\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e\x3ca rel='nofollow' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2F&amp;rut=def456' class='result-link'\x3eThe Rustdoc book\x3c/a\x3e\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e&nbsp;&nbsp;&nbsp;\x3c/td\x3e\x3ctd class='result-snippet'\x3eLearn the language &lt;em&gt;for free&lt;/em&gt;\x3c/td\x3e\x3c/tr\x3e\
\x3c/table\x3e";

    #[test]
    fn parse_ddg_lite_extracts_results_redirects_and_snippets() {
        let results = parse_ddg_lite(DDG_FIXTURE);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(
            results[0].snippet,
            "A language empowering everyone to build reliable & efficient software."
        );
        assert_eq!(results[1].title, "The Rustdoc book");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/");
        assert_eq!(results[1].snippet, "Learn the language \u{3c}em\u{3e}for free\u{3c}/em\u{3e}");
    }

    #[test]
    fn unwrap_ddg_redirect_variants() {
        assert_eq!(
            unwrap_ddg_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fx.org%2F&rut=z"),
            "https://x.org/"
        );
        assert_eq!(
            unwrap_ddg_redirect("https://duckduckgo.com/l/?uddg=a%20b&rut=z"),
            "a b"
        );
        assert_eq!(
            unwrap_ddg_redirect("//example.com/plain"),
            "https://example.com/plain"
        );
        assert_eq!(
            unwrap_ddg_redirect("https://example.com/full"),
            "https://example.com/full"
        );
    }

    #[test]
    fn web_search_and_fetch_validation_errors() {
        let err = reg().dispatch("web_search", &json!({"query": "   "})).unwrap_err();
        assert!(err.contains("query is empty"), "{err}");
        let err = reg()
            .dispatch("web_fetch", &json!({"url": "http://127.0.0.1:8018/health"}))
            .unwrap_err();
        assert!(err.contains("Local/private"), "{err}");
        let err = reg()
            .dispatch("web_fetch", &json!({"url": "notaurl"}))
            .unwrap_err();
        assert!(err.contains("Only http"), "{err}");
        let err = reg()
            .dispatch(
                "download_file",
                &json!({"url": "https://example.com/f.pdf", "filename": "   "}),
            )
            .unwrap_err();
        assert!(err.contains("Invalid filename"), "{err}");
    }
}
