//! Config-defined tools (v1.1 dynamism ladder): tools.toml tenants -
//! name/desc/schema plus exactly one allowlisted URL or command
//! template, no recompile. load_config_tools() stages them; the
//! registry folds them in at seed time (overrides via same-name
//! register).

use crate::tools::registry::ToolDef;
use crate::tools::shared::url_encode_value;
use crate::tools::basic::{shell_open, validate_open_url};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, OnceLock};

/* ================================================================ */
/* tools.toml — config-defined tools (dynamism ladder rung)          */
/* ================================================================ */

#[derive(Deserialize)]
struct ToolsFile {
    #[serde(default)]
    tool: Vec<ConfigToolEntry>,
}

#[derive(Deserialize)]
struct ConfigToolEntry {
    name: String,
    description: String,
    /* JSON text of the tool's parameter schema; omitted = no params. */
    schema: Option<String>,
    url: Option<String>,
    command: Option<Vec<String>>,
}

/// `{name}` placeholders — lowercase slug identifiers, so they can only
/// ever reference schema properties (which are declared with the same
/// slug rule), never arbitrary expressions.
static PLACEHOLDER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\{([a-z][a-z0-9_]*)\}").expect("static regex"));

/// Config-defined tools staged by `load_config_tools()` before the first
/// `registry()` use; the registry folds them in at seed time. Tests never
/// call the loader, so the test registry stays hermetic.
pub(crate) static CONFIG_TOOLS: OnceLock<Vec<ToolDef>> = OnceLock::new();

/// Config file location: `%APPDATA%\igris\tools.toml`.
pub fn config_tools_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("igris").join("tools.toml"))
}

/// Read + validate the user's `tools.toml` and stage the tools for
/// [`registry`]. Call once at CLI startup, before any registry use —
/// later calls cannot affect an already-built registry. A missing file
/// is normal (0 tools); an invalid file is reported on stderr and
/// ignored: bad config must never brick the assistant.
pub fn load_config_tools() -> usize {
    let Some(path) = config_tools_path() else {
        return 0;
    };
    let Ok(text) = fs::read_to_string(&path) else {
        return 0;
    };
    match parse_config_tools(&text) {
        Ok(defs) => {
            let n = defs.len();
            let _ = CONFIG_TOOLS.set(defs);
            n
        }
        Err(e) => {
            eprintln!("tools: ignoring {}: {e}", path.display());
            0
        }
    }
}

/// Parse + validate a `tools.toml` text into registrable tenants. Pure
/// function so tests can exercise it without touching the filesystem.
fn parse_config_tools(text: &str) -> Result<Vec<ToolDef>, String> {
    let file: ToolsFile = toml::from_str(text).map_err(|e| format!("invalid TOML: {e}"))?;
    let mut defs = Vec::new();
    for (i, entry) in file.tool.iter().enumerate() {
        let label = format!("tool #{}", i + 1);
        let name = entry.name.trim();
        if name.is_empty() {
            return Err(format!("{label}: 'name' is empty"));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "{label}: name '{name}' must be a slug: lowercase letters, digits, underscores"
            ));
        }
        if entry.description.trim().is_empty() {
            return Err(format!("{label} ({name}): 'description' is empty"));
        }
        if (entry.url.is_some()) == (entry.command.is_some()) {
            return Err(format!("{label} ({name}): define exactly one of 'url' or 'command'"));
        }
        let schema_text = entry
            .schema
            .as_deref()
            .unwrap_or("{\"type\":\"object\",\"properties\":{},\"required\":[]}");
        let schema: Value = serde_json::from_str(schema_text)
            .map_err(|e| format!("{label} ({name}): 'schema' is not valid JSON: {e}"))?;
        if !schema.is_object() {
            return Err(format!("{label} ({name}): 'schema' must be a JSON object"));
        }
        let props = schema.get("properties").cloned().unwrap_or_else(|| json!({}));
        if !props.is_object() {
            return Err(format!("{label} ({name}): 'schema.properties' must be an object"));
        }
        if let Some(url) = &entry.url {
            validate_url_template(url, &props).map_err(|e| format!("{label} ({name}): {e}"))?;
            let url = url.clone();
            defs.push(ToolDef {
                name: name.to_string(),
                desc: entry.description.trim().to_string(),
                schema,
                gated: false,
                plan: None,
                run: Arc::new(move |args: &Value| run_config_url(&url, args)),
            });
        } else if let Some(cmd) = &entry.command {
            validate_command_template(cmd, &props).map_err(|e| format!("{label} ({name}): {e}"))?;
            let cmd = cmd.clone();
            defs.push(ToolDef {
                name: name.to_string(),
                desc: entry.description.trim().to_string(),
                schema,
                gated: false,
                plan: None,
                run: Arc::new(move |args: &Value| run_config_command(&cmd, args)),
            });
        }
    }
    Ok(defs)
}

