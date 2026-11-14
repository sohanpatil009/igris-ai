//! run_command (v2, goal.md section 4): curated program+subcommand
//! allowlist (cargo dev subcommands + read-only git), no shell, argv
//! direct. Bare `cargo run` becomes build-then-launch so the app stays
//! alive.

use crate::tools::shared::{expand_home_path, required_str, truncate_chars};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/* ---------------------------------------------------------------- */
/* run_command — curated program+subcommand allowlist                */
/* ---------------------------------------------------------------- */

/// Output ceiling fed to the model (compiler dumps can be enormous).
const RUN_COMMAND_MAX_OUTPUT_CHARS: usize = 4_000;
/// Internal kill deadline. Deliberately BELOW the dispatch budget
/// (`tool_budget("run_command")` = 120 s) so the tool always answers
/// before the dispatcher gives up.
const RUN_COMMAND_INTERNAL_BUDGET: Duration = Duration::from_secs(105);

/// goal.md safety rules: never free-form. Program → permitted first
/// argument (the subcommand). `cargo run` is listed because it gets the
/// special two-phase build-then-launch path below.
const COMMAND_ALLOWLIST: &[(&str, &[&str])] = &[
    (
        "cargo",
        &[
            "check", "build", "test", "run", "new", "init", "add", "remove", "fmt",
            "clippy", "tree",
        ],
    ),
    ("git", &["status", "log", "diff", "show", "rev-parse"]),
];

fn allowed_subcommands(program: &str) -> Option<&'static [&'static str]> {
    COMMAND_ALLOWLIST
        .iter()
        .find(|(p, _)| *p == program)
        .map(|(_, subs)| *subs)
}

fn validate_command(program: &str, args: &[String]) -> Result<(), String> {
    let Some(subs) = allowed_subcommands(program) else {
        return Err(format!(
            "Program '{program}' is not allowed. Allowed: {}",
            COMMAND_ALLOWLIST
                .iter()
                .map(|(p, _)| *p)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    };
    if args.is_empty() {
        return Err(format!(
            "Missing subcommand for '{program}'. Allowed: {}",
            subs.join(", ")
        ));
    }
    let sub = args[0].as_str();
    if !subs.contains(&sub) {
        return Err(format!(
            "'{program} {sub}' is not allowed. Allowed: {}",
            subs.join(", ")
        ));
    }
    for a in args {
        if a.is_empty() {
            return Err("empty command argument".into());
        }
        /* No NUL (would truncate the argv) and no control bytes (no
         * newline smuggling into a console). Values are otherwise
         * verbatim argv entries — no shell, so no metacharacter risk. */
        if a.bytes().any(|b| b == 0 || b < 0x20) {
            return Err(format!("invalid control character in command argument '{a}'"));
        }
    }
    Ok(())
}

/// Spawn + capture + bounded wait. Returns (exit code, combined output).
/// `CREATE_NO_WINDOW` keeps console flashes off the desktop when the
/// assistant itself runs headless.
pub(crate) fn run_command_output(
    prog: &str,
    args: &[String],
    wd: Option<&Path>,
) -> Result<(i32, String), String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(prog);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(dir) = wd {
        cmd.current_dir(dir);
    }
    cmd.creation_flags(CREATE_NO_WINDOW);
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Could not start {prog}: {e}"))?;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let t_out = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stdout_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let t_err = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(p) = stderr_pipe.as_mut() {
            let _ = p.read_to_end(&mut buf);
        }
        buf
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) => {
                if started.elapsed() >= RUN_COMMAND_INTERNAL_BUDGET {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("Could not wait for {prog}: {e}")),
        }
    };
    let mut out_bytes = t_out.join().unwrap_or_default();
    let err_bytes = t_err.join().unwrap_or_default();
    if !out_bytes.is_empty() && !err_bytes.is_empty() && !out_bytes.ends_with(b"\n") {
        out_bytes.push(b'\n');
    }
    out_bytes.extend_from_slice(&err_bytes);
    let exit_code = status.map(|s| s.code().unwrap_or(-1)).unwrap_or(-1);
    Ok((exit_code, String::from_utf8_lossy(&out_bytes).into_owned()))
}


