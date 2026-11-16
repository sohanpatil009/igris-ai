//! Window tools (goal.md v3 batch 1): `list_windows` · `focus_window`.
//!
//! `list_windows` hands the model the desktop's window inventory in
//! z-order (front first) with pid → exe resolution, so it never has to
//! guess what is open. `focus_window` ranks candidates by match quality
//! and brings the winner forward, verifying the result instead of
//! claiming success. Both are ungated: focusing a window is less
//! invasive than the already-ungated `open_app`.

use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MENU,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow,
    GWL_EXSTYLE, SW_RESTORE, WS_EX_TOOLWINDOW,
};

use super::shared::required_str;
use crate::tools::system::enumerate_processes;

/// How many rows `list_windows` shows at most — enough for the model to
/// see what is open without dumping half the desktop into context.
const MAX_WINDOWS_SHOWN: usize = 15;

/// One window straight from EnumWindows: handle, owner pid and title,
/// still in z-order (front first).
struct WinLite {
    hwnd: isize,
    pid: u32,
    title: String,
}

/// A resolved row: the same window plus its process image name.
/// Kept `Copy`-free because titles/exes are owned strings; the pure
/// helpers below only borrow.
struct WinRow {
    hwnd: isize,
    pid: u32,
    exe: String,
    title: String,
}

/* ================================================================ */
/* list_windows                                                      */
/* ================================================================ */

/// EnumWindows callback: keep every visible, titled, non-toolwindow
/// top-level window. EnumWindows delivers in z-order (front first) and
/// we preserve that order — `focus_window` relies on it.
unsafe extern "system" fn collect_top_windows(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<WinLite>) };
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return true.into();
        }
        if GetWindowTextLengthW(hwnd) <= 0 {
            return true.into();
        }
        /* skip tool windows (taskbar buddies etc.), same rule as close_app */
        let exstyle = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if exstyle & (WS_EX_TOOLWINDOW.0 as isize) != 0 {
            return true.into();
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return true.into();
        }
        let mut buf = [0u16; 512];
        let copied = GetWindowTextW(hwnd, &mut buf);
        let n = (copied.max(0) as usize).min(buf.len());
        let title = String::from_utf16_lossy(&buf[..n]);
        out.push(WinLite {
            hwnd: hwnd.0 as isize,
            pid,
            title,
        });
    }
    true.into()
}

/// All top-level windows matching the filter, in z-order, with the
/// owner process image name resolved through one Toolhelp snapshot.
fn top_windows() -> Result<Vec<WinRow>, String> {
    let mut lites: Vec<WinLite> = Vec::new();
    /* SAFETY: lparam carries a pointer to our own local Vec; the
     * callback only pushes owned data. */
    unsafe {
        EnumWindows(
            Some(collect_top_windows),
            LPARAM(&mut lites as *mut Vec<WinLite> as isize),
        )
        .map_err(|e| format!("list_windows: EnumWindows failed: {e}"))?;
    }
    let procs: HashMap<u32, String> = enumerate_processes()?.into_iter().collect();
    Ok(lites
        .into_iter()
        .map(|l| WinRow {
            hwnd: l.hwnd,
            pid: l.pid,
            exe: procs.get(&l.pid).cloned().unwrap_or_else(|| "unknown".into()),
            title: l.title,
        })
        .collect())
}

pub(crate) fn run_list_windows(_args: &Value) -> Result<String, String> {
    let rows = top_windows()?;
    if rows.is_empty() {
        return Err("No visible windows with titles found.".into());
    }
    let mut out = format!("{} visible windows (front first)", rows.len());
    if rows.len() > MAX_WINDOWS_SHOWN {
        out.push_str(&format!(" — first {MAX_WINDOWS_SHOWN} shown"));
    }
    out.push(':');
    for (i, w) in rows.iter().enumerate().take(MAX_WINDOWS_SHOWN) {
        out.push_str(&format!(
            "\n  {}. {} — {} (pid {})",
            i + 1,
            w.title,
            w.exe,
            w.pid
        ));
    }
    Ok(out)
}

/* ================================================================ */
/* focus_window                                                      */
/* ================================================================ */

/// Match rank of one window against the query (higher wins):
/// exact case-insensitive title 3 > title substring 2 > exe
/// substring 1 > no match 0. A blank query never matches.
fn match_rank(title: &str, exe: &str, query: &str) -> u8 {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return 0;
    }
    let t = title.to_lowercase();
    let e = exe.to_lowercase();
    if t == q {
        3
    } else if t.contains(&q) {
        2
    } else if e.contains(&q) {
        1
    } else {
        0
    }
}

/// Best match in z-order: highest rank wins, ties go to the front-most
/// window (earlier rows are never displaced by equal ranks). None when
/// nothing matches. Pure — the ranking rule is testable hermetically.
fn find_best_match<'a>(rows: &'a [WinRow], query: &str) -> Option<&'a WinRow> {
    let mut best: Option<(u8, usize)> = None;
    for (i, w) in rows.iter().enumerate() {
        let r = match_rank(&w.title, &w.exe, query);
        if r == 0 {
            continue;
        }
        if best.map_or(true, |(br, _)| r > br) {
            best = Some((r, i));
        }
    }
    best.map(|(_, i)| &rows[i])
}

/// Error text for a failed match: the query, up to five candidate
/// titles in z-order, and the pointer to list_windows.
fn no_match_message(query: &str, rows: &[WinRow]) -> String {
    let mut candidates = String::new();
    for w in rows.iter().take(5) {
        if !candidates.is_empty() {
            candidates.push_str("; ");
        }
        candidates.push_str(&w.title);
    }
    format!(
        "No window matches '{query}'. Visible windows: {candidates}. \
         Call list_windows for the full list, then focus one by its exact title."
    )
}

