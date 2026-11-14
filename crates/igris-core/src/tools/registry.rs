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
    /// The budget comes from [`tool_budget`]: 5 s for everything except
    /// the v2 dev/network tools that declare a longer one.
    pub fn dispatch(&self, name: &str, args: &Value) -> Result<String, String> {
        self.dispatch_with_timeout(name, args, tool_budget(name))
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
        let args = unwrap_schema_confused_args(&def.schema, &args.clone());
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

    /* ---- list_dir ---- */

}
