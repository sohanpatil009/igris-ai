# IGRIS AI — Standalone Voice Assistant

IGRIS AI is an independent, local-first + online-ready desktop assistant built in Rust.

## Current Status (Week 3 — knowledge / dev workflow, 2026-10-31)

- Library crate with offline/online speech, NLU, reasoning, and installer primitives.
- Standalone binary: `igris_ai` (text REPL + agent loop).
- Tool core (plan.md Weeks 1-3, verified): async `Tool` with JSON schemas + hand-rolled registry validation, structured outcomes/errors, OS control suite (app/window/volume/power/brightness/screenshot/clipboard/files/media/input/browser), plus `read_document`, timers/reminders, `get_weather`, `web_search` (Brave/Tavily), and dev/git tools.
- Agent loop: online mode routes through LLM intent planner and executes multi-step plans; offline mode falls back local NLU (plus optional Ollama local reasoning).
- Online mode controlled by env `IGRIS_ONLINE_MODE=true`.
- Tests: 62 lib + 7 smoke + doc-tests green; cargo check and clippy clean on touched files.

## Build

```powershell
cargo build --bin igris_ai
.\target\debug\igris_ai.exe
```

## Commands

- `help`, `online`, `offline`, `run <text>`, `nlu <text>`, `reason <text>`, `speak <text>`
- `tools`, `tool <name> [json]`
- `pending`, `approve <id>`, `deny <id>`
- `voice-loop`, `quit`

## Security Baseline

- Capability labels: ReadOnly, Standard, Sensitive, Admin.
- Destructive tools are approval-per-call and not persistently grantable (`power_control`, `input_automation`, `file_trash`).
- `shell_run` is allowlisted with class-based metachar blocking.
- `read_file`/`file_search`/`file_trash` operate inside a canonicalized path sandbox (default cwd).
- All decisions are audited in `%APPDATA%\igris-ai\audit.log`.

## Environment (optional, fallback safely if unset)

- `IGRIS_ONLINE_MODE=true` — NVIDIA NIM online mode.
- `IGRIS_FILE_ROOTS=path1;path2` — extra path-sandbox roots (default: cwd).
- `BRAVE_SEARCH_API_KEY` / `TAVILY_API_KEY` — API web search; else opens browser.
- `OPENWEATHER_API_KEY` — OpenWeather weather; else opens search.
- `IGRIS_GGUF_MODEL` or `IGRIS_GGUF` — direct GGUF path via `llama-cli` or `IGRIS_LLM_BIN` (no Ollama required).
- `OLLAMA_HOST` / `OLLAMA_MODEL` — optional Ollama fallback.

## Roadmap

See [TODO.md](TODO.md) and [plan.md](plan.md). Build challenges are logged in [challenge.md](challenge.md). Module graph in [graphyfy/GRAPH.md](graphyfy/GRAPH.md).
