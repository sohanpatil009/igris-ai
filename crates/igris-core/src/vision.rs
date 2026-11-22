//! Optional image understanding via Cactus Mirage, run as a Python sidecar.
//!
//! Mirage is a standalone `.cact` vision model (not a llama.cpp GGUF), so it
//! cannot be attached to LFM3/Qwen as an mmproj. Instead it describes an
//! image in plain text, and that description is prefixed to the normal chat
//! prompt. Text-only chats are completely unaffected.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Locate `scripts/mirage_describe.py` relative to the resolved assets root,
/// with a CWD fallback for the common workspace-root launch.
fn script_path() -> PathBuf {
    let assets = crate::paths::asset_root();
    let candidate = assets
        .parent()
        .map(|p| p.join("scripts\\mirage_describe.py"))
        .unwrap_or_default();
    if candidate.is_file() {
        candidate
    } else {
        PathBuf::from("scripts\\mirage_describe.py")
    }
}

/// Describe `image` with the locally provisioned Mirage weights.
/// Returns a short plain-text summary suitable for prepending to a prompt.
pub fn describe_image(image: &Path) -> Result<String, String> {
    if !image.is_file() {
        return Err(format!("image not found: {}", image.display()));
    }
    let script = script_path();
    if !script.is_file() {
        return Err(format!("vision helper not found: {}", script.display()));
    }
    let weights = crate::paths::asset("mirage\\mirage.cact");
    if !weights.is_file() {
        return Err(format!(
            "Mirage weights not found: {} (download from Cactus-Compute/mirage)",
            weights.display()
        ));
    }
    let python = std::env::var("MOON_PYTHON").unwrap_or_else(|_| "python".to_string());
    let output = Command::new(&python)
        .arg(&script)
        .arg(image)
        .arg(&weights)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not start {python}: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if output.status.success() && !text.is_empty() {
        Ok(text)
    } else {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(format!(
            "Mirage failed ({}): {}",
            output.status,
            if err.is_empty() { "no details" } else { &err }
        ))
    }
}

/// A located UI element: centre coordinates in screenshot pixels plus the
/// label Mirage matched, for coordinate-based browser clicks.
#[derive(Debug, Clone)]
pub struct LocatedElement {
    pub x: f64,
    pub y: f64,
    pub name: String,
    pub score: f64,
}

/// Ask Mirage which detected object/text best matches `query` in `image`.
pub fn locate_in_image(image: &Path, query: &str) -> Result<LocatedElement, String> {
    if !image.is_file() {
        return Err(format!("image not found: {}", image.display()));
    }
    let script = script_path();
    let weights = crate::paths::asset("mirage\\mirage.cact");
    if !script.is_file() || !weights.is_file() {
        return Err("vision helper or Mirage weights missing".into());
    }
    let python = std::env::var("MOON_PYTHON").unwrap_or_else(|_| "python".to_string());
    let output = Command::new(&python)
        .arg(&script)
        .arg("locate")
        .arg(image)
        .arg(&weights)
        .arg(query)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not start {python}: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() {
        return Err(format!(
            "Mirage locate failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("bad locate result: {e}"))?;
    let x = v["x"].as_f64().ok_or("no x in locate result")?;
    let y = v["y"].as_f64().ok_or("no y in locate result")?;
    Ok(LocatedElement {
        x,
        y,
        name: v["name"].as_str().unwrap_or("").to_string(),
        score: v["score"].as_f64().unwrap_or(0.0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vision_rejects_missing_image_before_spawning_python() {
        let err = describe_image(Path::new("Z:/definitely/missing.png")).unwrap_err();
        assert!(err.contains("image not found"));
    }
}
