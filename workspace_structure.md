# IGRIS Workspace Structure

## Directory Layout

```
D:\igris-ai/
├── crates/
│   ├── igris-core/   # agent core lib: STT, LLM, TTS, toolparse, tools, assist, live, hotkey
│   └── igris-cli/    # reference terminal front-end (binary keeps the name igris-ai)
├── assets/          # audio assets, model weights, configs
├── graphyfy/        # ZeroClaw integration, graph generation
├── scripts/         # PowerShell scripts (gen_tts.ps1)
├── target/          # Build artifacts
├── .gitignore
├── Cargo.lock
├── Cargo.toml
├── goal.md
├── problem_faced.md
├── README.md
├── todo.md
└── tool_calling_plan.md
```

## Key Files

- **Cargo.toml**: Workspace dependencies (LLM, TTS, STT)
- **Cargo.lock**: Locked versions for reproducibility
- **main.rs**: Entry point, dispatches voice commands
- **assist.rs**: Core logic, voice command parsing, LLM orchestration
- **live.rs**: Audio session, mic control, streaming
- **hotkey.rs**: CTRL+SPACE trigger, fake mic test
- **tts.rs**: Piper TTS output
- **llm.rs**: llama.cpp integration
- **zeroclaw**: Tool calling integration (graph, parser)

## Architecture

- **Voice-first**: Push-to-talk, natural speech input
- **Streaming**: Sentence-by-sentence audio output
- **LLM**: llama.cpp (LMF3-1.7B), GPU/CPU fallback
- **TTS**: Piper speaker
- **Tools**: ZeroClaw integration for agent actions
- **Hotkey**: CTRL+SPACE for voice commands

## Roadmap

- **Phase 0**: Toolchain setup (DONE)
- **Phase 1**: Core voice assistant
- **Phase 2**: ZeroClaw integration
- **Phase 3**: Full agent capabilities

## Documentation

- **goal.md**: North-star mission
- **todo.md**: Current work items
- **tool_calling_plan.md**: Tool integration details
- **bug.md**: Known bugs and suspicions
- **README.md**: Overview
- **problem_faced.md**: Challenges encountered

---

**Last updated**: 2026-11-12
**Status**: Proposal awaiting review
