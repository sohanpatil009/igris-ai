//! Gated action tools (v2.5, goal.md): destructive machine actions that
//! only execute after a confirmed re-call — see [`crate::gating`] for the
//! machinery and [`crate::tools::registry`] for the dispatch flow.
//!
//! - `close_app`       — close an allowed app: WM_CLOSE first (graceful),
//!   then force-kill whatever is still open after a short wait. Notepad
//!   is NEVER force-closed (it may hold unsaved documents).
//! - `delete_file`     — move a single file to the Recycle Bin
//!   (`SHFileOperationW` with `FOF_ALLOWUNDO`); system locations, program
//!   folders, the profile root itself and the app's own directory are
//!   protected.
//! - `shutdown`        — schedule a shutdown 30 s out (cancelable);
//! - `shutdown_abort`  — UNGATED safety counterpart: cancels a countdown,
//!   harmless when none is pending (the safe direction is always
//!   allowed).
//! - `power_mode`      — switch the Windows power plan via
//!   `powercfg /setactive` over the well-known scheme GUIDs.
//!
//! Safety rules for this family: the executors re-validate everything
//! the plan validated (defense in depth — the executor runs only after
//! user confirmation, but never trusts that fact); `shutdown` is never
//! executed by the lib tests (it would really power the machine down —
//! the abort tool is the only one allowed to spawn in tests, because
//! aborting is always safe); and the allowlists here are separate from
//! `run_command`'s — shutdown.exe and powercfg.exe are owned by these
//! dedicated tools alone.

use super::shared::{expand_home_path, required_str};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GetWindowThreadProcessId, IsWindowVisible, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

/* ================================================================ */
/* close_app                                                         */
/* ================================================================ */

/// Voice name → candidate process image names (lowercase, no path).
/// A voice name may map to several images across Windows versions
/// (calc: the modern calculator vs. the classic one).
const APP_PROCESSES: &[(&str, &[&str])] = &[
    ("notepad", &["notepad.exe"]),
    ("calc", &["calculator.exe", "calc.exe"]),
    ("chrome", &["chrome.exe"]),
    ("spotify", &["spotify.exe"]),
    ("vscode", &["code.exe"]),
];

/// Apps that are never force-killed blind: they can hold unsaved
/// documents, and the cost of losing those beats the cost of an app
/// that stays open and gets reported.
const NEVER_FORCE: &[&str] = &["notepad"];

fn app_entry(app: &str) -> Result<&'static [&'static str], String> {
    APP_PROCESSES
        .iter()
        .find(|(n, _)| *n == app)
        .map(|(_, images)| *images)
        .ok_or_else(|| format!("'{app}' is not an allowed app for close_app"))
}

/// Plan (validator + narrator): runs on the first call, never executes.
pub(crate) fn plan_close_app(args: &Value) -> Result<String, String> {
    let app = required_str(args, "app", "close_app")?;
    app_entry(app)?;
    if NEVER_FORCE.contains(&app) {
        Ok(format!(
            "Close {app}. Windows get a close request first; if any window \
             stays open (it may hold unsaved work) it will be left alone \
             and reported instead of force-closed."
        ))
    } else {
        Ok(format!(
            "Close {app}: its windows get a close request first; anything \
             still open after a short wait is force-closed (unsaved work \
             in {app} could be lost)."
        ))
    }
}

/// Executor: graceful WM_CLOSE sweep, bounded wait, then (except for
/// never-force apps) a force kill of whatever is left.
pub(crate) fn run_close_app(args: &Value) -> Result<String, String> {
    let app = required_str(args, "app", "close_app")?;
    let images = app_entry(app)?;
    close_app_images(app, images)
}

