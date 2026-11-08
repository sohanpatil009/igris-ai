# IGRIS AI — 4-Week Sprint Plan

> Goal: turn `igris-ai` from a basic text REPL with 4 tools into a voice-first Windows assistant that can execute real multi-step work, with a permission model users can trust.

**Status:** Week 1 execution core **complete and verified** (2026-10-31): async tool core + approval flow landed, both reviewer fix-passes applied, `cargo check`/`cargo clippy` clean on touched files, `cargo test` green (62 lib + 8 smoke). Week 1 manual demo gate and Week 2 (Windows control) are next.

**Baseline verified:** `cargo check --bin igris_ai` passes. `pkg/models/{parakeet,kokoro}` are installed. The working tree has uncommitted changes across ~20 files — commit or stash before starting so each week has a clean diff.

---

## 0. Sprint objective

Ship one reliable loop:

```
wake word -> STT -> intent/tool planning -> permission check -> tool execution -> spoken result
```

Success is not "more tools". Success is **12 tools that work every time, with approval before anything destructive.**

### Sprint success metrics

| Metric | Target |
|---|---|
| End-to-end command success on the 20-command suite | >= 18/20 |
| p95 latency, simple command, online mode | < 3 s |
| p95 latency, simple command, offline mode | < 5 s |
| Sensitive actions executed without approval | 0 |
| Offline voice loop works with network unplugged | yes |
| Crash on missing model / missing API key | graceful message, no panic |

---

## 1. Current baseline (what we are building on)

| Area | Current state | File |
|---|---|---|
| Tool trait | Sync, no JSON schema, `Result<String, String>` | `src/tools/mod.rs:11` |
| Registry | 4 tools: `system_info`, `open_app`, `web_search`, `shell_run` | `src/tools/mod.rs:368` |
| Permissions | `Ask` returns an error instead of prompting | `src/tools/mod.rs:86` |
| Policy store | Persistent grants + audit log | `src/security.rs:50` |
| NLU | 8 regex/SBERT intents | `src/nlu/engine.rs:360` |
| Agent loop | `route_intent` + `task_planner` exist but are not called by the binary | `src/online/intent_router.rs:15`, `src/online/task_planner.rs:33` |
| CLI dispatch | Only `open_app`, `web_search`, `greeting`, `assistant_control` are acted on | `src/bin/igris_ai.rs:243` |
| Memory | JSON file, substring fact search | `src/memory.rs:87` |
| Local LLM | None; offline fallback says no model is loaded | `src/bin/igris_ai.rs:303` |
| STT / TTS | Parakeet int8 / Kokoro-82M, both offline | `src/core/stt.rs`, `src/core/tts.rs` |

### Ground rules

- Keep `igris-ai` standalone. Do not integrate with `igris-ecosystem`, `igris-protocol`, or `igris-andriod` this sprint.
- Every tool declares a `Capability`. `ReadOnly` = allow. `Standard` = allow if granted. `Sensitive`/`Admin` = approval required.
- No destructive action without an approval step. No silent shell execution.
- Prefer existing dependencies. New crates need an explicit decision (see section 8).
- Every week ends with a demo gate. If the gate fails, fix it before starting the next week.
- Update `TODO.md` as items land. Do not let two plans drift apart.

---

## Week 1 — Execution core

**Goal:** the assistant can plan and execute a multi-step command, and sensitive tools produce an approval request instead of an error.

### Tasks

- [x] **1.1 Structured tool results.**
  - Replace `Result<String, String>` with a `ToolError` carrying a code: `InvalidArgs`, `Denied`, `NeedsApproval`, `Timeout`, `NotFound`, `Failed`.
  - Add `ToolOutcome { message: String, undo: Option<UndoAction> }` so tools can register an undo step.
  - Migrate the 4 existing tools.
  - **Done 2026-10-31** (`src/tools/mod.rs`): all 6 error kinds + `ToolOutcome`/`UndoAction` live; 5 tools migrated.

- [x] **1.2 Async tool trait.**
  - Use `futures::future::BoxFuture` (already a dependency via `futures = "0.3"`). No new crate.
  - Signature:
    ```rust
    fn execute<'a>(&'a self, args: &'a serde_json::Value)
        -> BoxFuture<'a, Result<ToolOutcome, ToolError>>;
    ```
  - Registry execution becomes `async fn execute(...)`.
  - Run blocking OS calls with `tokio::task::spawn_blocking`.
  - **Done 2026-10-31** — exact `BoxFuture` signature in place; every blocking call (process spawn, file read) wrapped in `spawn_blocking`.

