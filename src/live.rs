//! `igris-ai live [WAV] [--model DIR]`
//!
//! Live streaming transcription in pure Rust on top of `moonshine-rs` +
//! `cpal` — a port of the C tool `mic_transcribe.c`. The status row shows
//! the sentence currently being spoken (live partial), the mic level
//! meter, the un-analyzed audio backlog and the model's measured speed
//! (Nx real-time). Completed sentences are committed as text lines with
//! the measured end-to-end latency ("text ready N ms after speech
//! stopped").
//!
//! Input is the default microphone via cpal, or a WAV-file streamed in
//! real time (`live path.wav`) which exercises the exact same pipeline
//! without a mic — handy for testing.
//!
//! `--model DIR` selects the model directory; the architecture (small vs
//! medium streaming) is inferred from the directory name.
//!
//! The backlog/speed numbers behind the status row:
//! ```text
//! backlog = audio fed to the stream - audio the model has analyzed
//! speed   = ms of CPU per 1 s of audio (EMA), displayed as Nx real-time
//! ```

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use moonshine_rs::audio::decode_audio_file;
use moonshine_rs::{ModelArch, Transcriber};

/* ================================================================ */
/* globals: stop + enter flags                                      */
/* ================================================================ */

pub(crate) static STOP: AtomicBool = AtomicBool::new(false);
pub(crate) static ENTER_PRESSED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" {
    fn SetConsoleOutputCP(cp: u32) -> i32;
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

unsafe extern "system" fn ctrl_handler(_event: u32) -> i32 {
    STOP.store(true, Ordering::SeqCst);
    1 /* handled: don't kill the process, exit gracefully */
}

pub(crate) fn install_console() {
    unsafe {
        SetConsoleOutputCP(65001); /* UTF-8 */
        SetConsoleCtrlHandler(Some(ctrl_handler), 1);
    }
}

/* EXIT-thread watcher: a blocking read on stdin that flips ENTER_PRESSED.
 * If stdin is already closed/EOF (background runs), nothing is set and the
 * user can still finish with Ctrl-C. */
pub(crate) fn spawn_enter_watch() {
    std::thread::spawn(|| {
        let mut line = String::new();
        let _ = io::stdin().read_line(&mut line);
        if !line.is_empty() {
            ENTER_PRESSED.store(true, Ordering::SeqCst);
        }
    });
}

/* ================================================================ */
/* console helpers                                                  */
/* ================================================================ */

/* UTF-8 safe: copy the first `n` characters of src */
fn head_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/* UTF-8 safe: copy the last `n` characters of src */
pub(crate) fn tail_chars(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    let start = chars.len().saturating_sub(n);
    chars[start..].iter().collect()
}

/* rewrites the live status row in place, padding over leftovers */
pub(crate) struct LiveRow {
    last: usize, /* chars of the previous row, to erase them */
}

impl LiveRow {
    pub(crate) fn new() -> Self {
        Self { last: 0 }
    }
    pub(crate) fn show(&mut self, s: &str) {
        let n = s.chars().count();
        let pad = self.last.saturating_sub(n);
        print!("\r{}{}", s, " ".repeat(pad));
        let _ = io::stdout().flush();
        self.last = n;
    }
    /* clear the live row entirely (before printing a committed line) */
    pub(crate) fn clear(&mut self) {
        print!("\r{}\r", " ".repeat(self.last.max(88)));
        let _ = io::stdout().flush();
        self.last = 0;
    }
}

/* ================================================================ */
/* audio ring buffer (capture thread -> main loop)                  */
/* ================================================================ */

pub(crate) struct Ring {
    inner: std::sync::Mutex<RingInner>, /* unread samples + capture gate */
}

struct RingInner {
    samples: Vec<f32>,
    enabled: bool,
}

