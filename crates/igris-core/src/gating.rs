//! Gated actions (v2.5): the two-step confirmation machinery for tools
//! that act on the user's machine in ways that are hard to undo (closing
//! apps, deleting files, shutting the PC down).
//!
//! Flow (design settled 2026-11-15): a gated tool's FIRST call only
//! validates the arguments and builds a plan — nothing executes. The call
//! arms a `PendingAction` in the file-backed pending store and the tool
//! returns a loud NOT EXECUTED — CONFIRMATION REQUIRED result; the model
//! announces the plan to the user and waits. When the user confirms, the
//! model re-calls the same tool with the same arguments in a LATER turn;
//! the dispatch layer recognizes the exact (tool, canonicalized args)
//! match against a pending armed in an earlier turn — or in an earlier
//! process (the store survives restarts, so a CLI one-shot arm + confirm
//! works across runs) — and only then executes.
//!
//! The same-turn bypass is closed structurally: a process-wide turn
//! counter is bumped once per agent turn, and freshness requires the
//! pending to be armed strictly before the current turn (or by another
//! process). Re-calling within the same turn re-arms instead of
//! executing. Drifted arguments are a new intent: they re-arm and never
//! execute the older intent (one pending slot per tool).
//!
//! Pendings expire after two minutes, at most eight tools hold a pending
//! at once (oldest evicted), and a consumed pending is spent on the
//! execution attempt even if the tool then fails — a retry always needs
//! a fresh arm plus a fresh confirmation.

use crate::memory::{self, MemoryKind};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/* ================================================================ */
/* turn counter                                                      */
/* ================================================================ */

/// Process-wide agent-turn counter: bumped once per user utterance at
/// `agent_turn` entry — the single choke point every front-end goes
/// through (the voice loop and the CLI chat both call it). Gated
/// execution requires `armed_turn < current_turn`, which makes a
/// same-turn re-call impossible by construction.
static TURN: AtomicU64 = AtomicU64::new(0);

/// Begin a new agent turn; returns its number (the first turn is 1).
pub fn begin_turn() -> u64 {
    TURN.fetch_add(1, Ordering::SeqCst) + 1
}

/// The number of the turn currently in progress (0 before the first).
pub fn current_turn() -> u64 {
    TURN.load(Ordering::SeqCst)
}

/// Current Unix time in milliseconds, for expiry and eviction order.
pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/* ================================================================ */
/* pending store                                                     */
/* ================================================================ */

/// How long an armed action stays confirmable.
pub const EXPIRY_MILLIS: u64 = 120_000;
/// At most this many tools hold a pending at once; arming beyond the cap
/// evicts the oldest.
pub const MAX_PENDING: usize = 8;

/// One armed action awaiting confirmation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PendingAction {
    pub tool: String,
    /// Canonicalized (key-sorted) JSON of the arguments — the exact
    /// signature the confirming re-call must repeat.
    pub args: String,
    /// The plan announced on arm (what the model showed the user).
    pub plan: String,
    /// The turn in which the pending was armed (see the turn counter).
    pub armed_turn: u64,
    /// The process that armed it: a pending from an earlier process is
    /// fresh for any re-call there, because every process handles its
    /// own turns — this is what makes the file-backed store work across
    /// CLI one-shot runs, where the turn counter restarts at 1.
    pub armed_pid: u32,
    /// Unix millis at arm time: expiry and eviction order.
    pub armed_at_millis: u64,
}

/// Freshness rule: the pending must be armed strictly before the current
/// turn in the same process (same-turn re-calls never execute), or armed
/// by a different process (every process owns its turn numbering), and
/// it must not be expired.
fn is_fresh(p: &PendingAction, turn: u64, pid: u32, now: u64) -> bool {
    (p.armed_turn < turn || p.armed_pid != pid)
        && now.saturating_sub(p.armed_at_millis) <= EXPIRY_MILLIS
}

/// File-backed pending store: `%APPDATA%\igris\pending.json` — separate
/// from memory.json on purpose, so confirmation state never leaks into
/// the recallable memory and vice versa. Every mutation writes
/// atomically (temp file + rename), mirroring the memory store.
#[derive(Debug)]
pub struct PendingStore {
    path: PathBuf,
    pending: Vec<PendingAction>,
}

#[derive(Serialize, Deserialize)]
struct PendingFile {
    version: u32,
    pending: Vec<PendingAction>,
}

