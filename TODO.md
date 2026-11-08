# TODO — IGRIS AI Voice Assistant (Standalone Crazy Desktop Assistant)

> **Status:** Week 1 execution core **complete & verified** (2026-10-31): async tool registry + schemas + sandbox + single-use approval flow + agent loop live in `src/tools/mod.rs` / `src/bin/igris_ai.rs`; reviewers' fix pass applied; tests green (62 lib + 8 smoke). See `plan.md` §1 and `challenge.md` for the build log.
> **Constraint:** Keep `igris-ai` a **separate, individual app**. Do **not** integrate it into `igris-ecosystem`, `igris-protocol`, or `igris-andriod` right now.

---


## Progress checkpoint (2026-10-31)

- Week 1 (execution core): COMPLETE, reviewed and gated (cargo test 62 lib + smoke suite green; clippy/check clean). Async tool trait, structured outcomes/errors, fail-closed schema validation, path sandbox, single-use approvals, intent agent loop.
- Week 2 (Windows control): COMPLETE. 13 OS control tools registered and security audit folded.
- Week 3 (knowledge/dev workflow): SUBSTANTIALLY COMPLETE. WebSearch v2, read_document, set_timer/set_reminder, get_weather, dev/git tools, semantic memory and conversation context persistence, local GGUF/Ollama fallback.
- Week 4 (platform/release): STARTING. Graph updated; MCP client exists, Tauri shell, benchmark/reliability/release build remain.
- Verification guidance: see plan.md ticks and challenge.md for per-milestone evidence.

---


## 0. Product Vision

Build a **fast, accurate, hands-free desktop AI assistant** that can:

1. Listen continuously with wake word (“Hey IGRIS”, etc.).
2. Understand natural language commands and questions.
3. Execute multi-step tasks on the user’s laptop.
4. Control apps, windows, files, browser, terminal, code, documents, media, system settings.
5. Answer questions using online APIs (LLM, web search, weather, calendar) and offline (local LLM/STT/TTS).
6. Speak responses naturally with low latency.
7. Remember context, preferences, routines, recent actions, undo.
8. Be safe: explicit permissions, confirmations for dangerous actions, audit log.
9. Run well on the target hardware:
   - Windows 11 x64
   - 8 GB RAM
   - Intel Iris Xe + NVIDIA GeForce MX350 (2 GB VRAM)
   - Prefer CPU/offload-friendly models and quantized GGUF/ONNX.

---

## 1. Reference Assistants & Inspirations

Study and borrow design patterns from (not necessarily code):

### Open-source assistants
- [ ] Leon AI — intent-based assistant architecture, modules, skills, NLU.
- [ ] Open Interpreter — code execution, multi-step reasoning, safety.
- [ ] Mycroft / OVOS / Neon — voice pipelines, skills framework, wake word, STT/TTS plugins.
- [ ] Rhasspy — local voice assistant pipeline, wake word, intent recognition.
- [ ] WhisperLive / faster-whisper — streaming STT.
- [ ] VOSK — offline keyword/commands/STT.
- [ ] Piper — offline TTS.
- [ ] Sherpa-ONNX — STT/TTS/VAD/wake word.
- [ ] Llamafile / Ollama / llama.cpp — local LLM serving.
- [ ] AutoGPT / LangChain agents — tool use, planning, memory.
- [ ] Pipecat / Livekit Agents — conversational pipelines.

### Closed-source assistants (capability model)
- [ ] Siri — device/app control, shortcuts, reminders, messages, calls.
- [ ] Google Assistant — search, smart home, multi-step, reminders, routines.
- [ ] Alexa — skills, routines, timers, smart home, music/media.
- [ ] Cortana legacy + Windows Copilot — OS integration, files, apps, settings.
- [ ] Apple Intelligence / ChatGPT voice — multimodal interactions, summarization, drafting.

### Key capability groups to replicate
- [ ] Voice conversation with barge-in/interruption.
- [ ] Wake word + push-to-talk.
- [ ] Streaming STT and streaming TTS.
- [ ] Multi intent in one sentence.
- [ ] Confidence scoring and clarification.
- [ ] Tool calling with structured args.
- [ ] ReAct-style reasoning loop.
- [ ] Permission-aware execution.
- [ ] Recoverable retries.
- [ ] Memory and personalization.
- [ ] Plugin/skill ecosystem.
- [ ] OS automation.
- [ ] Coding workflows.
- [ ] Document understanding.
- [ ] Media control.
- [ ] Calendar/reminder/alarm.

