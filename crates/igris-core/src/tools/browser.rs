//! Native browser automation over the Chrome DevTools Protocol (CDP).
//!
//! No MCP and no async runtime: one sync WebSocket (`tungstenite`) to the
//! browser's debug port drives a dedicated headless Chrome/Brave/Edge.
//! Three tools: `browser_open` (launch or reattach + navigate + preview),
//! `browser_read` (title/url/visible text) and `browser_close`.
//!
//! Session design:
//! - The session lives across calls in a process-global cell; a crashed
//!   run's orphan browser is reattached (its debug port + pid persist in
//!   `%APPDATA%\igris\browser.json`) instead of being duplicated.
//! - The browser runs its own profile dir (`%LOCALAPPDATA%\igris\
//!   browser-profile`), never the user's real one. Chrome 136+ also
//!   requires a non-default user-data-dir for remote debugging, and
//!   `--remote-debugging-pipe` must never accompany the port flag: with
//!   both present Chrome prefers the pipe and the endpoint never opens.
//! - One worker drives the socket per call (registry dispatch), so the
//!   session mutex is only contended if a previous call was cut off by
//!   its budget; the leftover thread then holds it a few seconds more.

use crate::tools::basic::{browser_exe, validate_open_url};
use serde_json::{json, Value};
use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tungstenite::{Message, WebSocket};

/* ================================================================ */
/* budgets                                                            */
/* ================================================================ */

/// Poll granularity: CDP reads use this as the socket read timeout so the
/// call loop can check deadlines instead of blocking on a wedged browser.
const POLL: Duration = Duration::from_millis(100);
/// Cold launch: the discovery endpoint can be slow on the very first run
/// after install (Defender scans the new exe before it starts).
const DISCOVERY_BUDGET: Duration = Duration::from_secs(12);
/// Finding and attaching to the browser's first page tab after launch.
const ATTACH_BUDGET: Duration = Duration::from_secs(4);
/// Page.navigate + document.readyState polling inside browser_open.
const LOAD_BUDGET: Duration = Duration::from_secs(6);
/// The final title/text evaluate inside browser_open.
const INFO_BUDGET: Duration = Duration::from_secs(5);
/// One page read inside browser_read.
const READ_BUDGET: Duration = Duration::from_secs(8);
/// How much visible text browser_read returns to the model.
const READ_CLIP: usize = 3000;
/// Small agent only for the loopback DevTools discovery GET (short
/// overall timeout; the poll loop retries).
static DISCOVERY_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .into()
});

/* ================================================================ */
/* CDP over one sync WebSocket                                        */
/* ================================================================ */

/// One request/reply round-trip on the wire. Chrome interleaves events
/// and periodic WebSocket pings into the stream: pings are answered,
/// events and stale replies are skipped. Reads use the 100 ms socket
/// timeout so the loop can check the deadline instead of blocking
/// forever on a wedged browser. Socket-level errors are prefixed
/// "CDP " so callers can tell a dead socket from a slow page.
fn cdp_call(
    ws: &mut WebSocket<TcpStream>,
    next_id: &mut u64,
    method: &str,
    params: Value,
    session_id: Option<&str>,
    deadline: Instant,
) -> Result<Value, String> {
    let id = *next_id;
    *next_id += 1;
    let mut msg = json!({ "id": id, "method": method, "params": params });
    if let Some(s) = session_id {
        msg["sessionId"] = json!(s);
    }
    ws.send(Message::text(msg.to_string()))
        .map_err(|e| format!("CDP send failed: {e}"))?;
    loop {
        if Instant::now() >= deadline {
            return Err(format!("{method} timed out"));
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                let Ok(v) = serde_json::from_str::<Value>(t.as_str()) else {
                    continue;
                };
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(err) = v.get("error") {
                        return Err(err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("CDP protocol error")
                            .to_string());
                    }
                    return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                }
                // An event or a reply to something else: keep reading.
            }
            Ok(Message::Ping(payload)) => {
                let _ = ws.send(Message::Pong(payload));
            }
            Ok(Message::Close(_)) => return Err("CDP connection closed by the browser".into()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // Deadline is checked at the top of the loop.
            }
            Err(e) => return Err(format!("CDP read failed: {e}")),
        }
    }
}

