//! Tool registry core: the ToolDef contract, the Registry (registration,
//! sorted spec export, thread-isolated dispatch with per-tool time
//! budgets), and the schema-confused-args unwrap.

use crate::llm::ToolSpec;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Default per-tool execution budget (voice UX: never deadlock the loop).
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Tools that legitimately outlive the 5 s voice budget. One table on
/// purpose: a single glance shows everything allowed to run long, and the
/// dispatch layer stays the only place that knows about budgets. Barge-in
/// still cuts the whole turn, so a slow build never traps the user.
fn tool_budget(name: &str) -> Duration {
    match name {
        // dev commands: a cold `cargo build`/`test` can take minutes
        "run_command" => Duration::from_secs(120),
        // streaming a large file over a slow link
        "download_file" => Duration::from_secs(120),
        // plain page loads / one search round-trip
        "web_fetch" => Duration::from_secs(25),
        "web_search" => Duration::from_secs(20),
        _ => TOOL_TIMEOUT,
    }
}

/// One executable tenant of the registry.
#[derive(Clone)]
pub struct ToolDef {
    pub name: String,
    pub desc: String,
    pub schema: Value,
    /// Gated tools (v2.5) validate + announce on their first call and
    /// only execute after a confirmed re-call in a later turn (see
    /// [`crate::gating`]). Config-defined tools are always ungated, and
    /// they may not shadow a gated built-in name.
    pub gated: bool,
    /// Gated tools only: validates the arguments and returns the plan
    /// announced to the user on arm. An error means the arguments are
    /// invalid — the call fails before anything arms or executes.
    pub plan: Option<Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>>,
    pub run: Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>,
}

/* Manual Debug: the `run` closure is an opaque Arc<dyn Fn>, so a derived
 * impl is impossible; tests use unwrap_err()/unwrap() on tool vectors. */
