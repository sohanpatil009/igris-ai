//! Enterprise tool system for IGRIS AI.
//!
//! Every assistant action goes through a named tool with a declared capability
//! level. The policy engine decides whether to allow, ask, or deny execution.
//! Tools are async (`futures::future::BoxFuture`, no extra dependency), declare
//! a JSON argument schema, and validate their args in the registry before
//! dispatch. Sensitive tools produce a pending approval instead of an error.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use futures::future::{BoxFuture, FutureExt};
use serde::{Deserialize, Serialize};

use crate::security::{AuditLog, PermissionDecision, PermissionPolicy};

pub mod dev;
pub mod os;

/// Canonical tool result type: a structured outcome (with optional undo step)
/// or a coded error.
pub type ToolResult = Result<ToolOutcome, ToolError>;

// ---------------------------------------------------------------------------
// Structured results (plan.md 1.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    InvalidArgs,
    Denied,
    NeedsApproval,
    Timeout,
    NotFound,
    Failed,
}

impl ToolErrorKind {
    const fn as_str(self) -> &'static str {
        match self {
            ToolErrorKind::InvalidArgs => "invalid args",
            ToolErrorKind::Denied => "denied",
            ToolErrorKind::NeedsApproval => "needs approval",
            ToolErrorKind::Timeout => "timeout",
            ToolErrorKind::NotFound => "not found",
            ToolErrorKind::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub message: String,
    /// Present when the error is `NeedsApproval` so callers can offer
    /// `approve <id>` / `deny <id>` without parsing the message.
    pub pending_id: Option<String>,
}

impl ToolError {
    pub fn new(kind: ToolErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            pending_id: None,
        }
    }

    pub fn invalid_args(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::InvalidArgs, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::NotFound, message)
    }
    pub fn denied(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Denied, message)
    }
    pub fn needs_approval(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::NeedsApproval, message)
    }
    pub fn failed(message: impl Into<String>) -> Self {
        Self::new(ToolErrorKind::Failed, message)
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

impl std::error::Error for ToolError {}

/// A reversible step the caller can run later to undo the action. No tool
/// registers one yet (the file-trash tool in Week 2 is the first user); the
/// type exists so tool authors opt in from day one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UndoAction {
    pub description: String,
    /// Tool name that reverses the action, plus its arguments (JSON string).
    pub tool: String,
    pub args: String,
}

#[derive(Debug, Clone, Default)]
pub struct ToolOutcome {
    pub message: String,
    pub undo: Option<UndoAction>,
}

