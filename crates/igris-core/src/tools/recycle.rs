//! Recycle-bin tools (v3, U1 follow-up): a read-only indexed listing and
//! the gated permanent-removal action, so "empty the recycle bin" and
//! "delete bin item 3" are real capabilities instead of the U1 probe's
//! five-round hunt for a folder `list_dir` cannot see.
//!
//! - `recycle_bin`       — UNGATED listing: one line per item with a
//!   1-based index, original name, size, deletion time and original
//!   location, newest first (the same items Explorer shows).
//! - `empty_recycle_bin` — GATED removal: `action=delete` permanently
//!   removes the indices given (the user picks from the listing);
//!   `action=empty` permanently removes every item. Both run only after
//!   the confirmed re-call ([`crate::gating`]) and re-enumerate at plan
//!   AND execute time — the confirmation names exactly what will go, and
//!   the readback names exactly what went.
//!
//! How entries are found (validated live on the benchmark host, 2026-11-16):
//! every recycle-bin item is a `$I<token><.ext>` metadata file (8-byte
//! version, 8-byte original size, 8-byte deletion FILETIME, then the
//! original path — fixed 260-wchar buffer in v1, u32 length + UTF-16 in
//! v2) next to its `$R<token><.ext>` payload; the pair differs only in
//! that first letter. A `$I` without its `$R` is an invisible remnant
//! Explorer does not count; it is swept by `action=empty` and never
//! listed. Any other file inside the bin folders (orphan `$R` payloads,
//! unindexed binaries) is NOT a recycle-bin entry: counted in readbacks,
//! never touched. All of it is plain `std::fs` — enumeration and removal
//! need no COM at all; the only unsafe code is the post-delete
//! `SHChangeNotify` nudge so Explorer's bin views refresh.

use crate::tools::shared::{fmt_bytes, required_str};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Listing rows are capped so a huge bin cannot flood the model context;
/// indices beyond the cap stay addressable — `action=empty` covers them.
const LIST_CAP: usize = 200;

/// One deletion call may name at most this many indices — a degenerate
/// reply must not turn into a megabyte of failure report.
const INDICES_CAP: usize = 1000;

/* ================================================================ */
/* bin metadata: parsing and pairing                                  */
/* ================================================================ */

/// What one `$I` metadata file says about the item it describes.
#[derive(Debug, Clone)]
struct BinEntry {
    /// The `$I` file itself — deleting it removes the entry.
    meta_i: PathBuf,
    /// The `$R` payload (file or folder) paired by token.
    payload: PathBuf,
    /// Whether the payload is a folder (payload kind is re-probed at
    /// deletion time; this is the listing-time view).
    is_dir: bool,
    /// Original file name (what Explorer shows).
    name: String,
    /// Original full path before the recycle.
    original: String,
    /// Original size in bytes (from the `$I` header).
    size: u64,
    /// Deletion time as a Windows FILETIME.
    deleted: u64,
}

/// The `$I` payload: (original size, deletion FILETIME, original path).
/// v2 (Windows 10+): u32 UTF-16 length after the 24-byte header. v1
/// (Vista/7): fixed 260-wchar buffer. Anything else is not a bin entry.
fn parse_i(b: &[u8]) -> Option<(u64, u64, String)> {
    if b.len() < 24 {
        return None;
    }
    let size = u64::from_le_bytes(b[8..16].try_into().unwrap());
    let ft = u64::from_le_bytes(b[16..24].try_into().unwrap());
    /* all three slices are inside the length-24 floor checked above */
    let original = match u64::from_le_bytes(b[0..8].try_into().unwrap()) {
        2 => {
            if b.len() < 28 {
                return None;
            }
            let len = u32::from_le_bytes(b[24..28].try_into().ok()?) as usize;
            if b.len() < 28 + 2 * len {
                return None;
            }
            let units: Vec<u16> = b[28..28 + 2 * len]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        }
        1 => {
            if b.len() < 24 + 520 {
                return None;
            }
            let units: Vec<u16> = b[24..24 + 520]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            let end = units
                .iter()
                .position(|&u| u == 0)
                .unwrap_or(units.len());
            String::from_utf16_lossy(&units[..end])
        }
        _ => return None,
    };
    Some((size, ft, original))
}

/// The `$R` payload path paired with a `$I` file: same token and
/// extension, first letter swapped. `$IK5F44M.zip` → `$RK5F44M.zip`;
/// `$I7KGTU0` (no extension, a recycled folder) → `$R7KGTU0`. The token
/// is everything after `$I` up to the first dot (tokens are random
/// alphanumerics, so a dot always starts the extension).
fn payload_path(meta_i: &Path) -> Option<PathBuf> {
    let name = meta_i.file_name()?.to_str()?;
    let rest = name.strip_prefix("$I")?;
    let (tok, ext) = match rest.find('.') {
        Some(d) => (&rest[..d], &rest[d..]),
        None => (rest, ""),
    };
    if tok.is_empty() {
        return None;
    }
    Some(meta_i.with_file_name(format!("$R{tok}{ext}")))
}

