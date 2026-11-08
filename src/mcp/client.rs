use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::future::FutureExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Mutex, RwLock};

use crate::tools::{
    Capability, Tool, ToolError, ToolErrorKind, ToolOutcome, ToolRegistry, ToolResult,
};

#[derive(Debug, Deserialize)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Clone)]
pub struct McpToolSpec {
    pub full_name: String,
    pub server_name: String,
    pub tool_name: String,
    pub description: String,
    pub schema: Value,
}

pub(crate) struct McpConnection {
    server_name: String,
    io: Mutex<(
        tokio::process::ChildStdin,
        BufReader<tokio::process::ChildStdout>,
    )>,
    _child: Mutex<tokio::process::Child>,
    next_id: AtomicU64,
    descriptors: RwLock<Vec<McpToolSpec>>,
}

#[derive(Clone)]
pub struct McpClient {
    connections: Vec<Arc<McpConnection>>,
}

pub struct McpTool {
    conn: Arc<McpConnection>,
    full_name: String,
    tool_name: String,
    description: &'static str,
    schema: Value,
}

impl McpClient {
    pub async fn load() -> Self {
        let path = std::env::var("IGRIS_MCP_CONFIG")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("mcp.json"));
        let mut client = Self {
            connections: Vec::new(),
        };
        if !path.exists() {
            return client;
        }
        let raw = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[mcp] cannot read {}: {e}", path.display());
                return client;
            }
        };
        let cfg: McpConfig = match serde_json::from_str(&raw) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[mcp] invalid config {}: {e}", path.display());
                return client;
            }
        };
        for server in cfg.servers {
            match connect(server).await {
                Ok(conn) => client.connections.push(conn),
                Err(e) => eprintln!("[mcp] server connect failed: {e}"),
            }
        }
        client
    }

    pub(crate) fn connections(&self) -> &[Arc<McpConnection>] {
        &self.connections
    }

    pub async fn all_descriptors(&self) -> Vec<McpToolSpec> {
        let mut out = Vec::new();
        for conn in &self.connections {
            out.extend(conn.descriptors.read().await.clone());
        }
        out
    }
}

async fn connect(cfg: McpServerConfig) -> Result<Arc<McpConnection>, String> {
    validate_server_config(&cfg)?;
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args);
    for (k, v) in &cfg.env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", cfg.command))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;

    let conn = Arc::new(McpConnection {
        server_name: cfg.name.clone(),
        io: Mutex::new((stdin, BufReader::new(stdout))),
        _child: Mutex::new(child),
        next_id: AtomicU64::new(1),
        descriptors: RwLock::new(Vec::new()),
    });

    // initialize
    let init_result = conn
        .request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "igris-ai", "version": "0.1.0"}
            }),
        )
        .await?;
    let _ = init_result;

    conn.notify("notifications/initialized", json!({})).await;

    let tools_result = conn.request("tools/list", json!({})).await?;
    let mut descriptors = Vec::new();
    if let Some(tools) = tools_result.get("tools").and_then(Value::as_array) {
        for t in tools {
            let name = t
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if name.is_empty() {
                continue;
            }
            let desc = t
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let schema = t
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            let full_name = format!("mcp.{}.{}", cfg.name, name);
            descriptors.push(McpToolSpec {
                full_name,
                server_name: cfg.name.clone(),
                tool_name: name,
                description: desc,
                schema,
            });
        }
    }
    *conn.descriptors.write().await = descriptors;
    Ok(conn)
}