---

## 2. Capability Matrix For v1 / v2 / v3

### A. Voice and Natural Language
- [ ] Wake word detection with multiple wake phrases.
- [ ] Hands-free always-listening mode.
- [ ] Push-to-talk hotkey.
- [ ] Voice Activity Detection.
- [ ] Barge-in / interrupt assistant while speaking.
- [ ] Speech-to-text (online + offline).
- [ ] Text-to-speech (online + offline).
- [ ] Streaming STT.
- [ ] Streaming TTS.
- [ ] Emotion/style-aware TTS (optional).
- [ ] Multi-language STT/TTS scaffold.
- [ ] Conversation context memory.
- [ ] Clarification questions.
- [ ] Confidence thresholding.
- [ ] Disambiguation (“which app do you mean?”).
- [ ] Noise reduction / echo cancellation hooks.

### B. Laptop / Application Control
- [ ] Launch apps by name/alias.
- [ ] Close apps.
- [ ] Switch windows.
- [ ] Focus app.
- [ ] Launch websites.
- [ ] Search web.
- [ ] Minimize/maximize windows.
- [ ] Split screen.
- [ ] Brightness control.
- [ ] Volume control.
- [ ] Wi-Fi / Bluetooth toggles (where supported).
- [ ] Lock / sleep / restart / shutdown with confirmation.
- [ ] Battery / system info.
- [ ] Screenshot.
- [ ] Clipboard read/write.
- [ ] Notifications.
- [ ] Keyboard shortcut execution.
- [ ] GUI automation (type, click, scroll).
- [ ] Multi-step automation workflows.
- [ ] Window layout presets.
- [ ] Media key control.

### C. Coding / Terminal
- [ ] Run shell commands (PowerShell/Bash).
- [ ] Explain output.
- [ ] Inspect errors.
- [ ] Write/modify code files.
- [ ] Scaffold project.
- [ ] Run tests.
- [ ] Git status/diff/commit helper.
- [ ] Package install (with confirmation).
- [ ] Open repo in editor.
- [ ] Run scripts.
- [ ] Code navigation.
- [ ] Terminal session management.
- [ ] Safety allowlist/blocklist.

### D. Files / Documents / Images
- [ ] Search files by name/content.
- [ ] Open file.
- [ ] Organize folders.
- [ ] Create/rename/move/delete files (confirm delete).
- [ ] Summarize text/PDFs.
- [ ] Draft emails/letters/reports/spreadsheets.
- [ ] Extract structured info.
- [ ] Image understanding (vision model API/local if available).
- [ ] OCR.
- [ ] Archive/compress.
- [ ] Duplicate finder.
- [ ] Safe path sanitization.
- [ ] Privacy mode: no cloud upload.

### E. Planning / Agent Execution
- [ ] Break request into subtasks.
- [ ] Select tools per step.
- [ ] Pass outputs between steps.
- [ ] Retry on transient errors.
- [ ] Resume from stored state.
- [ ] Long-running task queue.
- [ ] Scheduled tasks.
- [ ] Routines (morning routine, focus mode).
- [ ] Undo stack for reversible actions.
- [ ] User approval gates.
- [ ] Result narration.

---

## 3. Target Architecture (Standalone App)

```
igris-ai-app/
├─ Cargo.toml              # binary crate depending on igris-ai lib (local)
├─ src/
│  ├─ main.rs              # app entry, tokio runtime, signal handling
│  ├─ audio.rs             # audio device mgmt, capture loop
│  ├─ voice_pipeline.rs    # wake word -> STT -> intent -> action -> TTS
│  ├─ assistant.rs         # conversation orchestrator
│  ├─ tools/               # OS tool implementations
│  ├─ permissions.rs       # capability policy
│  ├─ memory.rs            # persistent state/context
│  ├─ scheduler.rs         # alarms/reminders/routines/timers
│  ├─ plugins/             # skill loader
│  ├─ ui/                 # optional TUI/GUI
│  └─ config.rs
├─ models/                 # model downloads (managed by setup_manager)
├─ pkg/                    # binaries and assets
├─ skills/                 # JSON/YAML skill definitions
├─ tests/
└─ TODO.md
```

