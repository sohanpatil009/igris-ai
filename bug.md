# BUG.md — open bugs & suspicions

Working list of known or suspected bugs in igris-ai, so they are visible
and get tackled deliberately instead of being re-discovered. Each entry
carries severity, status, evidence, and a suggested approach. Fixed items
move to the ledger at the bottom (one line + commit). Companion docs:
`goal.md` (north star), `todo.md` (roadmap), `tool_calling_plan.md`.

Statuses: OPEN · SUSPECTED (needs reproduction/verification) ·
FIXED (in ledger).

---

## Open

### BUG-008 — run_command cargo new/init can create folders anywhere — LOW (design review) — OPEN
`cargo new` / `cargo init` are allowlisted and write a project folder
into `working_dir` (or the process cwd) — i.e. writes OUTSIDE the
Downloads-only rule that governs download_file. Accepted when the
allowlist was designed (dev tools need it), but goal.md's safety wording
should be reconciled so the docs say "downloads go to Downloads; dev
scaffolding writes where the user points it".
Decision needed: document as-is, or gate new/init behind the same
two-step confirmation v2.5 will bring.

## Open

### BUG-010 — cargo-run chat may hang after SPAWNING llama-server under a piped stdout — LOW (flaky) — SUSPECTED
One occurrence 2026-11-15 (BUG-006 verification): with the live
llama-server killed, `cargo run -q -p igris-cli -- chat hi 2>&1 |
Out-File …` printed the full answer (gpu backend ready + reply) but did
not exit within a 5 min timeout; a follow-up run that only REUSED the
running server exited 0 quickly. Spawn machinery is untouched by
BUG-006, so this predates it; suspicion is the detached child holding
an inherited pipe/console handle alive when stdout is redirected.
Reproduction recipe: kill llama-server, run the piped chat command,
observe no exit; interactively (no pipe) it should exit normally.
Check next time it reproduces: does a plain console `cargo run -- chat`
exit cleanly after a spawn?

---

## Known limitations (documented trade-offs, not bugs)

- DDG snippets are zipped by index — a result without a snippet row can
  shift alignment (accepted; alignment shift beats missing snippets).
- `web_fetch` decodes bodies lossy-UTF-8; non-UTF-8 pages garble
  slightly instead of failing.
- `download_file` derives the filename from the URL; the server's
  Content-Disposition suggestion is ignored (feature gap, not a bug).
- `content_search` reports PARTIAL results when its 3.5 s / 1500-file
  budget is hit — by design, and the note is deliberately loud now.
- The web-tools private-host block includes DNS-level filtering now
  (BUG-001 `ab24b42`): the resolver refuses private literal hosts
  without DNS and drops private resolved addresses fail-closed, which
  also covers rebinding; the literal layer also refuses non-canonical
  IPv4 literals (BUG-002 `07b0e20`). Remaining trade-offs: CGNAT
  100.64/10 and TEST-NET ranges stay allowed, and hex-spelled
  loopback-ish v6 literals beyond `::1` (e.g. `[::ffff:7f00:1]`) are
  caught by the resolver layer only.
- `run_command` output is capped at 4000 chars — compiler dumps arrive
  truncated with an explicit marker.

## Fixed recently (ledger)

- 2026-11-15 (`6cbc9fe`): default screenshots accumulated without bound
  in the OneDrive-synced Pictures folder — after a default-path capture
  the oldest `igris_screenshot_*.png` beyond 20 are pruned (stamp order
  = age order, the just-saved file is protected, explicit user paths
  and unrelated files are never touched, prune failures are ignored);
  OneDrive note: deletions sync downward too, so the synced volume
  stays bounded (BUG-007).
- 2026-11-15 (`2cefc53`): llama-server reuse trusted a bare /health 200
  — ensure_server now identifies the running server via /props before
  reusing (GGUF file name from model_path/model_alias vs the bundled
  LFM3 name, case-insensitive) and refuses loudly on a different model
  or a foreign /health-only server (fail-closed when /props carries no
  identity). Verified live: a foreign /health-200 server on :8081 was
  refused with a clear error (pre-fix it would have been reused
  blindly), and the correct server still reuses (BUG-006).