impl PendingStore {
    /// Strict opener: missing file = empty store; unreadable or invalid
    /// file = error. The process-wide store degrades to empty instead —
    /// ephemeral confirmation state is never worth a quarantine dance.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self {
                path,
                pending: Vec::new(),
            });
        }
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("gating: cannot read {}: {e}", path.display()))?;
        let file: PendingFile = serde_json::from_str(&text)
            .map_err(|e| format!("gating: cannot parse {}: {e}", path.display()))?;
        Ok(Self {
            path,
            pending: file.pending,
        })
    }

    /// Lenient opener for the process-wide store: an unreadable or
    /// corrupt file means "no pending actions", never a broken agent.
    fn open_or_empty(path: &Path) -> Self {
        match Self::open(path) {
            Ok(store) => store,
            Err(e) => {
                eprintln!("gating: {e}; starting with no pending actions");
                Self {
                    path: path.to_path_buf(),
                    pending: Vec::new(),
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Arm (or re-arm) a tool: replaces any pending of the same tool
    /// (drifted arguments are a new intent — the older one can never
    /// execute afterwards), drops expired entries, and evicts the oldest
    /// beyond the cap.
    pub fn arm(
        &mut self,
        tool: &str,
        args: String,
        plan: &str,
        turn: u64,
        pid: u32,
        now: u64,
    ) {
        self.pending.retain(|p| {
            p.tool != tool && now.saturating_sub(p.armed_at_millis) <= EXPIRY_MILLIS
        });
        self.pending.push(PendingAction {
            tool: tool.to_string(),
            args,
            plan: plan.to_string(),
            armed_turn: turn,
            armed_pid: pid,
            armed_at_millis: now,
        });
        while self.pending.len() > MAX_PENDING {
            let Some(oldest) = self
                .pending
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| p.armed_at_millis)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.pending.remove(oldest);
        }
        self.flush();
    }

    /// Consume the pending of `tool` when the re-call repeats its exact
    /// argument signature AND the pending is fresh. Any other case —
    /// same-turn re-call, drifted arguments, expired pending, different
    /// tool — leaves the store untouched and returns `None`, so the
    /// caller re-arms instead of executing.
    pub fn take_fresh(
        &mut self,
        tool: &str,
        args: &str,
        turn: u64,
        pid: u32,
        now: u64,
    ) -> Option<PendingAction> {
        let idx = self
            .pending
            .iter()
            .position(|p| p.tool == tool && p.args == args && is_fresh(p, turn, pid, now))?;
        let action = self.pending.remove(idx);
        self.flush();
        Some(action)
    }

    /// Atomic write: serialize to `<path>.tmp`, then rename over the
    /// target (Windows `fs::rename` replaces an existing file). Errors
    /// are logged, never propagated: the in-memory store still enforces
    /// confirmation within this process, so a failed write only costs
    /// cross-process confirmation, and the next successful flush heals
    /// the file.
    fn flush(&self) {
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = PendingFile {
            version: 1,
            pending: self.pending.clone(),
        };
        let result = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("gating: serialize failed: {e}"))
            .and_then(|text| {
                let tmp = self.path.with_extension("json.tmp");
                std::fs::write(&tmp, text)
                    .map_err(|e| format!("gating: cannot write {}: {e}", tmp.display()))?;
                std::fs::rename(&tmp, &self.path)
                    .map_err(|e| format!("gating: cannot replace {}: {e}", self.path.display()))
            });
        if let Err(e) = result {
            eprintln!("gating: {e}");
        }
    }
}

/* ================================================================ */
/* process-wide store                                                */
/* ================================================================ */

/// Default location: `%APPDATA%\igris\pending.json` (same trust domain
/// as memory.json and tools.toml); falls back to the temp dir when the
/// config dir cannot be resolved, degrading gracefully like the memory
/// store.
pub fn default_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("igris")
        .join("pending.json")
}

static PENDING: std::sync::LazyLock<std::sync::Mutex<PendingStore>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(PendingStore::open_or_empty(&default_path())));

/// Process-wide pending store behind a mutex; the registry's dispatch
/// locks it per gated call.
pub fn global() -> &'static std::sync::Mutex<PendingStore> {
    &PENDING
}

/* ================================================================ */
/* announcement + activity log                                       */
/* ================================================================ */

