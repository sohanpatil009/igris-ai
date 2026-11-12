# TODO — moonshine-test

## Tool Calling (ZeroClaw integration)

Full details + evidence: `tool_calling_plan.md` · ZeroClaw graphs:
`E:\major_project\igris\zeroclaw\graphyfy\GRAPH.md` · this repo's graph:
`graphyfy/GRAPH.md`

### Phase 0 — toolchain
- [ ] `rustup update` → rustc 1.96 (parser is edition 2024; local is 1.95)
- [ ] Sanity: `cargo check -p zeroclaw-tool-call-parser` inside `E:\major_project\igris\zeroclaw`

### Phase 1 — vendored parser (`src/toolparse.rs`)
- [ ] Copy `zeroclaw/crates/zeroclaw-tool-call-parser/src/lib.rs` (~2.7k LOC) → `src/toolparse.rs`; keep trimmed test subset
- [ ] Header: upstream path + commit + `MIT OR Apache-2.0` notice
- [ ] Own `strip_think_tags` (port of `api/src/model_provider.rs:237-254`) **+ LFM3 think spans** (`<Token 1437>` … `<Token 9550>`)
- [ ] Stub the 2 `zeroclaw_log` warns (lib.rs 2230 / 2426, helper 1870)
- [ ] Trim `map_tool_name_alias` → identity + our names
- [ ] Add `regex = "1"` dep; extend `TERMINAL_MARKERS` with `<|eot_id|>`
- [ ] `cargo test` (parser subset green)

### Phase 2 — prompt protocol (`llm.rs`)
- [ ] `ToolSpec { name, description, parameters_schema }`
- [ ] `tools_protocol_block(specs)`: ZeroClaw `TOOL_CALL_PROTOCOL_INSTRUCTIONS` verbatim (`tool_call_format.rs:39-58`) + `### Available Tools`, specs **sorted by name** (byte-stable KV prefix)
- [ ] Append block to `VOICE_SYSTEM_PROMPT` once at history seed (assist.rs:658); + few-shot example kept
- [ ] `--no-tools` flag / `MOON_NO_TOOLS=1` → block dropped, behavior = today

### Phase 3 — tool registry (`src/tools.rs`, ~300 LOC)
- [ ] `ToolDef { name, desc, schema, run: fn(&Value) -> Result<String,String> }` + `static TOOLS` (sorted)
- [ ] `get_time` · `calculate` (port pattern of `zeroclaw-tools/src/calculator.rs`) · `open_app` (hard allowlist via `std::process::Command`, no shell)
- [ ] Security: no arbitrary shell v1 · allowlist const · per-tool timeout → error result · sequential only
- [ ] Unit tests per tool

### Phase 4 — agent loop (`turn_reply`, assist.rs)
- [ ] Iteration loop, max 10 (ZeroClaw `DEFAULT_MAX_TOOL_ITERATIONS`)
- [ ] Stream hold-back in `on_tok`: `looks_like_incomplete_tool_protocol_json` + tool-tag detection; terminal markers stripped cross-chunk → envelope never reaches TTS
- [ ] Tool turn: speak filler "ek seconds…" once; final prose streams as today
- [ ] `parse_tool_calls` on full text → filter calls against registry (unknown → error result)
- [ ] Malformed protocol → `detect_tool_call_parse_issue` corrective user msg, max 2 retries
- [ ] History: assistant msg keeps raw envelopes; user msg `[Tool results]` + tool_result blocks (`name`, `status="ok|error"`) — prefix stability for `cache_prompt` + `--cache-reuse 1`
- [ ] Barge-in (`stop_when`) cuts whole loop as today

### Phase 5 — tests + regression
- [ ] CLI e2e: `chat "what time is it"` → tool call → answer with time
- [ ] CLI e2e: `chat "2 plus 2"` → calculate → "4"
- [ ] Voice e2e: `assist --hotkey` spoken time question
- [ ] Regression: plain chat unchanged; prefill ping latency unchanged

### Unit testing (repo pattern: inline `#[cfg(test)] mod tests`, like hotkey.rs/live.rs)
- [ ] **toolparse.rs** — vendored parser subset:
  - [ ] single tool_call tag → 1 call, name + arguments parsed
  - [ ] multiple calls in one response
  - [ ] plain prose → 0 calls, cleaned text == input (no false positive)
  - [ ] malformed JSON recovery (file_write case) still returns a call
  - [ ] `strip_think_tags`: standard `<think>` + **LFM3 `<Token 1437>`…`<Token 9550>`** spans removed
  - [ ] terminal markers (`<|eom|>`, `<eom>`, `<|eot_id|>`) stripped trailing + mid-text
  - [ ] unknown/empty arguments → `{}` not a panic; non-UTF8 safe
- [ ] **llm.rs (protocol)** — pure functions only (no server):
  - [ ] `tools_protocol_block`: specs rendered **sorted by name** (byte-stable output for KV prefix)
  - [ ] block omitted when tools disabled (`--no-tools` / `MOON_NO_TOOLS=1`)
  - [ ] schema serialization round-trips serde_json
- [ ] **tools.rs** — per tool:
  - [ ] `get_time`: output format stable + parseable
  - [ ] `calculate`: correct results; div-by-zero / garbage input → `Err`, not panic
  - [ ] `open_app`: allowlisted name launches; **non-allowlisted → Err**; empty args → Err
  - [ ] timeout wrapper: hung tool → error result within budget
- [ ] **assist.rs (loop glue)** — test the pure pieces:
  - [ ] call filtering against registry (unknown name → error result, not crash)
  - [ ] tool_result user-message carrier shape (`[Tool results]` + `status="ok|error"` blocks)
  - [ ] stream hold-back detector: tool-tag start / incomplete JSON held; normal sentence passes
  - [ ] malformed-retry counter caps at 2, iteration counter caps at 10
  - [ ] history after a tool turn: assistant msg keeps raw envelope + results msg appended in order
- [ ] `cargo test` gate before every phase commit (Phase 1–5)

### Open decisions (user)
- [ ] 1. Vendored parser (recommended) vs ZeroClaw path-dep
- [ ] 2. Starter tool set ok?
- [ ] 3. `open_app` allowlist: notepad, calc, chrome, spotify, vscode?
- [ ] 4. Tools always-on + `--no-tools` escape, or `--tools` opt-in?
- [ ] 5. `rustup update` abhi?

### Later (v1.1+)
- [ ] `web_fetch` (ureq, size-capped) · `create_note` (workspace-scoped)
- [ ] Grammar-constrained tool JSON via llama.cpp `json_schema` param
- [ ] Spoken confirmation for destructive ops · parallel tools
- [ ] WASM/MCP plugin bridge (`zeroclaw-plugins`), if ever needed

## Pending (GPU/CPU backend — pre-tool-calling)
- [ ] Test C result confirm (GPU exe renamed → CPU fallback → restored) — running
- [ ] Commit llm.rs / assist.rs / main.rs / README (auto GPU→CPU backend)
- [ ] Relaunch `assist --hotkey` for the user