/// The browser session held between tool calls.
struct BrowserSession {
    ws: WebSocket<TcpStream>,
    next_id: u64,
    /// Target.attachToTarget session id for the page this process drives.
    page: String,
    /// The browser's OS pid (from our spawn, or the port file on reattach).
    pid: u32,
    /// Some when we spawned it (we hold the Child); None on reattach.
    child: Option<Child>,
}

impl BrowserSession {
    /// A call scoped to the page session (navigate, evaluate, ...).
    fn call(&mut self, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        cdp_call(
            &mut self.ws,
            &mut self.next_id,
            method,
            params,
            Some(&self.page),
            deadline,
        )
    }

    /// A browser-level call (Target.*, Browser.*).
    fn call_browser(
        &mut self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, String> {
        cdp_call(&mut self.ws, &mut self.next_id, method, params, None, deadline)
    }

    /// Poll `document.readyState` until complete or the deadline. A
    /// destroyed execution context mid-navigation reads as an error
    /// reply; that is just "not ready yet" until the deadline.
    fn wait_ready(&mut self, deadline: Instant) -> bool {
        std::thread::sleep(Duration::from_millis(150));
        loop {
            if let Ok(v) = self.call(
                "Runtime.evaluate",
                json!({ "expression": "document.readyState", "returnByValue": true }),
                deadline,
            ) {
                if v["result"]["value"].as_str() == Some("complete") {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(POLL);
        }
    }

    /// Pull title, URL and visible text in one evaluate round-trip.
    fn page_info(&mut self, deadline: Instant) -> Result<(String, String, String), String> {
        let reply = self.call(
            "Runtime.evaluate",
            json!({ "expression": PAGE_INFO_JS, "returnByValue": true }),
            deadline,
        )?;
        let payload = reply["result"]["value"].as_str().unwrap_or("");
        let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        Ok((
            v["title"].as_str().unwrap_or_default().to_string(),
            v["url"].as_str().unwrap_or_default().to_string(),
            v["text"].as_str().unwrap_or_default().to_string(),
        ))
    }
}

/// JS that fetches title, URL and visible text in a single evaluate.
const PAGE_INFO_JS: &str = "JSON.stringify({title: document.title, url: location.href, \
     text: document.body ? document.body.innerText : ''})";

static SESSION: OnceLock<Mutex<Option<BrowserSession>>> = OnceLock::new();

fn session_cell() -> &'static Mutex<Option<BrowserSession>> {
    SESSION.get_or_init(|| Mutex::new(None))
}

/* ================================================================ */
/* session lifecycle                                                  */
/* ================================================================ */

/// Reattach to a browser remembered in the port file, else spawn a fresh
/// one. On spawn success the port file is written so a later crash (of
/// IGRIS, not the browser) reattaches instead of duplicating.
fn open_session(exe: &Path, headless: bool) -> Result<BrowserSession, String> {
    let path = port_file_path();
    if let Some((port, pid)) = read_port_file(&path) {
        match connect_and_attach(port) {
            Ok((ws, next_id, page)) => {
                return Ok(BrowserSession { ws, next_id, page, pid, child: None });
            }
            Err(_) => {
                // Stale entry: the orphan browser is gone; fall through
                // and spawn a fresh one (the file is rewritten below).
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let (port, pid, child) = spawn_browser(exe, headless)?;
    write_port_file(&path, port, pid)?;
    let (ws, next_id, page) = connect_and_attach(port)?;
    Ok(BrowserSession { ws, next_id, page, pid, child: Some(child) })
}

/// Pick a free port, spawn the automation-dedicated browser and wait for
/// its DevTools discovery endpoint to answer.
fn spawn_browser(exe: &Path, headless: bool) -> Result<(u16, u32, Child), String> {
    let port = {
        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local debug port: {e}"))?;
        listener
            .local_addr()
            .map_err(|e| format!("no free local debug port: {e}"))?
            .port()
    };
    let profile = profile_dir();
    std::fs::create_dir_all(&profile)
        .map_err(|e| format!("could not create the browser profile dir: {e}"))?;

    let mut args: Vec<String> = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--window-size=1280,800".into(),
    ];
    if headless {
        args.push("--headless".into());
    }
    args.push("about:blank".into());

    let mut child = Command::new(exe)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("could not launch {}: {e}", exe.display()))?;
    let pid = child.id();

    let deadline = Instant::now() + DISCOVERY_BUDGET;
    loop {
        if ws_url(port).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the browser's DevTools endpoint never came up on port {port}"
            ));
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    Ok((port, pid, child))
}

/// Connect the browser-level CDP WebSocket and attach to the first page
/// tab. Returns (socket, next message id, page sessionId).
fn connect_and_attach(port: u16) -> Result<(WebSocket<TcpStream>, u64, String), String> {
    let url = ws_url(port)?;
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}")
        .parse()
        .map_err(|e| format!("bad loopback address: {e}"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))
        .map_err(|e| format!("could not reach the browser's debug port: {e}"))?;
    let (mut ws, _resp) = tungstenite::client(url.as_str(), stream)
        .map_err(|e| format!("CDP websocket handshake failed: {e}"))?;
    let _ = ws.get_ref().set_read_timeout(Some(POLL));
    let _ = ws.get_ref().set_write_timeout(Some(Duration::from_secs(5)));

    let mut next_id: u64 = 1;
    let deadline = Instant::now() + ATTACH_BUDGET;
    // The initial about:blank tab may take a moment to register after a
    // cold launch, so retry getTargets until a page target appears.
    let target = loop {
        if Instant::now() >= deadline {
            return Err("CDP attach failed: the browser had no page tab".into());
        }
        let result = cdp_call(&mut ws, &mut next_id, "Target.getTargets", json!({}), None, deadline)?;
        if let Some(id) = result["targetInfos"]
            .as_array()
            .and_then(|targets| targets.iter().find(|t| t["type"] == "page"))
            .and_then(|t| t["targetId"].as_str())
        {
            break id.to_string();
        }
        std::thread::sleep(POLL);
    };
    let attached = cdp_call(
        &mut ws,
        &mut next_id,
        "Target.attachToTarget",
        json!({ "targetId": target, "flatten": true }),
        None,
        deadline,
    )?;
    let page = attached["sessionId"]
        .as_str()
        .ok_or_else(|| "CDP attachToTarget returned no sessionId".to_string())?
        .to_string();
    Ok((ws, next_id, page))
}

/* ================================================================ */
/* discovery endpoint + port persistence                              */
/* ================================================================ */

/// Loopback-only GET of /json/version; returns webSocketDebuggerUrl.
fn ws_url(port: u16) -> Result<String, String> {
    let url = format!("http://127.0.0.1:{port}/json/version");
    let resp = DISCOVERY_AGENT
        .get(&url)
        .call()
        .map_err(|e| format!("the browser's DevTools endpoint did not answer: {e}"))?;
    let body = resp
        .into_body()
        .read_to_string()
        .map_err(|e| format!("bad DevTools endpoint reply: {e}"))?;
    let v: Value =
        serde_json::from_str(&body).map_err(|e| format!("bad DevTools endpoint JSON: {e}"))?;
    v["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "DevTools endpoint returned no webSocketDebuggerUrl".into())
}

/// Where the running browser's debug port + pid are remembered, so a
/// crashed run's orphan browser is reattached instead of duplicated.
fn port_file_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("igris")
        .join("browser.json")
}

/// Dedicated profile (never the user's real one): keeps automation state
/// out of the user's sessions and satisfies the Chrome 136+ requirement.
fn profile_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("igris")
        .join("browser-profile")
}

