//! TTS through the piper.exe runtime bundled in `models/piper/`.
//!
//! Exposes two things:
//! - the `speak` subcommand (`moonshine-test speak "some text"`), and
//! - `Speaker`, the streaming voice backend `assist` uses to read replies
//!   aloud sentence-by-sentence while the LLM is still generating.
//!
//! Runtime: piper 2023.11.14-2 (Windows x64) with its matching
//! `espeak-ng-data`. The voice lives in `models/tts/` and is fetched on
//! first use from the pinned `v1.0.0` tag of rhasspy/piper-voices (the
//! current `main`-branch voice configs carry multi-codepoint phoneme
//! keys this piper build rejects, so the tag is load-bearing).
//!
//! Protocol (verified empirically against this exact piper.exe): one
//! long-lived process, one text line on stdin per utterance; every line
//! is written as its own WAV file into `--output_dir .tts-utt/` with a
//! nanosecond-timestamp name (chronological order = utterance order).
//! The stdout pipe of this build carries CORRUPTED PCM - every voice
//! plays as noise regardless of byte-order interpretation (little-
//! endian, big-endian, float all garble), while `-f <file>` and
//! `--output_dir` output is clean, little-endian, exact-size. So the
//! reader consumes files instead of the pipe. A file is complete once
//! it parses as RIFF/WAVE with a non-empty, exact-size `data` chunk
//! (piper patches the header sizes after the last sample); it is then
//! parked in the next of three fixed rotation slots (`tts_1.wav` ..
//! `tts_3.wav`, overwritten round-robin) with the other slots cleared
//! while it is consumed, converted, pushed into the ring and deleted -
//! so the folder never accumulates files and disk use stays bounded
//! (MOON_TTS_DEBUG keeps the WAVs, but they rotate through the same
//! slots).
//!
//! `--sentence_silence` must stay OFF: with it, piper re-splits every
//! line into per-sentence WAVs whose streamed header sizes lie on a
//! pipe (it cannot seek back to patch them), which unframes the reader;
//! the pause between lines is inserted by the reader instead.
//!
//! The reader converts each utterance's PCM to mono f32, resamples to
//! the output device's rate and feeds a small ring; a cpal output
//! stream drains it, padding with silence on underrun. The piper child
//! stays alive between utterances so the voice loads once (warm
//! synthesis ~0.2 s; a fresh spawn costs ~0.5-3.5 s depending on disk
//! cache, which only happens after a barge-in). `stop()` kills it
//! (barge-in) and the next `speak()` respawns it.
//!
//! The default voice is `en_US-libritts_r-medium#051` (same voice the
//! igrisv3 app uses): LibriTTS-R medium is a 904-speaker model, so the
//! voice id carries a `#NNN` suffix with the speaker index, forwarded
//! to piper as `--speaker`. amy-style voices are single-speaker and
//! need no suffix.

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde_json::Value;

/* ================================================================ */
/* constants                                                        */
/* ================================================================ */

const PIPER_EXE: &str = "models/piper/piper.exe";
const ESPEAK_DATA: &str = "models/piper/espeak-ng-data";
const VOICE_DIR: &str = "models/tts";
const DEFAULT_VOICE: &str = "en_US-libritts_r-medium#051";
/* Pinned tag: main-branch voice configs break this piper build (see the
 * module docs). Do not bump to `main` without re-probing piper. */
const VOICES_URL: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0";

/* Per-utterance WAV files land here (relative to the working dir, like
 * every other path this app uses). The reader consumes and deletes
 * them; MOON_TTS_DEBUG keeps them for offline inspection. */
const OUT_DIR: &str = ".tts-utt";
/* The reader rotates finished utterances through this many fixed slot
 * names (.tts-utt/tts_1.wav ...) and clears the other slots while one
 * is being consumed, so the folder never accumulates files and disk
 * use stays bounded no matter how many sentences are queued. */
const UTT_SLOTS: usize = 3;
/* How often the reader polls the output dir / a pending file. */
const POLL: Duration = Duration::from_millis(15);
/* Give up on an utterance file that never completes (child crash). */
const WAIT_FILE_MAX: Duration = Duration::from_secs(30);
/* Ring capacity in device-rate samples (~1.4 s @ 48 kHz): the
 * backpressure window. Deep enough that brief LLM pauses between
 * sentences never empty the ring, which would flicker busy() and open
 * the mic gate mid-reply. */
const RING_CAP: usize = 64 * 1024;
/* Seconds of silence the reader inserts after each spoken line so
 * sentences don't run together. piper's own --sentence_silence does
 * this, but with it on piper re-splits every line into per-sentence
 * WAVs whose streamed header sizes lie on a pipe (it cannot seek back
 * to patch them), which unframes the reader - so we pad ourselves. */