impl Ring {
    pub(crate) fn new() -> Self {
        Self {
            inner: std::sync::Mutex::new(RingInner {
                samples: Vec::with_capacity(64 * 1024),
                enabled: true,
            }),
        }
    }
    /* The gate and sample append share one mutex. Disabling the gate first
     * makes release a hard boundary: no callback can append after the main
     * loop begins draining the held segment. */
    pub(crate) fn set_enabled(&self, enabled: bool) {
        self.inner.lock().unwrap().enabled = enabled;
    }
    /* append samples to the unread pool (gated by `enabled`); used by the
     * capture callback and the MOON_PTT_WAV synthetic feeder */
    pub(crate) fn push(&self, samples: &[f32]) {
        let mut g = self.inner.lock().unwrap();
        if g.enabled {
            g.samples.extend_from_slice(samples);
        }
    }
    /* move up to `cap` samples into `out` (appending); true if moved any */
    pub(crate) fn drain(&self, cap: usize, out: &mut Vec<f32>) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.samples.is_empty() {
            return false;
        }
        let take = g.samples.len().min(cap);
        out.extend_from_slice(&g.samples[..take]);
        g.samples.drain(..take);
        true
    }
}

/* ================================================================ */
/* session state                                                    */
/* ================================================================ */

struct Session {
    t0: Instant, /* wall-clock origin (like now_ms() in the C program) */

    /* audio accounting */
    pushed_sec: f64,   /* total audio fed into the stream                */
    analyzed_sec: f64, /* total audio actually analyzed by the model     */

    /* speed model: ms of CPU per 1 s of audio */
    speed: f64, /* EMA; 0 until first measurement                  */
    calls: u64,
    total_proc_ms: f64,

    /* decode latency (EMA over lines' last_transcription_latency_ms) */
    latency: f64,

    /* speech meter */
    speaking: bool,
    last_loud: Option<Instant>, /* last moment above the VAD threshold */
    last_rms: f32,

    /* display */
    partial: String,  /* in-flight sentence (tail) for the live row     */
    committed: usize, /* transcript lines already finalized in the UI  */
    ever_spoken: bool,
}

impl Session {
    fn new() -> Self {
        Self {
            t0: Instant::now(),
            pushed_sec: 0.0,
            analyzed_sec: 0.0,
            speed: 0.0,
            calls: 0,
            total_proc_ms: 0.0,
            latency: 0.0,
            speaking: false,
            last_loud: None,
            last_rms: 0.0,
            partial: String::new(),
            committed: 0,
            ever_spoken: false,
        }
    }

    fn backlog(&self) -> f64 {
        (self.pushed_sec - self.analyzed_sec).max(0.0)
    }

    /* speaking meter threshold (RMS); fixed default since the old
     * `--vad` tuning knob was removed */
    const METER_RMS: f32 = 0.010;