impl McpConnection {
    pub async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut guard = self.io.lock().await;
        guard
            .0
            .write_all(format!("{}\n", req).as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        guard.0.flush().await.map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err("mcp request timed out".to_string());
            }
            let mut line = String::new();
            let read_res = tokio::time::timeout(remaining, guard.1.read_line(&mut line)).await;
            match read_res {
                Ok(Ok(n)) if n == 0 => return Err("mcp server closed stdout".to_string()),
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e.to_string()),
                Err(_) => return Err("mcp request timed out".to_string()),
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("id") != Some(&json!(id)) {
                continue;
            }
            if let Some(err) = v.get("error") {
                return Err(err.to_string());
            }
            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    pub async fn notify(&self, method: &str, params: Value) {
        let req = json!({"jsonrpc": "2.0", "method": method, "params": params});
        if let Ok(mut guard) = self.io.try_lock() {
            let _ = guard.0.write_all(format!("{}\n", req).as_bytes()).await;
            let _ = guard.0.flush().await;
        }
    }

    pub async fn call_tool(&self, tool_name: &str, args: Value) -> Result<Value, String> {
        self.request("tools/call", json!({"name": tool_name, "arguments": args}))
            .await
    }
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn capability(&self) -> Capability {
        Capability::Sensitive
    }
    fn description(&self) -> &'static str {
        self.description
    }
    fn schema(&self) -> Value {
        self.schema.clone()
    }
    fn execute<'a>(&'a self, args: &'a Value) -> futures::future::BoxFuture<'a, ToolResult> {
        use futures::future::{BoxFuture, FutureExt};
        let conn = self.conn.clone();
        let tool_name = self.tool_name.clone();
        let args = args.clone();
        async move {
            match conn.call_tool(&tool_name, args).await {
                Ok(val) => {
                    let text = extract_text(&val);
                    // Cap
                    let text = if text.len() > 24_000 {
                        let mut idx = 24_000;
                        while idx > 0 && !text.is_char_boundary(idx) {
                            idx -= 1;
                        }
                        format!("{} …[truncated]", &text[..idx])
                    } else {
                        text
                    };
                    if val.get("isError").and_then(Value::as_bool).unwrap_or(false) {
                        Err(ToolError::failed(text))
                    } else {
                        Ok(ToolOutcome::plain(text))
                    }
                }
                Err(e) => Err(ToolError::failed(e)),
            }
        }
        .boxed()
    }
}

fn validate_server_config(cfg: &McpServerConfig) -> Result<(), String> {
    if cfg.name.trim().is_empty() {
        return Err("mcp server name is required".to_string());
    }
    if !cfg
        .name
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "mcp server name '{}' must be alphanumeric/underscore/hyphen",
            cfg.name
        ));
    }
    let command = cfg.command.trim();
    if command.is_empty() {
        return Err("mcp server command is required".to_string());
    }
    if command.contains('\0') || command.contains('\n') || command.contains('\r') {
        return Err("mcp server command contains control characters".to_string());
    }
    let cmd_path = std::path::Path::new(command);
    if command.contains(std::path::MAIN_SEPARATOR) {
        if !cmd_path.is_file() {
            return Err(format!("mcp server executable not found: {command}"));
        }
    }
    for (k, v) in &cfg.env {
        if k.is_empty()
            || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || k.chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            return Err(format!("invalid environment variable name: {k:?}"));
        }
        if v.contains('\0') || v.contains('\n') || v.contains('\r') {
            return Err(format!(
                "environment variable {k:?} contains control characters"
            ));
        }
    }
    Ok(())
}

fn extract_text(val: &Value) -> String {
    if let Some(arr) = val.get("content").and_then(Value::as_array) {
        let mut out = String::new();
        for item in arr {
            if let Some(t) = item.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    serde_json::to_string_pretty(val).unwrap_or_default()
}

pub async fn register_mcp_tools(client: &McpClient, registry: &ToolRegistry) {
    let descriptors = client.all_descriptors().await;
    for spec in descriptors {
        let conn = client
            .connections()
            .iter()
            .find(|c| c.server_name == spec.server_name)
            .cloned();
        if let Some(conn) = conn {
            let static_desc = Box::leak(spec.description.into_boxed_str());
            let tool = McpTool {
                conn,
                full_name: spec.full_name,
                tool_name: spec.tool_name,
                description: static_desc,
                schema: spec.schema,
            };
            registry.register(tool);
        }
    }
}
