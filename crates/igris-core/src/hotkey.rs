//! `igris-ai hotkey [--model DIR]`
//!
//! Push-to-talk driven by a global keyboard hook: hold CTRL+SPACE on any
//! application — audio is captured and fed to Moonshine. Release the keys
//! and ONLY the audio captured during the hold is finalized and printed as
//! transcript text. The console does not need focus; the hook is global.
//! The program keeps running for the next hold. Ctrl+C quits.
//!
//! Key handling (WH_KEYBOARD_LL):
//! - While Ctrl is down, Space key events are swallowed so pressing
//!   CTRL+SPACE does not type a stray space into the focused app.
//! - Only **physical** Ctrl+Space events can drive recording and key
//!   suppression. Injected events (SendInput / AutoHotkey / AHK) update no
//!   state and are never re-eaten. This keeps the privacy boundary physical.
//! - Left and right Ctrl are tracked independently so one Ctrl key can be
//!   released without ending the hold.
//!
//! Dev/test escape hatch: set `MOON_FAKE_HOTKEY=1` and the hotkey state is
//! driven by stdin lines instead of real keys — "p" = press, "r" = release.
//! Fake mode does NOT install the real global hook.
//!
//! The hotkey plumbing (hook guard, pressed-state, fake driver) is also
//! reused by `assist --hotkey`, which gates its microphone on the same
//! Ctrl+Space combo instead of recording continuously.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use moonshine_rs::{ModelArch, Transcriber};

use crate::live::{LiveRow, MicSession, STOP, install_console, open_mic, tail_chars};

/* ================================================================ */
/* hotkey state (updated by the keyboard hook / fakes)              */
/* ================================================================ */

pub(crate) struct CtrlState {
    down_l: bool,
    down_r: bool,
    generic: bool,
}

impl CtrlState {
    fn any(&self) -> bool {
        self.down_l || self.down_r || self.generic
    }
    fn on_key(&mut self, vk: u32, down: bool) {
        match vk {
            VK_LCONTROL => self.down_l = down,
            VK_RCONTROL => self.down_r = down,
            VK_CONTROL => self.generic = down,
            _ => {}
        }
    }
}

/* Space is also a physical-only state variable (injected Space events do not
 * affect whether the combo is active); shared with the swallow logic below. */
static SPACE_DOWN: AtomicBool = AtomicBool::new(false);
static SPACE_DOWN_EATEN: AtomicBool = AtomicBool::new(false);

pub(crate) fn hotkey_active(ctrl: &CtrlState) -> bool {
    ctrl.any() && SPACE_DOWN.load(Ordering::SeqCst)
}

