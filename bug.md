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

### BUG-002 — non-canonical IPv4 literals bypass the private-host block — MED — OPEN
`is_private_host` parses the host with `Ipv4Addr::parse`, which only
accepts full dotted quads. Windows' resolver (and most OS resolvers)
also accept decimal (`http://2130706433/` = 127.0.0.1), partial-dotted
(`http://127.1/`), and hex forms (`http://0x7f.0.0.1/`) — all resolve to
loopback while our check sees "not an IP, not private". Also flagged:
`bare.starts_with("::1")` is a catch-all for bracket-less IPv6 but does
not cover other loopback-ish literals.
Suggested fix: harden `validate_web_url`'s host extraction — if the
host (sans port) is not a valid full dotted quad, not bracketed IPv6,
and contains no ASCII letter, REJECT it (no real domain is letterless);
optionally expand integer forms and re-check instead of rejecting.
Tests: the forms above + `0177.0.0.1` must be rejected.
UPDATE 2026-11-15: partially mitigated at the web-tools socket layer by
the BUG-001 resolver veto (`ab24b42`) — getaddrinfo resolves the
non-canonical literals to 127.0.0.1 and the resolver's `is_private_ip`
filter blocks the resolved private address (fail-closed). The literal
hardening of `is_private_host`/`validate_web_url` stays open as
defense-in-depth.

### BUG-003 — fake inline tool_result JSON leaks into display/TTS — HIGH (UX) — OPEN
When LFM3 hallucinates a fake tool result AFTER a clean final answer
(bare `{"name": …, "result": …, "status": "ok"}` JSON with no envelope),
the parser correctly ignores it (no dispatchable shape survives), but
`StreamFilter` releases it to display/TTS — a stray JSON line leaks into
the chat/voice output. Observed live in v1.1 e2e.
Fix direction: StreamFilter hold-back should treat a trailing bare
`{"name":…,"result":…,"status":…}` line the way DSML spans are held —
hold it until it is provably not tool machinery, then drop it.
(Also tracked in todo.md under v1.1 batch 1 follow-up.)

### BUG-004 — memory recall returns hash-collision "similar" riders — MED (quality) — OPEN
v1.2 finding: hash-token collisions can attach a low-score "similar"
record to recall results even when it is not truly related; keyword
hits always rank first so the answer is usually right, but the rider is
noise the model may read aloud. Tests assert collision-tolerance, so it
is by-design tolerated, not desired.
Fix direction: raise the similarity admission floor, or drop riders
whose score is below the top hit by a margin; re-check the collision
test expectations when tuning.

### BUG-005 — file_search walk is DFS-LIFO (same starvation shape we fixed in content_search) — LOW — OPEN
v1 `file_search` still walks depth-first with a LIFO stack, so the
alphabetically-LAST top-level subtree is scanned first; with the 30k
entry cap it is far less likely to starve (name matching, no file
reads, default roots are user folders), but the pathology is the same
one that made content_search miss `set_master_volume` at repo scope
(fixed there with BFS + nested-repo skip in 0559b8e).
Fix direction: port the BFS + `is_nested_repo` skip to file_search's
`search_walk`; keep the 30k entry budget.

### BUG-006 — llama-server reuse trusts /health only — LOW — SUSPECTED
The agent loop reuses an already-running llama-server on :8018/:8081
after a bare `/health` 200. If a server with a DIFFERENT model/GGUF is
already bound to the port, we would reuse it blindly and get degraded
or wrong behavior with no error. Not yet observed live.
Verify: start a wrong-model server on :8081, run the CLI, see whether
anything detects the mismatch. Fix direction: compare a model
identifier (e.g. /props or /v1/models) against the expected GGUF before
reusing; log loudly on mismatch.

### BUG-007 — screenshots accumulate without bound in OneDrive Pictures — LOW (hygiene) — OPEN
Every screenshot writes `C:\Users\sohan\OneDrive\Pictures\igris_screenshot_*.png`
and nothing ever cleans up (OneDrive syncs them, too).
Fix direction: rotate (keep last N) or a spoken "clear my screenshots"
path; note OneDrive sync cost in the decision.

### BUG-008 — run_command cargo new/init can create folders anywhere — LOW (design review) — OPEN
`cargo new` / `cargo init` are allowlisted and write a project folder
into `working_dir` (or the process cwd) — i.e. writes OUTSIDE the
Downloads-only rule that governs download_file. Accepted when the
allowlist was designed (dev tools need it), but goal.md's safety wording
should be reconciled so the docs say "downloads go to Downloads; dev
scaffolding writes where the user points it".
Decision needed: document as-is, or gate new/init behind the same
two-step confirmation v2.5 will bring.

### BUG-009 — lib test binary intermittently exits 101 with all suites green — LOW (flaky) — SUSPECTED
Twice during the toolparse modularization the workspace gate
(`cargo test --workspace | Out-Null; $LASTEXITCODE`) returned 101 with
cargo's stderr "error: test failed, to rerun pass
`-p igris-core --lib`"; every run where stdout was actually captured or
displayed showed all suites green (lib 319/0/1, doctests 1/0/0) —
including a full `| Out-File` capture immediately after. The failure
detail was suppressed by Out-Null both times, so the offending test is
not yet identified. Hypotheses: (a) a timing-sensitive test failing
under machine load (candidates: `tools::registry` hung-tool budget
test, `content_search` 3.5 s budget test, screenshot smoke) — note
llama-server holds :8018/:8081 during these runs; (b) the lib test
binary crashing AFTER `test result: ok` during native teardown (the
same binary initializes ORT/moonshine and the screenshot test touches
GDI), which cargo reports identically with no `failures:` block.
Next step when it reproduces: capture full stdout
(`| Out-File D:\Temp\opencode\gate_ws.txt -Encoding utf8`) and check
for a `failures:` block vs. a clean tail; run the lib test binary
directly for its raw exit code (an 0xC0000005-class code confirms a
crash, not a test failure).

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
  also covers rebinding. Remaining trade-offs: CGNAT 100.64/10 and
  TEST-NET ranges stay allowed; BUG-002's non-canonical IPv4 literal
  hardening on `is_private_host` remains open (mitigated at the socket
  layer by the same veto).
- `run_command` output is capped at 4000 chars — compiler dumps arrive
  truncated with an explicit marker.

## Fixed recently (ledger)

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