    /* speech-state machine, fed with the RMS of the newly fed audio */
    fn update_meter(&mut self, rms: f32, now: Instant) {
        self.last_rms = 0.6 * rms + 0.4 * self.last_rms; /* decays to 0 on silence */
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

/* the live row: in-flight sentence + meter + backlog + speed */
fn build_status(s: &mut Session) -> String {
    let state = if s.speaking { "SPEAK" } else { "idle" };

    /* the in-flight sentence tail, displayed between quotes, fixed width */
    let shown = head_chars(&s.partial, 34);
    let pad = 34usize.saturating_sub(shown.chars().count());
    let in_prog = if s.partial.is_empty() {
        " ".repeat(36)
    } else {
        format!("\"{}\"{}", shown, " ".repeat(pad))
    };

    let bars = ((s.last_rms * 1500.0) as usize).clamp(0, 14);
    let meter: String = (0..14).map(|i| if i < bars { '#' } else { '.' }).collect();

    let xrt = if s.speed > 1.0 { 1000.0 / s.speed } else { 0.0 };

    format!(
        "[{:6.1}s] {:<5} {} {} backlog {:4.2}s | {:4.1}x",
        s.t0.elapsed().as_secs_f64(),
        state,
        in_prog,
        meter,
        s.backlog(),
        xrt
    )
}

/* ================================================================ */
/* transcription pass                                               */
/* ================================================================ */

/* handle one poll() result: commit completed sentences (printing the
 * measured latency), or refresh the in-flight partial shown on the
 * status row. Mirrors run_transcribe_pass(). */
fn transcript_pass(row: &mut LiveRow, s: &mut Session, tr: moonshine_rs::Transcript) {
    let n = tr.lines.len();
    if n == 0 {
        return;
    }
    while s.committed < n {
        let line = &tr.lines[s.committed];
        let is_last = s.committed + 1 == n;
        let is_final = line.is_complete || !is_last;

        if !is_final {
            /* live, still-being-spoken sentence: tail goes to the status row */
            let new_tail = tail_chars(&line.text, 34);
            if new_tail != s.partial {
                s.partial = new_tail;
            }
            if line.last_transcription_latency_ms > 0 {
                let l = line.last_transcription_latency_ms as f64;
                s.latency = if s.latency > 1.0 {
                    0.5 * l + 0.5 * s.latency
                } else {
                    l
                };
            }
            break;
        }

        /* completed sentence: commit with measured latency vs prediction */
        let now = Instant::now();
        let actual = if !s.speaking {
            s.last_loud
                .filter(|l| now.duration_since(*l) < Duration::from_millis(5000))
                .map(|l| now.duration_since(l).as_secs_f64() * 1000.0)
        } else {
            None
        };

        let text = if line.text.is_empty() {
            "(no text)"
        } else {
            &line.text
        };
        row.clear();
        if let Some(ms) = actual {
            println!(
                "[line {} | {:.2}s speech | decode {}ms | text ready {:.0} ms after speech stopped] {}",
                line.id % 100_000_000,
                line.duration,
                line.last_transcription_latency_ms,
                ms,
                text
            );
        } else {
            println!(
                "[line {} | {:.2}s speech | decode {}ms] {}",
                line.id % 100_000_000,
                line.duration,
                line.last_transcription_latency_ms,
                text
            );
        }
        let _ = io::stdout().flush();

        s.partial.clear();
        s.committed += 1;
        if actual.is_some() {
            s.last_loud = None; /* consumed */
        }
    }
}

/* ================================================================ */
/* microphone capture (cpal, any sample-format -> mono f32 ring)     */
/* ================================================================ */

pub(crate) struct MicSession {
    /* kept alive to run capture; dropped to stop the device. None for a
     * synthetic session (e.g. the MOON_PTT_WAV test feeder) which has no
     * device behind the ring. */
    #[allow(dead_code)] /* read only via Drop */ pub(crate) stream: Option<cpal::Stream>,
    pub(crate) ring: std::sync::Arc<Ring>,
    pub(crate) rate: u32,
}

/* build a mono downmix stream in the device's native sample format.
 * T is the concrete sample type; to_mono converts one input sample to the
 * f32 range [-1, 1] that the model expects (same math the C tool applies
 * to 16-bit capture). */
trait MicSample: cpal::SizedSample {
    fn to_mono(v: Self) -> f32;
}
impl MicSample for f32 {
    fn to_mono(v: Self) -> f32 {
        v
    }
}
impl MicSample for f64 {
    fn to_mono(v: Self) -> f32 {
        v as f32
    }
}
impl MicSample for i16 {
    fn to_mono(v: Self) -> f32 {
        v as f32 / 32768.0
    }
}
impl MicSample for i8 {
    fn to_mono(v: Self) -> f32 {
        v as f32 / 128.0
    }
}
impl MicSample for i32 {
    fn to_mono(v: Self) -> f32 {
        v as f32 / 2147483648.0
    }
}
impl MicSample for u16 {
    fn to_mono(v: Self) -> f32 {
        (v as f32 - 32768.0) / 32768.0
    }
}
impl MicSample for u8 {
    fn to_mono(v: Self) -> f32 {
        (v as f32 - 128.0) / 128.0
    }
}

fn mono_input_stream<T: MicSample>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    ring: std::sync::Arc<Ring>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let channels = (config.channels() as usize).max(1);
    let stream_config: cpal::StreamConfig = (*config).into();
    let stream = device.build_input_stream::<T, _, _>(
        stream_config,
        move |data: &[T], _info: &cpal::InputCallbackInfo| {
            let mut mono: Vec<f32> = Vec::with_capacity(data.len() / channels);
            if channels == 1 {
                for v in data {
                    mono.push(T::to_mono(*v));
                }
            } else {
                for frame in data.chunks(channels) {
                    let mut acc = 0f32;
                    for v in frame {
                        acc += T::to_mono(*v);
                    }
                    mono.push(acc / channels as f32);
                }
            }
            ring.push(&mono);
        },
        move |err| {
            eprintln!("capture error: {err}");
        },
        None,
    )?;
    Ok(stream)
}

pub(crate) fn open_mic() -> Result<MicSession, Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("microphone error: no default input device found")?;
    let config = device.default_input_config()?;
    let fmt = config.sample_format();
    let rate = config.sample_rate();
    let ring = std::sync::Arc::new(Ring::new());

