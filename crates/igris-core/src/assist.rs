//! `igris-ai assist [WAV] [--model DIR] [--hotkey] [--no-tts] [--voice ID] [--cpu] [--no-tools]`
//!
//! Live voice assistant: Moonshine streaming STT + the local LLM through
//! llama.cpp. While the user is still speaking, every growth of the
//! partial transcript is pushed into the model as a tiny prefill request
//! (`max_tokens = 1`). llama-server caches the KV state for that prefix
//! (longest-common-prefix slot matching), so when the sentence finally
//! completes, the real request only processes the last few tokens and the
//! reply starts streaming token-by-token almost instantly.
//!
//! Input is the default microphone, or a WAV file streamed in real time
//! (`assist path.wav`) which exercises the exact same pipeline without a
//! mic — handy for testing.
//!
//! `--hotkey` turns the microphone input into push-to-talk (the same
//! global CTRL+SPACE hook as the `hotkey` command): the microphone device
//! itself is opened only while the keys are held and handed back on
//! release, so nothing — not even the capture device — is active while
//! idle. Nothing is answered before the release either: during a hold the
//! partial text only feeds the prefill pings (which keep the LLM cache
//! warm), and every line heard during the hold is answered once, after
//! the keys go up. An answer that is still streaming when the keys are
//! pressed again is cut short immediately (barge-in).
//! `MOON_FAKE_HOTKEY=1` drives the combo from stdin ("p"/"r" lines) and
//! `MOON_PTT_WAV=path.wav` feeds a WAV file in real time as a synthetic
//! microphone, so the gating can be tested without real speech input.
//!
//! The console shows the same live status row as `live` (speech meter +
//! in-flight sentence tail + audio backlog), plus the prompt-cache state
//! of the prefill pings.

use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use moonshine_rs::audio::decode_audio_file;
use moonshine_rs::{ModelArch, Transcriber};
use serde_json::{json, Value};

use crate::hotkey::{maybe_spawn_fake, hotkey_active, HookGuard, CTRL_STATE, FAKE_ACTIVE};
use crate::live::{
    install_console, open_mic, rms, spawn_enter_watch, tail_chars, LiveRow, MicSession,
    ENTER_PRESSED, Ring, STOP,
};
use crate::llm;
use crate::tts::{self, Speaker};

/* ================================================================ */
/* assistant session state                                          */
/* ================================================================ */

struct Asst {
    t0: Instant, /* wall-clock origin */

    /* audio accounting (same model as live.rs) */
    pushed_sec: f64,
    analyzed_sec: f64,

    /* speech meter */
    speaking: bool,
    last_loud: Option<Instant>,
    last_rms: f32,

    /* live prefill tracking */
    partial: String,      /* full text of the in-flight sentence     */
    pinged: String,       /* text the last prefill ping carried      */
    last_ping: Option<Instant>,
    last_cache_n: u64,    /* prompt tokens served from cache (warm)  */

    committed: usize, /* transcript lines already handled            */
    turns: usize,     /* completed user<->llm exchanges              */
    ever_spoken: bool,
    tools_on: bool,   /* tool-calling protocol active (Phase 4)      */
}

impl Asst {
    fn new() -> Self {
        Self {
            t0: Instant::now(),
            pushed_sec: 0.0,
            analyzed_sec: 0.0,
            speaking: false,
            last_loud: None,
            last_rms: 0.0,
            partial: String::new(),
            pinged: String::new(),
            last_ping: None,
            last_cache_n: 0,
            committed: 0,
            turns: 0,
            ever_spoken: false,
            tools_on: false,
        }
    }

    fn backlog(&self) -> f64 {
        (self.pushed_sec - self.analyzed_sec).max(0.0)
    }

    /* speech-state machine (copied from live.rs) */
    const METER_RMS: f32 = 0.010;

    fn update_meter(&mut self, rms: f32, now: Instant) {
        self.last_rms = 0.6 * rms + 0.4 * self.last_rms;
        if rms > Self::METER_RMS {
            if !self.speaking {
                self.speaking = true;
            }
            self.last_loud = Some(now);
        } else if self.speaking
            && let Some(l) = self.last_loud
            && (now - l) > Duration::from_millis(250)
        {
            self.speaking = false;
        }
    }
}

/* ================================================================ */
/* status row                                                       */
/* ================================================================ */

