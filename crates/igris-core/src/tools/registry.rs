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
        // one API call + optional latest-release call
        "github_repo_info" => Duration::from_secs(20),
        // browser session: cold launch can hit a Defender scan of the exe
        "browser_open" => Duration::from_secs(30),
        "browser_read" => Duration::from_secs(10),
        "browser_close" => Duration::from_secs(10),
        // walk + hash budget inside find_duplicates itself
        "find_duplicates" => Duration::from_secs(120),
        _ => TOOL_TIMEOUT,
    }
}

/// Execute one resolved tool body on a worker thread with a hard budget.
/// The shared body of [`Registry::exec_def`] and the parallel batch
/// workers ([`Registry::dispatch_batch`]): a panic inside a tool drops
/// the sender on unwind and surfaces as "crashed", never as a panic.
fn exec_run(
    run: Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>,
    name: &str,
    args: &Value,
    timeout: Duration,
) -> Result<String, String> {
    let args = args.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
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
        self.dispatch_classified(name, args).0
    }

    /// [`Self::dispatch`] plus the phase classification the agent loop's
    /// retry heuristics need (v3 hardening): did the call arm, actually
    /// run, or die before any execution?
    pub fn dispatch_classified(
        &self,
        name: &str,
        args: &Value,
    ) -> (Result<String, String>, DispatchPhase) {
        let Some(def) = self.tools.iter().find(|t| t.name == name) else {
            return (Err(format!("unknown tool: {name}")), DispatchPhase::Invalid);
        };
        let args = unwrap_schema_confused_args(&def.schema, &args.clone());
        if let Err(msg) = check_arg_shapes(&def.schema, &args) {
            return (Err(msg), DispatchPhase::Invalid);
        }
        if !def.gated {
            return (
                self.exec_def(def, &args, tool_budget(name)),
                DispatchPhase::Ran,
            );
        }
        let step = {
            let mut store = match crate::gating::global().lock() {
                Ok(s) => s,
                Err(_) => {
                    return (
                        Err("gating: pending store lock poisoned".to_string()),
                        DispatchPhase::Invalid,
                    );
                }
            };
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
            Ok(GatedOutcome::Armed { plan }) => (
                Ok(crate::gating::armed_result(&plan)),
                DispatchPhase::Armed,
            ),
            Ok(GatedOutcome::Executed(result)) => (result, DispatchPhase::Ran),
            Err(e) => (Err(e), DispatchPhase::Invalid),
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

    /// Run a batch of calls in one agent round (v3): independent calls
    /// the model emitted together execute concurrently instead of
    /// one-by-one, so a chain's cost goes from the sum of the tools to
    /// roughly the slowest of them.
    ///
    /// Phase 1 (sequential) resolves each entry and makes every gating
    /// decision — gated calls validate + announce or consume a pending
    /// exactly as in [`Self::dispatch`] — while ungated runs and
    /// consumed-pending executes are collected for phase 2. Phase 2
    /// executes those concurrently, one worker thread each, with the
    /// same per-tool budget and the same panic-to-"crashed" semantics as
    /// [`Self::exec_def`]. Results return in call order, and consumed
    /// gated executes still get their activity-log lines.
    ///
    /// Gating semantics are unchanged: a same-turn re-call still only
    /// arms (freshness rule), and a batch that arms the same gated tool
    /// twice follows the one-slot-per-tool drift rule (the last arm
    /// wins; the earlier intent is dead, as the v2.5 design intends).
    pub fn dispatch_batch(&self, batch: &[(String, Value)]) -> Vec<BatchOutcome> {
        struct Deferred {
            slot: usize,
            name: String,
            run: Arc<dyn Fn(&Value) -> Result<String, String> + Send + Sync>,
            args: Value,
            timeout: Duration,
            gated: bool,
        }
        let mut outcomes: Vec<Option<Result<String, String>>> =
            (0..batch.len()).map(|_| None).collect();
        /* default Invalid covers the never-reached-a-body slots (unknown
         * name, refused plan); armed/executed slots overwrite it */
        let mut phases: Vec<DispatchPhase> = vec![DispatchPhase::Invalid; batch.len()];
        let mut deferred: Vec<Deferred> = Vec::new();
        /* phase 1: resolve + gate decisions (identical rules per call) */
        for (slot, (name, raw_args)) in batch.iter().enumerate() {
            let Some(def) = self.tools.iter().find(|t| t.name == name.as_str()) else {
                outcomes[slot] = Some(Err(format!("unknown tool: {name}")));
                continue;
            };
            let args = unwrap_schema_confused_args(&def.schema, &raw_args.clone());
            if let Err(msg) = check_arg_shapes(&def.schema, &args) {
                outcomes[slot] = Some(Err(msg));
                continue;
            }
            if !def.gated {
                phases[slot] = DispatchPhase::Ran;
                deferred.push(Deferred {
                    slot,
                    name: def.name.clone(),
                    run: Arc::clone(&def.run),
                    args,
                    timeout: tool_budget(&def.name),
                    gated: false,
                });
                continue;
            }
            let step = {
                let mut store = match crate::gating::global().lock() {
                    Ok(s) => s,
                    Err(_) => {
                        outcomes[slot] =
                            Some(Err("gating: pending store lock poisoned".to_string()));
                        continue;
                    }
                };
                self.decide_gated(
                    &mut store,
                    crate::gating::current_turn(),
                    std::process::id(),
                    def,
                    &args,
                )
            };
            match step {
                Ok(GatedDecision::Armed { plan }) => {
                    self.log_gated_step(def, &Ok(GatedOutcome::Armed { plan: plan.clone() }));
                    phases[slot] = DispatchPhase::Armed;
                    outcomes[slot] = Some(Ok(crate::gating::armed_result(&plan)));
                }
                Ok(GatedDecision::ExecuteNow) => {
                    phases[slot] = DispatchPhase::Ran;
                    deferred.push(Deferred {
                        slot,
                        name: def.name.clone(),
                        run: Arc::clone(&def.run),
                        args,
                        timeout: tool_budget(&def.name),
                        gated: true,
                    })
                }
                Err(e) => outcomes[slot] = Some(Err(e)),
            }
        }
        /* phase 2: concurrent executions with a bounded fan-out — a
         * degenerate reply with dozens of web calls must not open that
         * many sockets against one backend at once (DuckDuckGo
         * rate-limits). Chunks keep global call order: chunk i's
         * results are all collected before chunk i+1 spawns (a panic
         * drops its sender). */
        const BATCH_MAX_CONCURRENCY: usize = 4;
        while !deferred.is_empty() {
            let take = deferred.len().min(BATCH_MAX_CONCURRENCY);
            let chunk: Vec<Deferred> = deferred.drain(..take).collect();
            let (tx, rx) = std::sync::mpsc::channel();
            let spawned = chunk.len();
            for d in chunk {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let res = exec_run(d.run, &d.name, &d.args, d.timeout);
                    let _ = tx.send((d.slot, d.name, d.gated, res));
                });
            }
            drop(tx);
            let mut done = 0usize;
            while done < spawned {
                match rx.recv() {
                    Ok((slot, name, gated, res)) => {
                        done += 1;
                        if gated {
                            if let Some(def) = self.tools.iter().find(|t| t.name == name) {
                                self.log_gated_step(
                                    def,
                                    &Ok(GatedOutcome::Executed(res.clone())),
                                );
                            }
                        }
                        outcomes[slot] = Some(res);
                    }
                    Err(_) => break, /* all senders gone; missing slots crashed */
                }
            }
        }
        /* workers that panicked never sent: report crashed, keep order */
        for (slot, (name, _)) in batch.iter().enumerate() {
            if outcomes[slot].is_none() {
                outcomes[slot] = Some(Err(format!("tool '{name}' crashed")));
            }
        }
        batch
            .iter()
            .zip(outcomes)
            .zip(phases)
            .map(|(((name, _), r), phase)| BatchOutcome {
                name: name.clone(),
                result: r.unwrap_or_else(|| Err("internal: unfilled batch slot".to_string())),
                phase,
            })
            .collect()
    }

    /// Execute one resolved tenant on a worker thread with a hard budget
    /// (the shared body of [`Self::dispatch`], [`Self::dispatch_with_timeout`]
    /// and the gated execute step).
    fn exec_def(&self, def: &ToolDef, args: &Value, timeout: Duration) -> Result<String, String> {
        exec_run(Arc::clone(&def.run), &def.name, args, timeout)
    }

    /// The pure gating decision for one gated call against an explicit
    /// pending store, turn and process id — the unit-testable core; the
    /// production [`Self::dispatch`] supplies the global store and the
    /// real counters. A fresh cross-turn (or cross-process) exact match
    /// consumes the pending and reports [`GatedDecision::ExecuteNow`];
    /// everything else validates + arms and reports the plan.
    fn decide_gated(
        &self,
        store: &mut crate::gating::PendingStore,
        turn: u64,
        pid: u32,
        def: &ToolDef,
        args: &Value,
    ) -> Result<GatedDecision, String> {
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
            return Ok(GatedDecision::ExecuteNow);
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
        Ok(GatedDecision::Armed { plan: plan_text })
    }

    /// The gating decision plus inline execution for the sequential
    /// single-call path: [`Self::dispatch`]'s gated step.
    fn dispatch_gated_core(
        &self,
        store: &mut crate::gating::PendingStore,
        turn: u64,
        pid: u32,
        def: &ToolDef,
        args: &Value,
    ) -> Result<GatedOutcome, String> {
        match self.decide_gated(store, turn, pid, def, args)? {
            GatedDecision::Armed { plan } => Ok(GatedOutcome::Armed { plan }),
            GatedDecision::ExecuteNow => Ok(GatedOutcome::Executed(self.exec_def(
                def,
                args,
                tool_budget(&def.name),
            ))),
        }
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

/// The decision part of a gated dispatch, without the execution —
/// the shape [`Registry::dispatch_batch`] needs to defer consumed-pending
/// executions to the parallel phase.
#[derive(Debug, PartialEq)]
enum GatedDecision {
    Armed { plan: String },
    ExecuteNow,
}

/// One entry's result of a parallel dispatch batch, in call order.
#[derive(Debug)]
pub struct BatchOutcome {
    pub name: String,
    pub result: Result<String, String>,
    /// Which path the call took (v3 hardening): the agent loop's retry
    /// heuristics classify rounds by whether anything actually executed.
    pub phase: DispatchPhase,
}

/// Which path a dispatched call took. Loop machinery, not model
/// vocabulary — the model-facing status stays the plain "ok"/"error".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchPhase {
    /// The call validated + armed and waits for the user's confirmation.
    Armed,
    /// A tool body ran; the result (ok or a domain error) is real.
    Ran,
    /// The call never reached a tool body: unknown name, unparseable or
    /// wrong-typed arguments, refused plan. Nothing executed.
    Invalid,
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

/// A value that is the parameter's schema definition rather than an
/// argument value — the schema-echo defect seen live (2026-11-15):
/// `{"path": {"description": ..., "type": "string"}, "app": null}`. The
/// parser accepts this shape (it is valid JSON in an envelope), so it
/// reaches dispatch, and the tool's own `Missing required parameter`
/// misreports the problem — the model DID supply `path`, as the wrong
/// kind of JSON. Naming the real defect gives the model a correctable
/// diagnosis instead of a misleading one.
fn is_schema_echo(v: &Value) -> bool {
    v.as_object().is_some_and(|o| {
        o.contains_key("type")
            && (o.contains_key("description")
                || o.contains_key("properties")
                || o.contains_key("enum")
                || o.contains_key("items"))
    })
}

/// Pre-dispatch shape check over required parameters: a required
/// property that IS present must carry a plausible value, not a schema
/// echo. Absent properties stay with the tool's own validation (it
/// knows whether a default exists), and unknown extra keys are ignored
/// as before.
fn check_arg_shapes(schema: &Value, args: &Value) -> Result<(), String> {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return Ok(());
    };
    for name in required.iter().filter_map(Value::as_str) {
        if let Some(v) = args.get(name) {
            if is_schema_echo(v) {
                return Err(format!(
                    "argument '{name}' was given as its schema definition instead of a value — provide the actual value (a JSON string for '{name}')"
                ));
            }
        }
    }
    Ok(())
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
                "browser_close",
                "browser_open",
                "browser_read",
                "calculate",
                "close_app",
                "content_search",
                "copy_files",
                "create_folder",
                "create_note",
                "delete_file",
                "disk_stats",
                "download_file",
                "empty_recycle_bin",
                "file_metadata",
                "file_search",
                "find_duplicates",
                "focus_window",
                "get_time",
                "github_repo_info",
                "list_dir",
                "list_files",
                "list_windows",
                "media_key",
                "memory_recall",
                "memory_save",
                "open_app",
                "open_url",
                "power_mode",
                "process_stats",
                "read_file",
                "recycle_bin",
                "reminder_set",
                "replace_file",
                "run_command",
                "screenshot",
                "set_volume",
                "shutdown",
                "shutdown_abort",
                "sys_stats",
                "web_fetch",
                "web_search",
                "workspace_info",
                "write_file"
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

    /* ---- parallel batch dispatch (v3) ---- */

    #[test]
    fn dispatch_classified_marks_phases() {
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "cls_ok".to_string(),
            desc: "echo".to_string(),
            schema: json!({"type": "object", "properties": {}, "required": []}),
            gated: false,
            plan: None,
            run: Arc::new(|_a| Ok("ran".to_string())),
        });
        r.register(ToolDef {
            name: "cls_err".to_string(),
            desc: "domain failure".to_string(),
            schema: json!({"type": "object", "properties": {}, "required": []}),
            gated: false,
            plan: None,
            run: Arc::new(|_a| Err("not found".to_string())),
        });
        let (gated, flag) = gated_probe();
        /* unknown name never reaches a body */
        let (res, phase) = r.dispatch_classified("no_such", &json!({}));
        assert_eq!(phase, DispatchPhase::Invalid);
        assert_eq!(res.unwrap_err(), "unknown tool: no_such");
        /* success and domain failure both mean the body ran */
        let (res, phase) = r.dispatch_classified("cls_ok", &json!({}));
        assert_eq!(phase, DispatchPhase::Ran);
        assert_eq!(res.unwrap(), "ran");
        let (res, phase) = r.dispatch_classified("cls_err", &json!({}));
        assert_eq!(phase, DispatchPhase::Ran, "a domain error is a real result");
        assert_eq!(res.unwrap_err(), "not found");
        /* gated arm: validated + announced, nothing executed */
        let (res, phase) = gated.dispatch_classified(
            "probe_gated",
            &json!({"path": "cls-probe-unique"}),
        );
        assert_eq!(phase, DispatchPhase::Armed);
        assert!(res.unwrap().starts_with("NOT EXECUTED"));
        assert!(!flag.load(AtomicOrdering::Relaxed));
    }

    #[test]
    fn schema_echo_args_get_a_named_diagnosis() {
        let (r, flag) = gated_probe();
        /* the live defect shape: the parameter's schema as the value */
        let (res, phase) = r.dispatch_classified(
            "probe_gated",
            &json!({
                "path": {"description": "File to delete", "type": "string"},
                "app": null
            }),
        );
        assert_eq!(phase, DispatchPhase::Invalid);
        let err = res.unwrap_err();
        assert!(err.contains("schema definition"), "{err}");
        assert!(err.contains("'path'"), "{err}");
        /* nothing armed, nothing planned — the call died pre-execution */
        assert!(!flag.load(AtomicOrdering::Relaxed));
        /* the plain dispatch wrapper surfaces the same diagnosis */
        let err = r
            .dispatch("probe_gated", &json!({"path": {"type": "string", "description": "x"}}))
            .unwrap_err();
        assert!(err.contains("schema definition"), "{err}");
    }

    #[test]
    fn batch_reports_phase_per_call() {
        let r = echo_pair();
        let out = r.dispatch_batch(&[
            ("no_such_tool".to_string(), json!({})),
            ("batch_alpha".to_string(), json!({})),
        ]);
        assert_eq!(out[0].phase, DispatchPhase::Invalid);
        assert_eq!(out[1].phase, DispatchPhase::Ran);
    }

    /// Two plain echo tenants with distinct payloads, for order + content
    /// checks.
    fn echo_pair() -> Registry {
        let mut r = Registry::new();
        for name in ["batch_alpha".to_string(), "batch_beta".to_string()] {
            r.register(ToolDef {
                name: name.clone(),
                desc: "echo tenant".to_string(),
                schema: json!({"type": "object", "properties": {}, "required": []}),
                gated: false,
                plan: None,
                run: Arc::new(move |_a: &Value| -> Result<String, String> {
                    Ok(format!("ran {name}"))
                }),
            });
        }
        r
    }

    #[test]
    fn batch_reports_unknown_tools_and_keeps_call_order() {
        let r = echo_pair();
        let out = r.dispatch_batch(&[
            ("batch_beta".to_string(), json!({})),
            ("no_such_tool".to_string(), json!({})),
            ("batch_alpha".to_string(), json!({})),
        ]);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].name, "batch_beta");
        assert_eq!(out[0].result.as_deref(), Ok("ran batch_beta"));
        assert_eq!(out[1].name, "no_such_tool");
        assert_eq!(
            out[1].result.clone().unwrap_err(),
            "unknown tool: no_such_tool"
        );
        assert_eq!(out[2].name, "batch_alpha");
        assert_eq!(out[2].result.as_deref(), Ok("ran batch_alpha"));
    }

    #[test]
    fn batch_runs_independent_calls_concurrently() {
        let mut r = Registry::new();
        for name in ["batch_slow_a".to_string(), "batch_slow_b".to_string()] {
            r.register(ToolDef {
                name: name.clone(),
                desc: "sleeping tenant".to_string(),
                schema: json!({"type": "object", "properties": {}, "required": []}),
                gated: false,
                plan: None,
                run: Arc::new(move |_a: &Value| -> Result<String, String> {
                    std::thread::sleep(Duration::from_millis(400));
                    Ok(format!("done {name}"))
                }),
            });
        }
        let batch = vec![
            ("batch_slow_a".to_string(), json!({})),
            ("batch_slow_b".to_string(), json!({})),
        ];
        let t0 = Instant::now();
        let out = r.dispatch_batch(&batch);
        let elapsed = t0.elapsed();
        /* sequential would need ~800 ms; concurrent finishes in ~one
         * sleep. A generous margin keeps the assert load-tolerant. */
        assert!(elapsed < Duration::from_millis(700), "elapsed {elapsed:?}");
        assert_eq!(out[0].result.as_deref(), Ok("done batch_slow_a"));
        assert_eq!(out[1].result.as_deref(), Ok("done batch_slow_b"));
    }

    #[test]
    fn batch_bounds_concurrent_workers() {
        static LIVE: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        static PEAK: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        let mut r = Registry::new();
        r.register(ToolDef {
            name: "batch_capped_probe".to_string(),
            desc: "peak-tracker tenant".to_string(),
            schema: json!({"type": "object", "properties": {}, "required": []}),
            gated: false,
            plan: None,
            run: Arc::new(|_a: &Value| -> Result<String, String> {
                let live = LIVE.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                PEAK.fetch_max(live, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(60));
                LIVE.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                Ok("ok".to_string())
            }),
        });
        /* 7 calls against a cap of 4: two chunks, so the peak can never
         * exceed the cap even though every call would overlap without
         * one. */
        let batch: Vec<_> = (0..7)
            .map(|_| ("batch_capped_probe".to_string(), json!({})))
            .collect();
        let out = r.dispatch_batch(&batch);
        assert_eq!(out.len(), 7);
        assert!(out.iter().all(|o| o.result.is_ok()));
        assert!(
            PEAK.load(std::sync::atomic::Ordering::SeqCst) <= 4,
            "peak concurrent workers: {}",
            PEAK.load(std::sync::atomic::Ordering::SeqCst)
        );
    }

    #[test]
    fn batch_gated_arm_then_cross_turn_execute_via_global_store() {
        let mut r = Registry::new();
        let executed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&executed);
        r.register(ToolDef {
            name: "batch_probe_gated".to_string(),
            desc: "gated batch tenant".to_string(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
            gated: true,
            plan: Some(Arc::new(|args: &Value| -> Result<String, String> {
                Ok(format!(
                    "would remove {}",
                    args.get("path").and_then(|v| v.as_str()).unwrap_or("?")
                ))
            })),
            run: Arc::new(move |_args: &Value| -> Result<String, String> {
                flag.store(true, AtomicOrdering::Relaxed);
                Ok("removed".to_string())
            }),
        });
        /* turn T: the batch call only arms */
        crate::gating::begin_turn();
        let args = json!({"path": "batch-probe-unique-path"});
        let out = r
            .dispatch_batch(&[("batch_probe_gated".to_string(), args.clone())]);
        assert!(!executed.load(AtomicOrdering::Relaxed), "armed, not run");
        assert!(out[0]
            .result
            .as_deref()
            .unwrap()
            .starts_with("NOT EXECUTED"), "arm announcement: {:?}", out[0].result);
        /* turn T+1: the identical re-call consumes the pending, runs */
        crate::gating::begin_turn();
        let out = r
            .dispatch_batch(&[("batch_probe_gated".to_string(), args)]);
        assert!(executed.load(AtomicOrdering::Relaxed), "must execute now");
        assert_eq!(out[0].result.as_deref(), Ok("removed"));
        assert_eq!(out[0].name, "batch_probe_gated");
    }

    /* ---- list_dir ---- */

}