const SENTENCE_PAUSE: f32 = 0.2;

/* ================================================================ */
/* voice discovery + download                                       */
/* ================================================================ */

struct VoiceSpec {
    id: String,
    /* Speaker index inside a multi-speaker model, taken from the
     * "#NNN" suffix of the voice id (en_US-libritts_r-medium#051 =
     * piper --speaker 51). None for single-speaker voices (amy). */
    speaker: Option<String>,
    onnx: PathBuf,
    /* sample_rate from the voice config, for the info line only; the
     * real rate comes from each WAV's fmt chunk at runtime */
    sample_rate: u32,
    /* How many speakers the model carries (1 for amy-style voices).
     * Multi-speaker models require the "#NNN" suffix. */
    num_speakers: usize,
}

impl VoiceSpec {
    /* Resolve `<id>.onnx` + `<id>.onnx.json` under models/tts, downloading
     * from the pinned voices tag when missing or truncated. Same
     * size-check + redownload approach as crate::ensure_model. */
    fn ensure(id: &str) -> Result<Self, Box<dyn std::error::Error>> {
        /* A "#NNN" suffix picks a speaker from a multi-speaker model:
         * en_US-libritts_r-medium#051 = voice en_US-libritts_r-medium,
         * piper --speaker 51 (leading zeros are fine, piper parses the
         * value as a number). Single-speaker voices carry no suffix. */
        let (voice_id, speaker) = match id.split_once('#') {
            Some((v, s)) => (v, Some(s.to_string())),
            None => (id, None),
        };
        if let Some(s) = &speaker {
            if s.is_empty() || !s.chars().all(|c| c.is_ascii_digit()) {
                return Err(format!(
                    "invalid speaker index '{s}' after '#' in voice id '{id}' (expected digits)"
                )
                .into());
            }
        }
        /* The id is interpolated into file paths and the download URL,
         * so only the expected voice-id shape is accepted. */
        if !is_valid_voice_id(voice_id) {
            return Err(format!(
                "invalid voice id '{voice_id}' (expected something like en_US-amy-medium)"
            )
            .into());
        }
        /* id shape: {lang}_{region}-{name}-{quality} (en_US-amy-medium) */
        let (lang, rest) = voice_id
            .split_once('_')
            .ok_or("voice id must look like en_US-amy-medium")?;
        let (region, rest) = rest
            .split_once('-')
            .ok_or("voice id must look like en_US-amy-medium")?;
        let (name, quality) = rest
            .rsplit_once('-')
            .ok_or("voice id must look like en_US-amy-medium")?;
        let dir = Path::new(VOICE_DIR);
        fs::create_dir_all(dir)?;
        let onnx = dir.join(format!("{voice_id}.onnx"));
        let json = dir.join(format!("{voice_id}.onnx.json"));
        /* repo layout: {lang}/{lang}_{region}/{name}/{quality}/{id} */
        let base = format!("{VOICES_URL}/{lang}/{lang}_{region}/{name}/{quality}/{voice_id}");

        fetch_if_missing(&onnx, &format!("{base}.onnx"), 1_000_000)?;
        fetch_if_missing(&json, &format!("{base}.onnx.json"), 100)?;

        let config: Value = serde_json::from_str(&fs::read_to_string(&json)?)?;
        let sample_rate = config["audio"]["sample_rate"].as_u64().unwrap_or(22050) as u32;
        let num_speakers = config["num_speakers"].as_u64().unwrap_or(1) as usize;
        if num_speakers > 1 {
            match &speaker {
                Some(s) => {
                    let idx: usize = s.parse().unwrap_or(usize::MAX);
                    if idx >= num_speakers {
                        return Err(format!(
                            "speaker {s} is out of range for {voice_id} ({num_speakers} speakers)"
                        )
                        .into());
                    }
                }
                None => {
                    return Err(format!(
                        "{voice_id} is a multi-speaker model ({num_speakers} voices); pick one \
                         with a '#NNN' suffix, e.g. {voice_id}#051"
                    )
                    .into());
                }
            }
        }
        Ok(Self {
            id: id.to_string(),
            speaker,
            onnx,
            sample_rate,
            num_speakers,
        })
    }
}

/* Voice ids end up inside `models/tts/<id>.onnx` paths and download
 * URLs, so accept only a safe charset: letters, digits, `_`, `-`, not
 * starting with `-`, reasonably short. */