fn validate_url_template(template: &str, props: &Value) -> Result<(), String> {
    let t = template.trim();
    if !(t.starts_with("http://") || t.starts_with("https://")) {
        return Err("url template must start with http:// or https://".to_string());
    }
    for ph in placeholders_in(t) {
        if props.get(ph.as_str()).is_none() {
            return Err(format!(
                "url placeholder '{ph}' is not declared in the schema properties"
            ));
        }
    }
    Ok(())
}

fn validate_command_template(cmd: &[String], props: &Value) -> Result<(), String> {
    if cmd.is_empty() {
        return Err("command template must have at least a program".to_string());
    }
    if cmd[0].trim().is_empty() {
        return Err("command program is empty".to_string());
    }
    for part in cmd {
        for ph in placeholders_in(part) {
            if props.get(ph.as_str()).is_none() {
                return Err(format!(
                    "command placeholder '{ph}' is not declared in the schema properties"
                ));
            }
        }
    }
    Ok(())
}

fn placeholders_in(template: &str) -> Vec<String> {
    PLACEHOLDER_RE
        .find_iter(template)
        .map(|m| m.as_str()[1..m.len() - 1].to_string())
        .collect()
}

/// Argument value as a template string: strings verbatim, numbers and
/// bools in their JSON form. An argument the template needs but the
/// model didn't supply is an honest error — better than a mangled URL
/// or a half-formed argv.
pub(crate) fn arg_string(args: &Value, key: &str) -> Result<String, String> {
    match args.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(Value::Bool(b)) => Ok(b.to_string()),
        _ => Err(format!("Missing required parameter: {key}")),
    }
}


/// Substitute `{placeholder}`s from the call arguments. `encode` picks
/// URL-percent-encoding (templates) vs verbatim text (argv entries).
fn expand_template(template: &str, args: &Value, encode: bool) -> Result<String, String> {
    let mut out = String::new();
    let mut last = 0usize;
    for m in PLACEHOLDER_RE.find_iter(template) {
        out.push_str(&template[last..m.start()]);
        let key = &template[m.start() + 1..m.end() - 1];
        let raw = arg_string(args, key)?;
        let piece = if encode {
            url_encode_value(&raw)
        } else {
            raw
        };
        out.push_str(&piece);
        last = m.end();
    }
    out.push_str(&template[last..]);
    Ok(out)
}

pub(crate) fn run_config_url(template: &str, args: &Value) -> Result<String, String> {
    let url = expand_template(template, args, true)?;
    validate_open_url(&url)?;
    /* Optional standard browser arg (U1 follow-up): any config URL tool
     * whose schema declares `browser` gains browser targeting. The URL
     * template never references it, so expansion ignores it; only the
     * allowlisted names in basic::BROWSER_CANDIDATES are accepted. */
    match args.get("browser") {
        None | Some(Value::Null) => {
            shell_open(&url)?;
            Ok(format!("Opened {url}"))
        }
        Some(Value::String(name)) => {
            let msg = crate::tools::basic::open_in_named_browser(name, &url)?;
            Ok(format!("{msg}"))
        }
        Some(_) => Err("browser must be a string: chrome, brave, firefox or edge".into()),
    }
}

