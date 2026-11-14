//! STT model management: validate and fetch the Moonshine model files.
//!
//! Extracted from the old single-crate main.rs. `ensure_model` is shared by
//! every STT front-end (live, hotkey, assist) and by the CLI benchmark
//! mode; it returns the resolved model directory so callers can hand the
//! same path to `Transcriber::from_files`.

use moonshine_rs::{ModelArch, SttDependenciesOptions, get_stt_dependencies_with_options};
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::paths;

fn manifest_name(file: &Value) -> Result<&str, Box<dyn std::error::Error>> {
    let name = file["name"].as_str().ok_or("file missing name")?;
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(format!("unsafe model manifest filename: {name}").into());
    }
    Ok(name)
}

/// Fetch/refresh a Moonshine model directory under the `assets/stt` tree
/// and return the resolved directory. Accepts a bare model name
/// (`small-streaming`, resolved against `assets/stt`), an absolute path,
/// or a CWD-relative path (explicit `--model` flags keep that behavior).
pub fn ensure_model(dir: &str, arch: ModelArch) -> Result<PathBuf, Box<dyn std::error::Error>> {
    /* Only operate on directories we know belong to this tool; reject paths
     * that could reach outside a dedicated model tree. */
    let normalized = dir.trim();
    if normalized.is_empty() {
        return Err("model directory is empty".into());
    }
    let root = paths::asset("stt");
    let raw = PathBuf::from(normalized);
    let out = if raw.is_absolute() {
        raw
    } else if normalized.contains('/') || normalized.contains('\\') {
        /* a relative path with separators: CWD-relative, the historical
         * behavior for explicit --model flags */
        raw
    } else {
        /* a bare model name: resolve against the asset tree */
        root.join(normalized)
    };
    fs::create_dir_all(&root)?;
    /* Require exactly one normal child below the dedicated model root. This
     * rejects absolute paths, `..`, nested symlink paths, and `--model .`. */
    let rel = out.strip_prefix(&root).map_err(|_| {
        format!(
            "model directory must be under {} (got {})",
            root.display(),
            out.display()
        )
    })?;
    if rel.components().count() != 1
        || !matches!(rel.components().next(), Some(Component::Normal(_)))
    {
        return Err("model directory must be one direct child of assets/stt".into());
    }
    if let Ok(metadata) = fs::symlink_metadata(&out) {
        if !metadata.is_dir() {
            return Err("model path exists but is not a directory".into());
        }
        let existing = out.canonicalize()?;
        let root_canonical = root.canonicalize()?;
        if !existing.starts_with(&root_canonical) {
            return Err("model directory resolves outside assets/stt".into());
        }
    }
    fs::create_dir_all(&out)?;
    let root_canonical = root.canonicalize()?;
    let out_canonical = out.canonicalize()?;
    if !out_canonical.starts_with(&root_canonical) {
        return Err("model directory resolves outside assets/stt".into());
    }

    let manifest_json =
        get_stt_dependencies_with_options("en", &SttDependenciesOptions::new().with_arch(arch))?;
    let manifest: Value = serde_json::from_str(&manifest_json)?;
    let groups = manifest["groups"]
        .as_array()
        .ok_or("manifest missing groups")?;

    /* If any on-disk file is missing or has the wrong size (e.g. files from
     * an older model revision), rebuild the manifest-listed files so the
     * required file set always matches what this SDK build expects. */
    let mut stale = false;
    for group in groups {
        let files = group["files"].as_array().ok_or("group missing files")?;
        for file in files {
            let name = manifest_name(file)?;
            let expected = file["size"].as_u64().unwrap_or(0);
            let ok = if let Ok(meta) = fs::metadata(out.join(name)) {
                expected == 0 || meta.len() == expected
            } else {
                false
            };
            if !ok {
                stale = true;
            }
        }
    }
    if stale {
        println!(
            "  revision mismatch detected; rebuilding {} from scratch",
            out.display()
        );
        /* Delete only files named in the manifest, not the whole directory.
         * This avoids wiping unrelated files if the manifest or path is
         * misconfigured. */
        let mut to_remove = std::collections::HashSet::new();
        for group in groups {
            for file in group["files"].as_array().ok_or("group missing files")? {
                let name = manifest_name(file)?;
                to_remove.insert(name.to_string());
            }
        }
        let mut cleaned = 0usize;
        for entry in fs::read_dir(&out)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_file()
                && let Some(name) = p.file_name().and_then(|n| n.to_str())
                && to_remove.contains(name)
            {
                fs::remove_file(&p)?;
                cleaned += 1;
            }
        }
        println!("  removed {} stale files", cleaned);
    }

    for group in groups {
        let files = group["files"].as_array().ok_or("group missing files")?;
        for file in files {
            let name = manifest_name(file)?;
            let url = file["url"].as_str().ok_or("file missing url")?;
            let expected = file["size"].as_u64().unwrap_or(0);
            let dest = out.join(name);
            if dest.exists() {
                let have = fs::metadata(&dest)?.len();
                if expected == 0 || have == expected {
                    continue;
                }
            }
            println!(
                "    fetch {} ({:.1} MB) <- {}",
                name,
                expected as f64 / 1e6,
                url
            );
            let resp = ureq::get(url).call()?;
            let mut bytes = Vec::new();
            resp.into_body().into_reader().read_to_end(&mut bytes)?;
            fs::write(&dest, &bytes)?;
            println!("          wrote {} bytes", bytes.len());
        }
    }
    Ok(out)
}
