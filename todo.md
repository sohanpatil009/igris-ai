# TODO — igris-ai

## Tool Calling (ZeroClaw integration)

Full details + evidence: `tool_calling_plan.md` · ZeroClaw graphs:
`E:\major_project\igris\igris-ai\zeroclaw\graphyfy\GRAPH.md` (nested clone,
untracked in git) · this repo's graph: `graphyfy/GRAPH.md`

### Phase 0 — toolchain — DONE 2026-11-13
- [x] rustup update → rustc **1.100.0** default (minimal profile; parser needs ≥1.96, workspace pins 1.96.0)
- [x] Sanity: `cargo check -p zeroclaw-tool-call-parser` inside
      `E:\major_project\igris\igris-ai\zeroclaw` (nested clone — parser
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

### Phase 5 — tests + regression — CLI items DONE 2026-11-13
- [x] CLI e2e: `chat "what time is it"` → tool call → answer with time — first run exposed
      the model's real shape: the tool-call wrapper with an XML-attr body (`function=NAME`
      open + immediate close / `parameter=K` children), uncovered by the stock JSON/XML/GLM
      fallbacks → vendored parser extended with OUR `parse_function_attr_calls` (JSON takes
      precedence); second run: get_time round → real time
- [x] CLI e2e: `chat "2 plus 2"` → calculate → "4" — the tenant speaks upstream's
      `{function, values}` schema and the model follows it exactly; the catch: parameter
      children arrive as TEXT, so `parameter=values>[2, 2]` was a string and
      `extract_values` rejected it → param children are now JSON-decoded (real arrays /
      numbers, non-JSON stays string). Model threw in a bonus get_time round — loop fed
      the result back and the final answer was correct
- [x] Voice e2e: spoken time question → tool round → spoken answer — DONE 2026-11-13 via
      the WAV-driven pipeline (`assist <wav>` exercises the exact same path, no mic
      needed): piper-synthesized "what time is it" → STT transcribed → `[tools] get_time`
      → real time streamed + spoken. Human mic/PTT pass still welcome (`--hotkey`, or a
      synthetic PTT mic via MOON_PTT_WAV). e2e finding fixed along the way: a think span
      opened in one agent round but closed in the next leaked its stray close tag through
      the hold-back filter → StreamFilter now drops stray close tags outside any span
      (opener-first precedence, partial close tails held across chunks, `finish()`
      releases a partial tail verbatim); gate 243 passed / 0 failed / 1 ignored
- [x] Regression: `chat --no-tools "hello"` → plain reply, no `[tools]` line (the
      `--no-tools` path is the untouched pre-tools code); prefill ping latency: the
      protocol block sits in the stable system prefix, KV-cached after the first turn —
      confirm live in the voice session
- [x] e2e findings fixed in-loop: model repeats its envelope / emits identical calls
      several times per reply → `dedup_tool_calls` (name + upstream-canonicalized args
      signature) before dispatch, so an app is never opened twice; gated
      `MOON_DEBUG_RAW=1` raw-round dump in `agent_turn` for diagnosis; `chat "open
      notepad"` → real GUI launch verified (closes the Phase 3 deferred item)
- Note (operational): raw tag text in prompts/logs is live protocol for the vendored
      parser — keep it escaped (`\x3c`) in any dump meant for reading; the
      MOON_DEBUG_RAW dump does this

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
        install-path candidates asserted (**real GUI launch verified in Phase 5 e2e**:
        `chat "open notepad"` opened the window — unit tests never spawn an allowed app)
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
- [x] 2. Seed tool set: **get_time + calculate + open_app** (registry stays dynamic: v1 code → v1.1 `tools.toml` user tools → v3 MCP/WASM — MCP/WASM rung superseded 2026-11-13 by the no-MCP decision, see "Roadmap to goal.md" below)
- [x] 3. `open_app` allowlist: **notepad, calc, chrome, spotify, vscode**
- [x] 4. **Always-on + `--no-tools` escape** (`MOON_NO_TOOLS=1` too)
- [x] 5. **rustup update now** → 1.100.0 minimal profile (satisfies workspace `rust-version = 1.96.0`; full `stable` channel update was too slow due to cross-compile std targets)