fn should_eat_space_event(eaten: &AtomicBool, down: bool, ctrl_held: bool) -> bool {
    if down {
        if ctrl_held {
            eaten.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    } else {
        eaten.swap(false, Ordering::SeqCst)
    }
}

/* ================================================================ */
/* keyboard hook (global, Windows)                                  */
/* ================================================================ */

const WH_KEYBOARD_LL: i32 = 13;
const VK_SPACE: u32 = 0x20;
const VK_LCONTROL: u32 = 0xA2;
const VK_RCONTROL: u32 = 0xA3;
const VK_CONTROL: u32 = 0x11; /* generic control */
const LLKHF_UP: u32 = 0x80; /* bit set = key released */
const LLKHF_INJECTED: u32 = 0x10;

#[repr(C)]
struct KbdLlHookStruct {
    vk_code: u32,
    scan_code: u32,
    flags: u32,
    time: u32,
    extra_info: usize,
}

unsafe extern "system" fn hook_proc(code: i32, wparam: usize, lparam: isize) -> isize {
    /* Safety: WH_KEYBOARD_LL calls us with a pointer to KbdLlHookStruct that
     * matches this layout (MS documentation). We never mut statics or return
     * outside the documented range except via CallNextHookEx. */
    if code >= 0 && lparam != 0 {
        let st = lparam as *const KbdLlHookStruct;
        let vk = unsafe { (*st).vk_code };
        let flags = unsafe { (*st).flags };
        let down = flags & LLKHF_UP == 0;
        let injected = flags & LLKHF_INJECTED != 0;

        /* Injected events are purely informational: they never change the
         * PTT state and never suppress keys. Physical-only privacy boundary. */
        let is_space = vk == VK_SPACE;
        let ctrl_held = if !injected {
            let mut ctrl = match CTRL_STATE.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            ctrl.on_key(vk, down);
            if is_space {
                SPACE_DOWN.store(down, Ordering::SeqCst);
            }
            ctrl.any()
        } else {
            false
        };

        if is_space && !injected && should_eat_space_event(&SPACE_DOWN_EATEN, down, ctrl_held) {
            return 1;
        }
    }
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

#[repr(C)]
struct Msg {
    hwnd: *mut core::ffi::c_void,
    message: u32,
    wparam: usize,
    lparam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

const PM_NOREMOVE: u32 = 0;
const WM_QUIT: u32 = 0x0012;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
    fn GetModuleHandleW(module: *const u16) -> *mut core::ffi::c_void;
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetWindowsHookExW(
        id_hook: i32,
        hook_proc: Option<unsafe extern "system" fn(i32, usize, isize) -> isize>,
        module: *mut core::ffi::c_void,
        thread: u32,
    ) -> *mut core::ffi::c_void;
    fn UnhookWindowsHookEx(hook: *mut core::ffi::c_void) -> i32;
    fn PeekMessageW(
        msg: *mut Msg,
        hwnd: *mut core::ffi::c_void,
        min: u32,
        max: u32,
        remove: u32,
    ) -> i32;
    fn GetMessageW(msg: *mut Msg, hwnd: *mut core::ffi::c_void, min: u32, max: u32) -> i32;
    fn TranslateMessage(msg: *const Msg) -> i32;
    fn DispatchMessageW(msg: *const Msg) -> isize;
    fn PostThreadMessageW(thread: u32, message: u32, wparam: usize, lparam: isize) -> i32;
    fn CallNextHookEx(
        hook: *mut core::ffi::c_void,
        code: i32,
        wparam: usize,
        lparam: isize,
    ) -> isize;
}

pub(crate) static CTRL_STATE: std::sync::Mutex<CtrlState> = std::sync::Mutex::new(CtrlState {
    down_l: false,
    down_r: false,
    generic: false,
});

/* A managed hook: installs with validation, retains the handle, has a stop
 * event, joins the message loop cleanly, and unhooks. Fail-fast: the caller
 * must open the mic only after this succeeds. */
pub(crate) struct HookGuard {
    thread_id: u32,
    thread: Option<thread::JoinHandle<()>>,
}

impl HookGuard {
    pub(crate) fn try_install() -> io::Result<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = thread::Builder::new()
            .name("moonshine-keyboard-hook".to_string())
            .spawn(move || unsafe {
                /* A low-level hook is installed and serviced by the same
                 * thread. Its message queue must exist before installation. */
                let thread_id = GetCurrentThreadId();
                let mut msg = Msg {
                    hwnd: std::ptr::null_mut(),
                    message: 0,
                    wparam: 0,
                    lparam: 0,
                    time: 0,
                    pt_x: 0,
                    pt_y: 0,
                };
                let _ = PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);

                let module = GetModuleHandleW(std::ptr::null());
                if module.is_null() {
                    let _ =
                        ready_tx.send(Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1)));
                    return;
                }
                let callback: unsafe extern "system" fn(i32, usize, isize) -> isize = hook_proc;
                let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(callback), module, 0);
                if hook.is_null() {
                    let _ =
                        ready_tx.send(Err(io::Error::last_os_error().raw_os_error().unwrap_or(-1)));
                    return;
                }
                if ready_tx.send(Ok(thread_id)).is_err() {
                    let _ = UnhookWindowsHookEx(hook);
                    return;
                }

                loop {
                    let got = GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0);
                    if got <= 0 {
                        break;
                    }
                    let _ = TranslateMessage(&msg);
                    let _ = DispatchMessageW(&msg);
                }
                let _ = UnhookWindowsHookEx(hook);
            })
            .map_err(|e| io::Error::other(e.to_string()))?;

        let thread_id = match ready_rx.recv() {
            Ok(Ok(id)) => id,
            Ok(Err(code)) => {
                let _ = thread.join();
                return Err(io::Error::from_raw_os_error(code));
            }
            Err(e) => {
                let _ = thread.join();
                return Err(io::Error::other(e.to_string()));
            }
        };

        Ok(Self {
            thread_id,
            thread: Some(thread),
        })
    }
}

