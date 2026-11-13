//! Dynamic tool registry — the runtime list of tools the LLM may call.
//!
//! The registry is the product; individual tools are its tenants (plan
//! §3.3). v1 tenants are registered in code by [`seed_registry`]:
//!
//! - `get_time`  — current local date/time (read-only)
//! - `calculate` — arithmetic, ported from ZeroClaw's
//!   `zeroclaw-tools/src/calculator.rs` (v1 keeps its 13 most
//!   voice-relevant functions of upstream's 25; extend the enum + match
//!   arm when a use case needs more)
//! - `open_app`  — launch one of a hard allowlist of desktop apps through
//!   `std::process::Command`; no shell, no arbitrary paths (gated)
//!
//! Dynamism ladder: v1 code-registered → v1.1 `tools.toml` config-defined
//! tools (name/desc/schema + allowlisted command template, no recompile —
//! `register` replaces a same-name tenant to make overrides possible) → v3
//! MCP/WASM servers (ZeroClaw's `zeroclaw-plugins` pattern).
//!
//! Security policy (voice use): no arbitrary shell execution in v1
//! (ZeroClaw's `shell` tool is deliberately not ported); the `open_app`
//! allowlist is a fixed table; every dispatch runs on a worker thread with
//! a timeout, so a hung tool returns an error result instead of deadlocking
//! the voice loop; dispatches are sequential by construction (the caller
//! runs them one at a time between turns).
//!
//! Results are plain `Result<String, String>`: the string is what the model
//! sees inside the tool_result envelope when Phase 4 wires the loop.

use crate::llm::ToolSpec;
use serde_json::{json, Value};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// Per-tool execution budget (voice UX: never deadlock the loop).
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// One executable tenant of the registry.
pub struct ToolDef {
    pub name: String,
    pub desc: String,
    pub schema: Value,
    pub run: Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>,
}

/// Runtime registry, not a hardcoded table: tools can be added, replaced
/// and (in later ladder steps) loaded from configuration.
#[derive(Default)]
pub struct Registry {
    tools: Vec<ToolDef>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a tenant. Re-registering an existing name replaces it (this is
    /// what later lets config-defined tools override built-ins).
    pub fn register(&mut self, tool: ToolDef) {
        if let Some(slot) = self.tools.iter_mut().find(|t| t.name == tool.name) {
            *slot = tool;
        } else {
            self.tools.push(tool);
        }
    }

    /// Prompt-facing view: sorted by name, so the system prompt block is
    /// byte-stable for the llama-server KV cache.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name.clone(),
                description: t.desc.clone(),
                parameters_schema: t.schema.clone(),
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Run a tool and wait for its result (sequential, voice-context).
    pub fn dispatch(&self, name: &str, args: &Value) -> Result<String, String> {
        self.dispatch_with_timeout(name, args, TOOL_TIMEOUT)
    }

    /// Run a tool on a worker thread with a hard budget. Unknown names and
    /// hung/crashed tools are errors, never panics.
    pub fn dispatch_with_timeout(
        &self,
        name: &str,
        args: &Value,
        timeout: Duration,
    ) -> Result<String, String> {
        let Some(def) = self.tools.iter().find(|t| t.name == name) else {
            return Err(format!("unknown tool: {name}"));
        };
        let run = Arc::clone(&def.run);
        let args = args.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            /* A panic inside a tool drops the sender on unwind; the
             * receiver maps that to an error instead of dying. */
            let _ = tx.send(run(&args));
        });
        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                Err(format!("tool '{name}' timed out after {timeout:?}"))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("tool '{name}' crashed"))
            }
        }
    }
}

