# Problem Log — igris-ai

Dated log of problems hit during development, with root cause and fix, so
they can be recognised quickly if they ever come back. Newest entries on top.

---

## 2026-11-12 — Crash `STATUS_ACCESS_VIOLATION` after `cargo clean` (commit `2e82d3e`)

### Symptom

```
The requested API version [23] is not available, only API versions [1, 17] are supported in this build. Current ORT Version is: 1.17.1
error: process didn't exit successfully: `target\release\igris-ai.exe assist --hotkey` (exit code: 0xc0000005, STATUS_ACCESS_VIOLATION)
```

- Happened right after `cargo clean` + a fresh 3m12s rebuild.
- The warning printed twice (ORT env init + session create), then the process
  died during STT model load — the `models loaded` line never appeared.
- Every mode would crash the same way (`speak` too), because the dll is
  loaded at process start, before any code runs.

### Root cause

`moonshine-sys 0.2.9` ships a prebuilt moonshine voice library that needs
**onnxruntime API version 23** (runtime 1.23.x). It unpacks the matching
`onnxruntime.dll` into its build-script output dir but never puts it next to
the executable:

```
target\release\build\moonshine-sys-<hash>\out\moonshine-prebuilt\moonshine-voice-windows-x86_64\lib\onnxruntime.dll   (v1.23.20251021.3)
```

The executable's own directory is the **first** entry of the Windows dll
search order, and the old `target\release\` happened to contain a correct
copy — so everything worked. `cargo clean` deleted that copy, and the fresh
rebuild did not restore it. The loader then fell back to the **OS-built-in
copy in System32**:

```
C:\WINDOWS\System32\onnxruntime.dll   v1.17.260914-0959.1.os-germanium (Windows component — must not be removed)
```

System32 is searched *before* the PATH entries that `cargo run` adds, so the
bundled 1.23 dll never got a chance. The binding called
`OrtGetApiBase().GetApi(23)`; 1.17.1 only supports API versions `[1, 17]`,
returned null, and the process died with an access violation.

### Fix (`build.rs`)

- On every build, finds the newest bundled `onnxruntime.dll` under
  `target/<profile>/build/moonshine-sys-*/out/**` and copies it to
  `target/<profile>/` (next to the exe). Exe dir wins the dll search
  order deterministically, so System32's copy is never reached.
- Emits `cargo:rerun-if-changed=<profile>/onnxruntime.dll`, so even if the
  dll is deleted between builds, the next `cargo build` re-runs the script
  and restores it. The copy updates the mtime once, then builds stay fresh
  (no loop).
- Copies only when the exe-side copy is missing or older than the bundled
  one (covers future moonshine-sys version bumps).
- If the copy fails but a previous copy exists (dll locked by a running app),
  it is tolerated; otherwise the build fails loudly. If the bundled dll is
  not found at all, a `cargo:warning` is printed.

### Verification

- Delete `target\release\onnxruntime.dll` → `cargo build` → restored (7.7 s).
- Second build 0.3 s — fresh, no rebuild loop.
- `assist --hotkey` ran 30 s: no ORT warnings, `models loaded (3 s)`,
  hotkey hook installed, status row ticking.

### Follow-up notes

- If the same crash ever reappears, check first whether
  `target\release\onnxruntime.dll` exists and its version starts with
  `1.23`. Quick manual fix: copy it from the moonshine-sys build out dir
  shown above (or just rebuild — `build.rs` restores it).
- `cargo clean` is now safe; do not delete anything from `System32`.
- The version in the warning is the giveaway: "Current ORT Version is:
  1.17.1" means the OS copy from System32 got loaded instead of the bundled
  one.