fn is_valid_voice_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/* Fetch `url` into `dest` unless a healthy copy is already on disk. */
fn fetch_if_missing(dest: &Path, url: &str, min_size: u64) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(meta) = fs::metadata(dest) {
        if meta.len() >= min_size {
            return Ok(());
        }
        eprintln!(
            "  tts       : {} is too small ({} bytes); re-downloading",
            dest.display(),
            meta.len()
        );
        fs::remove_file(dest)?;
    }
    eprintln!("  tts       : fetching {} <- {url}", dest.display());
    let resp = ureq::get(url).call()?;
    let mut bytes = Vec::new();
    resp.into_body().into_reader().read_to_end(&mut bytes)?;
    fs::write(dest, &bytes)?;
    Ok(())
}

/* ================================================================ */
/* shared state                                                     */
/* ================================================================ */

struct Inner {
    voice: VoiceSpec,
    /* output device rate the reader resamples to */
    dst_rate: u32,

    stdin: Mutex<Option<ChildStdin>>,
    child: Mutex<Option<Child>>,

    /* resampled f32 samples at the output device's rate */
    ring: Arc<Mutex<VecDeque<f32>>>,
    /* wakes the reader when the playback drains the ring (backpressure) */
    ring_cv: Arc<Condvar>,
    /* utterances written to stdin but not yet consumed off disk by the
     * reader */
    pending: AtomicI64,
    /* set by the reader when the child is gone; speak() respawns then */
    dead: AtomicBool,
    /* last time the reader consumed an utterance (busy() recency) */
    last_byte: Mutex<Option<Instant>>,
    /* MOON_TTS_DEBUG=1 turns on state-machine traces and keeps the
     * raw utterance WAVs in .tts-utt/ */
    debug: bool,
    /* process start time; the MOON_TTS_DEBUG traces show queue ->
     * consume latency against it */
    t0: Instant,
}

impl Drop for Inner {
    fn drop(&mut self) {
        *self.stdin.lock().unwrap() = None; /* close pipe -> piper exits */
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait(); /* reap so nothing is left dangling */
        }
    }
}

pub struct Speaker {
    inner: Arc<Inner>,
    /* keeps the cpal output stream running; dropped before `inner` */
    _stream: Option<cpal::Stream>,
}

/* ================================================================ */
/* piper child + utterance reader thread                            */
/* ================================================================ */

/* Spawn piper and its dir-watching reader. Called once at start and
 * again after every kill (barge-in) or crash. */
fn spawn_child(inner: &Arc<Inner>) -> Result<(), Box<dyn std::error::Error>> {
    /* fresh output dir: stale files from a previous run must never be
     * mistaken for new utterances */
    let dir = Path::new(OUT_DIR);
    fs::create_dir_all(dir)?;
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x == "wav") {
                let _ = fs::remove_file(&p);
            }
        }
    }
    let mut cmd = Command::new(PIPER_EXE);
    cmd.arg("-m")
        .arg(&inner.voice.onnx)
        .arg("--espeak_data")
        .arg(ESPEAK_DATA);
    /* Multi-speaker model: the "#NNN" suffix of the voice id selects
     * the speaker inside it. */
    if let Some(sp) = &inner.voice.speaker {
        cmd.arg("--speaker").arg(sp);
    }
    cmd.arg("-q")
        .arg("--output_dir")
        .arg(OUT_DIR)
        .stdin(Stdio::piped())
        /* this build's stdout WAV stream carries corrupted PCM - the
         * utterances are consumed from --output_dir files instead */
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    /* 0x08000000 = CREATE_NO_WINDOW: keeps piper's console window from
     * flashing when this runs under a GUI host or a service */
    #[cfg(target_os = "windows")]
    cmd.creation_flags(0x0800_0000);
    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().ok_or("piper: stdin unavailable")?;
    *inner.child.lock().unwrap() = Some(child);
    *inner.stdin.lock().unwrap() = Some(stdin);
    inner.dead.store(false, Ordering::SeqCst);
    let reader = Arc::clone(inner);
    let dst_rate = inner.dst_rate;
    std::thread::Builder::new()
        .name("piper-utterances".into())
        .spawn(move || utterance_loop(reader, dst_rate))?;
    Ok(())
}

/* Parse a complete RIFF/WAVE buffer: returns
 * (rate, channels, bits, data bytes). Returns None while the buffer is
 * incomplete (chunk sizes exceed the buffer) - piper patches the
 * header sizes after the last sample, so an in-progress file does not
 * parse as complete yet. */