impl Drop for HookGuard {
    fn drop(&mut self) {
        /* WM_QUIT wakes GetMessageW and lets the hook thread unhook itself. */
        let posted = unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) } != 0;
        if !posted {
            eprintln!(
                "warning: failed to stop keyboard hook thread: {}",
                io::Error::last_os_error()
            );
        }
        if let Some(thread) = self.thread.take()
            && posted
        {
            let _ = thread.join();
        }
    }
}

/* ================================================================ */
/* dev-only: drive the hotkey state over stdin ("p"/"r" lines)      */
/* ================================================================ */

pub(crate) static FAKE_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(crate) fn maybe_spawn_fake(fake: bool) {
    if !fake {
        return;
    }
    println!("  fake hotkey : type p + ENTER = press, r + ENTER = release");
    std::thread::spawn(|| {
        let stdin = io::stdin();
        loop {
            let mut line = String::new();
            if stdin.read_line(&mut line).unwrap_or(0) == 0 {
                /* EOF: end the fake session */
                crate::live::STOP.store(true, Ordering::SeqCst);
                return;
            }
            match line.trim().chars().next() {
                Some('p') => FAKE_ACTIVE.store(true, Ordering::SeqCst),
                _ => FAKE_ACTIVE.store(false, Ordering::SeqCst),
            }
        }
    });
}

/* ================================================================ */
/* per-hold state                                                   */
/* ================================================================ */

struct Hold {
    t0: Instant,
    committed: usize,
    pushed_sec: f64,
    analyzed_sec: f64,
    speed: f64, /* ms CPU per 1 s audio (EMA) */
    latency: f64,
    partial: String,
}

impl Hold {
    fn new() -> Self {
        Self {
            t0: Instant::now(),
            committed: 0,
            pushed_sec: 0.0,
            analyzed_sec: 0.0,
            speed: 0.0,
            latency: 0.0,
            partial: String::new(),
        }
    }

    /* commit completed lines, show the in-flight tail on the status row */
    fn pass(&mut self, tr: moonshine_rs::Transcript, row: &mut LiveRow) {
        /* speed accounting rides along with the forced poll */
        while self.committed < tr.lines.len() {
            let line = &tr.lines[self.committed];
            let is_last = self.committed + 1 == tr.lines.len();
            if is_last && !line.is_complete {
                /* still being spoken: tail goes to the status row */
                if line.last_transcription_latency_ms > 0 {
                    let l = line.last_transcription_latency_ms as f64;
                    self.latency = if self.latency > 1.0 {
                        0.5 * l + 0.5 * self.latency
                    } else {
                        l
                    };
                }
                self.partial = tail_chars(&line.text, 40);
                return;
            }
            row.clear();
            println!(
                "[line {} | {:.2}s speech | decode {}ms] {}",
                line.id % 100_000_000,
                line.duration,
                line.last_transcription_latency_ms,
                if line.text.is_empty() {
                    "(no text)"
                } else {
                    &line.text
                }
            );
            let _ = io::stdout().flush();
            self.committed += 1;
        }
        self.partial.clear();
    }

    fn status(&self, row: &mut LiveRow) {
        let shown = tail_chars(&self.partial, 40);
        let pad = 40usize.saturating_sub(shown.chars().count());
        let in_prog = if self.partial.is_empty() {
            String::new()
        } else {
            format!("\"{}\"{} ", shown, " ".repeat(pad))
        };
        let xrt = if self.speed > 1.0 {
            1000.0 / self.speed
        } else {
            0.0
        };
        row.show(&format!(
            "[{:.1}s REC] {} backlog {:4.2}s | {:4.1}x",
            self.t0.elapsed().as_secs_f64(),
            in_prog,
            (self.pushed_sec - self.analyzed_sec).max(0.0),
            xrt
        ));
    }
}

/* ================================================================ */
/* command                                                          */
/* ================================================================ */

