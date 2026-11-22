# igris-ai

Rust test harness for checking **Moonshine Small (streaming)** and **Moonshine Medium (streaming)** speech-to-text speed and accuracy on your own machine — and the foundation of **IGRIS**, a voice-first Windows desktop agent (roadmap in `todo.md`).

## Workspace layout

```
igris-ai/
├── crates/
│   ├── igris-core/   # agent core lib: STT, LLM, TTS, toolparse, tools, assist, live, hotkey
│   └── igris-cli/    # reference terminal front-end (binary keeps the name igris-ai)
├── assets/           # gitignored runtime assets (provisioned once, reused forever)
│   ├── llama-cpp/    # vulkan/ + cpu/ llama-server builds, models/ {LFM3-Omni-2B,Qwen3.5-4B} GGUFs
│   ├── piper/        # piper.exe + espeak-ng-data
│   ├── tts/          # piper voice files (downloaded on first use)
│   └── stt/          # small-streaming/, medium-streaming/ Moonshine models
└── zeroclaw/         # vendored reference code (read-only, not part of the build)
```

Asset paths resolve through `igris-core::paths::asset` — CWD-independent
(`MOON_ASSETS` env override → walk up from the running exe → walk up from
the CWD → bare `assets/` fallback), so the CLI and any future GUI crate
find the binaries no matter where they are launched from.

## Build

```bash
cargo build --release
```

## Choose the local LLM

`MOON_LLM_MODEL` selects which GGUF in `assets/llama-cpp/models/` the Rust
code serves through the bundled llama.cpp (GPU first, CPU fallback). The
prompt renderer, tool protocol and server flags follow the chosen model, so
no code change is needed to switch:

| `MOON_LLM_MODEL` | GGUF | Notes |
|---|---|---|
| `lfm3` (default) | `LFM3-Omni-2B-Q4_K_M.gguf` | LFM3 ChatML dialect; `--swa-full` for its sliding-window layers |
| `qwen` | `qwen3-5-4B-Q4_K_M.gguf` | Qwen3.5-4B (~2.5 GB); its own ChatML/thinking prefix, no SWA flag |

Only these two models ship. The other GGUFs tested during provisioning were
removed: they were either broken (gibberish, reasoning loops, plain-text
tool calls) or unreliable as agents — see `benchmark.md`.

```powershell
$env:MOON_LLM_MODEL = 'qwen'
cargo run --release -- chat "who are you"
```

An unknown value warns and falls back to `lfm3`. Note that `llama-server`
stays alive after the app exits, so after switching models stop the old
server first (`Stop-Process -Name llama-server`) — the app refuses to reuse
a server serving a different GGUF. The logon autostart script honours the
same variable.

## Get an audio file

Record or generate a WAV (16 kHz mono is fine, the code re-samples). A quick synthetic test on Windows:

```powershell
powershell -ExecutionPolicy Bypass -File .\gen_tts.ps1
# produces speech.wav + reference.txt (ground truth)
```

## Run

```bash
# (first run checks/downloads both models into assets/stt/; ~570 MB total, needs internet)
cargo run --release -- speech16.wav reference.txt
```

Output for each model:
- load time
- inference time (ms) and real-time factor (RTF = audio_seconds / infer_seconds)
- transcript
- WER against `reference.txt` (if provided)

## Live microphone transcription (Rust — primary tool)

```bash
cargo run --release -- live
```

(or run the built binary directly from the repo root: `target\release\igris-ai.exe live`)

Add `--model assets/stt/medium-streaming` to run the medium streaming model instead
of small (the architecture is inferred from the directory name); e.g.

```bash
cargo run --release -- live --model assets/stt/medium-streaming
```

Streams your **default microphone** straight into `assets/stt/small-streaming` and prints
transcript lines the moment each sentence is finished. While you are speaking it shows
a live status row with the input meter (bar + RMS), speaking state, raw un-analyzed
backlog and the model's measured speed (Nx real-time). When a sentence completes it
prints the measured latency.