/// Parse one `$I` file and pair it with its payload. None when the file
/// is not parseable bin metadata or the payload is gone — such files are
/// invisible remnants, not entries.
fn parse_pair(meta_i: &Path) -> Option<BinEntry> {
    let bytes = fs::read(meta_i).ok()?;
    let (size, deleted, original) = parse_i(&bytes)?;
    let payload = payload_path(meta_i)?;
    let md = fs::metadata(&payload).ok()?;
    let name = Path::new(&original)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| original.clone());
    Some(BinEntry {
        meta_i: meta_i.to_path_buf(),
        payload,
        is_dir: md.is_dir(),
        name,
        original,
        size,
        deleted,
    })
}

/* ================================================================ */
/* enumeration                                                        */
/* ================================================================ */

/// One pass over the bin folders: paired items (what the user sees in
/// Explorer) and a count of unindexed files that are not recycle-bin
/// entries at all. `$I` remnants without payloads are simply not
/// entries; `sweep_orphan_i` re-walks and removes them on `empty`.
struct BinScan {
    items: Vec<BinEntry>,
    unindexed: usize,
}

/// `{L}:\$Recycle.Bin` for every drive letter where that folder exists —
/// the bin is per-drive, so one request spans all of them.
fn bin_roots() -> Vec<PathBuf> {
    (b'C'..=b'Z')
        .filter_map(|l| {
            let p = PathBuf::from(format!("{}:\\$Recycle.Bin", l as char));
            p.is_dir().then_some(p)
        })
        .collect()
}

/// Scan explicit bin roots (the real ones in production; a temp-tree
/// fake in tests). Every readable per-user SID folder under each root is
/// walked; `$I` files become entries or remnants, everything else is
/// counted as unindexed (except folder-view `desktop.ini`).
fn scan_roots(roots: &[PathBuf]) -> BinScan {
    let mut items = Vec::new();
    let mut unindexed = 0usize;
    for root in roots {
        let Ok(sids) = fs::read_dir(root) else {
            continue;
        };
        for sid in sids.flatten() {
            let Ok(entries) = fs::read_dir(sid.path()) else {
                continue;
            };
            /* snapshot the folder once: pair first, then count what is
             * left over — so the `$R` payloads of listed entries do not
             * count as unrelated files */
            let mut files: Vec<(String, PathBuf, bool)> = Vec::new();
            for e in entries.flatten() {
                files.push((
                    e.file_name().to_string_lossy().to_string(),
                    e.path(),
                    e.file_type().map(|t| t.is_file()).unwrap_or(false),
                ));
            }
            let mut payloads: Vec<PathBuf> = Vec::new();
            for (name, path, is_file) in &files {
                if name.starts_with("$I") && *is_file {
                    /* unparseable or payload-less `$I` files are not
                     * entries — `sweep_orphan_i` cleans them on empty */
                    if let Some(entry) = parse_pair(path) {
                        payloads.push(entry.payload.clone());
                        items.push(entry);
                    }
                }
            }
            for (name, path, _is_file) in &files {
                if name.starts_with("$I") || name.eq_ignore_ascii_case("desktop.ini")
                {
                    continue;
                }
                if payloads.iter().any(|p| p == path) {
                    continue;
                }
                unindexed += 1;
            }
        }
    }
    /* Newest first (Explorer's own ordering), ties by path so the order
     * is stable between the listing and the confirmed re-call. */
    items.sort_by(|a, b| {
        b.deleted
            .cmp(&a.deleted)
            .then_with(|| a.meta_i.cmp(&b.meta_i))
    });
    BinScan { items, unindexed }
}

fn scan() -> BinScan {
    scan_roots(&bin_roots())
}

/* ================================================================ */
/* formatting (pure, hermetically tested)                             */
/* ================================================================ */

/// FILETIME → local wall-clock, the same shape `file_metadata` prints.
fn fmt_ft(ft: u64) -> String {
    const TICKS_PER_SEC: u64 = 10_000_000;
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600;
    let to_string = |t: std::time::SystemTime| {
        chrono::DateTime::<chrono::Local>::from(t)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    };
    if ft < EPOCH_DIFF_SECS.saturating_mul(TICKS_PER_SEC) {
        return "unknown date".to_string();
    }
    let secs = ft / TICKS_PER_SEC - EPOCH_DIFF_SECS;
    let nanos = (ft % TICKS_PER_SEC) * 100;
    std::time::SystemTime::UNIX_EPOCH
        .checked_add(std::time::Duration::new(secs, nanos as u32))
        .map(to_string)
        .unwrap_or_else(|| "unknown date".to_string())
}