### Design principles
- [ ] Keep core logic in `igris-ai` library.
- [ ] Add a small app/binary crate, either in a new `igris-ai-app` dir or `igris-ai/src/bin/` without changing ecosystem repos.
- [ ] No integration into `igris-ecosystem`.
- [ ] Every destructive action gated by policy + confirmation.
- [ ] Async everywhere, no blocking main thread.
- [ ] Event bus for voice state transitions.
- [ ] Plugins are declarative + sandboxed.

---

## 4. Hardware-Aware Model Strategy

Target: Windows 11, 8GB RAM, Intel Iris Xe + NVIDIA MX350 (2GB VRAM).

### Recommended default offline stack
- [ ] STT: Sherpa-ONNX SenseVoice small (already present) or Whisper tiny/base optimized via ONNX.
- [ ] VAD: Silero VAD via Sherpa-ONNX.
- [ ] Wake word: Sherpa-ONNX keyword spotter or Porcupine-like.
- [ ] TTS: Piper medium/high voice (CPU).
- [ ] LLM: Qwen2.5 3B/7B Q4 (CPU/GPU hybrid), Phi-3.5 Mini Q4, or Llama 3.2 3B.
- [ ] Embeddings: SBERT already present (384-dim).
- [ ] Vision: online API first; offline small vision model optional.

### Performance rules
- [ ] Keep resident model RAM under ~3.5 GB.
- [ ] Keep first-response latency target < 2 sec for simple commands.
- [ ] Lazy-load heavy models only when needed.
- [ ] Cache embeddings and TTS audio.
- [ ] Cancel in-flight tasks when interrupted.
- [ ] Use MX350 for STT/LLM acceleration only if it improves latency; otherwise CPU with SIMD.
- [ ] Benchmark every model choice with `criterion` or simple timing.

### Online mode
- [ ] Support NVIDIA NIM (existing).
- [ ] Support OpenAI-compatible API.
- [ ] Support Groq.
- [ ] Support Google Gemini.
- [ ] Support Anthropic (optional).
- [ ] User supplies API keys via secure local config.
- [ ] Fallback chain: primary online -> secondary -> offline.

---

## 4A. Locked Model Decisions (Leader Selection)

These are the selected defaults and fallback chains. Adjust only if benchmarks fail.

### Speech-to-Text
- **Online (primary):** NVIDIA Riva Parakeet / Riva Streaming ASR (best accuracy + latency).
- **Online fallback:** Groq Whisper large-v3 (fast).
- **Offline primary:** Sherpa-ONNX SenseVoice small (already integrated, multilingual).
- **Offline fallback:** Whisper base via ONNX (faster-whisper CPU) for English-only optimization.
- **VAD:** Silero VAD via Sherpa-ONNX.
- **Wake word:** Sherpa-ONNX keyword spotter with phrase list:
  - "hey igris", "ok igris", "igris".

### Text-to-Speech
- **Online primary:** ElevenLabs / Cartesia (highest naturalness), user key required.
- **Online fallback:** OpenAI TTS (fast + good).
- **Offline primary:** Kokoro-82M ONNX (best quality at low footprint).
- **Offline fallback:** Piper `en_US-libritts_r-medium` (already in installer).
- **Voice style:** warm professional, configurable per persona.
- **Music / effects:** optional sound cues for wake/end, toggleable.

### LLM / Reasoning
- **Online:** OpenAI gpt-4.1 / Claude Sonnet 4.5 / Gemini 2.5 Pro (provider switchable).
- **Online fast mode:** Groq Llama 3.3 70B (low latency).
- **Offline primary:** Qwen2.5 7B Q4_K_M (balanced speed/accuracy).
- **Offline fast:** Qwen2.5 3B Q4 or Phi-3.5 mini.
- **Embeddings:** `BAAI/bge-small-en-v1.5` (upgrade from current SBERT 384, keep SBERT compatibility).
- **Vector memory:** use BGE embeddings in SQLite + optional FAISS/Qdrant lite.

### Image / Vision
- **Online:** GPT-4o / Claude 3.5 Sonnet vision.
- **Offline:** optional Phi-3.5 Vision or skip; default to online only.