- Press **ENTER** to stop; a final summary (total time, stats) is printed and the window
  stays usable until you close it.
- Capture runs at the mic's **native rate** (e.g. 48 kHz); resampling to 16 kHz happens
  inside the library.
- Occasional `buffer underrun or overrun occurred` warnings from the audio layer are
  harmless; capture keeps running.

Backlog on the status row = audio pushed into the stream minus audio the model has
already analyzed; speed = measured ms of CPU per second of audio (Nx real-time).

## Terminal chat with the local LLM

```bash
cargo run --release -- chat                     # interactive loop, replies stream live
cargo run --release -- chat "who are you"       # one-shot
```

Backed by the GGUF selected with `MOON_LLM_MODEL` (default: LFM3-Omni-2B) through the bundled llama.cpp — see [Choose the local LLM](#choose-the-local-llm).
On first use the app starts `llama-server` detached on `127.0.0.1:8081`; the
process intentionally keeps running after the app exits so later runs skip
the model load. Stop it manually with `taskkill /IM llama-server.exe /F`.

Two llama.cpp builds ship with the repo, and the app picks between them
automatically:

- **GPU first** — the Vulkan build in `assets/llama-cpp/vulkan/`. Used whenever
  the machine can run it.
- **CPU fallback** — the CPU build in `assets/llama-cpp/cpu/`. Takes over
  automatically when the GPU build cannot run: no Vulkan driver, too little
  VRAM, early crash, or a startup timeout. The console prints which backend
  came up, e.g. `llm : llama.cpp gpu backend ready (NVIDIA GeForce RTX 4050 Laptop GPU)`.

Pass `--cpu` (or set `MOON_LLM_CPU=1`) to skip the GPU attempt entirely —
useful on office laptops where you would rather keep the VRAM free.

The server runs with `--no-webui` (API only) plus `--cache-reuse 1`; LFM3 also
gets `--swa-full` because its sliding-window-attention layers reject the
default SWA cache on KV-shift reuse (Qwen3.5 has no SWA layers). Prompts are
rendered as the chosen model's ChatML dialect in `crates/igris-core/src/llm.rs`
and sent to the raw `/v1/completions` endpoint; the chat endpoint would
re-tokenize per request and defeat the reuse.

The agent loop remembers each `(tool name, canonical arguments)` for one turn:
a repeated call returns the original result with a "do not repeat" note instead
of re-executing it. This prevents the model from burning rounds re-running the
same search — measured 7.63 s → 4.31 s on a web task and 11.52 s → 8.53 s on a
research task (both on a warm GPU server, 2026-11-22). Gated tools' two-step
confirmation still works because the guard is scoped to a single turn.

Short **workflow skills** (browser, media, files, memory, research) are appended
to the system prompt when the first user message matches one, giving the model
explicit tool-selection rules for that task. `assist` uses a generic voice
skill. Skill choice is fixed at session seed so the KV-cache prefix stays
stable; `MOON_NO_TOOLS=1` skips both the protocol block and the skill. See
`crates/igris-core/src/skills.rs` and `benchmark.md` (the skills are guidance,
not a measured speed win).

Replies stream live after a minimal `ai:-` label — no timing or status lines
around them. When a tool runs, the console prints `executing: <tool>` (the
voice loop speaks "executing" once per turn). Action requests get one short
confirmation afterwards — e.g. `play rehbara song on youtube` -> "Playing the
song." — while informational questions still get a full answer.

### Image understanding (optional, via Cactus Mirage)

```bash
cargo run --release -- chat --image photo.png "What does this image show?"
```

Mirage is a **standalone** vision model in Cactus's `.cact` format — it is not
an llama.cpp projector, so it does not attach to LFM3-Omni or Qwen directly.
Instead the CLI describes the image with Mirage (via the `cactus-compute`
Python package as a local sidecar), then prepends that text to the normal
chat prompt, and either model answers through the existing path. Text-only
chats are unaffected. Setup:

```bash
python -m pip install -U cactus-compute
# weights already provisioned in assets/mirage/ (mirage.cact ~21 MB, safetensors ~206 MB)
```

The first image in a process costs ~10 s (engine load + inference); later
ones only pay inference. Set `MOON_PYTHON` if your interpreter is not `python`.
Skill selection is based on your question only, never on the image text.
See `scripts/mirage_describe.py` and `crates/igris-core/src/vision.rs`.

## Voice assistant: live STT -> LLM

```bash
cargo run --release -- assist                    # live microphone (ENTER to finish)
cargo run --release -- assist speech16.wav       # same pipeline with a WAV in real time
cargo run --release -- assist --model assets/stt/medium-streaming
cargo run --release -- assist --cpu              # force the CPU llama.cpp build
```

While you are still speaking, the partial Moonshine transcript is pushed into the
LLM every 250 ms as a tiny prefill request (`max_tokens = 1`). Because each ping
is a strict token-prefix of the final request, llama-server's KV cache already
contains almost the whole sentence when you stop — the final request only
processes the tokens spoken since the last ping, and the reply starts streaming
token-by-token typically within ~100-250 ms.

The console shows the same live status row as `live` (speech meter, in-flight
sentence tail, audio backlog) plus the prompt-cache state (`cold`/`warm`).
Replies follow a spoken-style persona (short, no markdown) and print
minimalistically as `you :- <heard text>` / `ai:- <reply>`. A session summary
(audio processed, exchanges) is printed on exit.

### Push-to-talk mode (`assist --hotkey`)

```bash
cargo run --release -- assist --hotkey
```

With `--hotkey` nothing is active while idle — not even the capture device: the
microphone is **opened only while the keys are held** and dropped again on
release, so no idle room noise is ever captured (nothing for the STT to
mishear). The same global **CTRL+SPACE** hook as the `hotkey` command drives it
from any application. During a hold, audio is captured and the partial text is
prefilled into the LLM (keeping its prompt cache warm), but **nothing is
answered yet**. Release the keys and only the audio captured during the hold is
finalized and answered, one turn per heard sentence. Pressing the keys again
while a reply is still streaming cuts it short at once (barge-in) and starts
the next hold. Pressed keys are only matched on **physical** key events
(injected ones are ignored). Quit mid-hold with Ctrl+C and the remaining
transcript is printed instead of answered.

Testing without a microphone: set `MOON_FAKE_HOTKEY=1` to drive the combo from
stdin (`p` + ENTER = press, `r` + ENTER = release, EOF or Ctrl+C to quit) and
`MOON_PTT_WAV=path.wav` to feed a WAV file in real time as a synthetic
microphone through the same gate:

```bash
$env:MOON_FAKE_HOTKEY = "1"; $env:MOON_PTT_WAV = "speech16.wav"
cargo run --release -- assist --hotkey
```

## Text-to-speech (piper)

```bash
cargo run --release -- speak "hello there"                # default voice (en_US-libritts_r-medium, speaker #051)
cargo run --release -- speak "hi" --voice en_US-amy-low   # smaller-sounding, 16 kHz voice
```

Replies in `assist` are also spoken: the streaming reply is cut into sentences
(`take_sentence`) and each one is synthesized while the LLM is still generating,
so speech starts before the text finishes. Flags and hooks:

- `--voice ID` (and `MOON_VOICE=en_US-amy-low`) picks the piper voice; the
  default is `en_US-libritts_r-medium#051` — the same voice the igrisv3 app
  uses. LibriTTS-R medium is a 904-speaker model, so a `#NNN` suffix on the
  voice id selects the speaker inside it (forwarded to piper as
  `--speaker`); amy voices are single-speaker and need no suffix.
  `amy-medium` is 22 kHz; `amy-low` is 16 kHz, ~20 MB less RAM
  and ~150 ms faster per sentence, slightly worse quality. Voices download on
  first use from the pinned `v1.0.0` tag of `rhasspy/piper-voices` — **do not**
  point this at `main`: the current `main` voice configs carry multi-codepoint
  phoneme keys this piper build rejects.
- `--no-tts` (or `MOON_NO_TTS=1`) runs `assist` text-only.
- While the voice is speaking, the mic is hard-muted: the capture gate is
  disabled, so nothing is collected at all, and the status row shows
  `MIKE OFF`. It reopens ~600 ms after the voice finishes so the STT never
  hears the assistant's own voice; barge-in in push-to-talk mode kills the
  synthesizer instantly and the next utterance respawns it.

Runtime: `assets/piper/piper.exe` (piper 2023.11.14-2 Windows x64) with the
matching `espeak-ng-data` folder next to it. The voice files live in
`assets/tts/`. One long-lived piper process serves all utterances — one text
line on stdin, one WAV file written into `.tts-utt/` via `--output_dir` — so
the model loads once (warm first audio within ~100-200 ms; a fresh spawn
costs ~0.5-3.5 s depending on disk cache, which only happens after a
barge-in). A reader thread watches `.tts-utt/`, consumes each utterance WAV
in name order (timestamped names preserve utterance order), converts it to
the device rate and deletes the file. Finished files rotate through three
fixed slots (`.tts-utt/tts_1.wav` ... `tts_3.wav`) — while one is being
consumed the other slots are cleared — so the folder never accumulates
files and disk use stays bounded even with many sentences queued.

Note: this piper build's stdout pipe (`-f -`) output carries corrupted PCM —
every voice plays as noise regardless of how the bytes are interpreted —
while file output (`-f <file>`, `--output_dir`) is clean little-endian
audio. That is why the runtime consumes `--output_dir` files instead of
streaming stdout, mirroring the file-based approach the igrisv3 app uses.

## Models downloaded

- `small-streaming`  -> `assets/stt/small-streaming` (`~142 MB`)
- `medium-streaming` -> `assets/stt/medium-streaming` (`~425 MB`)

Both are fetched from `download.moonshine.ai` automatically via
`moonshine-rs::get_stt_dependencies_with_options` (pinned manifest
`quantized_26_11_02`). On every run the on-disk files are size-checked against
the current manifest and stale manifest-listed files are replaced, so the file
set always matches the SDK build without touching unrelated files.

## Voice assistant (STT -> LLM, live streaming)

```bash
cargo run --release -- assist
```

Speaks into the mic; Moonshine streams partial text while you talk, and each
growth of that partial text is pushed into the LLM as a `max_tokens=1`
"prefill ping" so its prompt cache warms up live. When your sentence
completes, the reply streams back token-by-token (first token typically
< 1 s). Conversation history is kept across exchanges. ENTER to finish.

- The local LLM runs through **llama.cpp v1.0.0-dev (b12359)**, Vulkan build in
  `assets/llama-cpp/vulkan/` (GPU: any Vulkan-capable adapter; `-ngl 99 -fa on
  -c 8000 --cache-reuse 1`, plus `--swa-full` for LFM3 — the flags matter, see
  `crates/igris-core/src/llm.rs`). The CPU build stays in
  `assets/llama-cpp/cpu/` as fallback. b12359 matters for Qwen: its Vulkan
  prefill FlashAttention path took Qwen3.5-4B tool rounds from ~7–16 s
  (b12110) down to ~1.5–3 s on an RTX 4050. To re-provision the runtime on a
  fresh machine, run `powershell -ExecutionPolicy Bypass -File
  .\scripts\update_llama_binaries.ps1` (backs up the old binaries first).
- Model: selected with `MOON_LLM_MODEL` (default `assets/llama-cpp/models/LFM3-Omni-2B-Q4_K_M.gguf`, ~1.0 GB; `qwen` Qwen3.5-4B also bundled).
  The `llama-server` process keeps running after the app exits so the next
  run skips the model load; `Stop-Process -Name llama-server` frees it.
- Test without a mic (streams a WAV in real time through the same
  pipeline): `cargo run --release -- assist speech16.wav`

## Chat with the local LLM (text only)

```bash
cargo run --release -- chat "why is llama.cpp useful on small laptops?"
cargo run --release -- chat        # interactive loop
```

## Push-to-talk hotkey (Ctrl+Space)
```bash
cargo run --release -- hotkey
```

Hold **Ctrl+Space** in any application to start recording; release to stop and
transcribe the held segment. The window does not need focus — the keyboard hook
is global, and while Ctrl is held Space is swallowed so no stray character is
typed into the focused app. The program stays running for the next hold; Ctrl+C
quits.

- Moonshine expects 16 kHz mono f32 PCM; `audio::load_audio_for_transcription` decodes/converts most formats (WAV/MP3/AAC/FLAC/OGG/M4A).
- `Transcriber::transcribe` is synchronous/CPU-bound; in async apps offload with `tokio::task::spawn_blocking`.
- Both models ship on-device; no data leaves the machine once downloaded.

## Agent web tools with provenance (P3)

The chat/assist agent reads the public web through three tools in
`crates/igris-core/src/tools/web.rs`: `web_search` (DuckDuckGo "lite"
HTML backend, no API key), `web_fetch` (size-capped page reader, HTML
converted to plain text) and `download_file` (streamed save into the
user's Downloads folder only). Every HTTP hop runs through a hardened
ureq agent whose resolver vetoes local/private addresses, so no request
can target the machine's own network.

As of the P3 "freshness & source confidence" step (plan item 10), every
web result carries its provenance so the model can date its claims
instead of presenting fetched text as timeless truth:

- `web_fetch` results begin with `Source:` (the post-redirect final
  URL), `Retrieved:` (local wall-clock, minute precision) and
  `Last-Modified:` (when the server volunteers it) lines before the
  page text.
- `web_search` states the retrieval time in its header — snippets are
  often the only thing read, and they age (versions, prices, news).
- The tool descriptions instruct the model to cite the source URL and
  retrieval date in its answers and to prefer official project domains
  (docs, release notes, GitHub) for version/release questions.

Verified live (`web_fetch https://example.com`):

```
Source: https://example.com/
Retrieved: 2026-11-16 22:15
Last-Modified: Mon, 16 Nov 2026 00:30:20 GMT
```

Result shapes are assembled by pure formatters with hermetic unit
tests; the benchmark trail for this and earlier remediation phases
lives in `benchmark.md`.

## Benchmarks

`benchmark.md` holds the full evidence trail (goal.md category matrices,
browser tasks, runtime upgrade and cleanup records). Latest headline numbers
from the nine category probes on a warm RTX 4050 Vulkan server:

| Category | LFM3-Omni-2B | Qwen3.5-4B |
|---|---:|---:|
| Open an app | 0.97 s | 1.39 s |
| Media key | 1.09 s | 1.25 s |
| Find a file | 10.93 s | 1.72 s |
| Project structure | 2.02 s | 3.47 s |
| Web search | 4.31 s | 7.40 s |
| Open a URL | 4.61 s | 1.93 s |
| System stats | 1.95 s | 1.74 s |
| Memory save/recall | 0.87 s | 1.88 s |
| Research + summarize | 5.12 s | 8.53 s |

Both models pass 8/9 categories. LFM3 is faster on simple tool calls; Qwen is
much faster on file lookup (it picks `list_dir` instead of `file_search` +
`read_file`). The slow rows are long-output bound, not thinking bound.

`scripts/update_llama_binaries.ps1` re-provisions the bundled llama.cpp
runtime (b12359) if you need to restore it on a fresh machine.
