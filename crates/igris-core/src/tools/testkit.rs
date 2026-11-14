//! Test-only helpers shared by the tools test modules.

use super::{Registry, seed_registry};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Unique-name counter for test scratch dirs (process id already
/// separates concurrent test processes).
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn reg() -> Registry {
    seed_registry()
}

/// Fresh scratch directory under the system temp dir; callers clean
/// up with `fs::remove_dir_all` (errors ignored on purpose).
pub(crate) fn temp_root(tag: &str) -> PathBuf {
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
