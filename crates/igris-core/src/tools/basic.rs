//! Quick-action tools: get_time, calculate (v1 calculator tenant),
//! open_app (hard allowlist), open_url (http/https only), media_key.

use crate::tools::shared::required_str;
use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, VK_MEDIA_NEXT_TRACK,
    VK_MEDIA_PLAY_PAUSE, VK_MEDIA_PREV_TRACK, VK_MEDIA_STOP,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/* ================================================================ */
/* get_time                                                          */
/* ================================================================ */

/// Format stays stable so tests (and the model, next turn) can parse it.
pub(crate) fn run_get_time(_args: &Value) -> Result<String, String> {
    let now = chrono::Local::now();
    Ok(format!(
        "{} on {}",
        now.format("%Y-%m-%d %H:%M:%S"),
        now.format("%A")
    ))
}

/* ================================================================ */
/* calculate ΓÇö ported from zeroclaw-tools/src/calculator.rs          */
/* (v1 subset; helpers and formatting kept 1:1 with upstream)        */
/* ================================================================ */

const MAX_ROUND_DECIMALS: i64 = 15;
const MAX_VALUES_LEN: usize = 10_000;

pub(crate) fn run_calculate(args: &Value) -> Result<String, String> {
    let function = args
        .get("function")
        .and_then(|v| v.as_str())
        .ok_or("Missing required parameter: function")?;
    match function {
        "add" => calc_add(args),
        "subtract" => calc_subtract(args),
        "multiply" => calc_multiply(args),
        "divide" => calc_divide(args),
        "pow" => calc_pow(args),
        "sqrt" => calc_sqrt(args),
        "abs" => calc_abs(args),
        "modulo" => calc_modulo(args),
        "round" => calc_round(args),
        "sum" => calc_sum(args),
        "average" => calc_average(args),
        "min" => calc_min(args),
        "max" => calc_max(args),
        other => Err(format!("Unknown function: {other}")),
    }
}

pub(crate) fn extract_f64(args: &Value, key: &str, name: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(|v| v.as_f64())
        .ok_or_else(|| format!("Missing required parameter: {name}"))
}

pub(crate) fn extract_i64(args: &Value, key: &str, name: &str) -> Result<i64, String> {
    args.get(key)
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("Missing required parameter: {name}"))
}

pub(crate) fn extract_values(args: &Value, min_len: usize) -> Result<Vec<f64>, String> {
    let values = args
        .get("values")
        .and_then(|v| v.as_array())
        .ok_or("Missing required parameter: values (array of numbers)")?;
    if values.len() < min_len {
        return Err(format!(
            "Expected at least {min_len} value(s), got {}",
            values.len()
        ));
    }
    if values.len() > MAX_VALUES_LEN {
        return Err(format!(
            "values array must be at most {MAX_VALUES_LEN} entries, got {}",
            values.len()
        ));
    }
    let mut nums = Vec::with_capacity(values.len());
    for (i, v) in values.iter().enumerate() {
        match v.as_f64() {
            Some(n) => nums.push(n),
            None => return Err(format!("values[{i}] is not a valid number")),
        }
    }
    Ok(nums)
}

/// Integer-shaped floats print without a decimal tail (upstream parity).
fn format_num(n: f64) -> String {
    if n == n.floor() && n.abs() < 1e15 {
        let rounded = n.round() as i128;
        format!("{rounded}")
    } else {
        format!("{n}")
    }
}

fn calc_add(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    Ok(format_num(values.iter().sum()))
}

fn calc_subtract(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut iter = values.iter();
    let mut result = iter
        .next()
        .copied()
        .ok_or("Subtraction requires at least one value")?;
    for v in iter {
        result -= v;
    }
    Ok(format_num(result))
}

fn calc_multiply(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut result = 1.0;
    for v in &values {
        result *= v;
    }
    Ok(format_num(result))
}

fn calc_divide(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 2)?;
    let mut iter = values.iter();
    let mut result = iter
        .next()
        .copied()
        .ok_or("Division requires at least one value")?;
    for v in iter {
        if *v == 0.0 {
            return Err("Division by zero".to_string());
        }
        result /= v;
    }
    Ok(format_num(result))
}