fn entry_line(e: &BinEntry, ix: usize) -> String {
    /* folder payloads carry no meaningful size in the `$I` header, so
     * the kind takes its place */
    let kind_size = if e.is_dir {
        "folder".to_string()
    } else {
        fmt_bytes(e.size)
    };
    format!(
        "  {ix}. {} ({}, deleted {}, from {})",
        e.name,
        kind_size,
        fmt_ft(e.deleted),
        e.original
    )
}

/// The `recycle_bin` listing. Indices are the shared language between
/// the listing, the user's pick and the gated delete — so the tail names
/// the tool that consumes them.
fn list_text(items: &[BinEntry], unindexed: usize) -> String {
    if items.is_empty() {
        return "The Recycle Bin is empty.".to_string();
    }
    let total: u64 = items.iter().map(|e| e.size).sum();
    let mut out = format!(
        "Recycle bin: {} items ({} total, newest first).",
        items.len(),
        fmt_bytes(total)
    );
    let shown = items.len().min(LIST_CAP);
    for (i, e) in items.iter().take(shown).enumerate() {
        out.push('\n');
        out.push_str(&entry_line(e, i + 1));
    }
    if items.len() > shown {
        out.push_str(&format!(
            "\n  ... and {} more (action=empty removes everything).",
            items.len() - shown
        ));
    }
    out.push_str(
        "\nRemove with empty_recycle_bin: action=delete and the chosen \
         indices, or action=empty for all.",
    );
    if unindexed > 0 {
        out.push_str(&format!(
            "\n({unindexed} unindexed file(s) in the bin folders are not \
             recycle-bin entries; they are never listed or removed.)"
        ));
    }
    out
}

/// Plan (confirmation) text for `action=delete`: every requested index
/// must resolve right now, and the user sees the exact list before the
/// pending is armed.
fn plan_text_delete(items: &[BinEntry], indices: &[usize]) -> Result<String, String> {
    for &ix in indices {
        if ix == 0 || ix > items.len() {
            return Err(format!(
                "The Recycle Bin currently holds {} items; index {ix} is \
                 out of range. Call recycle_bin for a fresh list.",
                items.len()
            ));
        }
    }
    let mut out =
        String::from("Permanently delete from the Recycle Bin (cannot be undone):");
    for &ix in indices {
        out.push('\n');
        out.push_str(&entry_line(&items[ix - 1], ix));
    }
    Ok(out)
}

/// Plan text for `action=empty`: count + size + the newest few names, so
/// a 60-item confirmation stays one screenful.
fn plan_text_empty(items: &[BinEntry]) -> String {
    let total: u64 = items.iter().map(|e| e.size).sum();
    let mut out = format!(
        "Permanently empty the Recycle Bin (cannot be undone): {} items, \
         {} total.",
        items.len(),
        fmt_bytes(total)
    );
    let preview: Vec<&str> = items.iter().take(6).map(|e| e.name.as_str()).collect();
    let more = items.len().saturating_sub(preview.len());
    if !preview.is_empty() {
        out.push_str(&format!(" Newest: {}", preview.join(", ")));
        if more > 0 {
            out.push_str(&format!(" … and {more} more."));
        }
    }
    out
}

/// Execution readback shared by both actions: what went, what refused,
/// what invisible remnants were swept, what was left untouched, and the
/// fresh post-state count. Failure lists are capped so a mass failure
/// cannot flood the context.
fn delete_readback(
    deleted: &[String],
    failed: &[(String, String)],
    swept: usize,
    unindexed_left: usize,
    remaining: usize,
    emptied: bool,
) -> String {
    let mut out = if emptied {
        format!(
            "Emptied the Recycle Bin: permanently deleted {} item(s).",
            deleted.len()
        )
    } else {
        format!(
            "Permanently deleted {} item(s) from the Recycle Bin: {}.",
            deleted.len(),
            deleted.join(", ")
        )
    };
    if !failed.is_empty() {
        let shown = failed.len().min(5);
        out.push_str(&format!(
            "\n{} item(s) could not be removed: {}{}",
            failed.len(),
            failed[..shown]
                .iter()
                .map(|(n, e)| format!("{n} ({e})"))
                .collect::<Vec<_>>()
                .join(", "),
            if failed.len() > shown {
                format!(" … and {} more.", failed.len() - shown)
            } else {
                String::new()
            }
        ));
    }
    if swept > 0 {
        out.push_str(&format!(
            "\nAlso removed {swept} unindexed metadata remnant(s) left by \
             earlier partial deletions."
        ));
    }
    if unindexed_left > 0 {
        out.push_str(&format!(
            "\n{unindexed_left} unrelated file(s) in the bin folders were \
             left untouched (not recycle-bin entries)."
        ));
    }
    out.push_str(&format!(
        "\nThe Recycle Bin now holds {remaining} item(s)."
    ));
    out
}

