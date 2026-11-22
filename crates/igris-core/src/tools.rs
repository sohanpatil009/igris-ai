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
//! default through `tool_budget` (one auditable table).
//!
//! v2.5 (2026-11-15) — gated actions: destructive tools (close_app,
//! delete_file, shutdown, power_mode) carry `gated: true` plus a `plan`
//! validator. Their first call only validates + arms a pending (see
//! [`crate::gating`]) and announces the action; a confirmed re-call in a
//! later turn executes. Config-defined tools are always ungated and may
//! not shadow a gated built-in name (see
//! `Registry::register_ungated_guarded`).
//!
//! v3 (2026-11-16) — recycle bin (U1 real-world follow-up): `recycle_bin`
//! (ungated, read-only indexed listing) and `empty_recycle_bin` (gated
//! permanent removal — `action=delete` with 1-based indices from the
//! listing, or `action=empty` for everything). Entries are the bin's own
//! `$I`/`$R` metadata pairs, walked with plain `std::fs` over every
//! drive's `$Recycle.Bin`; invisible `$I` remnants are swept by
//! `action=empty`, unindexed files in the bin folders are counted in
//! readbacks and never touched.
//!
//! P3 item 9 (2026-11-17) — `github_repo_info`: repository facts
//! (stars, license, latest release, ...) straight from the GitHub API,
//! so framework/library comparison and version questions get
//! deterministic structured data instead of web rounds. The task-class
//! budget machinery around it lives in `crate::budget` and
//! `crate::llm`'s agent loop.
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


mod basic;
mod browser;
mod config_tools;
mod dev;
mod files;
mod gated;
mod memory_tools;
mod recycle;
mod registry;
mod shared;
#[cfg(test)]
mod testkit;
mod system;
mod web;
mod windows;

pub use config_tools::{config_tools_path, load_config_tools};
pub use registry::{DispatchPhase, Registry, ToolDef};
/// Re-export for llm.rs's history compaction (the shared module itself
/// stays private to the tool families).
pub(crate) use shared::truncate_chars;

