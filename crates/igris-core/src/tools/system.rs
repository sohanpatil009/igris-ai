//! System tools: sys_stats (CPU/RAM/disk/battery/uptime), set_volume
//! (COM endpoint volume), screenshot (GDI capture to PNG).

use crate::tools::basic::{extract_i64, shell_open, to_wide};
use crate::tools::shared::expand_home_path;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use windows::core::PCWSTR;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDeviceCaps, HGDIOBJ, HORZRES,
    ReleaseDC, SRCCOPY, SelectObject, VERTRES,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eMultimedia, eRender, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::SystemInformation::{
    GetTickCount64, GlobalMemoryStatusEx, MEMORYSTATUSEX,
};
use windows::Win32::System::Threading::GetSystemTimes;

/* ================================================================ */
/* sys_stats — CPU / RAM / disk / battery / uptime snapshot           */
/* ================================================================ */

pub(crate) fn run_sys_stats(_args: &Value) -> Result<String, String> {
    let cpu = cpu_percent();
    let ram = ram_summary()?;
    let disk = disk_summary()?;
    let power = power_summary()?;
    Ok(format!(
        "CPU: {cpu}% | RAM: {ram} | Disk: {disk} | {power} | Uptime: {}",
        uptime_string()
    ))
}

fn filetime_u64(ft: &FILETIME) -> u64 {
    ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64)
}

/// One (idle, kernel, user) counter snapshot. On Windows the kernel time
/// already includes idle time, so busy = kernel + user - idle.
fn system_times() -> Result<(u64, u64, u64), String> {
    let (mut idle, mut kernel, mut user) =
        (FILETIME::default(), FILETIME::default(), FILETIME::default());
    /* SAFETY: three out-pointers to our own locals. */
    unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }
        .map_err(|e| format!("sys_stats: GetSystemTimes failed: {e}"))?;
    Ok((
        filetime_u64(&idle),
        filetime_u64(&kernel),
        filetime_u64(&user),
    ))
}

/// CPU usage sampled over ~150 ms inside this call (two counter reads).
fn cpu_percent() -> u32 {
    let Ok(t1) = system_times() else {
        return 0;
    };
    std::thread::sleep(Duration::from_millis(150));
    let Ok(t2) = system_times() else {
        return 0;
    };
    /* wrapping_sub: counters never reset while we hold the machine, but a
     * debug-mode panic on u64 underflow is a worse failure than a wrong % */
    let idle = t2.0.wrapping_sub(t1.0);
    let kernel = t2.1.wrapping_sub(t1.1);
    let user = t2.2.wrapping_sub(t1.2);
    let total = kernel + user;
    if total == 0 {
        return 0;
    }
    let busy = total.saturating_sub(idle);
    ((busy as f64) * 100.0 / (total as f64)).round() as u32
}

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn ram_summary() -> Result<String, String> {
    let mut mem = MEMORYSTATUSEX::default();
    mem.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    /* SAFETY: out-pointer to our own struct, dwLength already set. */
    unsafe { GlobalMemoryStatusEx(&mut mem) }
        .map_err(|e| format!("sys_stats: GlobalMemoryStatusEx failed: {e}"))?;
    let used = gib(mem.ullTotalPhys - mem.ullAvailPhys);
    Ok(format!(
        "{} GB used of {} GB ({}%)",
        used,
        gib(mem.ullTotalPhys),
        mem.dwMemoryLoad
    ))
}

fn disk_summary() -> Result<String, String> {
    let drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
    let dir = format!("{drive}\\");
    let wide = to_wide(&dir);
    let (mut free, mut total) = (0u64, 0u64);
    /* SAFETY: NUL-terminated wide string built above; two out u64s. */
    unsafe { GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut free), Some(&mut total), None) }
        .map_err(|e| format!("sys_stats: GetDiskFreeSpaceExW failed: {e}"))?;
    if total == 0 {
        return Ok(format!("{drive} unknown"));
    }
    let used_pct = (((total - free) as f64) * 100.0 / (total as f64)).round() as u32;
    Ok(format!(
        "{drive} {} GB free of {} GB ({used_pct}% used)",
        gib(free),
        gib(total)
    ))
}

fn power_summary() -> Result<String, String> {
    let mut status = SYSTEM_POWER_STATUS::default();
    /* SAFETY: out-pointer to our own struct. */
    unsafe { GetSystemPowerStatus(&mut status) }
        .map_err(|e| format!("sys_stats: GetSystemPowerStatus failed: {e}"))?;
    const BATTERY_FLAG_NO_BATTERY: u8 = 128;
    if status.BatteryFlag & BATTERY_FLAG_NO_BATTERY != 0 {
        return Ok("Battery: none (desktop)".to_string());
    }
    let ac = match status.ACLineStatus {
        0 => "on battery",
        1 => "on AC power",
        _ => "power source unknown",
    };
    if status.BatteryLifePercent == 255 {
        return Ok(format!("Battery: unknown ({ac})"));
    }
    Ok(format!("Battery: {}% ({ac})", status.BatteryLifePercent))
}

