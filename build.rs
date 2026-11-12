//! Copies the onnxruntime.dll bundled by moonshine-sys next to the built
//! executable.
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
//! The executable's own directory is the first entry of the Windows dll
//! search order, so copying the bundled dll there makes the correct
//! runtime win deterministically. Re-running on every build keeps the
//! copy in place across `cargo clean`.

use std::{env, fs, path::PathBuf, time::UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    /* OUT_DIR is <target>/<profile>/build/<crate>-<hash>/out; three
     * ancestors up is the profile dir, which is also where the exe
     * lands (and therefore its dll search directory). */
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let profile = out_dir
        .ancestors()
        .nth(3)
        .expect("OUT_DIR has the standard build/<crate>-<hash>/out shape")
        .to_path_buf();

    /* The bundled dll lives in moonshine-sys's own build out dir (a
     * sibling of this crate's build dir). Find the newest copy across
     * any moonshine-sys-<hash> dirs that may be lying around. */
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    if let Ok(hash_dirs) = fs::read_dir(profile.join("build")) {
        for dir in hash_dirs.flatten() {
            if !dir
                .file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("moonshine-sys-"))
            {
                continue;
            }
            let mut stack = vec![dir.path().join("out")];
            while let Some(sub) = stack.pop() {
                let Ok(entries) = fs::read_dir(&sub) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.file_name().is_some_and(|n| n == "onnxruntime.dll") {
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
        }
    }

    let Some((src_mtime, bundled)) = newest else {
        println!(
            "cargo:warning=no bundled onnxruntime.dll under {}/build/moonshine-sys-*; \
             the exe will pick up the OS copy from System32 and crash at model load",
            profile.display()
        );
        return;
    };

    /* Copy only when the exe-side copy is missing or older than the
     * bundled one (e.g. after a moonshine-sys version bump). */
    let dest = profile.join("onnxruntime.dll");
    /* Watch the exe-side copy itself: deleting it (or replacing it with
     * an older runtime) makes cargo re-run this script on the next
     * build, which restores it. A copy updates its mtime once, so the
     * next build sees it unchanged and stays fresh. */
    println!("cargo:rerun-if-changed={}", dest.display());
    let needs_copy = fs::metadata(&dest)
        .and_then(|m| m.modified())
        .map_or(true, |t| src_mtime > t);
    if !needs_copy {
        return;
    }
    if let Err(e) = fs::copy(&bundled, &dest) {
        /* A running app locks the dll; if a previous copy is still in
         * place that is fine, otherwise the build must fail loudly. */
        if !dest.exists() {
            panic!("failed to copy bundled onnxruntime.dll next to the exe: {e}");
        }
    }
}
