//! Grammar-constrained tool calls (v3).
//!
//! Builds a GBNF grammar that admits ONLY well-formed tool-call envelopes
//! for the currently registered tools, for use as llama-server's `grammar`
//! sampling constraint. It is applied on corrective retry rounds — the
//! round that follows `AgentLoopState::malformed_verdict` flagging a
//! reply that tried to call a tool but did not parse. The constraint makes
//! malformed syntax physically unrepresentable: the sampler can only
//! choose tokens consistent with the grammar.
//!
//! This directly targets the two live defects the 2026-11-15 benchmark
//! exposed on Windows:
//! (1) the model repeatedly emitted invalid JSON backslash escaping in
//!     Windows paths, burning a 53 s flail with nothing armed — under the
//!     grammar, a string value can only contain a backslash as a valid
//!     double-backslash escape, so the path is either written correctly
//!     or not at all;
//! (2) format drift to the alias envelope and mangled template values
//!     (braced placeholders, whole-JSON-as-parameter) — under the
//!     grammar, none of those spellings can be sampled.
//!
//! Grammar shapes were live-probed against llama-server (:8081) on
//! 2026-11-15: optional groups, alternation, negated char classes with
//! hex ranges, `{m,n}` bounded repetition, and this exact string rule all
//! compile and produce well-formed output. Two probe findings are baked
//! into the design:
//! - whitespace between envelope elements is BOUNDED (`{0,3}`): an
//!   unbounded star let the model stall in a whitespace-token loop until
//!   the token budget cut the round;
//! - the tool name and its arguments rule are bound inside one
//!   alternation, because GBNF cannot correlate a dynamic name choice
//!   with a separately-chosen arguments rule.
//!
//! A second root cause was found the same day after the constrained
//! rounds still failed live: in GBNF, `"text"` is a literal whose
//! *emitted output* is `text` — the quote characters are syntax, not
//! content. The first builder produced literals like `"activity_log"`,
//! so the grammar forced the model to write the tool name BARE
//! (`activity_log , arguments : ...`), which is not JSON and which the
//! parser rightly rejects as malformed. The model was obeying the
//! constraint perfectly; the constraint described the wrong shape.
//! Every JSON string in the envelope — the `"name"` key, the tool name,
//! `"arguments"`, argument keys, and enum values — is now emitted by a
//! literal whose content CARRIES the quote characters (`"\"name\""` in
//! GBNF spelling), and the envelope follows the canonical protocol form
//! `{"name": "tool_name", "arguments": {...}}` that
//! `tool_call_protocol_instructions` teaches.
//!
//! The builder is deterministic (specs arrive sorted, so rule indices are
//! stable), and the source carries no raw tag text: tag literals come
//! from `toolparse::TOOL_CALL_OPEN`/`TOOL_CALL_CLOSE` at runtime.

use crate::llm::ToolSpec;
use serde_json::Value;

/* The strict JSON string rule: a string may contain any character except
 * a raw quote, a raw backslash, or a raw control character; a backslash
 * must introduce a valid JSON escape (one of the short escapes, a slash,
 * or `u` + four hex digits). This is the rule that makes correct
 * backslash-path JSON the only representable option. Spelled as a raw
 * string so the GBNF escape sequences pass through untouched. */
const STR_RULE: &str = r#"str ::= "\"" ( [^"\\\x00-\x1F] | "\\" ("\"" | "\\" | "/" | "b" | "f" | "n" | "r" | "t" | "u" [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F] [0-9a-fA-F]) )* "\"""#;