/* ================================================================ */
/* removal                                                            */
/* ================================================================ */

/// Delete one entry: payload first (if the payload removal fails, the
/// `$I` stays so the entry remains visible — no invisible leaked
/// payload), then the `$I`. A payload that vanished between listing and
/// execution is fine: the remnant `$I` is what gets removed.
fn delete_entry(entry: &BinEntry) -> Result<(), String> {
    match fs::metadata(&entry.payload) {
        Ok(md) => {
            let res = if md.is_dir() {
                fs::remove_dir_all(&entry.payload)
            } else {
                fs::remove_file(&entry.payload)
            };
            res.map_err(|e| format!("{}: {e}", entry.payload.display()))?;
        }
        Err(_) => { /* payload already gone; the $I below is the entry */ }
    }
    fs::remove_file(&entry.meta_i)
        .map_err(|e| format!("{}: {e}", entry.meta_i.display()))
}

/// Best-effort sweep of `$I` remnants whose payload is gone (or was
/// never paired). Payloads that still exist keep their `$I` — the entry
/// must stay visible and deletable.
fn sweep_orphan_i(roots: &[PathBuf]) -> usize {
    let mut swept = 0;
    for root in roots {
        let Ok(sids) = fs::read_dir(root) else {
            continue;
        };
        for sid in sids.flatten() {
            let Ok(entries) = fs::read_dir(sid.path()) else {
                continue;
            };
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let is_file = e.file_type().map(|t| t.is_file()).unwrap_or(false);
                if !(name.starts_with("$I") && is_file) {
                    continue;
                }
                if let Some(payload) = payload_path(&e.path()) {
                    if payload.exists() {
                        continue;
                    }
                }
                if fs::remove_file(e.path()).is_ok() {
                    swept += 1;
                }
            }
        }
    }
    swept
}

/// Tell the shell the bin folder changed, so desktop/bin views refresh
/// the way the native empty does. Cosmetic and best-effort: failures are
/// ignored.
fn nudge_shell_views() {
    use windows::Win32::UI::Shell::{
        ILFree, SHChangeNotify, SHGetSpecialFolderLocation, CSIDL_BITBUCKET,
        SHCNF_IDLIST, SHCNE_UPDATEDIR,
    };
    unsafe {
        /* shell32 hands back the bitbucket PIDL directly; free it on
         * every path that acquired one */
        let Ok(pidl) =
            SHGetSpecialFolderLocation(None, CSIDL_BITBUCKET as i32)
        else {
            return;
        };
        if !pidl.is_null() {
            SHChangeNotify(
                SHCNE_UPDATEDIR,
                SHCNF_IDLIST,
                Some(pidl as *const core::ffi::c_void),
                None,
            );
        }
        ILFree(Some(pidl as *const _));
    }
}

/* ================================================================ */
/* argument handling                                                  */
/* ================================================================ */

fn action_arg(args: &Value) -> Result<String, String> {
    let action = required_str(args, "action", "empty_recycle_bin")?;
    match action {
        "delete" | "empty" => Ok(action.to_string()),
        other => Err(format!(
            "action must be 'delete' or 'empty', got '{other}'"
        )),
    }
}