- 2026-11-15 (`03c0139`): the flaky lib-test exit 101 was a
  MOON_ASSETS env race — the two paths tests touch the env var while
  cargo runs test threads in parallel, so the asset-join test could
  resolve root and joined to different roots (assertion failure whose
  FAILED line `Out-Null` discarded, matching the observed all-green
  exit 101); the tests now serialize on a static lock (BUG-009).
- 2026-11-15 (`50d4c88`): file_search still walked depth-first with a
  LIFO stack (the same starvation shape fixed in content_search,
  0559b8e) and never skipped nested clones — search_walk is now
  breadth-first (top-level folders get the first turn, dependency-like
  trees the last) and skips nested repos on descent (explicit roots
  inside a clone still search it); the 30k entry / 3.5 s budgets are
  unchanged (BUG-005).
- 2026-11-15 (`4b6faac`): hash-token collisions attached low-score
  "similar" riders to recall results (the "march 3rd"-vs-mom collision,
  cosine ~0.26, passed the 0.2 base floor) — recall now drops similar
  riders that score further than RECALL_RIDER_MARGIN (0.6) below the
  top keyword hit; the pure-similar path keeps the base floor
  (calibration showed the collision band overlaps the weak-related
  band, so the floor is relative, not absolute) (BUG-004).
- 2026-11-15 (`2a93c0d`): a hallucinated bare tool-result JSON after a
  clean final answer (`{"name": …, "result": …, "status": …}` with no
  envelope) leaked to display/TTS — StreamFilter now holds any bare
  JSON object from its opening spell and drops it at the closing brace
  when the fake-result signature matches (name + result/status);
  non-machinery objects release verbatim, protocol spans inside the
  held object mark it contaminated, and the trigger never preempts a
  preceding envelope opener (BUG-003).
- 2026-11-15 (`07b0e20`): non-canonical IPv4 literals (2130706433,
  127.1, 0x7f.0.0.1, 0177.0.0.1) bypassed the private-host block —
  is_private_host now refuses any host whose every dot-segment is
  numeric (digits/0x-hex), restructures the port strip so the named
  checks see port-free hosts (localhost:8018 and foo.local:8080 had
  slipped too), and covers [::ffff:127.0.0.1]-style mapped literals by
  delegating the IP policy to the shared is_private_ip (BUG-002).
- 2026-11-15 (`ab24b42`): web tools followed redirects into private
  hosts (SSRF) — the ureq agent now vetoes local/private targets at the
  resolver layer on EVERY request hop (initial URL + each redirect,
  before any socket opens; also covers DNS rebinding via a fail-closed
  resolved-address filter), with the redirect history as a second
  backstop; CGNAT/TEST-NET stay allowed (BUG-001).
- 2026-11-14 (`0559b8e`): validate_web_url kept the `:port` in the
  authority, so `127.0.0.1:8018` slipped past the private-host block —
  is_private_host now strips all-digit ports (IPv6 brackets handled)
  and catches bare `::1`.
- 2026-11-14 (`0559b8e`): decode_entities' numeric branch failed to
  strip the terminating `;` before parsing, so `&#65;`/`&#x42;` passed
  through undecoded.
- 2026-11-14 (`0559b8e`): content_search at repo-root scope missed
  `set_master_volume` — DFS pops the alphabetically-last top-level
  subtree first and the nested `zeroclaw` clone ate the whole
  1500-file budget (walk sim: tools.rs at position 2296/2304); fixed
  with breadth-first walk + skip nested `.git` clones on descent.
- 2026-11-13 (v1.1): fake inline tool_result BLOCKS (envelope-shaped)
  are parser-ignored — only the trailing bare-JSON variant leaks
  (BUG-003 above).
