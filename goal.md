# GOAL.md — IGRIS: from voice assistant to full desktop agent

North-star document. What IGRIS must ultimately do, category by category,
spoken command by spoken command. Companion docs: `tool_calling_plan.md`
(how tool calling gets built), `todo.md` (current work items),
`graphyfy/GRAPH.md` (current architecture).

---

## Mission

IGRIS is a **voice-first AI assistant for Windows**: you hold push-to-talk,
speak naturally, and it understands, decides, acts on the machine, and
answers out loud. Today it can **listen (Moonshine STT) → think (LFM3-1.7B
via llama.cpp, GPU-with-CPU-fallback) → speak (piper TTS)**. The tool-calling
bridge (ZeroClaw parser + tool registry) turns that pipeline into an agent:
**the LLM's intent recognition + tool selection + argument generation +
decision making drive real actions on this laptop.**

```
voice in → STT → LLM (intent + tool choice + args) → tool execution
        → results back to LLM → spoken answer → voice out
```

Every table below is that loop with a different tool behind it.

> **The tables are examples, not a fixed scope.** Tool calling is
> **dynamic**: the tool registry grows over time (code-registered seed set
> → user-defined `tools.toml` tools → MCP/WASM plugin servers), and the
> prompt the LLM sees is generated from whatever is registered right now.
> One generic tool + generated arguments already covers many rows (e.g.
> `open_app` covers every "Open …" command); new spoken abilities come from
> adding tools to the registry, not from rewriting the core.

---

## 1. Desktop and application control

| You say | IGRIS performs | Tools needed |
|---|---|---|
| Open Chrome | Launches Chrome | `open_app` (allowlist) |
| Open my coding project | Opens the project | `open_app` + `memory` (project paths) |
| Close Discord | Closes Discord | `close_app` (process kill — gated) |
| Switch to VS Code | Changes active window | `focus_window` (Win32) |
| Minimize all windows | Minimizes windows | `minimize_all` (Win32) |
| Lock my laptop | Locks Windows | `lock_screen` (rundll32 call) |
| Turn volume to 40% | Adjusts system volume | `set_volume` |
| Take a screenshot | Captures screen | `screenshot` |

## 2. Music and entertainment

| You say | IGRIS performs | Tools needed |
|---|---|---|
| Play Rehbara on Spotify | Searches and plays song | `spotify_play` (integration) |
| Play Arijit Singh on YouTube | Finds artist/music | `youtube_play` (browser automation) |
| Pause music | Pauses playback | `media_key` (pause) |
| Skip this song | Plays next track | `media_key` (next) |
| Play my downloaded music | Searches local music | `file_search` + `open_app` |
| Play something relaxing | Searches suitable music | `web_search` / local library |
| Increase volume | Changes volume | `set_volume` |

## 3. File and folder management

| You say | IGRIS performs | Tools needed |
|---|---|---|
| Find my IGRIS README | Searches files | `file_search` (glob + content) |
| Create a folder called Projects | Creates folder | `create_folder` |
| Rename this file | Renames selected file | `rename_file` |
| Find all PDF files | Searches PDFs | `file_search` (glob *.pdf) |
| Read my README | Extracts file contents | `read_file` |
| Copy this folder to D drive | Copies folder | `copy_path` |
| Delete temporary files | Finds and deletes approved files | `file_search` + `delete_file` (**confirmed only**) |

## 4. Developer assistant

| You say | IGRIS performs | Tools needed |
|---|---|---|
| Create a Rust project called assistant | Runs cargo project creation | `run_command` (allowlisted cargo) |
| Check my Rust project for errors | Runs cargo check | `run_command` (cargo check) + read output |
| Add Tokio dependency | Updates Cargo.toml | `run_command` (cargo add) |
| Open my GitHub repository | Opens repository | `open_url` |
| Show me the project structure | Lists directories | `list_dir` |
| Find where this function is defined | Searches source code | `content_search` |
| Run my application | Builds and launches project | `run_command` (cargo run) |
| Explain this error | Reads and explains compiler output | `read_file` + the LLM itself |

## 5. Browser and internet assistant

| You say | IGRIS performs | Tools needed |
|---|---|---|
| Search latest AI models | Searches web | `web_search` |
| Find free LLM APIs | Searches and collects results | `web_search` + `web_fetch` |
| Open GitHub | Opens GitHub | `open_url` |
| Find a Rust tutorial | Searches tutorials | `web_search` |
| Summarize this webpage | Reads and summarizes page | `web_fetch` + LLM |
| Download this PDF | Downloads document | `download_file` |
| Search for a particular GitHub repository | Finds repositories | `web_search` |

## 6. Communication assistant

