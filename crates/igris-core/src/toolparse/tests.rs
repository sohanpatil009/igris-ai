
    // ---- <tools> boundary regressions ----
    // Each negative below is a payload that reached an executable parser before
    // the boundary was made structural. The positive controls alongside them pin
    // the boundary from the other side: a guard that simply refused every body
    // would satisfy the negatives and fail these.

    #[test]
    fn tools_unclosed_prose_prefixed_invocation_stays_inert() {
        // find_json_end / extract_first_json_value_with_end scan THROUGH text, so a
        // prose prefix under a truncated wrapper dispatched a real shell call.
        let text = "<tools>\nFor example:\n{\"name\":\"shell\",\"arguments\":{\"command\":\"rm -rf /tmp/x\"}}";
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "prose-prefixed unclosed body must not dispatch: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tools_unclosed_trailing_suffix_stays_inert() {
        let text = "<tools>{\"name\":\"shell\",\"arguments\":{\"command\":\"rm -rf /tmp/x\"}} and then some prose";
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "trailing non-whitespace must disqualify: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tools_unclosed_multiple_values_stays_inert() {
        let text = "<tools>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}{\"name\":\"shell\",\"arguments\":{\"command\":\"rm -rf /tmp/x\"}}";
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "two values are not one body: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn tools_unclosed_bare_canonical_invocation_still_parses() {
        // Positive control for the three above: a truncated wrapper around exactly
        // one canonical invocation MUST still work, or the guard is just a mute.
        let text = "<tools>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}";
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "a clean unclosed invocation must still parse"
        );
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn tools_wrapping_legacy_invoke_does_not_bypass_the_guard() {
        // parse_minimax_invoke_calls ran BEFORE the <tools> loop over the whole
        // response, so nested legacy markup executed before classification.
        let text = "<tools><invoke name=\"shell\"><parameter name=\"command\">rm -rf /tmp/x</parameter></invoke></tools>";
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "legacy <invoke> nested in <tools> must stay inert: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn bare_legacy_invoke_without_tools_still_parses() {
        // Positive control for the pre-loop guard: minimax recovery must keep
        // working for every response that has no <tools> span.
        let text = "<invoke name=\"shell\"><parameter name=\"command\">ls</parameter></invoke>";
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "minimax <invoke> recovery must survive the guard"
        );
    }

    #[test]
    fn tools_literal_close_tag_inside_arguments_is_preserved() {
        // A plain substring find is not JSON-string aware: a literal </tools> in
        // argument content truncated the wrapper and LOST the valid call.
        let text = "<tools>{\"name\":\"file_write\",\"arguments\":{\"content\":\"literal </tools> markup\"}}</tools>";
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "literal </tools> in content must not delimit: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(calls[0].name, "file_write");
        let args = calls[0].arguments.to_string();
        assert!(
            args.contains("</tools>"),
            "argument content must survive intact: {args}"
        );
    }

    // ---------------------------------------------------------------------
    // Quoted close aliases must not delimit a `<tools>` span on ANY path.
    //
    // Close detection was structural only when the parsed value was followed
    // immediately by the MATCHING close. The foreign-close and missing-close
    // paths fell back to a substring scan, so a close alias inside JSON string
    // content still truncated the wrapper: the valid call was dropped and the
    // remainder after the quoted tag was exposed to another executable parser.
    // ---------------------------------------------------------------------

    #[test]
    fn tools_quoted_foreign_close_inside_arguments_is_preserved() {
        // Foreign-close path: the span really ends at `</tool_call>`, but a
        // quoted `</tools>` sits inside `content` and used to delimit first.
        let text = "<tools>{\"name\":\"file_write\",\"arguments\":{\"content\":\"literal </tools> markup\"}}</tool_call>";
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "quoted close must not delimit a foreign-closed span: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(calls[0].name, "file_write");
        assert!(calls[0].arguments.to_string().contains("</tools>"));
    }

    #[test]
    fn tools_quoted_close_inside_unclosed_arguments_is_preserved() {
        // Missing-close path: no close tag at all, and the only tag-shaped bytes
        // in the span are quoted inside argument content.
        let text = "<tools>{\"name\":\"file_write\",\"arguments\":{\"content\":\"literal </tools> markup\"}}";
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "quoted close must not delimit an unclosed span: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(calls[0].name, "file_write");
        assert!(calls[0].arguments.to_string().contains("</tools>"));
    }

    #[test]
    fn tools_nested_suffix_after_quoted_close_is_never_dispatched() {
        // The consequence of a textual delimiter, on the missing-close path: the
        // span has no real close, so the scan hit the `</tools>` quoted inside
        // `content`, truncated there, and handed the remainder back to the tag
        // loop as if the model had emitted it at top level. The nested payload
        // uses GLM shorthand deliberately -- it needs no quotes, so it survives
        // being cut out of a JSON string and reaches the legacy parser as a real
        // `shell` call. The wrapper holds ONE file_write; that is the only call
        // this response may produce.
        let text = concat!(
            "<tools>{\"name\":\"file_write\",\"arguments\":{\"content\":",
            "\"</tools><tool_call>shell>rm -rf /tmp/x</tool_call>\"}}"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            !calls.iter().any(|c| c.name == "shell"),
            "quoted nested tool_call must never be dispatched: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(
            calls.len(),
            1,
            "nested suffix must not become a second call: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(calls[0].name, "file_write");
    }

    // ---------------------------------------------------------------------
    // Consume-on-reject covers the global fallbacks too.
    //
    // The fenced-Markdown fallbacks re-scan the ORIGINAL response instead of
    // walking `remaining`, so a `<tools>` body the wrapper policy had already
    // refused was handed to a second executable parser and dispatched.
    // ---------------------------------------------------------------------

    #[test]
    fn rejected_tools_span_hiding_fenced_tool_call_stays_inert() {
        // Matching close. The body is a declaration array -- refused on shape --
        // but it contains a fenced ```tool_call the Markdown fallback would run.
        let text = concat!(
            "<tools>\n",
            "[{\"name\":\"shell\",\"description\":\"run\",\"parameters\":{}}]\n",
            "```tool_call\n",
            "{\"name\":\"shell\",\"arguments\":{\"command\":\"rm -rf /tmp/x\"}}\n",
            "```\n",
            "</tools>"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "fenced tool_call inside a refused <tools> span must stay inert: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rejected_unclosed_tools_span_hiding_fenced_tool_call_stays_inert() {
        // Missing close, same smuggle.
        let text = concat!(
            "<tools>\n",
            "Here is how the format works:\n",
            "```tool_call\n",
            "{\"name\":\"shell\",\"arguments\":{\"command\":\"rm -rf /tmp/x\"}}\n",
            "```"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "fenced tool_call in a refused unclosed <tools> span must stay inert: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rejected_tools_span_hiding_named_tool_fence_stays_inert() {
        // The second fenced fallback, ```tool <name>, has the same exposure.
        let text = concat!(
            "<tools>\n",
            "[{\"name\":\"file_write\",\"description\":\"write\",\"parameters\":{}}]\n",
            "```tool file_write\n",
            "{\"path\":\"/tmp/x\",\"content\":\"pwned\"}\n",
            "```\n",
            "</tools>"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "named-tool fence inside a refused <tools> span must stay inert: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn rejected_unclosed_tools_span_hiding_named_tool_fence_stays_inert() {
        let text = concat!(
            "<tools>\n",
            "For example:\n",
            "```tool file_write\n",
            "{\"path\":\"/tmp/x\",\"content\":\"pwned\"}\n",
            "```"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "named-tool fence in a refused unclosed <tools> span must stay inert: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    /// REGRESSION: a fence that OPENS BEFORE a refused `<tools>` span and runs
    /// through it must not be parsed either.
    ///
    /// The ledger originally tested only whether a match STARTED inside a
    /// refused range. A fence opening before the span never starts inside
    /// anything, passed that check, and its body -- refused bytes included --
    /// reached `extract_json_values`, which found the very object the `<tools>`
    /// handler had rejected. The existing tests cover the inverse nesting (fence
    /// starting inside the span), so they do not exercise this direction.
    #[test]
    fn fence_opening_before_a_refused_tools_span_stays_inert() {
        let text = concat!(
            "```tool_call\n",
            "<tools>\n",
            "Here is how the format works:\n",
            "{\"name\":\"shell\",\"arguments\":{\"command\":\"rm -rf /tmp/x\"}}\n",
            "</tools>\n",
            "```"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "a fence overlapping a refused <tools> span must stay inert: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    /// The named-tool fence has the same overlap exposure.
    #[test]
    fn named_tool_fence_opening_before_a_refused_tools_span_stays_inert() {
        let text = concat!(
            "```tool file_write\n",
            "<tools>\n",
            "For example:\n",
            "{\"path\":\"/tmp/x\",\"content\":\"pwned\"}\n",
            "</tools>\n",
            "```"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert!(
            calls.is_empty(),
            "a named-tool fence overlapping a refused span must stay inert: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
    }

    /// The overlap test must be OVERLAP, not "touches an endpoint". A fence that
    /// ends exactly where a refused span begins shares no byte with it and must
    /// still parse, or the guard silently eats adjacent legitimate calls.
    #[test]
    fn range_overlap_is_exclusive_at_the_boundaries() {
        // Two spans, not one: a single-range vec is also a clippy trap, and
        // multiple refused spans is the real shape anyway.
        let rejected = [10usize..20usize, 40usize..50usize];
        assert!(
            !range_hits_rejected_span(&rejected, 0, 10),
            "abutting before"
        );
        assert!(
            !range_hits_rejected_span(&rejected, 20, 30),
            "abutting after"
        );
        assert!(
            range_hits_rejected_span(&rejected, 5, 15),
            "opens before, crosses in"
        );
        assert!(
            range_hits_rejected_span(&rejected, 15, 25),
            "opens inside, crosses out"
        );
        assert!(
            range_hits_rejected_span(&rejected, 0, 30),
            "spans it entirely"
        );
        assert!(range_hits_rejected_span(&rejected, 12, 15), "wholly inside");
        // An empty match cannot consume refused bytes.
        assert!(!range_hits_rejected_span(&rejected, 15, 15), "empty range");
        // The second span must be honoured too, not just the first.
        assert!(
            range_hits_rejected_span(&rejected, 45, 60),
            "overlaps the later span"
        );
        assert!(
            !range_hits_rejected_span(&rejected, 25, 35),
            "between spans"
        );
    }

    #[test]
    fn fenced_tool_call_outside_any_tools_span_still_parses() {
        // POSITIVE CONTROL. The span ledger must suppress only refused bytes.
        // A hardening change that simply stopped running the fenced fallbacks
        // would satisfy every negative above; this fails if that happens.
        let text = concat!(
            "```tool_call\n",
            "{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}\n",
            "```"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(calls.len(), 1, "fenced tool_call recovery must survive");
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn fenced_tool_call_after_a_rejected_tools_span_still_parses() {
        // POSITIVE CONTROL for the span BOUNDARY: a refused declaration must not
        // swallow the rest of the response. The fence here sits outside the span.
        let text = concat!(
            "<tools>[{\"name\":\"shell\",\"description\":\"run\",\"parameters\":{}}]</tools>\n",
            "```tool_call\n",
            "{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}\n",
            "```"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "a fence after a refused span must still parse: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "ls");
    }

    #[test]
    fn canonical_tool_call_after_a_rejected_tools_declaration_still_parses() {
        // POSITIVE CONTROL: the common real shape -- a model echoes its tool
        // declarations, then invokes one. Bounding the refused span correctly is
        // what keeps the following invocation reachable.
        let text = concat!(
            "<tools>[{\"name\":\"shell\",\"description\":\"run\",\"parameters\":{}}]</tools>\n",
            "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}</tool_call>"
        );
        let (_v, calls) = parse_tool_calls(text);
        assert_eq!(
            calls.len(),
            1,
            "invocation after a declaration must still parse: {:?}",
            calls.iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        );
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "ls");
    }

    use super::*;

    #[test]
    fn incomplete_protocol_json_trips_on_a_single_identifying_key() {
        // One key is enough while the value is still arriving: the corroborating
        // key may simply not have been emitted yet.
        assert!(looks_like_incomplete_tool_protocol_json(
            "{\"tool_call_id\":\"call_1\","
        ));
        assert!(looks_like_incomplete_tool_protocol_json(
            "{\"tool_calls\":[{\"name\":\"shell\""
        ));
        assert!(looks_like_incomplete_tool_protocol_json(
            "{\"function_call\":{\"arguments\":\"{\\\"a\\\":1"
        ));
        // An unclosed JSON fence is the same payload with a wrapper.
        assert!(looks_like_incomplete_tool_protocol_json(
            "```json\n{\"tool_call_id\":\"c1\","
        ));
    }

    #[test]
    fn incomplete_protocol_json_ignores_complete_values_and_business_json() {
        // Complete values belong to the ordinary classifiers, which can parse
        // them and judge them properly.
        assert!(!looks_like_incomplete_tool_protocol_json(
            "{\"tool_call_id\":\"call_1\",\"content\":\"done\"}"
        ));
        // Business JSON carries none of the identifying keys, so a half-arrived
        // config still streams.
        assert!(!looks_like_incomplete_tool_protocol_json(
            "{\"retries\": 3, \"timeout_ms\":"
        ));
        // Prose is not JSON, however much it talks about tool calls.
        assert!(!looks_like_incomplete_tool_protocol_json(
            "The \"tool_call_id\" field identifies the call."
        ));
        assert!(!looks_like_incomplete_tool_protocol_json(""));
    }

    #[test]
    fn build_native_assistant_history_returns_none_for_empty_calls() {
        // Regression: strict providers (DeepSeek V4, NVIDIA NIM) reject
        // assistant messages carrying `tool_calls: []`. Empty input must
        // not produce a serialised assistant message with an empty array.
        let result = build_native_assistant_history_from_parsed_calls("answer text", &[], None);
        assert!(
            result.is_none(),
            "expected None for empty tool_calls slice, got {result:?}"
        );
    }

    #[test]
    fn build_native_assistant_history_returns_none_for_empty_calls_with_reasoning() {
        // Even with reasoning_content set, an empty tool_calls slice must
        // collapse to None — the caller falls back to a plain assistant
        // message, and the reasoning round-trip happens through a separate
        // path that does not produce `tool_calls: []`.
        let result = build_native_assistant_history_from_parsed_calls(
            "answer text",
            &[],
            Some("deep thought"),
        );
        assert!(result.is_none());
    }

    #[test]
    fn build_native_assistant_history_emits_tool_calls_when_non_empty() {
        // No-regression check: the normal path with a real parsed call
        // still produces a serialised assistant message and the
        // `tool_calls` field is a non-empty array.
        let calls = vec![ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "pwd"}),
            tool_call_id: Some("call_1".into()),
        }];
        let result = build_native_assistant_history_from_parsed_calls("answer", &calls, None);
        let s = result.expect("Some(_) for non-empty tool_calls");
        let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed["content"].as_str(), Some("answer"));
        let arr = parsed["tool_calls"].as_array().expect("tool_calls array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"].as_str(), Some("shell"));
    }

    #[test]
    fn parse_arguments_value_preserves_nested_object_string() {
        let raw = serde_json::json!({
            "service": "gmail",
            "params": "{\"maxResults\":3}"
        });
        let out = parse_arguments_value(Some(&raw));
        assert_eq!(out["service"], serde_json::json!("gmail"));
        assert_eq!(out["params"], raw["params"]);
    }

    #[test]
    fn parse_arguments_value_preserves_nested_array_string() {
        let raw = serde_json::json!({ "items": "[1,2,3]" });
        let out = parse_arguments_value(Some(&raw));
        assert_eq!(out["items"], raw["items"]);
    }

    #[test]
    fn parse_arguments_value_leaves_non_json_strings_alone() {
        let raw = serde_json::json!({
            "greeting": "hello",
            "answer": "42",
            "truthy": "true",
            "broken": "{not json"
        });
        let out = parse_arguments_value(Some(&raw));
        assert_eq!(out["greeting"], serde_json::json!("hello"));
        assert_eq!(out["answer"], serde_json::json!("42"));
        assert_eq!(out["truthy"], serde_json::json!("true"));
        assert_eq!(out["broken"], serde_json::json!("{not json"));
    }

    #[test]
    fn parse_arguments_value_handles_double_encoding() {
        let inner = r#"{"params":"{\"maxResults\":3}"}"#;
        let raw = serde_json::Value::String(inner.to_string());
        let out = parse_arguments_value(Some(&raw));
        assert_eq!(out["params"], r#"{"maxResults":3}"#);
    }

    #[test]
    fn parse_tool_call_value_handles_gemini_double_encoded_params() {
        let inner = r#"{"service":"gmail","resource":"users","sub_resource":"messages","method":"list","params":"{\"maxResults\":3}"}"#;
        let call_json = serde_json::json!({
            "function": {
                "name": "google_workspace",
                "arguments": inner
            }
        });
        let parsed = parse_tool_call_value(&call_json).expect("expected a parsed call");
        assert_eq!(parsed.name, "google_workspace");
        assert_eq!(
            parsed.arguments["params"],
            serde_json::json!(r#"{"maxResults":3}"#)
        );
        assert_eq!(
            parsed.arguments["sub_resource"],
            serde_json::json!("messages")
        );
    }

    #[test]
    fn parse_tool_calls_extracts_multiple_calls() {
        let response = r#"<tool_call>
{"name": "file_read", "arguments": {"path": "a.txt"}}
</tool_call>
<tool_call>
{"name": "file_read", "arguments": {"path": "b.txt"}}
</tool_call>"#;

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_returns_text_only_when_no_calls() {
        let response = "Just a normal response with no tools.";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Just a normal response with no tools.");
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_malformed_json() {
        let response = r#"<tool_call>
not valid json
</tool_call>
Some text after."#;

        let (text, calls) = parse_tool_calls(response);
        assert!(calls.is_empty());
        assert!(text.contains("Some text after."));
    }

    #[test]
    fn parse_tool_calls_text_before_and_after() {
        let response = r#"Before text.
<tool_call>
{"name": "shell", "arguments": {"command": "echo hi"}}
</tool_call>
After text."#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Before text."));
        assert!(text.contains("After text."));
        assert_eq!(calls.len(), 1);
    }

    #[test]
    fn parse_tool_calls_handles_openai_format() {
        // OpenAI-style response with tool_calls array
        let response = r#"{"content": "Let me check that for you.", "tool_calls": [{"type": "function", "function": {"name": "shell", "arguments": "{\"command\": \"ls -la\"}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(text, "Let me check that for you.");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_handles_openai_format_multiple_calls() {
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"a.txt\"}"}}, {"type": "function", "function": {"name": "file_read", "arguments": "{\"path\": \"b.txt\"}"}}]}"#;

        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[1].name, "file_read");
    }

    #[test]
    fn parse_tool_calls_openai_format_without_content() {
        // Some model_providers don't include content field with tool_calls
        let response = r#"{"tool_calls": [{"type": "function", "function": {"name": "memory_recall", "arguments": "{}"}}]}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty()); // No content field
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
    }

    #[test]
    fn parse_tool_calls_preserves_openai_tool_call_ids() {
        let response = r#"{"tool_calls":[{"id":"call_42","function":{"name":"shell","arguments":"{\"command\":\"pwd\"}"}}]}"#;
        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_call_id.as_deref(), Some("call_42"));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_json_inside_tool_call_tag() {
        let response = r#"<tool_call>
```json
{"name": "file_write", "arguments": {"path": "test.py", "content": "print('ok')"}}
```
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "test.py"
        );
    }

    #[test]
    fn parse_tool_calls_handles_noisy_tool_call_tag_body() {
        let response = r#"<tool_call>
I will now call the tool with this payload:
{"name": "shell", "arguments": {"command": "pwd"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_call_inline_attributes() {
        let response = r#"<tool_call>send_message channel="user_channel" message="Hello! How can I assist you today?"</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "send_message");
        assert_eq!(
            calls[0].arguments.get("channel").unwrap().as_str().unwrap(),
            "user_channel"
        );
        assert_eq!(
            calls[0].arguments.get("message").unwrap().as_str().unwrap(),
            "Hello! How can I assist you today?"
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_call_function_style_arguments() {
        let response = r#"<tool_call>message_send(channel="general", message="test")</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "message_send");
        assert_eq!(
            calls[0].arguments.get("channel").unwrap().as_str().unwrap(),
            "general"
        );
        assert_eq!(
            calls[0].arguments.get("message").unwrap().as_str().unwrap(),
            "test"
        );
    }

    #[test]
    fn parse_tool_calls_handles_xml_nested_tool_payload() {
        let response = r#"<tool_call>
<memory_recall>
<query>project roadmap</query>
</memory_recall>
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
        assert_eq!(
            calls[0].arguments.get("query").unwrap().as_str().unwrap(),
            "project roadmap"
        );
    }

    #[test]
    fn parse_tool_calls_handles_plural_tool_calls_wrapper() {
        // Regression: Llama 4 Scout (via Groq) emits a plural `<tool_calls>`
        // wrapper rather than the singular `<tool_call>`. The parser must
        // enter it and execute the call instead of exposing raw XML.
        let (text, calls) = parse_tool_calls(
            "<tool_calls>\n{\"name\":\"myserver__some_tool\",\"arguments\":{\"key\":\"value\"}}\n</tool_calls>",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "myserver__some_tool");
        assert_eq!(
            calls[0].arguments.get("key").unwrap().as_str().unwrap(),
            "value"
        );
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_ignores_xml_thinking_wrapper() {
        let response = r#"<tool_call>
<thinking>Need to inspect memory first</thinking>
<memory_recall>
<query>recent deploy notes</query>
</memory_recall>
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "memory_recall");
        assert_eq!(
            calls[0].arguments.get("query").unwrap().as_str().unwrap(),
            "recent deploy notes"
        );
    }

    #[test]
    fn parse_tool_calls_handles_xml_with_json_arguments() {
        let response = r#"<tool_call>
<shell>{"command":"pwd"}</shell>
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
    }

    #[test]
    fn parse_tool_calls_handles_markdown_tool_call_fence() {
        let response = r#"I'll check that.
```tool_call
{"name": "shell", "arguments": {"command": "pwd"}}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
        assert!(text.contains("I'll check that."));
        assert!(text.contains("Done."));
        assert!(!text.contains("```tool_call"));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_tool_call_hybrid_close_tag() {
        let response = r#"Preface
```tool-call
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>
Tail"#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
        assert!(text.contains("Preface"));
        assert!(text.contains("Tail"));
        assert!(!text.contains("```tool-call"));
    }

    #[test]
    fn parse_tool_calls_handles_markdown_invoke_fence() {
        let response = r#"Checking.
```invoke
{"name": "shell", "arguments": {"command": "date"}}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
        assert!(text.contains("Checking."));
        assert!(text.contains("Done."));
    }

    #[test]
    fn parse_tool_calls_handles_tool_name_fence_format() {
        //: xAI grok models use ```tool <name> format
        let response = r#"I'll write a test file.
```tool file_write
{"path": "/home/user/test.txt", "content": "Hello world"}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "/home/user/test.txt"
        );
        assert!(text.contains("I'll write a test file."));
        assert!(text.contains("Done."));
    }

    #[test]
    fn parse_tool_calls_recovers_malformed_file_write_content_quotes() {
        let response = r#"<tool_call>
{"name":"file_write","arguments":{"path":"index.html","content":"<section class="hero"><script>const msg = "ok";</script></section>"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "index.html"
        );
        assert_eq!(
            calls[0].arguments.get("content").unwrap().as_str().unwrap(),
            r#"<section class="hero"><script>const msg = "ok";</script></section>"#
        );
    }

    #[test]
    fn parse_tool_calls_recovers_malformed_file_write_tool_name_fence() {
        let response = r#"```tool file_write
{"path":"index.html","content":"<div data-kind="card">ok</div>"}
```"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("content").unwrap().as_str().unwrap(),
            r#"<div data-kind="card">ok</div>"#
        );
    }

    #[test]
    fn parse_tool_calls_recovers_malformed_file_write_non_ascii_safely() {
        let response = r#"说明:
<tool_call>
{"name":"file_write","arguments":{"path":"页面.html","content":"<p title="问候">你好，世界 🌏</p>"}}
</tool_call>
完成"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("说明"));
        assert!(text.contains("完成"));
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "页面.html"
        );
        assert_eq!(
            calls[0].arguments.get("content").unwrap().as_str().unwrap(),
            r#"<p title="问候">你好，世界 🌏</p>"#
        );
    }

    #[test]
    fn parse_tool_calls_rejects_ambiguous_malformed_file_write() {
        let response = r#"<tool_call>
{"name":"file_write","arguments":{"path":"index.html","content":"<section class="hero">","mode":"append"}}
</tool_call>"#;

        let (_text, calls) = parse_tool_calls(response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_valid_file_write_json_unchanged() {
        let response = r#"{"name":"file_write","arguments":{"path":"index.html","content":"<section class=\"hero\">ok</section>"}}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(
            calls[0].arguments.get("content").unwrap().as_str().unwrap(),
            r#"<section class="hero">ok</section>"#
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_name_fence_shell() {
        //: Test shell command in ```tool shell format
        let response = r#"```tool shell
{"command": "ls -la"}
```"#;

        let (_text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls -la"
        );
    }

    #[test]
    fn parse_tool_calls_handles_multiple_tool_name_fences() {
        // Multiple tool calls in ```tool <name> format
        let response = r#"First, I'll write a file.
```tool file_write
{"path": "/tmp/a.txt", "content": "A"}
```
Then read it.
```tool file_read
{"path": "/tmp/a.txt"}
```
Done."#;

        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "file_write");
        assert_eq!(calls[1].name, "file_read");
        assert!(text.contains("First, I'll write a file."));
        assert!(text.contains("Then read it."));
        assert!(text.contains("Done."));
    }

    #[test]
    fn parse_tool_calls_handles_toolcall_tag_alias() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</toolcall>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
    }

    #[test]
    fn parse_tool_calls_handles_tools_tag_alias() {
        // Qwen2.5-Coder-32B wraps a well-formed Hermes call in the tool
        // *declaration* tag rather than the invocation tag. Observed
        // deterministically (6/6 across two independent runs, and independent
        // of how many tools are offered), so the call is recoverable and should
        // not be dropped as prose.
        let response = r#"<tools>
{"name": "get_weather", "arguments": {"city": "Paris"}}
</tools>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "get_weather");
        assert_eq!(
            calls[0].arguments.get("city").unwrap().as_str().unwrap(),
            "Paris"
        );
    }

    #[test]
    fn parse_tool_calls_rejects_tools_declaration_block() {
        // `<tools>` is ALSO the Hermes tag that DECLARES the available tools.
        // A declaration is an array of schemas -- `description` / `parameters`,
        // no `arguments` -- and must never be executed as an invocation.
        let response = r#"<tools>
[{"name": "get_weather", "description": "Get the current weather for a city.", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}]
</tools>"#;

        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "a tool DECLARATION must not be parsed as an invocation, got {calls:?}"
        );
    }

    #[test]
    fn parse_tool_calls_rejects_tools_block_discussed_in_prose() {
        // An assistant explaining the Hermes format must not trigger a call.
        let response = r#"In the Hermes prompt format the available tools are declared like this:

<tools>
[{"name": "shell", "description": "Run a command", "parameters": {}}]
</tools>

and the model then replies with a <tool_call> block."#;

        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "prose describing the format must not be parsed as an invocation, got {calls:?}"
        );
    }

    #[test]
    fn parse_tool_calls_rejects_echoed_system_prompt_tools_block() {
        // Some models echo their own system prompt. That echo contains the
        // declaration block verbatim and must stay inert.
        let response = r#"You are a helpful assistant with access to the following functions.
<tools>
[{"type": "function", "function": {"name": "file_read", "description": "Read a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}]
</tools>
Use them when appropriate."#;

        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "an echoed system prompt must not be parsed as an invocation, got {calls:?}"
        );
    }

    #[test]
    fn tools_wrapper_does_not_reach_glm_shortened_body_on_matching_close() {
        // GLM shortened bodies are executable legacy syntax: `shell>cmd` becomes
        // a shell call under the unambiguous tags. A value-shaped admission rule
        // cannot see it at all, because such a body never parses as JSON -- which
        // is why `<tools>` carries canonical JSON or nothing, rather than being
        // filtered on the way out of the legacy parsers.
        let response = "<tools>shell>rm -rf /tmp/x</tools>";
        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "GLM shortened body under <tools> must stay inert, got {calls:?}"
        );
    }

    #[test]
    fn tools_wrapper_does_not_reach_glm_shortened_body_on_foreign_close() {
        let response = "<tools>shell>rm -rf /tmp/x</tool_call>";
        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "GLM body under <tools> with a foreign close must stay inert, got {calls:?}"
        );
    }

    #[test]
    fn tools_wrapper_does_not_reach_glm_shortened_body_when_unclosed() {
        let response = "<tools>shell>rm -rf /tmp/x";
        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "unclosed GLM body under <tools> must stay inert, got {calls:?}"
        );
    }

    #[test]
    fn glm_shortened_body_still_works_under_an_unambiguous_tag() {
        // Positive control for the restriction: gating <tools> must not disable
        // legacy recovery for the tags that are not overloaded.
        let response = "<tool_call>shell>uname -a</tool_call>";
        let (_text, calls) = parse_tool_calls(response);
        assert_eq!(
            calls.len(),
            1,
            "GLM shortened body must still parse under <tool_call>"
        );
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn parse_tool_calls_rejects_tools_declaration_closed_by_foreign_alias() {
        // Mismatched close. The cross-alias recovery path used to parse this
        // body without consulting the <tools> guard, so closing a declaration
        // with a FOREIGN alias was enough to turn it into a call.
        let response = r#"<tools>
[{"name": "shell", "description": "Run a command", "parameters": {}}]
</tool_call>"#;

        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "a declaration closed by a foreign alias must stay inert, got {calls:?}"
        );
    }

    #[test]
    fn parse_tool_calls_rejects_unclosed_tools_declaration() {
        // Missing close. Truncation mid-stream reaches the brace-balancing
        // recovery path, which was likewise unguarded.
        let response = r#"<tools>
[{"name": "shell", "description": "Run a command", "parameters": {}}]"#;

        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "an unclosed declaration must stay inert, got {calls:?}"
        );
    }

    #[test]
    fn parse_tool_calls_rejects_tools_wrapper_with_args_key() {
        // `args` is not the canonical key: the parser reads `arguments`. When
        // the admission predicate accepted `args`, this was admitted as an
        // invocation and then dispatched with EMPTY arguments -- the runtime
        // received a different call from the one the model encoded. Staying
        // inert is correct; a corrupted call is not.
        let response = r#"<tools>
{"name": "shell", "args": {"command": "rm -rf /tmp/x"}}
</tools>"#;

        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "an `args`-shaped body must not be dispatched with empty arguments, got {calls:?}"
        );
    }

    #[test]
    fn parse_tool_calls_still_accepts_canonical_tools_invocation() {
        // Positive control: the motivating Qwen payload must keep working, so
        // the guards above cannot be satisfied by simply rejecting everything.
        let response = r#"<tools>
{"name": "shell", "arguments": {"command": "uname -a"}}
</tools>"#;

        let (_text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1, "canonical invocation must still parse");
        assert_eq!(calls[0].name, "shell");
        assert!(
            calls[0].arguments.get("command").is_some(),
            "arguments must be preserved, got {:?}",
            calls[0].arguments
        );
    }

    #[test]
    fn parse_tool_calls_handles_tool_dash_call_tag_alias() {
        let response = r#"<tool-call>
{"name": "shell", "arguments": {"command": "whoami"}}
</tool-call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "whoami"
        );
    }

    #[test]
    fn parse_tool_calls_handles_invoke_tag_alias() {
        let response = r#"<invoke>
{"name": "shell", "arguments": {"command": "uptime"}}
</invoke>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime"
        );
    }

    #[test]
    fn parse_tool_calls_handles_minimax_invoke_parameter_format() {
        let response = r#"<minimax:tool_call>
<invoke name="shell">
<parameter name="command">sqlite3 /tmp/test.db ".tables"</parameter>
</invoke>
</minimax:tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            r#"sqlite3 /tmp/test.db ".tables""#
        );
    }

    #[test]
    fn parse_tool_calls_handles_minimax_invoke_with_surrounding_text() {
        let response = r#"Preface
<minimax:tool_call>
<invoke name='http_request'>
<parameter name='url'>https://example.com</parameter>
<parameter name='method'>GET</parameter>
</invoke>
</minimax:tool_call>
Tail"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Preface"));
        assert!(text.contains("Tail"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "http_request");
        assert_eq!(
            calls[0].arguments.get("url").unwrap().as_str().unwrap(),
            "https://example.com"
        );
        assert_eq!(
            calls[0].arguments.get("method").unwrap().as_str().unwrap(),
            "GET"
        );
    }

    #[test]
    fn parse_tool_calls_handles_minimax_toolcall_alias_and_cross_close_tag() {
        let response = r#"<tool_call>
{"name":"shell","arguments":{"command":"date"}}
</minimax:toolcall>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
    }

    #[test]
    fn parse_tool_calls_handles_perl_style_tool_call_blocks() {
        let response = r#"TOOL_CALL
{tool => "shell", args => { --command "uname -a" }}}
/TOOL_CALL"#;

        let calls = parse_perl_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uname -a"
        );
    }

    #[test]
    fn parse_tool_calls_handles_square_bracket_tool_call_blocks() {
        let response =
            r#"[TOOL_CALL]{tool => "shell", args => {--command "echo hello"}}[/TOOL_CALL]"#;

        let calls = parse_perl_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "echo hello"
        );
    }

    #[test]
    fn parse_tool_calls_handles_square_bracket_multiline() {
        let response = r#"[TOOL_CALL]
{tool => "file_read", args => {
  --path "/tmp/test.txt"
  --description "Read test file"
}}
[/TOOL_CALL]"#;

        let calls = parse_perl_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(
            calls[0].arguments.get("path").unwrap().as_str().unwrap(),
            "/tmp/test.txt"
        );
        assert_eq!(
            calls[0]
                .arguments
                .get("description")
                .unwrap()
                .as_str()
                .unwrap(),
            "Read test file"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_unclosed_tool_call_with_json() {
        let response = r#"I will call the tool now.
<tool_call>
{"name": "shell", "arguments": {"command": "uptime -p"}}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("I will call the tool now."));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime -p"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_mismatched_close_tag() {
        let response = r#"<tool_call>
{"name": "shell", "arguments": {"command": "uptime"}}
</arg_value>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "uptime"
        );
    }

    #[test]
    fn parse_tool_calls_recovers_cross_alias_closing_tags() {
        let response = r#"<toolcall>
{"name": "shell", "arguments": {"command": "date"}}
</tool_call>"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.is_empty());
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn parse_tool_calls_rejects_raw_tool_json_without_tags() {
        // SECURITY: Raw JSON without explicit wrappers should NOT be parsed
        // This prevents prompt injection attacks where malicious content
        // could include JSON that mimics a tool call.
        let response = r#"Sure, creating the file now.
{"name": "file_write", "arguments": {"path": "hello.py", "content": "print('hello')"}}"#;

        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Sure, creating the file now."));
        assert_eq!(
            calls.len(),
            0,
            "Raw JSON without wrappers should not be parsed"
        );
    }

    #[test]
    fn parse_tool_calls_handles_empty_tool_result() {
        // Recovery: Empty tool_result tag should be handled gracefully
        let response = r#"I'll run that command.
<tool_result name="shell">

</tool_result>
Done."#;
        let (text, calls) = parse_tool_calls(response);
        assert!(text.contains("Done."));
        assert!(calls.is_empty());
    }

    #[test]
    fn strip_tool_result_blocks_removes_single_block() {
        let input = r#"<tool_result name="memory_recall" status="ok">
{"matches":["hello"]}
</tool_result>
Here is my answer."#;
        assert_eq!(strip_tool_result_blocks(input), "Here is my answer.");
    }

    #[test]
    fn strip_tool_result_blocks_removes_multiple_blocks() {
        let input = r#"<tool_result name="memory_recall" status="ok">
{"matches":[]}
</tool_result>
<tool_result name="shell" status="ok">
done
</tool_result>
Final answer."#;
        assert_eq!(strip_tool_result_blocks(input), "Final answer.");
    }

    #[test]
    fn strip_tool_result_blocks_removes_prefix() {
        let input =
            "[Tool results]\n<tool_result name=\"shell\" status=\"ok\">\nok\n</tool_result>\nDone.";
        assert_eq!(strip_tool_result_blocks(input), "Done.");
    }

    #[test]
    fn strip_tool_result_blocks_removes_thinking() {
        let input = "<thinking>\nLet me think...\n</thinking>\nHere is the answer.";
        assert_eq!(strip_tool_result_blocks(input), "Here is the answer.");
    }

    #[test]
    fn strip_tool_result_blocks_removes_think_tags() {
        let input = "<think>\nLet me reason...\n</think>\nHere is the answer.";
        assert_eq!(strip_tool_result_blocks(input), "Here is the answer.");
    }

    #[test]
    fn parse_tool_calls_strips_think_before_tool_call() {
        // Qwen regression: <think> tags before <tool_call> tags should be
        // stripped, allowing the tool call to be parsed correctly.
        let response = "<think>I need to list files to understand the project</think>\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}\n</tool_call>";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(
            calls.len(),
            1,
            "should parse tool call after stripping think tags"
        );
        assert_eq!(calls[0].name, "shell");
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "ls"
        );
        assert!(text.is_empty(), "think content should not appear as text");
    }

    #[test]
    fn parse_tool_calls_strips_think_only_returns_empty() {
        // When response is only <think> tags with no tool calls, should
        // return empty text and no calls.
        let response = "<think>Just thinking, no action needed</think>";
        let (text, calls) = parse_tool_calls(response);
        assert!(calls.is_empty());
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_qwen_think_with_multiple_tool_calls() {
        let response = "<think>I need to check two things</think>\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"date\"}}\n</tool_call>\n<tool_call>\n{\"name\":\"shell\",\"arguments\":{\"command\":\"pwd\"}}\n</tool_call>";
        let (_, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].arguments.get("command").unwrap().as_str().unwrap(),
            "date"
        );
        assert_eq!(
            calls[1].arguments.get("command").unwrap().as_str().unwrap(),
            "pwd"
        );
    }

    #[test]
    fn strip_tool_result_blocks_preserves_clean_text() {
        let input = "Hello, this is a normal response.";
        assert_eq!(strip_tool_result_blocks(input), input);
    }

    #[test]
    fn strip_tool_result_blocks_returns_empty_for_only_tags() {
        let input = "<tool_result name=\"memory_recall\" status=\"ok\">\n{}\n</tool_result>";
        assert_eq!(strip_tool_result_blocks(input), "");
    }

    #[test]
    fn parse_arguments_value_handles_null() {
        // Recovery: null arguments are returned as-is (Value::Null)
        let value = serde_json::json!(null);
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_null());
    }

    #[test]
    fn parse_tool_calls_handles_empty_tool_calls_array() {
        // Recovery: Empty tool_calls array returns original response (no tool parsing)
        let response = r#"{"content": "Hello", "tool_calls": []}"#;
        let (text, calls) = parse_tool_calls(response);
        // When tool_calls is empty, the entire JSON is returned as text
        assert!(text.contains("Hello"));
        assert!(calls.is_empty());
    }

    #[test]
    fn detect_tool_call_parse_issue_flags_malformed_payloads() {
        let response =
            "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"pwd\"}</tool_call>";
        let issue = detect_tool_call_parse_issue(response, &[]);
        assert!(
            issue.is_some(),
            "malformed tool payload should be flagged for diagnostics"
        );
    }

    #[test]
    fn detect_tool_call_parse_issue_ignores_normal_text() {
        let issue = detect_tool_call_parse_issue("Thanks, done.", &[]);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_tool_call_parse_issue_ignores_empty_tool_calls_array() {
        let issue = detect_tool_call_parse_issue(r#"{"content":"Hello","tool_calls":[]}"#, &[]);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_tool_call_parse_issue_ignores_json_fenced_business_tool_calls() {
        let response = r#"```json
{"tool_calls":[{"service":"billing","count":2}]}
```"#;
        let issue = detect_tool_call_parse_issue(response, &[]);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_tool_call_parse_issue_ignores_tool_call_fenced_example() {
        let response = r#"```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```
This is an example, not an invocation."#;

        let issue = detect_tool_call_parse_issue(response, &[]);

        assert!(issue.is_none());
    }

    #[test]
    fn detect_tool_call_parse_issue_flags_standalone_tool_call_fence() {
        let response = r#"```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```"#;

        let issue = detect_tool_call_parse_issue(response, &[]);

        assert!(issue.is_some());
    }

    #[test]
    fn detect_tool_call_parse_issue_ignores_tool_call_tag_example() {
        let response = r#"<tool_call>
{"name":"shell","arguments":{"command":"pwd"}}
</tool_call>
This is an example, not an invocation."#;

        let issue = detect_tool_call_parse_issue(response, &[]);

        assert!(issue.is_none());
    }

    #[test]
    fn detect_tool_call_parse_issue_flags_tagged_tool_call_with_trailing_text() {
        let response = r#"<tool_call>
{"name":"shell","arguments":{"command":"pwd"}}
</tool_call>
Done."#;

        let issue = detect_tool_call_parse_issue(response, &[]);

        assert!(issue.is_some());
    }

    #[test]
    fn detect_tool_call_parse_issue_flags_json_fenced_tool_protocol() {
        let response = r#"```json
{"tool_calls":[{"name":"shell","arguments":{"command":"pwd"}}]}
```"#;
        let issue = detect_tool_call_parse_issue(response, &[]);
        assert!(issue.is_some());
    }

    #[test]
    fn detect_tool_call_parse_issue_flags_malformed_tool_result_envelope() {
        let response = r#"{"tool_call_id":"call_1","content":"raw tool output""#;
        let issue = detect_tool_call_parse_issue(response, &[]);
        assert!(issue.is_some());
    }

    #[test]
    fn detect_tool_call_parse_issue_ignores_malformed_tool_call_id_only_json() {
        let response = r#"{"tool_call_id":"support-case-1""#;
        let issue = detect_tool_call_parse_issue(response, &[]);
        assert!(issue.is_none());
    }

    #[test]
    fn detect_tool_call_parse_issue_flags_malformed_nonempty_tool_calls_array() {
        let issue = detect_tool_call_parse_issue(
            r#"{"content":null,"tool_calls":[{"call_id":"call_1","arguments":"{}"}]}"#,
            &[],
        );
        assert!(issue.is_some());
    }

    #[test]
    fn detect_tool_call_parse_issue_ignores_malformed_business_tool_calls_without_call_id() {
        for response in [
            r#"{"tool_calls":[{"name":"support_case","arguments":{"id":"A1"}}"#,
            r#"{"toolcalls":[{"name":"support_case","arguments":{"id":"A1"}}"#,
        ] {
            let issue = detect_tool_call_parse_issue(response, &[]);

            assert!(
                issue.is_none(),
                "business JSON without a tool call id must not be treated as internal protocol: {response}"
            );
            assert!(
                !looks_like_malformed_tool_protocol_envelope(response),
                "business JSON without a tool call id must not be classified as malformed protocol: {response}"
            );
        }
    }

    #[test]
    fn looks_like_tool_protocol_envelope_flags_malformed_nonempty_tool_calls_array() {
        assert!(looks_like_tool_protocol_envelope(
            r#"{"content":null,"tool_calls":[{"call_id":"call_1","arguments":"{}"}]}"#
        ));
        assert!(!looks_like_tool_protocol_envelope(
            r#"{"content":"Hello","tool_calls":[]}"#
        ));
    }

    #[test]
    fn classify_tool_protocol_envelope_flags_internal_json_variants() {
        assert_eq!(
            classify_tool_protocol_envelope(
                r#"{"content":null,"tool_calls":[{"id":"call_1","name":"shell","arguments":"{}"}]}"#
            ),
            Some(ToolProtocolEnvelopeKind::ToolCalls)
        );
        assert_eq!(
            classify_tool_protocol_envelope(
                r#"{"toolcalls":[{"name":"shell","arguments":{"command":"pwd"}}]}"#
            ),
            Some(ToolProtocolEnvelopeKind::ToolCallsAlias)
        );
        assert_eq!(
            classify_tool_protocol_envelope(r#"{"tool_calls":[{"name":"shell","arguments":{}}]}"#),
            Some(ToolProtocolEnvelopeKind::ToolCalls)
        );
        assert_eq!(
            classify_tool_protocol_envelope(r#"{"toolcalls":[{"name":"shell","arguments":{}}]}"#),
            Some(ToolProtocolEnvelopeKind::ToolCallsAlias)
        );
        assert_eq!(
            classify_tool_protocol_envelope(
                r#"{"function_call":{"name":"shell","arguments":"{\"command\":\"pwd\"}"}}"#
            ),
            Some(ToolProtocolEnvelopeKind::FunctionCall)
        );
        assert_eq!(
            classify_tool_protocol_envelope(
                r#"{"tool_call_id":"call_1","content":"command output"}"#
            ),
            Some(ToolProtocolEnvelopeKind::ToolResult)
        );
        assert_eq!(
            classify_tool_protocol_envelope(
                r#"{"type":"function_call","call_id":"call_1","name":"shell","arguments":"{}"}"#
            ),
            Some(ToolProtocolEnvelopeKind::ResponsesFunctionCall)
        );
        assert_eq!(
            classify_tool_protocol_envelope(
                r#"```json
{"tool_calls":[{"name":"shell","arguments":{"command":"pwd"}}]}
```"#
            ),
            Some(ToolProtocolEnvelopeKind::ToolCalls)
        );
    }

    #[test]
    fn classify_tool_protocol_envelope_preserves_tool_call_examples() {
        let fenced_example = r#"```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```
This is an example, not an invocation."#;
        let embedded_fenced_example = r#"Here is an example:
```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```"#;
        let embedded_fenced_example_cn = r#"例如：
```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```"#;
        let tag_example = r#"<tool_call>
{"name":"shell","arguments":{"command":"pwd"}}
</tool_call>
This is an example, not an invocation."#;
        let tag_example_cn = r#"比如：
<tool_call>
{"name":"shell","arguments":{"command":"pwd"}}
</tool_call>"#;

        assert_eq!(classify_tool_protocol_envelope(fenced_example), None);
        assert!(!looks_like_tool_protocol_envelope(fenced_example));
        assert_eq!(
            classify_tool_protocol_envelope(embedded_fenced_example),
            None
        );
        assert!(!looks_like_tool_protocol_envelope(embedded_fenced_example));
        assert!(looks_like_tool_protocol_example(embedded_fenced_example));
        assert_eq!(
            classify_tool_protocol_envelope(embedded_fenced_example_cn),
            None
        );
        assert!(!looks_like_tool_protocol_envelope(
            embedded_fenced_example_cn
        ));
        assert!(looks_like_tool_protocol_example(embedded_fenced_example_cn));
        assert_eq!(classify_tool_protocol_envelope(tag_example), None);
        assert!(!looks_like_tool_protocol_envelope(tag_example));
        assert_eq!(classify_tool_protocol_envelope(tag_example_cn), None);
        assert!(!looks_like_tool_protocol_envelope(tag_example_cn));
        assert!(looks_like_tool_protocol_example(tag_example_cn));
    }

    #[test]
    fn contains_tool_protocol_tag_call_flags_embedded_tool_call_fences() {
        let embedded = r#"Let me call it:
```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```
Done."#;

        assert!(contains_tool_protocol_tag_call(embedded));
    }

    /// The two untagged call forms inside an invalid result fragment:
    /// no tag marker, so the tagged check does not look, while
    /// `parse_tool_calls` still returns the shell call. The fragment
    /// with a plain placeholder is the control: no call either way.
    #[test]
    fn contains_parseable_tool_call_reads_untagged_forms_the_tagged_check_misses() {
        let glm = "{\"tool_call_id\":\"call_1\",\"content\":\nshell/command>printf guard_probe\n}";
        let perl = "{\"tool_call_id\":\"call_1\",\"content\":\nTOOL_CALL { tool => \"shell\", args => { --command \"pwd\" }} /TOOL_CALL\n}";
        let inert = "{\"tool_call_id\": <call-123>, \"content\": <text>}";

        for (label, text) in [("glm", glm), ("TOOL_CALL", perl)] {
            let (_, calls) = parse_tool_calls(text);
            assert_eq!(
                calls
                    .iter()
                    .map(|call| call.name.as_str())
                    .collect::<Vec<_>>(),
                vec!["shell"],
                "{label}: the parser reads a shell call from the fragment"
            );
            assert!(
                !contains_tool_protocol_tag_call(text),
                "{label}: no tag marker, so the tagged check does not see the call"
            );
            assert!(
                contains_parseable_tool_call(text),
                "{label}: the any-syntax check must see what the parser reads"
            );
        }

        assert!(parse_tool_calls(inert).1.is_empty());
        assert!(!contains_parseable_tool_call(inert));
    }

    /// An opened call whose closer has not arrived parses as nothing, so
    /// only the opener check sees it. The `tool_call_id` key alone is not
    /// an opener.
    #[test]
    fn contains_tool_call_opener_sees_calls_the_parser_cannot_close_yet() {
        let cases = [
            (
                "single-quoted invoke",
                "{\"tool_call_id\": \"call_1\", \"content\": \"<invoke name='shell'><parameter name='command'>pwd</parameter>\"}",
            ),
            (
                "double-quoted invoke",
                "{\"tool_call_id\":\"call_1\",\"content\": <invoke name=\"shell\"><parameter name=\"command\">pwd</parameter>",
            ),
            (
                "TOOL_CALL",
                "{\"tool_call_id\":\"call_1\",\"content\":\"TOOL_CALL {\"}",
            ),
            (
                "[TOOL_CALL]",
                "{\"tool_call_id\":\"call_1\",\"content\":\"[TOOL_CALL]{\"}",
            ),
        ];
        for (label, text) in cases {
            assert!(
                parse_tool_calls(text).1.is_empty(),
                "{label}: the call has not closed, so the parser reads none"
            );
            assert!(
                contains_tool_call_opener(text),
                "{label}: the opener is seen"
            );
        }
        assert!(!contains_tool_call_opener(
            "{\"tool_call_id\": \"call_1\", \"content\": \"ok\"}"
        ));
    }

    #[test]
    fn classify_tool_protocol_envelope_flags_standalone_tool_fences() {
        let tool_call_fence = r#"```tool_call
{"name":"shell","arguments":{"command":"pwd"}}
```"#;
        let invoke_fence = r#"```invoke
{"name":"shell","arguments":{"command":"pwd"}}
```"#;
        let tool_name_fence = r#"```tool shell
{"command":"pwd"}
```"#;

        assert_eq!(
            classify_tool_protocol_envelope(tool_call_fence),
            Some(ToolProtocolEnvelopeKind::TaggedToolCall)
        );
        assert!(looks_like_tool_protocol_envelope(tool_call_fence));
        assert_eq!(
            classify_tool_protocol_envelope(invoke_fence),
            Some(ToolProtocolEnvelopeKind::TaggedToolCall)
        );
        assert!(looks_like_tool_protocol_envelope(invoke_fence));
        assert_eq!(
            classify_tool_protocol_envelope(tool_name_fence),
            Some(ToolProtocolEnvelopeKind::TaggedToolCall)
        );
        assert!(looks_like_tool_protocol_envelope(tool_name_fence));
    }

    #[test]
    fn classify_tool_protocol_envelope_preserves_top_level_arrays_without_protocol_marker() {
        assert!(!looks_like_tool_protocol_envelope(
            r#"[{"service":"billing","count":2}]"#
        ));

        assert!(!looks_like_tool_protocol_envelope(
            r#"[{"name":"shell","arguments":{}}]"#
        ));
    }

    #[test]
    fn classify_tool_protocol_envelope_preserves_top_level_schema_array() {
        let schema = r#"[{"name":"planner","parameters":{"goal":"string"}}]"#;

        assert_eq!(classify_tool_protocol_envelope(schema), None);
        assert!(!looks_like_tool_protocol_envelope(schema));
    }

    #[test]
    fn classify_tool_protocol_envelope_preserves_plain_user_json() {
        let profile = r#"{"name":"profile","parameters":{"timezone":"UTC"}}"#;
        assert_eq!(classify_tool_protocol_envelope(profile), None);
        assert!(!looks_like_tool_protocol_envelope(profile));
    }

    #[test]
    fn looks_like_tool_protocol_envelope_preserves_plain_json_with_similar_keys() {
        let config = r#"{"function_call":false,"description":"disable the feature"}"#;
        assert!(!looks_like_tool_protocol_envelope(config));

        let audit_log = r#"{"tool_calls":[{"service":"billing","count":2}]}"#;
        assert!(!looks_like_tool_protocol_envelope(audit_log));

        let queued_case =
            r#"{"tool_calls":[{"id":"case-1","status":"queued","service":"billing"}]}"#;
        assert!(!looks_like_tool_protocol_envelope(queued_case));

        let named_record =
            r#"{"tool_calls":[{"name":"planner","status":"queued","service":"workflow"}]}"#;
        assert!(!looks_like_tool_protocol_envelope(named_record));
    }

    #[test]
    fn parse_tool_calls_handles_whitespace_only_name() {
        // Recovery: Whitespace-only tool name should return None
        let value = serde_json::json!({"function": {"name": "   ", "arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_calls_handles_empty_string_arguments() {
        // Recovery: Empty string arguments should be handled
        let value = serde_json::json!({"name": "test", "arguments": ""});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test");
    }

    #[test]
    fn parse_arguments_value_handles_invalid_json_string() {
        // Recovery: Invalid JSON string should return empty object
        let value = serde_json::Value::String("not valid json".to_string());
        let result = parse_arguments_value(Some(&value));
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_arguments_value_handles_none() {
        // Recovery: None arguments should return empty object
        let result = parse_arguments_value(None);
        assert!(result.is_object());
        assert!(result.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_empty_array() {
        // Recovery: Empty tool_calls array should return empty vec
        let value = serde_json::json!({"tool_calls": []});
        let result = parse_tool_calls_from_json_value(&value);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_missing_tool_calls() {
        // Recovery: Missing tool_calls field should fall through
        let value = serde_json::json!({"name": "test", "arguments": {}});
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn parse_tool_calls_from_json_value_handles_top_level_array() {
        // Recovery: Top-level array of tool calls
        let value = serde_json::json!([
            {"name": "tool_a", "arguments": {}},
            {"name": "tool_b", "arguments": {}}
        ]);
        let result = parse_tool_calls_from_json_value(&value);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn parse_glm_style_browser_open_url() {
        let response = "browser_open/url>https://example.com";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "browser_open");
        assert_eq!(
            calls[0].1,
            serde_json::json!({"url": "https://example.com"})
        );
    }

    #[test]
    fn parse_glm_style_shell_quotes_url_apostrophes_and_metacharacters() {
        let calls = parse_glm_style_tool_calls("shell/url>https://example.com/it's;still=one");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert_eq!(
            calls[0].1["command"],
            r#"curl -s 'https://example.com/it'"'"'s;still=one'"#
        );
    }

    #[test]
    fn parse_glm_style_shell_command() {
        let response = "shell/command>ls -la";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert_eq!(calls[0].1["command"], "ls -la");
    }

    #[test]
    fn parse_glm_style_http_request() {
        let response = "http_request/url>https://api.example.com/data";
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "http_request");
        assert_eq!(calls[0].1["url"], "https://api.example.com/data");
        assert_eq!(calls[0].1["method"], "GET");
    }

    #[test]
    fn parse_glm_style_ignores_plain_url() {
        // A bare URL should NOT be interpreted as a tool call — this was
        // causing false positives when LLMs included URLs in normal text.
        let response = "https://example.com/api";
        let calls = parse_glm_style_tool_calls(response);
        assert!(
            calls.is_empty(),
            "plain URL must not be parsed as tool call"
        );
    }

    #[test]
    fn parse_glm_style_json_args() {
        let response = r#"shell/{"command": "echo hello"}"#;
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "shell");
        assert_eq!(calls[0].1["command"], "echo hello");
    }

    #[test]
    fn parse_glm_style_multiple_calls() {
        let response = r#"shell/command>ls
browser_open/url>https://example.com"#;
        let calls = parse_glm_style_tool_calls(response);
        assert_eq!(calls.len(), 2);
    }

    #[test]
    fn parse_glm_style_tool_call_integration() {
        // Integration test: GLM format should be parsed in parse_tool_calls
        let response = "Checking...\nbrowser_open/url>https://example.com\nDone";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "browser_open");
        assert_eq!(
            calls[0].arguments,
            serde_json::json!({"url": "https://example.com"})
        );
        assert!(text.contains("Checking"));
        assert!(text.contains("Done"));
    }

    #[test]
    fn parse_glm_style_shell_rejects_non_http_url_param() {
        let response = "shell/url>javascript:alert(1)";
        let calls = parse_glm_style_tool_calls(response);
        assert!(calls.is_empty());
    }

    #[test]
    fn parse_tool_calls_handles_unclosed_tool_call_tag() {
        let response = "<tool_call>{\"name\":\"shell\",\"arguments\":{\"command\":\"pwd\"}}\nDone";
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "pwd");
        assert_eq!(text, "Done");
    }

    #[test]
    fn parse_tool_calls_empty_input_returns_empty() {
        let (text, calls) = parse_tool_calls("");
        assert!(calls.is_empty(), "empty input should produce no tool calls");
        assert!(text.is_empty(), "empty input should produce no text");
    }

    #[test]
    fn parse_tool_calls_whitespace_only_returns_empty_calls() {
        let (text, calls) = parse_tool_calls("   \n\t  ");
        assert!(calls.is_empty());
        assert!(text.is_empty() || text.trim().is_empty());
    }

    #[test]
    fn parse_tool_calls_nested_xml_tags_handled() {
        // Double-wrapped tool call should still parse the inner call
        let response = r#"<tool_call><tool_call>{"name":"echo","arguments":{"msg":"hi"}}</tool_call></tool_call>"#;
        let (_text, calls) = parse_tool_calls(response);
        // Should find at least one tool call
        assert!(
            !calls.is_empty(),
            "nested XML tags should still yield at least one tool call"
        );
    }

    #[test]
    fn parse_tool_calls_truncated_json_no_panic() {
        // Incomplete JSON inside tool_call tags
        let response = r#"<tool_call>{"name":"shell","arguments":{"command":"ls"</tool_call>"#;
        let (_text, _calls) = parse_tool_calls(response);
        // Should not panic — graceful handling of truncated JSON
    }

    #[test]
    fn parse_tool_calls_empty_json_object_in_tag() {
        let response = "<tool_call>{}</tool_call>";
        let (_text, calls) = parse_tool_calls(response);
        // Empty JSON object has no name field — should not produce valid tool call
        assert!(
            calls.is_empty(),
            "empty JSON object should not produce a tool call"
        );
    }

    #[test]
    fn parse_tool_calls_closing_tag_only_returns_text() {
        let response = "Some text </tool_call> more text";
        let (text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "closing tag only should not produce calls"
        );
        assert!(
            !text.is_empty(),
            "text around orphaned closing tag should be preserved"
        );
    }

    #[test]
    fn parse_tool_calls_very_large_arguments_no_panic() {
        let large_arg = "x".repeat(100_000);
        let response = format!(
            r#"<tool_call>{{"name":"echo","arguments":{{"message":"{}"}}}}</tool_call>"#,
            large_arg
        );
        let (_text, calls) = parse_tool_calls(&response);
        assert_eq!(calls.len(), 1, "large arguments should still parse");
        assert_eq!(calls[0].name, "echo");
    }

    #[test]
    fn parse_tool_calls_special_characters_in_arguments() {
        let response = r#"<tool_call>{"name":"echo","arguments":{"message":"hello \"world\" <>&'\n\t"}}</tool_call>"#;
        let (_text, calls) = parse_tool_calls(response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "echo");
    }

    #[test]
    fn parse_tool_calls_text_with_embedded_json_not_extracted() {
        // Raw JSON without any tags should NOT be extracted as a tool call
        let response = r#"Here is some data: {"name":"echo","arguments":{"message":"hi"}} end."#;
        let (_text, calls) = parse_tool_calls(response);
        assert!(
            calls.is_empty(),
            "raw JSON in text without tags should not be extracted"
        );
    }

    #[test]
    fn parse_tool_calls_multiple_formats_mixed() {
        // Mix of text and properly tagged tool call
        let response = r#"I'll help you with that.

<tool_call>
{"name":"shell","arguments":{"command":"echo hello"}}
</tool_call>

Let me check the result."#;
        let (text, calls) = parse_tool_calls(response);
        assert_eq!(
            calls.len(),
            1,
            "should extract one tool call from mixed content"
        );
        assert_eq!(calls[0].name, "shell");
        assert!(
            text.contains("help you"),
            "text before tool call should be preserved"
        );
    }

    #[test]
    fn parse_tool_calls_cross_alias_close_tag_with_json() {
        // <tool_call> opened but closed with </invoke> — JSON body
        let input = r#"<tool_call>{"name": "shell", "arguments": {"command": "ls"}}</invoke>"#;
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "ls");
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_cross_alias_close_tag_with_glm_shortened() {
        // <tool_call>shell>uname -a</invoke> — GLM shortened inside cross-alias tags
        let input = "<tool_call>shell>uname -a</invoke>";
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "uname -a");
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_glm_shortened_body_in_matched_tags() {
        // <tool_call>shell>pwd</tool_call> — GLM shortened in matched tags
        let input = "<tool_call>shell>pwd</tool_call>";
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "pwd");
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_glm_yaml_style_in_tags() {
        // <tool_call>shell>\ncommand: date\napproved: true</invoke>
        let input = "<tool_call>shell>\ncommand: date\napproved: true</invoke>";
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "date");
        assert_eq!(calls[0].arguments["approved"], true);
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_attribute_style_in_tags() {
        // <tool_call>shell command="date" /></tool_call>
        let input = r#"<tool_call>shell command="date" /></tool_call>"#;
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "date");
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_file_read_shortened_in_cross_alias() {
        // <tool_call>file_read path=".env" /></invoke>
        let input = r#"<tool_call>file_read path=".env" /></invoke>"#;
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "file_read");
        assert_eq!(calls[0].arguments["path"], ".env");
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_unclosed_glm_shortened_no_close_tag() {
        // <tool_call>shell>ls -la (no close tag at all)
        let input = "<tool_call>shell>ls -la";
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "ls -la");
        assert!(text.is_empty());
    }

    #[test]
    fn parse_tool_calls_text_before_cross_alias() {
        // Text before and after cross-alias tool call
        let input = "Let me check that.\n<tool_call>shell>uname -a</invoke>\nDone.";
        let (text, calls) = parse_tool_calls(input);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments["command"], "uname -a");
        assert!(text.contains("Let me check that."));
        assert!(text.contains("Done."));
    }

    #[test]
    fn parse_glm_shortened_body_url_to_curl() {
        // URL values for shell should be wrapped in curl
        let call = parse_glm_shortened_body("shell>https://example.com/api").unwrap();
        assert_eq!(call.name, "shell");
        let cmd = call.arguments["command"].as_str().unwrap();
        assert!(cmd.contains("curl"));
        assert!(cmd.contains("example.com"));
    }

    #[test]
    fn parse_glm_shortened_body_browser_open_preserves_url() {
        let call = parse_glm_shortened_body("browser_open>https://example.com").unwrap();
        assert_eq!(call.name, "browser_open");
        assert_eq!(
            call.arguments,
            serde_json::json!({"url": "https://example.com"})
        );
    }

    #[test]
    fn parse_glm_shortened_body_memory_recall() {
        // memory_recall>some query — default param is "query"
        let call = parse_glm_shortened_body("memory_recall>recent meetings").unwrap();
        assert_eq!(call.name, "memory_recall");
        assert_eq!(call.arguments["query"], "recent meetings");
    }

    #[test]
    fn parse_glm_shortened_body_function_style_preserves_name() {
        let call =
            parse_glm_shortened_body(r#"sendmessage(channel="alerts", message="hi")"#).unwrap();
        assert_eq!(call.name, "sendmessage");
        assert_eq!(call.arguments["channel"], "alerts");
        assert_eq!(call.arguments["message"], "hi");
    }

    #[test]
    fn parse_glm_shortened_body_rejects_empty() {
        assert!(parse_glm_shortened_body("").is_none());
        assert!(parse_glm_shortened_body("   ").is_none());
    }

    #[test]
    fn parse_glm_shortened_body_rejects_invalid_tool_name() {
        // Tool names with special characters should be rejected
        assert!(parse_glm_shortened_body("not-a-tool>value").is_none());
        assert!(parse_glm_shortened_body("tool name>value").is_none());
    }

    #[test]
    fn build_native_assistant_history_from_parsed_calls_includes_reasoning_content() {
        let calls = vec![ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "pwd"}),
            tool_call_id: Some("call_2".into()),
        }];
        let result = build_native_assistant_history_from_parsed_calls(
            "answer",
            &calls,
            Some("deep thought"),
        );
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(result.as_deref().unwrap()).unwrap();
        assert_eq!(parsed["content"].as_str(), Some("answer"));
        assert_eq!(parsed["reasoning_content"].as_str(), Some("deep thought"));
        assert!(parsed["tool_calls"].is_array());
    }

    #[test]
    fn build_native_assistant_history_from_parsed_calls_omits_reasoning_content_when_none() {
        let calls = vec![ParsedToolCall {
            name: "shell".into(),
            arguments: serde_json::json!({"command": "pwd"}),
            tool_call_id: Some("call_2".into()),
        }];
        let result = build_native_assistant_history_from_parsed_calls("answer", &calls, None);
        assert!(result.is_some());
        let parsed: serde_json::Value = serde_json::from_str(result.as_deref().unwrap()).unwrap();
        assert_eq!(parsed["content"].as_str(), Some("answer"));
        assert!(parsed.get("reasoning_content").is_none());
    }

    // ═══════════════════════════════════════════════════════════════════════

    // ═══════════════════════════════════════════════════════════════════════
    // Additional parser internals tests (moved from zeroclaw-runtime to keep
    // functions crate-private per Beta-tier API stability policy)
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn parse_tool_call_value_handles_missing_name_field() {
        let value = serde_json::json!({"function": {"arguments": {}}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_none());
    }

    #[test]
    fn parse_tool_call_value_handles_top_level_name() {
        let value = serde_json::json!({"name": "test_tool", "arguments": {}});
        let result = parse_tool_call_value(&value);
        assert!(result.is_some());
        assert_eq!(result.unwrap().name, "test_tool");
    }

    #[test]
    fn parse_tool_call_value_accepts_top_level_parameters_alias() {
        let value = serde_json::json!({
            "name": "schedule",
            "parameters": {"action": "create", "message": "test"}
        });
        let result = parse_tool_call_value(&value).expect("tool call should parse");
        assert_eq!(result.name, "schedule");
        assert_eq!(
            result.arguments.get("action").and_then(|v| v.as_str()),
            Some("create")
        );
    }

    #[test]
    fn parse_tool_call_value_accepts_function_parameters_alias() {
        let value = serde_json::json!({
            "function": {
                "name": "shell",
                "parameters": {"command": "date"}
            }
        });
        let result = parse_tool_call_value(&value).expect("tool call should parse");
        assert_eq!(result.name, "shell");
        assert_eq!(
            result.arguments.get("command").and_then(|v| v.as_str()),
            Some("date")
        );
    }

    #[test]
    fn parse_tool_call_value_preserves_tool_call_id_aliases() {
        let value = serde_json::json!({
            "call_id": "legacy_1",
            "function": {
                "name": "shell",
                "arguments": {"command": "date"}
            }
        });
        let result = parse_tool_call_value(&value).expect("tool call should parse");
        assert_eq!(result.tool_call_id.as_deref(), Some("legacy_1"));
    }

    #[test]
    fn extract_json_values_handles_empty_string() {
        let result = extract_json_values("");
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_whitespace_only() {
        let result = extract_json_values(
            "   
	  ",
        );
        assert!(result.is_empty());
    }

    #[test]
    fn extract_json_values_handles_multiple_objects() {
        let input = r#"{"a": 1}{"b": 2}{"c": 3}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn extract_json_values_handles_arrays() {
        let input = r#"[1, 2, 3]{"key": "value"}"#;
        let result = extract_json_values(input);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn tool_alias_format_parity() {
        let arguments = serde_json::json!({
            "path": "example.txt",
            "content": "{\"keep\":\"a string\"}",
            "options": {"enabled": true},
        });
        // Vendored: alias table trimmed to identity — names pass through
        // unchanged (dotted prefixes are stripped separately below).
        for (raw, expected) in [
            ("readfile", "readfile"),
            ("bash", "bash"),
            ("send_message", "send_message"),
            ("file_read", "file_read"),
            ("custom_tool", "custom_tool"),
            ("Bash", "Bash"),
        ] {
            let bodies = [
                format!("<tool_call><{raw}>{arguments}</{raw}></tool_call>"),
                format!("<invoke name=\"{raw}\">{arguments}</invoke>"),
                format!("```tool {raw}\n{arguments}\n```"),
                format!(r#"<tool_call>{{"name":"{raw}","arguments":{arguments}}}</tool_call>"#),
            ];
            for body in bodies {
                let response = format!("Before\n{body}\nAfter");
                let (text, calls) = parse_tool_calls(&response);
                assert_eq!(text, "Before\nAfter", "{response}");
                assert_eq!(calls.len(), 1, "{response}");
                assert_eq!(calls[0].name, expected, "{response}");
                assert_eq!(calls[0].arguments, arguments, "{response}");
                assert_eq!(calls[0].tool_call_id, None, "{response}");
            }
        }
    }

    #[test]
    fn tool_alias_format_parity_for_dotted_minimax_names() {
        for (raw, expected) in [
            ("default_api.readfile", "readfile"),
            ("tools.bash", "bash"),
            ("tools.custom_tool", "custom_tool"),
            ("tools.Bash", "Bash"),
        ] {
            let json = format!(r#"{{"name":"{raw}","arguments":{{"path":"example.txt"}}}}"#);
            let minimax = format!(
                "<invoke name=\"{raw}\"><parameter name=\"path\">example.txt</parameter></invoke>"
            );
            let (_, json_calls) = parse_tool_calls(&json);
            let (_, minimax_calls) = parse_tool_calls(&minimax);
            assert_eq!(json_calls.len(), 1, "{json}");
            assert_eq!(minimax_calls.len(), 1, "{minimax}");
            assert_eq!(minimax_calls[0].name, expected);
            assert_eq!(minimax_calls[0].name, json_calls[0].name);
            assert_eq!(minimax_calls[0].arguments, json_calls[0].arguments);
        }
    }

    #[test]
    fn map_tool_name_alias_direct_coverage() {
        // Vendored: identity mapping — no tool-name aliases.
        assert_eq!(map_tool_name_alias("bash"), "bash");
        assert_eq!(map_tool_name_alias("filelist"), "filelist");
        assert_eq!(map_tool_name_alias("memorystore"), "memorystore");
        assert_eq!(map_tool_name_alias("memoryforget"), "memoryforget");
        assert_eq!(map_tool_name_alias("http"), "http");
        assert_eq!(map_tool_name_alias("get_time"), "get_time");
        assert_eq!(map_tool_name_alias("open_app"), "open_app");
        assert_eq!(
            map_tool_name_alias("totally_unknown_tool"),
            "totally_unknown_tool"
        );
    }

    #[test]
    fn map_tool_name_alias_strips_dotted_namespaces() {
        // Gemini-style static prefixes still work.
        assert_eq!(map_tool_name_alias("default_api.file_read"), "file_read");
        assert_eq!(map_tool_name_alias("tools.shell"), "shell");

        // MCP-server-name prefixes (Gemini-via-OpenRouter also emits these
        // when the tool originates from an MCP server; the registry is
        // indexed by bare tool name, so we must strip them too).
        assert_eq!(
            map_tool_name_alias("google_workspace.search_gmail_messages"),
            "search_gmail_messages"
        );

        // Only the final segment is kept even with multiple dots.
        assert_eq!(map_tool_name_alias("a.b.c.final"), "final");

        // Stripped segment is returned as-is (identity mapping).
        assert_eq!(map_tool_name_alias("default_api.bash"), "bash");

        // Names without any dot are unaffected.
        assert_eq!(map_tool_name_alias("file_read"), "file_read");
    }

    #[test]
    fn default_param_for_tool_coverage() {
        assert_eq!(default_param_for_tool("shell"), "command");
        assert_eq!(default_param_for_tool("bash"), "command");
        assert_eq!(default_param_for_tool("file_read"), "path");
        assert_eq!(default_param_for_tool("memory_recall"), "query");
        assert_eq!(default_param_for_tool("memory_store"), "content");
        assert_eq!(default_param_for_tool("web_search_tool"), "query");
        assert_eq!(default_param_for_tool("web_search"), "query");
        assert_eq!(default_param_for_tool("search"), "query");
        assert_eq!(default_param_for_tool("http_request"), "url");
        assert_eq!(default_param_for_tool("browser_open"), "url");
        assert_eq!(default_param_for_tool("unknown_tool"), "input");
    }

    #[test]
    fn strip_trailing_terminal_markers_basic() {
        assert_eq!(strip_trailing_terminal_markers("Summary<eom>"), "Summary");
        assert_eq!(strip_trailing_terminal_markers("Summary<|eom|>"), "Summary");
    }

    #[test]
    fn strip_trailing_terminal_markers_preserves_unmarked_whitespace() {
        // No marker present: trailing whitespace is ordinary text and must be
        // preserved, matching the streaming path which never trims whitespace
        // that is not part of a marker suffix.
        assert_eq!(strip_trailing_terminal_markers("Answer\n"), "Answer\n");
        assert_eq!(strip_trailing_terminal_markers("Answer  "), "Answer  ");
        assert_eq!(
            strip_trailing_terminal_markers("Plain response\n\n"),
            "Plain response\n\n"
        );
    }

    #[test]
    fn strip_trailing_terminal_markers_whitespace_before_marker() {
        // Whitespace BEFORE a recognized marker belongs to the response text
        // and is preserved, matching the streaming stripper ("Answer\n<eom>"
        // streams as "Answer\n"). Only the marker itself (plus any whitespace
        // that followed it) is removed.
        assert_eq!(
            strip_trailing_terminal_markers("Summary  <eom>"),
            "Summary  "
        );
        assert_eq!(
            strip_trailing_terminal_markers("Summary \n\t<|eom|>"),
            "Summary \n\t"
        );
    }

    #[test]
    fn strip_trailing_terminal_markers_stacked() {
        assert_eq!(
            strip_trailing_terminal_markers("Summary<eom><|eom|>"),
            "Summary"
        );
        assert_eq!(
            strip_trailing_terminal_markers("Summary<|eom|><eom>"),
            "Summary"
        );
    }

    #[test]
    fn strip_trailing_terminal_markers_with_whitespace() {
        assert_eq!(
            strip_trailing_terminal_markers("Summary<eom>  \n"),
            "Summary"
        );
        assert_eq!(
            strip_trailing_terminal_markers("Summary<eom>\t\n  "),
            "Summary"
        );
        assert_eq!(
            strip_trailing_terminal_markers("Summary<eom>           <|eom|>"),
            "Summary"
        );
    }

    #[test]
    fn strip_trailing_terminal_markers_preserves_inline() {
        assert_eq!(
            strip_trailing_terminal_markers("Text with <eom> inline"),
            "Text with <eom> inline"
        );
        assert_eq!(
            strip_trailing_terminal_markers("Code: <|eom|> here"),
            "Code: <|eom|> here"
        );
    }

    #[test]
    fn strip_trailing_terminal_markers_empty() {
        assert_eq!(strip_trailing_terminal_markers(""), "");
        // Pure whitespace with no marker is ordinary text and is preserved,
        // matching the streaming path (which never trims unmarked whitespace).
        assert_eq!(strip_trailing_terminal_markers("   "), "   ");
        assert_eq!(strip_trailing_terminal_markers("<eom>"), "");
    }

    #[test]
    fn strip_trailing_terminal_markers_marker_only_with_whitespace() {
        assert_eq!(strip_trailing_terminal_markers("<eom>\n"), "");
        assert_eq!(strip_trailing_terminal_markers("<|eom|>  "), "");
        assert_eq!(strip_trailing_terminal_markers("<eom>\n<|eom|>"), "");
    }

    // ---- Vendoring-specific tests (our ports/extensions, not upstream) ----

    /// Think-tag literals spelled with unicode escapes so the test source
    /// itself contains no raw tag text (transmission-safe under tooling
    /// that treats tag-like spans specially).
    const T_OPEN: &str = "\u{3c}think\u{3e}";
    const T_CLOSE: &str = "\u{3c}/think\u{3e}";

    #[test]
    fn strip_think_tags_removes_standard_think_span() {
        // Direct coverage of our ported strip_think_tags.
        assert_eq!(strip_think_tags(&format!("{}reasoning{}final answer", T_OPEN, T_CLOSE)), "final answer");
        assert_eq!(strip_think_tags(&format!("{}reasoning only", T_OPEN)), "");
        assert_eq!(
            strip_think_tags(&format!("before{}mid{}after", T_OPEN, T_CLOSE)),
            "beforeafter"
        );
        // Unclosed opening tag suppresses the remainder so partial
        // reasoning never becomes final output.
        assert_eq!(strip_think_tags(&format!("{}answer", T_OPEN)), "");
    }

    #[test]
    fn strip_think_tags_removes_lfm3_token_spans() {
        // LFM3-1.7B via llama.cpp renders its think block with special-token
        // IDs as literal text: <Token 1437> ... <Token 9550>.
        assert_eq!(
            strip_think_tags("<Token 1437>thinking<Token 9550>spoken answer"),
            "spoken answer"
        );
        assert_eq!(strip_think_tags("<Token 1437>thinking only"), "");
        assert_eq!(strip_think_tags("plain speech"), "plain speech");
        // Both forms in one response still end up clean.
        assert_eq!(
            strip_think_tags("<Token 1437>deep<Token 9550>hey there"),
            "hey there"
        );
    }

    #[test]
    fn strip_trailing_terminal_markers_removes_eot_id_extension() {
        // Our TERMINAL_MARKERS extension: llama.cpp leaks <|eot_id|>.
        assert_eq!(strip_trailing_terminal_markers("Summary<|eot_id|>"), "Summary");
        // Whitespace before the marker belongs to the response text and is
        // preserved (documented policy, shared with the streaming stripper).
        assert_eq!(strip_trailing_terminal_markers("Summary <|eot_id|>\n"), "Summary ");
        assert_eq!(strip_trailing_terminal_markers("Summary<eom><|eot_id|>"), "Summary");
        assert_eq!(strip_trailing_terminal_markers("<|eot_id|>"), "");
        // Mid-text markers stay untouched, matching upstream behavior.
        assert_eq!(
            strip_trailing_terminal_markers("a <|eot_id|> b"),
            "a <|eot_id|> b"
        );
    }

    /* ---- streaming hold-back filter (our addition) ---- */

    #[test]
    fn stream_filter_passes_plain_prose_immediately() {
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("Hello there, "), "Hello there, ");
        assert_eq!(f.feed("friend."), "friend.");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn stream_filter_suppresses_a_complete_tool_call_envelope() {
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("Sure. "), "Sure. ");
        assert_eq!(f.feed(&format!("{}{{\"name\"", TOOL_CALL_OPEN)), "");
        assert_eq!(f.feed(":\"get_time\",\"arguments\":{}}}"), "");
        assert_eq!(f.feed(&format!("{} done", TOOL_CALL_CLOSE)), " done");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn stream_filter_suppresses_think_spans_on_the_stream() {
        let mut f = StreamFilter::new();
        assert_eq!(
            f.feed(&format!("{}reasoning{}", THINK_OPEN, THINK_CLOSE)),
            ""
        );
        assert_eq!(f.feed("answer"), "answer");
        assert_eq!(f.feed(&format!("{}hidden", LFM3_THINK_OPEN)), "");
        assert_eq!(f.feed(&format!("{}shown", LFM3_THINK_CLOSE)), "shown");
    }

    #[test]
    fn stream_filter_drops_terminal_markers_across_chunks() {
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("Bye\x3c|eo"), "Bye");
        assert_eq!(f.feed("t_id|>"), "");
        assert_eq!(f.feed(" next"), " next");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn stream_filter_holds_partial_opener_then_completes_span() {
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("wait for it\x3ctoo"), "wait for it");
        assert_eq!(f.feed("l_call>x"), "");
        assert_eq!(f.finish(), "");
    }

    #[test]
    fn stream_filter_finish_releases_partial_tail_and_drops_open_span() {
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("text\x3ctoo"), "text");
        assert_eq!(f.finish(), "\x3ctoo");
        let mut g = StreamFilter::new();
        assert_eq!(g.feed(&format!("{}{{\"a\"", TOOL_CALL_OPEN)), "");
        assert_eq!(g.finish(), "");
    }

    #[test]
    fn stream_filter_suppresses_stray_tool_result_blocks() {
        let mut f = StreamFilter::new();
        let block = format!(
            "{}{}{}",
            TOOL_RESULT_OPEN,
            "{\"name\": \"get_time\", \"status\": \"ok\", \"result\": \"now\"}",
            TOOL_RESULT_CLOSE
        );
        let mut released = String::new();
        released.push_str(&f.feed(&format!("ok {block} bye")));
        released.push_str(&f.finish());
        assert_eq!(released, "ok  bye");
    }

    #[test]
    fn stream_filter_envelope_spanning_char_by_char_never_leaks() {
        let mut f = StreamFilter::new();
        let args = "{\"name\":\"get_time\",\"arguments\":{\"x\":1}}";
        let full = format!("A: {}{}{} well done", TOOL_CALL_OPEN, args, TOOL_CALL_CLOSE);
        let mut released = String::new();
        for c in full.chars() {
            released.push_str(&f.feed(&c.to_string()));
        }
        released.push_str(&f.finish());
        assert_eq!(released, "A:  well done");
    }

    /* ---- LFM3 function-attribute bodies (our addition) ---- */

    #[test]
    fn parse_tool_calls_accepts_lfm3_parameterless_function_attr_body() {
        // The exact shape LFM3-1.7B emitted in the Phase 5 e2e dump: the
        // tool-call wrapper with an attribute-style, body-less function
        // element.
        let raw = format!(
            "{}\n{}\n{}\n{}\n",
            TOOL_CALL_OPEN,
            "\u{3c}function=get_time\u{3e}",
            "\u{3c}/function\u{3e}",
            TOOL_CALL_CLOSE
        );
        let (_cleaned, calls) = parse_tool_calls(&raw);
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(calls[0].name, "get_time");
        assert_eq!(calls[0].arguments, serde_json::json!({}));
    }

    #[test]
    fn parse_tool_calls_accepts_lfm3_attributed_function_body_with_params() {
        let raw = format!(
            "{}\n{}\n{}\n{}\n{}\n",
            TOOL_CALL_OPEN,
            "\u{3c}function=calculate\u{3e}",
            "\u{3c}parameter=expression\u{3e}2+2\u{3c}/parameter\u{3e}",
            "\u{3c}/function\u{3e}",
            TOOL_CALL_CLOSE
        );
        let (_cleaned, calls) = parse_tool_calls(&raw);
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(calls[0].name, "calculate");
        assert_eq!(
            calls[0].arguments,
            serde_json::json!({"expression": "2+2"})
        );
    }

    #[test]
    fn parse_function_attr_calls_accepts_minimax_parameter_children() {
        let body = format!(
            "\u{3c}function=open_app\u{3e}\n\u{3c}parameter name=\"app\"\u{3e}notepad\u{3c}/parameter\u{3e}\n\u{3c}/function\u{3e}"
        );
        let calls = parse_function_attr_calls(&body).expect("attributed call must parse");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "open_app");
        assert_eq!(calls[0].arguments, serde_json::json!({"app": "notepad"}));
    }

    #[test]
    fn parse_function_attr_params_decode_json_children() {
        // calculate speaks upstream's {function, values} schema; the
        // parameter children arrive as text and must become real JSON
        // (an array for values, a string for the function name).
        let body = format!(
            "\u{3c}function=calculate\u{3e}\n\u{3c}parameter=function\u{3e}add\u{3c}/parameter\u{3e}\n\u{3c}parameter=values\u{3e}[2, 2]\u{3c}/parameter\u{3e}\n\u{3c}/function\u{3e}"
        );
        let calls = parse_function_attr_calls(&body).expect("call must parse");
        assert_eq!(calls[0].arguments,
            serde_json::json!({"function": "add", "values": [2, 2]}));
    }

    #[test]
    fn parse_tool_calls_returns_every_lfm3_envelope_for_loop_dedup() {
        let one = format!(
            "{}\n{}\n{}\n{}\n",
            TOOL_CALL_OPEN,
            "\u{3c}function=get_time\u{3e}",
            "\u{3c}/function\u{3e}",
            TOOL_CALL_CLOSE
        );
        let raw = format!("{one}{one}{one}");
        let (_cleaned, calls) = parse_tool_calls(&raw);
        assert_eq!(calls.len(), 3, "calls: {calls:?}");
        for c in &calls {
            assert_eq!(c.name, "get_time");
        }
    }

    #[test]
    fn stream_filter_drops_stray_close_tags_outside_any_span() {
        // A think span opened in an earlier round was already dropped by
        // that round's finish(); its close tag still shows up later and
        // must not reach the voice or the terminal.
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("Time is 3 PM"), "Time is 3 PM");
        let mut released = String::new();
        released.push_str(&f.feed(&format!(" now{}", THINK_CLOSE)));
        released.push_str(&f.feed(" more"));
        released.push_str(&f.finish());
        assert_eq!(released, " now more");
    }

    #[test]
    fn stream_filter_drops_stray_close_split_across_chunks() {
        let mut f = StreamFilter::new();
        // The partial close tail is held across the boundary, then the
        // completed close is dropped, not released.
        assert_eq!(f.feed(&format!("ok\x3c/thi")), "ok");
        let mut released = String::new();
        released.push_str(&f.feed(&format!("nk\x3e done{}", TOOL_CALL_CLOSE)));
        released.push_str(&f.finish());
        assert_eq!(released, " done");
    }

    #[test]
    fn stream_filter_finish_releases_partial_close_tail_verbatim() {
        let mut f = StreamFilter::new();
        assert_eq!(f.feed("bye\x3c/th"), "bye");
        assert_eq!(f.finish(), "\x3c/th");
    }

    /* ================================================================ */
    /* LFM3 native DSML envelope: parser + stream filter + detect        */
    /* ================================================================ */

    /// The e2e-observed DSML body (2026-11-14): JSON with a schema-shaped
    /// "properties" wrapper around the real arguments. The parser stays
    /// honest — the wrapper passes through as-is, dispatch's error result
    /// lets the model self-heal.
    fn dsml_e2e_body() -> String {
        serde_json::json!({
            "name": "media_key",
            "arguments": {"properties": {"key": "play_pause"}}
        })
        .to_string()
    }

    #[test]
    fn dsml_e2e_echo_is_ignored_by_parser_and_held_by_filter() {
        /* the exact post-success echo shape observed live 2026-11-14:
         * schema-confused body after a successful round. Policy: never
         * dispatched, held from the voice, no corrective round spent. */
        let raw = format!(
            "Play/pause has been pressed.\n\n{}function_calls>\n{}\n{}Play/pause was pressed. What would you like to do next?",
            DSML_OPEN,
            dsml_e2e_body(),
            DSML_CLOSE
        );
        let (_text, calls) = parse_tool_calls(&raw);
        assert!(calls.is_empty(), "calls: {calls:?}");
        assert!(detect_tool_call_parse_issue(&raw, &calls).is_none());

        /* the filter must hold the envelope while releasing the prose */
        let mut f = StreamFilter::new();
        let mut released = String::new();
        let mut start = 0;
        for (n, (idx, _)) in raw.char_indices().enumerate() {
            if n % 3 == 0 && idx > start {
                released.push_str(&f.feed(&raw[start..idx]));
                start = idx;
            }
        }
        released.push_str(&f.feed(&raw[start..]));
        released.push_str(&f.finish());
        assert!(
            released.contains("Play/pause has been pressed."),
            "{released}"
        );
        assert!(
            released.contains("What would you like to do next?"),
            "{released}"
        );
        assert!(!released.contains("DSML"), "{released}");
        assert!(!released.contains("properties"), "{released}");
        assert!(!released.contains("function_calls"), "{released}");
    }

    #[test]
    fn dsml_envelope_is_never_a_parse_family() {
        /* policy pin: the DSML envelope (closed or not) never produces a
         * dispatch — see the policy note on DSML_OPEN */
        let closed = format!(
            "{}function_calls>\n{}\n{}",
            DSML_OPEN,
            dsml_e2e_body(),
            DSML_CLOSE
        );
        let (_t, calls) = parse_tool_calls(&closed);
        assert!(calls.is_empty(), "calls: {calls:?}");
        let unclosed = format!("Sure. {}function_calls>\n{}", DSML_OPEN, dsml_e2e_body());
        let (_t, calls) = parse_tool_calls(&unclosed);
        assert!(calls.is_empty(), "calls: {calls:?}");
    }

    #[test]
    fn stream_filter_holds_dsml_span_across_chunks() {
        let raw = format!(
            "One moment. {}function_calls>\n{}\n{} All done.",
            DSML_OPEN,
            dsml_e2e_body(),
            DSML_CLOSE
        );
        /* feed in ~3-char chunks so tag bytes split across boundaries */
        let mut chunks: Vec<&str> = Vec::new();
        let mut start = 0;
        for (n, (idx, _)) in raw.char_indices().enumerate() {
            if n % 3 == 0 && idx > start {
                chunks.push(&raw[start..idx]);
                start = idx;
            }
        }
        chunks.push(&raw[start..]);
        let mut f = StreamFilter::new();
        let mut released = String::new();
        for c in &chunks {
            released.push_str(&f.feed(c));
        }
        released.push_str(&f.finish());
        assert_eq!(released, "One moment.  All done.");
        assert!(!released.contains("function_calls"), "{released}");
        assert!(!released.contains("media_key"), "{released}");
    }

    #[test]
    fn stream_filter_holdback_survives_multibyte_dsml_prefixes() {
        /* every byte-prefix of the opener must hold back cleanly; prefixes
         * that cut inside the 3-byte fullwidth bar must neither panic nor
         * partially match (they release verbatim — never lose text) */
        let mut ends: Vec<usize> = DSML_OPEN.char_indices().map(|(i, _)| i).collect();
        ends.push(DSML_OPEN.len());
        for e in ends {
            let mut f = StreamFilter::new();
            let input = format!("text {}", &DSML_OPEN[..e]);
            let released = f.feed(&input);
            let expected = if DSML_OPEN.is_char_boundary(e) {
                "text "
            } else {
                input.as_str()
            };
            assert_eq!(released, expected, "e={e}");
        }
    }

    #[test]
    fn stream_filter_finish_drops_unclosed_dsml_span() {
        let mut f = StreamFilter::new();
        let released = f.feed(&format!(
            "Working. {}function_calls>\n{}",
            DSML_OPEN,
            dsml_e2e_body()
        ));
        assert_eq!(released, "Working. ");
        /* an unclosed span's remainder is suppressed at finish */
        let tail = f.finish();
        assert_eq!(tail, "", "tail: {tail}");
    }

    #[test]
    fn detect_flags_dsml_envelope_with_unparsable_body() {
        let raw = format!(
            "{}function_calls>\nnot json at all\n{}",
            DSML_OPEN, DSML_CLOSE
        );
        let (_text, calls) = parse_tool_calls(&raw);
        assert!(calls.is_empty(), "calls: {calls:?}");
        assert!(detect_tool_call_parse_issue(&raw, &calls).is_some());
    }

    #[test]
    fn detect_fires_for_pure_dsml_drift_response() {
        /* the model's whole reply is a native DSML envelope: it meant a
         * call, but the format is not trusted to dispatch — the corrective
         * verdict re-anchors it to the protocol-block format */
        let raw = format!(
            "{}function_calls>\n{}\n{}",
            DSML_OPEN,
            dsml_e2e_body(),
            DSML_CLOSE
        );
        let (_text, calls) = parse_tool_calls(&raw);
        assert!(calls.is_empty(), "calls: {calls:?}");
        assert!(detect_tool_call_parse_issue(&raw, &calls).is_some());
    }

    #[test]
    fn detect_stays_silent_for_prose_wrapped_dsml_echo() {
        /* post-success echo wrapped in an actual answer: the prose is the
         * reply, the echo is filter-held noise — no corrective round */
        let raw = format!(
            "Play/pause has been pressed.\n\n{}function_calls>\n{}\n{}Play/pause was pressed. What would you like to do next?",
            DSML_OPEN,
            dsml_e2e_body(),
            DSML_CLOSE
        );
        let (_text, calls) = parse_tool_calls(&raw);
        assert!(calls.is_empty(), "calls: {calls:?}");
        assert!(detect_tool_call_parse_issue(&raw, &calls).is_none());
    }