    let stream = match fmt {
        cpal::SampleFormat::F32 => mono_input_stream::<f32>(&device, &config, ring.clone())?,
        cpal::SampleFormat::I16 => mono_input_stream::<i16>(&device, &config, ring.clone())?,
        cpal::SampleFormat::U16 => mono_input_stream::<u16>(&device, &config, ring.clone())?,
        cpal::SampleFormat::U8 => mono_input_stream::<u8>(&device, &config, ring.clone())?,
        cpal::SampleFormat::I8 => mono_input_stream::<i8>(&device, &config, ring.clone())?,
        cpal::SampleFormat::I32 => mono_input_stream::<i32>(&device, &config, ring.clone())?,
        cpal::SampleFormat::F64 => mono_input_stream::<f64>(&device, &config, ring.clone())?,
        other => return Err(format!("microphone sample format {other} is not supported").into()),
    };
    stream.play()?;
    Ok(MicSession {
        stream: Some(stream),
        ring,
        rate,
    })
}

/* ================================================================ */
/* main live command                                                */
/* ================================================================ */

pub fn cmd_live(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut file_path: Option<String> = None;
    let mut model_dir = "models/small-streaming".to_string();

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
            opt if opt.starts_with("--") => {
                return Err(
                    format!("unknown option '{opt}' (usage: live [WAV] [--model DIR])").into(),
                );
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

    /* architecture follows the directory name: "…medium…" -> MediumStreaming */
    let arch = if model_dir.to_lowercase().contains("medium") {
        ModelArch::MediumStreaming
    } else {
        ModelArch::SmallStreaming
    };
    let arch_label = match arch {
        ModelArch::MediumStreaming => "medium",
        _ => "small",
    };

    install_console();
    let t_begin = Instant::now();

    println!("Moonshine live transcriber (Rust, streaming {arch_label} model)");
    println!("  model dir  : {model_dir}");
    match &file_path {
        None => println!("  input      : microphone (press ENTER to finish)"),
        Some(p) => println!("  input      : {p} (streamed in real time)"),
    }
    println!("  status row : live speech + backlog + model speed");
    println!("  text lines : completed sentences with measured latency");
    println!();
    let _ = io::stdout().flush();

    /* fetch the pinned model set if missing (same manifest as batch mode) */
    crate::ensure_model(&model_dir, arch)?;

    /* ---------- load model ---------- */
    let t_load = Instant::now();
    let transcriber = Transcriber::from_files(&model_dir, arch, None)?;
    println!("  models    : loaded ({:.0} s)", t_load.elapsed().as_secs_f64());

    let mut stream = transcriber.create_stream()?;

    /* ---------- input source ---------- */
    let mut mic: Option<MicSession> = None;
    let mut file: Option<(Vec<f32>, u32, usize)> = None; /* (pcm, rate, fed) */

    if file_mode {
        let p = file_path.as_ref().unwrap();
        let decoded = decode_audio_file(p)?;
        let rate = decoded.sample_rate;
        let ch = decoded.channels as usize;
        let pcm = if ch > 1 {
            /* downmix interleaved frames to mono */
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
            "  wav info   : {} Hz, {:.2} s",
            rate,
            pcm.len() as f64 / rate as f64
        );
        println!("\nStreaming + transcribing...");
        file = Some((pcm, rate, 0));
    } else {
        let m = open_mic()?;
        println!(
            "  mic info   : native {} Hz (lib resamples internally)",
            m.rate
        );
        println!("\nLISTENING - speak now. ENTER to finish.");
        mic = Some(m);
        spawn_enter_watch();
    }
    let _ = io::stdout().flush();
    std::thread::sleep(Duration::from_millis(50)); /* let capture settle */

    /* ---------- live loop ---------- */
    let mut row = LiveRow::new();
    let mut session = Session::new();
    let mut prev_status = String::new();
    let mut last_pass = Instant::now();
    let mut chunk: Vec<f32> = Vec::with_capacity(6400);

    loop {
        if STOP.load(Ordering::SeqCst) {
            break;
        }

        /* ---- feed newly available audio into the model stream ---- */
        if let Some(m) = &mic {
            while m.ring.drain(6400, &mut chunk) {
                if chunk.is_empty() {
                    continue;
                }
                let now = Instant::now();
                session.update_meter(rms(&chunk), now);
                stream.add_audio(&chunk, m.rate)?;
                session.pushed_sec += chunk.len() as f64 / m.rate as f64;
                chunk.clear();
            }
        }
        if let Some((pcm, rate, fed)) = &mut file {
            /* real-time pacing: feed up to the wall-clock position */
            let target = ((session.t0.elapsed().as_secs_f64()) * *rate as f64) as usize;
            let target = target.min(pcm.len());
            while *fed < target {
                let take = (target - *fed).min(6400);
                let window = &pcm[*fed..*fed + take];
                let now = Instant::now();
                session.update_meter(rms(window), now);
                stream.add_audio(window, *rate)?;
                session.pushed_sec += take as f64 / *rate as f64;
                *fed += take;
            }
        }
        if session.speaking {
            session.ever_spoken = true;
        }

        /* ---- analyze: forced pass every 250 ms (like the C tool) ----
         * After EOF: only when the model still has audio to catch up on,
         * so the 350 ms post-EOF settle window in the exit check below
         * can actually elapse. */
        let file_eof = match &file {
            Some((pcm, _, fed)) => *fed >= pcm.len(),
            None => false,
        };
        let want_pass = if file_eof {
            session.backlog() > 0.03
        } else {
            last_pass.elapsed() >= Duration::from_millis(250)
        };
        if want_pass {
            let pass_t0 = Instant::now();
            match stream.poll(true) {
                Ok(tr) => {
                    let dt = pass_t0.elapsed().as_secs_f64() * 1000.0;
                    session.calls += 1;
                    session.total_proc_ms += dt;

                    /* measured speed: ms of CPU per second of new audio */
                    let new_audio = session.pushed_sec - session.analyzed_sec;
                    if new_audio > 0.02 && dt > 0.1 {
                        let inst = dt / new_audio;
                        session.speed = if session.speed > 1.0 {
                            0.55 * inst + 0.45 * session.speed
                        } else {
                            inst
                        };
                    }
                    session.analyzed_sec = session.pushed_sec;

                    transcript_pass(&mut row, &mut session, tr);
                }
                Err(e) => {
                    /* non-fatal: log once and keep going (same as the C tool) */
                    eprintln!("\rtranscribe error: {e}");
                }
            }
            last_pass = Instant::now();
        }

        /* ---- redraw the live status row when something changed ---- */
        let status = build_status(&mut session);
        if status != prev_status {
            row.show(&status);
            prev_status = status;
        }

        /* ---- exit conditions ---- */
        if ENTER_PRESSED.swap(false, Ordering::SeqCst) {
            break;
        }
        if file_eof && session.backlog() < 0.03 && last_pass.elapsed() > Duration::from_millis(350)
        {
            break;
        }

        std::thread::sleep(Duration::from_millis(15));
    }

    /* ---------- finalize ---------- */
    /* stop capture (release the device) but keep the ring + rate around so
     * whatever is still buffered can be drained, like the C program does */
    let mic_left = mic.map(|m| (m.ring, m.rate));
    row.clear();
    prev_status.clear();

    /* push whatever is left, then finalize (= stop stream + forced poll) */
    if let Some((ring, rate)) = &mic_left {
        while ring.drain(6400, &mut chunk) {
            if !chunk.is_empty() {
                stream.add_audio(&chunk, *rate)?;
                session.pushed_sec += chunk.len() as f64 / *rate as f64;
            }
            chunk.clear();
        }
    }
    if let Some((pcm, rate, fed)) = &mut file {
        while *fed < pcm.len() {
            let take = (pcm.len() - *fed).min(6400);
            stream.add_audio(&pcm[*fed..*fed + take], *rate)?;
            session.pushed_sec += take as f64 / *rate as f64;
            *fed += take;
        }
    }
    let final_tr = stream.finalize()?;
    transcript_pass(&mut row, &mut session, final_tr);

    /* ---------- summary ---------- */
    let wall = t_begin.elapsed().as_secs_f64();
    let steady_xrt = if session.speed > 1.0 {
        1000.0 / session.speed
    } else {
        0.0
    };
    let overall_xrt = if session.total_proc_ms > 1.0 {
        session.pushed_sec * 1000.0 / session.total_proc_ms
    } else {
        0.0
    };

    if session.committed == 0 && !session.ever_spoken {
        println!("\n(no speech detected)");
    }

    println!("\n----- session summary -----");
    println!("  audio processed : {:.2} s", session.pushed_sec);
    println!("  session length  : {:.2} s (wall clock)", wall);
    println!(
        "  analysis passes : {} (total CPU {:.0} ms)",
        session.calls, session.total_proc_ms
    );
    if steady_xrt > 0.0 {
        println!(
            "  model speed     : {:.1}x real-time (steady-state)",
            steady_xrt
        );
    } else {
        println!(
            "  model speed     : {:.1}x real-time (overall)",
            overall_xrt
        );
    }
    println!("  sentences done  : {}", session.committed);
    println!(
        "  decode latency  : {:.0} ms (EMA of model reports)",
        session.latency
    );
    println!("---------------------------");
    let _ = io::stdout().flush();

    Ok(())
}

/* rms of a chunk */
pub(crate) fn rms(chunk: &[f32]) -> f32 {
    if chunk.is_empty() {
        return 0.0;
    }
    let mut e = 0f64;
    for &v in chunk {
        e += v as f64 * v as f64;
    }
    (e / chunk.len() as f64).sqrt() as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_gate_drops_audio_while_disabled() {
        let ring = Ring::new();
        ring.set_enabled(false);
        ring.push(&[0.1, 0.2]);
        let mut out = Vec::new();
        assert!(!ring.drain(64, &mut out));

        ring.set_enabled(true);
        ring.push(&[0.3, 0.4]);
        assert!(ring.drain(64, &mut out));
        assert_eq!(out, vec![0.3, 0.4]);

        ring.set_enabled(false);
        ring.push(&[0.5]);
        assert!(!ring.drain(64, &mut Vec::new()));
    }
}