fn build_status(a: &Asst, mic_open: Option<bool>) -> String {
    let state = if a.speaking { "SPEAK" } else { "idle" };

    let shown: String = a.partial.chars().take(34).collect();
    let pad = 34usize.saturating_sub(shown.chars().count());
    let in_prog = if a.partial.is_empty() {
        " ".repeat(36)
    } else {
        format!("\"{}\"{}", shown, " ".repeat(pad))
    };

    let bars = ((a.last_rms * 1500.0) as usize).clamp(0, 14);
    let meter: String = (0..14).map(|i| if i < bars { '#' } else { '.' }).collect();

    let cache = if a.last_cache_n > 0 { "warm" } else { "cold" };

    /* open-mic mic state: muted while the assistant's voice plays */
    let mic = match mic_open {
        Some(true) => " | mic live",
        Some(false) => " | MIKE OFF",
        None => "",
    };

    format!(
        "[{:6.1}s] {:<5} {} {} backlog {:4.2}s | cache {}{}",
        a.t0.elapsed().as_secs_f64(),
        state,
        in_prog,
        meter,
        a.backlog(),
        cache,
        mic
    )
}

/* Status row for push-to-talk mode: while idle the meter keeps decaying
 * (nothing is being recorded) and the row says what to do; while held it
 * shows the hold timer, the in-flight transcript tail, the meter, the
 * un-analyzed audio backlog and the prompt-cache state. */
fn build_status_ptt(a: &Asst, holding: bool, hold_start: Option<Instant>) -> String {
    let bars = ((a.last_rms * 1500.0) as usize).clamp(0, 14);
    let meter: String = (0..14).map(|i| if i < bars { '#' } else { '.' }).collect();

    if !holding {
        return format!(
            "[{:6.1}s] HOLD CTRL+SPACE to talk {}",
            a.t0.elapsed().as_secs_f64(),
            meter
        );
    }
    let held = hold_start.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
    let shown = tail_chars(&a.partial, 34);
    let pad = 34usize.saturating_sub(shown.chars().count());
    let in_prog = if a.partial.is_empty() {
        " ".repeat(36)
    } else {
        format!("\"{}\"{}", shown, " ".repeat(pad))
    };
    let cache = if a.last_cache_n > 0 { "warm" } else { "cold" };
    format!(
        "[{:6.1}s] {:4.1}s REC {} {} backlog {:4.2}s | cache {}",
        a.t0.elapsed().as_secs_f64(),
        held,
        in_prog,
        meter,
        a.backlog(),
        cache
    )
}

/* ================================================================ */
/* synthetic test microphone (MOON_PTT_WAV)                          */
/* ================================================================ */

/* Build a MicSession whose ring is fed from a WAV file in real time
 * (looped), so push-to-talk gating can be exercised without a real mic.
 * The capture gate starts disabled; the PTT loop enables it on hold. */