fn calc_pow(args: &Value) -> Result<String, String> {
    let base = extract_f64(args, "a", "a (base)")?;
    let exp = extract_f64(args, "b", "b (exponent)")?;
    Ok(format_num(base.powf(exp)))
}

fn calc_sqrt(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    if x < 0.0 {
        return Err("Cannot compute square root of a negative number".to_string());
    }
    Ok(format_num(x.sqrt()))
}

fn calc_abs(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    Ok(format_num(x.abs()))
}

fn calc_modulo(args: &Value) -> Result<String, String> {
    let a = extract_f64(args, "a", "a")?;
    let b = extract_f64(args, "b", "b")?;
    if b == 0.0 {
        return Err("Modulo by zero".to_string());
    }
    Ok(format_num(a % b))
}

fn calc_round(args: &Value) -> Result<String, String> {
    let x = extract_f64(args, "x", "x")?;
    let decimals = extract_i64(args, "decimals", "decimals")?;
    if !(0..=MAX_ROUND_DECIMALS).contains(&decimals) {
        return Err(format!(
            "decimals must be between 0 and {MAX_ROUND_DECIMALS}"
        ));
    }
    let multiplier = 10_f64.powi(decimals as i32);
    Ok(format_num((x * multiplier).round() / multiplier))
}

fn calc_sum(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    Ok(format_num(values.iter().sum()))
}

fn calc_average(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    Ok(format_num(values.iter().sum::<f64>() / values.len() as f64))
}

fn calc_min(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    let Some(min_val) = values.iter().copied().reduce(f64::min) else {
        return Err("Cannot compute min of an empty array".to_string());
    };
    Ok(format_num(min_val))
}

fn calc_max(args: &Value) -> Result<String, String> {
    let values = extract_values(args, 1)?;
    let Some(max_val) = values.iter().copied().reduce(f64::max) else {
        return Err("Cannot compute max of an empty array".to_string());
    };
    Ok(format_num(max_val))
}

/* ================================================================ */
/* open_app ΓÇö hard allowlist, no shell, no arbitrary paths           */
/* ================================================================ */

const OPEN_APP_HINT: &str = "Allowed apps: notepad, calc, chrome, spotify, vscode";

