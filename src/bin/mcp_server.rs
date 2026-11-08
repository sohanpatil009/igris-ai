use serde_json::{json, Value};
use std::io::{BufRead, Write};

use igris_ai::security::{AuditLog, PermissionPolicy};
use igris_ai::tools::default_registry;

#[tokio::main]
async fn main() {
    let policy = std::sync::Arc::new(PermissionPolicy::load());
    let audit = std::sync::Arc::new(AuditLog::load());
    let tools = default_registry(policy, audit);

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

        if method == "notifications/initialized" {
            continue;
        }

        let result = match method {
            "initialize" => json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "igris-ai", "version": "0.1.0" }
            }),
            "notifications/initialized" => Value::Null,
            "tools/list" => {
                let arr: Vec<Value> = tools
                    .descriptors()
                    .into_iter()
                    .map(|d| {
                        json!({
                            "name": d.name,
                            "description": d.description,
                            "inputSchema": d.schema
                        })
                    })
                    .collect();
                json!({ "tools": arr })
            }
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match tools.execute(name, &args).await {
                    Ok(outcome) => json!({
                        "content": [{ "type": "text", "text": outcome.message }],
                        "isError": false
                    }),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": e.message }],
                        "isError": true
                    }),
                }
            }
            _ => json!({"code": -32601, "message": "Method not found"}),
        };

        if id.is_some() {
            // For tool errors we still respond with JSON-RPC result above.
            let resp = if method == "initialize" || method == "tools/list" || method == "tools/call"
            {
                json!({"jsonrpc":"2.0","id":id,"result":result})
            } else if method == "notifications/initialized" {
                continue;
            } else {
                json!({"jsonrpc":"2.0","id":id,"error":result})
            };
            let _ = writeln!(stdout, "{}", resp);
            let _ = stdout.flush();
        }
    }
}
