//! Micro-benchmark for a safe subset of IGRIS tools
//! (Week 4.4). Run with `cargo run --example benchmark`.

use igris_ai::security::{AuditLog, PermissionPolicy};
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;

fn stats(ns: &mut [u128]) -> (f64, f64, f64) {
    ns.sort();
    if ns.is_empty() {
        (0.0, 0.0, 0.0)
    } else {
        let n = ns.len() as f64;
        let mean = ns.iter().map(|x| *x as f64).sum::<f64>() / n;
        let p50 = ns[ns.len() / 2] as f64;
        let p95 = ns[((ns.len() as f64 * 0.95) as usize).min(ns.len() - 1)] as f64;
        (mean / 1_000_000.0, p50 / 1_000_000.0, p95 / 1_000_000.0)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("igris-bench");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let policy = Arc::new(PermissionPolicy::load_from(&dir));
    let audit = Arc::new(AuditLog::load_from(&dir));
    let r = igris_ai::tools::default_registry(policy, audit);
    r.grant("shell_run");
    r.grant("web_search");
    r.grant("clipboard_read");
    r.grant("screenshot");
    r.grant("file_search");
    r.grant("read_file");
    r.grant("system_info");

    let f = dir.join("note.txt");
    std::fs::write(&f, "hello benchmark content repeated 50 times ".repeat(50))?;

    println!("IGRIS safe-subset benchmark (50 reps/tool, warm)");
    println!("name,mean,p50,p95 (ms)");

    // system_info
    {
        let mut s = Vec::new();
        for _ in 0..50 {
            let t = Instant::now();
            let _ = r.execute("system_info", &json!({})).await;
            s.push(t.elapsed().as_nanos());
        }
        let (m, p50, p95) = stats(&mut s);
        println!("system_info,{m:.3},{p50:.3},{p95:.3}");
    }

    // read_file
    {
        let arg = json!({"path": f.display().to_string()});
        let mut s = Vec::new();
        for _ in 0..50 {
            let t = Instant::now();
            let _ = r.execute("read_file", &arg).await;
            s.push(t.elapsed().as_nanos());
        }
        let (m, p50, p95) = stats(&mut s);
        println!("read_file,{m:.3},{p50:.3},{p95:.3}");
    }

    // shell_run whoami
    {
        let arg = json!({"command":"whoami"});
        let mut s = Vec::new();
        for _ in 0..50 {
            let t = Instant::now();
            let _ = r.execute("shell_run", &arg).await;
            s.push(t.elapsed().as_nanos());
        }
        let (m, p50, p95) = stats(&mut s);
        println!("shell_run whoami,{m:.3},{p50:.3},{p95:.3}");
    }

    Ok(())
}