fn parse_wav(bytes: &[u8]) -> Option<(u32, usize, u16, &[u8])> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut p = 12;
    let mut fmt: Option<(u32, usize, u16)> = None;
    while p + 8 <= bytes.len() {
        let id = &bytes[p..p + 4];
        let size = u32::from_le_bytes([bytes[p + 4], bytes[p + 5], bytes[p + 6], bytes[p + 7]])
            as usize;
        if id == b"fmt " {
            if p + 8 + size > bytes.len() {
                return None;
            }
            let q = p + 8;
            let channels = u16::from_le_bytes([bytes[q + 2], bytes[q + 3]]) as usize;
            let rate = u32::from_le_bytes([bytes[q + 4], bytes[q + 5], bytes[q + 6], bytes[q + 7]]);
            let bits = u16::from_le_bytes([bytes[q + 14], bytes[q + 15]]);
            fmt = Some((rate, channels, bits));
            p += 8 + size;
        } else if id == b"data" {
            let (rate, channels, bits) = fmt?;
            if p + 8 + size > bytes.len() {
                return None; /* data still being written */
            }
            return Some((rate, channels, bits, &bytes[p + 8..p + 8 + size]));
        } else {
            /* unknown chunk: skip payload once fully buffered */
            if p + 8 + size > bytes.len() {
                return None;
            }
            p += 8 + size;
        }
    }
    None
}

/* True when the piper child has exited (or the slot is empty). */
fn child_gone(inner: &Inner) -> bool {
    inner
        .child
        .lock()
        .unwrap()
        .as_mut()
        .map(|c| c.try_wait().map(|s| s.is_some()).unwrap_or(false))
        .unwrap_or(true)
}

/* Convert one WAV stream's PCM bytes to mono f32 and feed the ring. */
fn push_pcm(
    inner: &Inner,
    res: &mut Option<Resampler>,
    dst_rate: u32,
    rate: u32,
    channels: usize,
    bits: u16,
    bytes: &[u8],
) {
    if bits != 16 || channels == 0 {
        return; /* piper always emits 16-bit; anything else is skipped */
    }
    let bytes_per_frame = channels * 2;
    let frames = bytes.len() / bytes_per_frame;
    let mut mono: Vec<f32> = Vec::with_capacity(frames);
    for f in 0..frames {
        let base = f * bytes_per_frame;
        let mut acc = 0i64;
        for c in 0..channels {
            /* file-mode output is little-endian PCM16 (the stdout pipe
             * of this build is corrupt and is not used) */
            let s = i16::from_le_bytes([bytes[base + c * 2], bytes[base + c * 2 + 1]]);
            acc += s as i64;
        }
        mono.push(acc as f32 / channels as f64 as f32 / 32768.0);
    }

    if res.as_ref().is_none_or(|r| r.rate != rate) {
        *res = Some(Resampler::new(rate, dst_rate));
    }
    let mut out: Vec<f32> = Vec::with_capacity(mono.len() * 2);
    res.as_mut().unwrap().push(&mono, &mut out);

    /* MOON_TTS_DEBUG: capture exactly what reaches the ring (f32le at
     * the device rate) so playback quality can be checked offline */
    if inner.debug {
        dump_ring(&out);
    }

    let mut g = inner.ring.lock().unwrap();
    for v in out {
        /* Backpressure: piper synthesizes far faster than real time, so
         * the reader waits when the ring is full instead of dropping
         * audio (dropping chopped every utterance into noise). The wait
         * releases the lock, so the playback callback keeps popping;
         * stop() sets `dead` and notifies to let the reader bail out. */
        while g.len() >= RING_CAP && !inner.dead.load(Ordering::SeqCst) {
            g = match inner.ring_cv.wait_timeout(g, Duration::from_millis(100)) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
        if g.len() >= RING_CAP {
            break; /* dead/barging-in: drop the rest of this chunk */
        }
        g.push_back(v);
    }
    drop(g);
}

/* Pad `secs` of silence into the ring after a spoken line ends, through
 * the same backpressure path as real audio. */
fn push_silence(inner: &Inner, dst_rate: u32, secs: f32) {
    let n = (dst_rate as f32 * secs) as usize;
    let mut g = match inner.ring.lock() {
        Ok(g) => g,
        Err(e) => e.into_inner(),
    };
    for _ in 0..n {
        while g.len() >= RING_CAP && !inner.dead.load(Ordering::SeqCst) {
            g = match inner.ring_cv.wait_timeout(g, Duration::from_millis(100)) {
                Ok((g, _)) => g,
                Err(e) => e.into_inner().0,
            };
        }
        if g.len() >= RING_CAP {
            break; /* dead/barging-in: stop padding */
        }
        g.push_back(0.0);
    }
    drop(g);
}

/* One utterance fully consumed (its file was read). Saturating so the
 * counter can never go negative if piper emits more WAVs than lines
 * were queued. */
fn dec_pending(inner: &Inner) {
    let _ = inner
        .pending
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| Some(v.saturating_sub(1)));
}

