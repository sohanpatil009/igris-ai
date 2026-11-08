// src/core/paths.rs
// Cross-platform asset resolution for the offline model pack.
//
// The old code hardcoded `./pkg`, which only worked when the process was
// launched from the repository root. Tauri, packaged desktop builds, and Unix
// installs all have different working directories, so every model lookup now
// goes through this module.

use std::path::{Path, PathBuf};

/// Environment override used by installers and tests.
const PKG_DIR_ENV: &str = "IGRIS_PKG_DIR";

/// A directory is a package root when it contains at least one of the
/// well-known subdirectories. This prevents an arbitrary executable directory
/// (for example `target/debug/examples`) from being mistaken for `pkg`.
fn looks_like_pkg(path: &Path) -> bool {
    path.join("models").is_dir() || path.join("piper").is_dir()
}

/// Candidate roots, in priority order.
fn candidate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();

    if let Ok(dir) = std::env::var(PKG_DIR_ENV) {
        if !dir.trim().is_empty() {
            roots.push(PathBuf::from(dir));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            // Bundled layout: <install>/pkg/...
            roots.push(exe_dir.join("pkg"));
            // Flat bundle layout: <install>/models/..., <install>/piper/...
            if looks_like_pkg(exe_dir) {
                roots.push(exe_dir.to_path_buf());
            }
        }
    }

    // Repository / cargo-run layout.
    roots.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("pkg"));

    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.join("pkg"));
        if looks_like_pkg(&cwd) {
            roots.push(cwd);
        }
    }

    // Per-user writable location used by the setup manager on installed builds.
    if let Some(data) = dirs::data_dir() {
        roots.push(data.join("igris-ai").join("pkg"));
    }

    roots
}

/// Resolve the active package directory.
///
/// An explicit `IGRIS_PKG_DIR` always wins. Otherwise the first existing root
/// that looks like a package wins. If none exists yet, the per-user data
/// directory is created and returned so installers have a writable destination.
pub fn pkg_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(PKG_DIR_ENV) {
        if !dir.trim().is_empty() {
            let path = PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&path);
            return path;
        }
    }

    for root in candidate_roots() {
        if root.exists() && looks_like_pkg(&root) {
            return root;
        }
    }

    // Installed builds have no repository checkout, so fall back to the
    // per-user data directory (and create it) instead of a build-machine path.
    if let Some(data) = dirs::data_dir() {
        let dir = data.join("igris-ai").join("pkg");
        let _ = std::fs::create_dir_all(&dir);
        return dir;
    }

    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("pkg")
}

/// Resolve a path relative to the package directory.
pub fn pkg_path(relative: &str) -> PathBuf {
    pkg_dir().join(relative)
}

/// Find an existing file beneath any candidate root.
pub fn find_file(relative: &str) -> Option<PathBuf> {
    for root in candidate_roots() {
        let candidate = root.join(relative);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Find an existing directory beneath any candidate root.
pub fn find_dir(relative: &str) -> Option<PathBuf> {
    for root in candidate_roots() {
        let candidate = root.join(relative);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

/// Locate a model directory, accepting both `models/<name>` and a flat
/// `<name>` directory (sherpa archives extract into a versioned folder).
pub fn model_dir(name: &str) -> Option<PathBuf> {
    if let Some(dir) = find_dir(&format!("models/{name}")) {
        return Some(dir);
    }
    find_dir(name)
}

/// Locate a model file inside a model directory.
pub fn model_file(model: &str, names: &[&str]) -> Option<PathBuf> {
    let dir = model_dir(model)?;
    for name in names {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Ensure the pkg directory layout exists.
pub fn ensure_layout(root: &Path) -> std::io::Result<()> {
    for relative in [
        "audio",
        "downloads",
        "models",
        "models/parakeet",
        "models/moonshine",
        "models/kokoro",
        "models/sense-voice",
        "models/bold_voice",
    ] {
        std::fs::create_dir_all(root.join(relative))?;
    }
    Ok(())
}
