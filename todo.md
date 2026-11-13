# TODO — moonshine-test

## Tool Calling (ZeroClaw integration)

Full details + evidence: `tool_calling_plan.md` · ZeroClaw graphs:
`E:\major_project\igris\moonshine-test\zeroclaw\graphyfy\GRAPH.md` (nested clone,
untracked in git) · this repo's graph: `graphyfy/GRAPH.md`

### Phase 0 — toolchain — DONE 2026-11-13
- [x] rustup update → rustc **1.100.0** default (minimal profile; parser needs ≥1.96, workspace pins 1.96.0)
- [x] Sanity: `cargo check -p zeroclaw-tool-call-parser` inside
      `E:\major_project\igris\moonshine-test\zeroclaw` (nested clone — parser
      lives there, NOT the old sibling path) → **pass** (2m04s)

### Phase 1 — vendored parser (`src/toolparse.rs`) — DONE 2026-11-13
- [x] Copy `zeroclaw/crates/zeroclaw-tool-call-parser/src/lib.rs` (~2.7k LOC) → `src/toolparse.rs`; keep trimmed test subset
- [x] Header: upstream path + commit + `MIT OR Apache-2.0` notice
- [x] Own `strip_think_tags` (port of `api/src/model_provider.rs:237-254`) **+ LFM3 think spans** (`<Token 1437>` … `<Token 9550>`)
- [x] Stub the 2 `zeroclaw_log` warns (lib.rs 2230 / 2426, helper 1870)
- [x] Trim `map_tool_name_alias` → identity + our names
- [x] Add `regex = "1"` dep; extend `TERMINAL_MARKERS` with `<|eot_id|>`
- [x] `cargo test` (parser subset green) → **198 passed / 0 failed / 1 ignored** (the
      ignored one is the pre-existing upstream scanner boundary case), 0.07s

### Phase 2 — prompt protocol (`llm.rs`) — DONE 2026-11-13
- [x] `ToolSpec { name, description, parameters_schema: serde_json::Value }`
- [x] `tools_protocol_block(specs)`: ZeroClaw `TOOL_CALL_PROTOCOL_INSTRUCTIONS` verbatim
      (`tool_call_format.rs:39-58`, byte-pinned by a test that transcribes the block
      independently) + `### Available Tools`, specs **sorted by name** (byte-stable KV prefix);
      tag literals come from `toolparse::TOOL_CALL_*` constants so no source file carries raw tags