/// v1 seed tenants (open decisions 2/3 of 2026-11-13). Later steps swap in
/// the dynamism ladder without touching the call sites.
pub fn seed_registry() -> Registry {
    let mut r = Registry::new();
    r.register(ToolDef {
        name: "calculate".to_string(),
        desc: "Perform arithmetic calculations: add, subtract, multiply, divide, pow, \
              sqrt, abs, modulo, round, sum, average, min, max. Use this tool whenever \
              you need to compute a numeric result instead of guessing."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "function": {
                    "type": "string",
                    "description": "Calculation to perform: add(values), subtract(values), \
                        divide(values), multiply(values), pow(a,b), sqrt(x), abs(x), \
                        modulo(a,b), round(x,decimals), sum(values), average(values), \
                        min(values), max(values)",
                    "enum": [
                        "add", "subtract", "multiply", "divide", "pow", "sqrt", "abs",
                        "modulo", "round", "sum", "average", "min", "max"
                    ]
                },
                "values": {
                    "type": "array",
                    "items": {"type": "number"},
                    "description": "Array of numeric values. Required for: add, subtract, \
                        divide, multiply, sum, average, min, max."
                },
                "a": {"type": "number", "description": "First operand. Required for: pow, modulo."},
                "b": {"type": "number", "description": "Second operand. Required for: pow, modulo."},
                "x": {"type": "number", "description": "Input number. Required for: sqrt, abs, round."},
                "decimals": {"type": "integer", "description": "Decimal places. Required for: round."}
            },
            "required": ["function"]
        }),
        run: Arc::new(run_calculate),
    });
    r.register(ToolDef {
        name: "get_time".to_string(),
        desc: "Get the current local date and time.".to_string(),
        schema: json!({"type": "object", "properties": {}, "required": []}),
        run: Arc::new(run_get_time),
    });
    r.register(ToolDef {
        name: "open_app".to_string(),
        desc: "Open one of the allowed desktop applications (notepad, calc, chrome, \
              spotify, vscode)."
            .to_string(),
        schema: json!({
            "type": "object",
            "properties": {
                "app": {
                    "type": "string",
                    "enum": ["notepad", "calc", "chrome", "spotify", "vscode"],
                    "description": "Which app to open"
                }
            },
            "required": ["app"]
        }),
        run: Arc::new(run_open_app),
    });
    r
}

/// The process-wide registry, seeded once on first use. Both prompt surfaces
/// (the voice history seed in assist.rs and `chat` in llm.rs) and the Phase 4
/// turn loop share this instance, so the specs advertised in the prompt and
/// the executable tools can never drift apart.
pub fn registry() -> &'static Registry {
    static REG: LazyLock<Registry> = LazyLock::new(seed_registry);
    &REG
}

/* ================================================================ */
/* get_time                                                          */
/* ================================================================ */

/// Format stays stable so tests (and the model, next turn) can parse it.
fn run_get_time(_args: &Value) -> Result<String, String> {
    let now = chrono::Local::now();
    Ok(format!(
        "{} on {}",
        now.format("%Y-%m-%d %H:%M:%S"),
        now.format("%A")
    ))
}

/* ================================================================ */
/* calculate — ported from zeroclaw-tools/src/calculator.rs          */
/* (v1 subset; helpers and formatting kept 1:1 with upstream)        */
/* ================================================================ */

const MAX_ROUND_DECIMALS: i64 = 15;
const MAX_VALUES_LEN: usize = 10_000;

fn run_calculate(args: &Value) -> Result<String, String> {
    let function = args
        .get("function")
        .and_then(|v| v.as_str())
        .ok_or("Missing required parameter: function")?;
    match function {
        "add" => calc_add(args),
        "subtract" => calc_subtract(args),
        "multiply" => calc_multiply(args),
        "divide" => calc_divide(args),
        "pow" => calc_pow(args),
        "sqrt" => calc_sqrt(args),
        "abs" => calc_abs(args),
        "modulo" => calc_modulo(args),
        "round" => calc_round(args),
        "sum" => calc_sum(args),
        "average" => calc_average(args),
        "min" => calc_min(args),
        "max" => calc_max(args),
        other => Err(format!("Unknown function: {other}")),
    }
}

fn extract_f64(args: &Value, key: &str, name: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("Missing required parameter: {name}"))
}

fn extract_i64(args: &Value, key: &str, name: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("Missing required parameter: {name}"))
}