/// The close flow against an explicit image list — split out so the
/// unit tests can exercise it with a synthetic process name that is
/// guaranteed not to exist (no real app is ever touched by tests).
fn close_app_images(app: &str, images: &[&str]) -> Result<String, String> {
    let mut pids = matching_pids(images);
    if pids.is_empty() {
        return Ok(format!("{app} is not running."));
    }
    let mut windows_contacted = 0;
    for pid in &pids {
        windows_contacted += post_close_to_windows(*pid);
    }
    /* Bounded wait: 15 x 300 ms = 4.5 s, then decide. */
    for _ in 0..15 {
        std::thread::sleep(Duration::from_millis(300));
        pids = matching_pids(images);
        if pids.is_empty() {
            return Ok(format!(
                "{app} closed ({windows_contacted} window(s) got the close \
                 request)."
            ));
        }
    }
    if NEVER_FORCE.contains(&app) {
        return Err(format!(
            "{app} is still open — it may have unsaved documents, and it \
             is never force-closed. Close it manually if you want it gone."
        ));
    }
    let mut killed = 0;
    for pid in &pids {
        if force_kill(*pid) {
            killed += 1;
        }
    }
    if killed == 0 {
        return Err(format!("{app} could not be closed (nothing terminated)."));
    }
    Ok(format!(
        "{app} closed (force-closed {killed} remaining process(es) after \
         the close request went unanswered)."
    ))
}

/// Process snapshot scan: every running PID whose image name matches.
fn matching_pids(images: &[&str]) -> Vec<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let want: Vec<String> = images.iter().map(|s| s.to_lowercase()).collect();
    let mut out = Vec::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W::default();
        entry.dwSize = core::mem::size_of::<PROCESSENTRY32W>() as u32;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase();
                if want.contains(&name) {
                    out.push(entry.th32ProcessID);
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    out
}

/// Window enumeration state passed through the LPARAM: which process we
/// are collecting for, and the visible non-tool top-level windows found.
struct WindowCollect {
    pid: u32,
    out: Vec<windows::Win32::Foundation::HWND>,
}

unsafe extern "system" fn collect_windows_of_pid(
    hwnd: windows::Win32::Foundation::HWND,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::core::BOOL {
    let collect = unsafe { &mut *(lparam.0 as *mut WindowCollect) };
    let mut wpid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut wpid));
        if wpid == collect.pid && IsWindowVisible(hwnd).as_bool() {
            /* skip tool windows (taskbar buddies etc.) */
            let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            if exstyle & (WS_EX_TOOLWINDOW.0 as isize) == 0 {
                collect.out.push(hwnd);
            }
        }
    }
    true.into()
}

/// Deliver WM_CLOSE to every visible top-level window of one process;
/// returns how many windows were contacted. This is the graceful path —
/// the app may ask "save changes?" and stay open, which the wait loop
/// then reports instead of killing.
fn post_close_to_windows(pid: u32) -> usize {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, PostMessageW, WM_CLOSE,
    };

    let mut collect = WindowCollect {
        pid,
        out: Vec::new(),
    };
    unsafe {
        let _ = EnumWindows(
            Some(collect_windows_of_pid),
            LPARAM(&mut collect as *mut WindowCollect as isize),
        );
        let n = collect.out.len();
        for hwnd in &collect.out {
            let _ = PostMessageW(Some(*hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        n
    }
}

/// Terminate one process; false when it cannot be opened or resists.
fn force_kill(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) else {
            return false;
        };
        let ok = TerminateProcess(h, 1).is_ok();
        let _ = CloseHandle(h);
        ok
    }
}

/* ================================================================ */
/* delete_file                                                       */
/* ================================================================ */

/// Subtrees that must never be touched (denied for anything under
/// them, including the root itself).
fn protected_subtrees() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for var in ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
        if let Some(v) = std::env::var_os(var) {
            out.push(PathBuf::from(v));
        }
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            out.push(dir.to_path_buf());
        }
    }
    out
}

/// The profile root is denied for the root itself only — files under it
/// (Documents, Desktop, Downloads) are exactly what the user deletes on
/// purpose.
fn profile_root() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// Drive roots ("C:\") are denied: no target that short is ever a file
/// the user named on purpose.
fn is_drive_root(path: &Path) -> bool {
    let Some(s) = path.to_str() else { return false };
    let bytes = s.as_bytes();
    bytes.len() <= 3 && bytes.len() >= 2 && bytes[1] == b':'
}