/// First `name = "..."` inside the `[package]` section of a Cargo.toml
/// (line scan: comments and later sections ignored).
fn manifest_package_name(text: &str) -> Option<String> {
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line.eq_ignore_ascii_case("[package]");
            continue;
        }
        if in_package && !line.starts_with('#') && line.starts_with("name") {
            if let Some(v) = line.split('=').nth(1) {
                let v = v.trim().trim_matches('"').trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// Two-phase bare `cargo run`: build (captured, bounded), then launch the
/// built executable detached so the app stays alive after the reply —
/// a bounded capture cannot do that (a never-exiting app would hang the
/// turn or get killed). Workspace manifests without a `[package]` section
/// fall back to a plain bounded `cargo run`.
fn cargo_build_then_launch(wd: Option<&Path>) -> Result<String, String> {
    let dir = wd
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let manifest = dir.join("Cargo.toml");
    let manifest_text = fs::read_to_string(&manifest)
        .map_err(|e| format!("Cannot read {}: {e}", manifest.display()))?;
    let Some(pkg) = manifest_package_name(&manifest_text) else {
        let (code, text) = run_command_output("cargo", &["run".to_string()], Some(&dir))?;
        return Ok(format!(
            "'cargo run' in {} finished with exit code {code}:\n{}",
            dir.display(),
            truncate_chars(&text, RUN_COMMAND_MAX_OUTPUT_CHARS)
        ));
    };
    let (code, text) = run_command_output("cargo", &["build".to_string()], Some(&dir))?;
    if code != 0 {
        return Ok(format!(
            "Build failed (exit code {code}):\n{}",
            truncate_chars(&text, RUN_COMMAND_MAX_OUTPUT_CHARS)
        ));
    }
    /* cargo maps hyphens to underscores in executable names. */
    let exe = dir
        .join("target")
        .join("debug")
        .join(format!("{}.exe", pkg.replace('-', "_")));
    if !exe.is_file() {
        return Err(format!(
            "Build reported success but {} was not found",
            exe.display()
        ));
    }
    let launch = std::process::Command::new(&exe)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not launch {}: {e}", exe.display()))?;
    let pid = launch.id();
    drop(launch);
    Ok(format!(
        "Build succeeded; launched {} (pid {pid}). It keeps running in the background.",
        exe.display()
    ))
}

pub(crate) fn run_run_command(args: &Value) -> Result<String, String> {
    let program = required_str(args, "program", "run_command")?
        .trim()
        .to_lowercase();
    let cmd_args: Vec<String> = match args.get("args") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => return Err("Missing required parameter: args (array of strings)".into()),
    };
    validate_command(&program, &cmd_args)?;
    let wd: Option<PathBuf> = match args.get("working_dir").and_then(|v| v.as_str()) {
        Some(d) if !d.trim().is_empty() => {
            let p = PathBuf::from(expand_home_path(d));
            if !p.is_dir() {
                return Err(format!("working_dir does not exist: {}", p.display()));
            }
            Some(p)
        }
        _ => None,
    };

    /* Bare `cargo run` gets the build-then-launch treatment. `cargo run
     * -- <appargs>` keeps bounded-capture semantics on purpose. */
    if program == "cargo" && cmd_args.len() == 1 && cmd_args[0] == "run" {
        return cargo_build_then_launch(wd.as_deref());
    }

    let (code, text) = run_command_output(&program, &cmd_args, wd.as_deref())?;
    let wd_note = wd
        .as_ref()
        .map(|p| format!(" in {}", p.display()))
        .unwrap_or_default();
    let out = if text.trim().is_empty() {
        "(no output)".to_string()
    } else {
        truncate_chars(text.trim_end(), RUN_COMMAND_MAX_OUTPUT_CHARS)
    };
    Ok(format!(
        "'{} {}'{wd_note} finished with exit code {code}:\n{out}",
        program,
        cmd_args.join(" ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::reg;
    use serde_json::json;

    #[test]
    fn run_command_allowlist_rejects_free_form() {
        let err = reg()
            .dispatch(
                "run_command",
                &json!({"program": "python", "args": ["-c", "print(1)"]}),
            )
            .unwrap_err();
        assert!(err.contains("is not allowed"), "{err}");
        let err = reg()
            .dispatch("run_command", &json!({"program": "cargo", "args": ["clean"]}))
            .unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
        let err = reg()
            .dispatch("run_command", &json!({"program": "git", "args": ["push", "origin"]}))
            .unwrap_err();
        assert!(err.contains("not allowed"), "{err}");
        let err = reg()
            .dispatch("run_command", &json!({"program": "git", "args": []}))
            .unwrap_err();
        assert!(err.contains("Missing subcommand"), "{err}");
        let err = reg()
            .dispatch(
                "run_command",
                &json!({"program": "git", "args": ["status", "--short\n"]}),
            )
            .unwrap_err();
        assert!(err.contains("control character"), "{err}");
    }

    #[test]
    fn run_command_real_git_status_reports_exit_code() {
        /* Read-only, fast, and proves the whole spawn-capture-wait path. */
        let out = reg()
            .dispatch(
                "run_command",
                &json!({
                    "program": "git",
                    "args": ["status"],
                    "working_dir": env!("CARGO_MANIFEST_DIR")
                }),
            )
            .unwrap();
        assert!(out.contains("finished with exit code 0"), "{out}");
        assert!(out.contains("'git status'"), "{out}");
    }

    #[test]
    fn run_command_output_reports_nonzero_exit_without_failing() {
        let (code, text) = run_command_output(
            "git",
            &["log".to_string(), "--definitely-not-a-flag".to_string()],
            Some(Path::new(env!("CARGO_MANIFEST_DIR"))),
        )
        .unwrap();
        assert_ne!(code, 0);
        assert!(!text.is_empty(), "usage errors print something");
    }

    #[test]
    fn run_command_working_dir_must_exist() {
        let err = reg()
            .dispatch(
                "run_command",
                &json!({"program": "git", "args": ["status"], "working_dir": "Z:\\no\\dir"}),
            )
            .unwrap_err();
        assert!(err.contains("working_dir does not exist"), "{err}");
    }

    #[test]
    fn manifest_package_name_scans_only_the_package_section() {
        let text = "[package]\n# name = \"commented\"\nname = \"my-app\"\nedition = \"2024\"\n\
                    \n[dependencies]\nname = \"should-not-win\"\n";
        assert_eq!(manifest_package_name(text), Some("my-app".to_string()));
        assert_eq!(manifest_package_name("[workspace]\nmembers = []\n"), None);
    }

    /* ---- shared v2 helpers ---- */

}
