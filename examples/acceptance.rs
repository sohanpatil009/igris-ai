//! IGRIS acceptance / latency harness (Week 4.4)
//! cargo run --example acceptance

use std::sync::Arc;
use std::time::Instant;
use igris_ai::security::{AuditLog, PermissionPolicy};
use igris_ai::tools::ToolRegistry;
use serde_json::json;

fn stats(ns: &mut [u128]) -> (f64, f64, f64) {
    ns.sort();
    if ns.is_empty() {
        (0.0, 0.0, 0.0)
    } else {
        let n = ns.len() as f64;
        let mean = ns.iter().map(|x| *x as f64).sum::<f64>() / n;
        let p50 = ns[ns.len() / 2] as f64;
        let idx = ((ns.len() as f64) * 0.95) as usize;
        let p95 = ns[idx.min(ns.len() - 1)] as f64;
        (mean, p50, p95)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("igris-accept");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let policy = Arc::new(PermissionPolicy::load_from(&dir));
    let audit = Arc::new(AuditLog::load_from(&dir));
    let reg = ToolRegistry::new(policy.clone(), audit);
    for t in [
        "system_info", "app_control", "volume_control", "media_control", "screenshot",
        "clipboard_read", "clipboard_write", "file_search", "read_file", "browser_control",
        "set_timer", "set_reminder", "get_weather", "window_control", "file_write",
        "folder_create", "read_document", "shell_run", "web_search",
    ] {
        reg.grant(t);
    }
    let f = dir.join("notes.txt");
    std::fs::write(&f, "hello note".repeat(4)).unwrap();

    println!("IGRIS acceptance suite: 20 commands, 20 reps each (warm)");
    println!("command,mean_ms,p50_ms,p95_ms,status");

    struct Case { label: &'static str, tool: &'static str, args: serde_json::Value, manual_only: bool }
    let cases = vec![
        Case { label: "open chrome", tool:"app_control", args: json!({"action":"launch","app":"chrome"}), manual_only:false },
        Case { label: "switch to chrome", tool:"app_control", args: json!({"action":"focus","app":"chrome"}), manual_only:false },
        Case { label: "put chrome on left half", tool:"window_control", args: json!({"action":"snap_left","process":"chrome"}), manual_only:false },
        Case { label: "mute", tool:"volume_control", args: json!({"action":"mute"}), manual_only:false },
        Case { label: "take screenshot", tool:"screenshot", args: json!({}), manual_only:false },
        Case { label: "volume up", tool:"volume_control", args: json!({"action":"up"}), manual_only:false },
        Case { label: "lock", tool:"power_control", args: json!({"action":"lock"}), manual_only:true },
        Case { label: "sleep", tool:"power_control", args: json!({"action":"sleep"}), manual_only:true },
        Case { label: "shutdown now", tool:"power_control", args: json!({"action":"shutdown"}), manual_only:true },
        Case { label: "restart now", tool:"power_control", args: json!({"action":"restart"}), manual_only:true },
        Case { label: "pause music", tool:"media_control", args: json!({"action":"pause"}), manual_only:false },
        Case { label: "take screenshot again", tool:"screenshot", args: json!({}), manual_only:false },
        Case { label: "read clipboard", tool:"clipboard_read", args: json!({}), manual_only:false },
        Case { label: "search youtube rust tutorials", tool:"browser_control", args: json!({"action":"open_url","url":"https://www.youtube.com/results?search_query=rust+tutorials"}), manual_only:false },
        Case { label: "open notes.txt", tool:"read_file", args: json!({"path": f.to_str().unwrap()}), manual_only:false },
        Case { label: "weather London", tool:"get_weather", args: json!({"location":"London"}), manual_only:false },
        Case { label: "weather New York", tool:"get_weather", args: json!({"location":"New York"}), manual_only:false },
        Case { label: "set timer 30 seconds", tool:"set_timer", args: json!({"seconds":30}), manual_only:false },
        Case { label: "set reminder 30 sec", tool:"set_reminder", args: json!({"seconds":30,"text":"smoke"}), manual_only:false },
        Case { label: "list running apps", tool:"app_control", args: json!({"action":"list"}), manual_only:false },
    ];
    let mut passes = 0u64;
    for c in cases {
        let mut ns = Vec::new();
        let mut last_status = "FAIL";
        for _ in 0..20 {
            let t = Instant::now();
            match reg.execute(c.tool, &c.args).await {
                Ok(_) => { last_status = "PASS"; }
                Err(e) => {
                    let text = format!("{e}");
                    if text.contains("needs approval") {
                        last_status = if c.manual_only { "GATED" } else { "FAIL" };
                        if let Some(id) = e.pending_id { reg.deny(&id); }
                    } else {
                        last_status = "FAIL";
                    }
                }
            }
            ns.push(t.elapsed().as_nanos());
        }
        ns.sort();
        let (mean, p50, p95) = stats(&mut ns);
        println!("{} ,{:.3},{:.3},{:.3},{}", c.label, mean, p50, p95, last_status);
        if last_status == "PASS" || last_status == "GATED" {
            passes += 1;
        }
    }
    println!("PASS/GATED: {passes}/20");
    Ok(())
}