fn extract_values(args: &Value, min_len: usize) -> Result<Vec<f64>, String> {
    let values = args
        .get("values")
        .and_then(|v| v.as_array())
        .ok_or("Missing required parameter: values (array of numbers)")?;
    if values.len() < min_len {
        return Err(format!(
            "Expected at least {min_len} value(s), got {}",
            values.len()
        ));
    }
    if values.len() > MAX_VALUES_LEN {
        return Err(format!(
            "values array must be at most {MAX_VALUES_LEN} entries, got {}",
            values.len()
        ));
    }
    let mut nums = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        match v.as_f64() {
            Some(n) => nums.push(n),
            None => return Err(format!("values[{i}] is not a valid number")),
        }
    }
    Ok(nums)
}

/// Integer-shaped floats print without a decimal tail (upstream parity).
fn format_num(n: f64) -> String {
    if n == n.floor() && n.abs() < 1e15 {
        let rounded = n.round() as i128;
        format!("{rounded}")
    } else {
        format!("{n}")
    }
}

fn calc_add(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    Ok(format_num(values.iter().sum()))
}

fn calc_subtract(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut iter = values.iter();
    let mut result = iter
        .next()
        .copied()
        .ok_or("Subtraction requires at least one value")?;
    for v in iter {
        result -= v;
    }
    Ok(format_num(result))
}

fn calc_multiply(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut result = 1.0;
    for v in &values {
        result *= v;
    }
    Ok(format_num(result))
}

fn calc_divide(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut iter = values.iter();
    let mut result = iter
        .next()
        .copied()
        .ok_or("Division requires at least one value")?;
    for v in iter {
        if *v == 0.0 {
            return Err("Division by zero".to_string());
        }
        result /= v;
    }
    Ok(format_num(result))
}

fn calc_pow(args: &Value) -> Result<String, String> {
    let base = extract_f64(args, "a", "a (base)")?;
    let exp = extract_f64(args, "b", "b (exponent)")?;
    Ok(format_num(base.powf(exp)))
}

fn calc_sqrt(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    if x < 0.0 {
        return Err("Cannot compute square root of a negative number".to_string());
    }
    Ok(format_num(x.sqrt()))
}

fn calc_abs(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    Ok(format_num(x.abs()))
}

fn calc_modulo(args: &Value) -> Result<String, String> {
    let a = extract_f64(args, "a", "a")?;
    let b = extract_f64(args, "b", "b")?;
    if b == 0.0 {
        return Err("Modulo by zero".to_string());
    }
    Ok(format_num(a % b))
}

fn calc_round(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    let decimals = extract_i64(args, "decimals", "decimals")?;
    if !(0..=MAX_ROUND_DECIMALS).contains(&decimals) {
        return Err(format!(
            "decimals must be between 0 and {MAX_ROUND_DECIMALS}"
        ));
    }
    let multiplier = 10_f64.powi(decimals as i32);
    Ok(format_num((x * multiplier).round() / multiplier))
}

fn calc_sum(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    Ok(format_num(values.iter().sum()))
}

fn calc_average(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    Ok(format_num(values.iter().sum::<f64>() / values.len() as f64))
}

fn calc_min(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    let Some(min_val) = values.iter().copied().reduce(f64::min) else {
        return Err("Cannot compute min of an empty array".to_string());
    };
    Ok(format_num(min_val))
}

fn calc_max(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    let Some(max_val) = values.iter().copied().reduce(f64::max) else {
        return Err("Cannot compute max of an empty array".to_string());
    };
    Ok(format_num(max_val))
}

/* ================================================================ */
/* open_app — hard allowlist, no shell, no arbitrary paths           */
/* ================================================================ */

const OPEN_APP_HINT: &str = "Allowed apps: notepad, calc, chrome, spotify, vscode";