| You say | IGRIS performs | Tools needed |
|---|---|---|
| Send a WhatsApp message to Rahul | Prepares/sends message | WhatsApp integration (deferred) |
| Open my Gmail | Opens Gmail | `open_url` |
| Find my latest college email | Searches emails | email integration (deferred) |
| Draft a reply to this email | Creates email draft | email integration (deferred) |
| Send this message to my friend | Sends via authorized integration | integration + permission |
| Read my notifications | Retrieves supported notifications | OS integration (deferred) |

> Messaging and email require appropriate integrations and account
> permissions — build last, behind explicit consent.

## 7. System monitoring and control

| You say | IGRIS performs | Tools needed |
|---|---|---|
| How much RAM am I using? | Reads RAM usage | `sys_stats` |
| Check GPU utilization | Reads GPU stats | `sys_stats` (nvidia-smi/wmi) |
| Is my laptop overheating? | Reads temperature sensors | `sys_stats` |
| Check my internet speed | Runs a speed test | `speedtest` (download probe) |
| Show Wi-Fi information | Reads network information | `net_info` (netsh) |
| Enable battery saver | Changes power settings | `power_mode` |
| Shut down my laptop | Requests shutdown | `shutdown` (**spoken confirmation required**) |

## 8. Memory and personal assistant

| You say | IGRIS performs | Tools needed |
|---|---|---|
| Remember my project directory | Saves a memory | `memory_save` (persistent store) |
| What project am I working on? | Retrieves memory | `memory_recall` |
| What was my last task? | Retrieves task history | `memory_recall` (activity log) |
| Remind me to study at 8 PM | Creates reminder | `reminder_set` (scheduler) |
| Remember my preferred music app | Saves preference | `memory_save` |
| What dependencies did I add yesterday? | Searches activity history | `memory_recall` (filtered) |

## 9. Advanced agentic tasks (multi-step planning)

Harder for LFM3-1.7B — these need **multiple tools per turn, planning,
verification**, and eventually a stronger model path (cloud fallback or a
bigger local model) for the planning step.

| You say | Expected workflow |
|---|---|
| Organize my Downloads folder | Inspect → categorize → move files |
| Find duplicate files | Scan → compare hashes → report |
| Fix my Rust compilation errors | Inspect → diagnose → edit → test |
| Set up a new Rust dev environment | Install dependencies → configure |
| Research the latest STT models | Search → compare → summarize |
| Create a presentation about AI | Research → generate slides |
| Clean up my project | Analyze unused files → suggest changes |

---

## Rollout phases

| Phase | Scope | Categories unlocked |
|---|---|---|
| **v1 — tool calling foundation** | ZeroClaw parser + prompt protocol + registry: `get_time`, `calculate`, `open_app` (allowlist) | 1 (partial) |
| **v1.1 — system & files** | `set_volume`, `screenshot`, `sys_stats`, `media_key`, `file_search`, `read_file`, `create_folder`, `open_url` | 1, 3, 5, 7 (read-only) |
| **v1.2 — memory** | persistent memory store + reminders + activity log | 8 |
| **v2 — developer & browser** | allowlisted `run_command` (cargo…), `web_search`, `web_fetch`, `download_file` | 4, 5 |
| **v2.5 — gated actions** | `close_app`, `delete_file`, `shutdown`, power control — each behind spoken confirmation | 1, 3, 7 (write) |
| **v3 — integrations & agentic** | Spotify/WhatsApp/email integrations, window control, multi-tool planning chains (stronger planner) | 2, 6, 9 |

## Safety rules (all phases)

- **Allowlists**: apps, commands, and writable paths are hardcoded consts —
  never free-form.
- **Read-only by default; writes are additive or gated**: reads need no
  confirmation. The only writes an early-phase IGRIS performs without a
  confirmation are additive-only and bounded: `download_file` saves into
  the Downloads folder only, and the allowlisted dev scaffolding
  (`cargo new` / `cargo init`) creates a project folder where the user
  points it — neither modifies or deletes anything that already exists.
  Any write that changes or removes existing state is confirmation-gated
  (spoken "haan karo" or hotkey hold, machinery lands in v2.5).
- **Destructive ops** (`delete`, `shutdown`, messaging) always require a
  two-step confirm; IGRIS states what it is about to do first.
- **Voice sandbox**: no arbitrary shell in the early phases; `run_command`
  arrives only as a curated allowlist (cargo, git status…).

## Success criteria

- [ ] 90%+ of category-1 v1 commands work hands-free in `assist --hotkey`.
- [ ] A spoken command → completed action end-to-end in < 10 s (LLM + tool).
- [ ] Zero tool-call envelopes ever spoken aloud (hold-back guard).
- [ ] Every gated action logs what was done (activity history).
- [ ] Memory survives restarts (persistent store).
- [ ] Category-9 tasks degrade gracefully: IGRIS says what it *can* do and
      asks instead of guessing.