### Model load policy
- [ ] Lazy load every heavy model.
- [ ] Load STT+VAD at startup (low footprint).
- [ ] Load TTS on first speak.
- [ ] Load LLM on first reasoning.
- [ ] Unload LLM after idle timeout (5 min).
- [ ] Keep RAM target < 3.5 GB in offline mode.

---

## 12. Enterprise Quality Bar

### Product / UX Standards
- [ ] WCAG 2.2 AA accessibility.
- [ ] Keyboard-first navigation.
- [ ] High-contrast mode.
- [ ] Clear status indicators (listening, thinking, executing, speaking).
- [ ] First-run onboarding under 3 minutes.
- [ ] Single installer with model download progress.
- [ ] Crash recovery and auto-restart watchdog.
- [ ] Error messages in plain language with next action.
- [ ] Undo for all reversible operations.
- [ ] Confirmation for dangerous operations.
- [ ] Audit log viewer.
- [ ] Offline-first resilience.

### UI Stack Decision
- [ ] Use **Tauri v2 + React + TypeScript + Tailwind + shadcn/ui** for frontend.
- [ ] Rust backend commands for all OS operations.
- [ ] System tray + settings window + compact always-on-top mini overlay.
- [ ] Dark/light theme with enterprise color tokens.
- [ ] Telemetry disabled by default.
- [ ] Optional analytics opt-in.

### Security Policy Architecture
- [ ] Capability-based permission model:
  - read-only
  - standard
  - sensitive
  - admin
- [ ] Every tool declares required capability.
- [ ] Persistent grant store with scope + expiry.
- [ ] Revoke any permission anytime.
- [ ] Dangerous actions require explicit confirmation.
- [ ] Command allowlist and blocklist.
- [ ] Path traversal protection.
- [ ] Shell command risk classifier.
- [ ] API keys in OS credential manager.
- [ ] Signed update design.
- [ ] No telemetry without consent.
- [ ] Secure default config.
- [ ] Audit log (immutable append-only local file).
- [ ] Threat model document.
- [ ] Dependency scanning.
- [ ] Fuzzing for parsers.

### Reliability & Performance Targets
- [ ] Cold start < 5 sec.
- [ ] Wake-to-ready transcription < 2 sec.
- [ ] Simple assistant response < 3 sec in online fast mode.
- [ ] Offline command response < 5 sec common tasks.
- [ ] STT WER < 10% on clean English audio.
- [ ] Memory < 3.5 GB offline.
- [ ] CPU usage < 20% idle.
- [ ] GPU optional, never required.
- [ ] No crash on missing models (graceful fallback).

### Enterprise Documentation
- [ ] Architecture document.
- [ ] Security model.
- [ ] API/tool reference.
- [ ] Plugin author guide.
- [ ] Deployment guide.
- [ ] Privacy statement.
- [ ] Model license matrix.
- [ ] Troubleshooting guide.
- [ ] Release notes format.

## 5. Major Work Phases

### Phase 0 — Baseline & Documentation (1-2 days)
- [x] Crate builds.
- [ ] Add `TODO.md` (this file).
- [ ] Add `README.md` for standalone app.
- [ ] Add architecture doc (`ARCHITECTURE.md`).
- [ ] Add model license registry.
- [ ] Add `.env.example`.
- [ ] Fix `build.rs` `piper_ffi` cfg warning.
- [ ] Add `cargo fmt`, `cargo clippy` baseline.
- [ ] Add smoke test.

### Phase 1 — Standalone App Shell (3-5 days)
- [ ] Create binary entrypoint.
- [ ] Load config.
- [ ] Initialize bridge.
- [ ] Initialize models.
- [ ] Terminal/chat mode for testing.
- [ ] Graceful startup/shutdown.
- [ ] Logging with `tracing`.
- [ ] Health check command.

### Phase 2 — Voice Pipeline (5-7 days)
- [ ] Audio device selection.
- [ ] Wake word.
- [ ] Push-to-talk hotkey.
- [ ] VAD tuning.
- [ ] STT offline.
- [ ] STT online.
- [ ] TTS offline.
- [ ] TTS online.
- [ ] Barge-in.
- [ ] Streaming pipeline.
- [ ] Voice state machine.
- [ ] Noise profile.