fn uptime_string() -> String {
    /* SAFETY: parameterless counter read. */
    let secs = unsafe { GetTickCount64() } / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hours, rem) = (rem / 3600, rem % 3600);
    let minutes = rem / 60;
    if days > 0 {
        format!("{days}d {hours}h {minutes:02}m")
    } else {
        format!("{hours}h {minutes:02}m")
    }
}

/* ================================================================ */
/* set_volume — COM IAudioEndpointVolume absolute set + read-back     */
/* ================================================================ */

pub(crate) fn run_set_volume(args: &Value) -> Result<String, String> {
    let volume = extract_i64(args, "volume", "set_volume")?;
    if !(0..=100).contains(&volume) {
        return Err(format!("volume must be 0-100, got {volume}"));
    }
    let achieved = set_master_volume(volume as u32)?;
    Ok(format!("System volume set to {achieved}%."))
}

/// Set the master output volume through the default audio endpoint and
/// read back the achieved level. Runs on the dispatch worker thread, so
/// COM is initialized for this thread only and balanced below.
fn set_master_volume(percent: u32) -> Result<u32, String> {
    /* SAFETY: thread-local COM initialization; on a fresh dispatch worker
     * this can only return S_OK/S_FALSE, both of which must be balanced
     * by CoUninitialize, which is exactly what happens at the end. */
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if hr.is_err() {
        return Err(format!("set_volume: COM initialization failed: {hr}"));
    }
    let result = (|| -> Result<u32, String> {
        let endpoint = audio_endpoint()?;
        let target = percent as f32 / 100.0;
        /* SAFETY: null event-context GUID; endpoint is alive through the
         * call and dropped by the closure scope afterwards. */
        unsafe { endpoint.SetMasterVolumeLevelScalar(target, std::ptr::null()) }
            .map_err(|e| format!("set_volume: SetMasterVolumeLevelScalar failed: {e}"))?;
        let achieved = unsafe { endpoint.GetMasterVolumeLevelScalar() }
            .map_err(|e| format!("set_volume: GetMasterVolumeLevelScalar failed: {e}"))?;
        Ok((achieved * 100.0).round() as u32)
    })();
    /* SAFETY: balances the successful CoInitializeEx above. */
    unsafe { CoUninitialize() };
    result
}

fn audio_endpoint() -> Result<IAudioEndpointVolume, String> {
    /* SAFETY: CoCreateInstance of the standard MMDeviceEnumerator class. */
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| format!("set_volume: COM instantiation failed: {e}"))?;
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
        .map_err(|e| format!("set_volume: no default audio device: {e}"))?;
    let endpoint: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }
        .map_err(|e| format!("set_volume: endpoint activation failed: {e}"))?;
    Ok(endpoint)
}

/* ================================================================ */
/* screenshot — GDI capture of the primary screen into a PNG          */
/* ================================================================ */

pub(crate) fn run_screenshot(args: &Value) -> Result<String, String> {
    let mut used_default = false;
    let path: PathBuf = match args.get("path") {
        None | Some(Value::Null) => {
            used_default = true;
            default_screenshot_path()?
        }
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                return Err("screenshot: path must not be empty".to_string());
            }
            expand_home_path(s).into()
        }
        Some(_) => return Err("screenshot: 'path' must be a string".to_string()),
    };
    let open = match args.get("open") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Bool(true)) => true,
        Some(_) => return Err("screenshot: 'open' must be a boolean".to_string()),
    };
    if path.is_dir() {
        return Err(format!(
            "screenshot: path points to an existing folder: {}",
            path.display()
        ));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| {
                format!(
                    "screenshot: cannot create folder {}: {e}",
                    parent.display()
                )
            })?;
        }
    }
    let (w, h, rgb) = capture_screen_rgb()?;
    image::save_buffer(&path, &rgb, w, h, image::ColorType::Rgb8)
        .map_err(|e| format!("screenshot: saving {} failed: {e}", path.display()))?;
    let mut msg = format!("Screenshot saved to {} ({}x{}).", path.display(), w, h);
    if used_default {
        /* BUG-007: default-named screenshots used to accumulate without
         * bound (and OneDrive syncs the Pictures folder). Rotate: after
         * saving into the default location, drop the oldest beyond
         * SCREENSHOT_KEEP. Explicit user paths are never pruned. */
        let folder = path.parent().map(Path::to_path_buf).unwrap_or_default();
        let pruned = prune_old_screenshots(&folder, &path);
        if pruned > 0 {
            msg.push_str(&format!(" (pruned {pruned} older screenshot(s).)"));
        }
    }
    if open {
        if let Err(e) = launch_default_viewer(&path) {
            msg.push_str(&format!(" (could not open viewer: {e})"));
        }
    }
    Ok(msg)
}