impl ToolOutcome {
    pub fn plain(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            undo: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Capability + trait (plan.md 1.1/1.2/1.3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Capability {
    ReadOnly = 0,
    Standard = 1,
    Sensitive = 2,
    Admin = 3,
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn capability(&self) -> Capability;
    fn description(&self) -> &'static str;
    /// JSON Schema fragment for the tool arguments. Bare `{"type":"object"}`
    /// means "everything accepted"; the registry rejects unknown and
    /// mistyped arguments when `properties` is present.
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({ "type": "object" })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult>;

    /// Optional deep validation beyond JSON Schema (allowlists, path safety,
    /// metachar checks). Runs before permission decisions so invalid or unsafe
    /// arguments are reported as `InvalidArgs` instead of minting approvals or
    /// executing code.
    fn validate(&self, _args: &serde_json::Value) -> Result<(), ToolError> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// JSON-schema validation (plan.md 1.3, hand-rolled on purpose: no schemars)
// ---------------------------------------------------------------------------

/// Validate `args` against the tool schema (plan.md 1.3, hand-rolled on
/// purpose: no schemars).
///
/// Supported subset: `{"type":"object"}` with flat `properties` and
/// `required`. Deliberately stricter than JSON Schema defaults:
/// - anything other than an `object` top-level type (missing, primitive, or
///   union `["string","null"]`) fails **closed** instead of silently skipping
///   validation;
/// - every unknown argument key is rejected whenever `properties` exists,
///   regardless of `additionalProperties`;
/// - required-presence is enforced even for schemas without `properties`.
///
/// Nested object/array item schemas are not recursed (documented v1 limit);
/// a declared `"object"`/`"array"`/`"null"` property type only checks the
/// value's top-level shape.
fn validate_args(schema: &serde_json::Value, args: &serde_json::Value) -> Result<(), String> {
    match schema.get("type").and_then(|t| t.as_str()) {
        Some("object") => {}
        _ => {
            return Err("unsupported schema: top-level type must be object".to_string());
        }
    }
    let Some(obj) = args.as_object() else {
        return Err("arguments must be a JSON object".to_string());
    };
    // Unknown keys first (most informative for a hallucinated argument),
    // then per-property types, then required presence — the required loop
    // runs in both branches, so a schema without `properties` still fails
    // closed on absent keys.
    let props = schema.get("properties").and_then(|p| p.as_object());
    if let Some(props) = &props {
        for key in obj.keys() {
            if !props.contains_key(key) {
                return Err(format!("unknown argument '{key}'"));
            }
        }
        for (key, expected) in props.iter() {
            let Some(value) = obj.get(key) else { continue };
            // Undeclared (`type` absent) is accepted; misdeclared (unknown or
            // union types, which `as_str()` cannot read) fail closed. Property
            // level matches the top level: fail closed for anything unreadable.
            let ok = match expected.get("type") {
                None => true,
                Some(t) => match t.as_str() {
                    Some("string") => value.is_string(),
                    Some("number") => value.is_number(),
                    Some("boolean") => value.is_boolean(),
                    Some("array") => value.is_array(),
                    Some("object") => value.is_object(),
                    Some("null") => value.is_null(),
                    _ => false,
                },
            };
            if !ok {
                let declared = expected
                    .get("type")
                    .and_then(|t| t.as_str())
                    .unwrap_or("(undeclared)");
                return Err(format!("argument '{key}' must be of type {declared}"));
            }
        }
    }
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for name in required.iter().filter_map(|v| v.as_str()) {
            if !obj.contains_key(name) {
                return Err(format!("missing required argument '{name}'"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Path sandbox (reviewer findings folded in: cwd-only default, bounded read
// usage belongs to the tool; see ReadFileTool)
// ---------------------------------------------------------------------------

/// Path sandbox for file tools.
///
/// Allowed roots come from `IGRIS_FILE_ROOTS` (a `;`-separated list; note that
/// root paths containing `;` in a path segment therefore cannot be expressed).
/// When unset or empty the sandbox defaults to the **current working
/// directory only** — the user home is deliberately not included because
/// `read_file` is auto-allowed (`ReadOnly`); add it explicitly via the env var.
///
/// Requested paths are canonicalized before the prefix check so `..`, symlink
/// escapes, and sibling-name confusion (`C:\\Users\\foo2` passing a
/// `C:\\Users\\foo` root) cannot get through.
///
/// v1 limitation: the read is not re-validated against the canonical check
/// after a TOCTOU swap by another local process; the threat model here is the
/// model's own output, not a racing local process.
#[derive(Clone)]
pub struct PathPolicy {
    roots: Vec<PathBuf>,
}

impl PathPolicy {
    pub fn from_env() -> Self {
        let mut roots: Vec<PathBuf> = Vec::new();
        if let Ok(raw) = std::env::var("IGRIS_FILE_ROOTS") {
            for part in raw.split(';') {
                let part = part.trim();
                if !part.is_empty() {
                    roots.push(PathBuf::from(part));
                }
            }
        }
        if roots.is_empty() {
            if let Ok(cwd) = std::env::current_dir() {
                roots.push(cwd);
            }
        }
        Self::from_roots(roots)
    }

    pub fn from_roots(roots: Vec<PathBuf>) -> Self {
        let roots = roots
            .into_iter()
            .map(|r| {
                std::fs::canonicalize(&r)
                    .inspect_err(|_| {
                        eprintln!(
                            "[tools] path sandbox: root '{}' not canonicalizable; keeping raw (paths below it will be denied)",
                            r.display()
                        )
                    })
                    .unwrap_or(r)
            })
            .collect();
        Self { roots }
    }

    /// Roots actually configured (cloned). Exposed for read-only file tools.
    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots.clone()
    }

    /// Canonicalize `raw` and require it to resolve inside a sandbox root.
    ///
    /// All denials carry the same message so a caller cannot distinguish
    /// "does not exist" from "exists outside the sandbox" (existence oracle
    /// closed for the model-output threat model).
    pub fn validate(&self, raw: &str) -> Result<PathBuf, ToolError> {
        const PATH_DENIED: &str = "path denied: outside the sandbox or not accessible";
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ToolError::invalid_args("missing arg: path"));
        }
        let candidate = Path::new(raw);
        if candidate
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(ToolError::invalid_args(PATH_DENIED));
        }
        let resolved =
            std::fs::canonicalize(candidate).map_err(|_| ToolError::invalid_args(PATH_DENIED))?;
        if self
            .roots
            .iter()
            .any(|root| starts_with_path(&resolved, root))
        {
            Ok(resolved)
        } else {
            Err(ToolError::invalid_args(PATH_DENIED))
        }
    }

    /// Resolve a (possibly not-yet-existing) write target inside an allowed root.
    /// Existing paths use the normal `validate`; this variant keeps the parent
    /// canonical/inside allowed roots for new files & directories (no traversal or escape).
    pub fn validate_path_or_parent(&self, raw: &str) -> Result<PathBuf, ToolError> {
        const PATH_DENIED: &str = "path denied: outside the sandbox or not accessible";
        let raw = raw.trim();
        if raw.is_empty() {
            return Err(ToolError::invalid_args("missing arg: path"));
        }
        let candidate = std::path::Path::new(raw);
        if candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(ToolError::invalid_args(PATH_DENIED));
        }
        // If it already exists, delegate to the canonical check.
        if candidate.exists() {
            return self.validate(raw);
        }
        // Parent resolution.
        let parent = candidate
            .parent()
            .ok_or_else(|| ToolError::invalid_args(PATH_DENIED))?;
        let parent = if parent.as_os_str().is_empty() {
            std::env::current_dir().map_err(|_| ToolError::invalid_args(PATH_DENIED))?
        } else {
            parent.to_path_buf()
        };
        let resolved_parent =
            std::fs::canonicalize(&parent).map_err(|_| ToolError::invalid_args(PATH_DENIED))?;
        if !self
            .roots
            .iter()
            .any(|root| starts_with_path(&resolved_parent, root))
        {
            return Err(ToolError::invalid_args(PATH_DENIED));
        }
        let base = resolved_parent;
        let name = candidate
            .file_name()
            .ok_or_else(|| ToolError::invalid_args(PATH_DENIED))?;
        Ok(base.join(name))
    }
}

/// Component-wise prefix check (unicode-folded so NTFS case-insensitivity is
/// approximated without a non-ASCII false denial). Never a string-prefix
/// comparison — `foo2` must not pass a `foo` root.
fn starts_with_path(path: &Path, root: &Path) -> bool {
    let pc: Vec<Component> = path.components().collect();
    let rc: Vec<Component> = root.components().collect();
    pc.len() >= rc.len()
        && pc.iter().zip(rc.iter()).all(|(p, r)| {
            p.as_os_str().to_string_lossy().to_lowercase()
                == r.as_os_str().to_string_lossy().to_lowercase()
        })
}

// ---------------------------------------------------------------------------
// Pending approvals (plan.md 1.4)
// ---------------------------------------------------------------------------

static PENDING_COUNTER: AtomicU64 = AtomicU64::new(1);
const APPROVAL_TTL_MINUTES: i64 = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingAction {
    pub id: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

impl PendingAction {
    fn new(tool: &str, args: &serde_json::Value) -> Self {
        let now = chrono::Utc::now();
        let counter = PENDING_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self {
            id: format!("pnd-{}-{counter}", now.timestamp_millis()),
            tool: tool.to_string(),
            args: args.clone(),
            created_at: now,
            expires_at: now + chrono::Duration::minutes(APPROVAL_TTL_MINUTES),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry (async, plan.md 1.2/1.4)
// ---------------------------------------------------------------------------

/// Normalize an LLM/router tool name to the registry name. One alias map for
/// the whole crate (plan.md 1.5): `search_web` -> `web_search`,
/// `get_system_info` -> `system_info`, `close_app` -> `app_control` (lands Week 2).
pub fn normalize_tool_name(name: &str) -> String {
    match name.trim().to_lowercase().as_str() {
        "search_web" => "web_search".to_string(),
        "get_system_info" => "system_info".to_string(),
        "close_app" => "app_control".to_string(),
        "open_app" => "app_control".to_string(),
        other => other.to_string(),
    }
}

/// Descriptor exposed to other surfaces (MCP server, UI) without exposing
/// the trait object collection.
#[derive(Debug, Clone)]
pub struct ToolDescriptor {
    pub name: String,
    pub capability: Capability,
    pub description: &'static str,
    pub schema: serde_json::Value,
}

pub struct ToolRegistry {
    /// `Arc` per tool so the registry can hand out clones and never hold the
    /// RwLock across an await point.
    tools: RwLock<HashMap<String, Arc<dyn Tool>>>,
    pending: RwLock<HashMap<String, PendingAction>>,
    policy: Arc<PermissionPolicy>,
    audit: Arc<AuditLog>,
}

impl ToolRegistry {
    pub fn new(policy: Arc<PermissionPolicy>, audit: Arc<AuditLog>) -> Self {
        Self {
            tools: RwLock::new(HashMap::new()),
            pending: RwLock::new(HashMap::new()),
            policy,
            audit,
        }
    }

    pub fn register<T: Tool + 'static>(&self, tool: T) {
        match self.tools.write() {
            Ok(mut tools) => {
                tools.insert(tool.name().to_string(), Arc::new(tool));
            }
            Err(_) => eprintln!(
                "[tools] registry lock poisoned; tool '{}' not registered",
                tool.name()
            ),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.tools
            .read()
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Grant a persisting permission, normalizing aliases so the stored key
    /// matches what `decision` looks up at execution time.
    pub fn grant(&self, tool: &str) {
        // Destructive tools are approval-per-call only and can never be
        // persisted as no-approval (BERU security finding). The grant is a
        // no-op for those names, with a visible warning.
        const NON_GRANTABLE: &[&str] = &[
            "power_control",
            "input_automation",
            "file_trash",
            "shell_run",
            "git_commit",
            "run_tests",
        ];
        let normalized = normalize_tool_name(tool);
        if NON_GRANTABLE.contains(&normalized.as_str()) || normalized.starts_with("mcp.") {
            eprintln!(
                "[tools] '{normalized}' cannot be persistently granted; it requires approval for every call"
            );
            return;
        }
        self.policy.grant(normalized.as_str());
    }

    /// Revoke a grant, through the same normalizer as `grant`.
    pub fn revoke(&self, tool: &str) {
        self.policy.revoke(normalize_tool_name(tool).as_str());
    }

    pub fn is_granted(&self, tool: &str) -> bool {
        self.policy.is_granted(normalize_tool_name(tool).as_str())
    }

    /// Fetch a tool snapshot by (already-normalized) name. The RwLock is only
    /// held for the clone — never across an await point.
    fn tool_by_name(&self, name: &str) -> Result<Arc<dyn Tool>, ToolError> {
        self.tools
            .read()
            .map_err(|_| ToolError::failed("tool registry lock poisoned"))?
            .get(name)
            .cloned()
            .ok_or_else(|| ToolError::not_found(format!("unknown tool: {name}")))
    }

    /// Validate `args` against the tool schema (plan.md 1.3) and run the
    /// permission flow. Locks are always dropped before any await.
    pub async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        let raw = name.trim();
        let name = normalize_tool_name(raw);
        let tool = self.tool_by_name(&name).map_err(|_| {
            let label = if raw.eq_ignore_ascii_case(&name) {
                format!("unknown tool: {name}")
            } else {
                format!("unknown tool: {name} (asked as '{raw}')")
            };
            ToolError::not_found(label)
        })?;

        validate_args(&tool.schema(), args)
            .map_err(|e| ToolError::invalid_args(format!("tool '{name}': {e}")))?;
        // Deep (security) validation before any policy check / approval mint.
        tool.validate(args)?;
        let decision = self.policy.decision(&name, tool.capability() as u8);
        let args_str = args.to_string();
        match decision {
            PermissionDecision::Deny => {
                self.audit
                    .record(&name, &args_str, "deny", "denied by policy");
                Err(ToolError::denied(format!(
                    "permission denied for tool '{name}'"
                )))
            }
            PermissionDecision::Ask => {
                let action = PendingAction::new(&name, args);
                let id = action.id.clone();
                match self.pending.write() {
                    Ok(mut pending) => {
                        // Bound the map: drop anything already expired before
                        // adding the new request.
                        let now = chrono::Utc::now();
                        pending.retain(|_, a| now <= a.expires_at);
                        pending.insert(id.clone(), action);
                    }
                    Err(_) => {
                        eprintln!(
                            "[tools] pending lock poisoned; approval for '{name}' not queueable ({id} advertised but not approveable — re-issue the request)"
                        );
                    }
                }
                self.audit
                    .record(&name, &args_str, "ask", "pending approval");
                Err(ToolError {
                    kind: ToolErrorKind::NeedsApproval,
                    message: format!(
                        "permission required for tool '{name}'; approve/deny the pending id"
                    ),
                    pending_id: Some(id),
                })
            }
            PermissionDecision::Allow => {
                self.run_tool(&tool, &name, args, &args_str, "allow").await
            }
        }
    }

    /// Execute a pending action once (single-use; plan.md 1.4).
    ///
    /// Removal **is** the authorization: the entry is consumed atomically
    /// before anything else, so a racing second `approve` cannot double-run
    /// and a racing `deny` cannot lose to an in-flight approve — whoever wins
    /// the `take_pending` runs/marks it, the loser gets NotFound.
    pub async fn approve(&self, id: &str) -> ToolResult {
        let Some(action) = self.take_pending(id) else {
            return Err(ToolError::not_found(format!(
                "no pending action with id: {id}"
            )));
        };
        if chrono::Utc::now() > action.expires_at {
            self.audit.record(
                &action.tool,
                &action.args.to_string(),
                "deny",
                "approval expired",
            );
            return Err(ToolError::needs_approval(format!(
                "pending action {id} expired; re-run the request"
            )));
        }
        // A lookup failure consumes the entry too: single-use even on error
        // (the caller re-issues the request to get a fresh pending).
        let tool = self.tool_by_name(&action.tool)?;
        let args_str = action.args.to_string();
        self.audit
            .record(&action.tool, &args_str, "approved", "single-use approval");
        self.run_tool(&tool, &action.tool, &action.args, &args_str, "approved")
            .await
    }

    pub fn descriptors(&self) -> Vec<ToolDescriptor> {
        let tools = self.tools.read().unwrap_or_else(|e| e.into_inner());
        tools
            .values()
            .map(|tool| ToolDescriptor {
                name: tool.name().to_string(),
                capability: tool.capability(),
                description: tool.description(),
                schema: tool.schema(),
            })
            .collect()
    }

    /// Deny a pending action. Returns true when an action was removed.
    pub fn deny(&self, id: &str) -> bool {
        let Some(action) = self.take_pending(id) else {
            return false;
        };
        self.audit.record(
            &action.tool,
            &action.args.to_string(),
            "deny",
            "rejected by user",
        );
        true
    }

    /// Snapshot of pending approvals (REPL `pending` command). Expired
    /// entries are pruned under the write lock so the operator is never shown
    /// an approval that can no longer run.
    pub fn list_pending(&self) -> Vec<PendingAction> {
        let mut pending = self
            .pending
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = chrono::Utc::now();
        pending.retain(|_, a| now <= a.expires_at);
        let mut items: Vec<PendingAction> = pending.values().cloned().collect();
        items.sort_by_key(|a| a.created_at);
        items
    }

    fn take_pending(&self, id: &str) -> Option<PendingAction> {
        // Same poison policy as `list_pending`: recover the inner map
        // (fail-closed contents, no delta loss) instead of faking NotFound.
        self.pending
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id)
    }

    /// Shared tail: run the tool and audit the result. `decision` is the tag
    /// stored in the audit log ("allow" or "approved"); `args_str` is the
    /// pre-serialized args so the slow path never re-serializes per record.
    async fn run_tool(
        &self,
        tool: &Arc<dyn Tool>,
        name: &str,
        args: &serde_json::Value,
        args_str: &str,
        decision: &'static str,
    ) -> ToolResult {
        let result = tool.execute(args).await;
        let result_str = match &result {
            Ok(outcome) => outcome.message.clone(),
            Err(e) => format!("error: {e}"),
        };
        self.audit.record(name, args_str, decision, &result_str);
        result
    }
}

// ---------------------------------------------------------------------------
// Built-in tools
// ---------------------------------------------------------------------------

pub struct SystemInfoTool;

impl Tool for SystemInfoTool {
    fn name(&self) -> &str {
        "system_info"
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Read-only system information (os, arch, pid, cwd; optional info scope)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": { "info": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, _args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        async {
            let os = std::env::consts::OS;
            let arch = std::env::consts::ARCH;
            let family = std::env::consts::FAMILY;
            let pid = std::process::id();
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "?".to_string());
            Ok(ToolOutcome::plain(format!(
                "os={os} arch={arch} family={family} pid={pid} cwd={cwd}"
            )))
        }
        .boxed()
    }
}

pub struct OpenAppTool;

impl Tool for OpenAppTool {
    fn name(&self) -> &str {
        "open_app"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Open an application by name (allowlisted safe name)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["app"],
            "properties": { "app": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        async {
            let app = args
                .get("app")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if app.is_empty() {
                return Err(ToolError::invalid_args("missing arg: app"));
            }
            // Explicit allowlist, matched case-insensitively. A charset check
            // alone would still pass shells and script interpreters (`cmd`,
            // `powershell`, `mshta`, ...) via `start <app>`; enumeration
            // beats validation for a named asset class.
            const APP_ALLOWLIST: [&str; 14] = [
                "notepad", "calc", "mspaint", "explorer", "chrome", "firefox", "msedge", "edge",
                "brave", "code", "vscode", "spotify", "discord", "taskmgr",
            ];
            let app_lower = app.to_lowercase();
            if !APP_ALLOWLIST.contains(&app_lower.as_str()) {
                return Err(ToolError::invalid_args(format!(
                    "app '{app}' is not in the open_app allowlist"
                )));
            }
            let app = app.to_string();
            let app_for_log = app.clone();
            let spawn = tokio::task::spawn_blocking(move || {
                #[cfg(target_os = "windows")]
                {
                    std::process::Command::new("cmd")
                        .args(["/C", "start", "", &app])
                        .spawn()
                }
                #[cfg(target_os = "macos")]
                {
                    std::process::Command::new("open")
                        .arg("-a")
                        .arg(&app)
                        .spawn()
                }
                #[cfg(target_os = "linux")]
                {
                    std::process::Command::new(&app).spawn()
                }
            })
            .await;
            match spawn {
                Ok(Ok(_)) => Ok(ToolOutcome::plain(format!("launched: {app_for_log}"))),
                Ok(Err(e)) => Err(ToolError::failed(format!("launch failed: {e}"))),
                Err(e) => Err(ToolError::failed(format!("spawn error: {e}"))),
            }
        }
        .boxed()
    }
}

pub struct WebSearchTool;

impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Open a web search for the given query (browser URL open, not an API)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["query"],
            "properties": { "query": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        async move {
            if query.is_empty() {
                return Err(ToolError::invalid_args("missing arg: query"));
            }
            // Prefer a real API search when credentials exist.
            let q = query.clone();
            let api = tokio::task::spawn_blocking(move || {
                crate::online::web_search::search_summary(&q, 5)
            })
            .await;
            match api {
                Ok(Ok(text)) => Ok(ToolOutcome::plain(format!("web_search: {text}"))),
                _ => {
                    // No API keys or network failure -> fall back to browser.
                    let url = format!(
                        "https://www.google.com/search?q={}",
                        crate::tools::urlencoding_simple(&query)
                    );
                    let url_for_open = url.clone();
                    let opened = tokio::task::spawn_blocking(move || open_url(&url_for_open)).await;
                    match opened {
                        Ok(Ok(())) => Ok(ToolOutcome::plain(format!("opened search: {query}"))),
                        Ok(Err(e)) => Err(e),
                        Err(e) => Err(ToolError::failed(format!("spawn error: {e}"))),
                    }
                }
            }
        }
        .boxed()
    }
}

pub struct ReadFileTool {
    policy: PathPolicy,
}

impl ReadFileTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Read a UTF-8 text file inside the path sandbox (canonicalized roots)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        async {
            let raw = args
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let policy = self.policy.clone();
            // Canonicalize + bounded read inside one blocking task so multi-GB
            // files cannot balloon memory (16 KB cap before the preview slice).
            let read = tokio::task::spawn_blocking(move || {
                let path = policy.validate(raw.as_str())?;
                use std::io::Read;
                let file = std::fs::File::open(&path)
                    .map_err(|e| ToolError::failed(format!("open failed: {e}")))?;
                let mut buf = Vec::new();
                file.take(16_384)
                    .read_to_end(&mut buf)
                    .map_err(|e| ToolError::failed(format!("read failed: {e}")))?;
                Ok::<_, ToolError>(buf)
            })
            .await;
            match read {
                Ok(Ok(bytes)) => {
                    let content = String::from_utf8_lossy(&bytes).to_string();
                    if content.is_empty() {
                        return Ok(ToolOutcome::plain("file is empty"));
                    }
                    let preview: String = content.chars().take(4000).collect();
                    Ok(ToolOutcome::plain(preview))
                }
                Ok(Err(e)) => Err(e),
                Err(e) => Err(ToolError::failed(format!("spawn error: {e}"))),
            }
        }
        .boxed()
    }
}

pub struct ShellCommandTool;

impl Tool for ShellCommandTool {
    fn name(&self) -> &str {
        "shell_run"
    }
    fn capability(&self) -> Capability {
        Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        "Run a validated single command (allowlisted, no chaining, permission gated)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["command"],
            "properties": { "command": { "type": "string" } }
        })
    }
    fn validate(&self, args: &serde_json::Value) -> Result<(), ToolError> {
        let command = args
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if command.is_empty() {
            return Err(ToolError::invalid_args("missing arg: command"));
        }

        let allowed = [
            "ipconfig",
            "systeminfo",
            "whoami",
            "tasklist",
            "where",
            "echo",
            "dir",
            "ver",
            "vol",
            "date",
            "time",
        ];
        let first = command.split_whitespace().next().unwrap_or("");
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(first)) {
            return Err(ToolError::invalid_args(format!(
                "command not allowlisted: {first}"
            )));
        }
        let is_forbidden =
            |c: char| "&|;><`$()*?\"'%^!".contains(c) || (c as u32) < 0x20 || (c as u32) == 0x7F;
        if command.chars().any(is_forbidden) {
            return Err(ToolError::invalid_args(
                "unsafe shell metacharacters are not allowed",
            ));
        }
        Ok(())
    }

    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        async {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if command.is_empty() {
                return Err(ToolError::invalid_args("missing arg: command"));
            }

            let allowed = [
                "ipconfig",
                "systeminfo",
                "whoami",
                "tasklist",
                "where",
                "echo",
                "dir",
                "ver",
                "vol",
                "date",
                "time",
            ];
            let first = command.split_whitespace().next().unwrap_or("");
            if !allowed.iter().any(|a| a.eq_ignore_ascii_case(first)) {
                return Err(ToolError::invalid_args(format!(
                    "command not allowlisted: {first}"
                )));
            }
            let is_forbidden = |c: char| {
                "&|;><`$()*?\"'%^!".contains(c) || (c as u32) < 0x20 || (c as u32) == 0x7F
            };
            if command.chars().any(is_forbidden) {
                return Err(ToolError::invalid_args(
                    "unsafe shell metacharacters are not allowed",
                ));
            }

            let command = command.to_string();
            let output = tokio::task::spawn_blocking(move || {
                std::process::Command::new("cmd")
                    .args(["/C", &command])
                    .output()
            })
            .await;
            match output {
                Ok(Ok(out)) => {
                    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    if !out.status.success() {
                        return Err(ToolError::failed(if stderr.is_empty() {
                            format!("command failed with exit code {}", out.status)
                        } else {
                            stderr
                        }));
                    }
                    let preview = if stdout.chars().count() > 3000 {
                        format!("{}...", stdout.chars().take(3000).collect::<String>())
                    } else {
                        stdout
                    };
                    Ok(ToolOutcome::plain(if preview.is_empty() {
                        "command exited successfully".to_string()
                    } else {
                        preview
                    }))
                }
                Ok(Err(e)) => Err(ToolError::failed(format!("command spawn failed: {e}"))),
                Err(e) => Err(ToolError::failed(format!("spawn error: {e}"))),
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Document reading (Week 3.4) — text-friendly formats (no PDF crate yet)
// ---------------------------------------------------------------------------

pub struct ReadDocumentTool {
    policy: PathPolicy,
}

impl ReadDocumentTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for ReadDocumentTool {
    fn name(&self) -> &str {
        "read_document"
    }
    fn capability(&self) -> Capability {
        Capability::ReadOnly
    }
    fn description(&self) -> &'static str {
        "Read a document (text, markdown, JSON, CSV, log) inside the sandbox."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": {
                "path": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let policy = self.policy.clone();
        async move {
            if raw.is_empty() {
                return Err(ToolError::invalid_args("missing arg: path"));
            }
            let path = policy.validate(raw.as_str())?;
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            match ext.as_str() {
                "pdf" | "doc" | "docx" => {
                    return Err(ToolError::invalid_args(format!(
                        "binary document format .{ext} is not readable as text yet; use read_file for text"
                    )));
                }
                "png" | "jpg" | "jpeg" | "gif" | "bmp" => {
                    return Err(ToolError::invalid_args(
                        "image files are not documents".to_string(),
                    ));
                }
                _ => {}
            }
            // Bounded read (64 KB) to avoid memory spikes.
            tokio::task::spawn_blocking(move || -> Result<String, ToolError> {
                use std::io::Read;
                let file = std::fs::File::open(&path)
                    .map_err(|e| ToolError::failed(format!("open failed: {e}")))?;
                let mut buf = vec![0u8; 65536];
                let n = file
                    .take(65536)
                    .read(&mut buf)
                    .map_err(|e| ToolError::failed(format!("read failed: {e}")))?;
                buf.truncate(n);
                Ok(String::from_utf8_lossy(&buf).to_string())
            })
            .await
            .map_err(|e| ToolError::failed(format!("spawn error: {e}")))?
            .map(|content| {
                if content.trim().is_empty() {
                    ToolOutcome::plain("document is empty".to_string())
                } else {
                    let preview: String = content.chars().take(4000).collect();
                    ToolOutcome::plain(format!("document preview ({ext}): {preview}"))
                }
            })
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// Timers / reminders (Week 3.6) — native Windows countdown, no extra crates
// ---------------------------------------------------------------------------

pub struct SetTimerTool;

impl SetTimerTool {
    pub fn parse_seconds(args: &serde_json::Value) -> Result<u64, ToolError> {
        if let Some(n) = args.get("seconds").and_then(|v| v.as_u64()) {
            if n == 0 {
                return Err(ToolError::invalid_args("seconds must be > 0"));
            }
            return Ok(n);
        }
        if let Some(text) = args.get("duration").and_then(|v| v.as_str()) {
            let cleaned = text.trim().to_lowercase();
            // Support "30s", "5m", "1h", "2 minutes", "30 seconds".
            let mut total: u64 = 0;
            let mut num = String::new();
            for ch in cleaned.chars() {
                if ch.is_ascii_digit() {
                    num.push(ch);
                } else {
                    let n: u64 = num.parse().unwrap_or(0);
                    num.clear();
                    match ch {
                        's' => total += n,
                        'm' => total += n * 60,
                        'h' => total += n * 3600,
                        ' ' | ',' | '\t' => {}
                        _ => {}
                    }
                }
            }
            if !num.is_empty() {
                let n: u64 = num.parse().unwrap_or(0);
                // Default unit when none specified = seconds.
                if total == 0 {
                    total = n;
                }
            }
            if total > 0 {
                return Ok(total);
            }
        }
        Err(ToolError::invalid_args(
            "provide 'seconds' (number) or 'duration' like '5m'/'30s'/'1h'",
        ))
    }
}

impl Tool for SetTimerTool {
    fn name(&self) -> &str {
        "set_timer"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Start a countdown timer and show a notification when it completes."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": { "type": "number" },
                "duration": { "type": "string" },
                "message": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let secs = match SetTimerTool::parse_seconds(args) {
            Ok(n) => n,
            Err(e) => return std::future::ready(Err(e)).boxed(),
        };
        let message = args
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Timer finished")
            .to_string();
        async move {
            #[cfg(target_os = "windows")]
            {
                let msg = message.clone();
                let spawned = std::process::Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        &format!(
                            "Start-Sleep -Seconds {secs}; Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.MessageBox]::Show('{}', 'IGRIS timer') | Out-Null",
                            message.replace('\'', "''")
                        ),
                    ])
                    .spawn();
                match spawned {
                    Ok(_) => Ok(ToolOutcome::plain(format!(
                        "timer set: {} seconds; notification when done",
                        secs
                    ))),
                    Err(e) => Err(ToolError::failed(format!("timer failed: {e}"))),
                }
            }
            #[cfg(not(target_os = "windows"))]
            {
                let _ = (secs, msg);
                Err(ToolError::failed("windows-only tool (v1)"))
            }
        }
        .boxed()
    }
}

pub struct SetReminderTool;

impl Tool for SetReminderTool {
    fn name(&self) -> &str {
        "set_reminder"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Set a reminder (text) to fire after a delay."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "seconds": { "type": "number" },
                "duration": { "type": "string" },
                "text": { "type": "string" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let secs = match SetTimerTool::parse_seconds(args) {
            Ok(n) => n,
            Err(e) => return std::future::ready(Err(e)).boxed(),
        };
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("Reminder")
            .to_string();
        async move {
            #[cfg(target_os = "windows")]
            {
                let spawned = std::process::Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        &format!(
                            "Start-Sleep -Seconds {secs}; Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.MessageBox]::Show('{}', 'IGRIS reminder') | Out-Null",
                            text.replace('\'', "''")
                        ),
                    ])
                    .spawn();
                match spawned {
                    Ok(_) => Ok(ToolOutcome::plain(format!(
                        "reminder set in {} seconds: {text}",
                        secs
                    ))),
                    Err(e) => Err(ToolError::failed(format!("reminder failed: {e}"))),
                }
            }
            #[cfg(not(target_os = "windows"))]
            Err(ToolError::failed("windows-only tool (v1)"))
        }
        .boxed()
    }
}

fn open_url(url: &str) -> Result<(), ToolError> {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(ToolError::invalid_args("invalid url"));
    }
    if url.chars().any(|c| {
        (c as u32) < 0x20
            || c == '\''
            || c == '"'
            || c == '`'
            || c == '$'
            || c == ';'
            || c == '|'
            || c == '&'
    }) {
        return Err(ToolError::invalid_args("invalid url"));
    }
    // Use Start-Process via PowerShell (no cmd.exe re-parsing) so URL
    // metacharacters cannot be treated as command separators.
    let escaped = url.replace('\'', "''");
    let spawned = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &format!("Start-Process '{}'", escaped),
        ])
        .spawn();
    match spawned {
        Ok(_) => Ok(()),
        Err(e) => Err(ToolError::failed(format!("open failed: {e}"))),
    }
}