fn write_port_file(path: &Path, port: u16, pid: u32) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(path, json!({ "port": port, "pid": pid }).to_string())
        .map_err(|e| format!("could not write the browser port file: {e}"))
}

/// Parse a port file. Missing or malformed -> None ("no browser").
fn read_port_file(path: &Path) -> Option<(u16, u32)> {
    let body = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let port = v.get("port")?.as_u64()?;
    let pid = v.get("pid")?.as_u64()?;
    if port == 0 || port > 65535 || pid == 0 {
        return None;
    }
    Some((port as u16, pid as u32))
}

/* ================================================================ */
/* browser pick                                                       */
/* ================================================================ */

/// Pick order for the automation browser when the caller names none.
const AUTOMATION_PICK_ORDER: &[&str] = &["chrome", "brave", "edge"];

/// Resolve which browser exe to drive. Firefox has no CDP, so it is
/// rejected with an explicit message instead of an opaque failure later.
fn pick_browser(arg: Option<&str>) -> Result<(PathBuf, String), String> {
    if let Some(name) = arg.map(str::trim).filter(|n| !n.is_empty()) {
        let lower = name.to_ascii_lowercase();
        if lower == "firefox" {
            return Err("firefox does not support the DevTools protocol (CDP); \
                        use chrome, brave or edge for browser tools"
                .into());
        }
        let exe = browser_exe(&lower)?;
        return Ok((exe, lower));
    }
    for name in AUTOMATION_PICK_ORDER {
        if let Ok(exe) = browser_exe(name) {
            return Ok((exe, (*name).to_string()));
        }
    }
    Err("no automation browser found - install chrome, brave or edge".into())
}