/// Candidate executables for an allowed app, tried in order (PATH name
/// first, then well-known install locations). Returns None for any name
/// outside the allowlist — that rejection happens before any spawn.
fn app_candidates(app: &str) -> Option<Vec<String>> {
    let path_names: &[&str] = match app {
        "notepad" => &["notepad"],
        "calc" => &["calc"],
        "vscode" => &["code"],
        "chrome" => &["chrome"],
        "spotify" => &["spotify"],
        _ => return None,
    };
    let mut candidates: Vec<String> = path_names.iter().map(|s| s.to_string()).collect();
    /* Well-known install locations for apps that are not on PATH. These
     * are fixed templates, resolved at use time — never caller-supplied. */
    let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let program_files_x86 =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    match app {
        "chrome" => {
            candidates.push(
                std::path::PathBuf::from(&program_files)
                    .join(r"Google\Chrome\Application\chrome.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
            candidates.push(
                std::path::PathBuf::from(&program_files_x86)
                    .join(r"Google\Chrome\Application\chrome.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        "spotify" => {
            if let Ok(appdata) = std::env::var("APPDATA") {
                candidates.push(
                    std::path::PathBuf::from(appdata)
                        .join(r"Spotify\Spotify.exe")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        _ => {}
    }
    Some(candidates)
}

/// Spawns the GUI process detached (the Child handle is dropped, the app
/// keeps running). Note for tests: no unit test dispatches an allowed
/// name — actually launching an app is Phase 5 e2e territory.
fn run_open_app(args: &Value) -> Result<String, String> {
    let app = args
        .get("app")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing required parameter: app. {OPEN_APP_HINT}"))?;
    let Some(candidates) = app_candidates(app) else {
        return Err(format!("'{app}' is not an allowed app. {OPEN_APP_HINT}"));
    };
    let mut last_err = String::new();
    for candidate in candidates {
        match std::process::Command::new(&candidate).spawn() {
            Ok(_child) => return Ok(format!("Launched {app}.")),
            Err(e) => last_err = format!("{candidate}: {e}"),
        }
    }
    Err(format!("Could not launch {app}. {last_err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn reg() -> Registry {
        seed_registry()
    }

    /* ---- registry mechanics ---- */

    #[test]
    fn specs_are_sorted_by_name() {
        let names: Vec<String> = reg().specs().into_iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["calculate", "get_time", "open_app"]);
    }

    #[test]
    fn specs_carry_nonempty_desc_and_object_schema() {
        for spec in reg().specs() {
            assert!(!spec.description.is_empty(), "{}", spec.name);
            assert!(
                spec.parameters_schema.is_object(),
                "{}",
                spec.name
            );
        }
    }

    #[test]
    fn unknown_tool_dispatch_is_an_error_not_a_panic() {
        let err = reg().dispatch("no_such_tool", &json!({})).unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");
    }

    #[test]
    fn register_replaces_same_name_and_keeps_others() {
        let mut r = reg();
        let before = r.tools.len();
        r.register(ToolDef {
            name: "get_time".to_string(),
            desc: "replacement".to_string(),
            schema: json!({"type": "object"}),
            run: Arc::new(|_a| Ok("replaced".to_string())),
        });
        assert_eq!(r.tools.len(), before, "replacement must not duplicate");
        assert_eq!(r.dispatch("get_time", &json!({})).unwrap(), "replaced");
    }

    /* ---- timeout / crash isolation ---- */

    #[test]
    fn hung_tool_returns_error_within_budget() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "slow".to_string(),
            desc: "test tenant".to_string(),
            schema: json!({"type": "object"}),
            run: Arc::new(|_a| {
                std::thread::sleep(Duration::from_secs(30));
                Ok("never".to_string())
            }),
        });
        let t0 = Instant::now();
        let err = r
            .dispatch_with_timeout("slow", &json!({}), Duration::from_millis(150))
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
        assert!(t0.elapsed() < Duration::from_secs(5), "must not wait 30s");
    }

    #[test]
    fn panicking_tool_becomes_an_error_result() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "boom".to_string(),
            desc: "test tenant".to_string(),
            schema: json!({"type": "object"}),
            run: Arc::new(|_a| panic!("intentional test panic")),
        });
        let err = r
            .dispatch_with_timeout("boom", &json!({}), Duration::from_secs(2))
            .unwrap_err();
        assert!(err.contains("crashed"), "{err}");
    }

    /* ---- get_time ---- */

    #[test]
    fn get_time_output_is_stable_and_parseable() {
        let out = reg().dispatch("get_time", &json!({})).unwrap();
        let re = regex::Regex::new(
            r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} on (Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday)$",
        )
        .unwrap();
        assert!(re.is_match(&out), "unexpected get_time output: {out}");
    }

    #[test]
    fn get_time_ignores_extra_args() {
        assert!(reg().dispatch("get_time", &json!({"x": 1})).is_ok());
    }

    /* ---- calculate ---- */

    #[test]
    fn calculate_arithmetic_core() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "add", "values": [2.0, 2.0]}))
                .unwrap(),
            "4"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "add", "values": [1.0, 2.0, 3.5]})
            )
            .unwrap(),
            "6.5"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "subtract", "values": [10.0, 3.0, 1.5]})
            )
            .unwrap(),
            "5.5"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "multiply", "values": [3.0, 4.0, 5.0]})
            )
            .unwrap(),
            "60"
        );
    }

    #[test]
    fn calculate_division_and_div_by_zero() {
        let r = reg();
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "divide", "values": [100.0, 4.0]})
            )
            .unwrap(),
            "25"
        );
        let err = r
            .dispatch(
                "calculate",
                &json!({"function": "divide", "values": [10.0, 0.0]}),
            )
            .unwrap_err();
        assert!(err.contains("zero"), "{err}");
    }

    #[test]
    fn calculate_pow_sqrt_modulo_round() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "pow", "a": 2.0, "b": 10.0}))
                .unwrap(),
            "1024"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "sqrt", "x": 144.0}))
                .unwrap(),
            "12"
        );
        assert!(r
            .dispatch("calculate", &json!({"function": "sqrt", "x": -4.0}))
            .is_err());
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "modulo", "a": 17.0, "b": 5.0}))
                .unwrap(),
            "2"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "round", "x": 2.715, "decimals": 2})
            )
            .unwrap(),
            "2.72"
        );
    }

    #[test]
    fn calculate_aggregates_and_unknown_function() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "sum", "values": [1.0, 2.0, 3.0]}))
                .unwrap(),
            "6"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "average", "values": [10.0, 20.0, 30.0]})
            )
            .unwrap(),
            "20"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "min", "values": [5.0, 1.0, 8.0]}))
                .unwrap(),
            "1"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "max", "values": [5.0, 1.0, 8.0]}))
                .unwrap(),
            "8"
        );
        let err = r
            .dispatch(
                "calculate",
                &json!({"function": "variance", "values": [1.0, 2.0]}),
            )
            .unwrap_err();
        assert!(err.contains("Unknown function"), "{err}");
    }

    #[test]
    fn calculate_garbage_input_is_error_not_panic() {
        let r = reg();
        assert!(r.dispatch("calculate", &json!({})).is_err());
        assert!(r.dispatch("calculate", &json!({"function": "add"})).is_err());
        assert!(r
            .dispatch(
                "calculate",
                &json!({"function": "add", "values": ["a", "b"]})
            )
            .is_err());
        assert!(r
            .dispatch("calculate", &json!({"function": "add", "values": [1.0]}))
            .is_err());
        assert!(r.dispatch("calculate", &json!({"function": "sqrt"})).is_err());
    }

    /* ---- open_app ---- */
    /* No unit test dispatches an allowed name: that would really launch a
     * GUI app. Real launches are Phase 5 e2e. */

    #[test]
    fn open_app_rejects_non_allowlisted_names_before_any_spawn() {
        let err = reg()
            .dispatch("open_app", &json!({"app": "terminal"}))
            .unwrap_err();
        assert!(err.contains("not an allowed app"), "{err}");
    }

    #[test]
    fn open_app_empty_or_missing_args_are_errors() {
        let r = reg();
        assert!(r.dispatch("open_app", &json!({})).is_err());
        assert!(r.dispatch("open_app", &json!({"app": 5})).is_err());
    }

    #[test]
    fn open_app_allowlist_covers_the_decision_3_set() {
        for app in ["notepad", "calc", "chrome", "spotify", "vscode"] {
            assert!(app_candidates(app).is_some(), "{app} should be allowed");
        }
        assert!(app_candidates("word").is_none());
    }

    #[test]
    fn open_app_spotify_candidates_include_the_appdata_install_path() {
        let cands = app_candidates("spotify").unwrap();
        assert!(cands.iter().any(|c| c.ends_with("Spotify.exe")));
    }
}
