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
use std::process::{Command, Stdio};
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
/// One click evaluate + the post-click settle read inside browser_click.
const CLICK_BUDGET: Duration = Duration::from_secs(10);
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
    child: Option<crate::llm::IsolatedChild>,
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

    /// Capture the current viewport as a PNG on disk (for Mirage vision).
    fn screenshot(&mut self, path: &Path, deadline: Instant) -> Result<(), String> {
        let reply = self.call(
            "Page.captureScreenshot",
            json!({ "format": "png", "captureBeyondViewport": false }),
            deadline,
        )?;
        let data = reply["data"]
            .as_str()
            .ok_or("screenshot reply had no data")?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|e| format!("bad screenshot data: {e}"))?;
        std::fs::write(path, bytes).map_err(|e| format!("could not write screenshot: {e}"))
    }

    /// Click at canvas coordinates with trusted CDP mouse events.
    fn click_xy(&mut self, x: f64, y: f64, deadline: Instant) -> Result<(), String> {
        let base = json!({ "x": x, "y": y, "button": "left", "clickCount": 1 });
        let mut pressed = base.clone();
        pressed["type"] = json!("mousePressed");
        self.call("Input.dispatchMouseEvent", pressed, deadline)?;
        let mut released = base;
        released["type"] = json!("mouseReleased");
        self.call("Input.dispatchMouseEvent", released, deadline)?;
        Ok(())
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

/// One-shot synthetic click. `__SEL__`/`__TXT__` are replaced with JSON
/// string literals (so quotes in caller input stay harmless). With both
/// arguments the element must match the selector AND contain the text
/// (case-insensitive, first 400 candidates); with one, that one rule
/// applies. Returns a JSON summary for honest feedback.
const CLICK_JS: &str = r#"JSON.stringify((function () {
    var sel = __SEL__;
    var tx = __TXT__;
    var el = null;
    var key = function (n) {
        return ((n.textContent || '') + ' ' + (n.getAttribute('title') || '')).trim().toLowerCase();
    };
    if (sel && tx) {
        var lc = tx.toLowerCase();
        var cands = document.querySelectorAll(sel);
        for (var i = 0; i < cands.length && i < 400; i++) {
            if (key(cands[i]).indexOf(lc) !== -1) { el = cands[i]; break; }
        }
    } else if (sel) {
        el = document.querySelector(sel);
    } else if (tx) {
        var lc2 = tx.toLowerCase();
        var cands2 = document.querySelectorAll('a,button,[role="button"],[role="link"]');
        for (var j = 0; j < cands2.length && j < 400; j++) {
            if (key(cands2[j]).indexOf(lc2) !== -1) { el = cands2[j]; break; }
        }
    }
    if (!el) { return { found: false }; }
    el.click();
    return { found: true, tag: el.tagName, text: (el.textContent || '').trim().slice(0, 80), href: el.href || '' };
})())"#;

/// Fill [`CLICK_JS`] with the caller's selector/text as JSON literals.
fn click_js(selector: Option<&str>, text: Option<&str>) -> String {
    let sel = serde_json::to_string(selector.unwrap_or("")).unwrap_or_else(|_| "\"\"".into());
    let txt = serde_json::to_string(text.unwrap_or("")).unwrap_or_else(|_| "\"\"".into());
    CLICK_JS.replace("__SEL__", &sel).replace("__TXT__", &txt)
}

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

/// The automation browser's launch line. Hermetically pinned by tests:
/// never pass `--remote-debugging-pipe` beside the port flag (Chrome
/// would prefer the pipe and the DevTools endpoint never opens), and
/// always allow unmuted autoplay (fresh automation profiles have zero
/// Media Engagement Index, so sites like YouTube refuse to autoplay
/// without the policy flag).
fn launch_args(port: u16, headless: bool, profile: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec![
        format!("--remote-debugging-port={port}"),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--autoplay-policy=no-user-gesture-required".into(),
        "--window-size=1280,800".into(),
    ];
    if headless {
        args.push("--headless".into());
    }
    args
}

