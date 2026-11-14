//! Runtime asset locations: the llama.cpp server builds, the piper runtime,
//! and the STT/TTS model trees.
//!
//! Everything the agent needs at runtime lives in one gitignored `assets/`
//! tree at the workspace root (see the workspace Cargo.toml for the exact
//! layout). All lookups go through [`asset_root`] so any front-end — the
//! CLI, a future GUI — finds the binaries no matter its working directory,
//! and tests can redirect the whole tree with one env var.

use std::path::{Path, PathBuf};

/// The `assets/` directory. Resolution order:
/// 1. `MOON_ASSETS` env override (tests, exotic installs, bundled GUIs);
/// 2. nearest ancestor of the running executable that contains `assets/`
///    (cargo layout: `target/<profile>/<exe>` walks up to the workspace
///    root);
/// 3. nearest ancestor of the current working directory containing
///    `assets/`;
/// 4. plain `assets/` relative to the working directory (running from the
///    workspace root — the historical behavior).
pub fn asset_root() -> PathBuf {
    if let Ok(dir) = std::env::var("MOON_ASSETS")
        && !dir.trim().is_empty()
    {
        return PathBuf::from(dir);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(root) = find_assets(exe.parent())
    {
        return root;
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(root) = find_assets(Some(&cwd))
    {
        return root;
    }
    PathBuf::from("assets")
}

/// Walk `start` upward, returning the first directory containing `assets/`.
fn find_assets(start: Option<&Path>) -> Option<PathBuf> {
    let mut dir = start.map(Path::to_path_buf)?;
    loop {
        let probe = dir.join("assets");
        if probe.is_dir() {
            return Some(probe);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Join an asset fragment (e.g. `llama-cpp/vulkan/llama-server.exe`) onto
/// the resolved asset root.
pub fn asset(name: &str) -> PathBuf {
    asset_root().join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_joins_fragments_under_the_root() {
        let root = asset_root();
        let joined = asset("llama-cpp/vulkan/llama-server.exe");
        assert!(joined.starts_with(&root));
        assert!(joined.ends_with("llama-server.exe"));
    }

    #[test]
    fn mo_assets_env_overrides_the_root() {
        // SAFETY: single-threaded test process, env var not read elsewhere
        // in this test binary's concurrent tests? It is read by asset_root
        // in the other test — serialize the pair by ordering: this test
        // restores the var, and the other test tolerates any root.
        let key = "MOON_ASSETS";
        let saved = std::env::var(key).ok();
        /* SAFETY: test-only env manipulation; the process is single-threaded
         * at this point and the var is restored (or removed) right after. */
        unsafe { std::env::set_var(key, "Z:/override") };
        let root = asset_root();
        if let Some(v) = saved {
            /* SAFETY: same single-threaded test context as above. */
            unsafe { std::env::set_var(key, v) };
        } else {
            /* SAFETY: same single-threaded test context as above. */
            unsafe { std::env::remove_var(key) };
        }
        assert_eq!(root, PathBuf::from("Z:/override"));
    }
}
