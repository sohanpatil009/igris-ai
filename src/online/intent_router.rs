/// Intent Router — lightweight LLM call for intent classification.
use crate::online::task_planner::{parse_llm_response, TaskPlan};

pub async fn route_intent(command: &str) -> Option<TaskPlan> {
    let system_prompt = intent_router_prompt();
    let timeout = std::time::Duration::from_secs(15);

    let fut = crate::online::reason_online_fast(&system_prompt, command);
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(output)) => {
            let output = output.trim();
            if output.is_empty() {
                println!("[IntentRouter] Empty response");
                return None;
            }
            println!("[IntentRouter] Raw: {}", &output[..output.len().min(300)]);
            match parse_llm_response(output) {
                Ok(plan) => {
                    println!("[IntentRouter] Parsed: {} step(s)", plan.steps.len());
                    Some(plan)
                }
                Err(e) => {
                    println!("[IntentRouter] Parse error: {}", e);
                    None
                }
            }
        }
        Ok(Err(e)) => {
            println!("[IntentRouter] Error: {}", e);
            None
        }
        Err(_) => {
            println!("[IntentRouter] Timed out after 15s");
            None
        }
    }
}

fn intent_router_prompt() -> String {
    let prompt = r#"You are an intent router. Classify the user's command and output the corresponding tool call(s) as JSON ONLY.

Available tools (use EXACT registry names; never invent names):
1. app_control {"action":"launch|close|focus|list","app":"name"} — control applications (launch allowlist: chrome, firefox, edge, vscode, notepad, calc, mspaint, spotify, discord, taskmgr, etc.)
2. browser_control {"action":"open_url|search","url":"https://...","query":"..."} — open a URL or search the web in the default browser (http/https only)
3. window_control {"action":"minimize|maximize|restore|snap_left|snap_right|move","title":"substring","process":"name","x":0,"y":0} — window management
4. volume_control {"action":"get|set|up|down|mute|unmute","level":0-100} — volume
5. media_control {"action":"play_pause|next|previous|stop"} — media playback
6. screenshot {} — capture screen to temp and return path
7. clipboard_read {} — read clipboard (max 32 KB)
8. clipboard_write {"text":"..."} — write text to clipboard
9. file_search {"query":"...","root":"optional"} — search files by name/content under sandbox roots
10. file_trash {"path":"..."} — move a file to the recycle bin (approval required)
11. power_control {"action":"lock|sleep|shutdown|restart"} — power (approval required)
12. brightness_control {"action":"get|set","level":0-100} — display brightness
13. input_automation {"action":"type_text|press_keys|click","text":"...","keys":"allowed-sequence","x":0,"y":0} — type / keys / click (approval required)
14. system_info {"info":"os|memory|cpu|ip|uptime|all"} — system information
15. web_search {"query":"..."} — open a Google web search
16. read_file {"path":"..."} — read a text file inside the sandbox
17. shell_run {"command":"allowlisted single command"} — run a validated shell command (approval required)
18. general_chat {"response":"..."} — greetings, farewells, casual chat

CRITICAL RULES:
- Use ONLY tool names from the list above; the model has no other tools.
- Never combine open_app + search_web; use browser_control for browser searches.
- For weather → general_chat with a helpful reply (no weather tool yet).
- "search youtube for rust tutorials" → {"tool":"browser_control","args":{"action":"open_url","url":"https://www.youtube.com/results?search_query=rust+tutorials"}}
- For jokes/facts/greetings → general_chat.
- Never ask clarifying questions; use best judgment.
- Output ONLY valid JSON. No markdown, no explanation, no extra text.
- Single action: {"tool":"tool_name","args":{...}}
- Multiple actions: [{"tool":"tool_name","args":{...}}, ...]"#;

    prompt.to_string()
}