/// Keep at most this many default-named screenshots in the Pictures
/// folder (BUG-007).
const SCREENSHOT_KEEP: usize = 20;

/// BUG-007 rotation: delete the oldest default-named screenshots
/// (`igris_screenshot_*.png` in `dir`) beyond [`SCREENSHOT_KEEP`]. The
/// stamp inside the default name (YYYY-MM-DD_HH-MM-SS) sorts
/// chronologically, so name order is age order — no mtime races.
/// `protect` (the file just saved) is never deleted even if a backwards
/// clock put it among the oldest. Failures to delete are ignored:
/// pruning is hygiene, and a locked file must not fail the screenshot.
/// Only the default-named prefix is touched — user-chosen paths and
/// unrelated files in the folder are left alone. Returns how many files
/// were deleted.
fn prune_old_screenshots(dir: &Path, protect: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut shots: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            name.starts_with("igris_screenshot_") && name.ends_with(".png")
        })
        .collect();
    let excess = shots.len().saturating_sub(SCREENSHOT_KEEP);
    if excess == 0 {
        return 0;
    }
    shots.sort();
    let mut deleted = 0usize;
    for p in shots.iter().filter(|p| p.as_path() != protect).take(excess) {
        if fs::remove_file(p).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

fn default_screenshot_path() -> Result<PathBuf, String> {
    let base = dirs::picture_dir().unwrap_or_else(std::env::temp_dir);
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    Ok(base.join(format!("igris_screenshot_{stamp}.png")))
}

fn launch_default_viewer(path: &Path) -> Result<(), String> {
    shell_open(&path.as_os_str().to_string_lossy())
}

/// Capture the primary monitor as RGB bytes (top-down, row-major).
fn capture_screen_rgb() -> Result<(u32, u32, Vec<u8>), String> {
    /* SAFETY: standard GDI screen capture. Every handle taken below is
     * released on every path before returning. */
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return Err("screenshot: GetDC failed".to_string());
        }
        let w = GetDeviceCaps(Some(hdc), HORZRES);
        let h = GetDeviceCaps(Some(hdc), VERTRES);
        if w <= 0 || h <= 0 {
            ReleaseDC(None, hdc);
            return Err("screenshot: screen dimensions unavailable".to_string());
        }
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, /* negative: rows come top-down */
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbmp = match CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bmp) => bmp,
            Err(e) => {
                ReleaseDC(None, hdc);
                return Err(format!("screenshot: CreateDIBSection failed: {e}"));
            }
        };
        let mem = CreateCompatibleDC(Some(hdc));
        if mem.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hbmp.0));
            ReleaseDC(None, hdc);
            return Err("screenshot: CreateCompatibleDC failed".to_string());
        }
        let old = SelectObject(mem, HGDIOBJ(hbmp.0));
        let blt = BitBlt(mem, 0, 0, w, h, Some(hdc), 0, 0, SRCCOPY).is_ok();
        let len = (w as usize) * (h as usize) * 4;
        let mut rgb = Vec::with_capacity((w as usize) * (h as usize) * 3);
        if blt {
            let px = std::slice::from_raw_parts(bits as *const u8, len);
            for quad in px.chunks_exact(4) {
                rgb.push(quad[2]); /* R (buffer is BGRA) */
                rgb.push(quad[1]); /* G */
                rgb.push(quad[0]); /* B */
            }
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, hdc);
        if !blt {
            return Err("screenshot: BitBlt failed".to_string());
        }
        Ok((w as u32, h as u32, rgb))
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::temp_root;
    use serde_json::json;

    #[test]
    fn sys_stats_reports_all_sections() {
        let out = run_sys_stats(&json!({})).expect("sys_stats succeeds");
        assert!(out.contains("CPU:") && out.contains("%"), "{out}");
        assert!(out.contains("RAM:") && out.contains("GB"), "{out}");
        assert!(out.contains("Disk:") && out.contains("GB"), "{out}");
        assert!(out.contains("Battery:"), "{out}");
        assert!(out.contains("Uptime:") && out.contains("m"), "{out}");
    }

    /* ---- set_volume ---- */
    /* Validation only in unit tests — real COM calls change the user's
     * actual output volume, so the set path is e2e territory. */

    #[test]
    fn set_volume_rejects_out_of_range_and_bad_args() {
        assert!(run_set_volume(&json!({"volume": 101})).unwrap_err().contains("0-100"));
        assert!(run_set_volume(&json!({"volume": -1})).unwrap_err().contains("0-100"));
        assert!(run_set_volume(&json!({})).unwrap_err().contains("set_volume"));
        assert!(run_set_volume(&json!({"volume": "loud"}))
            .unwrap_err()
            .contains("set_volume"));
    }

    /* ---- screenshot ---- */

    #[test]
    fn screenshot_rejects_bad_args_before_capturing() {
        assert!(run_screenshot(&json!({"path": "   "})).unwrap_err().contains("path"));
        assert!(run_screenshot(&json!({"path": 42})).unwrap_err().contains("path"));
        assert!(run_screenshot(&json!({"open": "yes"})).unwrap_err().contains("open"));
    }

    #[test]
    fn screenshot_smoke_saves_a_real_png() {
        let dir = temp_root("screenshot");
        let path = dir.join("shot.png");
        let out = run_screenshot(&json!({"path": path.to_string_lossy()})).expect("screenshot");
        assert!(out.contains("Screenshot saved"), "{out}");
        let meta = fs::metadata(&path).expect("png exists on disk");
        assert!(meta.len() > 1_000, "png suspiciously small: {}", meta.len());
        use std::io::Read;
        let mut head = [0u8; 4];
        fs::File::open(&path)
            .unwrap()
            .read_exact(&mut head)
            .expect("readable");
        assert_eq!(&head, &[0x89, b'P', b'N', b'G'], "not a PNG signature");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn screenshot_rotation_drops_oldest_default_named_only() {
        /* BUG-007: beyond SCREENSHOT_KEEP the oldest default-named
         * screenshots are pruned; decoys and explicit-looking files
         * must be untouched. */
        let dir = temp_root("shotrotate");
        for i in 0..25 {
            let name = format!("igris_screenshot_2026-01-01_00-{i:02}-00.png");
            fs::write(dir.join(&name), "x").unwrap();
        }
        fs::write(dir.join("igris_screenshot_2026-01-01_00-99-00.jpg"), "x").unwrap();
        fs::write(dir.join("my_vacation.png"), "x").unwrap();
        let protect = dir.join("igris_screenshot_2026-01-01_00-24-00.png");
        let deleted = prune_old_screenshots(&dir, &protect);
        assert_eq!(deleted, 5, "25 shots, keep 20 → 5 oldest go");
        for i in 0..5 {
            let name = format!("igris_screenshot_2026-01-01_00-{i:02}-00.png");
            assert!(!dir.join(&name).exists(), "{name} should be pruned");
        }
        for i in 5..25 {
            let name = format!("igris_screenshot_2026-01-01_00-{i:02}-00.png");
            assert!(dir.join(&name).exists(), "{name} should stay");
        }
        assert!(dir.join("igris_screenshot_2026-01-01_00-99-00.jpg").exists());
        assert!(dir.join("my_vacation.png").exists());
        /* Even if the just-saved file sorts among the oldest (backwards
         * clock), it is protected. */
        let dir = temp_root("shotrotate2");
        for i in 0..22 {
            let name = format!("igris_screenshot_2026-01-01_00-{i:02}-00.png");
            fs::write(dir.join(&name), "x").unwrap();
        }
        let protect = dir.join("igris_screenshot_2026-01-01_00-00-00.png");
        let deleted = prune_old_screenshots(&dir, &protect);
        assert_eq!(deleted, 2, "22 shots, keep 20 → 2 go, skipping protect");
        assert!(protect.exists(), "protected file survives");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn screenshot_rotation_below_keep_deletes_nothing() {
        let dir = temp_root("shotrotate3");
        for i in 0..5 {
            let name = format!("igris_screenshot_2026-01-01_00-{i:02}-00.png");
            fs::write(dir.join(&name), "x").unwrap();
        }
        let deleted = prune_old_screenshots(
            &dir,
            &dir.join("igris_screenshot_2026-01-01_00-04-00.png"),
        );
        assert_eq!(deleted, 0);
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 5);
        fs::remove_dir_all(&dir).ok();
    }

    /* ---- v1.2 memory: validation path only ------------------------ */

    /* Valid memory calls lock the process-wide store (the user's real
     * file), so lib tests exercise only the argument-validation path —
     * every case below errors out before the store lock is taken. The
     * store logic itself is covered in memory.rs, including the
     * survive-restart proof. */

}
