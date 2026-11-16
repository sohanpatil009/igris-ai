//! File tools: file_search, read_file, create_folder (v1) and the v2
//! list_dir + content_search (breadth-first bounded walk with skip
//! lists for dependency trees and nested git clones).

use crate::tools::gated::check_protected;
use crate::tools::shared::{expand_home_path, fmt_bytes, home_dir_string, optional_u64, required_str};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
/* Breadth-first on purpose (same shape as content_search below, BUG-005):
 * a DFS pops the alphabetically-LAST top-level subtree first (observed
 * live: a nested clone starved `crates/` entirely at repo-root scope).
 * BFS gives top-level project folders the first turn and dependency-like
 * trees the last. */
fn search_walk(roots: &[PathBuf], query_lc: &str, limit: usize) -> (Vec<PathBuf>, usize, bool) {
    let deadline = Instant::now() + SEARCH_TIME_BUDGET;
    let mut matches = Vec::new();
    let mut scanned = 0usize;
    let mut budget_hit = false;
    use std::collections::VecDeque;
    'roots: for root in roots {
        let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
        queue.push_back((root.clone(), 0usize));
        while let Some((dir, depth)) = queue.pop_front() {
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
                        && !is_nested_repo(&path)
                    {
                        queue.push_back((path, depth + 1));
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

pub(crate) fn run_file_search(args: &Value) -> Result<String, String> {
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

pub(crate) fn run_read_file(args: &Value) -> Result<String, String> {
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

pub(crate) fn run_create_folder(args: &Value) -> Result<String, String> {
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
/* write_file · replace_file · workspace_info (plan P0)              */
/*                                                                  */
/* One write family, two confirmation levels: creating a NEW file    */
/* under an allowed root is ungated and safe (worst case is a stray  */
/* text file the user can delete); OVERWRITING an existing file is   */
/* gated like the other destructive actions — the first call only    */
/* announces the plan, a confirmed re-call executes it.              */
/*                                                                  */
/* Path policy: absolute paths only (or a leading ~), required to    */
/* live strictly under the user profile, the OS temp directory, or   */
/* the IGRIS workspace root (see workspace_root). System folders,    */
/* the exe directory, drive roots and the profile root itself stay   */
/* protected via gated::check_protected, and executable/script       */
/* extensions are refused so a write can never manufacture something */
/* launchable. Content is text, capped at WRITE_FILE_MAX_BYTES.      */
/* ================================================================ */

/// Content cap per write. A runaway model must not stream megabytes
/// into a file; no legitimate report comes close to this.
const WRITE_FILE_MAX_BYTES: usize = 256 * 1024;

/// File extensions that must never be written: anything launchable or
/// loadable. A text-only write tool only writes text documents.
const WRITE_DENIED_EXTENSIONS: [&str; 12] =
    ["exe", "dll", "sys", "msi", "com", "scr", "bat", "cmd", "ps1", "vbs", "js", "lnk"];

/// How a write treats an existing file (see [`validate_write_target`]).
enum WriteMode {
    /// Refuse when the target exists — the ungated creator.
    CreateNew,
    /// Require the target to exist — the gated replacer.
    ReplaceExisting,
}

/// The IGRIS workspace root — "my project" (plan P0): `IGRIS_WORKSPACE`
/// when it points at a real directory, else the current directory when
/// it contains a Cargo.toml. `None` when neither holds; workspace_info
/// explains the situation to the model instead of leaving it guessing
/// among home directories (benchmark 2026-11-16: S2-7, S4-15, S5-20).
pub(crate) fn workspace_root() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("IGRIS_WORKSPACE") {
        let p = PathBuf::from(v);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.join("Cargo.toml").is_file() {
            return Some(cwd);
        }
    }
    None
}

/// Normalize a path for case-insensitive Windows comparison: backslash
/// separators, lowercased, trailing separators trimmed.
fn normalize_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\");
    s.trim_end_matches('\\').to_lowercase()
}

/// True when `path` may be written: it must live strictly under one of
/// the allowed roots. Compared on normalized strings with an explicit
/// separator, so a sibling prefix ("sohan" vs "sohan2") cannot pass.
fn write_root_allowed(path: &Path) -> bool {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("USERPROFILE") {
        roots.push(PathBuf::from(home));
    }
    roots.push(std::env::temp_dir());
    if let Some(ws) = workspace_root() {
        roots.push(ws);
    }
    let target = normalize_path(path);
    if target.is_empty() {
        return false;
    }
    roots.iter().any(|r| {
        let root = normalize_path(r);
        !root.is_empty() && target != root && target.starts_with(&format!("{root}\\"))
    })
}

/// One validated write target: the absolute path and the text to write.
struct WriteTarget {
    path: PathBuf,
    content: String,
}

/// Validate a write call's arguments against the family policy. Shared
/// by the ungated creator and the gated replacer so both paths enforce
/// exactly the same rules; the mode decides how an existing file is
/// treated. The executor re-validates (plan/run symmetry, TOCTOU-safe
/// on a single-user machine).
fn validate_write_target(
    args: &Value,
    mode: WriteMode,
    tool: &str,
) -> Result<WriteTarget, String> {
    let raw = required_str(args, "path", tool)?;
    if raw.trim().is_empty() {
        return Err("path is empty".into());
    }
    let path = PathBuf::from(expand_home_path(raw));
    if !path.is_absolute() {
        return Err(format!(
            "path '{raw}' is relative — give an absolute path (or one starting with ~), \
             e.g. ~\\Desktop\\report.md; call workspace_info for the allowed roots"
        ));
    }
    let content = required_str(args, "content", tool)?;
    if content.len() > WRITE_FILE_MAX_BYTES {
        return Err(format!(
            "content is too large ({}) — the cap is {} bytes",
            fmt_bytes(content.len() as u64),
            WRITE_FILE_MAX_BYTES
        ));
    }
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        let ext = ext.to_lowercase();
        if WRITE_DENIED_EXTENSIONS.contains(&ext.as_str()) {
            return Err(format!(
                "refusing to write a .{ext} file — this tool writes text documents only"
            ));
        }
    }
    if path.is_dir() {
        return Err(format!("'{raw}' is a directory, cannot write a file there"));
    }
    match mode {
        WriteMode::CreateNew => {
            if path.is_file() {
                return Err(format!(
                    "'{raw}' already exists — use the replace_file tool to overwrite it"
                ));
            }
        }
        WriteMode::ReplaceExisting => {
            if !path.is_file() {
                return Err(format!(
                    "'{raw}' does not exist — use write_file to create a new file"
                ));
            }
        }
    }
    check_protected(&path)
        .map_err(|e| e.replace("deleted", "written"))?;
    if !write_root_allowed(&path) {
        return Err(format!(
            "'{raw}' is outside the writable roots — write under the user profile ({}), \
             the OS temp directory ({}), or the IGRIS workspace root (see workspace_info)",
            std::env::var("USERPROFILE").unwrap_or_else(|_| "(unknown)".into()),
            std::env::temp_dir().display()
        ));
    }
    Ok(WriteTarget {
        path,
        content: content.to_string(),
    })
}

/// Create missing parents, write the bytes, and report honestly.
fn perform_write(target: &WriteTarget, replaced: bool) -> Result<String, String> {
    if let Some(parent) = target.path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Could not create the parent folder for {}: {e}",
                target.path.display()
            )
        })?;
    }
    let old_len = if replaced {
        Some(fs::metadata(&target.path).map(|m| m.len()).unwrap_or(0))
    } else {
        None
    };
    fs::write(&target.path, &target.content)
        .map_err(|e| format!("Could not write {}: {e}", target.path.display()))?;
    let n = target.content.len();
    Ok(match old_len {
        Some(old) => format!(
            "Replaced {} — {} written (was {}).",
            target.path.display(),
            fmt_bytes(n as u64),
            fmt_bytes(old)
        ),
        None => format!(
            "Wrote {} to {} (created).",
            fmt_bytes(n as u64),
            target.path.display()
        ),
    })
}

