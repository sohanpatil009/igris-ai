use std::time::Instant;

use igris_ai::security::{AuditLog, PermissionPolicy};
use igris_ai::tools::default_registry;

#[tokio::main]
async fn main() {
    let base = std::env::temp_dir().join("igris-bench");
    let _ = std::fs::create_dir_all(&base);
    let policy = std::sync::Arc::new(PermissionPolicy::load_from(&base));
    let audit = std::sync::Arc::new(AuditLog::load_from(&base));
    let tools = default_registry(policy, audit);

    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("system_info", serde_json::json!({})),
        ("system_info", serde_json::json!({"info":"os"})),
        ("git_status", serde_json::json!({"path":"."})),
        (
            "code_search",
            serde_json::json!({"path":".","query":"fn main","max_results":5}),
        ),
    ];

    let mut rows = String::from("command,mode,exec_ms,success\n");
    let mut timings = Vec::new();
    for (name, args) in &cases {
        let start = Instant::now();
        let ok = tools.execute(name, args).await.is_ok();
        let ms = start.elapsed().as_millis();
        timings.push(ms);
        rows.push_str(&format!("{name},offline,{ms},{ok}\n"));
        println!("{name}: {ms} ms (ok={ok})");
    }
    let _ = std::fs::create_dir_all("benchmarks");
    let _ = std::fs::write("benchmarks/results.csv", rows);
    if !timings.is_empty() {
        let mut sorted = timings.clone();
        sorted.sort();
        let p50 = sorted[sorted.len() / 2];
        let p95 = sorted[(sorted.len() as f64 * 0.95).floor() as usize % sorted.len()];
        println!("p50={} ms p95={} ms ({} samples)", p50, p95, timings.len());
    }
}
