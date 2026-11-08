//! Developer-workflow tools (plan.md 3.7).
//!
//! Read-only inspection over a sandboxed working directory (`git_status`,
//! `git_diff`, `git_log`, `code_search`) plus approval-gated mutation
//! (`git_commit`, `run_tests`). Every working directory passed in by the
//! model is canonicalized against the `PathPolicy` before any subprocess
//! spawns, so the claim "all commands run with an explicit working
//! directory; reject paths outside the sandbox" holds.

use futures::future::{BoxFuture, FutureExt};
use serde_json::Value;

use crate::tools::{
    Capability, PathPolicy, Tool, ToolError, ToolErrorKind, ToolOutcome, ToolResult,
};

const MAX_OUTPUT: usize = 24_000;

fn truncate(s: &str) -> String {
    if s.len() <= MAX_OUTPUT {
        s.to_string()
    } else {
        let mut idx = MAX_OUTPUT;
        while idx > 0 && !s.is_char_boundary(idx) {
            idx -= 1;
        }
        format!("{} …[truncated {} bytes]", &s[..idx], s.len() - idx)
    }
}

fn resolve_dir(policy: &PathPolicy, raw: &str) -> Result<std::path::PathBuf, ToolError> {
    let raw = if raw.trim().is_empty() { "." } else { raw };
    let p = policy.validate(raw)?;
    if !p.is_dir() {
        return Err(ToolError::not_found(format!(
            "working directory not found: {}",
            p.display()
        )));
    }
    Ok(p)
}

async fn run_command(
    program: &str,
    args: &[String],
    cwd: &std::path::Path,
    timeout_secs: u64,
) -> Result<(i32, String, String), ToolError> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .current_dir(cwd)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let fut = cmd.output();
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), fut).await {
        Ok(Ok(out)) => Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )),
        Ok(Err(e)) => Err(ToolError::failed(format!("failed to start {program}: {e}"))),
        Err(_) => Err(ToolError::new(
            ToolErrorKind::Timeout,
            format!("{program} timed out after {timeout_secs}s"),
        )),
    }
}

// ---------------------------------------------------------------------------
// git_status (ReadOnly)
// ---------------------------------------------------------------------------

pub struct GitStatusTool {
    policy: PathPolicy,
}