pub(crate) fn run_write_file(args: &Value) -> Result<String, String> {
    let target = validate_write_target(args, WriteMode::CreateNew, "write_file")?;
    perform_write(&target, false)
}

/// Plan (validator + narrator) for the gated overwrite: the executor
/// re-validates everything below before touching disk.
pub(crate) fn plan_replace_file(args: &Value) -> Result<String, String> {
    let target = validate_write_target(args, WriteMode::ReplaceExisting, "replace_file")?;
    let old = fs::metadata(&target.path).map(|m| m.len()).unwrap_or(0);
    Ok(format!(
        "Overwrite {} ({}) with {} of new text. The old contents are gone for good.",
        target.path.display(),
        fmt_bytes(old),
        fmt_bytes(target.content.len() as u64)
    ))
}

pub(crate) fn run_replace_file(args: &Value) -> Result<String, String> {
    let target = validate_write_target(args, WriteMode::ReplaceExisting, "replace_file")?;
    perform_write(&target, true)
}

pub(crate) fn run_workspace_info(_args: &Value) -> Result<String, String> {
    let home = std::env::var("USERPROFILE").unwrap_or_else(|_| "(unknown)".into());
    let mut out = format!(
        "Writable roots for file tools: the user profile ({home}) and the OS temp directory ({}).",
        std::env::temp_dir().display()
    );
    match workspace_root() {
        Some(ws) => out.push_str(&format!(
            "\nWorkspace root: {}\nTasks that mention 'my project' or 'the IGRIS project' mean this directory.",
            ws.display()
        )),
        None => out.push_str(
            "\nNo workspace root is configured — set the IGRIS_WORKSPACE environment \
             variable or run igris-ai from a directory containing Cargo.toml.",
        ),
    }
    Ok(out)
}

