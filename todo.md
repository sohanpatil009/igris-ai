# TODO — igris-ai

## Tool Calling (ZeroClaw integration)

Full details + evidence: `tool_calling_plan.md` · ZeroClaw graphs:
`E:\major_project\igris\igris-ai\zeroclaw\graphyfy\GRAPH.md` (nested clone,
untracked in git) · this repo's graph: `graphyfy/GRAPH.md` ·
open bugs & suspicions: `bug.md`

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
preferred over writing it. Persistent memory shipped in v1.2 as a hybrid:
own sync store + vendored mem0-rust similarity kernels (whole-crate
vendoring was rejected; see v1.2).

- [x] **v1.1 — system & files** (goal.md §1/§3/§5) — DONE 2026-11-14:
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
            fake-tool_result JSON the same way DSML spans are held —
            DONE 2026-11-15 (`2a93c0d`, BUG-003): the fake-result
            signature (name + result/status) is held from its `{"` spell
            and dropped at the closing brace; non-machinery JSON releases
            verbatim, protocol spans inside the held object mark it
            contaminated, and the trigger never preempts a preceding
            envelope opener; gate 331/0/1.
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
      - [x] **`tools.toml` DONE 2026-11-14 — v1.1 complete**: config-defined
            tools at `%APPDATA%\igris\tools.toml` (`dirs::config_dir()`).
            Each `[[tool]]`: name (slug: lowercase/digits/underscores),
            description, optional schema (inline JSON string; default
            no-params), and exactly one of `url` (browser via
            ShellExecuteW, http/https, substituted values percent-encoded
            down to RFC 3986 unreserved so values can never split the
            query) or `command` (direct `std::process::Command` argv,
            fire-and-forget like open_app, NO shell so argument values
            are syntax-inert). `{placeholder}`s must map to declared
            schema properties (checked at load). Missing file = 0 tools;
            invalid file = stderr warning + ignored — bad config never
            bricks the assistant. Wiring decision (recon 2026-11-14): the
            config load must NOT live in `registry()`'s LazyLock
            initializer (llm.rs pin tests call `registry()` and must stay
            hermetic) — `load_config_tools()` runs at the TOP of CLI
            main(), stages into a OnceLock side table, and `registry()`
            folds it in at first use; tests never call the loader. Config
            tenants can override built-ins (register replaces same-name,
            by design). Gate 285 passed / 0 failed / 1 ignored (+10
            tests; ToolDef gained Clone + a manual Debug for unwrap_err).
            e2e: real config file with `play_song` (YouTube search URL) →
            chat "play shape of you by ed sheeran" → ONE round, [tools]
            play_song dispatched, browser opened (ShellExecuteW success),
            reply clean; the batch-2 screenshot tool confirmed session
            state. New deps: serde (derive), toml 0.9.
