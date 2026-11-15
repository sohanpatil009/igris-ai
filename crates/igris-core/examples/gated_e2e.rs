//! Gated-action e2e probe (v2.5): drives the production dispatch path of
//! one gated tool from the command line.
//!
//! Usage: `cargo run -p igris-core --example gated_e2e -- <tool> <json-args>`
//! (or `<tool> -` with the JSON in the `GATED_E2E_JSON` env var — env vars
//! survive PowerShell's native-argv quote mangling, argv does not).
//!
//! Every invocation is a fresh process. The first call validates + arms
//! (prints the NOT EXECUTED announcement); a second invocation with the
//! identical arguments confirms and executes — the confirmation state
//! lives in the file-backed pending store (`%APPDATA%\igris\pending.json`),
//! so the arm and the confirm may come from different processes. That is
//! exactly the cross-process flow the CLI one-shot runs use.
//!
//! Live-e2e recipes (Batch 3):
//! - close_app:  start notepad, arm, confirm, check the process is gone;
//! - delete_file: create a temp file, arm, confirm, check it is recycled;
//! - power_mode: read the active scheme first, arm, confirm, restore;
//! - shutdown:   arm ONLY (never confirm here) — then `shutdown /a`
//!   reporting "no shutdown" proves nothing was scheduled; shutdown_abort
//!   is run live as the safe counterpart.

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        eprintln!("Usage: gated_e2e <tool> [json-args]  (or <tool> - with GATED_E2E_JSON set)");
        std::process::exit(2);
    }
    let tool = &args[1];
    let json_text = match args.get(2) {
        Some(t) if t != "-" => t.clone(),
        _ => match std::env::var("GATED_E2E_JSON") {
            Ok(v) => v,
            Err(_) => {
                eprintln!("gated_e2e: no JSON argument and GATED_E2E_JSON is not set");
                std::process::exit(2);
            }
        },
    };
    let json: serde_json::Value = match serde_json::from_str(&json_text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("gated_e2e: bad JSON: {e}");
            std::process::exit(2);
        }
    };
    match igris_core::tools::registry().dispatch(tool, &json) {
        Ok(out) => println!("OK: {out}"),
        Err(e) => {
            println!("ERR: {e}");
            std::process::exit(1);
        }
    }
}
