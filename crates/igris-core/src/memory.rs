//! Persistent assistant memory (v1.2, goal.md §7) — hybrid design.
//!
//! Decision 2026-11-14 (user choice after a source-level examination of
//! `mem0-rust` 0.2.0): keep our own persistent synchronous store and vendor
//! only the genuinely reusable math from mem0-rust — the cosine similarity
//! kernel from its in-memory vector store and the hash-of-tokens embedder
//! from its mock embedder (both MIT, see the vendored block below).
//!
//! What is deliberately ours, and why:
//! - **Persistence**: mem0-rust outsources durable storage to external
//!   servers (Qdrant / Postgres+pgvector / Redis) and keeps its default
//!   in-memory store volatile — restart means amnesia. A personal assistant
//!   must survive restarts, so the store here is a local JSON file under
//!   `%APPDATA%\igris\` (same trust domain as `tools.toml`), written
//!   atomically (temp file + rename) on every mutation.
//! - **Sync API**: mem0-rust is tokio-async end to end; IGRIS tools are
//!   sync closures on worker threads. No second runtime in the process.
//! - **Extraction at the tool layer**: mem0-rust's inference path spends
//!   two LLM calls per write (fact extraction + update decisions). Here the
//!   model itself structures the fact when it calls `memory_save` /
//!   `create_note` / `reminder_set`, with the user's explicit intent — no
//!   hidden LLM calls, no added write latency.
//! - **No vectors on disk**: embeddings are recomputed at recall time from
//!   the stored text (O(tokens × 128) per record — negligible at a personal
//!   assistant's scale of hundreds of records) so the file stays
//!   human-readable and small.
//!
//! Recall is hybrid: exact substring and tag matches rank first
//! (deterministic, explainable), and the vendored hash-embedding cosine
//! similarity ranks token-overlap neighbours beneath them. At assistant
//! scale this behaves like weighted token overlap — which is exactly what
//! the upstream default (MockEmbedder + InMemoryStore) would have delivered
//! anyway, minus the persistence we had to build.

use std::path::{Path, PathBuf};

pub const EMBED_DIMS: usize = 128;

/// Cosine similarity below which a non-keyword candidate is not returned.
const RECALL_SIMILARITY_THRESHOLD: f32 = 0.2;

/* ================================================================ */
/* Vendored from mem0-rust 0.2.0                                     */
/*                                                                   */
/* Copyright (c) mem0-rust contributors — MIT license                */
/* https://github.com/YASSERRMD/mem0-rust                            */
/*                                                                   */
/* Adapted from `src/vector_stores/memory.rs` (cosine_similarity) and */
/* `src/embeddings/mock.rs` (hash-of-tokens embedder). Upstream is    */
/* async-trait based; here the kernels are plain sync functions with  */
/* identical math. Attribution kept per the vendoring precedent set   */
/* by toolparse.rs (ZeroClaw).                                       */
/* ================================================================ */

/// Cosine similarity of two vectors (0.0 for empty/mismatched shapes).
fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }

    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;

    for (va, vb) in a.iter().zip(b.iter()) {
        dot += va * vb;
        norm_a += va * va;
        norm_b += vb * vb;
    }

    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }

    dot / (norm_a.sqrt() * norm_b.sqrt())
}

/// Deterministic hash-of-tokens embedding (upstream's MockEmbedder).
/// Tokens collide into shared dimensions, so cosine over two embeddings
/// behaves like magnitude-weighted token overlap — not semantics.
pub fn hash_embed(text: &str) -> Vec<f32> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut vector = vec![0.0f32; EMBED_DIMS];

    for token in text.split_whitespace() {
        let mut hasher = DefaultHasher::new();
        token.to_lowercase().hash(&mut hasher);
        let hash = hasher.finish();
        let idx = (hash as usize) % EMBED_DIMS;
        let sign = if hash & 1 == 0 { 1.0 } else { -1.0 };
        let magnitude = 1.0 + ((hash >> 1) as f32 / u64::MAX as f32);
        vector[idx] += sign * magnitude;
    }

    // Normalize
    let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }

    vector
}

/* ================================================================ */
/* Store                                                             */
/* ================================================================ */

/// What kind of record this is. The model picks via the tool it calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Memory,
    Note,
    Activity,
    Reminder,
}

impl MemoryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Note => "note",
            Self::Activity => "activity",
            Self::Reminder => "reminder",
        }
    }
}