impl std::fmt::Debug for ToolDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolDef")
            .field("name", &self.name)
            .field("desc", &self.desc)
            .field("schema", &self.schema)
            .field("gated", &self.gated)
            .field("plan", &self.plan.is_some())
            .field("run", &"<closure>")
            .finish()
    }
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
    /// byte-stable for the llama-server KV cache. Gated tools carry a
    /// short suffix so the model knows which calls need confirmation.
    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self
            .tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name.clone(),
                description: if t.gated {
                    format!("{} (gated — needs the user's confirmation before it acts)", t.desc)
                } else {
                    t.desc.clone()
                },
                parameters_schema: t.schema.clone(),
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Is this name a gated tenant? (Config-defined tools may not shadow
    /// gated names — see [`Registry::register_ungated_guarded`].)
    pub fn is_gated(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.name == name && t.gated)
    }

    /// Register a config-defined (always ungated) tenant. A same-name
    /// shadow of a gated built-in is refused with a stderr note: a plain
    /// replace would silently un-gate a destructive action.
    pub fn register_ungated_guarded(&mut self, def: ToolDef) {
        if self.is_gated(&def.name) {
            eprintln!(
                "tools: config-defined '{}' shadows a gated built-in and is ignored",
                def.name
            );
            return;
        }
        self.register(def);
    }

    /// Run a tool and wait for its result (sequential, voice-context).
    /// The budget comes from [`tool_budget`]: 5 s for everything except
    /// the v2 dev/network tools that declare a longer one. Gated tools
    /// go through the two-step confirmation flow (see [`crate::gating`]):
    /// without a fresh cross-turn pending they only validate + arm and
    /// return the NOT EXECUTED announcement; the confirmed re-call
    /// executes. The model-facing status stays plain "ok"/"error".
    pub fn dispatch(&self, name: &str, args: &Value) -> Result<String, String> {
        let Some(def) = self.tools.iter().find(|t| t.name == name) else {
            return Err(format!("unknown tool: {name}"));
        };
        let args = unwrap_schema_confused_args(&def.schema, &args.clone());
        if !def.gated {
            return self.exec_def(def, &args, tool_budget(name));
        }
        let step = {
            let mut store = crate::gating::global().lock().map_err(|_| {
                "gating: pending store lock poisoned".to_string()
            })?;
            self.dispatch_gated_core(
                &mut store,
                crate::gating::current_turn(),
                std::process::id(),
                def,
                &args,
            )
        };
        self.log_gated_step(def, &step);
        match step {
            Ok(GatedOutcome::Armed { plan }) => Ok(crate::gating::armed_result(&plan)),
            Ok(GatedOutcome::Executed(result)) => result,
            Err(e) => Err(e),
        }
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
        let args = unwrap_schema_confused_args(&def.schema, &args.clone());
        self.exec_def(def, &args, timeout)
    }

    /// Execute one resolved tenant on a worker thread with a hard budget
    /// (the shared body of [`Self::dispatch`], [`Self::dispatch_with_timeout`]
    /// and the gated execute step).
    fn exec_def(&self, def: &ToolDef, args: &Value, timeout: Duration) -> Result<String, String> {
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
                Err(format!("tool '{}' timed out after {timeout:?}", def.name))
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("tool '{}' crashed", def.name))
            }
        }
    }

    /// The pure gating decision for one gated call against an explicit
    /// pending store, turn and process id — the unit-testable core; the
    /// production [`Self::dispatch`] supplies the global store and the
    /// real counters. A fresh cross-turn (or cross-process) exact match
    /// consumes the pending and executes; everything else validates +
    /// arms and reports the plan.
    fn dispatch_gated_core(
        &self,
        store: &mut crate::gating::PendingStore,
        turn: u64,
        pid: u32,
        def: &ToolDef,
        args: &Value,
    ) -> Result<GatedOutcome, String> {
        let canonical = crate::toolparse::canonicalize_json_for_tool_signature(args).to_string();
        if store
            .take_fresh(
                &def.name,
                &canonical,
                turn,
                pid,
                crate::gating::now_millis(),
            )
            .is_some()
        {
            return Ok(GatedOutcome::Executed(self.exec_def(
                def,
                args,
                tool_budget(&def.name),
            )));
        }
        let Some(plan) = &def.plan else {
            return Err(format!("tool '{}' is gated but defines no plan", def.name));
        };
        let plan_text = plan(args).map_err(|e| format!("cannot plan '{}': {e}", def.name))?;
        store.arm(
            &def.name,
            canonical,
            &plan_text,
            turn,
            pid,
            crate::gating::now_millis(),
        );
        Ok(GatedOutcome::Armed { plan: plan_text })
    }

    /// Activity-log lines for the two gated outcomes (arm + execute), so
    /// "what did you do" questions (memory_recall) can answer for gated
    /// actions without the model having to call activity_log itself.
    fn log_gated_step(&self, def: &ToolDef, step: &Result<GatedOutcome, String>) {
        let tag = [def.name.clone()];
        match step {
            Ok(GatedOutcome::Armed { plan }) => {
                crate::gating::log_activity(&format!("requested: {plan}"), &tag);
            }
            Ok(GatedOutcome::Executed(Ok(out))) => {
                let detail = super::shared::truncate_chars(out, 200);
                crate::gating::log_activity(&format!("executed {}: {detail}", def.name), &tag);
            }
            Ok(GatedOutcome::Executed(Err(e))) => {
                crate::gating::log_activity(&format!("executed {}: FAILED: {e}", def.name), &tag);
            }
            Err(_) => {} /* nothing armed, nothing ran */
        }
    }
}

/// What a gated dispatch decided to do. Internal machinery only — the
/// model-facing status stays the plain "ok"/"error" (a third status
/// value reliably confuses the model's envelope handling, v1.1 lesson).
#[derive(Debug, PartialEq)]
enum GatedOutcome {
    /// Validated + armed; the announcement plan awaits confirmation.
    Armed { plan: String },
    /// A fresh cross-turn match consumed a pending and the tool ran.
    Executed(Result<String, String>),
}