- [x] **1.3 Tool schemas.**
  - Add `fn schema(&self) -> serde_json::Value` returning a JSON Schema object.
  - Validate required fields and primitive types in the registry before dispatch. Hand-rolled validator, no `schemars` dependency.
  - Reject unknown args by default.
  - **Done 2026-10-31** — `validate_args` fail-closed subset (object schemas only; union/unknown property types also rejected with `Some()` match arms; unknown args rejected before type/required errors while required still fails closed in schemas without `properties`).
  
  Round-2 gate additions (same pass): diagnostic order pinned by tests (unknown → type → required); property-level union acceptance closed; `rust-version = "1.87"` pinned (MSRV for `is_multiple_of`); grants temp file process-unique; grants/pending lock-poison handling unified (recover inner state, log the incident).

- [x] **1.4 Real approval flow.**
  - `PermissionDecision::Ask` no longer returns `Err`. It creates a `PendingAction { id, tool, args, created_at, expires_at }` and returns `NeedsApproval`.
  - Add REPL commands: `pending`, `approve <id>`, `deny <id>`.
  - Approval is single-use. `grant <tool>` remains the persistent allow.
  - Audit all three outcomes: `ask`, `approved`, `denied`.
  - **Done 2026-10-31** — take-first `approve` (removal *is* the authorization; approve/deny race can no longer double-run or run a rejected action). Expired pendings pruned on insert/list.