/// Build the tool-call grammar from the prompt-facing specs (sorted).
/// Returns `None` when there are no tools — nothing to constrain.
///
/// Shape (probed live 2026-11-15):
/// - `root`   one or more envelopes separated by bounded whitespace
/// - `env`    open tag, object, close tag
/// - `obj`    brace, the per-tool name+arguments selection, brace
/// - `a{i}`   the arguments object of tool i, from its JSON schema:
///            required properties first (in `required` order), each
///            optional property as a bounded `( , prop )?` group
pub fn tool_calls_grammar(specs: &[ToolSpec]) -> Option<String> {
    if specs.is_empty() {
        return None;
    }
    let mut g = String::with_capacity(2048 + specs.len() * 160);
    g.push_str("root ::= env ( ws env )*\n");
    g.push_str("env ::= open ws obj ws close\n");
    g.push_str(&format!(
        "open ::= {}\nclose ::= {}\n",
        lit(crate::toolparse::TOOL_CALL_OPEN),
        lit(crate::toolparse::TOOL_CALL_CLOSE),
    ));
    /* bounded whitespace: unbounded repetition let the model stall in a
     * whitespace loop until the token budget killed the round (probe 3) */
    g.push_str("ws ::= [ \\t\\r\\n]{0,3}\n");
    g.push_str(STR_RULE);
    g.push('\n');
    g.push_str("num ::= \"-\"? [0-9]+ ( \".\" [0-9]+ )?\n");
    g.push_str("int ::= \"-\"? [0-9]+\n");
    g.push_str("bl ::= \"true\" | \"false\"\n");
    g.push_str("nul ::= \"null\"\n");
    g.push_str("arr ::= \"[\" ws ( val ws ( \",\" ws val ws )* )? \"]\"\n");
    g.push_str("objg ::= \"{\" ws ( str ws \":\" ws val ws ( \",\" ws str ws \":\" ws val ws )* )? \"}\"\n");
    g.push_str("val ::= str | num | bl | nul | arr | objg\n");
    g.push_str("obj ::= \"{\" ws sel ws \"}\"\n");
    let mut sel = String::from("sel ::=");
    for (i, spec) in specs.iter().enumerate() {
        let args_rule = format!("a{i}");
        g.push_str(&format!(
            "{args_rule} ::= {}\n",
            args_rule_text(&spec.parameters_schema),
        ));
        if i > 0 {
            sel.push_str(" |");
        }
        sel.push_str(&format!(
            " {} ws \":\" ws {} ws \",\" ws {} ws \":\" ws {args_rule}",
            qlit("name"),
            qlit(&spec.name),
            qlit("arguments"),
        ));
    }
    g.push_str(&sel);
    g.push('\n');
    Some(g)
}

/// The arguments-object rule for one tool, derived from its JSON schema:
/// a brace, the mandatory properties joined by commas, each optional
/// property as a bounded optional group, a brace. Required properties
/// appear in `required` order; a schema with no properties yields the
/// empty object.
fn args_rule_text(schema: &Value) -> String {
    let empty = serde_json::Map::new();
    let props = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required_list: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    /* required entries that actually exist in properties, in the order
     * the schema declared them */
    let mut required: Vec<(&str, &Value)> = Vec::new();
    for name in &required_list {
        if let Some(v) = props.get(*name) {
            required.push((name, v));
        }
    }
    let mut optional: Vec<(&str, &Value)> = Vec::new();
    for (name, v) in props {
        if !required_list.contains(&name.as_str()) {
            optional.push((name.as_str(), v));
        }
    }
    let mut inner = String::new();
    if !required.is_empty() {
        required_chain(&required, &mut inner);
        if !optional.is_empty() {
            inner.push_str(" ( \",\" ws ");
            optional_chain(&optional, &mut inner);
            inner.push_str(" )?");
        }
    } else if !optional.is_empty() {
        optional_chain(&optional, &mut inner);
    }
    /* every populated path above ends with `ws`; the empty path leaves
     * the base `ws`, so the closing brace always follows a ws symbol */
    let mut s = String::from("\"{\" ws ");
    s.push_str(&inner);
    if inner.is_empty() {
        /* the base already ends with one space */
        s.push_str("\"}\"");
    } else {
        s.push_str(" \"}\"");
    }
    s
}

/// `name ws ":" ws value` for one property. The key is emitted as a
/// quoted JSON string (see [`qlit`]).
fn pair(name: &str, schema: &Value, out: &mut String) {
    out.push_str(&qlit(name));
    out.push_str(" ws \":\" ws ");
    out.push_str(&value_rule(schema));
}

/// Comma-joined chain of MANDATORY properties (no optionality).
fn required_chain(props: &[(&str, &Value)], out: &mut String) {
    let Some((first, rest)) = props.split_first() else {
        return;
    };
    pair(first.0, first.1, out);
    out.push_str(" ws");
    if !rest.is_empty() {
        out.push_str(" \",\" ws ");
        required_chain(rest, out);
    }
}

/// Chain of OPTIONAL properties: each after the first is nested in a
/// bounded `( , prop )?` group, so any prefix of the optional list is
/// representable (the model may omit trailing optionals freely).
fn optional_chain(props: &[(&str, &Value)], out: &mut String) {
    let Some((first, rest)) = props.split_first() else {
        return;
    };
    pair(first.0, first.1, out);
    out.push_str(" ws");
    if !rest.is_empty() {
        out.push_str(" ( \",\" ws ");
        optional_chain(rest, out);
        out.push_str(" )?");
    }
}