/* ================================================================ */
/* tool entry points                                                  */
/* ================================================================ */

pub(crate) fn run_browser_open(args: &Value) -> Result<String, String> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "browser_open needs a url (http or https)".to_string())?;
    validate_open_url(url)?;
    let headless = args.get("headless").and_then(Value::as_bool).unwrap_or(true);
    let (exe, name) = pick_browser(args.get("browser").and_then(Value::as_str))?;

    let mut cell = session_cell().lock().unwrap();
    let mut sess = match cell.take() {
        Some(s) => s,
        None => open_session(&exe, headless)?,
    };
    let deadline = Instant::now() + LOAD_BUDGET;
    let nav = match sess.call("Page.navigate", json!({ "url": url }), deadline) {
        Ok(v) => v,
        Err(e) if e.starts_with("CDP ") => {
            // The old socket died (browser closed between calls):
            // relaunch once inside this call instead of bouncing it back.
            sess = open_session(&exe, headless)?;
            sess.call("Page.navigate", json!({ "url": url }), deadline)?
        }
        Err(e) => {
            *cell = Some(sess);
            return Err(e);
        }
    };
    if let Some(err) = nav["errorText"].as_str().filter(|s| !s.is_empty()) {
        *cell = Some(sess);
        return Err(format!("navigation failed: {err}"));
    }
    let loaded = sess.wait_ready(deadline);
    let info = match sess.page_info(Instant::now() + INFO_BUDGET) {
        Ok(i) => i,
        Err(e) => {
            if !e.starts_with("CDP ") {
                *cell = Some(sess);
            }
            return Err(e);
        }
    };
    *cell = Some(sess);
    let (title, _, text) = info;

    let mode = if headless { "headless" } else { "headed" };
    let mut out = format!("Opened {url} in {name} ({mode}).");
    if !loaded {
        out.push_str("\nNote: the page did not signal load completion; content may be partial.");
    }
    if !title.is_empty() {
        out.push_str(&format!("\nTitle: {title}"));
    }
    let preview = clip_text(text.trim(), 400);
    if !preview.is_empty() {
        out.push('\n');
        out.push_str(&preview);
    }
    Ok(out)
}

