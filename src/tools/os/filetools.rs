//! File-search and Recycle-Bin trash tools (plan.md 2.9/2.10).

use futures::future::{BoxFuture, FutureExt};

pub struct FileSearchTool {
    policy: crate::tools::PathPolicy,
}

pub struct FileTrashTool {
    policy: crate::tools::PathPolicy,
}

impl FileSearchTool {
    pub fn new(policy: crate::tools::PathPolicy) -> Self {
        Self { policy }
    }
}

impl FileTrashTool {
    pub fn new(policy: crate::tools::PathPolicy) -> Self {
        Self { policy }
    }
}

impl crate::tools::Tool for FileTrashTool {
    fn name(&self) -> &str {
        "file_trash"
    }
    fn capability(&self) -> crate::tools::Capability {
        crate::tools::Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        "Move a file to the recycle bin (approval required; never permanent delete)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "required":["path"],
            "properties": {"path": {"type":"string"}}
        })
    }
    fn execute<'a>(
        &'a self,
        args: &'a serde_json::Value,
    ) -> BoxFuture<'a, crate::tools::ToolResult> {
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let roots = self.policy.roots();
        let _ = roots;
        let policy = self.policy.clone();
        async move {
            if raw.is_empty() {
                return Err(crate::tools::ToolError::invalid_args("missing arg: path"));
            }
            let validated = policy.validate(raw.as_str())?;
            let path_str = validated.display().to_string();
            let escaped = path_str.replace('\'', "''");
            let script = format!(
                "[Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile('{}', 'OnlyErrorDialogs', 'SendToRecycleBin'); Write-Output 'trashed: {}'",
                escaped, escaped
            );
            #[cfg(target_os = "windows")]
            {
                let out = crate::tools::os::run_powershell(&script, 8).await?;
                if out.success {
                    Ok(crate::tools::ToolOutcome::plain(format!("file_trash: {path_str}")))
                } else {
                    let msg = out.stderr.trim().to_string();
                    Err(crate::tools::ToolError::failed(if msg.is_empty() {
                        format!("trash failed with exit code {}", out.exit_code)
                    } else {
                        msg
                    }))
                }
            }
            #[cfg(not(target_os = "windows"))]
            Err(crate::tools::ToolError::failed("windows-only tool (v1)"))
        }
        .boxed()
    }
}

const SKIP_DIRS: [&str; 4] = ["node_modules", ".git", "target", "Temp"];

fn forbidden_text(s: &str) -> bool {
    s.contains('\'') || s.chars().any(|c| (c as u32) < 0x20)
}

use std::fs;
use std::time::Instant;

impl crate::tools::Tool for FileSearchTool {
    fn name(&self) -> &str {
        "file_search"
    }
    fn capability(&self) -> crate::tools::Capability {
        crate::tools::Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Search files by name and (small-text) content under sandbox roots."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "required":["query"],
            "properties": {
                "query": {"type":"string"},
                "root": {"type":"string"}
            }
        })
    }
    fn execute<'a>(
        &'a self,
        args: &'a serde_json::Value,
    ) -> BoxFuture<'a, crate::tools::ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let root_override = args
            .get("root")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let roots = self.policy.roots();
        let policy = self.policy.clone();
        async move {
            if query.is_empty() {
                return Err(crate::tools::ToolError::invalid_args("missing arg: query"));
            }
            let roots = if root_override.is_empty() {
                roots
            } else {
                vec![policy.validate(root_override.as_str())?]
            };
            let mut results: Vec<(String, u64)> = Vec::new();
            let start = Instant::now();
            walk_search(&roots, &query, 5, &mut results, start);
            if results.is_empty() {
                Ok(crate::tools::ToolOutcome::plain("no matches"))
            } else {
                let mut out = String::new();
                out.push_str(&format!("{} matches:\n", results.len().min(20)));
                for (p, sz) in results.iter().take(20) {
                    out.push_str(&format!("{p} ({sz} bytes)\n"));
                }
                Ok(crate::tools::ToolOutcome::plain(out.trim_end().to_string()))
            }
        }
        .boxed()
    }
}

fn walk_search(
    roots: &[std::path::PathBuf],
    query: &str,
    depth: usize,
    out: &mut Vec<(String, u64)>,
    start: Instant,
) {
    use std::fs;
    let q = query.to_lowercase();
    let mut stack: Vec<(std::path::PathBuf, usize)> =
        roots.iter().map(|r| (r.clone(), 0)).collect();
    while let Some((dir, d)) = stack.pop() {
        if d > depth {
            continue;
        }
        if start.elapsed().as_secs() > 15 {
            return;
        }
        if out.len() >= 20 {
            return;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            if out.len() >= 20 {
                return;
            }
            let path = e.path();
            let ft = match e.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_symlink() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_lowercase();
            if ft.is_dir() {
                if SKIP_DIRS.iter().any(|s| s.eq_ignore_ascii_case(&name)) {
                    continue;
                }
                stack.push((path, d + 1));
            } else if ft.is_file() {
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if name.contains(&q) {
                    out.push((path.display().to_string(), size));
                } else if size > 0 && size < 200_000 {
                    if let Ok(bytes) = fs::read(&path) {
                        if let Ok(text) = String::from_utf8(bytes) {
                            if text.to_lowercase().contains(&q) {
                                out.push((path.display().to_string(), size));
                            }
                        }
                    }
                }
            }
        }
    }
}