/* ================================================================ */
/* list_files · file_metadata · find_duplicates · copy_files (P2)    */
/*                                                                  */
/* Deterministic file analysis: the model must never sort, count or  */
/* hash by hand (benchmark 2026-11-16: S2-5/6/8, S4-17 all lost      */
/* arithmetic and grouping rounds). Walks reuse the file_search     */
/* discipline: wall-time + entry caps, and dependency/system trees  */
/* are never descended into. find_duplicates only ever REPORTS —    */
/* no deletion capability exists in this family.                    */
/* ================================================================ */

/// One discovered file: absolute-ish path and its byte size.
struct FoundFile {
    path: PathBuf,
    size: u64,
}

/// Bounded walk collecting files (optionally extension-filtered).
/// Reuses the file_search skip list and budget constants. Returns the
/// files and whether the walk hit a cap before finishing.
fn walk_files(
    root: &Path,
    recursive: bool,
    ext: Option<String>,
    time_budget: Duration,
    max_entries: usize,
) -> (Vec<FoundFile>, bool) {
    let want_ext = ext
        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
        .filter(|e| !e.is_empty());
    let mut files: Vec<FoundFile> = Vec::new();
    let mut truncated = false;
    let deadline = Instant::now() + time_budget;
    let mut queue: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut visited = 0usize;
    'walk: loop {
        let Some(dir) = queue.pop() else { break };
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            visited += 1;
            if visited > max_entries || Instant::now() > deadline {
                truncated = true;
                break 'walk;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                if recursive {
                    let name_lc = entry
                        .file_name()
                        .to_string_lossy()
                        .to_lowercase();
                    if !SEARCH_SKIP_DIRS.contains(&name_lc.as_str()) {
                        queue.push(path);
                    }
                }
                continue;
            }
            if let Some(ext) = &want_ext {
                let path_ext = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|s| s.to_lowercase());
                if path_ext.as_deref() != Some(ext.as_str()) {
                    continue;
                }
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push(FoundFile { path, size });
        }
    }
    (files, truncated)
}

/// Shared root-directory validation for the walk family.
fn walk_root(args: &Value) -> Result<PathBuf, String> {
    let raw = args
        .get("root")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("~");
    let root = PathBuf::from(expand_home_path(raw));
    if !root.is_dir() {
        return Err(format!("'{raw}' is not a directory"));
    }
    Ok(root)
}

pub(crate) fn run_list_files(args: &Value) -> Result<String, String> {
    let root = walk_root(args)?;
    let recursive = args.get("recursive").and_then(Value::as_bool).unwrap_or(false);
    let ext = args
        .get("extension")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(e) = &ext {
        if e.trim().is_empty() {
            return Err("extension is empty".into());
        }
    }
    let limit = optional_u64(args, "limit", 20, 1, 100) as usize;
    let (mut files, truncated) =
        walk_files(&root, recursive, ext, SEARCH_TIME_BUDGET, SEARCH_MAX_ENTRIES);
    /* the tool sorts, never the model (plan P2) */
    files.sort_by(|a, b| b.size.cmp(&a.size).then(a.path.cmp(&b.path)));
    let total = files.len();
    let mut out = if total == 0 {
        format!("No files under {}{}", root.display(), if truncated { " (scan truncated)" } else { "" })
    } else {
        format!(
            "{} file(s) under {} — top {} by size{}:",
            total,
            root.display(),
            limit.min(total),
            if truncated { " (scan truncated)" } else { "" }
        )
    };
    for f in files.into_iter().take(limit) {
        out.push_str(&format!("\n  {}  {}", fmt_bytes(f.size), f.path.display()));
    }
    Ok(out)
}