pub fn cmd_hotkey(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut model_dir = crate::paths::asset("stt/small-streaming").to_string_lossy().into_owned();
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
                return Err(format!("unknown option '{opt}' (usage: hotkey [--model DIR])").into());
            }
            _ => return Err("hotkey takes no positional arguments".into()),
        }
        i += 1;
    }

    let arch = if model_dir.to_lowercase().contains("medium") {
        ModelArch::MediumStreaming
    } else {
        ModelArch::SmallStreaming
    };

    install_console();

    println!("Moonshine push-to-talk (hold CTRL+SPACE anywhere)");
    println!("  model      : {model_dir}");
    println!("  record     : hold CTRL+SPACE, speak, release to transcribe");
    println!("  repeat     : hold again for the next segment; Ctrl+C quits");
    println!();
    let _ = io::stdout().flush();

    let model_dir = crate::models::ensure_model(&model_dir, arch)?;
    let transcriber = Transcriber::from_files(&model_dir, arch, None)?;
    println!("  models    : loaded");
    let _ = io::stdout().flush();
    let fake = std::env::var("MOON_FAKE_HOTKEY").is_ok();

    /* Install the global hook BEFORE opening the mic. If installation
     * fails, fail closed: no mic, no transcription, a real diagnostic. */
    let _hook = if fake {
        println!("  hook       : skipped (MOON_FAKE_HOTKEY=1)");
        None
    } else {
        let hook = HookGuard::try_install().map_err(|e| {
            format!(
                "failed to install global keyboard hook (code {}): {}",
                e.raw_os_error().unwrap_or(-1),
                e
            )
        })?;
        println!("  hook       : installed, waiting for CTRL+SPACE");
        Some(hook)
    };
    println!();
    let _ = io::stdout().flush();

    let m: MicSession = open_mic()?;
    /* Hotkey mode captures only during an active hold. */
    m.ring.set_enabled(false);
    println!(
        "  mic        : native {} Hz — waiting for the hotkey",
        m.rate
    );
    println!();
    println!("HOLD CTRL+SPACE to record. Release to transcribe. Ctrl+C to quit.");
    println!("[hotkey] ready — waiting for the next hold");
    println!();
    let _ = io::stdout().flush();

    maybe_spawn_fake(fake);

    /* capture runs from now on; while idle the ring is drained and dropped */
    let mut prev_hotkey = false;
    let mut row = LiveRow::new();
    let mut hold: Option<(_, Hold)> = None;
    let mut hold_error: Option<String> = None;
    let mut last_pass: Instant = Instant::now();
    let mut chunk: Vec<f32> = Vec::with_capacity(6400);
    let mut junk: Vec<f32> = Vec::with_capacity(6400);

    loop {
        if STOP.load(Ordering::SeqCst) {
            break;
        }

        let active = if fake {
            FAKE_ACTIVE.load(Ordering::SeqCst)
        } else {
            /* guard is poisoned only if the hook thread panicked; fall back
             * to a fresh controller so the tool keeps working */
            let ctrl = match CTRL_STATE.lock() {
                Ok(g) => g,
                Err(e) => e.into_inner(),
            };
            hotkey_active(&ctrl)
        };

        /* ---------- hold starts ---------- */
        if active && !prev_hotkey {
            m.ring.set_enabled(false);
            while m.ring.drain(6400, &mut junk) {
                junk.clear();
            }
            let stream = match transcriber.create_stream() {
                Ok(s) => s,
                Err(e) => {
                    hold_error = Some(format!("create stream: {e}"));
                    eprintln!("  [hold failed] {}", hold_error.as_deref().unwrap());
                    prev_hotkey = active;
                    std::thread::sleep(Duration::from_millis(15));
                    continue;
                }
            };
            hold_error = None;
            m.ring.set_enabled(true);
            hold = Some((stream, Hold::new()));
            last_pass = Instant::now();
            row.clear();
            println!("[hotkey] recording... (release to transcribe)");
            let _ = io::stdout().flush();
        }

        /* ---------- while holding: feed + analyze ---------- */
        let mut hold_failed = false;
        if active && let Some((stream, h)) = &mut hold {
            while m.ring.drain(6400, &mut chunk) {
                if chunk.is_empty() {
                    continue;
                }
                if let Err(e) = stream.add_audio(&chunk, m.rate) {
                    hold_error = Some(format!("add_audio: {e}"));
                    eprintln!("  [hold failed] {}", hold_error.as_deref().unwrap());
                    chunk.clear();
                    hold_failed = true;
                    break;
                }
                h.pushed_sec += chunk.len() as f64 / m.rate as f64;
                chunk.clear();
            }
            /* forced decode pass every 250 ms, like the live command */
            if !hold_failed && last_pass.elapsed() >= Duration::from_millis(250) {
                let pass_t0 = Instant::now();
                let tr = match stream.poll(true) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        hold_error = Some(format!("poll: {e}"));
                        eprintln!("  [hold failed] {}", hold_error.as_deref().unwrap());
                        hold_failed = true;
                        None
                    }
                };
                if let Some(tr) = tr {
                    let dt = pass_t0.elapsed().as_secs_f64() * 1000.0;
                    let new_audio = (h.pushed_sec - h.analyzed_sec).max(0.0);
                    if new_audio > 0.02 && dt > 0.1 {
                        let inst = dt / new_audio;
                        h.speed = if h.speed > 1.0 {
                            0.55 * inst + 0.45 * h.speed
                        } else {
                            inst
                        };
                    }
                    h.analyzed_sec = h.pushed_sec;
                    h.pass(tr, &mut row);
                    last_pass = Instant::now();
                }
            }
            if !hold_failed {
                h.status(&mut row);
            }
        }
        if hold_failed {
            hold = None;
            m.ring.set_enabled(false);
        }

        /* Do not retain microphone audio while the hotkey is idle. */
        if !active && !prev_hotkey {
            while m.ring.drain(6400, &mut junk) {
                junk.clear();
            }
        }

        /* ---------- hold ends: finalize ONLY this hold's audio ---------- */
        if !active && prev_hotkey {
            m.ring.set_enabled(false);
            row.clear();
            println!("[hotkey] released — capture stopped; transcribing segment...");
            let _ = io::stdout().flush();
            let mut emitted_text = false;
            if let Some((mut stream, mut h)) = hold.take() {
                emitted_text = h.committed > 0;
                let mut failed = false;
                /* push whatever is still buffered, then decode it all */
                while m.ring.drain(6400, &mut chunk) {
                    if !chunk.is_empty() {
                        if let Err(e) = stream.add_audio(&chunk, m.rate) {
                            hold_error = Some(format!("add_audio during release: {e}"));
                            failed = true;
                        } else {
                            h.pushed_sec += chunk.len() as f64 / m.rate as f64;
                        }
                    }
                    chunk.clear();
                    if failed {
                        break;
                    }
                }
                if !failed {
                    match stream.finalize() {
                        Ok(tr) => {
                            for line in tr.lines.iter().skip(h.committed) {
                                if line.text.is_empty() {
                                    continue;
                                }
                                emitted_text = true;
                                println!(
                                    "[line {} | {:.2}s speech | decode {}ms] {}",
                                    line.id % 100_000_000,
                                    line.duration,
                                    line.last_transcription_latency_ms,
                                    line.text
                                );
                            }
                        }
                        Err(e) => {
                            hold_error = Some(format!("finalize stream: {e}"));
                        }
                    }
                }
            }
            if !emitted_text {
                if let Some(error) = hold_error.take() {
                    println!("(hold discarded: {error})");
                } else {
                    println!("(nothing recognized from that hold)");
                }
            }
            println!();
            println!("[hotkey] ready for the next hold");
            let _ = io::stdout().flush();
        }

        prev_hotkey = active;
        std::thread::sleep(Duration::from_millis(15));
    }

    /* Ctrl-C: stop capture before finalizing an in-flight hold. */
    println!("[hotkey] stopping capture...");
    m.ring.set_enabled(false);
    if let Some((mut stream, h)) = hold.take() {
        while m.ring.drain(6400, &mut chunk) {
            if !chunk.is_empty() {
                let _ = stream.add_audio(&chunk, m.rate);
            }
            chunk.clear();
        }
        if let Ok(tr) = stream.finalize() {
            for line in tr.lines.iter().skip(h.committed) {
                if !line.text.is_empty() {
                    println!("{}", line.text);
                }
            }
        }
    }
    row.clear();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_sides_are_independent() {
        let mut state = CtrlState {
            down_l: false,
            down_r: false,
            generic: false,
        };
        state.on_key(VK_LCONTROL, true);
        state.on_key(VK_RCONTROL, true);
        state.on_key(VK_LCONTROL, false);
        assert!(state.any());
        state.on_key(VK_RCONTROL, false);
        assert!(!state.any());
    }

    #[test]
    fn space_down_and_up_are_eaten_as_a_pair() {
        let eaten = AtomicBool::new(false);
        assert!(!should_eat_space_event(&eaten, true, false));
        assert!(should_eat_space_event(&eaten, true, true));
        assert!(should_eat_space_event(&eaten, false, false));
        assert!(!should_eat_space_event(&eaten, false, false));
    }
}