### Phase 3 — Intent & Tool System (5-7 days)
- [ ] Define tool trait.
- [ ] JSON schema validation.
- [ ] Tool registry.
- [ ] Basic tools (open app, volume, brightness, screenshots, clipboard, files).
- [ ] App aliases.
- [ ] Web search tool.
- [ ] Email opener.
- [ ] Permission policy.
- [ ] Confirmation dialog.
- [ ] Undo registry.

### Phase 4 — Agent / Multi-Step Execution (7-10 days)
- [ ] ReAct loop.
- [ ] Planner.
- [ ] Step executor.
- [ ] Memory/context store.
- [ ] Tool output passing.
- [ ] Retry and recovery.
- [ ] Task state persistence.
- [ ] Progress narration.
- [ ] Approval gates.
- [ ] Cancel task.

### Phase 5 — Coding & Terminal Assistant (5-7 days)
- [ ] Safe shell executor.
- [ ] cwd/session management.
- [ ] Code generation.
- [ ] File edit tools.
- [ ] Test runner.
- [ ] Git helper.
- [ ] Error diagnosis.
- [ ] Diff preview.
- [ ] Rollback.

### Phase 6 — Files, Documents, Images (5-7 days)
- [ ] File search.
- [ ] PDF/text summarization.
- [ ] Document Q&A.
- [ ] Email/draft templates.
- [ ] Spreadsheet generation.
- [ ] OCR.
- [ ] Image understanding (API).
- [ ] Folder organization rules.
- [ ] Safe write constraints.

### Phase 7 — Memory & Personalization (4-5 days)
- [ ] User profile.
- [ ] Preferences.
- [ ] Routines.
- [ ] Knowledge base / notes.
- [ ] Reminders/alarms.
- [ ] Conversation history.
- [ ] Semantic memory search.
- [ ] Export/import settings.

### Phase 8 — Plugins / Skills (4-5 days)
- [ ] Declarative skill format.
- [ ] Plugin loader.
- [ ] Hot reload (optional).
- [ ] Skill marketplace structure.
- [ ] Safety review.
- [ ] Example skills.

### Phase 9 — UI / UX (5-7 days)
- [ ] Minimal tray app.
- [ ] Transcript view.
- [ ] Settings UI.
- [ ] Permissions UI.
- [ ] Voice waveform.
- [ ] Shortcut customization.
- [ ] Onboarding wizard.
- [ ] Notification feedback.

### Phase 10 — Quality, Testing, Benchmarking (continuous)
- [ ] Unit tests.
- [ ] Integration voice-loop tests.
- [ ] Safety tests.
- [ ] Latency benchmarks.
- [ ] STT WER tests.
- [ ] Memory use tests.
- [ ] Fuzz file/path handling.
- [ ] Regression prompt suite.
- [ ] Release packaging.

---

## 6. Detailed Task Backlog

### 6.1 Repository / Build

- [ ] Decide binary placement:
  - Option A: add `igris-ai/src/bin/igris_ai.rs` without publishing crate API changes.
  - Option B: create sibling `igris-ai-app` crate depending on `igris-ai`.
- [ ] Add workspace `Cargo.toml`? (optional, no multi-repo integration requirement).
- [ ] Fix cfg warning in `build.rs`:
  ```rust
  println!("cargo::rustc-check-cfg=cfg(piper_ffi)");
  ```
- [ ] Add clippy allow list cleanup.
- [ ] Add `rustfmt.toml`.
- [ ] Add CI script (`cargo fmt`, `cargo clippy`, `cargo test`, `cargo build`).
- [ ] Add `just` or `make` commands.

### 6.2 Voice Pipeline

- [ ] Implement `voice_pipeline.rs` state machine:
  - Idle
  - ListeningForWakeWord
  - CapturingCommand
  - Transcribing
  - Reasoning
  - ExecutingTool
  - Speaking
  - ErrorRecovery
- [ ] Tune VAD:
  - energy threshold
  - hangover
  - min speech duration
  - trailing silence cutoff
- [ ] Add noise gate.
- [ ] Add echo suppression flag (do not capture while TTS).
- [ ] Streaming partial STT.
- [ ] Streaming partial TTS.
- [ ] Wake word confidence threshold.
- [ ] Voice enrollment (future).

### 6.3 STT

Current:
- [x] Sherpa-onnx SenseVoice transcription (`transcribe_audio`).
- [x] Hybrid transcribe.
- [ ] Add model selector offline (SenseVoice / Whisper).
- [ ] Add streaming transcription.
- [ ] Add punctuation restoration.
- [ ] Add multilingual auto-detect.
- [ ] Benchmark WER.

