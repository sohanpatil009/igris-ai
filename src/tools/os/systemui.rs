//! Window, brightness, and input-automation tools (plan.md 2.3/2.6/2.12).

use futures::future::{BoxFuture, FutureExt};

use super::run_powershell;
use crate::tools::{Capability, Tool, ToolError, ToolErrorKind, ToolOutcome, ToolResult};

type Value = serde_json::Value;

// ---------------------------------------------------------------------------
// Window control (2.3)
// ---------------------------------------------------------------------------

pub struct WindowControlTool;

impl WindowControlTool {
    fn is_safe(s: &str) -> bool {
        !s.is_empty()
            && !s.chars().any(|c| {
                (c as u32) < 0x20
                    || c == '\''
                    || c == '"'
                    || c == '`'
                    || c == ';'
                    || c == '|'
                    || c == '&'
                    || c == '$'
                    || c == '?'
                    || c == '['
                    || c == ']'
            })
    }

    fn ps_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }
}

impl Tool for WindowControlTool {
    fn name(&self) -> &str {
        "window_control"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Control a window: minimize, maximize, restore, snap_left, snap_right, move. Target by title substring or process name."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": { "type": "string" },
                "title": { "type": "string" },
                "process": { "type": "string" },
                "x": { "type": "number" },
                "y": { "type": "number" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let action_raw = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .to_string();
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let process = args
            .get("process")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let x = args.get("x").and_then(|v| v.as_f64());
        let y = args.get("y").and_then(|v| v.as_f64());

        async move {
            let action = match action_raw.as_str() {
                "minimize" | "maximize" | "restore" | "snap_left" | "snap_right" | "move" => {
                    action_raw.clone()
                }
                _ => return Err(ToolError::invalid_args(format!("unknown action: {action_raw}"))),
            };

            if title.is_empty() && process.is_empty() {
                return Err(ToolError::invalid_args(
                    "window_control requires title or process".to_string(),
                ));
            }
            if !title.is_empty() && !WindowControlTool::is_safe(&title) {
                return Err(ToolError::invalid_args("unsafe title".to_string()));
            }
            if !process.is_empty() && !WindowControlTool::is_safe(&process) {
                return Err(ToolError::invalid_args("unsafe process name".to_string()));
            }

            if action == "move" && (x.is_none() || y.is_none()) {
                return Err(ToolError::invalid_args(
                    "x and y are required for move".to_string(),
                ));
            }

            #[cfg(target_os = "windows")]
            {
                // Build the window-handle lookup expression (first matching window).
                let handle_expr = if !process.is_empty() && !title.is_empty() {
                    format!(
                        "$p = Get-Process -Name {proc} -ErrorAction SilentlyContinue | Where-Object {{ $_.MainWindowTitle -like '*{title}*' -and $_.MainWindowHandle -ne 0 }} | Select-Object -First 1; if (-not $p) {{ $p = Get-Process -Name {proc} -ErrorAction SilentlyContinue | Where-Object {{ $_.MainWindowHandle -ne 0 }} | Select-Object -First 1 }}",
                        proc = WindowControlTool::ps_quote(&process),
                        title = title.replace('\'', "''").replace('*', "")
                    )
                } else if !process.is_empty() {
                    format!(
                        "$p = Get-Process -Name {proc} -ErrorAction SilentlyContinue | Where-Object {{ $_.MainWindowHandle -ne 0 }} | Select-Object -First 1",
                        proc = WindowControlTool::ps_quote(&process)
                    )
                } else {
                    format!(
                        "$p = Get-Process | Where-Object {{ $_.MainWindowTitle -like '*{title}*' -and $_.MainWindowHandle -ne 0 }} | Select-Object -First 1",
                        title = title.replace('\'', "''").replace('*', "")
                    )
                };

                let win_api = "Add-Type -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool ShowWindow(IntPtr h, int n); [DllImport(\"user32.dll\")] public static extern bool MoveWindow(IntPtr h,int X,int Y,int w,int hh,bool r); [DllImport(\"user32.dll\")] public static extern bool GetWindowRect(IntPtr h, out System.Drawing.Rectangle rc);' -Name U -Namespace W;";

                let script = match action.as_str() {
                    "minimize" => format!(
                        "{handle}\nif (-not $p) {{ throw 'window not found' }}\n$h = $p.MainWindowHandle\n{api}\n[W.U]::ShowWindow($h, 6) | Out-Null\nWrite-Output 'minimized'",
                        handle = handle_expr,
                        api = win_api
                    ),
                    "maximize" => format!(
                        "{handle}\nif (-not $p) {{ throw 'window not found' }}\n$h = $p.MainWindowHandle\n{api}\n[W.U]::ShowWindow($h, 3) | Out-Null\nWrite-Output 'maximized'",
                        handle = handle_expr,
                        api = win_api
                    ),
                    "restore" => format!(
                        "{handle}\nif (-not $p) {{ throw 'window not found' }}\n$h = $p.MainWindowHandle\n{api}\n[W.U]::ShowWindow($h, 9) | Out-Null\nWrite-Output 'restored'",
                        handle = handle_expr,
                        api = win_api
                    ),
                    "snap_left" => {
                        let add_forms = "Add-Type -AssemblyName System.Windows.Forms; $wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea;";
                        format!(
                            "{handle}\nif (-not $p) {{ throw 'window not found' }}\n$h = $p.MainWindowHandle\n{forms}\n$half = [int]($wa.Width / 2)\n{api}\n[W.U]::MoveWindow($h, $wa.Left, $wa.Top, $half, $wa.Height, $true) | Out-Null\nWrite-Output 'snapped left'",
                            handle = handle_expr,
                            forms = add_forms,
                            api = win_api
                        )
                    }
                    "snap_right" => {
                        let add_forms = "Add-Type -AssemblyName System.Windows.Forms; $wa = [System.Windows.Forms.Screen]::PrimaryScreen.WorkingArea;";
                        format!(
                            "{handle}\nif (-not $p) {{ throw 'window not found' }}\n$h = $p.MainWindowHandle\n{forms}\n$half = [int]($wa.Width / 2)\n{api}\n[W.U]::MoveWindow($h, $wa.Left + $half, $wa.Top, $half, $wa.Height, $true) | Out-Null\nWrite-Output 'snapped right'",
                            handle = handle_expr,
                            forms = add_forms,
                            api = win_api
                        )
                    }
                    "move" => {
                        let xi = x.unwrap_or(0.0) as i32;
                        let yi = y.unwrap_or(0.0) as i32;
                        format!(
                            "{handle}\nif (-not $p) {{ throw 'window not found' }}\n$h = $p.MainWindowHandle\n$rc = New-Object System.Drawing.Rectangle\n{api}\n[W.U]::GetWindowRect($h, [ref]$rc) | Out-Null\n$w = $rc.Width\n$hh = $rc.Height\n[W.U]::MoveWindow($h, {xi}, {yi}, $w, $hh, $true) | Out-Null\nWrite-Output 'moved'",
                            handle = handle_expr,
                            api = win_api,
                            xi = xi,
                            yi = yi
                        )
                    }
                    _ => unreachable!(),
                };

                let out = run_powershell(&script, 8).await?;
                if out.success {
                    let label = match action.as_str() {
                        "minimize" => "window: minimized",
                        "maximize" => "window: maximized",
                        "restore" => "window: restored",
                        "snap_left" => "window: snapped left",
                        "snap_right" => "window: snapped right",
                        "move" => "window: moved",
                        _ => "window: done",
                    };
                    Ok(ToolOutcome::plain(label))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(ToolError::failed(if msg.is_empty() {
                        format!("window command failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }

            #[cfg(not(target_os = "windows"))]
            {
                let _ = (action, x, y);
                Err(ToolError::failed("windows-only tool (v1)"))
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Brightness control (2.6)
// ---------------------------------------------------------------------------

pub struct BrightnessControlTool;

impl Tool for BrightnessControlTool {
    fn name(&self) -> &str {
        "brightness_control"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Get or set display brightness via WMI (0-100)."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": { "type": "string" },
                "level": { "type": "number" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let action_raw = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .to_string();
        let level = args.get("level").and_then(|v| v.as_i64());

        async move {
            match action_raw.as_str() {
                "get" => {}
                "set" => {
                    let lvl = level.ok_or_else(|| {
                        ToolError::invalid_args("level is required for set".to_string())
                    })?;
                    if !(0..=100).contains(&lvl) {
                        return Err(ToolError::invalid_args("level must be 0..=100".to_string()));
                    }
                }
                _ => {
                    return Err(ToolError::invalid_args(format!(
                        "unknown action: {action_raw}"
                    )))
                }
            }

            #[cfg(target_os = "windows")]
            {
                let script = if action_raw == "get" {
                    "(Get-WmiObject -Namespace root/WMI -Class WmiMonitorBrightness).CurrentBrightness"
                        .to_string()
                } else {
                    let lvl = level.unwrap_or(0);
                    format!(
                        "$m = Get-WmiObject -Namespace root/WMI -Class WmiMonitorBrightnessMethods; $m.WmiSetBrightness(1, {lvl}) | Out-Null; Write-Output 'ok'",
                        lvl = lvl
                    )
                };

                let out = run_powershell(&script, 8).await?;
                let stderr_low = out.stderr.to_lowercase();
                if stderr_low.contains("not supported") || stderr_low.contains("0x80041002") {
                    return Ok(ToolOutcome::plain(
                        "brightness control not supported on this display".to_string(),
                    ));
                }
                if !out.success {
                    let msg = out.stderr.trim().to_string();
                    return Err(ToolError::failed(if msg.is_empty() {
                        format!("brightness command failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }));
                }
                if action_raw == "get" {
                    Ok(ToolOutcome::plain(format!(
                        "brightness: {}",
                        out.stdout.trim()
                    )))
                } else {
                    Ok(ToolOutcome::plain(format!(
                        "brightness set to {}",
                        level.unwrap_or(0)
                    )))
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
// Input automation (2.12) — Sensitive
// ---------------------------------------------------------------------------

pub struct InputAutomationTool;

impl InputAutomationTool {
    fn is_safe_text(s: &str) -> bool {
        !s.chars().any(|c| {
            let cp = c as u32;
            (cp < 0x09) || (cp > 0x0D && cp < 0x20) || cp == 0x7F
        })
    }
    fn ps_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }
}

impl Tool for InputAutomationTool {
    fn name(&self) -> &str {
        "input_automation"
    }
    fn capability(&self) -> Capability {
        Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        "Automate input: type_text (clipboard paste), press_keys, click. Approval required."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": { "type": "string" },
                "text": { "type": "string" },
                "keys": { "type": "string" },
                "x": { "type": "number" },
                "y": { "type": "number" },
                "button": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let action_raw = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .to_string();
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let keys = args
            .get("keys")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let x = args.get("x").and_then(|v| v.as_f64());
        let y = args.get("y").and_then(|v| v.as_f64());

        async move {
            match action_raw.as_str() {
                "type_text" => {
                    if text.is_empty() {
                        return Err(ToolError::invalid_args("missing arg: text".to_string()));
                    }
                    if !InputAutomationTool::is_safe_text(&text) {
                        return Err(ToolError::invalid_args("unsafe text".to_string()));
                    }
                }
                "press_keys" => {
                    if keys.is_empty() {
                        return Err(ToolError::invalid_args("missing arg: keys".to_string()));
                    }
                }
                "click" => {
                    if x.is_none() || y.is_none() {
                        return Err(ToolError::invalid_args(
                            "x and y are required for click".to_string(),
                        ));
                    }
                }
                _ => {
                    return Err(ToolError::invalid_args(format!(
                        "unknown action: {action_raw}"
                    )))
                }
            }

            #[cfg(target_os = "windows")]
            {
                let script = match action_raw.as_str() {
                    "type_text" => {
                        let quoted = InputAutomationTool::ps_quote(&text);
                        format!(
                            "Set-Clipboard -Value {q}; Start-Sleep -Milliseconds 60; Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait('^v'); Write-Output 'typed'",
                            q = quoted
                        )
                    }
                    "press_keys" => {
                        // Allow SendKeys metacharacters; only block control chars.
                        if keys.chars().any(|c| (c as u32) < 0x20) {
                            return Err(ToolError::invalid_args("unsafe keys".to_string()));
                        }
                        format!(
                            "Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.SendKeys]::SendWait({q}); Write-Output 'pressed'",
                            q = InputAutomationTool::ps_quote(&keys)
                        )
                    }
                    "click" => {
                        let xi = x.unwrap_or(0.0) as i32;
                        let yi = y.unwrap_or(0.0) as i32;
                        format!(
                            "Add-Type -MemberDefinition '[DllImport(\"user32.dll\")] public static extern bool SetCursorPos(int X,int Y); [DllImport(\"user32.dll\")] public static extern void mouse_event(uint f,uint dx,uint dy,uint c,UIntPtr e);' -Name M -Namespace I; [I.M]::SetCursorPos({xi},{yi}) | Out-Null; [I.M]::mouse_event(0x0002,0,0,0,[UIntPtr]::Zero); [I.M]::mouse_event(0x0004,0,0,0,[UIntPtr]::Zero); Write-Output 'clicked'",
                            xi = xi,
                            yi = yi
                        )
                    }
                    _ => unreachable!(),
                };

                let out = run_powershell(&script, 10).await?;
                if !out.success {
                    let msg = out.stderr.trim().to_string();
                    return Err(ToolError::failed(if msg.is_empty() {
                        format!("input automation failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }));
                }
                let label = match action_raw.as_str() {
                    "type_text" => format!("input: typed {} chars", text.chars().count()),
                    "press_keys" => "input: keys pressed".to_string(),
                    "click" => format!("input: clicked at {},{}", x.unwrap_or(0.0) as i32, y.unwrap_or(0.0) as i32),
                    _ => "input: done".to_string(),
                };
                Ok(ToolOutcome::plain(label))
            }

            #[cfg(not(target_os = "windows"))]
            {
                let _ = (text, keys, x, y);
                Err(ToolError::failed("windows-only tool (v1)"))
            }
        }
        .boxed()
    }
}