fn urlencoding_simple(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Default registry
// ---------------------------------------------------------------------------

pub fn default_registry(policy: Arc<PermissionPolicy>, audit: Arc<AuditLog>) -> ToolRegistry {
    let r = ToolRegistry::new(policy.clone(), audit.clone());
    r.register(SystemInfoTool);
    r.register(WebSearchTool);
    r.register(ReadFileTool::new(PathPolicy::from_env()));
    r.register(ShellCommandTool);
    let os_policy = PathPolicy::from_env();
    let dev_policy = PathPolicy::from_env();
    r.register(crate::tools::os::AppControlTool);
    r.register(crate::tools::os::WindowControlTool);
    r.register(crate::tools::os::VolumeControlTool);
    r.register(crate::tools::os::PowerControlTool);
    r.register(crate::tools::os::BrightnessControlTool);
    r.register(crate::tools::os::ScreenshotTool);
    r.register(crate::tools::os::ClipboardReadTool);
    r.register(crate::tools::os::ClipboardWriteTool);
    r.register(crate::tools::os::FileSearchTool::new(os_policy.clone()));
    r.register(crate::tools::os::FileTrashTool::new(os_policy.clone()));
    r.register(crate::tools::os::MediaControlTool);
    r.register(crate::tools::os::InputAutomationTool);
    r.register(ReadDocumentTool::new(PathPolicy::from_env()));
    r.register(SetTimerTool);
    r.register(SetReminderTool);
    r.register(GetWeatherTool);
    r.register(FileWriteTool::new(PathPolicy::from_env()));
    r.register(FolderCreateTool::new(PathPolicy::from_env()));
    r.register(crate::tools::dev::GitStatusTool::new(dev_policy.clone()));
    r.register(crate::tools::dev::GitDiffTool::new(dev_policy.clone()));
    r.register(crate::tools::dev::GitLogTool::new(dev_policy.clone()));
    r.register(crate::tools::dev::GitCommitTool::new(dev_policy.clone()));
    r.register(crate::tools::dev::RunTestsTool::new(dev_policy.clone()));
    r.register(crate::tools::dev::CodeSearchTool::new(dev_policy.clone()));
    r
}

// ---------------------------------------------------------------------------
// Weather (Week 3.5)
// ---------------------------------------------------------------------------

pub struct GetWeatherTool;

impl Tool for GetWeatherTool {
    fn name(&self) -> &str {
        "get_weather"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Current weather for a city (OpenWeather when key present; else browser search)."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["location"],
            "properties": { "location": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let loc = args
            .get("location")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        async move {
            if loc.is_empty() {
                return Err(ToolError::invalid_args("missing arg: location"));
            }
            if let Ok(key) = std::env::var("OPENWEATHER_API_KEY") {
                if !key.trim().is_empty() {
                    let key = key.trim().to_string();
                    let loc2 = loc.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        let client = reqwest::blocking::Client::builder()
                            .timeout(std::time::Duration::from_secs(10))
                            .build()
                            .map_err(|e| e.to_string())?;
                        let url = format!(
                            "https://api.openweathermap.org/data/2.5/weather?q={}&units=metric&appid={}",
                            crate::tools::urlencoding_simple(&loc2),
                            key
                        );
                        let resp = client.get(&url).send().map_err(|e| e.to_string())?;
                        if !resp.status().is_success() {
                            return Err(format!("weather HTTP {}", resp.status()));
                        }
                        let v: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
                        let temp = v
                            .get("main")
                            .and_then(|x| x.get("temp"))
                            .and_then(|x| x.as_f64())
                            .unwrap_or(0.0);
                        let desc = v
                            .get("weather")
                            .and_then(|x| x.get(0))
                            .and_then(|x| x.get("description"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        let humid = v
                            .get("main")
                            .and_then(|x| x.get("humidity"))
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0);
                        Ok(format!(
                            "weather in {loc2}: {temp:.1} C, {desc}, humidity {humid}%"
                        ))
                    })
                    .await;
                    match result {
                        Ok(Ok(text)) => return Ok(ToolOutcome::plain(text)),
                        Ok(Err(_)) | Err(_) => {}
                    }
                }
            }
            // Fallback browser.
            let q = format!("weather in {}", loc);
            let url = format!(
                "https://www.google.com/search?q={}",
                crate::tools::urlencoding_simple(&q)
            );
            let opened = tokio::task::spawn_blocking(move || open_url(&url)).await;
            match opened {
                Ok(Ok(())) => Ok(ToolOutcome::plain(format!("opened search: {q}"))),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(ToolError::failed(format!("spawn error: {e}"))),
            }
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// File write / folder create (TODO.md Phase 3 hardening utilities)
// ---------------------------------------------------------------------------

pub struct FileWriteTool {
    policy: PathPolicy,
}

pub struct FolderCreateTool {
    policy: PathPolicy,
}

impl FileWriteTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl FolderCreateTool {
    pub fn new(policy: PathPolicy) -> Self {
        Self { policy }
    }
}

impl Tool for FileWriteTool {
    fn name(&self) -> &'static str {
        "file_write"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Write (or append) a UTF-8 text file inside the path sandbox. Existing files are backed up to .bak first."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path", "content"],
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" },
                "append": { "type": "boolean" }
            }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let append = args
            .get("append")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let policy = self.policy.clone();
        async move {
            if raw.is_empty() {
                return Err(ToolError::invalid_args("missing arg: path"));
            }
            let resolved = policy.validate_path_or_parent(raw.as_str())?;
            let parent = resolved
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."));
            if !parent.is_dir() {
                return Err(ToolError::invalid_args("parent directory does not exist"));
            }
            let target = resolved.clone();
            // Back up existing file before overwrite (unless appending).
            if !append && target.exists() {
                let backup = target.with_extension("bak");
                if let Err(e) = std::fs::copy(&target, &backup) {
                    return Err(ToolError::failed(format!("backup failed: {e}")));
                }
            }
            if append {
                use std::io::Write;
                let mut f = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&target)
                    .map_err(|e| ToolError::failed(format!("open failed: {e}")))?;
                f.write_all(content.as_bytes())
                    .map_err(|e| ToolError::failed(format!("write failed: {e}")))?;
            } else {
                std::fs::write(&target, &content)
                    .map_err(|e| ToolError::failed(format!("write failed: {e}")))?;
            }
            Ok(ToolOutcome::plain(format!(
                "file_write: {} ({} bytes; backup in .bak)",
                target.display(),
                content.len()
            )))
        }
        .boxed()
    }
}

impl Tool for FolderCreateTool {
    fn name(&self) -> &'static str {
        "folder_create"
    }
    fn capability(&self) -> Capability {
        Capability::Standard
    }
    fn description(&self) -> &'static str {
        "Create a directory inside the path sandbox."
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string" } }
        })
    }
    fn execute<'a>(&'a self, args: &'a serde_json::Value) -> BoxFuture<'a, ToolResult> {
        let raw = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let policy = self.policy.clone();
        async move {
            if raw.is_empty() {
                return Err(ToolError::invalid_args("missing arg: path"));
            }
            let resolved = policy.validate_path_or_parent(raw.as_str())?;
            std::fs::create_dir_all(&resolved)
                .map_err(|e| ToolError::failed(format!("create_dir failed: {e}")))?;
            Ok(ToolOutcome::plain(format!(
                "folder_create: {}",
                resolved.display()
            )))
        }
        .boxed()
    }
}