/// Pick a free port, spawn the automation-dedicated browser and wait for
/// its DevTools discovery endpoint to answer.
fn spawn_browser(
    exe: &Path,
    headless: bool,
) -> Result<(u16, u32, crate::llm::IsolatedChild), String> {
    let port = {        let listener =
            TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free local debug port: {e}"))?;
        listener
            .local_addr()
            .map_err(|e| format!("no free local debug port: {e}"))?
            .port()
    };
    let profile = profile_dir();
    std::fs::create_dir_all(&profile)
        .map_err(|e| format!("could not create the browser profile dir: {e}"))?;

    let mut args = launch_args(port, headless, &profile);
    args.push("about:blank".into());

    /* Isolated spawn: a plain std::process::Command sets
     * bInheritHandles=TRUE with no handle list, so a browser launched
     * under a redirected shell keeps the caller's pipe write-ends alive
     * after the app exits and the shell hangs waiting for EOF (observed
     * 392 s on the example.com probe). The shared isolated spawn passes
     * only NUL/NUL/log to the child. */
    let log = std::env::temp_dir().join("igris-browser-launch-stderr.log");
    let child = crate::llm::spawn_isolated(exe, &args, &log)
        .map_err(|e| format!("could not launch {}: {e}", exe.display()))?;
    let pid = child.pid();

    let deadline = Instant::now() + DISCOVERY_BUDGET;
    loop {
        if ws_url(port).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            child.kill();
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

pub(crate) fn run_browser_open_and_click(args: &Value) -> Result<String, String> {
    let url = args
        .get("url")
        .and_then(Value::as_str)
        .ok_or_else(|| "browser_open_and_click needs a url (http or https)".to_string())?;
    validate_open_url(url)?;
    let selector = args
        .get("selector")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if selector.is_none() && text.is_none() {
        return Err(
            "browser_open_and_click needs text (what to click once the page \
             is open) or a selector"
                .into(),
        );
    }
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
    sess.wait_ready(deadline);

    // The click: same one-shot evaluate browser_click uses, then a settle
    // read so the model learns what the click landed on.
    let reply = match sess.call(
        "Runtime.evaluate",
        json!({ "expression": click_js(selector, text), "returnByValue": true }),
        Instant::now() + CLICK_BUDGET,
    ) {
        Ok(v) => v,
        Err(e) => {
            if !e.starts_with("CDP ") {
                *cell = Some(sess);
            }
            return Err(e);
        }
    };
    if let Some(d) = reply.get("exceptionDetails") {
        *cell = Some(sess);
        let msg = d["exception"]["description"]
            .as_str()
            .or_else(|| d["text"].as_str())
            .unwrap_or("unknown script error");
        return Err(format!("click script failed: {msg}"));
    }
    let payload = reply["result"]["value"].as_str().unwrap_or("");
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    if v["found"].as_bool() != Some(true) {
        // Best-effort page state so the model can see WHERE it looked.
        let where_at = sess
            .page_info(Instant::now() + Duration::from_secs(3))
            .map(|(t, u, _)| format!("{} ({u})", if t.is_empty() { "(untitled)" } else { &t }))
            .unwrap_or_else(|_| "(unknown page)".into());
        *cell = Some(sess);
        let what = selector
            .map(|s| format!("selector '{s}'"))
            .or_else(|| text.map(|t| format!("text '{t}'")))
            .unwrap_or_default();
        return Err(format!(
            "opened {url} but nothing matched {what} on page {where_at} - nothing was clicked"
        ));
    }

    std::thread::sleep(Duration::from_millis(800));
    let after = match sess.page_info(Instant::now() + INFO_BUDGET) {
        Ok(info) => Some(info),
        Err(_) => {
            std::thread::sleep(Duration::from_millis(500));
            sess.page_info(Instant::now() + INFO_BUDGET).ok()
        }
    };
    *cell = Some(sess);

    let mode = if headless { "headless" } else { "headed" };
    let tag = v["tag"].as_str().unwrap_or("?");
    let clicked = v["text"].as_str().unwrap_or_default();
    let mut out = format!(
        "Opened {url} in {name} ({mode}) and clicked: <{tag}> \"{clicked}\""
    );
    if let Some(href) = v["href"].as_str().filter(|h| !h.is_empty()) {
        out.push_str(&format!("\nLink: {href}"));
    }
    if let Some((title, url2, _)) = after {
        if !url2.is_empty() && url2 != url {
            out.push_str(&format!(
                "\nPage now: {} ({url2})",
                if title.is_empty() { "(untitled)" } else { &title }
            ));
        } else if !title.is_empty() {
            out.push_str(&format!("\nPage: {title}"));
        }
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

pub(crate) fn run_browser_click(args: &Value) -> Result<String, String> {
    let selector = args
        .get("selector")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if selector.is_none() && text.is_none() {
        return Err("browser_click needs a css selector or text to find".into());
    }

    let mut cell = session_cell().lock().unwrap();
    let Some(mut sess) = cell.take() else {
        return Err("no browser session is open - call browser_open first".into());
    };

    // The URL before the click, so the post-click read can tell the
    // model whether navigation happened (a click often opens a new page).
    let before_url = match sess.page_info(Instant::now() + Duration::from_secs(3)) {
        Ok((_, url, _)) => url,
        Err(e) => {
            if !e.starts_with("CDP ") {
                *cell = Some(sess);
            }
            return Err(e);
        }
    };

    let js = click_js(selector, text);
    let deadline = Instant::now() + CLICK_BUDGET;
    let reply = match sess.call(
        "Runtime.evaluate",
        json!({ "expression": js, "returnByValue": true }),
        deadline,
    ) {
        Ok(v) => v,
        Err(e) => {
            if !e.starts_with("CDP ") {
                *cell = Some(sess);
            }
            return Err(e);
        }
    };
    if let Some(d) = reply.get("exceptionDetails") {
        *cell = Some(sess);
        let msg = d["exception"]["description"]
            .as_str()
            .or_else(|| d["text"].as_str())
            .unwrap_or("unknown script error");
        return Err(format!("click script failed: {msg}"));
    }

    let payload = reply["result"]["value"].as_str().unwrap_or("");
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    if v["found"].as_bool() != Some(true) {
        let where_at = sess
            .page_info(Instant::now() + Duration::from_secs(3))
            .map(|(t, u, _)| format!("{} ({u})", if t.is_empty() { "(untitled)" } else { &t }))
            .unwrap_or_else(|_| "(unknown page)".into());
        *cell = Some(sess);
        let hint = text.map(|t| format!(" containing '{t}'")).unwrap_or_default();
        return Err(format!(
            "no clickable element matched{} on page {where_at} - nothing was clicked",
            selector
                .map(|s| format!(" selector '{s}'"))
                .or(Some(hint))
                .unwrap_or_default()
        ));
    }

    // Let SPA navigation settle, then read the page the click landed on.
    // A mid-navigation destroyed context reads as an error; retry once
    // before giving up on the follow-up info (the click itself happened).
    std::thread::sleep(Duration::from_millis(800));
    let after = match sess.page_info(Instant::now() + INFO_BUDGET) {
        Ok(info) => Some(info),
        Err(_) => {
            std::thread::sleep(Duration::from_millis(500));
            sess.page_info(Instant::now() + INFO_BUDGET).ok()
        }
    };
    *cell = Some(sess);

    let tag = v["tag"].as_str().unwrap_or("?");
    let clicked = v["text"].as_str().unwrap_or_default();
    let mut out = format!("Clicked: <{tag}> \"{clicked}\"");
    if let Some(href) = v["href"].as_str().filter(|h| !h.is_empty()) {
        out.push_str(&format!("\nLink: {href}"));
    }
    if let Some((title, url, _)) = after {
        if url != before_url && !url.is_empty() {
            out.push_str(&format!("\nPage now: {} ({url})", if title.is_empty() { "(untitled)" } else { &title }));
        } else if !title.is_empty() {
            out.push_str(&format!("\nPage: {title}"));
        }
    }
    Ok(out)
}

/// Click a UI element by its screen position, located by Mirage from a
/// screenshot. This replaces fragile CSS selectors for pages whose markup
/// drifts (YouTube), and always reports whether navigation actually happened.
pub(crate) fn run_browser_vision_click(args: &Value) -> Result<String, String> {
    let query = args
        .get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("first result");
    let direct = match (
        args.get("x").and_then(Value::as_f64),
        args.get("y").and_then(Value::as_f64),
    ) {
        (Some(x), Some(y)) => Some((x, y)),
        _ => None,
    };

    let mut cell = session_cell().lock().unwrap();
    let Some(mut sess) = cell.take() else {
        return Err("no browser session is open - call browser_open first".into());
    };
    let before_url = sess
        .page_info(Instant::now() + Duration::from_secs(3))
        .map(|(_, u, _)| u)
        .unwrap_or_default();

    let shot = std::env::temp_dir().join("igris-browser-vision.png");
    let result: Result<String, String> = (|| {
        sess.screenshot(&shot, Instant::now() + Duration::from_secs(6))?;
        let (x, y, label) = if let Some((x, y)) = direct {
            (x, y, "provided coordinates".to_string())
        } else {
            let loc = crate::vision::locate_in_image(&shot, query)?;
            (loc.x, loc.y, format!("{} (score {:.2})", loc.name, loc.score))
        };
        sess.click_xy(x, y, Instant::now() + Duration::from_secs(6))?;
        std::thread::sleep(Duration::from_millis(900));
        let after = sess
            .page_info(Instant::now() + INFO_BUDGET)
            .ok();
        let mut out = format!("Vision click at ({:.0}, {:.0}) on {label}.", x, y);
        match after {
            Some((title, url, _)) if !url.is_empty() && url != before_url => {
                out.push_str(&format!(
                    "\nOPENED: {} ({url})",
                    if title.is_empty() { "(untitled)" } else { &title }
                ));
            }
            Some((title, url, _)) => {
                out.push_str(&format!(
                    "\nNOT OPENED - still on {}{}",
                    if title.is_empty() { "(untitled)" } else { &title },
                    if url.is_empty() { String::new() } else { format!(" ({url})") }
                ));
            }
            None => out.push_str("\nRESULT UNKNOWN - page did not answer after click"),
        }
        Ok(out)
    })();

    if let Err(e) = &result
        && e.starts_with("CDP ")
    {
        // Transport died; do not put the session back.
    } else {
        *cell = Some(sess);
    }
    let _ = std::fs::remove_file(&shot);
    result
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
fn ensure_dead(mut child: Option<&mut crate::llm::IsolatedChild>, pid: u32) {
    if let Some(c) = child.as_mut() {
        for _ in 0..30 {
            if c.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Some(c) = child {
        c.kill();
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
    fn launch_args_carry_the_cdp_and_autoplay_policies() {
        let args = launch_args(9223, true, Path::new("C:\\p"));
        assert!(args.iter().any(|a| a.contains("--remote-debugging-port=9223")));
        assert!(args.iter().any(|a| a.contains("user-data-dir")));
        // Chrome prefers the pipe when both flags are present and then the
        // DevTools endpoint never opens - the port flag must stand alone.
        assert!(args.iter().all(|a| !a.contains("remote-debugging-pipe")));
        // Fresh automation profiles have zero Media Engagement Index.
        assert!(args
            .iter()
            .any(|a| a.contains("--autoplay-policy=no-user-gesture-required")));
        assert!(args.iter().any(|a| a.contains("--headless")));
        assert!(!launch_args(9223, false, Path::new("C:\\p"))
            .iter()
            .any(|a| a.contains("--headless")));
    }

    #[test]
    fn browser_click_validates_arguments_before_any_session() {
        let err = run_browser_click(&json!({})).unwrap_err();
        assert!(err.contains("selector"), "{err}");
        assert!(run_browser_click(&json!({ "selector": "   " })).is_err());
        assert!(run_browser_click(&json!({ "text": "" })).is_err());
        assert!(run_browser_click(&json!({ "selector": "a", "text": "x" }))
            .unwrap_err()
            .contains("browser_open"));
    }

    #[test]
    fn browser_open_and_click_validates_arguments_before_any_session() {
        assert!(run_browser_open_and_click(&json!({}))
            .unwrap_err()
            .contains("url"));
        assert!(run_browser_open_and_click(&json!({ "url": "https://example.com" }))
            .unwrap_err()
            .contains("text"));
        assert!(run_browser_open_and_click(&json!({
            "url": "https://www.youtube.com/results?search_query=x",
            "text": "x",
            "browser": "firefox"
        }))
        .unwrap_err()
        .contains("DevTools protocol"));
        assert!(run_browser_open_and_click(&json!({
            "url": "file:///etc/passwd",
            "text": "x"
        }))
        .is_err());
        /* A selector alone is a valid click target, but every valid-arg
         * call proceeds to session work - the live success path, which
         * hermetic tests must never drive. Only invalid-arg rejections
         * are asserted here. */
    }

    #[test]
    fn click_js_embeds_arguments_as_json_literals() {
        let js = click_js(Some("a#video-title"), Some("ra\"b <x>"));
        assert!(!js.contains("__SEL__") && !js.contains("__TXT__"), "{js}");
        assert!(js.contains(r#""a#video-title""#), "{js}");
        // Quotes and angle brackets in caller input must arrive escaped,
        // so they can never break out of the string literal.
        assert!(js.contains(r#""ra\"b <x>""#), "{js}");
        assert!(js.contains("querySelectorAll(sel)") && js.contains("el.click()"), "{js}");
        let js2 = click_js(None, Some("hello"));
        assert!(js2.contains(r#""hello""#), "{js2}");
        assert_eq!(click_js(None, None), CLICK_JS.replace("__SEL__", "\"\"").replace("__TXT__", "\"\""));
    }

    #[test]
    fn browser_read_click_and_close_are_honest_without_a_session() {
        assert!(run_browser_read(&json!({})).unwrap_err().contains("browser_open"));
        assert!(run_browser_click(&json!({ "text": "play" }))
            .unwrap_err()
            .contains("browser_open"));
        assert!(run_browser_close(&json!({})).is_ok());
    }
}
