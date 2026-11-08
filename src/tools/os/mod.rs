//! Windows control tools for IGRIS AI.

pub use self::filetools::{FileSearchTool, FileTrashTool};
pub use self::systemui::{BrightnessControlTool, InputAutomationTool, WindowControlTool};

use crate::tools::{Capability, Tool, ToolError, ToolErrorKind, ToolOutcome, ToolResult};
use futures::future::{BoxFuture, FutureExt};
use serde_json::Value;

pub mod filetools;
pub mod systemui;

#[derive(Debug, Clone)]
pub struct PsOutput {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
    pub exit_code: i32,
}

pub async fn run_powershell(script: &str, timeout_secs: u64) -> Result<PsOutput, ToolError> {
    let timeout = std::time::Duration::from_secs(timeout_secs.max(1));
    let script = script.to_string();
    let fut = tokio::process::Command::new("powershell")
        .arg("-NoProfile")
        .arg("-NonInteractive")
        .arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-Command")
        .arg(&script)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(out)) => Ok(PsOutput {
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            success: out.status.success(),
            exit_code: out.status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => Err(ToolError::failed(format!(
            "failed to start powershell: {e}"
        ))),
        Err(_) => Err(ToolError::new(
            ToolErrorKind::Timeout,
            "powershell timed out",
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VolAction {
    Get,
    Set,
    Up,
    Down,
    Mute,
    Unmute,
}

impl VolAction {
    fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "get" => Some(Self::Get),
            "set" => Some(Self::Set),
            "up" => Some(Self::Up),
            "down" => Some(Self::Down),
            "mute" => Some(Self::Mute),
            "unmute" => Some(Self::Unmute),
            _ => None,
        }
    }
}

fn tap_vk(vk: u32, times: usize) -> String {
    let mut s = String::new();
    s += "Add-Type -Namespace K -Name U -MemberDefinition '[DllImport(\"user32.dll\")] public static extern void keybd_event(byte bVk,byte bScan,uint dwFlags,UIntPtr dwExtraInfo);';\n";
    for _ in 0..times.max(1) {
        s.push_str(&format!(
            "[K.U]::keybd_event({vk},0,0,[UIntPtr]::Zero); [K.U]::keybd_event({vk},0,2,[UIntPtr]::Zero); Start-Sleep -Milliseconds 15;\n"
        ));
    }
    s
}
pub struct VolumeControlTool;

impl Tool for VolumeControlTool {
    fn name(&self) -> &str {
        "volume_control"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Volume: get, set <0-100>, up, down, mute, unmute (media-key based MVP)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": { "type": "string" },
                "level": { "type": "number" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let action_raw = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let level = args.get("level").and_then(|v| v.as_i64());
        async move {
            let action = VolAction::parse(&action_raw)
                .ok_or_else(|| ToolError::invalid_args(format!("unknown action: {action_raw}")))?;
            #[cfg(target_os = "windows")]
            {
                let script = match action {
                    VolAction::Get => {
                        return Ok(ToolOutcome::plain(
                            "volume level is not enumerable in media-key mode (use set/up/down/mute)",
                        ));
                    }
                    VolAction::Set => {
                        let level = level.ok_or_else(|| {
                            ToolError::invalid_args("level is required for set".to_string())
                        })?;
                        if !(0..=100).contains(&level) {
                            return Err(ToolError::invalid_args("level must be 0..=100".to_string()));
                        }
                        if level == 0 {
                            tap_vk(173, 1)
                        } else {
                            let presses = ((level / 10).max(1)) as usize;
                            tap_vk(175, presses)
                        }
                    }
                    VolAction::Up => tap_vk(175, 3),
                    VolAction::Down => tap_vk(174, 3),
                    VolAction::Mute => tap_vk(173, 1),
                    VolAction::Unmute => tap_vk(173, 1),
                };
                let out = run_powershell(&script, 5).await?;
                let echo = if matches!(action, VolAction::Set) {
                    format!("adjusted volume (level target ~{})", level.unwrap_or(0))
                } else {
                    "done".to_string()
                };
                if out.success {
                    Ok(ToolOutcome::plain(echo))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(ToolError::failed(if msg.is_empty() {
                        format!("volume command failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = level;
                Err(ToolError::failed("windows-only tool (v1)"))
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Media control (2.11)
// ---------------------------------------------------------------------------

pub struct MediaControlTool;

impl Tool for MediaControlTool {
    fn name(&self) -> &str {
        "media_control"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Media playback: play_pause, next, previous, stop (media keys)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": { "action": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .to_string();
        async move {
            let (vk, label) = match action.as_str() {
                "play_pause" | "play" | "pause" => (179u32, "play/pause"),
                "next" => (176u32, "next"),
                "previous" | "prev" => (177u32, "previous"),
                "stop" => (178u32, "stop"),
                _ => return Err(ToolError::invalid_args(format!("unknown action: {action}"))),
            };
            let script = tap_vk(vk, 1);
            #[cfg(target_os = "windows")]
            {
                let out = run_powershell(&script, 5).await?;
                if out.success {
                    Ok(ToolOutcome::plain(format!("media: {label}")))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(ToolError::failed(if msg.is_empty() {
                        format!("media command failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }
            #[cfg(not(target_os = "windows"))]
            Err(ToolError::failed("windows-only tool (v1)"))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Clipboard (2.8)
// ---------------------------------------------------------------------------

pub struct ClipboardReadTool;

impl Tool for ClipboardReadTool {
    fn name(&self) -> &str {
        "clipboard_read"
    }
    fn capability(&self) -> Capability {
        Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        "Read the clipboard (capped at 32 KB)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    fn execute<'a>(&'a self, _args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        async {
            #[cfg(target_os = "windows")]
            {
                let script = "Get-Clipboard -Raw".to_string();
                let out = run_powershell(&script, 5).await?;
                let mut text = out.stdout;
                if text.chars().count() > 32768 {
                    text = text.chars().take(32768).collect();
                }
                if text.trim().is_empty() && !out.success {
                    let msg = out.stderr.trim().to_string();
                    return Err(ToolError::failed(if msg.is_empty() {
                        format!("clipboard read failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }));
                }
                Ok(ToolOutcome::plain(format!("clipboard: {text}")))
            }
            #[cfg(not(target_os = "windows"))]
            Err(ToolError::failed("windows-only tool (v1)"))
        }
        .boxed()
    }
}

pub struct ClipboardWriteTool;

impl Tool for ClipboardWriteTool {
    fn name(&self) -> &str {
        "clipboard_write"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Write text to the clipboard."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["text"],
            "properties": { "text": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let text_owned = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        async move {
            if text_owned.is_empty() {
                return Err(ToolError::invalid_args("missing arg: text"));
            }
            let escaped = text_owned.replace('\'', "''");
            let script = format!(
                "Set-Clipboard -Value '{}'; Write-Output 'copied {} chars'",
                escaped,
                text_owned.chars().count()
            );
            #[cfg(target_os = "windows")]
            {
                let out = run_powershell(&script, 5).await?;
                if out.success {
                    Ok(ToolOutcome::plain(format!(
                        "clipboard: copied {} chars",
                        text_owned.chars().count()
                    )))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(ToolError::failed(if msg.is_empty() {
                        format!("clipboard write failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }
            #[cfg(not(target_os = "windows"))]
            Err(ToolError::failed("windows-only tool (v1)"))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Screenshot (2.7)
// ---------------------------------------------------------------------------

pub struct ScreenshotTool;

impl Tool for ScreenshotTool {
    fn name(&self) -> &str {
        "screenshot"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Capture the screen to %TEMP%\\igris\\screenshot-<ts>.png and return the path."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    fn execute<'a>(&'a self, _args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        async {
            let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S-%3f");
            let dir = std::env::temp_dir().join("igris");
            let path = dir.join(format!("screenshot-{}.png", ts));
            let path_str = path.display().to_string();
            let script = format!(
                "Add-Type -AssemblyName System.Drawing; $d = Join-Path $env:TEMP 'igris'; New-Item -ItemType Directory -Force -Path $d | Out-Null; $b = [System.Drawing.Bitmap]::new( [System.Drawing.Screen]::PrimaryScreen.Bounds.Width, [System.Drawing.Screen]::PrimaryScreen.Bounds.Height); $g=[System.Drawing.Graphics]::FromImage($b); $g.CopyFromScreen([System.Drawing.Point]::Empty,[System.Drawing.Point]::Empty,$b.Size); $b.Save('{}',[System.Drawing.Imaging.ImageFormat]::Png); $g.Dispose(); $b.Dispose(); Write-Output '{}'",
                path_str.replace('\'', "''"),
                path_str.replace('\'', "''")
            );
            #[cfg(target_os = "windows")]
            {
                let out = run_powershell(&script, 12).await?;
                if out.success {
                    let reported = if out.stdout.trim().is_empty() {
                        path_str.clone()
                    } else {
                        out.stdout.trim().to_string()
                    };
                    Ok(ToolOutcome::plain(format!("screenshot saved: {reported}")))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(ToolError::failed(if msg.is_empty() {
                        format!("screenshot failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = dir;
                let _ = path;
                Err(ToolError::failed("windows-only tool (v1)"))
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Power control (2.5) — Sensitive
// ---------------------------------------------------------------------------

pub struct PowerControlTool;

impl Tool for PowerControlTool {
    fn name(&self) -> &str {
        "power_control"
    }
    fn capability(&self) -> Capability {
        Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        "Lock, sleep, shutdown, or restart the workstation (approval required)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": { "action": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .to_string();
        async move {
            let action = match action.as_str() {
                "lock" | "sleep" | "shutdown" | "restart" => action.clone(),
                _ => return Err(ToolError::invalid_args(format!("unknown action: {action}"))),
            };
            let script = match action.as_str() {
                "lock" => "rundll32.exe user32.dll,LockWorkStation".to_string(),
                "sleep" => "rundll32.exe powrprof.dll,SetSuspendState 0,1,0".to_string(),
                "shutdown" => "shutdown /s /t 30".to_string(),
                "restart" => "shutdown /r /t 30".to_string(),
                _ => unreachable!(),
            };
            #[cfg(target_os = "windows")]
            {
                let out = run_powershell(&script, 8).await?;
                if out.success {
                    Ok(ToolOutcome::plain(format!("power: {action} initiated")))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(ToolError::failed(if msg.is_empty() {
                        format!("power command failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }
            #[cfg(not(target_os = "windows"))]
            Err(ToolError::failed("windows-only tool (v1)"))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Browser control (2.13)
// ---------------------------------------------------------------------------

pub struct BrowserControlTool;

impl Tool for BrowserControlTool {
    fn name(&self) -> &str {
        "browser_control"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Open a URL or run a web search in the default browser (http/https only)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string" },
                "url": { "type": "string" },
                "query": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("open_url")
            .trim()
            .to_lowercase()
            .to_string();
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        async move {
            let target = if action == "search" {
                if query.is_empty() {
                    return Err(ToolError::invalid_args("missing arg: query"));
                }
                format!(
                    "https://duckduckgo.com/?q={}",
                    crate::tools::urlencoding_simple(&query)
                )
            } else {
                if url.is_empty() {
                    return Err(ToolError::invalid_args("missing arg: url"));
                }
                let lower = url.to_lowercase();
                if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                    return Err(ToolError::invalid_args(
                        "browser_control only accepts http/https URLs",
                    ));
                }
                // Reject URLs missing a host or containing userinfo/separators
                // that Start-Process/PowerShell may mis-handle. The later control-
                // char check hardens the interpolation, but these checks keep
                // the model honest.
                let rest = &url[7.min(url.len())..]; // heuristic; harmless
                let host_part = rest.split('/').next().unwrap_or("");
                let host_part = host_part.split('?').next().unwrap_or(host_part);
                if host_part.is_empty() || host_part.contains('@') || host_part.contains(' ') {
                    return Err(ToolError::invalid_args("browser URL must include a host"));
                }
                if url
                    .chars()
                    .any(|c| (c as u32) < 0x20 || c == '\'' || c == '`')
                {
                    return Err(ToolError::invalid_args("unsafe url"));
                }
                url.clone()
            };
            if target
                .chars()
                .any(|c| (c as u32) < 0x20 || c == '\'' || c == '`')
            {
                return Err(ToolError::invalid_args("unsafe url"));
            }
            let script = format!("Start-Process '{}'", target);
            #[cfg(target_os = "windows")]
            {
                let out = run_powershell(&script, 8).await?;
                if out.success {
                    Ok(ToolOutcome::plain(format!("opened: {target}")))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(ToolError::failed(if msg.is_empty() {
                        format!("open failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = target;
                Err(ToolError::failed("windows-only tool (v1)"))
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// App control (2.2)
// ---------------------------------------------------------------------------

const APP_ALLOWLIST: [&str; 14] = [
    "notepad", "calc", "mspaint", "explorer", "chrome", "firefox", "msedge", "edge", "brave",
    "code", "vscode", "spotify", "discord", "taskmgr",
];

pub struct AppControlTool;

impl Tool for AppControlTool {
    fn name(&self) -> &str {
        "app_control"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Launch, close, focus, or list applications (curated allowlist for launch)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": { "type": "string" },
                "app": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("launch")
            .trim()
            .to_lowercase()
            .to_string();
        let app_raw = args
            .get("app")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        async move {
            let action = if action.is_empty() { "launch".to_string() } else { action };
            match action.as_str() {
                "launch" => {
                    if app_raw.is_empty() {
                        return Err(ToolError::invalid_args("missing arg: app"));
                    }
                    let app_lower = app_raw.to_lowercase();
                    if !APP_ALLOWLIST.contains(&app_lower.as_str()) {
                        return Err(ToolError::invalid_args(format!(
                            "app '{app_raw}' is not in the app allowlist"
                        )));
                    }
                    #[cfg(target_os = "windows")]
                    {
                        let app = app_lower.clone();
                        let launched = tokio::task::spawn_blocking(move || {
                            std::process::Command::new("cmd")
                                .args(["/C", "start", "", &app])
                                .spawn()
                        })
                        .await;
                        match launched {
                            Ok(Ok(_)) => Ok(ToolOutcome::plain(format!("app_control: launched {app_lower}"))),
                            Ok(Err(e)) => Err(ToolError::failed(format!("launch failed: {e}"))),
                            Err(e) => Err(ToolError::failed(format!("spawn error: {e}"))),
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    Err(ToolError::failed("windows-only tool (v1)"))
                }
                "close" => {
                    if app_raw.is_empty() {
                        return Err(ToolError::invalid_args("missing arg: app"));
                    }
                    if app_raw
                        .chars()
                        .any(|c| (c as u32) < 0x20 || c == '\'' || c == '"' || c == ';' || c == '|' || c == '&' || c == '`' || c == '$')
                    {
                        return Err(ToolError::invalid_args("unsafe app name"));
                    }
                    let app = app_raw.clone();
                    let script = format!(
                        "$p = Get-Process -Name '{app}' -ErrorAction SilentlyContinue | Select-Object -First 1; if (-not $p) {{ throw 'not running: {app}' }}; $p.CloseMainWindow() | Out-Null; Write-Output 'closing: {app}'"
                    );
                    #[cfg(target_os = "windows")]
                    {
                        let out = run_powershell(&script, 8).await?;
                        if out.success {
                            Ok(ToolOutcome::plain(format!("app_control: close requested for {app}")))
                        } else {
                            let msg = out.stderr.trim().to_string();
                            Err(ToolError::failed(if msg.is_empty() {
                                format!("close failed with exit code {}", out.exit_code)
                            } else {
                                msg
                            }))
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    Err(ToolError::failed("windows-only tool (v1)"))
                }
                "focus" => {
                    if app_raw.is_empty() {
                        return Err(ToolError::invalid_args("missing arg: app"));
                    }
                    if app_raw
                        .chars()
                        .any(|c| (c as u32) < 0x20 || c == '\'' || c == '"' || c == ';' || c == '|' || c == '&' || c == '`' || c == '$')
                    {
                        return Err(ToolError::invalid_args("unsafe app name"));
                    }
                    let app = app_raw.clone();
                    let script = format!(
                        "Add-Type -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool SetForegroundWindow(IntPtr h);' -Name U -Namespace W; $p = Get-Process -Name '{app}' -ErrorAction SilentlyContinue | Where-Object {{ $_.MainWindowHandle -ne 0 }} | Select-Object -First 1; if (-not $p) {{ throw 'not found: {app}' }}; [W.U]::SetForegroundWindow($p.MainWindowHandle) | Out-Null; Write-Output 'focused: {app}'"
                    );
                    #[cfg(target_os = "windows")]
                    {
                        let out = run_powershell(&script, 8).await?;
                        if out.success {
                            Ok(ToolOutcome::plain(format!("app_control: focused {app}")))
                        } else {
                            let msg = out.stderr.trim().to_string();
                            Err(ToolError::failed(if msg.is_empty() {
                                format!("focus failed with exit code {}", out.exit_code)
                            } else {
                                msg
                            }))
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    Err(ToolError::failed("windows-only tool (v1)"))
                }
                "list" => {
                    #[cfg(target_os = "windows")]
                    {
                        let script = "Get-Process | Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 25 -ExpandProperty ProcessName | Out-String".to_string();
                        let out = run_powershell(&script, 8).await?;
                        if out.success {
                            let preview = if out.stdout.trim().is_empty() {
                                "(no windows)".to_string()
                            } else {
                                out.stdout.trim().to_string()
                            };
                            Ok(ToolOutcome::plain(format!("open windows:\n{preview}")))
                        } else {
                            let msg = out.stderr.trim().to_string();
                            Err(ToolError::failed(if msg.is_empty() {
                                format!("list failed with exit code {}", out.exit_code)
                            } else {
                                msg
                            }))
                        }
                    }
                    #[cfg(not(target_os = "windows"))]
                    Err(ToolError::failed("windows-only tool (v1)"))
                }
                other => Err(ToolError::invalid_args(format!("unknown action: {other}"))),
            }
        }
        .boxed()
    }
}
