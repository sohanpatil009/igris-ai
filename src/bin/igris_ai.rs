//! Standalone IGRIS AI text/voice assistant (Phase 0 skeleton).
//!
//! This binary keeps `igris-ai` as a *separate* app. It does not integrate with
//! any other repository in `major_project`.

use std::io::{self, Write};

use igris_ai::bridge::{self, HostConfig, Personality, PluginSnapshot};
use igris_ai::core::audio_capture::capture_command;
use igris_ai::core::stt::{init_stt_engine, transcribe_audio};
use igris_ai::core::tts::speak;
use igris_ai::nlu::engine::NluEngine;
use igris_ai::online::{self, task_planner::TaskPlan};
use igris_ai::tools::default_registry;
use igris_ai::tools::{ToolError, ToolErrorKind, ToolRegistry};

fn speaker_id() -> String {
    "051".to_string()
}
fn assistant_name() -> String {
    "IGRIS".to_string()
}
fn personality() -> Personality {
    Personality::Igris
}
fn plugins() -> Vec<PluginSnapshot> {
    Vec::new()
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    bridge::install(
        HostConfig {
            speaker_id,
            assistant_name,
            personality,
        },
        plugins,
    );

    online::init_from_env();

    let mut nlu = NluEngine::new();
    if let Err(e) = nlu.initialize() {
        eprintln!("warning: NLU init failed: {e}");
    }

    println!("IGRIS AI assistant (text mode). Type 'help', 'online', 'offline', 'quit'.");
    let stdin = io::stdin();
    let policy = std::sync::Arc::new(igris_ai::security::PermissionPolicy::load());
    let audit = std::sync::Arc::new(igris_ai::security::AuditLog::load());
    let tools = default_registry(policy.clone(), audit);
    let memory = std::sync::Arc::new(igris_ai::memory::MemoryStore::load());
    let _mcp_client = igris_ai::mcp::McpClient::load().await;
    igris_ai::mcp::register_mcp_tools(&_mcp_client, &tools).await;

    // First-run permission wizard so the user is never prompted mid-command.
    let mut tool_names = tools.names();
    tool_names.sort();
    if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        print!("\n{} tools available.\n", tool_names.len());
        println!(
            "Persisted grants were loaded; use 'grant <tool>' to enable otherwise skipped tools."
        );
        println!("First-run permission setup (Enter = skip, y = grant once-and-persist except destructive tools):");
        for name in &tool_names {
            if tools.is_granted(name) {
                continue;
            }
            print!("  grant '{name}'? [y/N] ");
            let _ = std::io::stdout().flush();
            let mut ans = String::new();
            if std::io::stdin().read_line(&mut ans).is_ok() && ans.trim().eq_ignore_ascii_case("y")
            {
                tools.grant(name);
            }
        }
        println!("Permissions initialized.\n");
    }
    loop {
        let _ = io::stdout().flush();
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => break, // stdin EOF (e.g. piped script): exit instead of spinning
            Ok(_) => {}
            Err(e) => {
                eprintln!("stdin read error ({e}); exiting");
                break;
            }
        }
        if line.trim().is_empty() {
            continue;
        }
        let input = line.trim();
        match input {
            _ if input.starts_with("grant ") => {
                let tool = input.strip_prefix("grant ").unwrap_or("").trim();
                // Route through the registry so aliases normal identically
                // to what `decision` looks up (`grant search_web` ->
                // `web_search`); echo the normalized key that persists.
                tools.grant(tool);
                println!("granted: {}", igris_ai::tools::normalize_tool_name(tool));
            }
            _ if input.starts_with("revoke ") => {
                let tool = input.strip_prefix("revoke ").unwrap_or("").trim();
                tools.revoke(tool);
                println!("revoked: {}", igris_ai::tools::normalize_tool_name(tool));
            }
            "pending" => {
                let items = tools.list_pending();
                if items.is_empty() {
                    println!("no pending approvals");
                }
                for action in items {
                    println!(
                        "{} tool={} args={} created={} expires={}",
                        action.id, action.tool, action.args, action.created_at, action.expires_at
                    );
                }
            }
            _ if input.starts_with("approve ") => {
                let id = input.strip_prefix("approve ").unwrap_or("").trim();
                match tools.approve(id).await {
                    Ok(outcome) => {
                        println!("approved {id} -> {}", outcome.message);
                        let _ = igris_ai::core::tts::speak("Approved and done.");
                    }
                    Err(e) => tool_error_to_console(&e),
                }
            }
            _ if input.starts_with("deny ") => {
                let id = input.strip_prefix("deny ").unwrap_or("").trim();
                if tools.deny(id) {
                    println!("denied: {id}");
                } else {
                    println!("no pending action with id: {id}");
                }
            }
            _ if input.starts_with("run ") => {
                let text = input.strip_prefix("run ").unwrap_or("").trim();
                let should_exit =
                    run_text_command(text, &mut nlu, &tools, Some(memory.as_ref())).await;
                if should_exit {
                    break;
                }
            }
            "voice-loop" => match init_stt_engine() {
                Ok(engine) => {
                    println!("voice loop started. Say 'hello' wake phrase; 'goodbye' exits.");
                    'voice: loop {
                        if let Err(e) = igris_ai::core::wake_word::listen_for_wake_word(&engine) {
                            eprintln!("wake word error: {e}");
                            break 'voice;
                        }
                        match capture_command() {
                            Ok(capture) => match transcribe_audio(&capture.samples, &engine) {
                                Ok(text) if !text.trim().is_empty() => {
                                    println!("heard: {}", text);
                                    let should_exit = run_text_command(
                                        &text,
                                        &mut nlu,
                                        &tools,
                                        Some(memory.as_ref()),
                                    )
                                    .await;
                                    if should_exit {
                                        break 'voice;
                                    }
                                }
                                Ok(_) => {}
                                Err(e) => eprintln!("stt error: {e}"),
                            },
                            Err(e) => eprintln!("capture error: {e}"),
                        }
                    }
                    println!("voice loop stopped");
                }
                Err(e) => eprintln!("stt init error: {e}"),
            },
            _ if input.starts_with("remember ") => {
                let mut fact = input.strip_prefix("remember ").unwrap_or("").trim();
                if let Some(stripped) = fact.strip_prefix("that ") {
                    fact = stripped;
                }
                let id = memory.add_fact(fact);
                println!("remembered: {fact} (id {id})");
            }
            _ if input.starts_with("forget ") => {
                let raw = input.strip_prefix("forget ").unwrap_or("").trim();
                match raw.parse::<u64>() {
                    Ok(id) if memory.forget(id) => println!("forgot: {id}"),
                    Ok(_) => println!("no memory with id: {raw}"),
                    Err(_) => println!("usage: forget <id>"),
                }
            }
            "facts" => {
                let facts = memory.facts();
                if facts.is_empty() {
                    println!("no stored facts");
                } else {
                    for (id, text) in facts {
                        println!("#{id}: {text}");
                    }
                }
            }
            _ if input.starts_with("recall ") => {
                let q = input.strip_prefix("recall ").unwrap_or("").trim();
                let facts = memory.search_facts(q);
                if facts.is_empty() {
                    println!("no matching memories");
                } else {
                    for f in facts {
                        println!("- {f}");
                    }
                }
            }
            "memory" => {
                let recent = memory.recent(5);
                println!("recent turns: {}", recent.len());
                for t in recent {
                    println!("  [{}] {}", t.role, t.text);
                }
            }
            "clear memory" => {
                memory.clear();
                println!("memory cleared");
            }
            "quit" | "exit" => break,
            "online" => {
                online::enable_online_mode();
                println!("online mode: {}", online::is_online_mode());
            }
            "offline" => {
                online::disable_online_mode();
                println!("online mode: {}", online::is_online_mode());
            }
            "help" => {
                println!("Commands: online, offline, speak <text>, reason <text>, nlu <text>, listen, voice-loop, tools, tool <name> [json], pending, approve <id>, deny <id>, grant <tool>, revoke <tool>, remember <fact>, recall <text>, memory, clear memory, quit");
            }
            "listen" => match init_stt_engine() {
                Ok(engine) => {
                    println!("listening for a command... (speak now)");
                    match capture_command() {
                        Ok(capture) => {
                            println!(
                                "captured {} ms, speech_detected={}",
                                capture.duration_ms, capture.speech_detected
                            );
                            match transcribe_audio(&capture.samples, &engine) {
                                Ok(text) if !text.trim().is_empty() => {
                                    println!("heard: {}", text);
                                    match nlu.process_input(&text) {
                                        Ok(res) => println!("nlu: {:?}", res),
                                        Err(e) => eprintln!("nlu error: {e}"),
                                    }
                                }
                                Ok(_) => println!("no speech transcribed"),
                                Err(e) => eprintln!("stt error: {e}"),
                            }
                        }
                        Err(e) => eprintln!("capture error: {e}"),
                    }
                }
                Err(e) => eprintln!("stt init error: {e}"),
            },
            "tools" => {
                println!("tools: {:?}", tools.names());
            }
            _ if input.starts_with("tool ") => {
                let rest = input.strip_prefix("tool ").unwrap_or("").trim();
                let (name, json) = match rest.split_once(' ') {
                    Some((n, j)) => (n.trim(), j.trim()),
                    None => (rest, "{}"),
                };
                let args: serde_json::Value = match serde_json::from_str(json) {
                    Ok(v) => v,
                    Err(e) => {
                        println!("[warn] invalid JSON args ('{e}'); executing with empty object");
                        serde_json::json!({})
                    }
                };
                match tools.execute(name, &args).await {
                    Ok(outcome) => println!("tool result: {}", outcome.message),
                    Err(e) => tool_error_to_console(&e),
                }
            }
            _ if input.starts_with("speak ") => {
                let text = input.strip_prefix("speak ").unwrap_or("").trim();
                if let Err(e) = speak(text) {
                    eprintln!("tts error: {e}");
                }
            }
            _ if input.starts_with("reason ") => {
                let text = input.strip_prefix("reason ").unwrap_or("").trim();
                match online::reason_online(
                    "You are IGRIS, a helpful desktop assistant. Answer concisely.",
                    text,
                )
                .await
                {
                    Ok(answer) => println!("assistant: {answer}"),
                    Err(e) => eprintln!("reasoning error: {e}"),
                }
            }
            _ if input.starts_with("nlu ") => {
                let text = input.strip_prefix("nlu ").unwrap_or("").trim();
                match nlu.process_input(text) {
                    Ok(res) => println!("nlu: {:?}", res),
                    Err(e) => eprintln!("nlu error: {e}"),
                }
            }
            _ => {
                let mem_hits = memory.search_facts(input);
                if !mem_hits.is_empty()
                    && (input.to_lowercase().contains("my ")
                        || input.to_lowercase().starts_with("what"))
                {
                    for h in &mem_hits {
                        println!("- {h}");
                    }
                    let _ = igris_ai::core::tts::speak(&mem_hits.join(". "));
                    continue;
                }
                match nlu.process_input(input) {
                    Ok(res) => println!("nlu: {:?}", res),
                    Err(e) => eprintln!("nlu error: {e}"),
                }
            }
        }
    }
}
async fn run_text_command(
    text: &str,
    nlu: &mut igris_ai::nlu::engine::NluEngine,
    tools: &igris_ai::tools::ToolRegistry,
    memory: Option<&igris_ai::memory::MemoryStore>,
) -> bool {
    if let Some(m) = memory {
        m.add_turn("user", text);
        let hits = m.search_facts(text);
        if !hits.is_empty()
            && (text.to_lowercase().contains("my ") || text.to_lowercase().starts_with("what"))
        {
            for h in &hits {
                println!("- {h}");
            }
            let _ = igris_ai::core::tts::speak(&hits.join(". "));
            return false;
        }
    }

    // Agent loop (plan.md 1.5): online mode first routes through the LLM
    // intent router; an executable plan executes step by step. Offline mode
    // (and chat-shaped router output) falls back to the local NLU engine.
    if igris_ai::online::is_online_mode() {
        if let Some(plan) = igris_ai::online::intent_router::route_intent(text).await {
            if plan_has_executable_step(&plan) {
                println!("plan: {} step(s)", plan.steps.len());
                match execute_plan(&plan, tools).await {
                    Ok(summary) => {
                        println!("plan complete: {summary}");
                        let _ = igris_ai::core::tts::speak(&summary);
                    }
                    Err(e) => {
                        tool_error_to_console(&e);
                        if e.kind == ToolErrorKind::NeedsApproval {
                            let _ = igris_ai::core::tts::speak("I need your approval to continue.");
                        } else {
                            let _ = igris_ai::core::tts::speak("One step failed, so I stopped.");
                        }
                    }
                }
                return false;
            }
        }
    }

    match nlu.process_input(text) {
        Ok(res) => {
            println!("nlu: {} ({:.2})", res.intent_name, res.confidence);
            match res.intent_name.as_str() {
                "open_app" => {
                    let app = res.entities.get("app").cloned().unwrap_or_default();
                    if app.is_empty() {
                        let _ = igris_ai::core::tts::speak("Which app should I open?");
                    } else {
                        match tools
                            .execute(
                                "app_control",
                                &serde_json::json!({ "action": "launch", "app": app }),
                            )
                            .await
                        {
                            Ok(outcome) => {
                                println!("tool: {}", outcome.message);
                                let _ = igris_ai::core::tts::speak(&outcome.message);
                            }
                            Err(e) => {
                                tool_error_to_console(&e);
                                let _ = igris_ai::core::tts::speak("I could not open that app.");
                            }
                        }
                    }
                }
                "web_search" => {
                    match tools
                        .execute(
                            "web_search",
                            &serde_json::json!({ "query": text.to_string() }),
                        )
                        .await
                    {
                        Ok(outcome) => {
                            println!("tool: {}", outcome.message);
                            let _ = igris_ai::core::tts::speak("Searching the web now.");
                        }
                        Err(e) => {
                            tool_error_to_console(&e);
                            let _ = igris_ai::core::tts::speak("Web search is not permitted yet.");
                        }
                    }
                }
                "greeting" => {
                    let _ = igris_ai::core::tts::speak("Hello! How can I help you today?");
                }
                "assistant_control" => {
                    let _ = igris_ai::core::tts::speak("Goodbye. IGRIS AI shutting down.");
                    return true;
                }
                _ => {
                    // Prefer an on-disk memory fact when the query clearly
                    // targets one, so recall works without an LLM round-trip.
                    let mut memory_handled = false;
                    if let Some(m) = memory {
                        let hits = m.search_facts(text);
                        if !hits.is_empty()
                            && (text.to_lowercase().contains("my ")
                                || text.to_lowercase().starts_with("what"))
                        {
                            for h in &hits {
                                println!("- {h}");
                            }
                            let _ = igris_ai::core::tts::speak(&hits.join(". "));
                            memory_handled = true;
                        }
                    }
                    if !memory_handled {
                        if igris_ai::online::is_online_mode() {
                            match igris_ai::online::reason_online_fast(
                                "You are IGRIS, a helpful desktop assistant. Answer concisely.",
                                text,
                            )
                            .await
                            {
                                Ok(answer) => {
                                    println!("assistant: {answer}");
                                    let _ = igris_ai::core::tts::speak(&answer);
                                }
                                Err(e) => {
                                    eprintln!("reasoning error: {e}");
                                    let msg = {
                                        let t = format!("{e}").to_lowercase();
                                        if t.contains("timeout")
                                            || t.contains("network")
                                            || t.contains("connection")
                                            || t.contains("timed out")
                                        {
                                            "I can't reach the network right now."
                                        } else {
                                            "I had trouble thinking about that."
                                        }
                                    };
                                    let _ = igris_ai::core::tts::speak(msg);
                                }
                            }
                        } else {
                            let sys =
                                "You are IGRIS, a helpful desktop assistant. Answer concisely.";
                            let qtext = text.to_string();
                            let local = tokio::task::spawn_blocking(move || {
                                igris_ai::online::local_reasoning::reason_local_once(sys, &qtext)
                            })
                            .await;
                            match local {
                                Ok(Ok(answer)) if !answer.is_empty() => {
                                    println!("assistant: {answer}");
                                    let _ = igris_ai::core::tts::speak(&answer);
                                }
                                _ => {
                                    let _ = igris_ai::core::tts::speak(
                                        "I heard you, but I do not have a local reasoning model loaded yet.",
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        Err(e) => eprintln!("nlu error: {e}"),
    }
    false
}

/// Execute a TaskPlan step by step (plan.md 1.5). Stops on the first tool
/// error; `NeedsApproval` carries a pending id the user can approve via the
/// REPL, then re-issues the command.
async fn execute_plan(plan: &TaskPlan, tools: &ToolRegistry) -> Result<String, ToolError> {
    let total = plan.steps.len();
    let mut completed = 0usize;
    for (index, step) in plan.steps.iter().enumerate() {
        let args: serde_json::Value = match serde_json::from_str(&step.args) {
            Ok(value) => value,
            Err(e) => {
                return Err(ToolError::invalid_args(format!(
                    "step {} ('{}') has invalid args JSON: {e}",
                    index + 1,
                    step.tool
                )));
            }
        };
        println!("step {}/{}: {} {}", index + 1, total, step.tool, step.args);
        match tools.execute(&step.tool, &args).await {
            Ok(outcome) => {
                completed += 1;
                println!("  ok: {}", outcome.message);
                let _ = igris_ai::core::tts::speak(&format!("Step {} done.", index + 1));
            }
            Err(e) => return Err(e),
        }
    }
    Ok(format!("{completed}/{total} step(s) completed."))
}

fn plan_has_executable_step(plan: &TaskPlan) -> bool {
    plan.steps
        .iter()
        .any(|step| !matches!(step.tool.as_str(), "general_chat" | "switch_mode"))
}

fn tool_error_to_console(e: &ToolError) {
    eprintln!("tool error: {e}");
    if e.kind == ToolErrorKind::NeedsApproval {
        if let Some(id) = &e.pending_id {
            println!("-> pending {id}: approve {id} (execute once) or deny {id}; 'grant <tool>' allows permanently; 'pending' lists all");
        }
    }
}
