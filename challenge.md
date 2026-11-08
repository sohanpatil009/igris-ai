# challenge.md — IGRIS AI Build Challenges

> Running log of real problems hit while building IGRIS AI and how they were
> solved. Each entry: what broke, why it broke, the fix, and the reusable rule.
> Newest at the bottom within each milestone.

## Week 1 — Execution core (async tools + approval sandbox)

1. **Compile-data races with concurrent edits**
   - What: editing `src/tools/mod.rs` while `cargo test` was mid-compile could feed rustc a half-written file.
   - Fix: hard gate — never touch `.rs` files between `cargo check/test` start and its finish; doc edits (`*.md`, new files outside the mod tree) are the only safe mid-build work.
   - Rule: one write window per build window; batch edits, then build once.

2. **Sibling-directory path confusion (`foo2` passing a `foo` root)**
   - What: an early version of `starts_with_path` used `Path::starts_with`/string prefix semantics; `C:\...\Temp\opencode\igris-smoke-sandboxx` shares a string prefix with `igris-smoke-sandbox-`.
   - Fix: component-wise comparison (`Path::components()` zip, unicode-folded), never string prefixes; plus a regression test that builds the literal `foo`/`foo2` siblings.
   - Rule: prefix checks on paths are comparisons of component sequences, not strings.

3. **Escaping borrows into `tokio::task::spawn_blocking`**
   - What: `E0521`/`E0382` — `&self.policy` and `&args`-backed `&str` moved into `'static` blocking closures.
   - Fix: `Clone` the sandbox policy and own (`String`) every captured string before the closure; compute results strings *after* the join (`app_for_log`).
   - Rule: spawn_blocking closures must be `'static` — clone-in, print-from-the-returned-value.

4. **Lock across `.await` in the registry**
   - What: original sync registry re-read the map with a std `RwLock` guard around execution; converting to async made "lock held during `tool.execute`" a writer-starvation/`register` deadlock risk (and E0382 if the guard escaped).
   - Fix: map is now `HashMap<String, Arc<dyn Tool>>`; `tool_by_name` clones the `Arc` and drops the guard before any await; same for the pending map (insert/remove-only, never across await).
   - Rule: hold locks only long enough to clone the handle; the handle outlives the lock.

5. **Cargo/PowerShell noise as false failures**
   - What: PS 5.1 turned rustc's stderr (progress + benign notes) into `$LASTEXITCODE = 1` via `2>&1` piping, and `&&` is not a statement separator — every green run looked red.
   - Fix: filter by content (`Select-String` on `error|Finished`), run commands individually; treat the last "Finished …" line as ground truth, not exit code.
   - Rule: on this machine, verification = read the output text, not the exit code.

6. **10-minute test-compile timeouts**
   - What: full `cargo test` (sherpa-onnx, tonic, prost codegen...) blows past 10 min from cold cache; a background run is mandatory.
   - Fix: run long builds with `background=true` and a 25-min ceiling; warm-cache incremental builds are minutes.
   - Rule: long builds go background and get resumed on notification, never polled.

7. **Silent revocation of all grants**
   - What: a corrupt `permissions.json` was swallowed by `.ok().and_then().unwrap_or_default()`; the next `grant()` overwrote the file — everything silently revoked.
   - Fix: explicit parse match with an `eprintln!` warning naming the file; behavior stays fail-default (no grants) but is now visible.
   - Rule: silent fallback paths must log; especially when the fallback erases user-authored state.

8. **Over-broad default sandbox (reviewer catch)**
   - What: first default was user-home + cwd while `read_file` is auto-allowed (`ReadOnly`) — silent reads of the whole user profile (`%USERPROFILE%\.ssh`, browser credential stores) out of the box.
   - Fix: default roots = cwd only; home is opt-in via `IGRIS_FILE_ROOTS`.
   - Rule: auto-allowed tools must default to the narrowest reading scope; widen only by explicit operator choice.

9. **Plan-doc line drift**
   - What: `plan.md` line references (§1 table) pointed at pre-baseline positions; every edit shifted them.
   - Fix: subagent re-derived the real current map file:line by file:line (explore agent); symlinked finders (grep) beat stored line numbers.
   - Rule: stored line numbers in docs are smell; locate by symbol when acting.