impl GitStatusTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for GitStatusTool {
    fn name(&self) -> &str {
        "git_status"
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Show git status (branch + changed files) for a repository directory."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let policy = self.policy.clone();
        let path_raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        async move {
            let cwd = resolve_dir(&policy, &path_raw)?;
            let args = vec![
                "-C".to_string(),
                cwd.display().to_string(),
                "status".to_string(),
                "--short".to_string(),
                "--branch".to_string(),
            ];
            let (code, stdout, stderr) = run_command("git", &args, &cwd, 60).await?;
            Ok(ToolOutcome::plain(format!(
                "git status (exit {code}):\n{}{}",
                truncate(stdout.trim()),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n[stderr] {}", truncate(stderr.trim()))
                }
            )))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// git_diff (ReadOnly)
// ---------------------------------------------------------------------------

pub struct GitDiffTool {
    policy: PathPolicy,
}

impl GitDiffTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for GitDiffTool {
    fn name(&self) -> &str {
        "git_diff"
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Show the working-tree diff for a repository (stat summary by default; full diff on request)."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "staged": { "type": "boolean" },
                "full": { "type": "boolean" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let policy = self.policy.clone();
        let path_raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let staged = args
            .get("staged")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let full = args.get("full").and_then(|v| v.as_bool()).unwrap_or(false);
        async move {
            let cwd = resolve_dir(&policy, &path_raw)?;
            let mut cmd_args = vec![
                "-C".to_string(),
                cwd.display().to_string(),
                "diff".to_string(),
            ];
            if staged {
                cmd_args.push("--cached".to_string());
            }
            if !full {
                cmd_args.push("--stat".to_string());
            }
            let (code, stdout, stderr) = run_command("git", &cmd_args, &cwd, 60).await?;
            Ok(ToolOutcome::plain(format!(
                "git diff (exit {code}):\n{}{}",
                truncate(if stdout.trim().is_empty() {
                    "(no diff)"
                } else {
                    stdout.trim()
                }),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n[stderr] {}", truncate(stderr.trim()))
                }
            )))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// git_log (ReadOnly)
// ---------------------------------------------------------------------------

pub struct GitLogTool {
    policy: PathPolicy,
}

impl GitLogTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for GitLogTool {
    fn name(&self) -> &str {
        "git_log"
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Show recent commit log entries for a repository."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "count": { "type": "number" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let policy = self.policy.clone();
        let path_raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let count = args
            .get("count")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .clamp(1, 100) as usize;
        async move {
            let cwd = resolve_dir(&policy, &path_raw)?;
            let args = vec![
                "-C".to_string(),
                cwd.display().to_string(),
                "log".to_string(),
                format!("-{}", count),
                "--oneline".to_string(),
                "--decorate".to_string(),
            ];
            let (code, stdout, stderr) = run_command("git", &args, &cwd, 60).await?;
            Ok(ToolOutcome::plain(format!(
                "git log (exit {code}):\n{}{}",
                truncate(if stdout.trim().is_empty() {
                    "(no commits)"
                } else {
                    stdout.trim()
                }),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n[stderr] {}", truncate(stderr.trim()))
                }
            )))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// git_commit (Sensitive)
// ---------------------------------------------------------------------------

pub struct GitCommitTool {
    policy: PathPolicy,
}

impl GitCommitTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for GitCommitTool {
    fn name(&self) -> &str {
        "git_commit"
    }
    fn capability(&self) -> Capability {
        Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        "Stage files (or everything) and create a git commit with the given message. Always requires approval."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["message"],
            "properties": {
                "path": { "type": "string" },
                "message": { "type": "string" },
                "files": { "type": "array", "items": { "type": "string" } }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let policy = self.policy.clone();
        let path_raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let files: Vec<String> = args
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        async move {
            if message.is_empty() {
                return Err(ToolError::invalid_args("missing arg: message"));
            }
            if message.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
                return Err(ToolError::invalid_args(
                    "commit message contains control chars",
                ));
            }
            let cwd = resolve_dir(&policy, &path_raw)?;
            // Stage.
            let mut add_args = vec![
                "-C".to_string(),
                cwd.display().to_string(),
                "add".to_string(),
            ];
            if files.is_empty() {
                add_args.push("-A".to_string());
            } else {
                // Prevent option injection into `git add`.
                add_args.push("--".to_string());
                for f in &files {
                    if f.trim().is_empty() {
                        continue;
                    }
                    if f.trim_start().starts_with('-') {
                        return Err(ToolError::invalid_args(
                            "staged file name cannot start with '-'",
                        ));
                    }
                    // Keep each staged path inside the sandbox. Treat relative-to-cwd.
                    let full = cwd.join(f);
                    // Re-validate through policy by checking canonical existence.
                    let canon = std::fs::canonicalize(&full)
                        .map_err(|_| ToolError::invalid_args("staged file outside sandbox"))?;
                    if !policy
                        .roots()
                        .iter()
                        .any(|r| starts_with_path_pub(&canon, r))
                    {
                        return Err(ToolError::invalid_args("staged file outside sandbox"));
                    }
                    add_args.push(f.clone());
                }
                if add_args.len() == 3 {
                    // only -C path add, nothing to stage
                    return Err(ToolError::invalid_args("no files to stage"));
                }
            }
            let (acode, aout, aerr) = run_command("git", &add_args, &cwd, 120).await?;
            if acode != 0 {
                return Err(ToolError::failed(format!(
                    "git add failed (exit {acode}): {}",
                    truncate(aerr.trim())
                )));
            }
            let commit_args = vec![
                "-C".to_string(),
                cwd.display().to_string(),
                "commit".to_string(),
                "-m".to_string(),
                message.clone(),
            ];
            let (code, stdout, stderr) = run_command("git", &commit_args, &cwd, 120).await?;
            Ok(ToolOutcome::plain(format!(
                "git commit (exit {code}):\n{}{}{}\nadd: {}",
                truncate(if stdout.trim().is_empty() {
                    "(no output)"
                } else {
                    stdout.trim()
                }),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n[stderr] {}", truncate(stderr.trim()))
                },
                "",
                truncate(aout.trim())
            )))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// run_tests (Sensitive)
// ---------------------------------------------------------------------------

pub struct RunTestsTool {
    policy: PathPolicy,
}

impl RunTestsTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for RunTestsTool {
    fn name(&self) -> &str {
        "run_tests"
    }
    fn capability(&self) -> Capability {
        Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        "Detect the project type and run its test suite (cargo/npm/pytest). Always requires approval."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let policy = self.policy.clone();
        let path_raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        async move {
            let cwd = resolve_dir(&policy, &path_raw)?;
            let (program, cmd_args): (&str, Vec<String>) = if cwd.join("Cargo.toml").is_file() {
                ("cargo", vec!["test".to_string()])
            } else if cwd.join("package.json").is_file() {
                ("npm", vec!["test".to_string()])
            } else if cwd.join("pyproject.toml").is_file()
                || cwd.join("pytest.ini").is_file()
                || cwd.join("setup.cfg").is_file()
            {
                (
                    "python",
                    vec!["-m".to_string(), "pytest".to_string(), "-q".to_string()],
                )
            } else {
                return Err(ToolError::failed(
                    "could not detect project type (no Cargo.toml/package.json/pyproject.toml)",
                ));
            };
            let (code, stdout, stderr) = run_command(program, &cmd_args, &cwd, 600).await?;
            let combined = format!("{}\n{}", stdout, stderr);
            let passed = combined
                .lines()
                .filter(|l| l.contains("passed") || l.contains("ok."))
                .count();
            let failed = combined
                .lines()
                .filter(|l| {
                    l.to_lowercase().contains("failed")
                        || l.contains("FAILED")
                        || l.contains("error[")
                })
                .count();
            Ok(ToolOutcome::plain(format!(
                "run_tests {program} (exit {code}, passed~{passed}, failed~{failed}):\n{}{}",
                truncate(stdout.trim()),
                if stderr.trim().is_empty() {
                    String::new()
                } else {
                    format!("\n[stderr] {}", truncate(stderr.trim()))
                }
            )))
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// code_search (ReadOnly) — ripgrep wrapper with a result cap
// ---------------------------------------------------------------------------

pub struct CodeSearchTool {
    policy: PathPolicy,
}

impl CodeSearchTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for CodeSearchTool {
    fn name(&self) -> &str {
        "code_search"
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Search file contents under a sandboxed root. Returns matching lines with paths."
    }
    fn schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": {
                "path": { "type": "string" },
                "query": { "type": "string" },
                "max_results": { "type": "number" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a Value) -> BoxFuture<'a, ToolResult> {
        let policy = self.policy.clone();
        let path_raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or(".")
            .to_string();
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        async move {
            if query.is_empty() {
                return Err(ToolError::invalid_args("missing arg: query"));
            }
            if query.chars().any(|c| (c as u32) < 0x20) {
                return Err(ToolError::invalid_args("query contains control characters"));
            }
            let re = regex::Regex::new(&query)
                .map_err(|e| ToolError::invalid_args(format!("invalid regex: {e}")))?;
            let root = resolve_dir(&policy, &path_raw)?;
            let max_results_c = max_results;
            let result = tokio::task::spawn_blocking(move || {
                let skip_dirs = ["target", "node_modules", ".git", "dist", "build"];
                let mut hits: Vec<String> = Vec::new();
                let mut files_scanned = 0usize;
                let mut stack = vec![root.clone()];
                while let Some(dir) = stack.pop() {
                    if hits.len() >= max_results_c {
                        break;
                    }
                    if files_scanned > 5_000 {
                        break;
                    }
                    let entries = match std::fs::read_dir(&dir) {
                        Ok(e) => e,
                        Err(_) => continue,
                    };
                    for entry in entries.flatten() {
                        if hits.len() >= max_results_c {
                            break;
                        }
                        let path = entry.path();
                        let ft = match entry.file_type() {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        if ft.is_symlink() {
                            continue;
                        }
                        if ft.is_dir() {
                            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                            if skip_dirs.contains(&name) {
                                continue;
                            }
                            stack.push(path);
                        } else if ft.is_file() {
                            files_scanned += 1;
                            if files_scanned > 5_000 {
                                break;
                            }
                            let meta = entry.metadata().ok();
                            if meta.map(|m| m.len()).unwrap_or(0) > 1_000_000 {
                                continue;
                            }
                            if let Ok(content) = std::fs::read_to_string(&path) {
                                for (idx, line) in content.lines().enumerate() {
                                    if hits.len() >= max_results_c {
                                        break;
                                    }
                                    if re.is_match(line) {
                                        hits.push(format!(
                                            "{}:{}:{}",
                                            path.display(),
                                            idx + 1,
                                            line.trim()
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                hits
            })
            .await
            .map_err(|e| ToolError::failed(format!("search task failed: {e}")))?;
            let count = result.len();
            let body = if result.is_empty() {
                "(no matches)".to_string()
            } else {
                truncate(&result.join("\n"))
            };
            Ok(ToolOutcome::plain(format!(
                "code_search ({} match lines, cap {}):\n{}",
                count, max_results, body
            )))
        }
        .boxed()
    }
}

pub fn starts_with_path_pub(path: &std::path::Path, root: &std::path::Path) -> bool {
    path.components().count() >= root.components().count()
        && path.components().zip(root.components()).all(|(a, b)| {
            a.as_os_str().to_string_lossy().to_lowercase()
                == b.as_os_str().to_string_lossy().to_lowercase()
        })
}