/* MOON_TTS_DEBUG: ring-bound audio lands here (f32le, device rate) so
 * playback quality can be analyzed offline. */
fn dump_ring(samples: &[f32]) {
    static DUMP: std::sync::OnceLock<Mutex<Option<std::fs::File>>> = std::sync::OnceLock::new();
    let cell =
        DUMP.get_or_init(|| Mutex::new(std::fs::File::create("tts_dump.raw").ok()));
    if let Ok(mut g) = cell.lock() {
        if let Some(f) = g.as_mut() {
            let mut bytes = Vec::with_capacity(samples.len() * 4);
            for v in samples {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            let _ = f.write_all(&bytes);
        }
    }
}

/* Utterance reader: polls `.tts-utt/` for new timestamp-named WAVs
 * (chronological name order = utterance order), waits for each file to
 * be complete, converts it into the ring, deletes it. Exits when the
 * child is gone; sets `dead` and drops `pending`. */
fn utterance_loop(inner: Arc<Inner>, dst_rate: u32) {
    let dir = Path::new(OUT_DIR);
    let mut last_done: u64 = 0;
    let mut slot_idx: usize = 0;
    let mut res: Option<Resampler> = None;

    'outer: loop {
        if child_gone(&inner) {
            break 'outer;
        }
        /* collect utterances not consumed yet, oldest first */
        let mut names: Vec<u64> = Vec::new();
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let path = e.path();
                if !path.extension().map_or(false, |x| x == "wav") {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    if let Ok(id) = stem.parse::<u64>() {
                        if id > last_done {
                            names.push(id);
                        }
                    }
                }
            }
        }
        names.sort_unstable();
        let any = !names.is_empty();
        for id in names {
            let mut path = dir.join(format!("{id}.wav"));
            /* wait until the file parses as a complete WAV: piper
             * writes the header first and patches the sizes after the
             * last sample, so an in-progress file does not parse yet */
            let deadline = Instant::now() + WAIT_FILE_MAX;
            let mut done: Option<Vec<u8>> = None;
            while done.is_none() {
                if child_gone(&inner) {
                    break 'outer;
                }
                if Instant::now() >= deadline {
                    break;
                }
                if let Ok(b) = fs::read(&path) {
                    let complete = parse_wav(&b).map_or(false, |(_, _, _, d)| !d.is_empty());
                    if complete {
                        done = Some(b);
                    }
                }
                if done.is_none() {
                    std::thread::sleep(POLL);
                }
            }
            match done {
                Some(b) => {
                    if let Some((rate, channels, bits, data)) = parse_wav(&b) {
                        if inner.debug {
                            eprintln!(
                                "[tts] +{:>5} ms utterance {id}: rate={rate} ch={channels} bits={bits} bytes={}",
                                inner.t0.elapsed().as_millis(),
                                data.len()
                            );
                        }
                        /* Park the finished utterance in the next fixed
                         * slot (.tts-utt/tts_N.wav, round-robin) and
                         * clear the other slots while this one is being
                         * consumed, so the folder never accumulates
                         * files and disk use stays bounded. Under
                         * MOON_TTS_DEBUG the kept files rotate through
                         * the same slots instead of piling up. */
                        let slot = dir.join(format!("tts_{}.wav", (slot_idx % UTT_SLOTS) + 1));
                        slot_idx = (slot_idx + 1) % UTT_SLOTS;
                        if fs::rename(&path, &slot).is_ok() {
                            for s in 1..=UTT_SLOTS {
                                let other = dir.join(format!("tts_{s}.wav"));
                                if other != slot {
                                    let _ = fs::remove_file(&other);
                                }
                            }
                            path = slot;
                        }
                        push_pcm(&inner, &mut res, dst_rate, rate, channels, bits, data);
                        *inner.last_byte.lock().unwrap() = Some(Instant::now());
                    }
                }
                None => {
                    if inner.debug {
                        eprintln!("[tts] utterance {id}: gave up waiting for file");
                    }
                }
            }
            dec_pending(&inner);
            push_silence(&inner, dst_rate, SENTENCE_PAUSE);
            /* Consumed -> remove (non-debug). MOON_TTS_DEBUG keeps the
             * slot file for offline inspection, but the rotation reuses
             * the same slot names so kept files stay bounded too. */
            if !inner.debug {
                let _ = fs::remove_file(&path);
            }
            last_done = id;
        }
        if !any {
            std::thread::sleep(POLL);
        }
    }

    /* child gone: finalize */
    if inner.debug {
        eprintln!("[tts] reader exiting (child gone)");
    }
    inner.pending.store(0, Ordering::SeqCst);
    inner.dead.store(true, Ordering::SeqCst);
    /* wake any backpressured push so it sees `dead` and bails */
    inner.ring_cv.notify_all();
}