/// The value rule for one schema node: a shared named rule for known
/// scalar types, an inline typed array for arrays, quoted alternations
/// for enums, and the generic recursive `val` fallback for anything the
/// schema does not pin down.
fn value_rule(schema: &Value) -> String {
    /* enum wins over `type`: our schemas spell string enums as
     * {"type": "string", "enum": [...]} and the enum constraint is the
     * tighter, intended one */
    let enum_values = schema
        .get("enum")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(qlit)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !enum_values.is_empty() {
        return format!("( {} )", enum_values.join(" | "));
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => "str".to_string(),
        Some("integer") => "int".to_string(),
        Some("number") => "num".to_string(),
        Some("boolean") => "bl".to_string(),
        Some("null") => "nul".to_string(),
        Some("array") => {
            let item = schema
                .get("items")
                .map(value_rule)
                .unwrap_or_else(|| "val".to_string());
            format!("\"[\" ws ( {item} ws ( \",\" ws {item} ws )* )? \"]\"")
        }
        Some("object") => "objg".to_string(),
        _ => {
            /* no usable `type`: an object-like schema falls back to the
             * generic object rule, everything else to the generic value
             * rule */
            if schema.get("properties").is_some() {
                "objg".to_string()
            } else {
                "val".to_string()
            }
        }
    }
}