10. **Test-only cross-platform asserts**
    - What: `assert!(out.contains("os=windows"))` is Windows-only trivia posing as a test.
    - Fix: `format!("os={}", std::env::consts::OS)`.
    - Rule: assert on constants via the constant source, not the literal.

11. **Consent must be state, not a read (approve TOCTOU)**
    - What: `approve()` peeked the pending entry, then removed it; a racing second `approve` (or `deny`) between peek and take still executed the tool — double-run on one consent, and a user-rejected action running mid-flight.
    - Fix: removal *is* the authorization: `take_pending` first, act on `Some`, return NotFound otherwise; expired-deny path re-removes.
    - Rule: consent is a piece of consumable state; acquire it by atomic removal, never by read-then-consume.

12. **Blocklists need character classes (cmd.exe parser)**
    - What: `tasklist\nshutdown /s` passed the punctuation blocklist (control chars missing), and `%VAR%`-style expansion re-parses *after* the check (`^`/`!` extend that lever).
    - Fix: reject everything `< 0x20`, `0x7F`, plus `% ^ !` in `ShellCommandTool`.
    - Rule: network/LLM-supplied strings to a shell need class-based bans (control chars), not just visible punctuation.

13. **One name, one normalizer (grant keys drift)**
    - What: REPL `grant search_web` wrote a grant no decision ever looked up (registry normalizes internally at decision time; REPL bypassed it).
    - Fix: `grant`/`revoke` go through the registry (`ToolRegistry::{grant,revoke}`), one shared `normalize_tool_name`.
    - Rule: every authorization-write surface shares the identifier-normalizer with the authorization-check surface.

14. **"Allowlisted" must be a list, not a charset**
    - What: `OpenAppTool`'s description claimed a name allowlist while the code checked characters only — `powershell`, `cmd`, `mshta` all pass.
    - Fix: explicit app-name allowlist; interpreters/shells excluded.
    - Rule: a regex over a broad charset does not make a danger category safe; enumeration beats validation for named asset classes.

13. **One name, one normalizer (grant keys drift)**
    - What: REPL `grant search_web` wrote a grant no decision ever looked up (registry normalizes internally at decision time; REPL bypassed it).
    - Fix: `grant`/`revoke` go through the registry (`ToolRegistry::{grant,revoke}`), one shared `normalize_tool_name`.
    - Rule: every authorization-write surface shares the identifier-normalizer with the authorization-check surface.

14. **"Allowlisted" must be a list, not a charset**
    - What: `OpenAppTool`'s description claimed a name allowlist while the code checked characters only — `powershell`, `cmd`, `mshta` all pass.
    - Fix: explicit app-name allowlist; interpreters/shells excluded.
    - Rule: a regex over a broad charset does not make a danger category safe; enumeration beats validation for named asset classes.

## Fix pass 1 — reviewer-gated hardening (2026-10-31)