/// One durable record. `content` is the user-facing text; everything else
/// is bookkeeping. Stored as pretty JSON so the file stays inspectable.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryRecord {
    pub id: String,
    pub kind: MemoryKind,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub created_at: String,
    /// Reminders only: when it is due, as ISO 8601 or the model's own
    /// wording (no parsing dependency — recall shows it verbatim).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
}

/// On-disk envelope (versioned for future migrations).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryFile {
    pub version: u32,
    #[serde(default)]
    pub records: Vec<MemoryRecord>,
}

/// A recall hit: the record, its score, and how it matched
/// (`exact` substring, `tag`, or `similar` via the vendored kernel).
#[derive(Debug, Clone)]
pub struct ScoredMemory {
    pub record: MemoryRecord,
    pub score: f32,
    pub via: &'static str,
}

/// Sync, file-backed store. Every mutation flushes atomically before
/// returning, so a crash can lose at most the in-flight write.
#[derive(Debug)]
pub struct MemoryStore {
    path: PathBuf,
    records: Vec<MemoryRecord>,
    seq: std::sync::atomic::AtomicU64,
}

impl MemoryStore {
    /// Strict opener: missing file = empty store; unreadable or invalid
    /// file = error (the process-wide store quarantines instead — see
    /// [`open_or_quarantine`]).
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        if !path.exists() {
            return Ok(Self::empty(path));
        }
        let text = fs_read(&path)?;
        let file: MemoryFile =
            serde_json::from_str(&text).map_err(|e| format!("memory: cannot parse {}: {e}", path.display()))?;
        Ok(Self {
            path,
            records: file.records,
            seq: std::sync::atomic::AtomicU64::new(1),
        })
    }

    fn empty(path: PathBuf) -> Self {
        Self {
            path,
            records: Vec::new(),
            seq: std::sync::atomic::AtomicU64::new(1),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn records(&self) -> &[MemoryRecord] {
        &self.records
    }

    /// Append a record. Exact duplicates (same kind, case-insensitively
    /// equal content) are not re-inserted — the returned bool says whether
    /// the record already existed, so the caller can answer "already
    /// saved" instead of accumulating copies.
    pub fn add(
        &mut self,
        kind: MemoryKind,
        content: &str,
        tags: &[String],
        due_at: Option<String>,
    ) -> Result<(MemoryRecord, bool), String> {
        let content = content.trim();
        if content.is_empty() {
            return Err(format!("memory: {} content must not be empty", kind.label()));
        }
        if kind == MemoryKind::Reminder {
            let due_ok = due_at.as_ref().map(|d| !d.trim().is_empty()).unwrap_or(false);
            if !due_ok {
                return Err("memory: a reminder needs a due time".to_string());
            }
        }

        let content_lc = content.to_lowercase();
        if let Some(existing) = self
            .records
            .iter()
            .find(|r| r.kind == kind && r.content.to_lowercase() == content_lc)
        {
            return Ok((existing.clone(), true));
        }

        /* Tags: trimmed, lowercased, sorted + deduped so file bytes are
         * deterministic across runs. */
        let mut tag_list: Vec<String> = tags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        tag_list.sort();
        tag_list.dedup();

        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let id = format!("m{millis}-{}", self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let record = MemoryRecord {
            id,
            kind,
            content: content.to_string(),
            tags: tag_list,
            created_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            due_at: due_at.map(|d| d.trim().to_string()),
        };
        self.records.push(record.clone());
        self.flush()?;
        Ok((record, false))
    }

    /// Hybrid recall: `tags` (when given) restrict to records carrying any
    /// of them; scoring is exact substring > tag match > vendored cosine
    /// (dropped below [`RECALL_SIMILARITY_THRESHOLD`]); ties break toward
    /// the most recent record. Scans every record — trivial at assistant
    /// scale, and there is no index to keep in sync.
    pub fn recall(&self, query: &str, tags: &[String], limit: usize) -> Vec<ScoredMemory> {
        let query = query.trim();
        if query.is_empty() {
            return Vec::new();
        }
        let query_lc = query.to_lowercase();
        let query_tags: Vec<String> = tags
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        let query_emb = hash_embed(&query_lc);

        let mut scored: Vec<ScoredMemory> = Vec::new();
        for record in &self.records {
            if !query_tags.is_empty() && !record.tags.iter().any(|t| query_tags.contains(t)) {
                continue;
            }
            let content_lc = record.content.to_lowercase();
            let (score, via) = if content_lc.contains(&query_lc) {
                (1.0, "exact")
            } else if record
                .tags
                .iter()
                .any(|t| t.len() >= 3 && query_lc.contains(t.as_str()))
                /* The kind label is an implicit tag, so "what are my
                 * reminders" / "show my notes" surface the whole kind
                 * even when no saved word matches. Plurals: +s, and
                 * y -> ies ("activities", "memories"). */
                || kind_label_in_query(record.kind, &query_lc)
            {
                (0.9, "tag")
            } else {
                let hay = format!("{} {}", content_lc, record.tags.join(" "));
                let sim = cosine_similarity(&query_emb, &hash_embed(&hay));
                if sim < RECALL_SIMILARITY_THRESHOLD {
                    continue;
                }
                (sim, "similar")
            };
            scored.push(ScoredMemory {
                record: record.clone(),
                score,
                via,
            });
        }

        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.record.created_at.cmp(&a.record.created_at))
        });
        scored.truncate(limit.max(1));
        scored
    }

    /// Atomic write: serialize to `<path>.tmp`, then rename over the
    /// target (Windows `fs::rename` replaces an existing file).
    fn flush(&self) -> Result<(), String> {
        let file = MemoryFile {
            version: 1,
            records: self.records.clone(),
        };
        let text = serde_json::to_string_pretty(&file).map_err(|e| format!("memory: serialize failed: {e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        fs_write(&tmp, &text)?;
        std::fs::rename(&tmp, &self.path)
            .map_err(|e| format!("memory: cannot replace {}: {e}", self.path.display()))
    }
}

/// Does the query mention this kind by label (singular or plural)?
fn kind_label_in_query(kind: MemoryKind, query_lc: &str) -> bool {
    let label = kind.label();
    if query_lc.contains(label) {
        return true;
    }
    let plural = match label.strip_suffix('y') {
        Some(stem) => format!("{stem}ies"),
        None => format!("{label}s"),
    };
    query_lc.contains(&plural)
}

fn fs_read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("memory: cannot read {}: {e}", path.display()))
}