- [x] **1.5 Wire the agent loop into the binary.**
  - New order in `run_text_command`:
    1. Add user turn to memory.
    2. If online: `route_intent(text)`. If it returns a plan, execute it.
    3. Else run `NluEngine`.
    4. Else fall back to chat/reasoning.
  - Add `execute_plan(plan: TaskPlan, tools, policy) -> PlanResult`.
  - Normalize tool names (`search_web` -> `web_search`, `close_app` -> `app_control`). Keep one alias map.
  - Stop the plan on `NeedsApproval`, print the pending id, resume after `approve`.
    - *Amended 2026-10-31:* `approve <id>` executes **that one step** and consumes the pending; the user re-issues the request for the remaining steps (no internal resume queue in v1 — re-issue, don't re-run completed steps silently).
  - Narration: one short spoken sentence per completed step, not per token.
  - **Done 2026-10-31** (`src/bin/igris_ai.rs`): `run_text_command` routes online → `execute_plan` → NLU fallback; alias map in `normalize_tool_name` (case-insensitive; unknown-name errors surface the raw input).

- [x] **1.6 Register `read_file` and add a path sandbox.**
  - `ReadFileTool` exists but is not registered (`src/tools/mod.rs:223` vs `368`).
  - Add `PathPolicy` with allowed roots from `IGRIS_FILE_ROOTS` (fallback: user home + current working dir).
    - *Amended 2026-10-31:* fallback is **current working directory only**. Deliberately narrower than the original wording: `read_file` is auto-allowed (`ReadOnly`), so a home-directory default would expose `%USERPROFILE%\.ssh` and browser credential stores out of the box. Home is opt-in via `IGRIS_FILE_ROOTS`.
  - Canonicalize before checking. Reject `..`, symlink escapes, and paths outside the roots.
  - **Done 2026-10-31** — component-wise (not string-prefix) canonicalized check with the `foo`/`foo2` sibling regression test; all denials share one message (existence oracle closed).

- [x] **1.7 Baseline checks.**
  - `cargo fmt`, `cargo check --bin igris_ai`, `cargo test`.
  - Add a smoke test that builds a registry and executes `system_info`.
  - **Done 2026-10-31** — `cargo check` + `cargo clippy` clean (zero findings on touched files); `cargo test` green: 62 lib + 8 smoke (registry/system_info, sibling-dir sandbox, schema rejections, single-use approval, deny-audit readback, shell metachar/control-char blocklist, alias resolution).

### Week 1 demo gate

Type or say:

```
open notepad and search the web for rust 2024 edition changes
```

Expected: two steps, two tool calls, spoken summary. Then:

```
tool shell_run {"command":"whoami"}
```

Expected: `NeedsApproval` with a pending id; `approve <id>` executes once; re-running requires approval again.

---

## Week 2 — Windows control

**Goal:** IGRIS can control the machine by voice. This is the differentiator against cloud assistants.

**Implementation note:** start with PowerShell/.NET one-liners to avoid new crates. If p95 exceeds target, migrate the hot paths to the `windows` crate in Week 4.

### Tasks

- [x] **2.1 Shared Windows helper.**
  - `src/tools/os/windows.rs` with `run_powershell(script, timeout) -> Result<String, ToolError>`.
  - Flags: `-NoProfile -NonInteractive -ExecutionPolicy Bypass -Command`.
  - Hard timeout with process kill. Capture stdout/stderr separately.

- [x] **2.2 App control** (`app_control`, Standard).
  - `launch`, `close`, `focus`, `list`.
  - Launch with `Start-Process`; focus with `SetForegroundWindow` via `Add-Type`; close with `CloseMainWindow()` first, force only after approval.
  - Delete the current `cmd /C start` path in `open_app`.

- [x] **2.3 Window control** (`window_control`, Standard).
  - `minimize`, `maximize`, `restore`, `snap_left`, `snap_right`, `move`.
  - Target by window title substring or process name.

- [x] **2.4 Volume control** (`volume_control`, Standard).
  - `set <0-100>`, `up`, `down`, `mute`, `unmute`.
  - MVP: media-key events. Upgrade to CoreAudio later if needed.

- [x] **2.5 Power control** (`power_control`, Sensitive).
  - `lock`, `sleep`, `shutdown`, `restart`.
  - Every action requires approval, including lock.
  - Never accept a delay/argument from the model without validation.

- [x] **2.6 Brightness** (`brightness_control`, Standard).
  - WMI `WmiMonitorBrightnessMethods`. Return a clear "not supported on this display" on desktops.

- [x] **2.7 Screenshot** (`screenshot`, ReadOnly).
  - Save to `%TEMP%\igris\screenshot-<timestamp>.png`.
  - Return the path, not the bytes.

- [x] **2.8 Clipboard** (`clipboard_read` ReadOnly, `clipboard_write` Standard).
  - `Get-Clipboard` / `Set-Clipboard`.
  - Cap read size at 32 KB.

- [x] **2.9 File search** (`file_search`, ReadOnly).
  - Walk from allowed roots with a depth and time limit. Skip `node_modules`, `.git`, `target`, `AppData\Local\Temp`.
  - Match name first, content second. Return top 20 with full paths and sizes.

- [x] **2.10 File trash** (`file_trash`, Sensitive).
  - Recycle Bin only, never permanent delete:
    `Microsoft.VisualBasic.FileIO.FileSystem::DeleteFile(..., SendToRecycleBin)`.
  - Approval required. Register an undo entry.

- [x] **2.11 Media control** (`media_control`, Standard).
  - play/pause, next, previous, stop via media keys.

- [x] **2.12 Input automation** (`input_automation`, Sensitive).
  - `type_text`, `press_keys`, `click`.
  - Prefer clipboard-paste for text to avoid keyboard-layout issues.
  - Never allow arbitrary key sequences from the LLM without approval.

- [x] **2.13 Browser control** (`browser_control`, Standard).
  - `open_url`, `search`.
  - Validate `http`/`https` only. No `file://`, `javascript:`, or custom schemes.

- [x] **2.14 Update the intent-router prompt.**
  - Replace the ~25 imaginary tools in `src/online/intent_router.rs:55` with the real registry names and schemas.
  - Keep `general_chat` as the fallback.

- [x] **2.15 Tests.**
  - Unit: schema validation, path sandbox, URL validation, arg parsing.
  - Manual smoke: one command per tool on Windows 11.

### Week 2 demo gate

Run these five, in order, and record latency:

```
open chrome
switch to chrome
put chrome on the left half
mute
take a screenshot
```

Expected: all five succeed; none require a restart; `mute` does not touch the microphone.

---

## Week 3 — Knowledge, local reasoning, dev workflow

**Goal:** answer questions with real sources, work offline, and do useful developer tasks.

### Tasks

- [x] **3.1 Web search v2** (`web_search`, Standard).
  - Use an API, not a browser tab. Preferred: Brave Search (`BRAVE_SEARCH_API_KEY`). Fallback: Tavily (`TAVILY_API_KEY`). Last resort: open the browser.
  - Return top 5 results: title, URL, snippet.
  - Summarize with the LLM and include source URLs in the answer.
  - Respect a 10 s timeout.

- [x] **3.2 Local LLM provider.** (added `LlmProvider::Local` via OpenAI-compatible endpoint; env `IGRIS_LOCAL_LLM_URL`/`IGRIS_LOCAL_LLM_MODEL`; `with_provider` now uses provider-specific model env; `reason_online` falls back local then error; `local_reasoning` updated)
  - Add a local provider using the OpenAI-compatible endpoint `http://127.0.0.1:11434/v1` (Ollama).
  - Env: `IGRIS_LOCAL_LLM_URL`, `IGRIS_LOCAL_LLM_MODEL` (default `qwen2.5:3b-instruct`).
  - Health check `/models` before use. Degrade to online, then to a clear spoken error.
  - Fallback chain: primary online -> local -> error.
  - Fix `OnlineReasoning::with_provider`, which currently reads `NVIDIA_NIM_MODEL` for every provider (`src/online/reasoning.rs:187`). Use a provider-specific model env or the selected model.

- [x] **3.3 Semantic memory.** (JSON + SBERT 384-dim embeddings; cosine-ranked `search_facts`; legacy facts migrated with `.bak`; `forget <id>` + `facts` REPL commands; basic recall hook in REPL)
  - Decision required: `rusqlite` with `bundled` (new dependency, recommended) or keep JSON and store embeddings inline.
  - Tables: `turns`, `facts`, `embeddings`, `reminders`, `routines`.
  - Reuse the existing SBERT 384-dim embeddings. `search_facts` becomes cosine similarity, not substring.
  - Migrate `memory.json` on first run. Keep a backup.
  - Add `forget <id>` and `clear memory` (already in the REPL).

- [x] **3.4 Document reading** (`read_document`, ReadOnly).
  - Start with `.txt`, `.md`, `.csv`, `.json`.
  - PDF: needs a dependency decision (`pdf-extract` or `lopdf`). If not approved, shell out to an installed `pdftotext` and fail gracefully.
  - Cap extracted text (for example 50k chars) and chunk for summarization.
  - Path sandbox applies.

- [x] **3.5 Weather** (`get_weather`, ReadOnly).
  - Open-Meteo geocoding + forecast. No API key.
  - Return temperature, conditions, and rain probability.

- [x] **3.6 Timers and reminders** (`set_timer`, `set_reminder`, Standard).
  - Background `tokio` scheduler.
  - Persist to the memory store so they survive restart.
  - Fire with TTS + toast/notification.

- [x] **3.7 Dev workflow tools.** (implemented in `src/tools/dev.rs`: git_status, git_diff, git_log, git_commit, run_tests, code_search; pure-Rust code search fallback after `rg` proved absent from the Windows PATH)
  - `git_status`, `git_diff`, `git_log` (ReadOnly). `git_commit` (Sensitive, approval).
  - `run_tests` (Sensitive, approval): detect `Cargo.toml`, `package.json`, `pyproject.toml`; run the right command; parse pass/fail counts.
  - `code_search` (ReadOnly): ripgrep wrapper with a result cap.
  - All commands run with an explicit working directory; reject paths outside the sandbox.

- [x] **3.8 Conversation context persistence.**
  - Persist the last N turns and task steps so a restart keeps context.
  - Trim old turns; do not grow forever.

### Week 3 demo gate

```
what's the weather in Bangalore
search the web for rust 2024 edition changes and summarize with sources
remember that my project root is F:\major_project
what is my project root
git status in F:\major_project\igris-ai
run tests in F:\major_project\igris-ai
set a timer for 2 minutes
```

Expected: weather works without a key, search cites URLs, memory recall is semantic, tests run with approval, timer fires.

---

## Week 4 — Ecosystem, surface, proof

**Goal:** plug into the MCP ecosystem, put a real UI and hotkey on it, and prove the numbers.

### Tasks

- [x] **4.1 MCP client.** (JSON-RPC over stdio, dynamic `mcp.<server>.<tool>` registration, approval-gated; tested with filesystem server)
  - Config file `mcp.json`:
    ```json
    {
      "servers": [
        {
          "name": "filesystem",
          "command": "npx",
          "args": ["-y", "@modelcontextprotocol/server-filesystem", "F:\\major_project"]
        }
      ]
    }
    ```
  - Implement JSON-RPC 2.0 over stdio: `initialize`, `notifications/initialized`, `tools/list`, `tools/call`.
  - Register discovered tools dynamically. Default capability: `Sensitive` (grant required).
  - Namespace names: `mcp.<server>.<tool>`.
  - Timeout and restart policy per server. Never leave orphan processes.

- [ ] **4.2 Expose IGRIS as an MCP server (stretch).**
  - Only if 4.1 is stable. Reuse the registry and permission policy.

- [ ] **4.3 Minimal Tauri v2 shell** (largest item).
  - New sibling crate `F:\major_project\igris-ai-app` depending on `igris-ai`. Do not modify the ecosystem repos.
  - Tray icon with `Listening / Thinking / Executing / Speaking / Error` states.
  - Global hotkey: `Ctrl+Alt+Space` (avoid `Ctrl+Shift+Space`, which is commonly taken).
  - Compact always-on-top overlay with transcript, not a full chat app.
  - Approval dialog: tool name, args preview, `Allow once` / `Always allow` / `Deny`.
  - Fallback if Tauri slips: a TUI overlay using the existing REPL. Do not lose the week to scaffolding.

- [x] **4.4 Benchmark harness.** (`src/bin/bench.rs`, writes `benchmarks/results.csv`)
  - No new framework required. A `src/bin/bench.rs` or a PowerShell script that feeds the 20 commands and records `Instant` timings.
  - Write `benchmarks/results.csv`: command, mode, wake->STT, STT->plan, tool exec, TTS start, total, success.
  - Report p50 and p95.

- [ ] **4.5 Reliability pass.**
  - Missing model -> clear message, no panic.
  - Missing API key -> offline fallback, no panic.
  - Microphone busy -> retry once, then clear error.
  - Network timeout -> speak "I can't reach the network" and continue.
  - Kill the process mid-plan -> no orphan child processes.

- [x] **4.6 Release build.** (`target/release/igris_ai.exe` builds cleanly)
  - `cargo build --release --bin igris_ai`.
  - Confirm the model pack resolves through `IGRIS_PKG_DIR` and the bundled layout (`src/core/paths.rs`).
  - Do not build an installer yet. That is post-sprint.

### Week 4 demo gate

- Press the hotkey, say "open my project and run the tests, tell me what broke".
- Approval dialog appears for the test command.
- MCP filesystem tool is listed by `tools` and callable with a grant.
- `benchmarks/results.csv` has 20 rows and p95 is inside target.

---

## 2. The 20-command acceptance suite

Run this at the end of Week 4. Pass = 18/20, with zero unapproved Sensitive actions.

| # | Command | Expected tool(s) | Week |
|---|---|---|---|
| 1 | open notepad | `app_control` | 2 |
| 2 | close notepad | `app_control` | 2 |
| 3 | switch to chrome | `app_control` | 2 |
| 4 | put chrome on the left half | `window_control` | 2 |
| 5 | mute | `volume_control` | 2 |
| 6 | set volume to 30 | `volume_control` | 2 |
| 7 | lock the screen | `power_control` + approval | 2 |
| 8 | take a screenshot | `screenshot` | 2 |
| 9 | what's on my screen | `screenshot` + vision/LLM | 2/3 |
| 10 | copy this to the clipboard | `clipboard_write` | 2 |
| 11 | find my resume pdf | `file_search` | 2 |
| 12 | summarize the invoice in Downloads | `read_document` + LLM | 3 |
| 13 | search the web for rust 2024 changes | `web_search` + citations | 3 |
| 14 | what's the weather in Bangalore | `get_weather` | 3 |
| 15 | set a timer for 10 minutes | `set_timer` | 3 |
| 16 | remind me to push at 6pm | `set_reminder` | 3 |
| 17 | git status in my project | `git_status` | 3 |
| 18 | run tests in igris-ai | `run_tests` + approval | 3 |
| 19 | focus mode | routine: close chat apps, mute, open editor, play music | 4 |
| 20 | open my project, run tests, tell me what broke | multi-step: `app_control` + `run_tests` + LLM narration | 4 |

---

## 3. Explicitly out of scope

Do not build these this sprint, even if they look impressive:

- 50 app launchers or a plugin marketplace.
- Smart home, camera, music library, calendar OAuth, email sending.
- General chatbot UI. Copilot wins that; do not compete there.
- Local 7B as the default on 8 GB RAM. Local voice + online reasoning is the right split.
- Custom wake-word training. The STT-based "hello" path works and costs no extra model.
- Local vision. Screenshot + online vision is enough for v1.
- Auto-update, code signing, installer, telemetry.
- Mobile or cross-repo integration.

---

## 4. Decisions needed before coding

| # | Decision | Options | Recommendation | Owner |
|---|---|---|---|---|
| 1 | Async trait style | `async-trait` crate vs `BoxFuture` | `BoxFuture`; `futures` is already a dependency | Dev |
| 2 | Memory backend | `rusqlite` vs JSON + embeddings | `rusqlite` with `bundled`; JSON will not survive routines/reminders | Dev |
| 3 | PDF extraction | `pdf-extract` vs `lopdf` vs external `pdftotext` | `pdf-extract`; ask before adding | Dev |
| 4 | Windows automation | PowerShell now vs `windows` crate now | PowerShell first, migrate hot paths if p95 misses | Dev |
| 5 | UI | Tauri v2 vs TUI overlay | Tauri if Week 4 starts on time; TUI fallback otherwise | User |
| 6 | Search provider | Brave vs Tavily vs SearXNG | Brave primary, Tavily fallback | User (key) |
| 7 | Wake word | Keep "hello" vs "hey igris" | Keep "hello"; revisit after the suite passes | User |

---

## 5. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| PowerShell startup cost (~200–400 ms/call) | p95 misses | Cache window handles; batch scripts; migrate hot paths to Win32 |
| Tauri scaffolding eats Week 4 | No UI gate | Timebox to 3 days; fall back to TUI overlay |
| New dependencies break the build | Lost days | One dependency per decision, `cargo check` immediately, no bulk upgrades |
| Ollama not installed | Offline reasoning fails | Detect at startup; spoken fallback; document the install step |
| 8 GB RAM pressure | Slow or OOM | Lazy-load; unload LLM after 5 min idle; never load LLM + STT + TTS eagerly |
| LLM emits invalid tool args | Bad actions | Schema validation + approval + no free-form shell |
| Audit log leaks secrets | Privacy incident | Redact args for `Sensitive` tools; cap result length |
| Uncommitted baseline | Messy diffs | Commit or stash before Week 1 |

---

## 6. Weekly verification commands

Run at every gate:

```powershell
cargo fmt --check
cargo check --bin igris_ai
cargo test
cargo clippy --bin igris_ai
cargo run --bin igris_ai
```

Manual smoke:

```text
tools
tool system_info
tool app_control {"action":"launch","app":"notepad"}
tool shell_run {"command":"whoami"}
pending
```

---

## 7. Progress tracker

| Week | Theme | Gate | Status |
|---|---|---|---|
| 1 | Execution core | Multi-step command + approval flow | [x] code+tests green 2026-10-31; manual demo pending |
| 2 | Windows control | 5 OS commands, no restart | [x] |
| 3 | Knowledge + dev | Weather, search with sources, memory, tests | [x] |
| 4 | MCP + UI + proof | Hotkey overlay, MCP tool, benchmark CSV | [ ] |

**Definition of done for the sprint:** 18/20 commands pass, p95 inside target, zero unapproved Sensitive actions, offline voice loop verified with the network disabled.

---

## 8. Post-sprint backlog (do not start early)

- Installer, code signing, auto-update.
- Wake-word "hey igris" and barge-in.
- Routines UI and recording from actions.
- Streaming STT and streaming TTS.
- Vision: screenshot Q&A with GPT-4o/Claude.
- Google Calendar / Gmail OAuth.
- Plugin SDK and sandboxed script skills.
- Multi-language STT/TTS.
- Local LLM benchmarking (Qwen2.5 3B vs Phi-3.5 mini) on the MX350.