pub(crate) fn run_config_command(template: &[String], args: &Value) -> Result<String, String> {
    let mut argv = Vec::with_capacity(template.len());
    for part in template {
        argv.push(expand_template(part, args, false)?);
    }
    /* Direct argv spawn, fire-and-forget (same policy as open_app):
     * values are separate arguments, never shell syntax. */
    std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .spawn()
        .map_err(|e| format!("Could not run '{}': {e}", argv[0]))?;
    Ok(format!("Ran {}.", argv[0]))
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::seed_registry;
    use serde_json::json;

    const PLAY_SONG_TOML: &str = r#"
[[tool]]
name = "play_song"
description = "Search YouTube for a song and open the results page"
schema = '{"type":"object","properties":{"query":{"type":"string","description":"Song or artist to search"}},"required":["query"]}'
url = "https://www.youtube.com/results?search_query={query}"
"#;

    const NOTE_CMD_TOML: &str = r#"
[[tool]]
name = "show_note"
description = "Show a note in notepad"
schema = '{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}'
command = ["notepad.exe", "{text}"]
"#;

    #[test]
    fn config_parse_accepts_url_and_command_templates() {
        let defs = parse_config_tools(PLAY_SONG_TOML).expect("url tool parses");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "play_song");
        assert!(defs[0].schema["properties"]["query"].is_object());
        let defs = parse_config_tools(NOTE_CMD_TOML).expect("command tool parses");
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "show_note");
    }

    #[test]
    fn config_parse_empty_and_comment_only_files_are_zero_tools() {
        assert_eq!(parse_config_tools("").unwrap().len(), 0);
        assert_eq!(parse_config_tools("# just a comment\n").unwrap().len(), 0);
    }

    #[test]
    fn config_parse_rejects_bad_names_and_missing_descriptions() {
        let bad_name = PLAY_SONG_TOML.replace("play_song", "Play-Song");
        assert!(parse_config_tools(&bad_name).unwrap_err().contains("slug"));
        let empty_name = PLAY_SONG_TOML.replace("play_song", "");
        assert!(parse_config_tools(&empty_name).unwrap_err().contains("'name' is empty"));
        let no_desc = PLAY_SONG_TOML.replace(
            "description = \"Search YouTube for a song and open the results page\"",
            "description = \"\"",
        );
        assert!(parse_config_tools(&no_desc).unwrap_err().contains("'description' is empty"));
    }

    #[test]
    fn config_parse_requires_exactly_one_of_url_or_command() {
        let both = format!("{PLAY_SONG_TOML}command = [\"notepad.exe\"]\n");
        assert!(parse_config_tools(&both).unwrap_err().contains("exactly one"));
        let neither = PLAY_SONG_TOML.replace(
            "url = \"https://www.youtube.com/results?search_query={query}\"\n",
            "",
        );
        assert!(parse_config_tools(&neither).unwrap_err().contains("exactly one"));
    }

    #[test]
    fn config_parse_rejects_unknown_placeholders_and_bad_schemes() {
        let undeclared = PLAY_SONG_TOML.replace("{query}", "{track}");
        let err = parse_config_tools(&undeclared).unwrap_err();
        assert!(err.contains("'track' is not declared"), "{err}");
        let bad_scheme = PLAY_SONG_TOML.replace(
            "https://www.youtube.com/results?search_query={query}",
            "file://C:/Windows/{query}",
        );
        assert!(parse_config_tools(&bad_scheme).unwrap_err().contains("http"));
        let undeclared_cmd = NOTE_CMD_TOML.replace("{text}", "{note}");
        assert!(parse_config_tools(&undeclared_cmd)
            .unwrap_err()
            .contains("'note' is not declared"));
    }

    #[test]
    fn config_parse_rejects_invalid_schemas() {
        let bad_json = PLAY_SONG_TOML.replace(
            "'{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\",\"description\":\"Song or artist to search\"}},\"required\":[\"query\"]}'",
            "'{\"type\": \"object\",'",
        );
        assert!(parse_config_tools(&bad_json).unwrap_err().contains("valid JSON"));
        let non_object = PLAY_SONG_TOML.replace(
            "'{\"type\":\"object\",\"properties\":{\"query\":{\"type\":\"string\",\"description\":\"Song or artist to search\"}},\"required\":[\"query\"]}'",
            "'[1, 2]'",
        );
        assert!(parse_config_tools(&non_object)
            .unwrap_err()
            .contains("JSON object"));
    }

    #[test]
    fn config_url_expansion_encodes_values_but_keeps_template() {
        let args = json!({"query": "shape of you"});
        let url = expand_template(
            "https://www.youtube.com/results?search_query={query}",
            &args,
            true,
        )
        .unwrap();
        assert_eq!(
            url,
            "https://www.youtube.com/results?search_query=shape%20of%20you"
        );
        /* '&' inside a VALUE must not split the query string. */
        let args = json!({"query": "dance & move"});
        let url = expand_template(
            "https://x.example/search?q={query}&safe=true",
            &args,
            true,
        )
        .unwrap();
        assert_eq!(url, "https://x.example/search?q=dance%20%26%20move&safe=true");
        /* A missing argument is an honest error, not a mangled URL. */
        assert!(expand_template(
            "https://www.youtube.com/results?search_query={query}",
            &json!({}),
            true
        )
        .unwrap_err()
        .contains("query"));
    }

    #[test]
    fn config_command_expansion_is_verbatim_argv() {
        let args = json!({"text": "a b \"quoted\""});
        let expanded = expand_template("{text}", &args, false).unwrap();
        assert_eq!(expanded, "a b \"quoted\"");
    }

    #[test]
    fn placeholders_ignore_non_slug_braces() {
        assert_eq!(
            placeholders_in("keep {Bad} {good_2} {x and {unclosed}"),
            vec!["good_2".to_string(), "unclosed".to_string()]
        );
    }

    #[test]
    fn config_tools_override_builtins_and_extend_the_registry() {
        let mut r = seed_registry();
        for def in parse_config_tools(PLAY_SONG_TOML).unwrap() {
            r.register(def);
        }
        let names: Vec<String> = r.specs().into_iter().map(|s| s.name).collect();
        assert!(names.contains(&"play_song".to_string()));
        assert_eq!(names.len(), 48); /* 47 built-ins + 1 config tenant */
        /* register() replaces same-name tenants: an override of get_time. */
        let defs = parse_config_tools(
            r#"
[[tool]]
name = "get_time"
description = "Config override of get_time"
url = "https://time.example/{q}"
schema = '{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}'
"#,
        )
        .unwrap();
        r.register(defs.into_iter().next().unwrap());
        let get_time = r.specs().into_iter().find(|s| s.name == "get_time").unwrap();
        assert!(get_time.description.contains("Config override"));
    }

    /* ---- v2: per-tool budgets ---- */

}
