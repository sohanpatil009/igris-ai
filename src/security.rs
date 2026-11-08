//! Security, permissions, and audit log for IGRIS AI.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use lazy_static::lazy_static;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct Grants {
    #[serde(default)]
    allowed: HashMap<String, bool>,
}

/// Write `contents` via temp file + rename so a crash mid-write never
/// truncates the destination (std `rename` replaces an existing file on
/// Windows). Falls back to a direct write only if the rename machinery fails,
/// and cleans the temp file either way.
fn write_string_atomic(path: &std::path::Path, contents: &str) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "igris-ai.json".to_string());
    let tmp = path.with_file_name(format!("{name}.{}.tmp", std::process::id()));
    let ok = fs::write(&tmp, contents)
        .and_then(|_| fs::rename(&tmp, path))
        .is_ok();
    if !ok {
        let _ = fs::remove_file(&tmp);
        let _ = fs::write(path, contents);
    }
}

pub struct PermissionPolicy {
    path: PathBuf,
    grants: RwLock<Grants>,
}

fn default_base() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("igris-ai")
}

impl PermissionPolicy {
    pub fn load() -> Self {
        Self::load_from(&default_base())
    }

    /// Build the policy from an explicit directory. Tests and alternative
    /// stores use this so the real user grants file is never touched.
    pub fn load_from(base: &std::path::Path) -> Self {
        let base = base.to_path_buf();
        let _ = fs::create_dir_all(&base);
        let path = base.join("permissions.json");
        let grants = if path.exists() {
            let raw = fs::read_to_string(&path).unwrap_or_default();
            match serde_json::from_str::<Grants>(&raw) {
                Ok(g) => g,
                Err(_) => {
                    // Preserve the evidence before the next grant overwrites
                    // the file.
                    let backup = path.with_file_name("permissions.json.bak");
                    let _ = fs::copy(&path, &backup);
                    eprintln!(
                        "[security] permissions.json is corrupt (saved to {}); starting with no grants",
                        backup.display()
                    );
                    Grants::default()
                }
            }
        } else {
            Grants::default()
        };
        Self {
            path,
            grants: RwLock::new(grants),
        }
    }

    pub fn decision(&self, tool: &str, capability: u8) -> PermissionDecision {
        // capability: 0 ReadOnly, 1 Standard, 2 Sensitive, 3 Admin.
        if capability == 0 {
            return PermissionDecision::Allow;
        }
        // Capability denies come before grants: a persisted grant must never
        // un-deny an Admin-class tool.
        if capability >= 3 {
            return PermissionDecision::Deny;
        }
        let granted = self
            .grants
            .read()
            .ok()
            .and_then(|g| g.allowed.get(tool).copied())
            .unwrap_or(false);
        if granted {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Ask
        }
    }

    pub fn grant(&self, tool: &str) {
        match self.grants.write() {
            Ok(mut g) => {
                g.allowed.insert(tool.to_string(), true);
                let json = serde_json::to_string_pretty(&*g).unwrap_or_else(|_| "{}".to_string());
                write_string_atomic(&self.path, &json);
            }
            Err(_) => {
                eprintln!("[security] grants lock poisoned; grant for '{tool}' not persisted")
            }
        }
    }

    pub fn revoke(&self, tool: &str) {
        match self.grants.write() {
            Ok(mut g) => {
                g.allowed.remove(tool);
                let json = serde_json::to_string_pretty(&*g).unwrap_or_else(|_| "{}".to_string());
                write_string_atomic(&self.path, &json);
            }
            Err(_) => {
                eprintln!("[security] grants lock poisoned; revoke of '{tool}' not persisted")
            }
        }
    }

    pub fn is_granted(&self, tool: &str) -> bool {
        self.grants
            .read()
            .ok()
            .and_then(|g| g.allowed.get(tool).copied())
            .unwrap_or(false)
    }
}

pub struct AuditLog {
    path: PathBuf,
    /// Failed audit-write count; keeps the operator warning rate-limited.
    write_failures: AtomicU64,
}

impl AuditLog {
    pub fn load() -> Self {
        Self::load_from(&default_base())
    }

    /// Build the audit log from an explicit directory (tests use this).
    pub fn load_from(base: &std::path::Path) -> Self {
        let base = base.to_path_buf();
        let _ = fs::create_dir_all(&base);
        Self {
            path: base.join("audit.log"),
            write_failures: AtomicU64::new(0),
        }
    }

    pub fn record(&self, tool: &str, args: &str, decision: &str, result: &str) {
        // Security: never let a tool argument or output (which may contain
        // credentials echoed in queries/error messages) land verbatim in the
        // audit trail. Scrub well-known secret shapes, then cap each field.
        const FIELD_CAP: usize = 4096;
        let args = redact_sensitive(args);
        let args = truncate_chars(&args, FIELD_CAP);
        let result = redact_sensitive(result);
        let result = truncate_chars(&result, FIELD_CAP);
        let line = serde_json::json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "tool": tool,
            "args": args,
            "decision": decision,
            "result": result,
        });
        let wrote = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map(|mut f| writeln!(f, "{}", line).is_ok())
            .unwrap_or(false);
        if !wrote {
            let fail = self.write_failures.fetch_add(1, Ordering::Relaxed) + 1;
            // A silent security log is a disabled security log: warn on the
            // first failure, then every 100th so disk/ACL problems surface.
            if fail == 1 || fail.is_multiple_of(100) {
                eprintln!(
                    "[security] audit log write failed ({fail} total): {}",
                    self.path.display()
                );
            }
        }
    }
}

pub fn redact_sensitive(s: &str) -> String {
    lazy_static! {
        static ref RE_KV: Regex = Regex::new(
            r#"(?i)((?:api[_-]?key|api[_-]?token|access[_-]?token|refresh[_-]?token|auth(?:entication)?[_-]?token|client[_-]?secret|password|passwd|secret|authorization|bearer)\s*[:=]\s*)("(?:[^"]*)"|'(?:[^']*)'|[^\s,;\]]{4,})"#
        )
        .unwrap();
        static ref RE_URL: Regex = Regex::new(
            r#"(?i)([?&](?:api[_-]?key|key|token|access_token|secret|auth)=)[^&#\s]+"#
        )
        .unwrap();
        static ref RE_STANDALONE: Vec<Regex> = vec![
            Regex::new(r"sk-[A-Za-z0-9_-]{16,}").unwrap(),
            Regex::new(r"sk-ant-[A-Za-z0-9_-]{16,}").unwrap(),
            Regex::new(r"gh[pousr]_[A-Za-z0-9]{20,}").unwrap(),
            Regex::new(r"xox[baprs]-[A-Za-z0-9-]{10,}").unwrap(),
            Regex::new(r"AIza[A-Za-z0-9_-]{30,}").unwrap(),
            Regex::new(r"pplx-[A-Za-z0-9_-]{20,}").unwrap(),
            Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}").unwrap(),
            Regex::new(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{10,}").unwrap(),
        ];
        static ref RE_BEARER: Regex = Regex::new(r"(?i)(bearer\s+)[A-Za-z0-9._~+/=-]{10,}").unwrap();
    }

    let mut out = RE_KV.replace_all(s, "${1}[REDACTED]").to_string();
    out = RE_URL.replace_all(&out, "${1}[REDACTED]").to_string();
    out = RE_BEARER.replace_all(&out, "${1}[REDACTED]").to_string();
    for re in RE_STANDALONE.iter() {
        out = re.replace_all(&out, "[REDACTED]").to_string();
    }
    out
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str(" …[truncated]");
    out
}
