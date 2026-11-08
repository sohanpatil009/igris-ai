# AGENTS.md — igris-ai

Standing rules for any agent working in this repo. Full roster dispatcher:
run `/agents <task>` (global command, defined in `~/.config/opencode/commands/agents.md`).

## Default delegation policy (set by user, 2026-10-31)

Do not work solo on multi-step tasks. Route through the harness subagents by
default; the parent agent keeps judgment, integration, and verification.

| Work | Default agent |
|---|---|
| Recon / codebase questions before touching code | `explore` (medium) |
| Implementation plan for a feature/refactor | `planner` (or `architect` for design questions) |
| Review of every Rust change, before calling work done | `rust-reviewer` |
| Cargo compile/link/feature errors | `rust-build-resolver` (minimal diffs only) |
| Anything touching paths, shell, permissions, keys | `security-reviewer` |
| Docs / codemaps kept in sync with code | `doc-updater` |
| Self-contained parallelizable pieces | `general` |
| Library/API documentation questions | `docs-lookup` |
| Other languages/frameworks (this repo: rare) | matching `*-reviewer` / `*build-resolver` |

Rules:

1. Spawns state: goal, files in scope, output contract, skills to load, and any
   "do not X" rules (e.g. "do not run cargo" while a build holds the lock).
2. Two background agents never write the same path.
3. Verification is parent-owned: `cargo fmt --check`, `cargo check --bin igris_ai`,
   `cargo test`, `cargo clippy` on touched areas before "done" is claimed.
4. Reference agents: `igris-ai` ships standalone — never integrate the
   `igris-ecosystem`, `igris-protocol`, or `igris-andriod` repos without an
   explicit user instruction.

## Project facts (so agents don't re-derive them)

- Crate: `igris-ai` (Rust 2021, single lib + `src/bin/igris_ai.rs` REPL; optional
  `with_riva` (gRPC STT) and `piper_ffi` (native TTS) cfgs set by `build.rs`).
- Plans live in `TODO.md` (long-range) and `plan.md` (4-week sprint, Week 1 =
  execution core). Update the matching checkbox when an item lands.
- Working tree is intentionally uncommitted (large baseline). Do not commit,
  stash, or revert without the user asking.
- Architecture graph: `graphyfy/GRAPH.md` (module + flow map; keep it valid when
  structure changes significantly).