### Roadmap to goal.md (no-MCP) — set 2026-11-13
Decision: goal.md is achieved WITHOUT MCP, external or local (user decision).
The registry already is an in-process dynamic tool surface — runtime
register/replace, specs generated into the prompt, timeout-guarded dispatch —
and `tools.toml` (v1.1) covers extension without recompiling. The old
"v3 MCP/WASM" rung is replaced by this roadmap; an MCP client becomes a
someday-optional `mcp_tool` tenant type if a community integration is ever
preferred over writing it. mem0-rs was evaluated for persistent memory and
REJECTED (see v1.2).

- [ ] **v1.1 — system & files** (goal.md §1/§3/§5):
      - [x] **batch 1 DONE 2026-11-14**: `file_search` (bounded
            case-insensitive name walk over the common user folders or an
            explicit root; skip-list + depth/entry/3.5 s budget caps so
            partial results beat the 5 s dispatch timeout) · `read_file`
            (~-expansion, 10 MB cap, max_chars truncation 2000/8000, binary
            sniff refuses non-text files) · `create_folder` (mkdir -p;
            file-collision + idempotence handled) · `open_url` (ShellExecuteW,
            http/https scheme allowlist, no shell) · `media_key` (keybd_event
            of VK_MEDIA_* 0xB0-0xB3; `windows` crate 0.62.2 — decision YES).
            Note: 0.62.2's VK_MEDIA consts match the official Winuser.h table
            (media keys 0xB0-0xB3, volume keys sit at 0xAD-0xAF). Gate
            260 passed / 0 failed / 1 ignored (+15 tests). e2e: chat-driven
            file_search + read_file (found both WAVs, refused binary garbage),
            create_folder verified on disk, open_url opened example.com,
            media_key real press (exactly one — dedup held). e2e finding:
            LFM3 sometimes appends its NATIVE DSML-shaped tool envelope
            (a fullwidth-bar wrapper the vendored parser does not know) after
            a proper call — FIXED 2026-11-14 in toolparse.rs: the
            StreamFilter now holds the DSML span so it never reaches
            display/TTS; detect_tool_call_parse_issue issues a corrective
            verdict only when the reply OPENS with the envelope (pure
            native-format drift), while prose-wrapped echoes are ignored
            (no wasted round). DSML is deliberately NOT a parse family:
            MOON_DEBUG_RAW e2e showed every DSML body is schema-confused
            (the JSON schema echoed as the arguments value), so parsing +
            dispatching it produced "missing parameter" errors that sent
            the model into a hallucination loop (fake inline tool results,
            nested garbled spans, repeat presses); also added a guard so
            the GLM shortened-body fallback can no longer turn the
            envelope's label into a bogus call, and a char-boundary fix in
            stream holdback (multibyte tag prefixes would panic). Gate
            269 passed / 0 failed / 1 ignored (+8 DSML tests). e2e re-run
            clean: play/pause, next track, get_time — no envelope text in
            display, no haywire rounds. DSML follow-ups, revisited by the
            batch-2 e2e: (a) ~~honest-error preferred over unwrapping~~
            REVERSED 2026-11-14 — the attr-format confusion
            (`parameter=properties` + JSON child) does NOT reliably
            self-heal: `media_key` healed in one round because its arg is
            a scalar string the model can retype (`parameter=key`), but
            `set_volume` (object/number args) burned 5 rounds repeating
            `parameter=properties` → `parameter=parameters` and gave up
            without ever dispatching. Fix shipped in tools.rs:
            `unwrap_schema_confused_args` at dispatch — when args are
            exactly `{"properties"| "parameters": {…}}` and the tool's
            schema does not declare that key, strip one level and dispatch
            the real args (schema-checked, so a tool that legitimately
            declares the key keeps it; dedup makes double-dispatch a
            non-risk). set_volume e2e now succeeds in ONE round; gate 275
            passed (+2 tests). (b) LFM3 hallucinates fake inline
            tool_result blocks when confused — parser ignores them (no
            name-bearing dispatchable shape survives the gates), bounded
            by MAX_TOOL_ITERATIONS; NEW 2026-11-14: when the model emits
            the fake result AFTER a clean final answer (bare
            `{"name": …, "result": …, "status": "ok"}` JSON, no envelope),
            the parser correctly ignores it but the StreamFilter releases
            it to display/TTS — the JSON line leaks into the chat/voice
            output. Stream-filter follow-up: hold/strip trailing
            fake-tool_result JSON the same way DSML spans are held.
      - [x] **batch 2 DONE 2026-11-14**: `sys_stats` (GetSystemTimes CPU
            sampled over ~150 ms + GlobalMemoryStatusEx RAM +
            GetDiskFreeSpaceExW on %SystemDrive% + GetSystemPowerStatus
            battery/AC + GetTickCount64 uptime; all read-only) ·
            `set_volume` (COM: CoInitializeEx → MMDeviceEnumerator →
            default render endpoint → IAudioEndpointVolume
            SetMasterVolumeLevelScalar, read back via
            GetMasterVolumeLevelScalar; COM init balanced on the dispatch
            worker thread) · `screenshot` (GDI GetDC + CreateDIBSection
            32-bpp top-down + BitBlt → BGRA→RGB → `image` crate PNG
            (png-only feature set); default path = dirs::picture_dir()
            with timestamp name, optional `open` via ShellExecuteW; every
            GDI handle released on every path). New windows features:
            Win32_System_{Threading,SystemInformation,Power,Com} ·
            Win32_Storage_FileSystem · Win32_Media_Audio +
            Win32_Media_Audio_Endpoints (endpoint interfaces live in a
            submodule with their own feature gate) · Win32_Graphics_Gdi.
            Gate 275 passed / 0 failed / 1 ignored (+6 tests incl. a live
            screenshot smoke test verifying the PNG signature on disk).
            e2e all clean, one round each: sys_stats (plausible live
            values: CPU 3%, RAM 5.6/7.7 GB, disk 26.1 GB free, battery
            54% AC, uptime 2d 13h) · set_volume set 40 → "System volume
            set to 40%." read back, restored to 70 · screenshot → real
            PNG verified on disk AND visually (default OneDrive Pictures
            path). The set_volume e2e also produced the unwrap fix above.
      - [ ] remaining v1.1: `tools.toml` config-defined tools
            (commands/URLs, incl. `play_song` → YouTube search URL) — no
            core change per new tool; wiring decision pending: config
            load must NOT live in `registry()`'s LazyLock initializer
            (llm.rs pin tests call `registry()` and must stay hermetic)