pub(crate) fn run_file_metadata(args: &Value) -> Result<String, String> {
    let raw = required_str(args, "path", "file_metadata")?;
    if raw.trim().is_empty() {
        return Err("path is empty".into());
    }
    let path = PathBuf::from(expand_home_path(raw));
    let md = fs::metadata(&path).map_err(|e| format!("Could not stat '{raw}': {e}"))?;
    let kind = if md.is_dir() { "directory" } else { "file" };
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("(none)");
    let fmt_time = |st: Option<std::time::SystemTime>| -> String {
        st.map(|t| {
            chrono::DateTime::<chrono::Local>::from(t)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "(unknown)".into())
    };
    Ok(format!(
        "Path: {}\nType: {}\nSize: {}\nExtension: {}\nCreated: {}\nModified: {}\nAccessed: {}\nRead-only: {}",
        path.display(),
        kind,
        fmt_bytes(md.len()),
        ext,
        fmt_time(md.created().ok()),
        fmt_time(md.modified().ok()),
        fmt_time(md.accessed().ok()),
        if md.permissions().readonly() { "yes" } else { "no" },
    ))
}

pub(crate) fn run_find_duplicates(args: &Value) -> Result<String, String> {
    let root = walk_root(args)?;
    let limit = optional_u64(args, "limit", 20, 1, 100) as usize;
    let (mut files, truncated) =
        walk_files(&root, true, None, Duration::from_secs(10), SEARCH_MAX_ENTRIES);
    let total_scanned = files.len();
    /* size prefilter (plan P2): only same-size groups can be true
     * duplicates — the cheap check happens before any hashing */
    let mut by_size: std::collections::HashMap<u64, Vec<PathBuf>> =
        std::collections::HashMap::new();
    for f in files.drain(..) {
        by_size.entry(f.size).or_default().push(f.path);
    }
    let candidates: Vec<(u64, Vec<PathBuf>)> = by_size
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .collect();
    if candidates.is_empty() {
        return Ok(format!(
            "No same-size files under {}{} — no duplicates possible. Nothing was deleted.",
            root.display(),
            if truncated { " (scan truncated)" } else { "" }
        ));
    }
    /* hash budget: 60 s and 512 MB total, single files over 256 MB are
     * skipped — a report must always come back within the tool budget */
    const HASH_FILE_CAP: u64 = 256 * 1024 * 1024;
    const HASH_BYTE_BUDGET: u64 = 512 * 1024 * 1024;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut hashed = 0usize;
    let mut hashed_bytes = 0u64;
    let mut budget_hit = false;
    let mut groups: Vec<(u64, String, Vec<PathBuf>)> = Vec::new();
    for (size, paths) in candidates {
        if budget_hit {
            break;
        }
        let mut digests: std::collections::HashMap<String, Vec<PathBuf>> =
            std::collections::HashMap::new();
        for p in paths {
            if size > HASH_FILE_CAP
                || Instant::now() > deadline
                || hashed_bytes + size > HASH_BYTE_BUDGET
            {
                budget_hit = true;
                break;
            }
            let Ok(bytes) = fs::read(&p) else { continue };
            hashed += 1;
            hashed_bytes += size;
            digests.entry(sha256_hex(&bytes)).or_default().push(p);
        }
        for (hex, ps) in digests {
            if ps.len() >= 2 {
                groups.push((size, hex, ps));
            }
        }
    }
    if groups.is_empty() {
        return Ok(format!(
            "No duplicate files under {} ({} files scanned, {} hashed{}). Nothing was deleted.",
            root.display(),
            total_scanned,
            hashed,
            if budget_hit || truncated { ", budget reached — report may be partial" } else { "" }
        ));
    }
    groups.sort_by(|a, b| b.0.cmp(&a.0).then(b.2.len().cmp(&a.2.len())));
    let shown = groups.len().min(limit);
    let mut out = format!(
        "Found {} duplicate group(s) under {} ({} files scanned, {} hashed{}). Nothing was deleted.",
        groups.len(),
        root.display(),
        total_scanned,
        hashed,
        if budget_hit || truncated {
            ", budget reached — report may be partial"
        } else {
            ""
        }
    );
    for (i, (size, hex, paths)) in groups.iter().take(shown).enumerate() {
        out.push_str(&format!(
            "\n{}. {} file(s) × {} — sha256 {}…",
            i + 1,
            paths.len(),
            fmt_bytes(*size),
            &hex[..16]
        ));
        for p in paths.iter().take(10) {
            out.push_str(&format!("\n     {}", p.display()));
        }
        if paths.len() > 10 {
            out.push_str(&format!("\n     … and {} more", paths.len() - 10));
        }
    }
    Ok(out)
}

pub(crate) fn run_copy_files(args: &Value) -> Result<String, String> {
    let sources = args
        .get("sources")
        .and_then(Value::as_array)
        .ok_or_else(|| "Missing required parameter: sources (tool copy_files)".to_string())?;
    if sources.is_empty() {
        return Err("sources is empty".into());
    }
    if sources.len() > 50 {
        return Err(format!(
            "too many sources ({}) — at most 50 per call",
            sources.len()
        ));
    }
    let dest_raw = required_str(args, "dest_dir", "copy_files")?;
    if dest_raw.trim().is_empty() {
        return Err("dest_dir is empty".into());
    }
    let dest = PathBuf::from(expand_home_path(dest_raw));
    if !dest.is_absolute() {
        return Err(format!(
            "dest_dir '{dest_raw}' is relative — give an absolute path (or one starting with ~)"
        ));
    }
    if dest.is_file() {
        return Err(format!("'{dest_raw}' exists as a file, not a folder"));
    }
    /* the destination is a write: the same root policy as write_file */
    check_protected(&dest).map_err(|e| e.replace("deleted", "written"))?;
    if !write_root_allowed(&dest) {
        return Err(format!(
            "'{dest_raw}' is outside the writable roots — see workspace_info"
        ));
    }
    fs::create_dir_all(&dest).map_err(|e| format!("Could not create destination: {e}"))?;
    let mut copied = 0usize;
    let mut skipped: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    for s in sources {
        let Some(sraw) = s.as_str() else {
            errors.push(format!("source entry is not a string: {s}"));
            continue;
        };
        let spath = PathBuf::from(expand_home_path(sraw));
        if !spath.is_file() {
            errors.push(format!("not a file: {sraw}"));
            continue;
        }
        let Some(fname) = spath.file_name() else {
            errors.push(format!("no file name: {sraw}"));
            continue;
        };
        let target = dest.join(fname);
        if target.exists() {
            skipped.push(format!(
                "{} (already exists — nothing overwritten)",
                target.display()
            ));
            continue;
        }
        match fs::copy(&spath, &target) {
            Ok(_) => copied += 1,
            Err(e) => errors.push(format!("copy {sraw} failed: {e}")),
        }
    }
    let mut out = format!(
        "Copied {copied} file(s) into {} (nothing overwritten).",
        dest.display()
    );
    let mut notes = |label: &str, list: &[String]| {
        if list.is_empty() {
            return;
        }
        out.push_str(&format!("\n{label} {}:", list.len()));
        for item in list.iter().take(10) {
            out.push_str(&format!("\n  {item}"));
        }
        if list.len() > 10 {
            out.push_str(&format!("\n  … and {} more", list.len() - 10));
        }
    };
    notes("Skipped", &skipped);
    notes("Errors", &errors);
    Ok(out)
}

/* ---- pure-Rust SHA-256 (plan P2) --------------------------------- */
/* Self-contained: adding a hashing crate is an offline-build risk on */
/* this machine, and the known vectors pin correctness in tests.      */

const SHA256_K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
    0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
    0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
    0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
    0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// SHA-256 of one byte slice, lowercase hex. Single-shot: the duplicate
/// finder caps file sizes at 256 MB before reading.
fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
        0x1f83d9ab, 0x5be0cd19,
    ];
    /* padding: message + 0x80 + zeros + 64-bit big-endian bit length */
    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for chunk in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in chunk.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(SHA256_K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}