/* ================================================================ */
/* linear resampler                                                 */
/* ================================================================ */

/* Streaming linear resampler: src-rate f32 in, dst-rate f32 out. */
struct Resampler {
    step: f64, /* src samples per dst sample */
    pos: f64,  /* src-domain position of the next output sample */
    last: f32,
    base: f64, /* absolute src index of the sample after `last` */
    rate: u32,
}

impl Resampler {
    fn new(src_rate: u32, dst_rate: u32) -> Self {
        Self {
            step: src_rate as f64 / dst_rate as f64,
            pos: 0.0,
            last: 0.0,
            base: 0.0,
            rate: src_rate,
        }
    }

    fn push(&mut self, src: &[f32], out: &mut Vec<f32>) {
        for (i, &cur) in src.iter().enumerate() {
            let m = self.base + i as f64;
            while self.pos <= m {
                /* interpolate between the previous src sample (at m-1)
                 * and this one (at m) */
                let t = (self.pos - (m - 1.0)) as f32;
                out.push(self.last + (cur - self.last) * t);
                self.pos += self.step;
            }
            self.last = cur;
        }
        self.base += src.len() as f64;
    }
}

/* ================================================================ */
/* cpal playback                                                    */
/* ================================================================ */

/* convert a ring f32 sample (-1..1) to the device's sample format */
trait PlaySample: cpal::SizedSample {
    fn from_f32(v: f32) -> Self;
}
impl PlaySample for f32 {
    fn from_f32(v: f32) -> Self {
        v
    }
}
impl PlaySample for f64 {
    fn from_f32(v: f32) -> Self {
        v as f64
    }
}
impl PlaySample for i16 {
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) * 32768.0) as i16
    }
}
impl PlaySample for u16 {
    fn from_f32(v: f32) -> Self {
        ((v.clamp(-1.0, 1.0) + 1.0) * 32767.5) as u16
    }
}
impl PlaySample for i8 {
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) * 127.0) as i8
    }
}
impl PlaySample for u8 {
    fn from_f32(v: f32) -> Self {
        ((v.clamp(-1.0, 1.0) + 1.0) * 127.5) as u8
    }
}
impl PlaySample for i32 {
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) * 2147483647.0) as i32
    }
}

/* Mono f32 ring -> the device's channels/format. Pops silence on
 * underrun, so the callback never blocks. */
fn output_stream<T: PlaySample>(
    device: &cpal::Device,
    config: &cpal::SupportedStreamConfig,
    ring: Arc<Mutex<VecDeque<f32>>>,
    ring_cv: Arc<Condvar>,
) -> Result<cpal::Stream, Box<dyn std::error::Error>> {
    let channels = (config.channels() as usize).max(1);
    let stream_config: cpal::StreamConfig = (*config).into();
    let stream = device.build_output_stream::<T, _, _>(
        stream_config,
        move |data: &mut [T], _info: &cpal::OutputCallbackInfo| {
            let mut g = match ring.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            for frame in data.chunks_mut(channels) {
                let v = g.pop_front().unwrap_or(0.0);
                for ch in frame {
                    *ch = T::from_f32(v);
                }
            }
            /* release the reader if it is waiting on backpressure */
            ring_cv.notify_all();
        },
        move |err| {
            eprintln!("playback error: {err}");
        },
        None,
    )?;
    Ok(stream)
}

/* ================================================================ */
/* speaker                                                          */
/* ================================================================ */

impl Speaker {
    /* Same as `start_with` for the default voice. */
    pub fn start() -> Result<Speaker, Box<dyn std::error::Error>> {
        Self::start_with(DEFAULT_VOICE)
    }

