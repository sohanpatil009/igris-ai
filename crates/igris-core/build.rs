//! Copies the onnxruntime.dll bundled by moonshine-sys next to the built
//! executables.
//!
//! moonshine-sys ships a prebuilt moonshine voice library that needs
//! onnxruntime API version 23 (runtime 1.23.x); it unpacks the matching
//! dll into its build-script output dir, but nothing copies it next to
//! the executable. The loader then falls back to the OS-built-in copy in
//! System32 (onnxruntime 1.17, which supports only API versions 1 and
//! 17): `OrtGetApiBase().GetApi(23)` returns null and the process dies
//! with STATUS_ACCESS_VIOLATION right after printing
//! "The requested API version [23] is not available ...".
//!
//! Windows loads a dll from the running executable's directory first, so
//! the bundled dll is copied to every directory that hosts one of our
//! binaries:
//! - the profile root (`target/<profile>` — where the main exe lands), and
//! - this build script's own OUT_DIR, which (cargo >= 1.100 layout) is
//!   also where the test binaries are placed
//!   (`target/<profile>/build/<pkg>/<hash>/out/<pkg>-<hash>.exe`).
//! The profile root is derived layout-robustly: the OUT_DIR ancestor whose
//! parent directory is named `target`.
//!
//! Re-running on every build keeps the copies in place across `cargo clean`.

use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));

    /* The profile dir is the OUT_DIR ancestor whose parent is the target
     * dir — stable across cargo's old (`build/<pkg>-<hash>`) and new
     * (`build/<pkg>/<hash>`) build-dir layouts. */
    let profile = out_dir
        .ancestors()
        .find(|p| p.parent().is_some_and(|pp| pp.file_name().is_some_and(|n| n == "target")))
        .map(Path::to_path_buf)
        .expect("OUT_DIR lives under target/<profile>/build/...");

    /* The bundled dll lives in moonshine-sys's own build out dir. The
     * directory shape around it varies by cargo version, so walk the whole
     * build dir recursively and accept any hit below a moonshine-sys path
     * component. */
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    let mut stack = vec![profile.join("build")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n == "onnxruntime.dll")
                && path
                    .components()
                    .any(|c| c.as_os_str().to_string_lossy().starts_with("moonshine-sys"))
            {
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(UNIX_EPOCH);
                if newest.as_ref().map_or(true, |(t, _)| mtime > *t) {
                    newest = Some((mtime, path));
                }
            }
        }
    }

    let Some((src_mtime, bundled)) = newest else {
        println!(
            "cargo:warning=no bundled onnxruntime.dll under {}/build/**/moonshine-sys*/; \
             the exe will pick up the OS copy from System32 and crash at model load",
            profile.display()
        );
        return;
    };

    /* Copy next to the main exe, and next to every binary this package
     * builds: cargo >= 1.100 places test binaries in their own
     * `build/<pkg>/<hash>/out` dir (build scripts keep their own out dir
     * there too). Copy into each `out` dir belonging to this package that
     * already exists; an out dir created later by a fresh unit gets the
     * dll on the next build-script rerun (build.rs itself is a
     * rerun-if-changed input). */
    let mut dests = vec![profile.join("onnxruntime.dll")];
    let mut stack = vec![profile.join("build")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "out")
                    && path
                        .components()
                        .any(|c| c.as_os_str().to_string_lossy().starts_with("igris-core"))
                {
                    dests.push(path.join("onnxruntime.dll"));
                }
                stack.push(path);
            }
        }
    }
    for dest in dests {
        println!("cargo:rerun-if-changed={}", dest.display());
        copy_if_stale(&bundled, src_mtime, &dest);
    }
}

/// Copy the bundled dll over `dest` only when it is missing or older than
/// the bundled one (e.g. after a moonshine-sys version bump). A copy
/// updates the dest's mtime once, so later builds stay fresh (no loop).
///
/// A locked file is tolerated when a previous copy is still in place (a
/// running app holds the dll); otherwise the build fails loudly.
fn copy_if_stale(src: &Path, src_mtime: SystemTime, dest: &Path) {
    let needs_copy = fs::metadata(dest)
        .and_then(|m| m.modified())
        .map_or(true, |t| src_mtime > t);
    if !needs_copy {
        return;
    }
    if let Err(e) = fs::copy(src, dest) {
        if !dest.exists() {
            panic!(
                "failed to copy bundled onnxruntime.dll to {}: {e}",
                dest.display()
            );
        }
    }
}