/// Candidate executables for an allowed app, tried in order (PATH name
/// first, then well-known install locations). Returns None for any name
/// outside the allowlist ΓÇö that rejection happens before any spawn.
fn app_candidates(app: &str) -> Option<Vec<String>> {
    let path_names: &[&str] = match app {
        "notepad" => &["notepad"],
        "calc" => &["calc"],
        "vscode" => &["code"],
        "chrome" => &["chrome"],
        "spotify" => &["spotify"],
        _ => return None,
    };
    let mut candidates: Vec<String> = path_names.iter().map(|s| s.to_string()).collect();
    /* Well-known install locations for apps that are not on PATH. These
     * are fixed templates, resolved at use time ΓÇö never caller-supplied. */
    let program_files = std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".into());
    let program_files_x86 =
        std::env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".into());
    match app {
        "chrome" => {
            candidates.push(
                std::path::PathBuf::from(&program_files)
                    .join(r"Google\Chrome\Application\chrome.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
            candidates.push(
                std::path::PathBuf::from(&program_files_x86)
                    .join(r"Google\Chrome\Application\chrome.exe")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        "spotify" => {
            if let Ok(appdata) = std::env::var("APPDATA") {
                candidates.push(
                    std::path::PathBuf::from(appdata)
                        .join(r"Spotify\Spotify.exe")
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
        _ => {}
    }
    Some(candidates)
}

/// Chrome launches into its profile picker when several profiles exist
/// and the command line pins none — the user then sees a selector, not
/// the usable browser `open_app` claims to have launched (observed on
/// the benchmark host, S3-10 family). Pick the profile Chrome itself
/// used last, from its own Local State. Pure over the file's text so
/// the JSON shape is testable hermetically.
fn chrome_profile_dir(local_state: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(local_state).ok()?;
    v.get("profile")?.get("last_used")?.as_str().and_then(|s| {
        (!s.is_empty()).then(|| s.to_string())
    })
}

/// The launch arguments that open Chrome straight into a usable
/// window: the last-used profile (falling back to the classic Default
/// profile when Local State is unreadable) and no first-run wizard.
fn chrome_launch_args() -> Vec<String> {
    let dir = std::env::var("LOCALAPPDATA")
        .ok()
        .and_then(|l| {
            std::fs::read_to_string(
                std::path::PathBuf::from(l)
                    .join(r"Google\Chrome\User Data\Local State"),
            )
            .ok()
        })
        .and_then(|s| chrome_profile_dir(&s))
        .unwrap_or_else(|| "Default".to_string());
    vec![
        "--no-first-run".to_string(),
        format!("--profile-directory={dir}"),
    ]
}

/// Spawns the GUI process detached (the Child handle is dropped, the app
/// keeps running). Note for tests: no unit test dispatches an allowed
/// name ΓÇö actually launching an app is Phase 5 e2e territory.
pub(crate) fn run_open_app(args: &Value) -> Result<String, String> {
    let app = args
        .get("app")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing required parameter: app. {OPEN_APP_HINT}"))?;
    let Some(candidates) = app_candidates(app) else {
        return Err(format!("'{app}' is not an allowed app. {OPEN_APP_HINT}"));
    };
    let mut last_err = String::new();
    for candidate in candidates {
        let mut cmd = std::process::Command::new(&candidate);
        if app == "chrome" {
            for a in chrome_launch_args() {
                cmd.arg(a);
            }
        }
        match cmd.spawn() {
            Ok(_child) => return Ok(format!("Launched {app}.")),
            Err(e) => last_err = format!("{candidate}: {e}"),
        }
    }
    Err(format!("Could not launch {app}. {last_err}"))
}


/* ================================================================ */
/* open_url ΓÇö ShellExecuteW, http/https scheme allowlist             */
/* ================================================================ */

/// Only web URLs: prevents protocol-handler abuse (e.g. `file://` or an
/// installed-app scheme launching a local program) ΓÇö the browser is the
/// only permitted target.
pub(crate) fn validate_open_url(url: &str) -> Result<(), String> {
    let u = url.trim();
    if !(u.starts_with("http://") || u.starts_with("https://")) {
        return Err(format!(
            "Only http:// and https:// URLs are allowed (got '{url}')"
        ));
    }
    if u.len() < 8 {
        return Err("URL is too short".into());
    }
    for c in u.chars() {
        if c.is_control() || matches!(c, ' ' | '"' | '<' | '>' | '\'' | '\\') {
            return Err(format!("URL contains the invalid character '{c}'"));
        }
    }
    Ok(())
}

/// ShellExecuteW "open" on a target string (a URL for the default
/// browser, or a file path for its default handler). Shell semantics:
/// instance values <= 32 mean failure.
pub(crate) fn shell_open(target: &str) -> Result<(), String> {
    let verb = to_wide("open");
    let file = to_wide(target);
    /* SAFETY: ShellExecuteW with NUL-terminated buffers we built above;
     * nothing borrowed outlives the call. */
    let hinst = unsafe {
        ShellExecuteW(
            Some(HWND::default()),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if (hinst.0 as isize) <= 32 {
        return Err(format!("ShellExecuteW failed for '{target}'"));
    }
    Ok(())
}

/// UTF-16 with a trailing NUL, for `PCWSTR` arguments.
pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub(crate) fn run_open_url(args: &Value) -> Result<String, String> {
    let url = required_str(args, "url", "open_url")?;
    validate_open_url(url)?;
    let url = url.trim().to_string();
    shell_open(&url)?;
    Ok(format!("Opened {url} in the default browser."))
}

/* ================================================================ */
/* media_key ΓÇö global keybd_event of four fixed VK_MEDIA_* keys      */
/* ================================================================ */

/// Map the tool's key name to its virtual-key code. Only these four
/// exist ΓÇö no free-form key injection.
fn media_vk(key: &str) -> Option<u8> {
    match key {
        "play_pause" => Some(VK_MEDIA_PLAY_PAUSE.0 as u8),
        "next_track" => Some(VK_MEDIA_NEXT_TRACK.0 as u8),
        "previous_track" => Some(VK_MEDIA_PREV_TRACK.0 as u8),
        "stop" => Some(VK_MEDIA_STOP.0 as u8),
        _ => None,
    }
}

pub(crate) fn run_media_key(args: &Value) -> Result<String, String> {
    let key = required_str(args, "key", "media_key")?;
    let Some(vk) = media_vk(key) else {
        return Err(format!(
            "Unknown media key '{key}'. Allowed: play_pause, next_track, previous_track, stop"
        ));
    };
    /* SAFETY: a standard global synthetic keypress (down + up) of a fixed
     * VK_MEDIA_* code; no other input state is touched. */
    unsafe {
        keybd_event(vk, 0, KEYBD_EVENT_FLAGS(0), 0);
        keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
    }
    Ok(format!("Pressed media key: {key}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::reg;
    use serde_json::json;

    #[test]
    fn get_time_output_is_stable_and_parseable() {
        let out = reg().dispatch("get_time", &json!({})).unwrap();
        let re = regex::Regex::new(
            r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2} on (Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday)$",
        )
        .unwrap();
        assert!(re.is_match(&out), "unexpected get_time output: {out}");
    }

    #[test]
    fn get_time_ignores_extra_args() {
        assert!(reg().dispatch("get_time", &json!({"x": 1})).is_ok());
    }

    /* ---- calculate ---- */

    #[test]
    fn calculate_arithmetic_core() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "add", "values": [2.0, 2.0]}))
                .unwrap(),
            "4"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "add", "values": [1.0, 2.0, 3.5]})
            )
            .unwrap(),
            "6.5"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "subtract", "values": [10.0, 3.0, 1.5]})
            )
            .unwrap(),
            "5.5"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "multiply", "values": [3.0, 4.0, 5.0]})
            )
            .unwrap(),
            "60"
        );
    }

    #[test]
    fn calculate_division_and_div_by_zero() {
        let r = reg();
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "divide", "values": [100.0, 4.0]})
            )
            .unwrap(),
            "25"
        );
        let err = r
            .dispatch(
                "calculate",
                &json!({"function": "divide", "values": [10.0, 0.0]}),
            )
            .unwrap_err();
        assert!(err.contains("zero"), "{err}");
    }

    #[test]
    fn calculate_pow_sqrt_modulo_round() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "pow", "a": 2.0, "b": 10.0}))
                .unwrap(),
            "1024"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "sqrt", "x": 144.0}))
                .unwrap(),
            "12"
        );
        assert!(r
            .dispatch("calculate", &json!({"function": "sqrt", "x": -4.0}))
            .is_err());
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "modulo", "a": 17.0, "b": 5.0}))
                .unwrap(),
            "2"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "round", "x": 2.715, "decimals": 2})
            )
            .unwrap(),
            "2.72"
        );
    }

    #[test]
    fn calculate_aggregates_and_unknown_function() {
        let r = reg();
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "sum", "values": [1.0, 2.0, 3.0]}))
                .unwrap(),
            "6"
        );
        assert_eq!(
            r.dispatch(
                "calculate",
                &json!({"function": "average", "values": [10.0, 20.0, 30.0]})
            )
            .unwrap(),
            "20"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "min", "values": [5.0, 1.0, 8.0]}))
                .unwrap(),
            "1"
        );
        assert_eq!(
            r.dispatch("calculate", &json!({"function": "max", "values": [5.0, 1.0, 8.0]}))
                .unwrap(),
            "8"
        );
        let err = r
            .dispatch(
                "calculate",
                &json!({"function": "variance", "values": [1.0, 2.0]}),
            )
            .unwrap_err();
        assert!(err.contains("Unknown function"), "{err}");
    }

    #[test]
    fn calculate_garbage_input_is_error_not_panic() {
        let r = reg();
        assert!(r.dispatch("calculate", &json!({})).is_err());
        assert!(r.dispatch("calculate", &json!({"function": "add"})).is_err());
        assert!(r
            .dispatch(
                "calculate",
                &json!({"function": "add", "values": ["a", "b"]})
            )
            .is_err());
        assert!(r
            .dispatch("calculate", &json!({"function": "add", "values": [1.0]}))
            .is_err());
        assert!(r.dispatch("calculate", &json!({"function": "sqrt"})).is_err());
    }

    /* ---- open_app ---- */
    /* No unit test dispatches an allowed name: that would really launch a
     * GUI app. Real launches are Phase 5 e2e. */

    #[test]
    fn open_app_rejects_non_allowlisted_names_before_any_spawn() {
        let err = reg()
            .dispatch("open_app", &json!({"app": "terminal"}))
            .unwrap_err();
        assert!(err.contains("not an allowed app"), "{err}");
    }

    #[test]
    fn chrome_profile_dir_reads_last_used_else_falls_back() {
        assert_eq!(
            chrome_profile_dir(r#"{"profile":{"last_used":"Profile 1"}}"#).as_deref(),
            Some("Profile 1")
        );
        /* this host's real shape */
        assert_eq!(
            chrome_profile_dir(r#"{"profile":{"last_used":"Default"}}"#).as_deref(),
            Some("Default")
        );
        /* missing, empty or broken Local State -> caller falls back to
         * "Default"; never an empty --profile-directory= */
        assert!(chrome_profile_dir("{}").is_none());
        assert!(chrome_profile_dir(r#"{"profile":{"last_used":""}}"#).is_none());
        assert!(chrome_profile_dir("not json").is_none());
        assert!(chrome_profile_dir(r#"{"profile":"notanobject"}"#).is_none());
    }

    #[test]
    fn open_app_empty_or_missing_args_are_errors() {
        let r = reg();
        assert!(r.dispatch("open_app", &json!({})).is_err());
        assert!(r.dispatch("open_app", &json!({"app": 5})).is_err());
    }

    #[test]
    fn open_app_allowlist_covers_the_decision_3_set() {
        for app in ["notepad", "calc", "chrome", "spotify", "vscode"] {
            assert!(app_candidates(app).is_some(), "{app} should be allowed");
        }
        assert!(app_candidates("word").is_none());
    }

    #[test]
    fn open_app_spotify_candidates_include_the_appdata_install_path() {
        let cands = app_candidates("spotify").unwrap();
        assert!(cands.iter().any(|c| c.ends_with("Spotify.exe")));
    }

    /* ---- shared path helpers ---- */

    #[test]
    fn open_url_validates_schemes_and_characters() {
        assert!(validate_open_url("https://example.com").is_ok());
        assert!(validate_open_url("http://localhost:8080/x?y=1&z=2").is_ok());
        for bad in [
            "ftp://example.com",
            "file:///C:/Windows/System32/calc.exe",
            "example.com",
            "ms-settings:display",
            "https://exa mple.com",
            "https://ex\"ample.com",
            "https://ex\nample.com",
            "",
        ] {
            assert!(validate_open_url(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn open_url_rejects_missing_args_without_launching() {
        assert!(reg().dispatch("open_url", &json!({})).is_err());
        assert!(reg().dispatch("open_url", &json!({"url": 5})).is_err());
    }

    /* ---- media_key ---- */
    /* Valid presses are never dispatched in unit tests ΓÇö that toggles the
     * user's real media playback. Mapping + validation are unit-tested;
     * the actual keybd_event path is e2e territory. */

    #[test]
    fn media_key_mapping_covers_the_four_keys() {
        /* Official Winuser.h values (0xB0-0xB3); volume keys sit lower at
         * 0xAD-0xAF, so no name can ever map to a volume key. */
        assert_eq!(media_vk("next_track"), Some(0xB0));
        assert_eq!(media_vk("previous_track"), Some(0xB1));
        assert_eq!(media_vk("stop"), Some(0xB2));
        assert_eq!(media_vk("play_pause"), Some(0xB3));
        assert!(media_vk("volume_up").is_none());
        assert!(media_vk("").is_none());
    }

    #[test]
    fn media_key_rejects_unknown_and_missing_keys() {
        assert!(reg().dispatch("media_key", &json!({})).is_err());
        assert!(reg()
            .dispatch("media_key", &json!({"key": "volume_mute"}))
            .is_err());
    }

    /* ---- sys_stats ---- */
    /* Read-only queries on the live OS, so a full smoke run is hermetic. */

}