- [x] **v1.2 — memory** (goal.md §7) — DONE 2026-11-14: approach — hybrid
      (user choice): own persistent sync store + vendored mem0-rust
      utilities. Deep source examination 2026-11-14 of
      `mem0-rust` 0.2.0 (YASSERRMD — the real crate; "mem0-rs"
      404s on crates.io; mem0-rust itself = 1 version, 70 lifetime downloads,
      MIT, tokio-based): core `Memory` orchestrates Embedder → VectorStore →
      optional LLM → optional SQLite history → optional reranker, all
      async/tokio. Key facts: (a) default backend = `InMemoryStore`
      (RwLock HashMap) + `MockEmbedder` (hash-of-tokens vectors) → EVERYTHING
      volatile, restart = amnesia; the SQLite side persists only an audit
      history log, NOT the memories; (b) persistence is explicitly outsourced
      to external servers (Qdrant / Postgres+pgvector / Redis) — there is NO
      local-file or SQLite vector backend; (c) inference path = 2 LLM calls
      per write (fact extraction + update decisions) — unneeded here because
      the tool-call layer already does extraction with user intent;
      (d) found a real shipped bug: `reset()` has a `TODO: Build proper
      filters` → scoped reset silently wipes ALL memories + history;
      (e) vendoring the whole crate for IGRIS would mean WE write the
      persistent vector-store backend (the hard part), an embedder story,
      and a tokio↔sync bridge, then strip reqwest/HuggingFace/
      Anthropic/graph/rerankers/pyo3/bins. What shipped instead
      (`crates/igris-core/src/memory.rs` + tools): new module `memory` —
      `MemoryStore` (sync, file-backed at `%APPDATA%\igris\memory.json`,
      same trust domain as tools.toml; pretty JSON, versioned envelope,
      atomic write = tmp + rename on EVERY mutation; corrupt file is
      quarantined as `memory.json.corrupt-<stamp>` — never destroyed,
      store restarts fresh) with kinds Memory/Note/Activity/Reminder
      (id `m<millis>-<seq>`, tags normalized lowercase+dedup, created_at
      local `%Y-%m-%d %H:%M:%S`, due_at verbatim for reminders).
      VENDORED from mem0-rust 0.2.0 (MIT, attribution header, same
      precedent as toolparse.rs/ZeroClaw): `cosine_similarity` + the
      hash-of-tokens embedder (`hash_embed`, EMBED_DIMS=128), sync ports
      of the same math. Recall is hybrid: exact substring (1.0) > tag
      match incl. kind label singular/plural ("what are my reminders"
      surfaces the whole kind; y→ies plural rule) (0.9) > vendored cosine
      ≥ 0.2 ("similar"); ties break to newest; embeddings recomputed at
      recall time (no vectors on disk — file stays human-readable);
      exact-duplicate adds are not re-inserted (ci-equal same-kind
      content → "already saved"). Zero new deps (chrono/serde/dirs
      already present). Tools: `memory_save`, `memory_recall` (query +
      optional tags filter + limit 1-50), `create_note`, `activity_log`,
      `reminder_set` (content+due_at required; due_at stored verbatim,
      ISO 8601 encouraged). Hermeticity: memory tool run fns validate
      args BEFORE locking the process-wide store, so lib tests exercise
      only the validation path via dispatch — the store logic (including
      a unit-level survive-restart proof: write → drop → reopen →
      recall) is tested directly in memory.rs with temp dirs. Gate 299
      passed / 0 failed / 1 ignored (+14 tests: 11 memory + 2 tools
      validation/format + sorted-names & count pins updated 11→16).
      e2e (each CLI run = a fresh process, so this is real restart
      survival): "remember that my exam is on March 3rd, and remind me
      to call mom tomorrow at 9am" → ONE round, [tools] memory_save +
      reminder_set; NEXT process "when is my exam?" → [tools]
      memory_recall → March 3rd; "what reminders do I have?" → Call mom
      (kind-label match, zero word overlap); "note that the wifi password
      is hunter2" → saved (model chose memory_save with tags wifi/
      password — reasonable for a durable fact; create_note unused that
      turn); next process "what is my wifi password?" → hunter2.
      PROMPT FINDING (e2e-driven): with the plain description, LFM3
      skipped memory_recall for generic phrasings ("what is my X" —
      answered "I don't have access" without calling any tool) while
      "do you remember..." triggered it. Two fixes landed: (1) tool-call
      protocol instructions gained a persistent-memory rule (call
      memory_recall before claiming ignorance; pin updated); (2)
      description rewritten to trigger-phrase form ("Call this tool for
      ANY question phrased like 'what is my ...', 'when is my ...',
      'do you remember ...'") — this second one flipped the failing
      phrasing to a correct cross-session recall. Known model traits
      (not bugs): hash-token collisions can add a low-score "similar"
      rider to recall results (keyword hits always rank first; tests
      assert collision-tolerance); activity_log saw no natural use this
      session — its automation ("activity log wired into gated actions")
      stays a v2.5 item. One transient environmental note: a full gate
      once hung ~20 min on a stale build lock (no cargo process was
      left); rerun passed — treat as machine noise, not a code issue.
- [x] **v2 — dev & browser** (goal.md §4/§6) — DONE 2026-11-14: six tools
      over zero new deps (ureq 3 + regex + serde already present):
      `run_command` (curated allowlist: cargo check/build/test/run/new/
      init/add/remove/fmt/clippy/tree + read-only git status/log/diff/
      show/rev-parse; no shell, argv direct, control-byte rejection;
      bare `cargo run` = build-then-launch detached so the app stays
      alive) · `list_dir` (dirs-first, sizes, 200-entry cap) ·
      `content_search` (line-number grep, NUL-binary sniff, 1500-file /
      3.5 s budget) · `web_fetch` (512 KB cap, HTML→text, SSRF host
      block) · `download_file` (Downloads-only, 200 MB cap, HTML-page
      guard, content-length + streamed cap+1 reader) · `web_search`
      (DDG Lite HTML, keyless — user decision; redirect-unwrap +
      entity-decode). Per-tool time budgets via one `tool_budget` table
      (run_command/download 120 s, web_fetch 25 s, web_search 20 s,
      else the 5 s voice default). 22 tools registered; llm.rs protocol
      pin updated (ordering chain + Parameters count). Gate: 319 passed
      / 0 failed / 1 ignored. E2E all six green through the real agent
      loop (LFM3 picked the right tool every time with 22 tools
      advertised). FINDINGS (e2e-driven, both fixed in-tool): (1)
      content_search at repo-root scope MISSED `set_master_volume` —
      DFS pops the alphabetically-last top-level subtree first and the
      nested `zeroclaw` clone ate the whole 1500-file budget (walk sim:
      tools.rs sat at position 2296/2304); fixed with a breadth-first
      walk + skip nested `.git` clones on descent (explicit root INTO
      a clone still searches it) + PARTIAL-budget note made louder +
      desc now tells the model to retry with a narrower root. (2)
      validate_web_url kept the `:port` in the authority so
      `127.0.0.1:8018` slipped past the private-host block —
      is_private_host now strips all-digit ports (IPv6 brackets
      handled) and catches bare `::1`. Test-evidence notes: decode
      entities' numeric branch must strip the trailing `;` before
      parsing; `validate_command` checks the subcommand before control
      bytes (test inputs must put the control byte in a later arg).
- [ ] **Modularize tools.rs (~4.7k lines) and toolparse.rs (~2.7k
      vendored)** — user call 2026-11-14, do AFTER the v2 commit:
      tools.rs → `tools/` module split (registry core / per-family
      tool files / shared helpers / tests split alongside, structure
      only, gate stays 319/0/1); toolparse.rs → mechanical split
      keeping the vendored-notice header and per-part origin markers
      (upstream diffing gets harder — accepted).
- [x] **v2.5 — gated actions** (goal.md §1/§3 safety rows): two-step
      confirmation, machinery + four gated tools (2026-11-15, commits
      `1adf26d` core + `9e38253` tools, gate **375 passed / 0 failed /
      1 ignored**):
      - **Core** (`crates/igris-core/src/gating.rs`): file-backed pending
        store `%APPDATA%\igris\pending.json` (atomic tmp+rename, survives
        restarts → cross-process CLI arm+confirm works); cap 8
        evict-oldest, expiry 120 s, one pending slot per tool (drifted
        args replace → old intent unexecutable); freshness = armed in a
        strictly earlier turn of the same process OR by another process
        (turn counter bumped once per utterance at `agent_turn` entry —
        the single choke point voice+CLI share); consumed pending is
        spent even on execution failure. First call validates + arms and
        returns a loud `NOT EXECUTED — CONFIRMATION REQUIRED` plan
        (status stays "ok"); confirmed re-call (exact canonicalized args)
        executes. `Registry::dispatch` runs the flow; activity log lines
        `requested: <plan>` / `executed <tool>: <detail>` / `FAILED` are
        written automatically (tags `gated` + tool name). Config-defined
        tools stay ungated and may not shadow a gated builtin name.
      - **Tools** (`src/tools/gated.rs`): `close_app` (WM_CLOSE sweep via
        window enumeration → 4.5 s bounded wait → force-kill fallback,
        except notepad which is NEVER force-closed) · `delete_file`
        (Recycle Bin via SHFileOperationW `FOF_ALLOWUNDO`; protected
        subtrees: Windows/Program Files/ProgramData/exe dir, profile root
        exact, drive roots; folders refused; everything re-validated in
        the executor) · `shutdown` (`shutdown.exe /s /t 30`, cancelable)
        · `shutdown_abort` (UNGATED safe counterpart, harmless when
        nothing is pending) · `power_mode` (powercfg /setactive over the
        well-known scheme GUIDs, **availability verified at plan time
        against `powercfg /list`** — live-learned 2026-11-15: Windows 11
        ships only Balanced, so saver/performance are refused with a
        clear plan-time error instead of failing after confirmation).
        Executor re-validates everything the plan validated.
      - **Live e2e** (via new `examples/gated_e2e.rs` probe, one process
        arms, another confirms — the file-backed store's cross-process
        flow): close_app notepad arm→confirm → process gone; delete_file
        temp file arm→confirm → recycled, dead doubled-path intent then
        errors at plan time (`file not found`); power_mode saver
        plan-refused live, balanced arm→confirm → `Power plan set to
        Balanced.` (original scheme never left, nothing to restore);
        shutdown arm-only (`shutdown.exe /a` → error 1116 proves nothing
        was scheduled) + shutdown_abort live (`No shutdown was in
        progress`); activity log shows the full
        requested/executed/FAILED trail.
- [ ] **v3 — integrations & agentic** (goal.md §2/§8/§9): `spotify_play`
      (OAuth + token refresh over ureq) · planner-model decision (bigger GGUF vs cloud
      fallback; grammar-constrained tool JSON via llama.cpp `json_schema`
      evaluated here) · WhatsApp/email LAST,
      behind explicit consent (external-approval-bound)
  - [x] Window control — DONE 2026-11-16 (`list_windows`, `focus_window`,
        commit `31d5898`): z-order listing with pid→exe, ranked match
        (exact ci title > substring > exe, ties front-most), ALT-tap +
        SetForegroundWindow verified via GetForegroundWindow. Live probe
        W1–W4: all four paths honest, no hallucinated success; desktop
        Progman correctly filtered as a shell toolwindow.
  - [x] Parallel tool execution — DONE 2026-11-16 (commit `d48d6d8`):
        multi-call rounds run on concurrent workers (from `d54b3ac`),
        fan-out now bounded at 4 at a time with global call order kept
        (7-call peak test). Live probe T1: 5 calls in one round at
        236 ms, timing marker `over 5 call(s) parallel`.
  - [x] Recycle-bin control - DONE 2026-11-17 (commit `5acfaa9`, probes
        V1-V3): `recycle_bin` (ungated read-only indexed listing, newest
        first) + `empty_recycle_bin` (gated arm-confirm; action=delete with
        1-based indices from the listing, action=empty for all).
        Enumeration/deletion is plain std::fs over the bin's own $I/$R
        metadata pairs across every drive's $Recycle.Bin\<SID> - no COM
        data path; the only unsafe code is the SHChangeNotify
        view-refresh nudge. Only paired entries are listed (55 $I files ->
        39 paired = Explorer's count, validated live); orphan $I remnants
        are swept by action=empty; unindexed files in the bin folders are
        counted in readbacks, never touched. Gate 442 passed / 0 failed /
        1 ignored (+15 tests). Live probes: V1 listing (42 rows rendered
        correctly), V2/V2B index-1 delete (42 -> 41, payload verified
        gone), V3/V3B full empty (41 deleted + 16 metadata remnants swept,
        Explorer 0, 9 unrelated files untouched) - cross-process
        arm->execute worked first try for both pairs. Evidence:
        benchmark.md V-series 2026-11-17.
  - [x] Task-class round budget (plan P3 item 9) - DONE 2026-11-17
        (commit `b0c8819`): `budget.rs` classifies each turn Simple /
        Research / Report by keyword with a safe bias (misclassification
        only loosens; Simple keeps the historical 10-round default) and
        caps rounds / tool calls / web calls (Simple 10/12/4,
        Research 10/24/6, Report 12/32/8). Delivery-deadline notices are
        injected at 2-rounds-left and last-round (once each); spent web
        or tool-call budgets give deterministic refusal tool_results;
        tools that report a definitive `unsupported: ` error are blocked
        for the rest of the turn (power_mode plan-missing, no default
        audio device). New `github_repo_info` (GitHub REST, not
        web-budgeted) gives deterministic repo facts. Gate 455/0/1
        (+13 tests). Live re-run of the four cap-exhaustion data points:
        S3-11 Miss -> delivered Desktop comparison file via
        github_repo_info; S3-13 Miss -> answer delivered in ~7 rounds;
        S4-15 Miss -> architecture_report.md written; S5-20 Miss ->
        still a miss (12 rounds ran, two context overflows compacted the
        investigation away, final round released nothing) - the round
        cap is no longer the binding constraint there. Evidence:
        benchmark.md P3 item 9 section 2026-11-17.
  - [x] Browser targeting (U1 probe remainder) - DONE 2026-11-17:
        optional `browser` argument (chrome, brave, firefox, edge) on
        `open_url` and on config URL tools whose schema declares it
        (play_song's tools.toml updated). Resolution is an allowlist
        table of fixed install locations (Program Files / x86 /
        LocalAppData), first existing exe wins; the URL is one argv
        element via std::process::Command, never a shell, never a
        user-supplied path; unknown names die with the allowed list at
        dispatch time. Gate 459/0/1 (+4 tests). Live: brave 9->10 and
        10->11 procs (open_url + play_song), firefox 0->11 (open_url),
        netscape -> honest allowlist refusal, no window. Finding:
        page-shaped prompts ("open https://... in Firefox") twice
        steered the model to web_fetch instead - one retry gave a flatly
        false "I've opened it in Brave"; "Launch the <browser> with the
        page open" phrasing flips it to open_url. Evidence:
        benchmark.md B-series 2026-11-17.
- [ ] **browser-rs Windows port** (vendored `browser-rs/`, own git clone,
      MIT, upstream macOS/Linux-only): cdp.rs WebSocket transport
      (--remote-debugging-port + /json/version discovery) replacing the
      Unix pipe pair; watchdog body -> `taskkill /PID <pid> /T /F`;
      install.rs win64 CfT URLs + %LOCALAPPDATA% cache + tar.exe extract;
      profile_dir SingletonLock check cfg-gated to unix. Zero new deps
      (tokio-tungstenite + reqwest already present). Evidence:
      benchmark.md mcp-reff audit section 2026-11-17.
- [ ] **`mcp_tool` client tenant** (v3 rung made concrete by the audit):
      persistent server, `tools/list` cached at connect, MCP tool schemas
      -> ToolDef tenants, dispatch over JSON-RPC; start ungated under the
      same guarded-name rules as config tenants; first server to wire =
      ported browser-rs. Never per-request spawn, never the 41 core tools.
- [ ] `clipboard_get`/`clipboard_set` (audit item 3, d3sk-mcp MIT as
      reference) - text-only, Windows clipboard via the windows crate
      (needs Win32_System_DataExchange + Win32_System_Memory features).
- [ ] `edit_file` in-place editing (audit item 1, octofs Apache-2.0
      line_hash.rs as reference): exact old_text/new_text with unique-match
      enforcement, or line-id targeting with stale detection whose errors
      show fresh ids around the target and relocate the hash nearest-first;
      replace_file stays gated (whole-file overwrite).
- [ ] Optional someday: WASM/MCP plugin bridge (`zeroclaw-plugins`) — only
      as an `mcp_tool` client tenant type, never a dependency of the core

## Pending (GPU/CPU backend — pre-tool-calling)
- [x] Test A (`--cpu` → CPU backend ready) — pass
- [x] Test B (auto → GPU backend ready) — pass
- [x] Test C (GPU exe renamed → auto CPU fallback → restored) — pass
- [x] Commit: `c5b16e3` (backend) + `fbb9d34` (docs)