/// The protection check, shared by plan and executor.
fn check_protected(path: &Path) -> Result<(), String> {
    if is_drive_root(path) {
        return Err(format!("refusing to delete a drive root ({})", path.display()));
    }
    let norm = |p: &Path| -> String {
        let s = p.to_string_lossy().to_lowercase();
        let s = s.trim_end_matches('\\');
        s.to_string()
    };
    let target = norm(path);
    for prefix in protected_subtrees() {
        let prefix = norm(&prefix);
        if target == prefix || target.starts_with(&format!("{prefix}\\")) {
            return Err(format!(
                "{} is in a protected location — it can never be deleted",
                path.display()
            ));
        }
    }
    if let Some(home) = profile_root() {
        if target == norm(&home) {
            return Err(format!(
                "{} is the user profile root — it can never be deleted",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Plan (validator + narrator): existence, file-ness, protection.
pub(crate) fn plan_delete_file(args: &Value) -> Result<String, String> {
    let path = delete_target(args)?;
    check_protected(&path)?;
    Ok(format!(
        "Move {} to the Recycle Bin (recoverable from there).",
        path.display()
    ))
}

/// Executor: Recycle Bin move via SHFileOperationW. Re-validates
/// everything the plan validated.
pub(crate) fn run_delete_file(args: &Value) -> Result<String, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE,
        SHFILEOPSTRUCTW,
    };

    let path = delete_target(args)?;
    check_protected(&path)?;
    let display = path.display().to_string();

    /* pFrom must be double-null-terminated. */
    let mut from: Vec<u16> = path.as_os_str().encode_wide().collect();
    from.push(0);
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: HWND(std::ptr::null_mut()),
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        pTo: PCWSTR::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        fAnyOperationsAborted: windows::core::BOOL(0),
        hNameMappings: std::ptr::null_mut(),
        lpszProgressTitle: PCWSTR::null(),
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 {
        return Err(format!(
            "the Recycle Bin move failed (code {code}) for {display}"
        ));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err(format!("the Recycle Bin move was aborted for {display}"));
    }
    if path.exists() {
        return Err(format!("{display} still exists after the delete attempt"));
    }
    Ok(format!("Moved {display} to the Recycle Bin."))
}

/// Resolve the `path` argument: ~ expansion, existence, file-ness.
fn delete_target(args: &Value) -> Result<PathBuf, String> {
    let raw = required_str(args, "path", "delete_file")?;
    let path = PathBuf::from(expand_home_path(raw));
    if path.as_os_str().is_empty() {
        return Err("path is empty".to_string());
    }
    if !path.exists() {
        return Err(format!("file not found: {}", path.display()));
    }
    if path.is_dir() {
        return Err(format!(
            "{} is a folder — delete_file only removes single files",
            path.display()
        ));
    }
    Ok(path)
}

/* ================================================================ */
/* shutdown + shutdown_abort + power_mode                            */
/* ================================================================ */

/// Absolute path of a System32 utility (the allowlist for these names
/// is owned here, never by run_command).
fn system32_exe(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(name)
}

/// Run a System32 utility to completion and return its combined text
/// output (these tools exit in milliseconds, so the pipe lifetime is
/// not a BUG-010-style hazard).
fn run_system32(exe: &Path, args: &[&str]) -> Result<(bool, String), String> {
    if !exe.exists() {
        return Err(format!("{} not found", exe.display()));
    }
    let out = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not run {}: {e}", exe.display()))?;
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.success(), text))
}

pub(crate) fn plan_shutdown(_args: &Value) -> Result<String, String> {
    Ok(
        "Shut the PC down in 30 seconds — all open apps will close and \
         unsaved work will be lost. The countdown can be canceled with \
         shutdown_abort."
            .to_string(),
    )
}

pub(crate) fn run_shutdown(_args: &Value) -> Result<String, String> {
    let exe = system32_exe("shutdown.exe");
    let (ok, text) = run_system32(&exe, &["/s", "/t", "30"])?;
    if !ok {
        return Err(format!("shutdown.exe failed: {}", text.trim()));
    }
    Ok("Shutdown scheduled for 30 seconds from now. Say 'abort the \
        shutdown' to cancel it."
        .to_string())
}

/// UNGATED: cancels a pending shutdown countdown. The safe direction is
/// always allowed, and when no shutdown is pending the result is a
/// harmless note rather than an error.
pub(crate) fn run_shutdown_abort(_args: &Value) -> Result<String, String> {
    let exe = system32_exe("shutdown.exe");
    let (ok, text) = run_system32(&exe, &["/a"])?;
    let no_shutdown = text.contains("1116") || text.to_lowercase().contains("no shutdown");
    if ok {
        Ok("Shutdown canceled — the PC stays on.".to_string())
    } else if no_shutdown {
        Ok("No shutdown was in progress — nothing to cancel.".to_string())
    } else {
        Err(format!("shutdown.exe /a failed: {}", text.trim()))
    }
}

/// Well-known power scheme GUIDs (present on every Windows install).
const POWER_SAVER: &str = "a1841308-3541-42a9-bfd1-a4e5e19da1f4";
const POWER_BALANCED: &str = "381b4222-f694-41f0-9685-ff5bb260df2e";
const POWER_PERFORMANCE: &str = "8c5e7fda-e8bf-4a96-9a85-a6e23a8c635c";

fn power_scheme(mode: &str) -> Result<(&'static str, &'static str), String> {
    match mode {
        "saver" => Ok(("Power saver", POWER_SAVER)),
        "balanced" => Ok(("Balanced", POWER_BALANCED)),
        "performance" => Ok(("High performance", POWER_PERFORMANCE)),
        other => Err(format!(
            "'{other}' is not a power mode (saver, balanced, performance)"
        )),
    }
}

pub(crate) fn plan_power_mode(args: &Value) -> Result<String, String> {
    let mode = required_str(args, "mode", "power_mode")?;
    let (label, guid) = power_scheme(mode)?;
    Ok(format!("Switch the power plan to {label} ({guid})."))
}

pub(crate) fn run_power_mode(args: &Value) -> Result<String, String> {
    let mode = required_str(args, "mode", "power_mode")?;
    let (label, guid) = power_scheme(mode)?;
    let exe = system32_exe("powercfg.exe");
    let (ok, text) = run_system32(&exe, &["/setactive", guid])?;
    if !ok {
        return Err(format!("powercfg /setactive failed: {}", text.trim()));
    }
    Ok(format!("Power plan set to {label}."))
}

/* ================================================================ */
/* tests                                                             */
/* ================================================================ */

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_root(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "igris_gated_{tag}_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /* ---- close_app ---- */

    #[test]
    fn close_app_plans_grace_vs_never_force_wording() {
        let notepad = plan_close_app(&json!({"app": "notepad"})).unwrap();
        assert!(notepad.contains("Close notepad"), "{notepad}");
        assert!(notepad.contains("left alone"), "{notepad}");
        let chrome = plan_close_app(&json!({"app": "chrome"})).unwrap();
        assert!(chrome.contains("Close chrome"), "{chrome}");
        assert!(chrome.contains("force-closed"), "{chrome}");
        assert!(
            plan_close_app(&json!({"app": "terminal"})).is_err(),
            "outside the allowlist"
        );
        assert!(plan_close_app(&json!({})).is_err());
        assert!(plan_close_app(&json!({"app": 5})).is_err());
    }

    #[test]
    fn close_app_on_a_not_running_synthetic_app_is_a_note() {
        /* A synthetic image name that cannot exist: proves the
         * empty-snapshot path without ever touching a real app. */
        let out = close_app_images("probe", &["definitely-not-running-xyz-igris.exe"]).unwrap();
        assert!(out.contains("not running"), "{out}");
    }

    #[test]
    fn matching_pids_finds_the_test_process_by_image() {
        let exe = std::env::current_exe().unwrap();
        let image = exe.file_name().unwrap().to_string_lossy().to_lowercase();
        let pids = matching_pids(&[image.as_str()]);
        assert!(
            pids.contains(&std::process::id()),
            "the snapshot scan must find this very process"
        );
    }

    /* ---- delete_file ---- */

    #[test]
    fn delete_target_resolves_and_validates() {
        let root = temp_root("target");
        let file = root.join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let out = delete_target(&json!({"path": file.to_string_lossy()})).unwrap();
        assert_eq!(out, file);
        /* missing file */
        assert!(delete_target(&json!({"path": root.join("nope.txt").to_string_lossy()})).is_err());
        /* directory is refused */
        assert!(delete_target(&json!({"path": root.to_string_lossy()})).is_err());
        /* empty string */
        assert!(delete_target(&json!({"path": "   "})).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn protected_locations_are_refused() {
        let root = temp_root("protected");
        std::fs::write(root.join("f.txt"), "x").unwrap();
        check_protected(&root.join("f.txt")).unwrap(); /* temp dir is fine */
        for p in [
            "C:\\Windows\\Temp\\x.txt",
            "C:\\Program Files\\x.txt",
            "C:\\Program Files (x86)\\x.txt",
            "C:\\ProgramData\\x.txt",
            "C:\\",
        ] {
            let path = PathBuf::from(p);
            let err = check_protected(&path).unwrap_err();
            assert!(err.contains("protected") || err.contains("drive root"), "{err}");
        }
        /* the profile root itself, exactly */
        if let Some(home) = profile_root() {
            assert!(check_protected(&home).is_err(), "profile root denied");
        }
        /* the app's own directory subtree */
        let exe_dir = std::env::current_exe().unwrap().parent().unwrap().join("f.txt");
        assert!(check_protected(&exe_dir).is_err(), "exe dir denied");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_file_plan_announces_the_recycle_bin() {
        let root = temp_root("plan");
        let file = root.join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let plan = plan_delete_file(&json!({"path": file.to_string_lossy()})).unwrap();
        assert!(plan.contains("Recycle Bin"), "{plan}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_file_executor_recycles_a_temp_file() {
        let root = temp_root("recycle");
        let file = root.join("doomed.txt");
        std::fs::write(&file, "doomed").unwrap();
        let out = run_delete_file(&json!({"path": file.to_string_lossy()})).unwrap();
        assert!(out.contains("Recycle Bin"), "{out}");
        assert!(!file.exists(), "the file must be gone from its folder");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /* ---- shutdown / shutdown_abort / power_mode ---- */

    #[test]
    fn shutdown_plan_announces_the_countdown() {
        let plan = plan_shutdown(&json!({})).unwrap();
        assert!(plan.contains("30 seconds"), "{plan}");
        assert!(plan.contains("shutdown_abort"), "{plan}");
    }

    #[test]
    fn shutdown_abort_is_always_safe_to_run() {
        /* /a with no pending shutdown is a harmless note; if one WERE
         * pending, aborting keeps the PC on — safe either way. */
        let out = run_shutdown_abort(&json!({})).unwrap();
        assert!(
            out.contains("canceled") || out.contains("No shutdown"),
            "{out}"
        );
    }

    #[test]
    fn power_mode_maps_the_well_known_schemes() {
        let (label, guid) = power_scheme("saver").unwrap();
        assert_eq!(guid, POWER_SAVER);
        assert_eq!(label, "Power saver");
        assert_eq!(power_scheme("balanced").unwrap().1, POWER_BALANCED);
        assert_eq!(power_scheme("performance").unwrap().1, POWER_PERFORMANCE);
        assert!(power_scheme("turbo").is_err());
        let plan = plan_power_mode(&json!({"mode": "performance"})).unwrap();
        assert!(plan.contains("High performance"), "{plan}");
        assert!(plan_power_mode(&json!({})).is_err());
        assert!(plan_power_mode(&json!({"mode": 7})).is_err());
    }
}