/* ================================================================ */
/* v2 — dev & browser (goal.md §4/§5/§6)                             */
/* list_dir · content_search · run_command ·                        */
/* web_fetch · download_file · web_search                           */
/*                                                                  */
/* Safety policy: run_command is a curated program+subcommand        */
/* allowlist (no shell, argv direct) whose write-shaped entries      */
/* (cargo new/init) only CREATE new files/folders, never modify or   */
/* delete existing state — destructive writes stay v2.5-gated;       */
/* download_file writes only into the user's Downloads folder; web   */
/* tools block loopback/private literal hosts so a fetched page      */
/* cannot send the assistant after local services; every web read is */
/* byte-capped.                                                      */
/* ================================================================ */

/* ---------------------------------------------------------------- */
/* list_dir                                                          */
/* ---------------------------------------------------------------- */

/// Hard entry cap: voice replies never need thousands of lines.
const LIST_DIR_MAX_ENTRIES: usize = 200;


pub(crate) fn run_list_dir(args: &Value) -> Result<String, String> {
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

pub(crate) fn run_content_search(args: &Value) -> Result<String, String> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::{reg, temp_root};
    use serde_json::json;

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

    #[test]
    fn file_search_walk_is_breadth_first() {
        /* BUG-005: with limit 1 the alphabetically-first top-level
         * subtree must win. DFS-LIFO popped the last-pushed subtree
         * first, so the late subtree starved the early one. */
        let root = temp_root("fs_bfs");
        fs::create_dir_all(root.join("aaa_sub")).unwrap();
        fs::create_dir_all(root.join("zzz_sub")).unwrap();
        fs::write(root.join("aaa_sub").join("aaa_target.txt"), "x").unwrap();
        fs::write(root.join("zzz_sub").join("zzz_target.txt"), "x").unwrap();
        let out = reg()
            .dispatch(
                "file_search",
                &json!({"query": "target", "limit": 1, "root": root.to_string_lossy()}),
            )
            .unwrap();
        assert!(out.contains("Found 1"), "{out}");
        assert!(out.contains("aaa_sub"), "BFS must reach the first subtree: {out}");
        assert!(!out.contains("zzz_sub"), "{out}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn file_search_skips_nested_repos_on_descent() {
        /* Same dependency-tree semantics as content_search (BUG-005):
         * a nested clone is not descended into from a parent root, but
         * an explicit root INTO the clone still searches it. */
        let root = temp_root("fs_repo");
        fs::create_dir_all(root.join("nested").join(".git")).unwrap();
        fs::write(root.join("nested").join("hidden.txt"), "x").unwrap();
        fs::write(root.join("top.txt"), "x").unwrap();
        let out = reg()
            .dispatch(
                "file_search",
                &json!({"query": "txt", "root": root.to_string_lossy()}),
            )
            .unwrap();
        assert!(out.contains("top.txt"), "{out}");
        assert!(!out.contains("nested"), "{out}");
        let out = reg()
            .dispatch(
                "file_search",
                &json!({
                    "query": "txt",
                    "root": root.join("nested").to_string_lossy()
                }),
            )
            .unwrap();
        assert!(out.contains("hidden.txt"), "{out}");
        fs::remove_dir_all(&root).ok();
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

    /* ---- write_file / replace_file / workspace_info (plan P0) ---- */

    #[test]
    fn write_file_creates_file_and_parents() {
        let root = temp_root("wf_create");
        let target = root.join("sub").join("report.txt");
        let out = reg()
            .dispatch(
                "write_file",
                &json!({"path": target.to_string_lossy(), "content": "hello world"}),
            )
            .unwrap();
        assert!(out.contains("Wrote 11 B"), "{out}");
        assert!(out.contains("(created)"), "{out}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "hello world");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn write_file_refuses_existing_file_untouched() {
        let root = temp_root("wf_exists");
        let f = root.join("dup.txt");
        fs::write(&f, "old").unwrap();
        let err = reg()
            .dispatch(
                "write_file",
                &json!({"path": f.to_string_lossy(), "content": "new"}),
            )
            .unwrap_err();
        assert!(err.contains("replace_file"), "{err}");
        assert_eq!(fs::read_to_string(&f).unwrap(), "old");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn write_file_rejects_relative_paths_and_bad_args() {
        let root = temp_root("wf_args");
        /* content is required */
        assert!(reg()
            .dispatch(
                "write_file",
                &json!({"path": root.join("a.txt").to_string_lossy()})
            )
            .is_err());
        /* relative paths are refused outright: they would resolve
         * against an unpredictable working directory */
        let err = reg()
            .dispatch("write_file", &json!({"path": "report.txt", "content": "x"}))
            .unwrap_err();
        assert!(err.contains("absolute"), "{err}");
        /* oversize content */
        let big = "x".repeat(256 * 1024 + 1);
        let err = reg()
            .dispatch(
                "write_file",
                &json!({"path": root.join("big.txt").to_string_lossy(), "content": big}),
            )
            .unwrap_err();
        assert!(err.contains("too large"), "{err}");
        /* launchable file types are refused */
        let err = reg()
            .dispatch(
                "write_file",
                &json!({"path": root.join("run.bat").to_string_lossy(), "content": "x"}),
            )
            .unwrap_err();
        assert!(err.contains(".bat"), "{err}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn write_file_accepts_forward_slash_paths() {
        let root = temp_root("wf_slash");
        let target = root.join("slash.txt");
        let forward = target.to_string_lossy().replace('\\', "/");
        reg()
            .dispatch("write_file", &json!({"path": forward, "content": "ok"}))
            .unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "ok");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn replace_file_plans_then_replaces() {
        let root = temp_root("rf_plan");
        let f = root.join("report.md");
        /* direct plan/run calls (never reg().dispatch) so the global
         * pending store is not polluted with an armed action */
        assert!(
            plan_replace_file(&json!({"path": f.to_string_lossy(), "content": "new"})).is_err()
        );
        fs::write(&f, "old content").unwrap();
        let plan =
            plan_replace_file(&json!({"path": f.to_string_lossy(), "content": "new"})).unwrap();
        assert!(plan.contains("Overwrite"), "{plan}");
        assert!(plan.contains("11 B"), "{plan}");
        let out =
            run_replace_file(&json!({"path": f.to_string_lossy(), "content": "new"})).unwrap();
        assert!(out.contains("Replaced"), "{out}");
        assert!(out.contains("was 11 B"), "{out}");
        assert_eq!(fs::read_to_string(&f).unwrap(), "new");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn write_root_allowed_scopes() {
        let home = PathBuf::from(home_dir_string());
        /* strictly under an allowed root */
        assert!(write_root_allowed(&home.join("Desktop").join("x.txt")));
        assert!(write_root_allowed(&std::env::temp_dir().join("x.txt")));
        /* the roots themselves are not writable targets */
        assert!(!write_root_allowed(&home));
        assert!(!write_root_allowed(&std::env::temp_dir()));
        /* drive roots are outside every allowed root */
        assert!(!write_root_allowed(&PathBuf::from("C:\\")));
        /* the sibling-prefix trap: sohan2 is NOT under sohan */
        let sibling = home
            .parent()
            .unwrap_or(&home)
            .join(format!(
                "{}2",
                home.file_name().and_then(|n| n.to_str()).unwrap_or("x")
            ));
        assert!(!write_root_allowed(&sibling));
    }

    #[test]
    fn workspace_info_reports_the_roots() {
        /* cargo runs tests with the package root as cwd, which contains
         * a Cargo.toml, so a workspace root always resolves here */
        let out = reg().dispatch("workspace_info", &json!({})).unwrap();
        assert!(out.contains("Workspace root:"), "{out}");
        assert!(out.contains("user profile"), "{out}");
    }

    /* ---- list_files / file_metadata / find_duplicates / copy_files (P2) ---- */

    #[test]
    fn list_files_sorts_by_size_and_filters_extension() {
        let root = temp_root("lf_sort");
        fs::write(root.join("big.txt"), "x".repeat(300)).unwrap();
        fs::write(root.join("small.txt"), "y".repeat(30)).unwrap();
        fs::write(root.join("note.md"), "z".repeat(100)).unwrap();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub").join("nested.md"), "n".repeat(500)).unwrap();
        /* non-recursive default: only the root's own files */
        let out = reg()
            .dispatch("list_files", &json!({"root": root.to_string_lossy()}))
            .unwrap();
        assert!(out.contains("3 file(s)"), "{out}");
        assert!(!out.contains("nested.md"), "{out}");
        /* recursive: the sub folder is descended into */
        let out = reg()
            .dispatch(
                "list_files",
                &json!({"root": root.to_string_lossy(), "recursive": true}),
            )
            .unwrap();
        assert!(out.contains("4 file(s)"), "{out}");
        /* extension filter + deterministic sort: nested.md (500 B) beats
         * note.md (100 B) — the tool sorts, never the model */
        let out = reg()
            .dispatch(
                "list_files",
                &json!({
                    "root": root.to_string_lossy(),
                    "recursive": true,
                    "extension": ".md"
                }),
            )
            .unwrap();
        assert!(out.contains("2 file(s)"), "{out}");
        let nested = out.find("nested.md").unwrap();
        let note = out.find("note.md").unwrap();
        assert!(nested < note, "{out}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn file_metadata_reports_shape_and_errors() {
        let root = temp_root("fm_shape");
        let f = root.join("report.txt");
        fs::write(&f, "hello").unwrap();
        let out = reg()
            .dispatch("file_metadata", &json!({"path": f.to_string_lossy()}))
            .unwrap();
        assert!(out.contains("Type: file"), "{out}");
        assert!(out.contains("Size: 5 B"), "{out}");
        assert!(out.contains("Extension: txt"), "{out}");
        assert!(out.contains("Created: "), "{out}");
        assert!(out.contains("Read-only: "), "{out}");
        let out = reg()
            .dispatch("file_metadata", &json!({"path": root.to_string_lossy()}))
            .unwrap();
        assert!(out.contains("Type: directory"), "{out}");
        let err = reg()
            .dispatch(
                "file_metadata",
                &json!({"path": root.join("missing.bin").to_string_lossy()}),
            )
            .unwrap_err();
        assert!(err.contains("Could not stat"), "{err}");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn find_duplicates_groups_by_hash_and_never_deletes() {
        let root = temp_root("fd_groups");
        let a = root.join("a.bin");
        let b = root.join("b.bin");
        let c = root.join("c.bin");
        fs::write(&a, "identical content").unwrap();
        fs::write(&b, "identical content").unwrap();
        fs::write(&c, "different").unwrap();
        let out = reg()
            .dispatch("find_duplicates", &json!({"root": root.to_string_lossy()}))
            .unwrap();
        assert!(out.contains("Found 1 duplicate group(s)"), "{out}");
        assert!(out.contains("2 file(s)"), "{out}");
        assert!(out.contains("Nothing was deleted"), "{out}");
        /* report-only: every file still on disk */
        assert!(a.is_file() && b.is_file() && c.is_file());
        /* the no-duplicates case is an honest empty report */
        let root2 = temp_root("fd_none");
        fs::write(root2.join("solo.txt"), "only one").unwrap();
        let out = reg()
            .dispatch("find_duplicates", &json!({"root": root2.to_string_lossy()}))
            .unwrap();
        assert!(out.contains("Nothing was deleted"), "{out}");
        fs::remove_dir_all(root).ok();
        fs::remove_dir_all(root2).ok();
    }

    #[test]
    fn sha256_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        /* the 448-bit standard vector crosses a block boundary */
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        /* padding boundary: 55 bytes fit one block's message area, 56 do
         * not — both must hash differently */
        assert_ne!(
            sha256_hex("a".repeat(55).as_bytes()),
            sha256_hex("a".repeat(56).as_bytes())
        );
    }

    #[test]
    fn copy_files_copies_and_never_overwrites() {
        let src = temp_root("cf_src");
        let dst = temp_root("cf_dst");
        fs::write(src.join("one.txt"), "111").unwrap();
        fs::write(src.join("two.txt"), "222").unwrap();
        fs::write(dst.join("one.txt"), "original").unwrap();
        let out = reg()
            .dispatch(
                "copy_files",
                &json!({
                    "sources": [
                        src.join("one.txt").to_string_lossy(),
                        src.join("two.txt").to_string_lossy()
                    ],
                    "dest_dir": dst.to_string_lossy()
                }),
            )
            .unwrap();
        assert!(out.contains("Copied 1 file(s)"), "{out}");
        assert!(out.contains("Skipped 1:"), "{out}");
        assert_eq!(fs::read_to_string(dst.join("one.txt")).unwrap(), "original");
        assert_eq!(fs::read_to_string(dst.join("two.txt")).unwrap(), "222");
        /* a missing source lands in Errors but the tool still reports */
        let out = reg()
            .dispatch(
                "copy_files",
                &json!({
                    "sources": ["Z:\\definitely\\missing.txt"],
                    "dest_dir": dst.to_string_lossy()
                }),
            )
            .unwrap();
        assert!(out.contains("Copied 0 file(s)"), "{out}");
        assert!(out.contains("Errors 1:"), "{out}");
        /* empty source list is rejected outright */
        let err = reg()
            .dispatch(
                "copy_files",
                &json!({"sources": [], "dest_dir": dst.to_string_lossy()}),
            )
            .unwrap_err();
        assert!(err.contains("sources is empty"), "{err}");
        fs::remove_dir_all(src).ok();
        fs::remove_dir_all(dst).ok();
    }
}