/// A double-quoted GBNF string literal for `s`, with the two characters
/// GBNF treats specially (quote and backslash) escaped, and real
/// newlines spelled as their escape so a literal never spans lines.
fn lit(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A GBNF literal whose *emitted output* is a double-quoted JSON string:
/// the quote characters are part of the output, so the grammar forces
/// the model to actually write them. In GBNF spelling this is
/// `"\"text\""` — a literal whose content is `"text"`. Used for every
/// JSON string in the envelope (the `"name"` key, tool names,
/// `"arguments"`, argument keys, enum values); bare `lit` would force
/// the text WITHOUT quotes, which is not JSON (root cause of the
/// 2026-11-15 constrained-round failures).
fn qlit(s: &str) -> String {
    lit(&format!("\"{s}\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str, schema: Value) -> ToolSpec {
        ToolSpec {
            name: name.to_string(),
            description: "test tool".to_string(),
            parameters_schema: schema,
        }
    }

    #[test]
    fn empty_specs_return_none() {
        assert!(tool_calls_grammar(&[]).is_none());
    }

    #[test]
    fn grammar_carries_core_rules_and_tag_literals() {
        let g = tool_calls_grammar(&[spec(
            "get_time",
            json!({"type": "object", "properties": {}, "required": []}),
        )])
        .unwrap();
        assert!(g.contains("root ::= env ( ws env )*"));
        assert!(g.contains("env ::= open ws obj ws close"));
        assert!(g.contains("obj ::= \"{\" ws sel ws \"}\""));
        assert!(g.contains("ws ::= [ \\t\\r\\n]{0,3}"));
        assert!(g.starts_with("root"));
        assert!(g.contains(&format!("open ::= {}", lit(crate::toolparse::TOOL_CALL_OPEN))));
        assert!(g.contains(&format!("close ::= {}", lit(crate::toolparse::TOOL_CALL_CLOSE))));
        /* the strict string rule is present and names the escape set */
        assert!(g.contains("str ::= \"\\\"\""));
        assert!(g.contains("[^\"\\\\\\x00-\\x1F]"));
        assert!(g.contains("[0-9a-fA-F]"));
        assert!(g.contains("val ::= str | num | bl | nul | arr | objg"));
    }

    #[test]
    fn zero_arg_tools_get_empty_object_rule() {
        let g = tool_calls_grammar(&[spec(
            "get_time",
            json!({"type": "object", "properties": {}, "required": []}),
        )])
        .unwrap();
        eprintln!("DEBUG-GRAMMAR:\n{g}");
        assert!(g.contains("a0 ::= \"{\" ws \"}\""));
        assert!(g.contains(&format!(
            "sel ::= {} ws \":\" ws {} ws \",\" ws {} ws \":\" ws a0",
            qlit("name"),
            qlit("get_time"),
            qlit("arguments")
        )));
    }

    #[test]
    fn sel_forces_the_canonical_name_key_form() {
        /* The parser reads a `"name"` KEY: `{"name": "tool", "arguments":
         * {...}}`. The grammar must force exactly that shape, with the
         * quote characters part of the emitted text (GBNF `"\"name\""`).
         * Root cause of the 2026-11-15 bare-name failures. */
        let g = tool_calls_grammar(&[spec(
            "delete_file",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        )])
        .unwrap();
        let sel = g.lines().find(|l| l.starts_with("sel ::= ")).unwrap();
        let expected = format!(
            "sel ::= {} ws \":\" ws {} ws \",\" ws {} ws \":\" ws a0",
            qlit("name"),
            qlit("delete_file"),
            qlit("arguments")
        );
        assert_eq!(sel, expected, "canonical envelope form: {sel}");
        /* and the name literal itself carries quote characters in its
         * content, so the model must emit them */
        assert!(sel.contains("\\\"delete_file\\\""), "{sel}");
    }

    #[test]
    fn required_props_come_first_in_required_order() {
        let g = tool_calls_grammar(&[spec(
            "reminder_set",
            json!({
                "type": "object",
                "properties": {
                    "due_at": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["content", "due_at"]
            }),
        )])
        .unwrap();
        /* content before due_at: the required array order wins over the
         * property-declaration order */
        let a0 = g.lines().find(|l| l.starts_with("a0 ::= ")).unwrap();
        let content_pos = a0.find("\\\"content\\\"").expect("content pair");
        let due_pos = a0.find("\\\"due_at\\\"").expect("due_at pair");
        assert!(content_pos < due_pos, "required order: {a0}");
        /* two mandatory props are joined by a plain comma (no optionality) */
        assert!(
            a0.contains(&format!(
                "{} ws \":\" ws str ws \",\" ws {}",
                qlit("content"),
                qlit("due_at")
            )),
            "{a0}"
        );
    }

    #[test]
    fn optional_props_become_bounded_optional_groups() {
        let g = tool_calls_grammar(&[spec(
            "tool_a",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "count": {"type": "integer"}
                },
                "required": ["path"]
            }),
        )])
        .unwrap();
        let a0 = g.lines().find(|l| l.starts_with("a0 ::= ")).unwrap();
        assert!(
            a0.contains(&format!(
                "( \",\" ws {} ws \":\" ws int ws )?",
                qlit("count")
            )),
            "{a0}"
        );
        /* and with two optionals each trailing one nests in its own group */
        let g2 = tool_calls_grammar(&[spec(
            "tool_b",
            json!({
                "type": "object",
                "properties": {
                    "req": {"type": "string"},
                    "o1": {"type": "string"},
                    "o2": {"type": "integer"}
                },
                "required": ["req"]
            }),
        )])
        .unwrap();
        let a0 = g2.lines().find(|l| l.starts_with("a0 ::= ")).unwrap();
        assert!(
            a0.contains(&format!(
                "( \",\" ws {} ws \":\" ws str ws ( \",\" ws {} ws \":\" ws int ws )? )?",
                qlit("o1"),
                qlit("o2")
            )),
            "{a0}"
        );
    }

    #[test]
    fn enums_become_quoted_alternations() {
        let g = tool_calls_grammar(&[spec(
            "media_control",
            json!({
                "type": "object",
                "properties": {"key": {"type": "string", "enum": ["play_pause", "stop"]}},
                "required": ["key"]
            }),
        )])
        .unwrap();
        assert!(g.contains(&format!(
            "( {} | {} )",
            qlit("play_pause"),
            qlit("stop")
        )));
    }

    #[test]
    fn arrays_use_their_item_type() {
        let g = tool_calls_grammar(&[spec(
            "math",
            json!({
                "type": "object",
                "properties": {"values": {"type": "array", "items": {"type": "number"}}},
                "required": ["values"]
            }),
        )])
        .unwrap();
        assert!(g.contains("\"[\" ws ( num ws ( \",\" ws num ws )* )? \"]\""));
    }

    #[test]
    fn literals_are_escaped_and_rules_are_index_stable() {
        /* a tool whose name contains a quote would otherwise corrupt the
         * grammar line; lit() guards it. Registry names never contain
         * quotes, but the builder must stay robust for config tools. */
        let g = tool_calls_grammar(&[spec(
            "we\"ird",
            json!({"type": "object", "properties": {}, "required": []}),
        )])
        .unwrap();
        assert!(g.contains("we\\\"ird"));
        /* two tools, passed in sorted order (the caller's contract):
         * args rules are a0/a1 in that order */
        let g2 = tool_calls_grammar(&[
            spec("alpha", json!({"type": "object", "properties": {}, "required": []})),
            spec("zeta", json!({"type": "object", "properties": {}, "required": []})),
        ])
        .unwrap();
        assert!(g2.contains("a0 ::= \"{\" ws \"}\""));
        assert!(g2.contains("a1 ::= \"{\" ws \"}\""));
        /* sorted specs put alpha first in the sel alternation */
        let sel = g2.lines().find(|l| l.starts_with("sel ::= ")).unwrap();
        let alpha_pos = sel.find("alpha").unwrap();
        let zeta_pos = sel.find("zeta").unwrap();
        assert!(alpha_pos < zeta_pos, "sorted sel: {sel}");
    }
}