- [x] Block appended once at history seed — voice (assist.rs:658) **and** the `chat` path
      (llm.rs `stream_persona`, which Phase 5's e2e drives); few-shot example kept (the
      block's fenced example); `render_prompt` unchanged per plan §3.2
- [x] `--no-tools` flag / `MOON_NO_TOOLS=1` → block dropped, behavior = today (both paths)

### Phase 3 — dynamic tool registry (`src/tools.rs`) — DONE 2026-11-13
- [x] `Registry { tools: Vec<ToolDef> }` — **runtime registry, not a hardcoded table**;
      `ToolDef { name, desc, schema, run: Arc<dyn Fn + Send + Sync> }` (**Arc not Box** — the
      timeout dispatch moves the closure onto a worker thread, which needs Send+Sync)
- [x] `specs()` (sorted → prompt block) + `dispatch(name, args)`; unknown name → error result;
      `register()` replaces a same-name tenant (overrides groundwork)
- [x] Seed set v1 (registry tenants): `get_time` · `calculate` (port pattern of
      `zeroclaw-tools/src/calculator.rs`, 13-function voice subset) · `open_app` (hard
      allowlist via `std::process::Command`, no shell); `registry()` LazyLock singleton feeds
      both prompt surfaces — llm.rs Phase-2 interim `seed_specs()` removed, one owner
- [x] Dynamism ladder designed: v1 code-registered (done) → v1.1 **`tools.toml`
      config-defined tools** (name/desc/schema + allowlisted command template, `{arg}`
      substitution, no recompile) → v3 MCP/WASM (ZeroClaw plugins pattern) — documented in
      the tools.rs module header; v1.1/v3 are future phases
- [x] Security: no arbitrary shell v1 · allowlist consts · per-tool 5s timeout → error
      result · sequential only
- [x] Unit tests per tool (17 new)

### Phase 4 — agent loop — DONE 2026-11-13
- [x] Iteration loop, max 10 (ZeroClaw `DEFAULT_MAX_TOOL_ITERATIONS`) — lives in
      `llm.rs::agent_turn` (shared core), driven by voice `turn_reply` **and** the `chat`
      path's `stream_persona`; `AgentLoopState` keeps the caps pure and unit-tested
- [x] Stream hold-back: `toolparse::StreamFilter` (stream_guard concept, chunk-boundary
      safe) — tool-call envelopes, tool_result blocks, think spans (standard + LFM3) and
      terminal markers are withheld as spans and dropped; partial-opener tails held across
      chunks; `finish()` releases leftover text verbatim (never lose text) → envelope never
      reaches TTS or the terminal. Parsing still runs on the full raw text
- [x] Tool turn: filler "ek seconds…" spoken once per turn while the tools run; final prose
      streams as today
- [x] `parse_tool_calls` on full text → every call dispatched against the registry
      (unknown name → `status="error"` result fed back, never a crash)
- [x] Malformed protocol → `detect_tool_call_parse_issue` → corrective user msg,
      max 2 retries (`MAX_MALFORMED_RETRIES`), then plain-text handling; budget resets on a
      reply that parses
- [x] History: assistant msg keeps raw envelopes (minus leaked trailing terminal markers);
      results in a user message with the `[Tool results]` carrier + `tool_result` blocks
      (`name`, `status="ok|error"`, `result`) — history only ever appends, so each round is
      a strict prompt-prefix of the next for `cache_prompt`
- [x] Barge-in (`stop_when`) cuts the whole loop: `on_released` returns false → stream
      stops → `AgentTurnEnd::Cut`, in-flight TTS killed, partial reply still stored (parity
      with the plain path)

### Phase 5 — tests + regression
- [ ] CLI e2e: `chat "what time is it"` → tool call → answer with time
- [ ] CLI e2e: `chat "2 plus 2"` → calculate → "4"
- [ ] Voice e2e: `assist --hotkey` spoken time question
- [ ] Regression: plain chat unchanged; prefill ping latency unchanged

### Unit testing (repo pattern: inline `#[cfg(test)] mod tests`, like hotkey.rs/live.rs)
- [x] **toolparse.rs** — vendored parser subset:
  - [x] single tool_call tag → 1 call, name + arguments parsed
  - [x] multiple calls in one response
  - [x] plain prose → 0 calls, cleaned text == input (no false positive)
  - [x] malformed JSON recovery (file_write case) still returns a call
  - [x] `strip_think_tags``<think>` + **LFM3 `<Token 1437>`…`<Token 9550>`** spans removed
  - [x] terminal markers (`<|eom|>`, `<eom>`, `<|eot_id|>`) stripped trailing (+ stacked);
        mid-text preserved — upstream policy, whitespace-before-marker kept
  - [x] unknown/empty arguments → `{}` not a panic; non-UTF8 safe
- [x] **llm.rs (protocol)** — pure functions only (no server):
  - [x] `tools_protocol_block`: specs rendered **sorted by name** (byte-stable output for KV prefix)
  - [x] block omitted when tools disabled (`--no-tools` / `MOON_NO_TOOLS=1`)
  - [x] schema serialization round-trips serde_json (+ byte-verbatim protocol pin)
- [x] **tools.rs** — per tool:
  - [x] `get_time`: output format stable + parseable (regex-pinned)
  - [x] `calculate`: correct results; div-by-zero / garbage input → `Err`, not panic
  - [x] `open_app`: non-allowlisted → Err; empty args → Err; allowlist coverage +
        install-path candidates asserted (**real GUI launch deferred to Phase 5 e2e** —
        unit tests never spawn an allowed app)
  - [x] timeout wrapper: hung tool → error result within budget; panicking tool → "crashed" error
- [x] **assist.rs (loop glue)** — the loop is `llm.rs::agent_turn` with `turn_reply` as a
      thin surface; its pure pieces are all unit-tested:
  - [x] call filtering against registry (unknown name → `status="error"`, not crash) —
        registry `dispatch` tests + error-outcome case in the carrier test
  - [x] tool_result user-message carrier shape (`[Tool results]` + `status="ok|error"` blocks)
        — llm.rs `tool_results_user_message_carrier_shape`
  - [x] stream hold-back detector: envelope/think-span/marker spans held cross-chunk;
        plain prose passes immediately — toolparse.rs `stream_filter_*` (8 tests)
  - [x] malformed-retry counter caps at 2, iteration counter caps at 10 —
        llm.rs `AgentLoopState` tests
  - [ ] history after a tool turn: assistant msg keeps raw envelope + results msg appended in order
- [ ] `cargo test` gate before every phase commit (Phase 1–5)

### Open decisions (user) — RESOLVED 2026-11-13
- [x] 1. **Vendored parser** (chosen; nested zeroclaw clone is untracked so vendoring also preserves the source)
- [x] 2. Seed tool set: **get_time + calculate + open_app** (registry stays dynamic: v1 code → v1.1 `tools.toml` user tools → v3 MCP/WASM)
- [x] 3. `open_app` allowlist: **notepad, calc, chrome, spotify, vscode**
- [x] 4. **Always-on + `--no-tools` escape** (`MOON_NO_TOOLS=1` too)
- [x] 5. **rustup update now** → 1.100.0 minimal profile (satisfies workspace `rust-version = 1.96.0`; full `stable` channel update was too slow due to cross-compile std targets)

### Later (v1.1+)
- [ ] `web_fetch` (ureq, size-capped) · `create_note` (workspace-scoped)
- [ ] Grammar-constrained tool JSON via llama.cpp `json_schema` param
- [ ] Spoken confirmation for destructive ops · parallel tools
- [ ] WASM/MCP plugin bridge (`zeroclaw-plugins`), if ever needed

## Pending (GPU/CPU backend — pre-tool-calling)
- [x] Test A (`--cpu` → CPU backend ready) — pass
- [x] Test B (auto → GPU backend ready) — pass
- [x] Test C (GPU exe renamed → auto CPU fallback → restored) — pass
- [x] Commit: `c5b16e3` (backend) + `fbb9d34` (docs)