fn fs_write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("memory: cannot write {}: {e}", path.display()))
}

/* ================================================================ */
/* Process-wide store                                                */
/* ================================================================ */

/// Default location: `%APPDATA%\igris\memory.json` (same trust domain as
/// `tools.toml`); falls back to the temp dir when the config dir cannot be
/// resolved, degrading gracefully rather than panicking.
pub fn default_path() -> PathBuf {
    let dir = dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("igris");
    dir.join("memory.json")
}

/// Process-wide store behind a mutex; tool closures lock it per call.
/// A corrupt file never bricks the assistant: it is quarantined as
/// `memory.json.corrupt-<stamp>` (nothing is destroyed) and the store
/// starts fresh.
static MEMORY: std::sync::LazyLock<std::sync::Mutex<MemoryStore>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(open_or_quarantine(&default_path())));

pub fn global() -> &'static std::sync::Mutex<MemoryStore> {
    &MEMORY
}

fn open_or_quarantine(path: &Path) -> MemoryStore {
    match MemoryStore::open(path) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("memory: {e}");
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            let quarantine = path.with_extension(format!("json.corrupt-{stamp}"));
            if std::fs::rename(path, &quarantine).is_ok() {
                eprintln!("memory: corrupt file moved to {}; starting fresh", quarantine.display());
            } else {
                eprintln!("memory: could not quarantine; starting with an empty store");
            }
            /* Either quarantined or not, the in-process store starts
             * empty; the next successful write replaces what is on disk. */
            MemoryStore::empty(path.to_path_buf())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_counter() -> &'static AtomicU64 {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        &COUNTER
    }

    /// Fresh scratch path (the file itself is not created until the first
    /// write); callers clean up the parent dir.
    fn temp_store_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "igris_memory_{}_{}_{}_{}",
            tag,
            std::process::id(),
            nanos,
            temp_counter().fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("memory.json")
    }

    fn tags(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /* ---- vendored kernels ---- */

    #[test]
    fn vendored_cosine_matches_upstream_contract() {
        assert!((cosine_similarity(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]).abs() < 1e-6, "shape mismatch = 0");
        assert!(cosine_similarity(&[], &[]).abs() < 1e-6);
        assert!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]).abs() < 1e-6, "zero vector = 0");
    }

    #[test]
    fn vendored_hash_embedder_is_deterministic_and_normalized() {
        let a = hash_embed("hello world");
        let b = hash_embed("hello world");
        assert_eq!(a.len(), EMBED_DIMS);
        assert_eq!(a, b, "deterministic");
        let norm: f32 = a.iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01, "normalized, got {norm}");
        assert!(hash_embed("").iter().all(|v| v.abs() < 1e-6), "empty text = zero vector");
    }

    /* ---- store mechanics ---- */

    #[test]
    fn store_rejects_empty_content_and_bad_reminders() {
        let dir = temp_store_path("bad");
        let mut store = MemoryStore::open(&dir).unwrap();
        assert!(store.add(MemoryKind::Memory, "   ", &[], None).is_err());
        assert!(
            store
                .add(MemoryKind::Reminder, "call mom", &[], None)
                .unwrap_err()
                .contains("due time"),
            "reminder without due_at is an error"
        );
        assert!(store.is_empty());
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn store_persists_across_reopen() {
        let dir = temp_store_path("persist");
        let path = dir.clone();
        let mut store = MemoryStore::open(&path).unwrap();
        let (first, existed) = store
            .add(MemoryKind::Memory, "My exam is on March 3rd", &tags(&["exam"]), None)
            .unwrap();
        assert!(!existed);
        drop(store);

        /* A fresh instance over the same file is the unit-level
         * "survive restart" proof. */
        let reopened = MemoryStore::open(&path).unwrap();
        assert_eq!(reopened.len(), 1);
        let hits = reopened.recall("when is my exam", &[], 5);
        assert_eq!(hits.len(), 1, "tag 'exam' should match");
        assert_eq!(hits[0].record.id, first.id);
        assert_eq!(hits[0].record.content, "My exam is on March 3rd");
        assert_eq!(hits[0].via, "tag");
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn store_exact_duplicate_is_not_reinserted() {
        let path = temp_store_path("dup");
        let mut store = MemoryStore::open(&path).unwrap();
        store.add(MemoryKind::Memory, "Likes green tea", &[], None).unwrap();
        let (rec, existed) = store.add(MemoryKind::Memory, "  likes GREEN tea ", &[], None).unwrap();
        assert!(existed, "case/whitespace-insensitive duplicate detected");
        assert_eq!(store.len(), 1);
        /* Same words under a different kind are a distinct record. */
        let (_, existed) = store.add(MemoryKind::Note, "Likes green tea", &[], None).unwrap();
        assert!(!existed);
        assert_eq!(store.len(), 2);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn recall_ranks_exact_over_tag_over_similar() {
        let path = temp_store_path("rank");
        let mut store = MemoryStore::open(&path).unwrap();
        store
            .add(MemoryKind::Memory, "My exam is on March 3rd", &tags(&["exam", "college"]), None)
            .unwrap();
        store
            .add(MemoryKind::Memory, "call mom tomorrow at 9", &tags(&["family"]), None)
            .unwrap();

        /* Exact substring wins and ranks first; hash-token collisions may
         * let the unrelated record ride along, but only as a `similar`
         * hit below the keyword score (the vendored embedder is
         * deterministic per build, so this stays stable run to run). */
        let hits = store.recall("march 3rd", &[], 5);
        assert_eq!(hits[0].via, "exact", "{hits:?}");
        assert_eq!(hits[0].record.content, "My exam is on March 3rd");
        assert!(hits[0].score >= 1.0);
        assert!(hits[1..].iter().all(|h| h.via == "similar"), "{hits:?}");

        /* Tag match without substring. */
        let hits = store.recall("when is my exam", &[], 5);
        assert_eq!(hits[0].via, "tag", "{hits:?}");
        assert_eq!(hits[0].record.content, "My exam is on March 3rd");
        assert!(hits[1..].iter().all(|h| h.via == "similar"), "{hits:?}");

        /* Limit truncates. */
        let hits = store.recall("the", &[], 1);
        assert!(hits.len() <= 1);

        /* Empty query = no hits, never a panic. */
        assert!(store.recall("   ", &[], 5).is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn recall_tag_filter_restricts_and_activity_is_searchable() {
        let path = temp_store_path("tags");
        let mut store = MemoryStore::open(&path).unwrap();
        store
            .add(MemoryKind::Memory, "Wifi password is hunter2", &tags(&["wifi", "home"]), None)
            .unwrap();
        store
            .add(MemoryKind::Note, "Buy milk and eggs", &tags(&["shopping"]), None)
            .unwrap();
        store
            .add(MemoryKind::Activity, "set volume to 40", &[], None)
            .unwrap();

        /* Restrict by tag: only the wifi record can match. */
        let hits = store.recall("password", &tags(&["wifi"]), 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.content, "Wifi password is hunter2");

        /* Unrelated tag filter excludes everything. */
        assert!(store.recall("password", &tags(&["work"]), 5).is_empty());

        /* Activity records are searchable (feeds 'what did I do'). */
        let hits = store.recall("set volume", &[], 5);
        assert!(hits.iter().any(|h| h.record.kind == MemoryKind::Activity));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn flush_is_atomic_and_leaves_no_tmp_file() {
        let dir = temp_store_path("atomic");
        let path = dir.clone();
        let mut store = MemoryStore::open(&path).unwrap();
        store.add(MemoryKind::Note, "test note", &[], None).unwrap();
        let entries: Vec<String> = std::fs::read_dir(dir.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["memory.json".to_string()], "tmp file renamed away, dir: {entries:?}");
        /* File is valid JSON with the expected envelope. */
        let text = std::fs::read_to_string(&path).unwrap();
        let file: MemoryFile = serde_json::from_str(&text).unwrap();
        assert_eq!(file.version, 1);
        assert_eq!(file.records.len(), 1);
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn corrupt_file_open_is_a_strict_error() {
        let dir = temp_store_path("corrupt");
        let path = dir.clone();
        std::fs::write(&path, "not json at all {").unwrap();
        let err = MemoryStore::open(&path).unwrap_err();
        assert!(err.contains("cannot parse"), "{err}");
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn quarantine_preserves_the_corrupt_file_and_recovers() {
        let dir = temp_store_path("quarantine");
        let path = dir.clone();
        std::fs::write(&path, "broken {").unwrap();
        let store = open_or_quarantine(&path);
        assert!(store.is_empty());
        /* The original bytes survive under a .corrupt- name. */
        let quarantined: Vec<String> = std::fs::read_dir(dir.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".corrupt-"))
            .collect();
        assert_eq!(quarantined.len(), 1, "one quarantine file, got {quarantined:?}");
        /* Store can write again from a clean slate. */
        let mut store = store;
        store.add(MemoryKind::Memory, "fresh start", &[], None).unwrap();
        assert_eq!(store.len(), 1);
        std::fs::remove_dir_all(dir.parent().unwrap()).ok();
    }

    #[test]
    fn recall_kind_label_query_surfaces_the_whole_kind() {
        let path = temp_store_path("kind");
        let mut store = MemoryStore::open(&path).unwrap();
        store
            .add(MemoryKind::Reminder, "Call mom", &tags(&["family"]), Some("tomorrow 9am".to_string()))
            .unwrap();
        store
            .add(MemoryKind::Note, "Buy milk and eggs", &[], None)
            .unwrap();
        store
            .add(MemoryKind::Memory, "Wifi password is hunter2", &[], None)
            .unwrap();

        /* No content/tag word overlap with "call mom" — the kind label
         * is what makes this query work. Hash collisions may add a
         * `similar` rider, but the keyword hit always ranks first. */
        let hits = store.recall("what are my reminders", &[], 5);
        assert!(hits[0].record.content == "Call mom", "{hits:?}");
        assert_eq!(hits[0].via, "tag");
        assert!(hits[1..].iter().all(|h| h.via == "similar"), "{hits:?}");

        let hits = store.recall("show my notes", &[], 5);
        assert!(hits[0].record.content == "Buy milk and eggs", "{hits:?}");
        assert_eq!(hits[0].via, "tag");

        /* Plural of the label matches too ("activities"). */
        store
            .add(MemoryKind::Activity, "set volume to 40", &[], None)
            .unwrap();
        let hits = store.recall("what activities did I do", &[], 5);
        assert!(hits.iter().any(|h| h.record.kind == MemoryKind::Activity), "{hits:?}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    /* ---- default path ---- */

    #[test]
    fn default_path_is_under_the_igris_config_dir() {
        let path = default_path();
        assert!(path.ends_with("memory.json"), "{path:?}");
        assert!(path.to_string_lossy().contains("igris"), "{path:?}");
    }
}