    /* Probe the output device, make sure the voice is on disk (downloads
     * once) and start the piper child + reader + playback threads.
     * `voice_id` looks like `en_US-amy-low`, `en_US-amy-medium` or
     * `en_US-libritts_r-medium#051`. */
    pub fn start_with(voice_id: &str) -> Result<Speaker, Box<dyn std::error::Error>> {
        let voice = VoiceSpec::ensure(voice_id)?;
        if !Path::new(PIPER_EXE).exists() {
            return Err(format!(
                "piper runtime not found at {PIPER_EXE} (expected piper.exe + espeak-ng-data)"
            )
            .into());
        }
        if !Path::new(ESPEAK_DATA).is_dir() {
            return Err(format!("espeak-ng data not found at {ESPEAK_DATA}").into());
        }

        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("no default output device found")?;
        let config = device.default_output_config()?;

        let inner = Arc::new(Inner {
            voice,
            dst_rate: config.sample_rate(),
            stdin: Mutex::new(None),
            child: Mutex::new(None),
            ring: Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP))),
            ring_cv: Arc::new(Condvar::new()),
            pending: AtomicI64::new(0),
            dead: AtomicBool::new(true),
            last_byte: Mutex::new(None),
            debug: std::env::var("MOON_TTS_DEBUG").is_ok(),
            t0: Instant::now(),
        });

        let ring = Arc::clone(&inner.ring);
        let ring_cv = Arc::clone(&inner.ring_cv);
        let stream = match config.sample_format() {
            cpal::SampleFormat::F32 => output_stream::<f32>(&device, &config, ring, ring_cv)?,
            cpal::SampleFormat::I16 => output_stream::<i16>(&device, &config, ring, ring_cv)?,
            cpal::SampleFormat::U16 => output_stream::<u16>(&device, &config, ring, ring_cv)?,
            cpal::SampleFormat::U8 => output_stream::<u8>(&device, &config, ring, ring_cv)?,
            cpal::SampleFormat::I8 => output_stream::<i8>(&device, &config, ring, ring_cv)?,
            cpal::SampleFormat::I32 => output_stream::<i32>(&device, &config, ring, ring_cv)?,
            cpal::SampleFormat::F64 => output_stream::<f64>(&device, &config, ring, ring_cv)?,
            other => return Err(format!("playback sample format {other} is not supported").into()),
        };
        stream.play()?;

        spawn_child(&inner)?;

        println!(
            "  tts       : piper voice {} ({} speaker{}) @ {} Hz (device {} Hz, {} ch, {:?})",
            inner.voice.id,
            inner.voice.num_speakers,
            if inner.voice.num_speakers > 1 { "s" } else { "" },
            inner.voice.sample_rate,
            config.sample_rate(),
            config.channels(),
            config.sample_format()
        );

        Ok(Speaker {
            inner,
            _stream: Some(stream),
        })
    }

    /* Queue one utterance. Non-blocking: synthesis and playback happen on
     * the reader/playback threads while the caller keeps streaming. */
    pub fn speak(&self, text: &str) -> Result<(), Box<dyn std::error::Error>> {
        /* one line per utterance: CR/LF inside the text would end it */
        let mut line = String::with_capacity(text.len());
        for c in text.chars() {
            match c {
                '\n' | '\r' => line.push(' '),
                c if (c as u32) < 32 => {}
                c => line.push(c),
            }
        }
        let line = line.trim();
        if line.is_empty() {
            return Ok(());
        }
        if self.inner.dead.load(Ordering::SeqCst) {
            self.respawn()?;
        }
        let mut g = match self.inner.stdin.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        match g.as_mut() {
            Some(stdin) => {
                let r = (|| -> std::io::Result<()> {
                    stdin.write_all(line.as_bytes())?;
                    stdin.write_all(b"\n")?;
                    stdin.flush()
                })();
                match r {
                    Ok(()) => {
                        let n = self.inner.pending.fetch_add(1, Ordering::SeqCst) + 1;
                        if self.inner.debug {
                            eprintln!(
                                "[tts] +{:>5} ms speak queued (pending now {n})",
                                self.inner.t0.elapsed().as_millis()
                            );
                        }
                        Ok(())
                    }
                    /* the child died mid-write; respawn on the next call */
                    Err(e) => {
                        self.inner.dead.store(true, Ordering::SeqCst);
                        Err(format!("piper stdin write failed: {e}").into())
                    }
                }
            }
            None => Err("piper stdin is closed".into()),
        }
    }

    /* Barge-in: kill the synthesizer and discard everything queued; the
     * next speak() respawns it. */
    pub fn stop(&self) {
        if let Some(mut c) = self.inner.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        *self.inner.stdin.lock().unwrap() = None;
        self.inner.ring.lock().unwrap().clear();
        self.inner.ring_cv.notify_all();
        self.inner.pending.store(0, Ordering::SeqCst);
        self.inner.dead.store(true, Ordering::SeqCst);
        /* drop any unconsumed utterance files so the respawned reader
         * never replays stale audio */
        if let Ok(entries) = fs::read_dir(Path::new(OUT_DIR)) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().map_or(false, |x| x == "wav") {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }

    /* True while audio is queued/playing, an utterance is still being
     * synthesized, or a file was consumed in the last ~0.4 s. Used by
     * assist to mute its own mic input. */
    pub fn busy(&self) -> bool {
        if self.inner.pending.load(Ordering::SeqCst) > 0 {
            return true;
        }
        if !self.inner.ring.lock().unwrap().is_empty() {
            return true;
        }
        let recent = *self.inner.last_byte.lock().unwrap();
        self.inner.child.lock().unwrap().is_some()
            && recent.is_some_and(|t| t.elapsed() < Duration::from_millis(400))
    }

    /* Block until the voice finishes (or `max` elapses). */
    pub fn wait_drain(&self, max: Duration) {
        let start = Instant::now();
        while self.busy() && start.elapsed() < max {
            std::thread::sleep(Duration::from_millis(20));
        }
        if self.inner.debug {
            if self.busy() {
                eprintln!(
                    "[tts] wait_drain TIMED OUT after {max:?}: pending={} ring={}",
                    self.inner.pending.load(Ordering::SeqCst),
                    self.inner.ring.lock().unwrap().len()
                );
            } else {
                eprintln!("[tts] drained in {:?}", start.elapsed());
            }
        }
    }

    /* Kill + replace the piper child (after a barge-in or a crash). The
     * device rate is fixed at start and kept in `inner`, so nothing
     * needs re-probing here. */
    fn respawn(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.inner.ring.lock().unwrap().clear();
        self.inner.ring_cv.notify_all();
        self.inner.pending.store(0, Ordering::SeqCst);
        spawn_child(&self.inner)
    }
}