fn synthetic_mic_session(path: &str) -> Result<MicSession, Box<dyn std::error::Error>> {
    let decoded = decode_audio_file(path)?;
    let rate = decoded.sample_rate;
    let ch = decoded.channels as usize;
    let pcm = if ch > 1 {
        let frames = decoded.pcm_data.len() / ch;
        let mut mono = Vec::with_capacity(frames);
        for f in 0..frames {
            let s = f * ch;
            let mut acc = 0f32;
            for c in 0..ch {
                acc += decoded.pcm_data[s + c];
            }
            mono.push(acc / ch as f32);
        }
        mono
    } else {
        decoded.pcm_data
    };
    println!(
        "  ptt test  : {path} ({} Hz, {:.2} s) loops as a synthetic mic",
        rate,
        pcm.len() as f64 / rate as f64
    );
    let ring = Arc::new(Ring::new());
    ring.set_enabled(false);
    let feeder_ring = Arc::clone(&ring);
    std::thread::spawn(move || {
        /* push samples up to the wall-clock position, like the WAV test
         * mode of live/assist; restart from the top after the file ends */
        let mut fed = 0usize;
        let mut t0 = Instant::now();
        loop {
            if STOP.load(Ordering::SeqCst) {
                return;
            }
            let target = ((t0.elapsed().as_secs_f64()) * rate as f64) as usize;
            if target >= pcm.len() {
                fed = 0;
                t0 = Instant::now();
                continue;
            }
            while fed < target {
                let take = (target - fed).min(6400);
                feeder_ring.push(&pcm[fed..fed + take]);
                fed += take;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    });
    Ok(MicSession {
        stream: None,
        ring,
        rate,
    })
}

/* ================================================================ */
/* live prefill ping                                                */
/* ================================================================ */

/* Push the partial transcript into the model's prompt cache. Throttled
 * to one ping per 250 ms and only when the text actually changed. */
fn maybe_ping(a: &mut Asst, history: &[Value]) {
    let due = a
        .last_ping
        .map(|t| t.elapsed() >= Duration::from_millis(250))
        .unwrap_or(true);
    if !due || a.partial.is_empty() || a.partial == a.pinged {
        return;
    }
    let mut msgs = history.to_vec();
    msgs.push(json!({"role": "user", "content": a.partial}));
    match llm::prefill(&msgs) {
        Ok(cache_n) => {
            a.pinged = a.partial.clone();
            a.last_cache_n = cache_n;
        }
        Err(e) => {
            /* the pipeline must survive server hiccups: log and go on */
            eprintln!("\rprefill ping failed: {e}");
        }
    }
    a.last_ping = Some(Instant::now());
}

/* ================================================================ */
/* one exchange: completed utterance -> streamed reply               */
/* ================================================================ */

/* `stop_when` is consulted for every token while Some; returning true
 * cuts the reply short — push-to-talk pressed again while the answer
 * was still streaming (barge-in; the in-flight TTS is killed too).
 * None = always stream to the end. 'k is the short reborrow lifetime,
 * 'd how long the underlying closure stays valid (they must not be
 * forced equal). `tts` speaks the reply sentence-by-sentence while the
 * tokens are still streaming. */
fn turn_reply<'k, 'd>(
    a: &mut Asst,
    history: &mut Vec<Value>,
    user_text: &str,
    mut stop_when: Option<&'k mut (dyn FnMut() -> bool + 'd)>,
    tts: Option<&Speaker>,
) -> Result<(), Box<dyn std::error::Error>> {
    let text = user_text.trim().trim_matches(|c: char| !c.is_alphanumeric() || c == '.' || c == ',').trim();
    let text = if text.is_empty() || text == "(no text)" {
        user_text.trim()
    } else {
        text
    };
    if text.is_empty() || text == "(no text)" {
        return Ok(());
    }

    println!("you :- {text}");
    history.push(json!({"role": "user", "content": text}));
    print!("ai:- ");
    let _ = io::stdout().flush();

    let mut cut = false;
    let mut spoken_buf = String::new();
    if a.tools_on {
        /* Agent loop (Phase 4): the model may answer with tool calls.
         * llm::agent_turn streams with protocol hold-back (envelopes,
         * think spans and terminal markers never reach the voice or the
         * terminal), executes the calls against the registry, feeds the
         * results back and re-streams until a plain final reply. */
        let mut filler_done = false;
        let mut on_released = |released: &str| -> bool {
            let stop = match stop_when.as_mut() {
                Some(f) => (*f)(),
                None => false,
            };
            if stop {
                cut = true;
                /* barge-in kills the in-flight voice as well */
                if let Some(sp) = tts {
                    sp.stop();
                }
                return false;
            }
            /* stream complete sentences to the voice while the LLM keeps
             * generating */
            if let Some(sp) = tts {
                spoken_buf.push_str(released);
                while let Some(sentence) = tts::take_sentence(&mut spoken_buf) {
                    if let Err(e) = sp.speak(&sentence) {
                        eprintln!("\r[tts] {e}");
                    }
                }
            }
            print!("{released}");
            let _ = io::stdout().flush();
            true
        };
        let mut on_tool_round = |names: &[String]| {
            if !filler_done {
                filler_done = true;
                /* one short filler per turn, spoken while the tools run */
                if let Some(sp) = tts {
                    let _ = sp.speak("ek seconds…");
                }
            }
            println!("\n[tools] {}", names.join(", "));
        };
        let mut sink = llm::StreamSink {
            on_released: &mut on_released,
            on_tool_round: &mut on_tool_round,
        };
        let end = llm::agent_turn(history, 512, &mut sink)?;
        println!();
        if cut {
            println!();
        }
        if matches!(end, llm::AgentTurnEnd::Exhausted) {
            if let Some(sp) = tts {
                let _ = sp.speak("I could not finish that one.");
            }
        }
    } else {
        let msgs = history.clone();
        let mut on_tok = |tok: &str| -> bool {
            let stop = match stop_when.as_mut() {
                Some(f) => (*f)(),
                None => false,
            };
            if stop {
                cut = true;
                /* barge-in kills the in-flight voice as well */
                if let Some(sp) = tts {
                    sp.stop();
                }
                return false;
            }
            /* stream complete sentences to the voice while the LLM keeps
             * generating */
            if let Some(sp) = tts {
                spoken_buf.push_str(tok);
                while let Some(sentence) = tts::take_sentence(&mut spoken_buf) {
                    if let Err(e) = sp.speak(&sentence) {
                        eprintln!("\r[tts] {e}");
                    }
                }
            }
            print!("{tok}");
            let _ = io::stdout().flush();
            true
        };
        let (reply, _first_ms, _stop) = llm::chat_stream(&msgs, 512, &mut on_tok, None)?;
        println!();
        if cut {
            println!();
        }
        if !reply.is_empty() {
            history.push(json!({"role": "assistant", "content": reply}));
        }
    }
    /* The tail after the last terminator is spoken too - unless the turn
     * was cut (barge-in): a hold during the reply means "be quiet and
     * listen", so nothing more is queued and the killed voice stays dead
     * until the next speak(). Neither mode blocks here anymore: open-mic
     * serializes via transcript_pass's busy-guard (its mic gate shows
     * MIKE OFF meanwhile) and push-to-talk serializes via the hold flow -
     * a new hold kills a still-playing voice at hold start. */
    if let Some(sp) = tts {
        let rest = tts::sanitize(&spoken_buf);
        if !cut && !rest.is_empty() {
            if let Err(e) = sp.speak(&rest) {
                eprintln!("\r[tts] {e}");
            }
        }
    }

    a.partial.clear();
    a.pinged.clear();
    a.last_ping = None;
    a.turns += 1;
    Ok(())
}

/* ================================================================ */
/* transcript handling (mirrors live.rs transcript_pass)             */
/* ================================================================ */

/* Handle one poll() result. In push-to-talk holds (`answer = false`)
 * completed sentences stay pending until the release: only the newest
 * line feeds the status row and the prefill pings, so the model never
 * hears anything before the user lets go of the keys. In always-on mode
 * (`answer = true`) a completed sentence becomes an LLM turn right away. */
fn transcript_pass(
    a: &mut Asst,
    history: &mut Vec<Value>,
    tr: moonshine_rs::Transcript,
    answer: bool,
    tts: Option<&Speaker>,
) {
    let n = tr.lines.len();

    if !answer {
        /* hold in progress: ping the newest line's text into the prompt
         * cache, but never start an exchange */
        if let Some(line) = tr.lines.last()
            && line.text != a.partial
        {
            a.partial = line.text.clone();
            maybe_ping(a, history);
        }
        return;
    }

    /* The voice is still speaking: hold the answer until it finishes.
     * The line stays pending and is answered on a later pass - this is
     * what serializes turns in open-mic mode now that turn_reply no
     * longer blocks on wait_drain (the main loop's mic gate keeps the
     * STT quiet while the voice plays). */
    if tts.is_some_and(|sp| sp.busy()) {
        return;
    }

    while a.committed < n {
        let line = &tr.lines[a.committed];
        let is_last = a.committed + 1 == n;
        let is_final = line.is_complete || !is_last;

        if !is_final {
            /* live, still-being-spoken sentence: full text is the partial */
            if line.text != a.partial {
                a.partial = line.text.clone();
                maybe_ping(a, history);
            }
            return;
        }

        /* completed sentence: one full exchange with the LLM */
        let text = line.text.clone();
        a.committed += 1;
        if let Err(e) = turn_reply(a, history, &text, None, tts) {
            eprintln!("llm turn failed: {e}");
        }
    }
}

/* ================================================================ */
/* main command                                                     */
/* ================================================================ */

pub fn cmd_assist(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut file_path: Option<String> = None;
    let mut model_dir = crate::paths::asset("stt/small-streaming").to_string_lossy().into_owned();
    let mut hotkey_mode = false;
    let mut no_tts = false;
    let mut voice: Option<String> = None;
    let mut cpu_only = false;
    let mut no_tools = false;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--model" => {
                i += 1;
                model_dir = args
                    .get(i)
                    .ok_or("option --model needs a directory argument")?
                    .clone();
            }
            "--hotkey" => {
                hotkey_mode = true;
            }
            "--no-tts" => {
                no_tts = true;
            }
            "--cpu" => {
                cpu_only = true;
            }
            "--no-tools" => {
                no_tools = true;
            }
            "--voice" => {
                i += 1;
                voice = Some(
                    args.get(i)
                        .ok_or("option --voice needs a voice id (e.g. en_US-amy-low)")?
                        .clone(),
                );
            }
            opt if opt.starts_with("--") => {
                return Err(format!(
                    "unknown option '{opt}' (usage: assist [WAV] [--model DIR] [--hotkey] [--no-tts] [--voice ID] [--cpu] [--no-tools])"
                )
                .into());
            }
            wav => {
                if file_path.is_some() {
                    return Err("only one positional WAV path is allowed".into());
                }
                file_path = Some(wav.to_string());
            }
        }
        i += 1;
    }
    let file_mode = file_path.is_some();
    if hotkey_mode && file_mode {
        /* a WAV input already streams in real time; PTT gating only makes
         * sense on a live microphone */
        return Err("--hotkey is microphone-only; a WAV input already streams in real time".into());
    }
    /* `MOON_NO_TTS=1` does the same as --no-tts (env test hooks style) */
    let use_tts = !no_tts && std::env::var("MOON_NO_TTS").is_err();
    /* Tools are always on; --no-tools / MOON_NO_TOOLS=1 is the escape hatch */
    let tools_on = llm::tools_enabled(no_tools);
    /* `MOON_VOICE=en_US-amy-low` does the same as --voice */
    let voice = voice.or_else(|| std::env::var("MOON_VOICE").ok());

    /* architecture follows the directory name (same rule as live.rs) */
    let arch = if model_dir.to_lowercase().contains("medium") {
        ModelArch::MediumStreaming
    } else {
        ModelArch::SmallStreaming
    };
    let arch_label = if matches!(arch, ModelArch::MediumStreaming) {
        "medium"
    } else {
        "small"
    };

    install_console();

    println!("Moonshine voice assistant (streaming {arch_label} STT + LFM3-1.7B via llama.cpp)");
    println!("  model dir : {model_dir}");
    match &file_path {
        None if hotkey_mode => {
            println!("  input     : microphone — push-to-talk (hold CTRL+SPACE;");
            println!("              the mic opens on press and closes on release)")
        }
        None => println!("  input     : microphone (ENTER to finish)"),
        Some(p) => println!("  input     : {p} (streamed in real time)"),
    }
    println!("  pipeline  : partial STT text is prefilled into the LLM while you");
    println!("              speak; on sentence end the reply streams live");
    println!();
    let _ = io::stdout().flush();

    /* ---------- models ---------- */
    let model_dir = crate::models::ensure_model(&model_dir, arch)?;

    let t_load = Instant::now();
    let transcriber = Transcriber::from_files(&model_dir, arch, None)?;
    llm::ensure_server(cpu_only)?;
    println!("  models    : loaded ({:.0} s)", t_load.elapsed().as_secs_f64());

    /* ---------- voice: piper speaks the replies sentence-by-sentence.
     * Failure is non-fatal: the assistant degrades to text-only. ---------- */
    let speaker = if use_tts {
        let started = match voice.as_deref() {
            Some(v) => tts::Speaker::start_with(v),
            None => tts::Speaker::start(),
        };
        match started {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("  tts       : unavailable ({e}); replies stay text-only");
                None
            }
        }
    } else {
        None
    };

    /* ---------- push-to-talk setup (hook before the mic, fail-closed) ---------- */
    let fake = hotkey_mode && std::env::var("MOON_FAKE_HOTKEY").is_ok();
    let _hook = if hotkey_mode {
        if fake {
            println!("  hotkey    : FAKE mode (MOON_FAKE_HOTKEY=1) — stdin p/r drives it");
            None
        } else {
            let hook = HookGuard::try_install().map_err(|e| {
                format!(
                    "failed to install global keyboard hook (code {}): {}",
                    e.raw_os_error().unwrap_or(-1),
                    e
                )
            })?;
            println!("  hotkey    : global hook installed (hold CTRL+SPACE to talk)");
            Some(hook)
        }
    } else {
        None
    };

    /* MOON_PTT_WAV swaps the real microphone for a WAV fed in real time
     * through the same ring gate — a way to exercise PTT gating (and the
     * fake hotkey) with recorded speech instead of a live mic. */
    let ptt_wav: Option<String> = if hotkey_mode {
        std::env::var("MOON_PTT_WAV").ok()
    } else {
        None
    };

    /* ---------- input source (same as live.rs) ---------- */
    let mut mic: Option<MicSession> = None;
    let mut file: Option<(Vec<f32>, u32, usize)> = None; /* (pcm, rate, fed) */

    if file_mode {
        let p = file_path.as_ref().unwrap();
        let decoded = decode_audio_file(p)?;
        let rate = decoded.sample_rate;
        let ch = decoded.channels as usize;
        let pcm = if ch > 1 {
            let frames = decoded.pcm_data.len() / ch;
            let mut mono = Vec::with_capacity(frames);
            for f in 0..frames {
                let s = f * ch;
                let mut acc = 0f32;
                for c in 0..ch {
                    acc += decoded.pcm_data[s + c];
                }
                mono.push(acc / ch as f32);
            }
            mono
        } else {
            decoded.pcm_data
        };
        println!(
            "  wav info  : {} Hz, {:.2} s",
            rate,
            pcm.len() as f64 / rate as f64
        );
        println!("\nStreaming (assistant listens)...");
        file = Some((pcm, rate, 0));
    } else if let Some(wav) = &ptt_wav {
        mic = Some(synthetic_mic_session(wav)?);
        maybe_spawn_fake(fake);
        println!("\nHOLD CTRL+SPACE to talk; release and the answer streams. Ctrl+C quits.");
    } else if hotkey_mode {
        /* Real microphone — but the device is NOT opened here: it is
         * opened exactly when the keys go down and dropped again on
         * release, so nothing, not even the capture device, is active
         * while idle (no idle room noise for the STT to mishear). */
        maybe_spawn_fake(fake);
        if !fake {
            spawn_enter_watch();
        }
        println!("\nHOLD CTRL+SPACE to talk; release and the answer streams. Ctrl+C quits.");
    } else {
        let m = open_mic()?;
        println!("  mic info  : native {} Hz (lib resamples internally)", m.rate);
        mic = Some(m);
        spawn_enter_watch();
        println!("\nLISTENING - ask something, pause, hear the answer. ENTER to finish.");
    }
    let _ = io::stdout().flush();
    std::thread::sleep(Duration::from_millis(50)); /* let capture settle */

    /* One long-running stream in always-listening mode; in push-to-talk
     * mode a fresh stream is created for every hold (mirrors hotkey.rs),
     * so each hold transcribes exactly the audio captured during it. */
    let mut stream = if hotkey_mode {
        None
    } else {
        Some(transcriber.create_stream()?)
    };

    /* ---------- live loop ---------- */
    let mut row = LiveRow::new();
    let mut asst = Asst::new();
    asst.tools_on = tools_on;
    let mut history: Vec<Value> = vec![json!({
        "role": "system",
        "content": llm::system_prompt_with_tools(llm::VOICE_SYSTEM_PROMPT, tools_on)
    })];
    let mut prev_status = String::new();
    let mut last_pass = Instant::now();
    let mut chunk: Vec<f32> = Vec::with_capacity(6400);
    let mut junk: Vec<f32> = Vec::with_capacity(6400);
    let mut prev_hotkey = false;
    /* Half-duplex latch: after the voice goes idle, keep discarding mic
     * input for a short while. The voice's own busy() already covers
     * synthesis gaps via its recency window; this latch covers the tail
     * of the drain and any mic samples still in flight. */
    let mut tts_busy_latch: Option<Instant> = None;
    /* Hard capture-gate state, kept for the status row: Some(true) =
     * collecting, Some(false) = muted (assistant speaking or the post-
     * speech latch is running), None = no open-mic mic session. */
    let mut mic_live: Option<bool> = None;
    let mut hold_start: Option<Instant> = None;
    let mut holds = 0usize;
    /* backoff after a failed hold attempt, so a broken/unplugged device
     * held down does not turn the idle loop into an open-mic retry storm */
    let mut hold_retry: Option<Instant> = None;

    /* Push-to-talk key state, read on demand: the main loop uses it to
     * detect hold start/end, and a reply that is still streaming when the
     * keys go down again is cut short on the next token (barge-in). */
    let mut key_held = || -> bool {
        if !hotkey_mode {
            return true;
        }
        if fake {
            FAKE_ACTIVE.load(Ordering::SeqCst)
        } else {
            let ctrl = match CTRL_STATE.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            hotkey_active(&ctrl)
        }
    };

    loop {
        if STOP.load(Ordering::SeqCst) {
            break;
        }

        let active = key_held();

        /* While the assistant is speaking (open-mic mode only) the mic
         * goes hard OFF: the capture gate itself is disabled below, so
         * the callback collects nothing at all, and the status row
         * shows "MIKE OFF". The busy latch keeps the mute running
         * through the drain tail (600 ms) so the STT never hears the
         * assistant's own voice. */
        let tts_busy = !hotkey_mode
            && !file_mode
            && speaker.as_ref().is_some_and(|sp| sp.busy());
        let mic_gated = tts_busy
            || tts_busy_latch.is_some_and(|t| t.elapsed() < Duration::from_millis(600));

        /* ---------- hold starts: open the mic, fresh gated stream ----------
         * Level-triggered on purpose: the keys can go down again while a
         * previous answer is still being cut short, and the hold must
         * start anyway. In real-mic mode the capture device itself is
         * opened exactly here — while idle nothing, not even the device,
         * is running, so there is no idle noise for the STT to mishear. */
        let can_start = hotkey_mode
            && active
            && hold_start.is_none()
            && stream.is_none()
            && hold_retry
                .map(|t| t.elapsed() >= Duration::from_millis(500))
                .unwrap_or(true);
        if can_start {
            /* barge-in at hold start: if the voice is still playing,
             * kill it here so the fresh recording never captures the
             * assistant's own speech (mic stays clean while the key is
             * held; a turn still streaming gets cut on its next token) */
            if let Some(sp) = speaker.as_ref() {
                if sp.busy() {
                    sp.stop();
                }
            }
            let mut opened = true;
            if ptt_wav.is_none() {
                match open_mic() {
                    Ok(m) => {
                        println!("[ptt] mic opened (native {} Hz)", m.rate);
                        mic = Some(m);
                        /* let the capture callback start producing */
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Err(e) => {
                        eprintln!("  [hold failed] open mic: {e}");
                        opened = false;
                    }
                }
            } else if let Some(m) = &mic {
                /* synthetic mic: everything buffered since idle is junk */
                m.ring.set_enabled(false);
                while m.ring.drain(6400, &mut junk) {
                    junk.clear();
                }
            }
            if opened
                && let Some(m) = &mic
            {
                match transcriber.create_stream() {
                    Ok(s) => {
                        /* fresh stream -> fresh transcript line numbering */
                        asst.committed = 0;
                        asst.partial.clear();
                        asst.pinged.clear();
                        asst.last_ping = None;
                        m.ring.set_enabled(true);
                        stream = Some(s);
                        hold_start = Some(Instant::now());
                        last_pass = Instant::now();
                        hold_retry = None;
                        row.clear();
                        println!("[ptt] recording... (release to answer)");
                    }
                    Err(e) => {
                        eprintln!("  [hold failed] create stream: {e}");
                        /* a half-opened real device must not leak */
                        if ptt_wav.is_none() {
                            mic = None;
                        }
                    }
                }
                let _ = io::stdout().flush();
            }
            if !opened {
                hold_retry = Some(Instant::now());
            }
        }

        /* ---- feed newly available audio into the STT stream ---- */
        let mut hold_failed = false;
        if let Some(m) = &mic {
            let recording = !hotkey_mode || active;
            if recording
                && let Some(s) = &mut stream
            {
                /* open-mic only: flip the hard capture gate to match the
                 * mute state (disabled => the callback appends nothing
                 * at all, not even junk) */
                if !hotkey_mode {
                    let want = !mic_gated;
                    if mic_live != Some(want) {
                        m.ring.set_enabled(want);
                        mic_live = Some(want);
                    }
                }
                if mic_gated {
                    /* the assistant's own voice: never let the STT hear it */
                    while m.ring.drain(6400, &mut chunk) {
                        chunk.clear();
                    }
                } else {
                    while m.ring.drain(6400, &mut chunk) {
                        if chunk.is_empty() {
                            continue;
                        }
                        let now = Instant::now();
                        asst.update_meter(rms(&chunk), now);
                        match s.add_audio(&chunk, m.rate) {
                            Ok(()) => {
                                asst.pushed_sec += chunk.len() as f64 / m.rate as f64;
                            }
                            Err(e) => {
                                if hotkey_mode {
                                    eprintln!("  [hold failed] add_audio: {e}");
                                    hold_failed = true;
                                } else {
                                    return Err(e.into());
                                }
                            }
                        }
                        chunk.clear();
                        if hold_failed {
                            break;
                        }
                    }
                }
            }
            if hotkey_mode && !active {
                /* synthetic mic between holds: nothing is retained */
                while m.ring.drain(6400, &mut junk) {
                    junk.clear();
                }
            }
        }
        if hotkey_mode && !active {
            /* idle: nothing is being recorded; keep the meter decaying */
            asst.update_meter(0.0, Instant::now());
        }
        if let Some((pcm, rate, fed)) = &mut file {
            /* real-time pacing: feed up to the wall-clock position */
            let target = ((asst.t0.elapsed().as_secs_f64()) * *rate as f64) as usize;
            let target = target.min(pcm.len());
            while *fed < target {
                let take = (target - *fed).min(6400);
                let window = &pcm[*fed..*fed + take];
                let now = Instant::now();
                asst.update_meter(rms(window), now);
                if let Some(s) = &mut stream {
                    s.add_audio(window, *rate)?;
                }
                asst.pushed_sec += take as f64 / *rate as f64;
                *fed += take;
            }
        }
        if hold_failed {
            /* drop the failed hold and go back to gated idle; the real
             * device goes away too, and the hold restarts (with backoff)
             * if the keys are still down */
            stream = None;
            hold_start = None;
            if ptt_wav.is_none() {
                mic = None;
            } else if let Some(m) = &mic {
                m.ring.set_enabled(false);
            }
            hold_retry = Some(Instant::now());
        }
        if asst.speaking {
            asst.ever_spoken = true;
        }

        /* ---- STT pass every 250 ms; completed lines trigger LLM turns ---- */
        let file_eof = match &file {
            Some((pcm, _, fed)) => *fed >= pcm.len(),
            None => false,
        };
        let want_pass = if file_eof {
            asst.backlog() > 0.03
        } else {
            last_pass.elapsed() >= Duration::from_millis(250)
        };
        if want_pass
            && let Some(s) = &mut stream
        {
            match s.poll(true) {
                Ok(tr) => {
                    asst.analyzed_sec = asst.pushed_sec;
                    /* in push-to-talk holds a completed sentence stays
                     * pending; it is answered only after the release */
                    transcript_pass(&mut asst, &mut history, tr, !hotkey_mode, speaker.as_ref());
                }
                Err(e) => {
                    eprintln!("\rtranscribe error: {e}");
                }
            }
            last_pass = Instant::now();
        }

        /* ---------- hold ends: answer, then hand back the mic ---------- */
        if hotkey_mode && !active && prev_hotkey && hold_start.is_some() {
            holds += 1;
            hold_start = None;
            row.clear();
            println!("[ptt] released — capture stopped; answering...");
            let _ = io::stdout().flush();
            if let Some(m) = &mic {
                /* hard boundary: no callback can append from here on */
                m.ring.set_enabled(false);
                if let Some(mut s) = stream.take() {
                    /* push whatever is still buffered, then decode it all */
                    while m.ring.drain(6400, &mut chunk) {
                        if !chunk.is_empty() {
                            if let Err(e) = s.add_audio(&chunk, m.rate) {
                                eprintln!("add_audio on release: {e}");
                            } else {
                                asst.pushed_sec += chunk.len() as f64 / m.rate as f64;
                            }
                        }
                        chunk.clear();
                    }
                    match s.finalize() {
                        Ok(tr) => {
                            asst.analyzed_sec = asst.pushed_sec;
                            while asst.committed < tr.lines.len() {
                                /* barge-in: the keys went down again before
                                 * every pending line was answered */
                                if key_held() {
                                    break;
                                }
                                let text = tr.lines[asst.committed].text.clone();
                                asst.committed += 1;
                                /* answers happen only here, after the
                                 * release; if the keys are pressed again
                                 * mid-reply the stream is cut on the next
                                 * token */
                                if let Err(e) = turn_reply(
                                    &mut asst,
                                    &mut history,
                                    &text,
                                    Some(&mut key_held),
                                    speaker.as_ref(),
                                ) {
                                    eprintln!("llm turn failed: {e}");
                                }
                            }
                            asst.committed = 0;
                        }
                        Err(e) => eprintln!("finalize stream: {e}"),
                    }
                }
            }
            stream = None;
            /* the capture device goes away again until the next hold */
            if ptt_wav.is_none() {
                mic = None;
            }
            asst.partial.clear();
            asst.pinged.clear();
            if !key_held() {
                println!("[ptt] ready — hold CTRL+SPACE to talk again");
                println!();
            }
            let _ = io::stdout().flush();
        }

        /* ---- redraw the status row when something changed ---- */
        let status = if hotkey_mode {
            build_status_ptt(&asst, active, hold_start)
        } else {
            build_status(&asst, mic_live)
        };
        if status != prev_status {
            row.show(&status);
            prev_status = status;
        }

        /* ---- exit conditions ---- */
        if ENTER_PRESSED.swap(false, Ordering::SeqCst) {
            break;
        }
        if file_eof && asst.backlog() < 0.03 && last_pass.elapsed() > Duration::from_millis(350) {
            break;
        }

        prev_hotkey = active;
        if tts_busy {
            tts_busy_latch = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(15));
    }

    /* ---------- finalize ---------- */
    /* stop capture, drain whatever is still buffered (same as live.rs) */
    let mic_left = mic.map(|m| (m.ring, m.rate));
    row.clear();
    prev_status.clear();

    if let Some((ring, rate)) = &mic_left {
        while ring.drain(6400, &mut chunk) {
            if !chunk.is_empty() {
                if let Some(s) = &mut stream {
                    let _ = s.add_audio(&chunk, *rate);
                    asst.pushed_sec += chunk.len() as f64 / *rate as f64;
                }
            }
            chunk.clear();
        }
    }
    if let Some((pcm, rate, fed)) = &mut file {
        while *fed < pcm.len() {
            let take = (pcm.len() - *fed).min(6400);
            if let Some(s) = &mut stream {
                s.add_audio(&pcm[*fed..*fed + take], *rate)?;
            }
            asst.pushed_sec += take as f64 / *rate as f64;
            *fed += take;
        }
    }
    if let Some(s) = stream.take() {
        let final_tr = s.finalize()?;
        asst.analyzed_sec = asst.pushed_sec;

        if hotkey_mode {
            /* quit while a hold was still running: the user interrupted,
             * so show the remaining transcript instead of starting new
             * LLM exchanges */
            for line in final_tr.lines.iter().skip(asst.committed) {
                if !line.text.is_empty() {
                    println!("{}", line.text);
                }
            }
        } else {
            /* every line still unhandled is spoken text too; each a turn */
            while asst.committed < final_tr.lines.len() {
                let text = final_tr.lines[asst.committed].text.clone();
                asst.committed += 1;
                if let Err(e) = turn_reply(&mut asst, &mut history, &text, None, speaker.as_ref()) {
                    eprintln!("llm turn failed: {e}");
                }
            }
        }
    }

    /* ---------- summary ---------- */
    if asst.turns == 0 && !asst.ever_spoken {
        println!("\n(no speech detected)");
    }
    println!("\n----- session summary -----");
    println!("  audio processed : {:.2} s", asst.pushed_sec);
    println!(
        "  session length  : {:.1} s (wall clock)",
        asst.t0.elapsed().as_secs_f64()
    );
    if hotkey_mode {
        println!("  holds           : {}", holds);
    }
    println!("  exchanges       : {}", asst.turns);
    println!("---------------------------");
    let _ = io::stdout().flush();

    Ok(())
}