Online:
- [x] NVIDIA Riva STT (feature `with_riva`).
- [ ] OpenAI Whisper API.
- [ ] Google Speech API (if configured).
- [ ] Groq Whisper.
- [ ] Automatic fallback.

### 6.4 TTS

Current:
- [x] Piper/sherpa `speak`.
- [x] Piper FFI optional.
- [ ] Add non-blocking TTS queue.
- [ ] Add voice selection UI.
- [ ] Add streaming TTS.
- [ ] Add interruption.
- [ ] Cache rendered wave.
- [ ] Add voice speed/pitch.

### 6.5 NLU / Agent

Current:
- [x] NLU engine.
- [x] SBERT engine.
- [x] NER.
- [x] Context memory.
- [x] Online reasoning.
- [x] Intent router.
- [x] Task planner parser.
- [x] Define canonical tool schema (registry `schema()` + fail-closed `validate_args`, 2026-10-31).
- [x] Add function/tool execution loop (online `route_intent` → `execute_plan`, 2026-10-31).
- [ ] Unify local NLU + online planner into one `Agent` API.
- [ ] Add conversation history trimming.
- [ ] Add JSON-repair for LLM outputs.
- [ ] Add confidence + clarification.
- [ ] Add “do not hallucinate tool args” constraints.
- [ ] Add evaluation harness for intents.

### 6.6 OS Tools

Create a new `tools/` module:

- [x] `open_app`: resolve aliases and launch (allowlisted app names, 2026-10-31; full `app_control` Week 2).
- [ ] `close_app`.
- [ ] `focus_window`.
- [ ] `open_url`.
- [x] `web_search` (browser-tab v1; API-backed v2 in Week 3).
- [ ] `screenshot`.
- [ ] `clipboard_read/write`.
- [ ] `volume_set/get`.
- [ ] `brightness_set`.
- [x] `system_info`.
- [ ] `lock_screen`.
- [ ] `sleep`.
- [ ] `shutdown/restart` (confirm).
- [ ] `file_search`.
- [x] `file_read` (sandboxed `read_file`, cwd-only default).
- [ ] `file_write`.
- [ ] `file_delete` (confirm + trash).
- [ ] `folder_create`.
- [x] `run_command` (allowlist as `shell_run`, approval + metachar blocklist).
- [ ] `run_script`.
- [ ] `git_status/diff/log`.
- [ ] `create_code_project`.
- [ ] `email_compose`.
- [ ] `read_pdf`.
- [ ] `summarize_document`.
- [ ] `type_text`.
- [ ] `click_at` (accessibility automation).
- [ ] `press_keys`.
- [ ] `move_window`.
- [ ] `list_windows`.
- [ ] `toggle_dark_mode`.
- [ ] `set_timer`.
- [ ] `set_reminder`.
- [ ] `calendar_read` (if local integration supported).

### 6.7 Permission Policy
- [x] Define capability levels (ReadOnly / Standard / Sensitive / Admin).
- [x] Per-tool allow/ask/deny (registry + `PermissionPolicy::decision`).
- [x] Persist user decisions (`permissions.json`, atomic tmp+rename writes).
- [x] Audit log (`audit.log`, JSON lines, write-failure warnings).
- [ ] Rate limits.
- [x] Sandboxed command executor (shell allowlist + metachar blocklist; path sandbox for `read_file`).
- [x] Single-use approval flow with pending map + expiry (2026-10-31).

### 6.8 Memory
- [ ] SQLite or JSON store.
- [ ] Conversation turns.
- [ ] Task history.
- [ ] User facts.
- [ ] Preferences.
- [ ] Reminders.
- [ ] Routines.
- [ ] Vector memory using SBERT.
- [ ] Export/import.
- [ ] Clear memory command.

### 6.9 Plugins / Skills
- [ ] Skill manifest schema.
- [ ] Loading from `./skills`.
- [ ] Declarative intents.
- [ ] Script-based handlers.
- [ ] Permission declarations.
- [ ] Safety review.
- [ ] Examples: timer, weather, jokes, facts, unit conversion, search.