/// `indices` for `action=delete`: an array of 1-based integers (floats
/// with a zero fraction are tolerated — models send `2.0`), deduped and
/// sorted ascending.
fn indices_arg(args: &Value) -> Result<Vec<usize>, String> {
    let hint = "indices is required for action=delete — an array of \
                1-based indices from the recycle_bin listing";
    let Some(list) = args.get("indices").and_then(|v| v.as_array()) else {
        return Err(hint.to_string());
    };
    if list.is_empty() {
        return Err(hint.to_string());
    }
    if list.len() > INDICES_CAP {
        return Err(format!(
            "indices takes at most {INDICES_CAP} entries per call"
        ));
    }
    let mut out = Vec::with_capacity(list.len());
    for v in list {
        /* i64 first; zero-fraction floats are tolerated (models send
         * 2.0) — anything else is not a whole number */
        let n = match (v.as_i64(), v.as_f64()) {
            (Some(n), _) => n,
            (None, Some(f)) if f.is_finite() && f.fract() == 0.0 => f as i64,
            _ => {
                return Err(
                    "indices must be an array of whole numbers (1-based)"
                        .to_string(),
                )
            }
        };
        if n < 1 {
            return Err("indices are 1-based (the first item is 1)".to_string());
        }
        out.push(n as usize);
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/* ================================================================ */
/* tool bodies                                                        */
/* ================================================================ */

/// `recycle_bin` — read-only listing, ungated.
pub(crate) fn run_recycle_bin(_args: &Value) -> Result<String, String> {
    let s = scan();
    Ok(list_text(&s.items, s.unindexed))
}

/// Plan (validator + narrator) for the gated `empty_recycle_bin`. Runs
/// on the first call, never executes. Validation is against a fresh
/// enumeration, so an empty bin is refused outright (nothing to
/// confirm) and every named index must exist right now.
pub(crate) fn plan_empty_recycle_bin(args: &Value) -> Result<String, String> {
    let action = action_arg(args)?;
    let s = scan();
    if s.items.is_empty() {
        return Err("The Recycle Bin is already empty — nothing to remove."
            .to_string());
    }
    match action.as_str() {
        "empty" => Ok(plan_text_empty(&s.items)),
        _ => {
            let indices = indices_arg(args)?;
            plan_text_delete(&s.items, &indices)
        }
    }
}

/// Executor for the gated `empty_recycle_bin`. Re-validates everything
/// the plan validated: fresh enumeration, per-index resolution against
/// that enumeration (drift shows up as honest per-item failures, never
/// silent), then the readback with the fresh post-state.
pub(crate) fn run_empty_recycle_bin(args: &Value) -> Result<String, String> {
    let action = action_arg(args)?;
    let pre = scan();
    if pre.items.is_empty() {
        return Ok("The Recycle Bin is already empty; nothing was deleted."
            .to_string());
    }
    let mut deleted = Vec::new();
    let mut failed: Vec<(String, String)> = Vec::new();
    match action.as_str() {
        "empty" => {
            for e in &pre.items {
                match delete_entry(e) {
                    Ok(()) => deleted.push(e.name.clone()),
                    Err(err) => failed.push((e.name.clone(), err)),
                }
            }
        }
        _ => {
            let indices = indices_arg(args)?;
            for ix in indices {
                match pre.items.get(ix - 1) {
                    Some(e) => match delete_entry(e) {
                        Ok(()) => deleted.push(e.name.clone()),
                        Err(err) => failed.push((e.name.clone(), err)),
                    },
                    None => failed.push((
                        format!("index {ix}"),
                        format!(
                            "out of range (the bin now holds {} items)",
                            pre.items.len()
                        ),
                    )),
                }
            }
        }
    }
    let emptied = action == "empty";
    let swept = if emptied {
        sweep_orphan_i(&bin_roots())
    } else {
        0
    };
    nudge_shell_views();
    let post = scan();
    Ok(delete_readback(
        &deleted,
        &failed,
        swept,
        post.unindexed,
        post.items.len(),
        emptied,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::temp_root;
    use serde_json::json;

    /* ---- $I byte builders ---- */

    /// Windows 10+ style metadata (version 2).
    fn write_v2_i(
        dir: &Path,
        tok: &str,
        ext: &str,
        size: u64,
        ft: u64,
        original: &str,
    ) -> PathBuf {
        let mut b = Vec::new();
        b.extend_from_slice(&2u64.to_le_bytes());
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&ft.to_le_bytes());
        let units: Vec<u16> = original.encode_utf16().collect();
        b.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for u in units {
            b.extend_from_slice(&u.to_le_bytes());
        }
        let p = dir.join(format!("$I{tok}{ext}"));
        fs::write(&p, &b).unwrap();
        p
    }

    /// A complete pair: the `$I` metadata plus a non-empty payload.
    fn write_pair(
        dir: &Path,
        tok: &str,
        ext: &str,
        size: u64,
        ft: u64,
        original: &str,
    ) -> PathBuf {
        let i = write_v2_i(dir, tok, ext, size, ft, original);
        let payload = payload_path(&i).unwrap();
        fs::write(&payload, b"payload bytes").unwrap();
        i
    }

    /* ---- parse_i ---- */

    #[test]
    fn parse_i_reads_v2_header_and_path() {
        let dir = temp_root("recycle_parse");
        let p = write_v2_i(
            &dir,
            "K5F44M",
            ".zip",
            1_396_375_906,
            134_283_387_534_850_000,
            r"C:\Users\sohan\Downloads\setup.zip",
        );
        let (size, ft, original) = parse_i(&fs::read(&p).unwrap()).unwrap();
        assert_eq!(size, 1_396_375_906);
        assert_eq!(ft, 134_283_387_534_850_000);
        assert_eq!(original, r"C:\Users\sohan\Downloads\setup.zip");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn parse_i_reads_v1_fixed_buffer() {
        let mut b = Vec::new();
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&42u64.to_le_bytes());
        b.extend_from_slice(&123u64.to_le_bytes());
        let mut units = "C:\\old\\thing.txt"
            .encode_utf16()
            .collect::<Vec<_>>();
        units.resize(260, 0);
        for u in units {
            b.extend_from_slice(&u.to_le_bytes());
        }
        let (size, ft, original) = parse_i(&b).unwrap();
        assert_eq!(size, 42);
        assert_eq!(ft, 123);
        assert_eq!(original, "C:\\old\\thing.txt");
    }

    #[test]
    fn parse_i_rejects_garbage_and_unknown_versions() {
        assert!(parse_i(&[]).is_none());
        assert!(parse_i(&[0u8; 10]).is_none());
        /* unknown version marker */
        let mut b = vec![9u8; 24];
        b.extend_from_slice(&[0u8; 520]);
        assert!(parse_i(&b).is_none());
        /* v2 with a truncated path */
        let mut b = 2u64.to_le_bytes().to_vec();
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&100u32.to_le_bytes());
        b.extend_from_slice(&[0u8; 10]);
        assert!(parse_i(&b).is_none());
    }

    /* ---- payload pairing ---- */

    #[test]
    fn payload_pairs_by_token_and_extension() {
        let mk = |n: &str| PathBuf::from(n);
        assert_eq!(
            payload_path(&mk(r"C:\bin\$IK5F44M.zip"))
                .unwrap()
                .file_name()
                .unwrap(),
            "$RK5F44M.zip"
        );
        /* recycled folder: no extension at all */
        assert_eq!(
            payload_path(&mk(r"C:\bin\$I7KGTU0"))
                .unwrap()
                .file_name()
                .unwrap(),
            "$R7KGTU0"
        );
        /* multi-dot original extension */
        assert_eq!(
            payload_path(&mk(r"C:\bin\$Iab12cd34.tar.gz"))
                .unwrap()
                .file_name()
                .unwrap(),
            "$Rab12cd34.tar.gz"
        );
        /* not a $I file at all */
        assert!(payload_path(&mk(r"C:\bin\$R7KGTU0")).is_none());
        assert!(payload_path(&mk(r"C:\bin\desktop.ini")).is_none());
    }

    /* ---- enumeration ---- */

    /// A fake bin root shaped like the real thing: `$Recycle.Bin\<SID>`
    /// holding pairs, an orphan `$I`, a desktop.ini and unindexed junk.
    fn fake_bin(tag: &str) -> PathBuf {
        /* one temp_root: everything below must live inside the same
         * fresh scratch dir the caller scans */
        let base = temp_root(tag);
        let root = base.join("$Recycle.Bin").join("S-1-5-21-test");
        fs::create_dir_all(&root).unwrap();
        /* older pair */
        write_pair(
            &root,
            "aaaaaa",
            ".txt",
            9,
            1_000,
            r"C:\Users\t\Desktop\doomed.txt",
        );
        /* newer pair */
        write_pair(
            &root,
            "bbbbbb",
            ".log",
            2_048,
            2_000,
            r"C:\Users\t\logs\run.log",
        );
        /* a recycled folder (extensionless pair) */
        let i = write_v2_i(
            &root,
            "cccccc",
            "",
            0,
            1_500,
            r"C:\Users\t\Documents\stuff",
        );
        let payload = payload_path(&i).unwrap();
        fs::create_dir_all(&payload).unwrap();
        /* invisible remnant + unindexed junk + folder-view file */
        write_v2_i(&root, "dddddd", ".txt", 5, 1_800, r"C:\gone.txt");
        fs::write(root.join("$Reeeeee.txt"), b"orphan payload").unwrap();
        fs::write(root.join("desktop.ini"), b"[shell]").unwrap();
        fs::write(root.join("mystery.bin"), b"junk").unwrap();
        base
    }

    #[test]
    fn scan_lists_only_paired_newest_first() {
        let base = fake_bin("recycle_scan");
        let sid = base.join("$Recycle.Bin").join("S-1-5-21-test");
        let s = scan_roots(&[sid.parent().unwrap().to_path_buf()]);
        assert_eq!(s.items.len(), 3, "orphan $I is not an entry");
        /* newest first */
        assert_eq!(s.items[0].name, "run.log");
        assert_eq!(s.items[1].name, "stuff");
        assert_eq!(s.items[2].name, "doomed.txt");
        /* the folder entry is marked as one */
        let text = list_text(&s.items, s.unindexed);
        assert!(text.contains("2. stuff (folder,"), "{text}");
        assert!(text.contains("1. run.log (2.0 KB,"), "{text}");
        /* 4 `$I` files exist, only 3 paired: one invisible remnant */
        let i_count = fs::read_dir(&sid)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name().to_string_lossy().starts_with("$I")
                    && e.file_type().map(|t| t.is_file()).unwrap_or(false)
            })
            .count();
        assert_eq!(i_count - s.items.len(), 1, "one orphan $I");
        /* junk: orphan $R + mystery.bin (desktop.ini excluded) */
        assert_eq!(s.unindexed, 2);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn listing_shows_indices_totals_and_remnant_note() {
        let base = fake_bin("recycle_list");
        let s = scan_roots(&[base.join("$Recycle.Bin")]);
        let text = list_text(&s.items, s.unindexed);
        assert!(text.starts_with("Recycle bin: 3 items ("), "{text}");
        assert!(text.contains("1. run.log"), "{text}");
        assert!(text.contains("2. stuff"), "{text}");
        assert!(text.contains("3. doomed.txt"), "{text}");
        assert!(text.contains("empty_recycle_bin"), "{text}");
        assert!(text.contains("2 unindexed file(s)"), "{text}");
        assert_eq!(list_text(&[], 0), "The Recycle Bin is empty.");
        fs::remove_dir_all(&base).ok();
    }

    /* ---- deletion + sweep ---- */

    #[test]
    fn delete_entry_removes_payload_then_metadata() {
        let base = fake_bin("recycle_del");
        let s = scan_roots(&[base.join("$Recycle.Bin")]);
        let doomed = s.items.iter().find(|e| e.name == "doomed.txt").unwrap();
        delete_entry(doomed).unwrap();
        assert!(!doomed.payload.exists());
        assert!(!doomed.meta_i.exists());
        /* the folder entry: remove_dir_all path */
        let stuff = s.items.iter().find(|e| e.name == "stuff").unwrap();
        delete_entry(stuff).unwrap();
        assert!(!stuff.payload.exists());
        assert!(!stuff.meta_i.exists());
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn sweep_removes_only_payloadless_i_files() {
        let base = fake_bin("recycle_sweep");
        let sid = base.join("$Recycle.Bin").join("S-1-5-21-test");
        let count_i = |dir: &Path| -> usize {
            fs::read_dir(dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name().to_string_lossy().starts_with("$I")
                        && e.file_type().map(|t| t.is_file()).unwrap_or(false)
                })
                .count()
        };
        let before = count_i(&sid);
        let swept = sweep_orphan_i(&[sid.parent().unwrap().to_path_buf()]);
        assert_eq!(swept, 1, "only the orphan $I");
        assert_eq!(count_i(&sid), before - 1);
        /* every surviving $I still has its payload */
        let after = scan_roots(&[sid.parent().unwrap().to_path_buf()]);
        assert_eq!(after.items.len(), 3);
        fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn ft_to_local_time_renders_or_flags_unknown() {
        assert_eq!(fmt_ft(0), "unknown date");
        /* a real FILETIME from the benchmark host's bin: 134283387534850000
         * = 2026-07-12 in +02:00 local time — assert only the date shape
         * so the test stays timezone-tolerant */
        let s = fmt_ft(134_283_387_534_850_000);
        assert_eq!(s.len(), "0000-00-00 00:00:00".len(), "{s}");
        assert!(s.starts_with("2026-07-1"), "{s}");
    }

    /* ---- pure plan/readback formatting ---- */

    fn hand_entry(name: &str, size: u64) -> BinEntry {
        BinEntry {
            meta_i: PathBuf::from(format!(r"C:\b\$I000000")),
            payload: PathBuf::from(format!(r"C:\b\$R000000")),
            is_dir: false,
            name: name.to_string(),
            original: format!(r"C:\src\{name}"),
            size,
            deleted: 1_337,
        }
    }

    #[test]
    fn plan_delete_names_every_index_and_rejects_out_of_range() {
        let items = vec![hand_entry("a.txt", 1), hand_entry("b.bin", 2)];
        let text =
            plan_text_delete(&items, &[2, 1]).unwrap();
        assert!(text.starts_with("Permanently delete from the Recycle Bin"), "{text}");
        assert!(text.contains("1. a.txt"), "{text}");
        assert!(text.contains("2. b.bin"), "{text}");
        assert!(text.contains("cannot be undone"), "{text}");
        let err = plan_text_delete(&items, &[3]).unwrap_err();
        assert!(err.contains("holds 2 items"), "{err}");
        assert!(err.contains("out of range"), "{err}");
        assert!(plan_text_delete(&items, &[0]).is_err());
    }

    #[test]
    fn plan_empty_sums_size_and_previews_names() {
        let items = vec![
            hand_entry("a.txt", 10),
            hand_entry("b.bin", 2_048),
            hand_entry("c", 7),
        ];
        let text = plan_text_empty(&items);
        assert!(text.contains("3 items"), "{text}");
        assert!(text.contains("2.0 KB total"), "{text}");
        assert!(text.contains("a.txt, b.bin, c"), "{text}");
        assert!(text.contains("cannot be undone"), "{text}");
    }

    #[test]
    fn readback_reports_deletions_failures_and_post_state() {
        let text = delete_readback(
            &["a.txt".into(), "b.bin".into()],
            &[],
            0,
            0,
            37,
            false,
        );
        assert!(
            text.starts_with(
                "Permanently deleted 2 item(s) from the Recycle Bin: \
                 a.txt, b.bin."
            ),
            "{text}"
        );
        assert!(text.contains("now holds 37"), "{text}");

        let text = delete_readback(
            &["a.txt".into()],
            &[("locked".into(), "os error 5".into())],
            3,
            8,
            0,
            true,
        );
        assert!(
            text.starts_with("Emptied the Recycle Bin: permanently deleted 1 item(s)."),
            "{text}"
        );
        assert!(text.contains("1 item(s) could not be removed: locked (os error 5)"), "{text}");
        assert!(text.contains("3 unindexed metadata remnant(s)"), "{text}");
        assert!(text.contains("8 unrelated file(s)"), "{text}");
        assert!(text.contains("now holds 0"), "{text}");
    }

    /* ---- argument validation (machine-independent paths only) ---- */

    #[test]
    fn action_and_indices_args_are_validated() {
        assert_eq!(action_arg(&json!({"action": "empty"})).unwrap(), "empty");
        assert_eq!(action_arg(&json!({"action": "delete"})).unwrap(), "delete");
        let err = action_arg(&json!({"action": "purge"})).unwrap_err();
        assert!(err.contains("'delete' or 'empty'"), "{err}");
        assert!(action_arg(&json!({})).is_err());

        let err = indices_arg(&json!({"indices": []})).unwrap_err();
        assert!(err.contains("indices is required"), "{err}");
        let err = indices_arg(&json!({})).unwrap_err();
        assert!(err.contains("indices is required"), "{err}");
        let err = indices_arg(&json!({"indices": ["all"]})).unwrap_err();
        assert!(err.contains("whole numbers"), "{err}");
        let err = indices_arg(&json!({"indices": [0]})).unwrap_err();
        assert!(err.contains("1-based"), "{err}");
        /* zero-fraction floats are tolerated; dedup + ascending order */
        assert_eq!(
            indices_arg(&json!({"indices": [3, 1.0, 2, 3]})).unwrap(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn plan_refuses_empty_bin_and_bad_action_before_any_arm() {
        /* bad action never touches the bin */
        let err = plan_empty_recycle_bin(&json!({"action": "purge"})).unwrap_err();
        assert!(err.contains("'delete' or 'empty'"), "{err}");
        /* the wild index is refused at plan time without arming anything.
         * Host-state note: on a host whose bin is EMPTY (as after a full
         * emptying probe), the already-empty refusal fires first; on a
         * bin with items, the range check fires. Both are pre-arm
         * refusals - which one appears depends only on the host, never
         * on arming. */
        let err = plan_empty_recycle_bin(
            &json!({"action": "delete", "indices": [999_999_999]}),
        )
        .unwrap_err();
        assert!(
            err.contains("out of range") || err.contains("already empty"),
            "{err}"
        );
    }

    #[test]
    fn dispatch_wiring_listing_runs_and_removal_is_gated() {
        let r = crate::tools::testkit::reg();
        /* ungated listing actually runs (read-only) */
        let out = r.dispatch("recycle_bin", &json!({})).unwrap();
        assert!(
            out.starts_with("Recycle bin:") || out.starts_with("The Recycle Bin is empty."),
            "{out}"
        );
        /* the removal tool is registered gated; the listing is not */
        assert!(r.is_gated("empty_recycle_bin"));
        assert!(!r.is_gated("recycle_bin"));
        /* arg-shape errors never reach the executor */
        assert!(r
            .dispatch("empty_recycle_bin", &json!({"action": "delete"}))
            .is_err());
        assert!(r
            .dispatch("empty_recycle_bin", &json!({"action": 7}))
            .is_err());
    }

    /* drift guard: the executor resolves indices against its own fresh
     * enumeration; out-of-range at execution is a per-item failure, not
     * a silent skip. Pure-level proof: */
    #[test]
    fn executor_treats_stale_index_as_explicit_failure() {
        let pre = vec![hand_entry("a.txt", 1)];
        /* simulate: index 2 requested but only 1 item existed */
        let mut failed = Vec::new();
        for ix in [2usize] {
            match pre.get(ix - 1) {
                Some(e) => {
                    let _ = delete_entry(e);
                }
                None => failed.push((format!("index {ix}"), "out of range".into())),
            }
        }
        assert_eq!(failed.len(), 1);
        let text = delete_readback(&[], &failed, 0, 0, 1, false);
        assert!(text.contains("index 2 (out of range)"), "{text}");
    }
}