pub(crate) fn run_browser_read(_args: &Value) -> Result<String, String> {
    let mut cell = session_cell().lock().unwrap();
    let Some(mut sess) = cell.take() else {
        return Err("no browser session is open - call browser_open first".into());
    };
    let result = sess.page_info(Instant::now() + READ_BUDGET);
    match result {
        Ok((title, url, text)) => {
            *cell = Some(sess);
            let mut out = String::new();
            if !title.is_empty() {
                out.push_str(&format!("Title: {title}\n"));
            }
            if !url.is_empty() {
                out.push_str(&format!("URL: {url}\n\n"));
            }
            out.push_str(&clip_text(text.trim(), READ_CLIP));
            Ok(out)
        }
        Err(e) => {
            if !e.starts_with("CDP ") {
                *cell = Some(sess);
            }
            Err(e)
        }
    }
}

pub(crate) fn run_browser_close(_args: &Value) -> Result<String, String> {
    let mut cell = session_cell().lock().unwrap();
    let Some(mut sess) = cell.take() else {
        return Ok("No browser session was open.".into());
    };
    // Ask the browser to shut down; the CDP connection dies with it, so
    // an error here is expected and ignored.
    let _ = sess.call_browser("Browser.close", json!({}), Instant::now() + Duration::from_secs(3));
    ensure_dead(sess.child.as_mut(), sess.pid);
    drop(sess);
    let _ = std::fs::remove_file(port_file_path());
    Ok("Browser closed.".into())
}

/// Make sure the browser process is gone: give a graceful close ~3 s,
/// then kill the whole tree (taskkill /T covers Chrome's helper
/// processes; needed for reattached orphans, belt-and-braces for ours).
fn ensure_dead(mut child: Option<&mut Child>, pid: u32) {
    if let Some(c) = child.as_mut() {
        for _ in 0..30 {
            match c.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break,
            }
        }
    }
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Some(c) = child {
        let _ = c.wait();
    }
}

/// Trim to `max` characters without splitting a multi-byte character.
fn clip_text(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}...", s[..cut].trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    /* Hermetic tests only: nothing here may launch a browser or touch a
     * socket. The live success path is exercised manually via the chat
     * binary and recorded in benchmark.md. */

    #[test]
    fn browser_open_validates_url_and_browser_before_any_session() {
        assert!(run_browser_open(&json!({})).unwrap_err().contains("url"));
        assert!(run_browser_open(&json!({ "url": "file:///etc/passwd" })).is_err());
        assert!(run_browser_open(&json!({ "url": "https://example.com", "browser": "firefox" }))
            .unwrap_err()
            .contains("DevTools protocol"));
    }

    #[test]
    fn pick_browser_rejects_firefox_with_a_cdp_message() {
        let err = pick_browser(Some("firefox")).unwrap_err();
        assert!(err.contains("DevTools protocol"), "{err}");
        assert!(pick_browser(Some("FireFox ")).is_err());
    }

    #[test]
    fn port_file_roundtrips_and_rejects_garbage() {
        let dir = std::env::temp_dir().join(format!("igris-browser-test-{}", std::process::id()));
        let path = dir.join("browser.json");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(read_port_file(&path).is_none());
        write_port_file(&path, 9223, 4242).unwrap();
        assert_eq!(read_port_file(&path), Some((9223, 4242)));
        std::fs::write(&path, "not json").unwrap();
        assert!(read_port_file(&path).is_none());
        std::fs::write(&path, r#"{"port":0,"pid":1}"#).unwrap();
        assert!(read_port_file(&path).is_none());
        std::fs::write(&path, r#"{"port":70000,"pid":1}"#).unwrap();
        assert!(read_port_file(&path).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clip_text_respects_char_boundaries_and_marks_truncation() {
        assert_eq!(clip_text("short", 100), "short");
        let long = "é".repeat(50);
        assert_eq!(clip_text(&long, 10), format!("{}...", "é".repeat(10)));
        assert_eq!(clip_text("", 10), "");
    }

    #[test]
    fn browser_read_and_close_are_honest_without_a_session() {
        assert!(run_browser_read(&json!({})).unwrap_err().contains("browser_open"));
        assert!(run_browser_close(&json!({})).is_ok());
    }
}