/// The tool result a gated call returns when it ARMED instead of
/// executing. The dispatch status stays "ok" (a third status value
/// reliably confuses the model's envelope handling — v1.1 lesson); the
/// loud prefix carries the meaning.
pub fn armed_result(plan: &str) -> String {
    format!(
        "NOT EXECUTED — CONFIRMATION REQUIRED\n\
         {plan}\n\n\
         Nothing has happened yet. Tell the user exactly what this action \
         will do and wait for a clear yes. If the user confirms, call this \
         exact tool again with identical arguments and it will execute. \
         If the user declines or asks for something different, do not \
         call it again."
    )
}

/// Append a gated-action line to the activity log (the same store the
/// activity_log tool writes), so "what did you do" questions can answer
/// for gated actions. Logging is an observability nicety, never a
/// precondition: poison and store errors are swallowed.
pub fn log_activity(action: &str, tags: &[String]) {
    let Ok(mut store) = memory::global().lock() else {
        return;
    };
    let mut tag_list = vec!["gated".to_string()];
    tag_list.extend(tags.iter().cloned());
    let _ = store.add(MemoryKind::Activity, action, &tag_list, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_counter() -> &'static AtomicU64 {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        &COUNTER
    }

    /// Fresh scratch path (the file itself is not created until the
    /// first flush); callers clean up the parent dir.
    fn temp_store_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "igris_gating_{}_{}_{}_{}",
            tag,
            std::process::id(),
            nanos,
            temp_counter().fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("pending.json")
    }

    /* ---- turn counter ---- */

    #[test]
    fn begin_turn_increments_monotonically() {
        let a = begin_turn();
        let b = begin_turn();
        assert!(b > a, "turn numbers must strictly increase");
        assert!(current_turn() >= b);
    }

    /* ---- open / roundtrip ---- */

    #[test]
    fn store_opens_missing_file_as_empty() {
        let path = temp_store_path("missing");
        let store = PendingStore::open(&path).unwrap();
        assert!(store.is_empty());
        assert_eq!(store.path(), path.as_path());
    }

    #[test]
    fn store_rejects_corrupt_file() {
        let path = temp_store_path("corrupt");
        std::fs::write(&path, "not json at all").unwrap();
        let err = PendingStore::open(&path).unwrap_err();
        assert!(err.contains("cannot parse"), "{err}");
    }

    #[test]
    fn arm_then_fresh_take_roundtrips_and_persists() {
        let path = temp_store_path("roundtrip");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", r#"{"app":"notepad"}"#.into(), "Close notepad", 3, 111, 5_000);
        assert_eq!(store.len(), 1);
        let taken = store
            .take_fresh("close_app", r#"{"app":"notepad"}"#, 4, 111, 6_000)
            .expect("fresh cross-turn call takes the pending");
        assert_eq!(taken.tool, "close_app");
        assert_eq!(taken.args, r#"{"app":"notepad"}"#);
        assert_eq!(taken.plan, "Close notepad");
        assert_eq!(taken.armed_turn, 3);
        assert_eq!(taken.armed_pid, 111);
        assert!(store.is_empty(), "consumed pending is spent");
        /* the spent state is on disk too */
        let reopened = PendingStore::open(&path).unwrap();
        assert!(reopened.is_empty());
    }

    /* ---- freshness rules ---- */

    #[test]
    fn same_turn_recall_never_takes() {
        let path = temp_store_path("sameturn");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", r#"{"app":"notepad"}"#.into(), "p", 3, 111, 5_000);
        assert!(
            store
                .take_fresh("close_app", r#"{"app":"notepad"}"#, 3, 111, 5_500)
                .is_none(),
            "same-turn re-call must re-arm, not execute"
        );
        assert_eq!(store.len(), 1, "the pending stays armed");
    }

    #[test]
    fn cross_process_recall_is_fresh_even_in_the_same_turn_number() {
        let path = temp_store_path("crossproc");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", r#"{"app":"notepad"}"#.into(), "p", 3, 111, 5_000);
        let taken = store
            .take_fresh("close_app", r#"{"app":"notepad"}"#, 3, 222, 5_500)
            .expect("another process is always a new turn");
        assert_eq!(taken.armed_pid, 111);
    }

    #[test]
    fn drifted_args_never_take() {
        let path = temp_store_path("drift");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("delete_file", r#"{"path":"a.txt"}"#.into(), "p", 3, 111, 5_000);
        assert!(
            store
                .take_fresh("delete_file", r#"{"path":"b.txt"}"#, 4, 111, 5_500)
                .is_none(),
            "different arguments are a new intent"
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn different_tool_never_takes() {
        let path = temp_store_path("othertool");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", r#"{"app":"notepad"}"#.into(), "p", 3, 111, 5_000);
        assert!(
            store
                .take_fresh("shutdown", r#"{"app":"notepad"}"#, 4, 111, 5_500)
                .is_none()
        );
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn expired_pending_is_not_fresh_but_boundary_is() {
        let path = temp_store_path("expiry");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", r#"{"app":"notepad"}"#.into(), "p", 3, 111, 5_000);
        /* exactly at the expiry boundary it is still confirmable */
        assert!(
            store
                .take_fresh("close_app", r#"{"app":"notepad"}"#, 4, 111, 5_000 + EXPIRY_MILLIS)
                .is_some()
        );
        /* one millis later it is not */
        store.arm("close_app", r#"{"app":"notepad"}"#.into(), "p", 3, 111, 5_000);
        assert!(
            store
                .take_fresh(
                    "close_app",
                    r#"{"app":"notepad"}"#,
                    4,
                    111,
                    5_000 + EXPIRY_MILLIS + 1
                )
                .is_none(),
            "expired pendings re-arm"
        );
    }

    /* ---- arm mechanics ---- */

    #[test]
    fn arm_replaces_same_tool_and_keeps_others() {
        let path = temp_store_path("replace");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", r#"{"app":"notepad"}"#.into(), "old plan", 3, 111, 5_000);
        store.arm("delete_file", r#"{"path":"a.txt"}"#.into(), "p2", 3, 111, 5_100);
        store.arm("close_app", r#"{"app":"calc"}"#.into(), "new plan", 4, 111, 5_200);
        assert_eq!(store.len(), 2, "one slot per tool");
        assert!(
            store
                .take_fresh("close_app", r#"{"app":"notepad"}"#, 5, 111, 5_300)
                .is_none(),
            "the replaced intent can never execute"
        );
        let taken = store
            .take_fresh("close_app", r#"{"app":"calc"}"#, 5, 111, 5_300)
            .expect("the new intent is armed");
        assert_eq!(taken.plan, "new plan");
        assert!(store.take_fresh("delete_file", r#"{"path":"a.txt"}"#, 5, 111, 5_300).is_some());
    }

    #[test]
    fn cap_eight_evicts_oldest() {
        let path = temp_store_path("cap");
        let mut store = PendingStore::open(&path).unwrap();
        for i in 0..9 {
            store.arm(
                &format!("tool{i}"),
                "{}".to_string(),
                "p",
                3,
                111,
                1_000 + i as u64,
            );
        }
        assert_eq!(store.len(), MAX_PENDING);
        assert!(
            store.take_fresh("tool0", "{}", 4, 111, 2_000).is_none(),
            "the oldest pending was evicted"
        );
        assert!(
            store.take_fresh("tool8", "{}", 4, 111, 2_000).is_some(),
            "the newest pending survived"
        );
    }

    #[test]
    fn arm_purges_expired_entries() {
        let path = temp_store_path("purge");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", "{}".into(), "p", 3, 111, 5_000);
        store.arm("delete_file", "{}".into(), "p", 3, 111, 5_000);
        /* both are expired by now */
        store.arm("shutdown", "{}".into(), "p", 4, 111, 5_000 + EXPIRY_MILLIS + 1);
        assert_eq!(store.len(), 1, "expired entries are dropped on arm");
        assert!(store.take_fresh("shutdown", "{}", 5, 111, 6_000).is_some());
    }

    /* ---- flush hygiene ---- */

    #[test]
    fn flush_is_atomic_and_leaves_no_tmp_file() {
        let path = temp_store_path("atomic");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", "{}".into(), "p", 3, 111, 5_000);
        let tmp = path.with_extension("json.tmp");
        assert!(!tmp.exists(), "the temp file must be renamed away");
        let reopened = PendingStore::open(&path).unwrap();
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.pending[0].tool, "close_app");
    }

    #[test]
    fn flush_creates_missing_parent_dir() {
        let dir = temp_store_path("mkdir");
        let path = dir.parent().unwrap().join("subdir").join("pending.json");
        let mut store = PendingStore::open(&path).unwrap();
        store.arm("close_app", "{}".into(), "p", 3, 111, 5_000);
        assert!(path.exists());
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    /* ---- announcement ---- */

    #[test]
    fn armed_result_is_loud_and_carries_the_plan() {
        let text = armed_result("Close notepad");
        assert!(text.starts_with("NOT EXECUTED — CONFIRMATION REQUIRED"), "{text}");
        assert!(text.contains("Close notepad"), "{text}");
        assert!(text.contains("identical arguments"), "{text}");
    }
}