use crate::tools::basic::{run_calculate, run_get_time, run_media_key, run_open_app, run_open_url};
use crate::tools::browser::{
    run_browser_click, run_browser_close, run_browser_open, run_browser_open_and_click,
    run_browser_vision_click,
    run_browser_read,
};
use crate::tools::config_tools::CONFIG_TOOLS;
use crate::tools::dev::run_run_command;
use crate::tools::files::{
    plan_replace_file, run_content_search, run_copy_files, run_create_folder, run_file_metadata,
    run_file_search, run_find_duplicates, run_list_dir, run_list_files, run_read_file,
    run_replace_file, run_workspace_info, run_write_file,
};
use crate::tools::gated::{
    plan_close_app, plan_delete_file, plan_power_mode, plan_shutdown, run_close_app,
    run_delete_file, run_power_mode, run_shutdown, run_shutdown_abort,
};
use crate::tools::memory_tools::{
    run_activity_log, run_create_note, run_memory_recall, run_memory_save, run_reminder_set,
};
use crate::tools::recycle::{
    plan_empty_recycle_bin, run_empty_recycle_bin, run_recycle_bin,
};
use crate::tools::system::{run_disk_stats, run_process_stats, run_set_volume, run_screenshot, run_sys_stats};
use crate::tools::web::{
    run_download_file, run_github_repo_info, run_web_fetch, run_web_search,
};
use crate::tools::windows::{run_focus_window, run_list_windows};
use serde_json::json;
use std::sync::{Arc, LazyLock};

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
        gated: false,
        plan: None,
        run: Arc::new(run_calculate),
    });
    r.register(ToolDef {
        name: "get_time".to_string(),
        desc: "Get the current local date and time.".to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
        run: Arc::new(run_create_folder),
    });
    r.register(ToolDef {
        name: "write_file".to_string(),
        desc: "Create a NEW text file with the given content (missing parent folders are \
              created). Paths must be absolute or start with ~, and must live under the \
              user profile, the OS temp dir, or the IGRIS workspace root — call \
              workspace_info to see them. Refuses to overwrite an existing file (use \
              replace_file for that) and refuses executable/script file types. Content \
              is capped at 256 KB."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path to create (absolute or ~ based)"
                },
                "content": {
                    "type": "string",
                    "description": "Full text content to write into the file"
                }
            },
            "required": ["path", "content"]
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_write_file),
    });
    r.register(ToolDef {
        name: "replace_file".to_string(),
        desc: "Overwrite an EXISTING text file with new content. Gated: the first call \
              only validates the arguments and announces the plan — it does NOT write. \
              Tell the user exactly what will be replaced and wait for a clear yes; \
              after the user confirms, call it again with identical arguments and it \
              executes. Same path rules as write_file."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Existing file path to overwrite (absolute or ~ based)"
                },
                "content": {
                    "type": "string",
                    "description": "New text content that replaces the file's contents"
                }
            },
            "required": ["path", "content"]
        }),
        gated: true,
        plan: Some(Arc::new(plan_replace_file)),
        run: Arc::new(run_replace_file),
    });
    r.register(ToolDef {
        name: "workspace_info".to_string(),
        desc: "Report the IGRIS workspace root (the project directory) and the folders \
              file tools can read and write. Call this when a task mentions 'my \
              project', 'the IGRIS project', or any path you are unsure about."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: false,
        plan: None,
        run: Arc::new(run_workspace_info),
    });
    r.register(ToolDef {
        name: "list_files".to_string(),
        desc: "List files under a folder sorted by size, largest first. Set recursive to \
              descend into subfolders and extension to filter (e.g. 'pdf'). Use this \
              instead of guessing file names or counting by hand."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "root": {
                    "type": "string",
                    "description": "Folder to list (default: your home folder)"
                },
                "recursive": {
                    "type": "boolean",
                    "description": "Also descend into subfolders (default false)"
                },
                "extension": {
                    "type": "string",
                    "description": "Only list files with this extension, e.g. 'pdf' or '.md'"
                },
                "limit": {
                    "type": "integer",
                    "description": "How many files to show (default 20, max 100)"
                }
            },
            "required": []
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_list_files),
    });
    r.register(ToolDef {
        name: "file_metadata".to_string(),
        desc: "Report a file or folder's type, size, extension, creation/modification/\
              access timestamps and read-only flag. Call this instead of guessing a \
              file's size or kind."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File or folder path (absolute or ~ based)"
                }
            },
            "required": ["path"]
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_file_metadata),
    });
    r.register(ToolDef {
        name: "find_duplicates".to_string(),
        desc: "Find duplicate files under a folder: same-size prefilter, then SHA-256 \
              hashing. Report-only — nothing is ever deleted. Large scans are \
              budget-limited and say so in the result."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "root": {
                    "type": "string",
                    "description": "Folder to scan (default: your home folder)"
                },
                "limit": {
                    "type": "integer",
                    "description": "How many duplicate groups to show (default 20, max 100)"
                }
            },
            "required": []
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_find_duplicates),
    });
    r.register(ToolDef {
        name: "copy_files".to_string(),
        desc: "Copy up to 50 files into one destination folder. Never overwrites: files \
              that already exist in the destination are skipped and reported. The \
              destination follows the same path rules as write_file."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "sources": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Absolute paths of the files to copy"
                },
                "dest_dir": {
                    "type": "string",
                    "description": "Destination folder (absolute or ~ based)"
                }
            },
            "required": ["sources", "dest_dir"]
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_copy_files),
    });
    r.register(ToolDef {
        name: "open_url".to_string(),
        desc: "Open a web URL (http or https) in a browser. By default the system's \
              default browser; pass browser (chrome, brave, firefox or edge) to target \
              a specific one, e.g. when the user says 'in Brave'. If that browser is \
              not installed the error says so - omit browser and retry, or tell the user."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to open, e.g. https://example.com"
                },
                "browser": {
                    "type": "string",
                    "description": "Optional: chrome, brave, firefox or edge. Omit for the system default."
                }
            },
            "required": ["url"]
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_open_url),
    });
    r.register(ToolDef {
        name: "browser_open".to_string(),
        desc: "Open a URL in an automation browser you control (Chrome DevTools \
              Protocol): launches a dedicated browser - chrome, then brave, \
              then edge; firefox is not supported - and returns the page \
              title with a short text preview. It launches headless unless \
              you pass headless false - do that when the user should see or \
              hear the page (music, video). Use this when you must read \
              the page or interact with it in later steps (browser_read, \
              browser_click). Use web_fetch for plain pages, and open_url \
              when the user just wants to see the site."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to open, e.g. https://example.com"
                },
                "browser": {
                    "type": "string",
                    "description": "Optional: chrome, brave or edge. Default: first installed of chrome, brave, edge."
                },
                "headless": {
                    "type": "boolean",
                    "description": "Optional: false shows the browser window (default true = headless)"
                }
            },
            "required": ["url"]
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_browser_open),
    });
    r.register(ToolDef {
        name: "browser_open_and_click".to_string(),
        desc: "Navigate the automation browser to a URL and click a target \
              - in one step. Give text (the first link/button whose text or \
              title contains it is clicked), or selector (exact CSS). For \
              YouTube search results, selector \"ytd-video-renderer \
              a#video-title\" always clicks the FIRST video whatever the \
              song's spelling. E.g. to play a song: open the YouTube \
              results?search_query=... URL and click the first video; \
              autoplay is allowed in this browser, but finish with \
              focus_window - Chrome defers playback in unfocused windows."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The URL to open, e.g. https://www.youtube.com/results?search_query=NAME"
                },
                "text": {
                    "type": "string",
                    "description": "Text to click: the first link/button whose text or title contains it"
                },
                "selector": {
                    "type": "string",
                    "description": "CSS selector to click; on YouTube search results 'ytd-video-renderer a#video-title' is the first video"
                },
                "browser": {
                    "type": "string",
                    "description": "Optional: chrome, brave or edge. Default: first installed of chrome, brave, edge."
                },
                "headless": {
                    "type": "boolean",
                    "description": "Optional: false shows the browser window and lets audio play out (default true = headless)"
                }
            }
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_browser_open_and_click),
    });
    r.register(ToolDef {
        name: "browser_read".to_string(),
        desc: "Read the page currently open in the automation browser: title, \
              URL and up to ~3000 characters of visible text. Call \
              browser_open first."
            .to_string(),
        schema: json!({ "type": "object", "properties": {} }),
        gated: false,
        plan: None,
        run: Arc::new(run_browser_read),
    });
    r.register(ToolDef {
        name: "browser_click".to_string(),
        desc: "Click on the page currently open in the automation browser \
              (call browser_open first): a CSS selector, text, or both - \
              with both, the clicked element must match the selector AND \
              contain the text (the safe way to say 'the link named X'). \
              Use it e.g. to click the first video of a YouTube search so \
              it starts playing; open the browser with headless false \
              first if the user should hear it."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "selector": {
                    "type": "string",
                    "description": "CSS selector; if text is also given, the element must match both"
                },
                "text": {
                    "type": "string",
                    "description": "Text to find: the clicked element's text or title must contain it"
                }
            }
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_browser_click),
    });
    r.register(ToolDef {
        name: "browser_vision_click".to_string(),
        desc: "Click a button or link by its SCREEN POSITION instead of a CSS \
               selector. Mirage takes a screenshot of the open page and locates \
               the element whose visible text or name best matches `text`; a \
               trusted mouse click is sent at its centre. Use this when \
               browser_click's selectors miss (YouTube markup changes). Give \
               text describing what to click, e.g. 'first video'. The result \
               states OPENED with the new title/URL, or NOT OPENED if nothing \
               happened. Optional x/y skips detection and clicks directly."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "text": {
                    "type": "string",
                    "description": "What to click, e.g. 'first video' or 'Sign in'"
                },
                "x": { "type": "number", "description": "Optional direct screenshot x coordinate" },
                "y": { "type": "number", "description": "Optional direct screenshot y coordinate" }
            }
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_browser_vision_click),
    });
    r.register(ToolDef {
        name: "browser_close".to_string(),
        desc: "Close the automation browser opened by browser_open and free \
              its memory. Safe to call when nothing is open."
            .to_string(),
        schema: json!({ "type": "object", "properties": {} }),
        gated: false,
        plan: None,
        run: Arc::new(run_browser_close),
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
        gated: false,
        plan: None,
        run: Arc::new(run_media_key),
    });
    r.register(ToolDef {
        name: "sys_stats".to_string(),
        desc: "Get a system status snapshot: CPU usage, RAM usage, free disk space on the \
              system drive, battery level and power source, and uptime. Read-only."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: false,
        plan: None,
        run: Arc::new(run_sys_stats),
    });
    r.register(ToolDef {
        name: "process_stats".to_string(),
        desc: "List the top system processes by memory or CPU usage, measured live over \
              a 400 ms window. Deterministic ranking with sizes and percentages \
              computed by the tool."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "sort": {
                    "type": "string",
                    "enum": ["memory", "cpu"],
                    "description": "Rank by memory (default) or cpu"
                },
                "limit": {
                    "type": "integer",
                    "description": "How many processes to show (default 10, max 50)"
                }
            },
            "required": []
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_process_stats),
    });
    r.register(ToolDef {
        name: "disk_stats".to_string(),
        desc: "Report total, free and used space for every ready local drive. Call this \
              instead of estimating disk capacity from memory."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: false,
        plan: None,
        run: Arc::new(run_disk_stats),
    });
    r.register(ToolDef {
        name: "list_windows".to_string(),
        desc: "List the visible top-level windows in front-to-back (z) order with each \
              window's process name and pid. Call this to see what is open before \
              acting on windows."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: false,
        plan: None,
        run: Arc::new(run_list_windows),
    });
    r.register(ToolDef {
        name: "focus_window".to_string(),
        desc: "Bring a window to the foreground. Match by title or application name \
              (e.g. 'Spotify', 'main.rs - Notepad'). Call list_windows first if you are \
              not sure of the exact title."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "description": "Window title or application name to focus"
                }
            },
            "required": ["target"]
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_focus_window),
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
        run: Arc::new(run_content_search),
    });
    r.register(ToolDef {
        name: "web_fetch".to_string(),
        desc: "Fetch a public web page and return its readable text (HTML converted \
              to plain text; JSON/other text returned as-is; size-capped). The \
              result begins with Source/Retrieved/Last-Modified lines — carry \
              the source URL and retrieval date into your answer, and prefer \
              official project domains (docs, release notes, GitHub) for \
              version or release questions; if sources conflict, say so \
              instead of picking one silently. Use for 'summarize this URL', \
              'read this page', or to read a result found by web_search. \
              Binary files are refused — use download_file for those."
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
        gated: false,
        plan: None,
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
        gated: false,
        plan: None,
        run: Arc::new(run_download_file),
    });
    r.register(ToolDef {
        name: "web_search".to_string(),
        desc: "Search the public web with DuckDuckGo. Call this tool for ANY request \
              phrased like 'search the web for ...', 'look up ... online', 'find ... \
              on the internet', or whenever current/live information beyond your \
              training is needed (news, versions, prices, docs). The header states \
              the retrieval date — cite it and the source URLs in your answer; \
              for version or release questions, prefer fetching the project's \
              official site or GitHub over third-party snippets. Returns ranked \
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
        gated: false,
        plan: None,
        run: Arc::new(run_web_search),
    });
    r.register(ToolDef {
        name: "github_repo_info".to_string(),
        desc: "Facts about one public GitHub repository, straight from the GitHub \
               API: description, language, stars, forks, open issues, license, \
               default branch, last push, and the latest release. Call this for \
               'how many stars does X have', framework/library comparisons, and \
               version questions - it is more reliable than web_search snippets \
               and costs no page fetch. repo takes owner/name or a github.com \
               repository URL."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "repo": {
                    "type": "string",
                    "description": "owner/name or a github.com repository URL"
                }
            },
            "required": ["repo"]
        }),
        gated: false,
        plan: None,
        run: Arc::new(run_github_repo_info),
    });
    /* v2.5 — gated actions (goal.md): destructive actions that only run
     * after a confirmed re-call (crate::gating). shutdown_abort is the
     * UNGATED safety counterpart: canceling is always safe. */
    r.register(ToolDef {
        name: "close_app".to_string(),
        desc: "Close one of the allowed desktop applications (notepad, calc, chrome, \
              spotify, vscode). Windows get a close request first; anything still \
              open after a short wait is force-closed — except notepad, which is \
              never force-closed (it may hold unsaved documents)."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "app": {
                    "type": "string",
                    "enum": ["notepad", "calc", "chrome", "spotify", "vscode"],
                    "description": "Which app to close"
                }
            },
            "required": ["app"]
        }),
        gated: true,
        plan: Some(Arc::new(plan_close_app)),
        run: Arc::new(run_close_app),
    });
    r.register(ToolDef {
        name: "delete_file".to_string(),
        desc: "Move a single file to the Recycle Bin (recoverable from there). Use \
              for 'delete the file X' requests. Folders are refused; system \
              locations, program folders, the profile root itself and this app's \
              own directory are protected."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File to delete (absolute, or ~ based)"
                }
            },
            "required": ["path"]
        }),
        gated: true,
        plan: Some(Arc::new(plan_delete_file)),
        run: Arc::new(run_delete_file),
    });
    r.register(ToolDef {
        name: "recycle_bin".to_string(),
        desc: "List the Recycle Bin: one line per item with a 1-based index, \
               original name, size, deletion time and original location, \
               newest first. Read-only; use it to answer 'what is in the \
               recycle bin' and to pick indices before empty_recycle_bin."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: false,
        plan: None,
        run: Arc::new(run_recycle_bin),
    });
    r.register(ToolDef {
        name: "empty_recycle_bin".to_string(),
        desc: "Permanently remove Recycle Bin items — there is no undo. \
               action=delete removes just the indices given (take them \
               from the recycle_bin listing); action=empty removes every \
               item. Gated: nothing is removed until the user confirms \
               the exact list."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["delete", "empty"],
                    "description": "delete = only the indices given; empty = every item"
                },
                "indices": {
                    "type": "array",
                    "items": {"type": "integer", "minimum": 1},
                    "description": "delete only: 1-based indices from the recycle_bin listing"
                }
            },
            "required": ["action"]
        }),
        gated: true,
        plan: Some(Arc::new(plan_empty_recycle_bin)),
        run: Arc::new(run_empty_recycle_bin),
    });
    r.register(ToolDef {
        name: "power_mode".to_string(),
        desc: "Switch the Windows power plan: saver, balanced, or performance. \
              Use for 'switch to high performance' / 'power saving mode' requests."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["saver", "balanced", "performance"],
                    "description": "Which power plan to switch to"
                }
            },
            "required": ["mode"]
        }),
        gated: true,
        plan: Some(Arc::new(plan_power_mode)),
        run: Arc::new(run_power_mode),
    });
    r.register(ToolDef {
        name: "shutdown".to_string(),
        desc: "Shut the PC down in 30 seconds. All open apps close; unsaved work \
              is lost. The countdown can be canceled with shutdown_abort."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: true,
        plan: Some(Arc::new(plan_shutdown)),
        run: Arc::new(run_shutdown),
    });
    r.register(ToolDef {
        name: "shutdown_abort".to_string(),
        desc: "Cancel a pending shutdown countdown. Harmless when no shutdown is \
              pending. Use it when the user says 'abort the shutdown' or changes \
              their mind after a shutdown was announced."
            .to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        gated: false,
        plan: None,
        run: Arc::new(run_shutdown_abort),
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
                /* Config tools are always ungated, and they must not
                 * shadow a gated built-in name (that would silently
                 * un-gate a destructive action). */
                r.register_ungated_guarded(def.clone());
            }
        }
        r
    });
    &REG
}