/// LFM3 sometimes emits the DSML attr-form with the schema name instead of
/// the argument name — `parameter=properties` (or `parameter=parameters`)
/// with the intended arguments object as the body. The parser faithfully
/// keys that body under "properties", so the real arguments end up one
/// level deep: `{"properties": {"volume": 40}}`. When the tool's schema
/// declares no parameter by that name, the wrapper is a formatting
/// artifact: strip one level and dispatch the intended arguments. The
/// honest-error alternative costs the model a recovery round it often
/// cannot find — e2e evidence (2026-11-14): `set_volume` burned 5 rounds
/// on the same confused shape and gave up, while a scalar-string-arg tool
/// (`media_key`) does self-heal in one round. Double-dispatch is not a
/// risk: dedup is per-response and this runs inside a single dispatch.
fn unwrap_schema_confused_args(schema: &Value, args: &Value) -> Value {
    const WRAPPER_KEYS: [&str; 2] = ["properties", "parameters"];
    let Some(map) = args.as_object() else {
        return args.clone();
    };
    if map.len() != 1 {
        return args.clone();
    }
    let (key, value) = map.iter().next().expect("len checked above");
    if !WRAPPER_KEYS.contains(&key.as_str()) || !value.is_object() {
        return args.clone();
    }
    /* A schema that genuinely declares the key keeps it. */
    let declared = schema
        .get("properties")
        .and_then(|p| p.as_object())
        .map(|p| p.contains_key(key));
    if declared != Some(false) {
        return args.clone();
    }
    value.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::reg;
    use serde_json::json;
    use std::time::Instant;

    /* ---- registry mechanics ---- */

    #[test]
    fn specs_are_sorted_by_name() {
        let names: Vec<String> = reg().specs().into_iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "activity_log",
                "calculate",
                "content_search",
                "create_folder",
                "create_note",
                "download_file",
                "file_search",
                "get_time",
                "list_dir",
                "media_key",
                "memory_recall",
                "memory_save",
                "open_app",
                "open_url",
                "read_file",
                "reminder_set",
                "run_command",
                "screenshot",
                "set_volume",
                "sys_stats",
                "web_fetch",
                "web_search"
            ]
        );
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
            gated: false,
            plan: None,
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
            gated: false,
            plan: None,
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
            gated: false,
            plan: None,
            run: Arc::new(|_a| panic!("intentional test panic")),
        });
        let err = r
            .dispatch_with_timeout("boom", &json!({}), Duration::from_secs(2))
            .unwrap_err();
        assert!(err.contains("crashed"), "{err}");
    }

    /* ---- get_time ---- */

    #[test]
    fn confused_schema_wrapper_args_are_unwrapped() {
        let schema = reg()
            .specs()
            .into_iter()
            .find(|s| s.name == "set_volume")
            .unwrap()
            .parameters_schema;
        /* The e2e-observed shape: properties wrapper around the real args. */
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"properties": {"volume": 40}})),
            json!({"volume": 40})
        );
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"parameters": {"volume": 5}})),
            json!({"volume": 5})
        );
        /* Clean calls pass through untouched. */
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"volume": 40})),
            json!({"volume": 40})
        );
        /* Extra keys or non-object wrapper values are not repairable. */
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"properties": {"volume": 40}, "x": 1})),
            json!({"properties": {"volume": 40}, "x": 1})
        );
        assert_eq!(
            unwrap_schema_confused_args(&schema, &json!({"properties": 3})),
            json!({"properties": 3})
        );
        /* A tool that legitimately declares the key keeps it. */
        let declared = json!({
            "type": "object",
            "properties": {"properties": {"type": "string"}},
            "required": []
        });
        assert_eq!(
            unwrap_schema_confused_args(&declared, &json!({"properties": {"a": 1}})),
            json!({"properties": {"a": 1}})
        );
        /* Schema without a properties field is left alone (conservative). */
        assert_eq!(
            unwrap_schema_confused_args(&json!({"type": "object"}), &json!({"properties": {}})),
            json!({"properties": {}})
        );
    }

    #[test]
    fn dispatch_unwraps_confused_wrapper_before_running() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "probe".to_string(),
            desc: "probe".to_string(),
            schema: json!({
                "type": "object",
                "properties": {"value": {"type": "integer"}},
                "required": ["value"]
            }),
            gated: false,
            plan: None,
            run: Arc::new(|args: &Value| -> Result<String, String> {
                Ok(format!(
                    "value={}",
                    args.get("value").and_then(|v| v.as_i64()).unwrap_or(-1)
                ))
            }),
        });
        let out = r.dispatch("probe", &json!({"properties": {"value": 7}})).unwrap();
        assert_eq!(out, "value=7");
    }

    /* ---- tools.toml config-defined tools ---- */

    #[test]
    fn tool_budget_gives_v2_tools_more_than_the_voice_default() {
        assert_eq!(tool_budget("run_command"), Duration::from_secs(120));
        assert_eq!(tool_budget("download_file"), Duration::from_secs(120));
        assert_eq!(tool_budget("web_fetch"), Duration::from_secs(25));
        assert_eq!(tool_budget("web_search"), Duration::from_secs(20));
        /* everything else keeps the 5 s voice budget */
        assert_eq!(tool_budget("get_time"), TOOL_TIMEOUT);
        assert_eq!(tool_budget("no_such_tool"), TOOL_TIMEOUT);
    }

    /* ---- gating (v2.5): the two-step confirmation flow ---- */

    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    /// A gated test tenant: the plan echoes the target, the run records
    /// execution in a flag so tests can prove nothing ran.
    fn gated_probe() -> (Registry, Arc<AtomicBool>) {
        let executed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&executed);
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "probe_gated".to_string(),
            desc: "gated test tenant".to_string(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            gated: true,
            plan: Some(Arc::new(|args: &Value| -> Result<String, String> {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .ok_or("missing path")?;
                Ok(format!("would delete {path}"))
            })),
            run: Arc::new(move |_args: &Value| -> Result<String, String> {
                flag.store(true, AtomicOrdering::Relaxed);
                Ok("deleted".to_string())
            }),
        });
        (r, executed)
    }

    fn temp_pending(tag: &str) -> crate::gating::PendingStore {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let dir = std::env::temp_dir().join(format!(
            "igris_gating_reg_{tag}_{}_{}",
            std::process::id(),
            nanos
        ));
        std::fs::create_dir_all(&dir).unwrap();
        crate::gating::PendingStore::open(dir.join("pending.json")).unwrap()
    }

    fn probe_def(r: &Registry) -> &ToolDef {
        r.tools.iter().find(|t| t.name == "probe_gated").unwrap()
    }

    #[test]
    fn gated_first_call_arms_without_executing() {
        let (r, executed) = gated_probe();
        let mut store = temp_pending("first");
        let step = r.dispatch_gated_core(
            &mut store,
            5,
            111,
            probe_def(&r),
            &json!({"path": "C:\\a.txt"}),
        );
        assert_eq!(
            step,
            Ok(GatedOutcome::Armed {
                plan: "would delete C:\\a.txt".to_string()
            })
        );
        assert!(
            !executed.load(AtomicOrdering::Relaxed),
            "the first call must never execute"
        );
        assert_eq!(store.len(), 1, "the pending is armed");
    }

    #[test]
    fn gated_same_turn_recall_re_arms_not_executes() {
        let (r, executed) = gated_probe();
        let mut store = temp_pending("sameturn");
        let def = probe_def(&r);
        let args = json!({"path": "C:\\a.txt"});
        r.dispatch_gated_core(&mut store, 5, 111, def, &args).unwrap();
        let again = r.dispatch_gated_core(&mut store, 5, 111, def, &args);
        assert_eq!(
            again,
            Ok(GatedOutcome::Armed {
                plan: "would delete C:\\a.txt".to_string()
            }),
            "same-turn re-call must re-arm"
        );
        assert!(!executed.load(AtomicOrdering::Relaxed));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn gated_cross_turn_exact_match_executes_and_consumes() {
        let (r, executed) = gated_probe();
        let mut store = temp_pending("crossturn");
        let def = probe_def(&r);
        let args = json!({"path": "C:\\a.txt"});
        r.dispatch_gated_core(&mut store, 5, 111, def, &args).unwrap();
        let step = r.dispatch_gated_core(&mut store, 6, 111, def, &args);
        assert_eq!(step, Ok(GatedOutcome::Executed(Ok("deleted".to_string()))));
        assert!(executed.load(AtomicOrdering::Relaxed));
        assert!(store.is_empty(), "the pending is spent on execution");
        /* a second call in the same turn needs a fresh confirmation */
        let again = r.dispatch_gated_core(&mut store, 6, 111, def, &args);
        assert!(
            matches!(again, Ok(GatedOutcome::Armed { .. })),
            "retry after execution needs a new arm"
        );
    }

    #[test]
    fn gated_cross_process_recall_executes() {
        let (r, executed) = gated_probe();
        let mut store = temp_pending("crossproc");
        let def = probe_def(&r);
        let args = json!({"path": "C:\\a.txt"});
        r.dispatch_gated_core(&mut store, 5, 111, def, &args).unwrap();
        let step = r.dispatch_gated_core(&mut store, 5, 222, def, &args);
        assert_eq!(step, Ok(GatedOutcome::Executed(Ok("deleted".to_string()))));
        assert!(executed.load(AtomicOrdering::Relaxed));
    }

    #[test]
    fn gated_drifted_args_re_arm_and_old_intent_never_runs() {
        let (r, executed) = gated_probe();
        let mut store = temp_pending("drift");
        let def = probe_def(&r);
        let a = json!({"path": "C:\\a.txt"});
        let b = json!({"path": "C:\\b.txt"});
        r.dispatch_gated_core(&mut store, 5, 111, def, &a).unwrap();
        let drifted = r.dispatch_gated_core(&mut store, 6, 111, def, &b);
        assert!(
            matches!(drifted, Ok(GatedOutcome::Armed { .. })),
            "drifted arguments re-arm"
        );
        /* the old intent can no longer execute: its slot was replaced */
        let old = r.dispatch_gated_core(&mut store, 6, 111, def, &a);
        assert!(
            matches!(old, Ok(GatedOutcome::Armed { .. })),
            "the older intent must never execute after a drift"
        );
        assert!(!executed.load(AtomicOrdering::Relaxed));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn gated_plan_error_arms_nothing() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "bad_plan".to_string(),
            desc: "gated test tenant".to_string(),
            schema: json!({"type": "object"}),
            gated: true,
            plan: Some(Arc::new(|_args: &Value| -> Result<String, String> {
                Err("no such file".to_string())
            })),
            run: Arc::new(|_a| Ok("never".to_string())),
        });
        let mut store = temp_pending("planerr");
        let def = r.tools.iter().find(|t| t.name == "bad_plan").unwrap();
        let step = r.dispatch_gated_core(&mut store, 5, 111, def, &json!({}));
        assert!(step.is_err());
        assert!(store.is_empty(), "a failed plan must not arm");
    }

    #[test]
    fn gated_tool_without_plan_is_an_error() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "noplans".to_string(),
            desc: "gated test tenant".to_string(),
            schema: json!({"type": "object"}),
            gated: true,
            plan: None,
            run: Arc::new(|_a| Ok("never".to_string())),
        });
        let mut store = temp_pending("noplan");
        let def = r.tools.iter().find(|t| t.name == "noplans").unwrap();
        let err = r
            .dispatch_gated_core(&mut store, 5, 111, def, &json!({}))
            .unwrap_err();
        assert!(err.contains("defines no plan"), "{err}");
    }

    #[test]
    fn gated_expired_pending_re_arms() {
        let (r, executed) = gated_probe();
        let mut store = temp_pending("expiry");
        let def = probe_def(&r);
        let args = json!({"path": "C:\\a.txt"});
        /* arm long ago by writing a pending with an old timestamp */
        store.arm("probe_gated", "{\"path\":\"C:\\\\a.txt\"}".to_string(), "old", 1, 111, 0);
        let step = r.dispatch_gated_core(&mut store, 6, 111, def, &args);
        assert!(
            matches!(step, Ok(GatedOutcome::Armed { .. })),
            "an expired pending re-arms instead of executing"
        );
        assert!(!executed.load(AtomicOrdering::Relaxed));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn specs_mark_gated_tools_in_the_description() {
        let (r, _flag) = gated_probe();
        let spec = r.specs().into_iter().find(|s| s.name == "probe_gated").unwrap();
        let desc = spec.description;
        assert!(desc.contains("gated"), "{desc}");
    }

    #[test]
    fn register_ungated_guarded_refuses_to_shadow_gated_builtins() {
        let (mut r, _flag) = gated_probe();
        r.register_ungated_guarded(ToolDef {
            name: "probe_gated".to_string(),
            desc: "config shadow".to_string(),
            schema: json!({"type": "object"}),
            gated: false,
            plan: None,
            run: Arc::new(|_a| Ok("shadow ran".to_string())),
        });
        assert!(
            r.is_gated("probe_gated"),
            "the gated tenant must survive the shadow attempt"
        );
        /* an ungated name is replaceable as before */
        r.register_ungated_guarded(ToolDef {
            name: "probe".to_string(),
            desc: "replacement".to_string(),
            schema: json!({"type": "object"}),
            gated: false,
            plan: None,
            run: Arc::new(|_a| Ok("replaced".to_string())),
        });
        assert_eq!(r.dispatch("probe", &json!({})).unwrap(), "replaced");
    }

    /* ---- list_dir ---- */

}