/* ================================================================ */
/* helpers used by the reply stream                                 */
/* ================================================================ */

/* Strip markdown leftovers and control chars so piper reads clean
 * prose. */
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = true;
    for c in s.chars() {
        match c {
            '*' | '_' | '#' | '`' | '~' | '\\' => {}
            '\n' | '\r' | '\t' | ' ' => {
                if !last_space {
                    out.push(' ');
                    last_space = true;
                }
            }
            c if (c as u32) < 32 => {}
            c => {
                out.push(c);
                last_space = false;
            }
        }
    }
    out.trim().to_string()
}

/* Pull the next complete sentence out of the streaming reply buffer.
 * Cuts after . ! ? : (3+ chars, digits around '.' protected), or at the
 * last space once the buffer exceeds 200 chars so a run-on stream
 * cannot grow unbounded. Returns None while incomplete. */
pub fn take_sentence(buf: &mut String) -> Option<String> {
    if buf.chars().count() > 200 {
        if let Some(pos) = buf.rfind(' ') {
            let s: String = buf.drain(..pos).collect();
            let s = sanitize(&s);
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    /* Scan bytes: the terminators are ASCII, so byte indices land on
     * real char boundaries in UTF-8. */
    let bytes = buf.as_bytes();
    let mut cut = None;
    for i in 0..bytes.len() {
        let c = bytes[i];
        if matches!(c, b'.' | b'!' | b'?' | b':') && i >= 2 {
            if c == b'.'
                && bytes[i - 1].is_ascii_digit()
                && bytes.get(i + 1).is_some_and(|b| b.is_ascii_digit())
            {
                continue; /* 3.14 is not a sentence end */
            }
            cut = Some(i + 1); /* +1 includes the terminator itself */
            break;
        }
    }
    if let Some(end) = cut {
        let s: String = buf.drain(..end).collect();
        let s = sanitize(&s);
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

/* ================================================================ */
/* speak subcommand                                                 */
/* ================================================================ */

/// `speak "some text"`: synthesize + play a phrase, wait for the audio,
/// exit. Exercises the whole TTS chain without the STT/LLM stack.
pub fn cmd_speak(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut voice: Option<&str> = None;
    let mut words: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--voice" => {
                i += 1;
                voice = Some(
                    args.get(i)
                        .ok_or("option --voice needs a voice id (e.g. en_US-amy-low)")?,
                );
            }
            opt if opt.starts_with("--") => {
                return Err(format!(
                    "unknown option '{opt}' (usage: speak \"TEXT\" [--voice ID])"
                )
                .into());
            }
            w => words.push(w),
        }
        i += 1;
    }
    let text = words.join(" ");
    if text.trim().is_empty() {
        eprintln!("Usage: moonshine-test speak \"TEXT\" [--voice ID]");
        eprintln!();
        eprintln!("  Synthesizes TEXT through a piper voice from {VOICE_DIR}");
        eprintln!("  (auto-downloaded on first run) and plays it.");
        eprintln!("  Voices: en_US-libritts_r-medium#051 (default; '#NNN' = speaker index");
        eprintln!("  inside a multi-speaker model), en_US-amy-medium, en_US-amy-low.");
        std::process::exit(2);
    }
    crate::live::install_console();
    let speaker = match voice {
        Some(v) => Speaker::start_with(v)?,
        None => Speaker::start()?,
    };
    println!("speaking: {text}");
    speaker.speak(&text)?;
    speaker.wait_drain(Duration::from_secs(120));
    Ok(())
}