- [ ] **v1.2 — memory** (goal.md §7): own ~150-LOC JSON store (mem0-rs
      rejected 2026-11-13: single-version crates with double-digit lifetime
      downloads, stale; mem0-style LLM extraction adds write-time latency +
      nondeterminism this doesn't need) · `memory_save`, `memory_recall`,
      `create_note` (workspace-scoped), `activity_log`, `reminder_set` ·
      must survive restart
- [ ] **v2 — dev & browser** (goal.md §4/§6): allowlisted `run_command` ·
      `list_dir`, `content_search` · `web_fetch` (ureq, size-capped) ·
      `download_file` · `web_search` (one backend decision: DDG HTML vs
      API key)
- [ ] **v2.5 — gated actions** (goal.md §1/§3 safety rows): two-step spoken
      confirm UX in the agent loop (state intent → wait for spoken yes →
      execute; barge-in interplay) · `close_app` (live-learned 2026-11-13:
      graceful WM_CLOSE can silently no-op across session/elevation
      boundaries, force-kill is the fallback, NEVER force unsaved-data apps
      like Notepad blind) · `delete_file`, `shutdown` · activity log wired
      into gated actions
- [ ] **v3 — integrations & agentic** (goal.md §2/§8/§9): `spotify_play`
      (OAuth + token refresh over ureq) · window control (`focus_window`,
      `list_windows`) · planner-model decision (bigger GGUF vs cloud
      fallback; grammar-constrained tool JSON via llama.cpp `json_schema`
      evaluated here) · parallel tool execution · WhatsApp/email LAST,
      behind explicit consent (external-approval-bound)
- [ ] Optional someday: WASM/MCP plugin bridge (`zeroclaw-plugins`) — only
      as an `mcp_tool` client tenant type, never a dependency of the core

## Pending (GPU/CPU backend — pre-tool-calling)
- [x] Test A (`--cpu` → CPU backend ready) — pass
- [x] Test B (auto → GPU backend ready) — pass
- [x] Test C (GPU exe renamed → auto CPU fallback → restored) — pass
- [x] Commit: `c5b16e3` (backend) + `fbb9d34` (docs)