### 6.10 UI
- [ ] Start with CLI/TUI (fastest to iterate).
- [ ] Add system tray.
- [ ] Live transcript.
- [ ] Voice status indicator.
- [ ] Settings panel:
  - model selection
  - API keys
  - voices
  - hotkeys
  - permissions
- [ ] Onboarding wizard:
  - download models
  - microphone test
  - TTS test
  - API key setup
- [ ] Toasts for background tasks.

---

## 7. Safety, Privacy, and Compliance

- [ ] No hidden cloud uploads in offline mode.
- [ ] Clear mode indicator: online/offline.
- [ ] Redact secrets from logs.
- [ ] API keys stored securely (Windows Credential Manager or encrypted file).
- [ ] Confirm dangerous commands.
- [ ] Block destructive commands by default.
- [ ] Path traversal protection.
- [ ] Rate limits for shell.
- [ ] User data export/delete.
- [ ] Telemetry off by default.
- [ ] Privacy documentation.

---

## 8. Testing Plan

### Unit tests
- [x] Tool arg validation (smoke: unknown/missing/wrong-type, 2026-10-31).
- [x] Permission policy (smoke: Read-only allow, Sensitive approval, deny audit; lib: setup_manager defaults).
- [ ] Intent parsing.
- [x] Path sanitization (smoke: `..` traversal + `foo`/`foo2` sibling denial).
- [ ] Context memory.
- [ ] Undo.

### Integration tests
- [ ] Voice pipeline smoke (with fake audio).
- [ ] STT transcription sample.
- [ ] TTS render sample.
- [x] Tool execution sandbox (smoke: in-root read succeeds, sibling root denied, approval flow gated, 2026-10-31).
- [ ] Online mode mocked API.
- [ ] Offline mode startup.

### Evaluation
- [ ] STT WER dataset (10-20 local audio samples).
- [ ] Intent accuracy dataset (200+ commands).
- [ ] Latency benchmark:
  - wake->STT
  - STT->intent
  - tool exec
  - TTS start
- [ ] Memory footprint.
- [ ] GPU/CPU usage.

### Security tests
- [x] Path traversal (smoke: `..` pre-canonicalize denial).
- [x] Shell injection (smoke: newline/CR/`%`/`^`/`!`/NUL blocklist regression).
- [ ] Prompt injection (ignore hidden instructions in docs).
- [ ] Plugin malicious payload.
- [x] Unauthorized destructive action (smoke: ungranted Sensitive tool never runs; deny audits "rejected by user").

---

## 9. Release Checklist
- [ ] Build release binary.
- [ ] Bundle models via setup manager.
- [ ] Installer script (Windows first).
- [ ] README + usage.
- [ ] License + model license file.
- [ ] Crash reporting optional.
- [ ] Auto-update design.
- [ ] Changelog.

---

## 10. Immediate Next Actions (Recommended Order)
1. [x] Create standalone binary skeleton (no ecosystem integration) — `src/bin/igris_ai.rs`, REPL + agent loop.
2. [x] Document model assets and verify paths — `pkg/models/{parakeet,kokoro}` verified (plan.md baseline).
3. [x] Fix cfg warning and baseline `cargo test`.
4. [x] Build minimal text REPL using existing `NluEngine`, `reasonable_online`, and `speak`.
5. [x] Add tool registry with 3 safe tools (`system_info`, `open_app`, `web_search`) — now 5 tools incl. `read_file` + `shell_run`, with schemas + approval.
6. [x] Add permission policy — capability levels, persisted grants, audit log, take-first approvals.
7. [x] Add audio capture + wake word loop (`voice-loop` REPL arm).
8. [ ] Benchmark offline pipeline.
9. [ ] Week 2: `src/tools/os/` PowerShell helper + app/window/volume/power/brightness/screenshot/clipboard/file-search/trash/media/input/browser tools (plan.md 2.1–2.13).

---

## 11. Notes on Current Codebase
- `igris-ai` compiles as library.
- `Bridge` must be installed before engine use.
- STT available via Sherpa SenseVoice.
- TTS via Piper/sherpa.
- VAD/wake word/capture implemented.
- NLU + context + SBERT implemented.
- Online reasoning/intent routing implemented.
- NVIDIA NIM online mode implemented; Riva STT feature-gated.
- Setup manager handles model downloads/extraction/validation.
- No standalone app binary currently.
- No desktop GUI/tray currently in this crate.
- Keep this TODO updated as modules are implemented.