/// Bring one window to the foreground: restore it if minimized, tap ALT
/// once (the standard workaround that lets a background process claim
/// the foreground), SetForegroundWindow, then verify — a failed focus
/// change is reported as failed, never as success.
fn focus_hwnd(win: &WinRow) -> Result<String, String> {
    /* SAFETY: hwnd came from a live EnumWindows result this run; each
     * call is a single standard user32 operation on that handle. */
    let hwnd = HWND(win.hwnd as *mut core::ffi::c_void);
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let alt = VK_MENU.0 as u8;
        keybd_event(alt, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(alt, 0, KEYEVENTF_KEYUP, 0);
        let _ = SetForegroundWindow(hwnd);
        /* give the focus change a moment to settle before verifying */
        std::thread::sleep(Duration::from_millis(120));
        let fg = GetForegroundWindow();
        if fg.0 as usize != hwnd.0 as usize {
            return Err(format!(
                "Focused '{}' but Windows did not move the foreground window (focus change \
                 denied). Click once anywhere, then try again.",
                win.title
            ));
        }
    }
    Ok(format!(
        "Focused: {} — {} (pid {}). It is now the foreground window.",
        win.title, win.exe, win.pid
    ))
}

pub(crate) fn run_focus_window(args: &Value) -> Result<String, String> {
    let target = required_str(args, "target", "focus_window")?;
    if target.trim().is_empty() {
        return Err(
            "focus_window: target is empty — name a window title or application name.".into(),
        );
    }
    let rows = top_windows()?;
    if rows.is_empty() {
        return Err("No visible windows with titles found — nothing to focus.".into());
    }
    let Some(win) = find_best_match(&rows, target) else {
        return Err(no_match_message(target, &rows));
    };
    focus_hwnd(win)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::reg;
    use serde_json::json;

    /// Synthetic row for the pure helpers (hwnd/pid are irrelevant to
    /// ranking; z-order is the Vec order).
    fn row(title: &str, exe: &str) -> WinRow {
        WinRow {
            hwnd: 0,
            pid: 0,
            exe: exe.to_string(),
            title: title.to_string(),
        }
    }

    #[test]
    fn match_rank_prefers_exact_then_substring_then_exe() {
        assert_eq!(match_rank("Notepad", "notepad.exe", "notepad"), 3);
        assert_eq!(match_rank("Untitled - Notepad", "notepad.exe", "notepad"), 2);
        assert_eq!(match_rank("Blank", "notepad.exe", "notepad"), 1);
        assert_eq!(match_rank("Blank", "other.exe", "notepad"), 0);
        /* case-insensitive on both sides */
        assert_eq!(match_rank("SPOTIFY", "x.exe", "spotify"), 3);
        /* a blank query matches nothing, even an identical title */
        assert_eq!(match_rank("Notepad", "notepad.exe", "   "), 0);
    }

    #[test]
    fn find_best_match_ranks_and_prefers_frontmost() {
        let rows = vec![
            row("Notepad", "notepad.exe"),
            row("Untitled - Notepad", "notepad.exe"),
            row("rust-analyzer status", "rust-analyzer.exe"),
        ];
        /* exact title beats a deeper substring even when it is in front */
        let win = find_best_match(&rows, "notepad").unwrap();
        assert_eq!(win.title, "Notepad");
        /* substring title match */
        assert_eq!(find_best_match(&rows, "untitled").unwrap().title, "Untitled - Notepad");
        /* exe-only match */
        assert_eq!(
            find_best_match(&rows, "rust-analyzer").unwrap().title,
            "rust-analyzer status"
        );
        /* tie on rank → the front-most window wins */
        let tied = vec![row("foo - bar", "a.exe"), row("bar - baz", "b.exe")];
        assert_eq!(find_best_match(&tied, "bar").unwrap().title, "foo - bar");
        /* no match at all → None */
        assert!(find_best_match(&rows, "zzz-nothing").is_none());
        assert!(find_best_match(&rows, "   ").is_none());
    }

    #[test]
    fn no_match_message_lists_candidates_and_next_step() {
        let rows = vec![
            row("one alpha", "a.exe"),
            row("two beta", "b.exe"),
            row("three gamma", "c.exe"),
        ];
        let msg = no_match_message("velvet", &rows);
        assert!(msg.contains("No window matches 'velvet'"), "{msg}");
        assert!(msg.contains("one alpha; two beta; three gamma"), "{msg}");
        assert!(msg.contains("list_windows"), "{msg}");
    }

    #[test]
    fn focus_window_rejects_missing_and_empty_targets() {
        let err = reg().dispatch("focus_window", &json!({})).unwrap_err();
        assert!(err.contains("focus_window"), "{err}");
        let err = reg()
            .dispatch("focus_window", &json!({"target": "   "}))
            .unwrap_err();
        assert!(err.contains("target is empty"), "{err}");
    }

    #[test]
    fn focus_window_no_match_reports_candidates() {
        /* real enumeration on this machine; the impossible query is the
         * point — success (which would steal focus) is never reached */
        let err = reg()
            .dispatch("focus_window", &json!({"target": "zzq impossible window 4128"}))
            .unwrap_err();
        assert!(err.contains("No window matches"), "{err}");
        assert!(err.contains("list_windows"), "{err}");
    }

    #[test]
    fn list_windows_reports_shape() {
        let out = reg().dispatch("list_windows", &json!({})).expect("list_windows");
        assert!(out.contains("visible windows (front first)"), "{out}");
        /* rows carry exe + pid, and numbering starts at 1 */
        assert!(out.contains(" (pid "), "{out}");
        assert!(out.contains("\n  1. "), "{out}");
    }
}