15. **Approval-arg validation runs at execution, not at request time (and that's the correct split)**
    - What: first draft of the shell-blocklist regression test expected `InvalidArgs` for `{"command":"echo hi\nwhoami"}` via the registry — but pre-approval the registry only schema-validates and mints a pending; the tool's own blocklist runs at execution (grant/approve) time. The naive test would have failed.
    - Fix: test grants `shell_run` first so the blocklist path actually executes; plus asserts rejected args never mint pendings.
    - Rule: know *which layer* an invariant lives in — registry (schema/args shape), policy (capability), or tool runtime validation — before writing the regression test for it.

16. **Existence oracles hide in differentiated error messages**
    - What: `read_file` returned three distinguishable denials ("traversal denied", "path not accessible: <os error>", "path outside allowed roots"), letting the auto-allowed tool probe whether arbitrary paths exist.
    - Fix: every denial now emits the identical `path denied: outside the sandbox or not accessible`.
    - Rule: when a read-only surface is auto-allowed, make *all* of its rejections indistinguishable, not just the common ones.

17. **Capability denies outrank persisted grants**
    - What: `decision()` checked `granted → Allow` before the Admin deny, so a grant (present or future GUI "always allow") could un-deny an Admin tool.
    - Fix: deny-by-capability returns before the grant check (one-line flip, latent risk retired).
    - Rule: policy ordering encodes privilege — the strongest constraint is evaluated first.

18. **Write gates come free with guard rails — keep them in test scope**
    - What: two of my clippy fixes were brand-new-code lint (`fail % 100` → `is_multiple_of`, `sort_by` → `sort_by_key`); the remaining 8 clippy warnings live in untouched baseline files (`core/vad.rs`, `core/wake_word.rs`, `core/tts.rs`, `nlu/sbert.rs`, `setup_manager/mod.rs`, `core/about.rs`).
    - Fix: touched files lint-clean; baseline warnings recorded as a backlog item, not "fixed" blind.
    - Rule: lint gates judge the diff; whole-repo cleanliness is a scheduled pass, not a fix-pass side quest — never bulk-rewrite unreviewed baseline files.

## Fix pass 2 — round-2 reviewer + cache incident (2026-10-31)

20. **A fix can ship its own type bug (`as_str()` returns `Option`)**
    - What: the property-level union fix matched literal `&str` arms directly on `t.as_str()` — six `E0308`s, because `Value::as_str()` yields `Option<&str>`; the arms needed `Some("string")` patterns.
    - Fix: wrap arm patterns in `Some(...)`; `_ => false` stays the fail-closed catch-all.
    - Rule: never hand-write a match on `as_str()`/`as_object()` without the `Option` layer; unwrap deliberately or match both variants.

21. **Previously-green crates failing with format errors = suspect the cache, not the code**
    - What: `cargo test` failed with "crate `sherpa_onnx_sys` required to be available in rlib format" — the `deps\` dir held only `.d` fragments, no `.rlib`, for a crate my diff never touched (stale/interrupted artifact between the first green run and the rebuild).
    - Fix: targeted `cargo clean -p sherpa-onnx-sys -p sherpa-onnx` (1.1 GB of hollowed artifacts) and a full rebuild; green again at check/clippy.
    - Rule: an error naming a crate you didn't edit → `cargo clean -p <that-package>` before suspecting source; verify with `Get-ChildItem target\debug\deps -Filter "<crate>*"` (rlib present or not).

22. **Round-2 review gate — SHIP with five folding items (all applied)**
    - Property-level union types now fail closed (`Some()` patterns, same as top level).
    - One poison policy per map: `take_pending` recovers via `PoisonError::into_inner` like `list_pending`; `grant`/`revoke`/Ask-insert log poison instead of silently skipping.
    - Grants temp file is process-unique (`permissions.json.tmp` → `permissions.json<PID>.tmp`) so two writers sharing a base dir cannot corrupt each other's rename.
    - `rust-version = "1.87"` pinned (`u64::is_multiple_of` MSRV; toolchain is 1.95) — honest floor, not a silent requirement.
    - REPL `grant`/`revoke` echo the *normalized* key that actually persists in `permissions.json`.
    - Rule: second-round review on a fix pass is where new defects hide; the reviewer verified all 10 round-1 items line-by-line and found the regression classes the parent eye closed after the first edit.

23. **Diagnostic order is API (assert it)**
    - What: hoisting `required` above unknown-key detection changed which error an LLM's bad call surfaces (`missing required argument 'app'` instead of the telling `unknown argument 'appx'`) and broke a smoke test — the reorder was my own over-application of a review note that only demanded required-presence in the no-`properties` branch.
    - Fix: validator order restored to unknown → type → required, with required still enforced unconditionally; the smoke tests now pin that exact order.
    - Rule: error precedence is part of the contract — reorder only with a reason, and let a test own the choice.

24. **Re-read every edited hunk before the next action (fires ~1 in 3 saves)**
    - What: this pass alone produced (a) a stray moved-value expression in a match arm, (b) `&&Map` that wouldn't iterate, (c) an `Option`-unaware match — each caught only by the next compile, each a full build cycle (the first rustc cycle can be minutes after an artifact clean).
    - Fix: no mechanical fix — the habit: after every `edit`, read back the region once before moving on.
    - Rule: the weakest reviewer is the one that writes; the cheapest build is the one that compiles first.

## Standing definitions (kept current)

- **Module pet-track**: `graphyfy/GRAPH.md` is the architecture map; regenerate after structural changes (new top-level module).
- **Verification gates** (every milestone, in order): `rustfmt` (touched files) → `cargo check --bin igris_ai` → `cargo test` → `cargo clippy` → reviewer agents on the diff.
