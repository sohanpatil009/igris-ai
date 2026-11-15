//! Web tools (v2, goal.md sections 5/6): web_fetch (size-capped page
//! reader), download_file (streamed save into the user's Downloads
//! folder only), web_search (DuckDuckGo "lite" HTML backend, no API
//! key). Shared here: the ureq agent — hardened so no request hop can
//! ever target a local/private host (BUG-001): the URL policy blocks
//! the initial literal host up front, the agent's resolver vetoes
//! EVERY hop (initial URL and each followed redirect, including
//! DNS-resolved private addresses), and the redirect history is a
//! final backstop. Plus HTML-to-text, entity decode, filename
//! sanitizing.

use crate::tools::basic::validate_open_url;
use crate::tools::shared::{fmt_bytes, optional_u64, required_str, truncate_chars, url_encode_value};
use regex::Regex;
use serde_json::Value;
use std::fs;
use std::net::IpAddr;
use std::sync::LazyLock;
use std::time::Duration;
use ureq::config::Config;
use ureq::http::Uri;
use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::NextTimeout;
use ureq::{Error, ResponseExt};

/* ---------------------------------------------------------------- */
/* web shared: agent, URL policy, entity decode                      */
/* ---------------------------------------------------------------- */

/// Browser-like UA: many sites (and DDG) 403 unknown clients.
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// Web agent for the v2 browser tools — separate from the LLM server
/// agent (different timeout regime); per-request timeouts set at call
/// time so each tool controls its own budget.
///
/// Built via `Agent::with_parts` to attach the `PrivateBlockResolver`
/// veto. `DefaultConnector` is the stock TCP+TLS stack the plain
/// config-built agent gets — only the resolver differs.
static WEB_AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::Agent::with_parts(
        ureq::Agent::config_builder()
            .user_agent(WEB_USER_AGENT)
            .save_redirect_history(true)
            .build(),
        ureq::unversioned::transport::DefaultConnector::default(),
        PrivateBlockResolver,
    )
});

const WEB_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const WEB_SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
const WEB_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(90);

fn web_get(
    url: &str,
    timeout: Duration,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    validate_web_url(url)?;
    let resp = WEB_AGENT
        .get(url)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .call()
        .map_err(|e| e.to_string())?;
    /* Second line of defense (BUG-001): the history carries every hop
     * (initial, intermediates, final). If a private host slipped past
     * the resolver veto anyway, fail here — the body is never read. */
    if let Some(history) = resp.get_redirect_history() {
        if let Some(msg) = refused_history_hop(history) {
            return Err(msg);
        }
    }
    Ok(resp)
}

/// Same scheme/character policy as `validate_open_url`, plus a block on
/// loopback/private LITERAL hosts (SSRF guard: fetched-page content must
/// never be able to send the assistant after local services). Best
/// effort by hostname literal — DNS-resolution checks are out of scope.
fn validate_web_url(url: &str) -> Result<(), String> {
    validate_open_url(url)?;
    let rest = url.trim();
    let rest = rest.split_once("://").map(|(_, r)| r).unwrap_or(rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    /* strip userinfo */
    let host = authority.rsplit('@').next().unwrap_or(authority);
    if is_private_host(host) {
        return Err(format!(
            "Local/private addresses are not allowed for web tools (got '{host}')"
        ));
    }
    Ok(())
}

fn is_private_host(host: &str) -> bool {
    let h = host.trim().trim_end_matches('.').to_lowercase();
    if h.is_empty()
        || h == "localhost"
        || h == "0.0.0.0"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
    {
        return true;
    }
    /* [IPv6] or [IPv6]:port — a bare suffix(']') strip would miss ports. */
    if let Some(end) = h.find(']') {
        let inner = &h[1..end];
        let v6 = inner.split('%').next().unwrap_or(inner);
        return v6 == "::1"
            || v6.starts_with("fe8")
            || v6.starts_with("fc")
            || v6.starts_with("fd");
    }
    /* Bare host, possibly with a :port suffix — an authority like
     * "127.0.0.1:8018" keeps the port, so the port must go before the
     * IPv4 parse. Only strip an all-digit port when the prefix is a
     * dotted/IPv4-shaped address (a bracket-less IPv6 like ::1 has no
     * port and would be mangled by the split). */
    let bare = if let Some(colon) = h.rfind(':') {
        let (addr, port) = (&h[..colon], &h[colon + 1..]);
        if !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit())
            && (addr.contains('.') || addr.parse::<std::net::Ipv4Addr>().is_ok())
        {
            addr
        } else {
            h.as_str()
        }
    } else {
        h.as_str()
    };
    if let Ok(ip) = bare.parse::<std::net::Ipv4Addr>() {
        return ip.is_loopback()
            || ip.is_private()
            || ip.is_link_local()
            || ip.is_broadcast()
            || ip.is_unspecified();
    }
    /* Bracket-less IPv6 loopback (invalid in a URL authority, but cheap
     * to catch; no legal DNS name contains a colon). */
    bare.starts_with("::1")
}

/* ---------------------------------------------------------------- */
/* BUG-001: private-host veto at the resolver layer                  */
/* ---------------------------------------------------------------- */

/// Literal-host veto decision for one request-hop URI: Some(refusal)
/// when the host is local/private. Pure so tests can pin the exact
/// decision the resolver applies to every hop.
fn private_host_refusal(uri: &Uri) -> Option<String> {
    let host = uri.authority()?.host();
    if is_private_host(host) {
        Some(format!("refusing local/private address '{host}'"))
    } else {
        None
    }
}

/// IP-level mirror of `is_private_host`'s address policy, applied to
/// RESOLVED socket addresses (the literal path cannot see these).
/// Parity set: loopback, RFC1918 private, link-local, broadcast,
/// unspecified — plus IPv6 loopback, ULA (fc00::/7), link-local
/// (fe80::/10) and v4-mapped forms. CGNAT 100.64/10 and TEST-NET
/// documentation ranges stay allowed (documented trade-off).
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_private_ip(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xfe00) == 0xfc00
                || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Resolver for `WEB_AGENT` (BUG-001): ureq re-resolves the URI of
/// every request hop — the initial URL and each followed redirect
/// (`call_run` resolves per loop iteration) — so vetoing here refuses
/// a redirect to a local/private target BEFORE any socket opens, and
/// no hand-rolled Location handling is needed (ureq's own redirect
/// machinery stays authoritative). Two layers:
///
/// 1. the literal authority host (`is_private_host`) — refused with no
///    DNS lookup at all;
/// 2. the RESOLVED addresses (DNS-rebinding backstop: a public-looking
///    name that resolves into a private range) — private addrs are
///    dropped, and the hop fails closed if nothing public remains.
///
/// Scope note: `WEB_AGENT` runs proxy-less (a proxy configured with
/// remote resolution would skip the resolver) and `DefaultConnector`
/// supplies the same stock stack the plain config-built agent gets.
#[derive(Debug)]
struct PrivateBlockResolver;

impl Resolver for PrivateBlockResolver {
    fn resolve(
        &self,
        uri: &Uri,
        config: &Config,
        timeout: NextTimeout,
    ) -> Result<ResolvedSocketAddrs, Error> {
        if let Some(msg) = private_host_refusal(uri) {
            return Err(Error::BadUri(msg));
        }
        let addrs = DefaultResolver::default().resolve(uri, config, timeout)?;
        let mut kept = self.empty();
        let mut seen_private: Option<String> = None;
        for addr in &addrs {
            if is_private_ip(addr.ip()) {
                seen_private.get_or_insert_with(|| addr.ip().to_string());
                continue;
            }
            kept.push(*addr);
        }
        if kept.is_empty() {
            let host = uri.authority().map(|a| a.host()).unwrap_or("");
            return Err(Error::BadUri(format!(
                "refusing local/private address '{host}' (resolved to {})",
                seen_private.unwrap_or_default()
            )));
        }
        Ok(kept)
    }
}

/// Backstop over `get_redirect_history` (BUG-001 second line of
/// defense): with `save_redirect_history(true)` the final response
/// carries every hop — initial, intermediates, final. If a private
/// host appears anywhere in the chain despite the resolver veto, the
/// whole fetch fails before the body is read.
fn refused_history_hop(history: &[Uri]) -> Option<String> {
    history.iter().find_map(|uri| {
        let host = uri.authority().map(|a| a.host()).unwrap_or("");
        if is_private_host(host) {
            Some(format!("redirect passed local/private address '{host}'"))
        } else {
            None
        }
    })
}

/// Decode common HTML entities found in page/search text (named + decimal
/// + hex numeric). Unknown or malformed entities pass through unchanged.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest.find(';').filter(|&i| i <= 10).map(|i| &rest[..=i]);
        match entity {
            Some(e) => {
                match e {
                    "&amp;" => out.push('&'),
                    "&lt;" => out.push('\u{3c}'),
                    "&gt;" => out.push('\u{3e}'),
                    "&quot;" => out.push('"'),
                    "&apos;" | "&#39;" => out.push('\''),
                    "&nbsp;" => out.push(' '),
                    other => {
                        if let Some(num) = other.strip_prefix("&#") {
                            /* `other` still carries the terminating ';' */
                            let num = num.strip_suffix(';').unwrap_or(num);
                            let decoded = if let Some(hex) =
                                num.strip_prefix('x').or_else(|| num.strip_prefix('X'))
                            {
                                u32::from_str_radix(hex, 16).ok()
                            } else {
                                num.parse::<u32>().ok()
                            };
                            match decoded.and_then(char::from_u32) {
                                Some(c) => out.push(c),
                                None => out.push_str(other),
                            }
                        } else {
                            out.push_str(other);
                        }
                    }
                }
                rest = &rest[e.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/* ---------------------------------------------------------------- */
/* web_fetch — size-capped readable-text page reader                 */
/* ---------------------------------------------------------------- */

/// Hard byte cap on the response body actually read.
const FETCH_MAX_BYTES: usize = 512 * 1024;
/// Ceiling fed to the model after conversion.
const FETCH_MAX_CHARS: usize = 6_000;

static HTML_COMMENT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)\x3c!--.*?--\x3e").expect("static regex"));
static HTML_SCRIPT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\x3cscript\b[^>]*>.*?\x3c/script\x3e"#).expect("static regex")
});
static HTML_STYLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)\x3cstyle\b[^>]*>.*?\x3c/style\x3e"#).expect("static regex")
});
static HTML_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)\x3c(?:br\b[^>]*|/(?:p|div|h[1-6]|li|tr|td|table|ul|ol|blockquote|",
        r"section|article|header|footer|nav|main|pre)\b[^>]*)\x3e"
    ))
    .expect("static regex")
});
static HTML_TAG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x3c[^>]*\x3e").expect("static regex"));
static HTML_WS_RUN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(" {2,}").expect("static regex"));

/// Minimal HTML-to-readable-text conversion for model consumption:
/// scripts/styles/comments removed, block boundaries become newlines,
/// remaining tags stripped, entities decoded, whitespace collapsed.
fn html_to_text(html: &str) -> String {
    let s = HTML_COMMENT_RE.replace_all(html, " ");
    let s = HTML_SCRIPT_RE.replace_all(&s, " ");
    let s = HTML_STYLE_RE.replace_all(&s, " ");
    let s = HTML_BLOCK_RE.replace_all(&s, "\n");
    let s = HTML_TAG_RE.replace_all(&s, " ");
    let s = HTML_WS_RUN_RE.replace_all(&s, " ");
    let s = decode_entities(&s);
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn is_binary_content_type(ctype: &str) -> bool {
    let main = ctype.split(';').next().unwrap_or("").trim();
    if main.is_empty() {
        return false; /* unknown: try the text path, sniffed next */
    }
    !(main.starts_with("text/")
        || main.contains("json")
        || main.contains("xml")
        || main.contains("javascript")
        || main.contains("csv"))
}

pub(crate) fn run_web_fetch(args: &Value) -> Result<String, String> {
    use std::io::Read;
    let url = required_str(args, "url", "web_fetch")?;
    validate_web_url(url)?;
    let url = url.trim().to_string();
    let resp = web_get(&url, WEB_FETCH_TIMEOUT)
        .map_err(|e| format!("Could not fetch {url}: {e}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status} from {url}"));
    }
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if is_binary_content_type(&ctype) {
        return Err(format!(
            "'{url}' serves binary content ({ctype}) — web_fetch returns text; use \
             download_file for files."
        ));
    }
    /* take(cap + 1): read at most the cap, and one extra byte flags a
     * bigger page without ever tripping the reader's own limit. */
    let mut reader = resp
        .into_body()
        .into_with_config()
        .limit((FETCH_MAX_BYTES + 1) as u64)
        .reader();
    let mut buf: Vec<u8> = Vec::new();
    (&mut reader)
        .take((FETCH_MAX_BYTES + 1) as u64)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Could not read body of {url}: {e}"))?;
    let truncated = buf.len() > FETCH_MAX_BYTES;
    buf.truncate(FETCH_MAX_BYTES);
    let text = String::from_utf8_lossy(&buf);
    let readable = if ctype.contains("html") || (ctype.is_empty() && text.contains('\u{3c}')) {
        html_to_text(&text)
    } else {
        text.into_owned()
    };
    let mut out = truncate_chars(&readable, FETCH_MAX_CHARS);
    if truncated {
        out.push_str("\n[page larger than the fetch cap; content truncated]");
    }
    Ok(format!("Content of {url}:\n{out}"))
}

/* ---------------------------------------------------------------- */
/* download_file — streamed save into Downloads (fixed writable path) */
/* ---------------------------------------------------------------- */

const DOWNLOAD_MAX_BYTES: u64 = 200 * 1024 * 1024;

/// Path-safety: no separators, no traversal, no reserved/control chars,
/// no leading dots, sane length.
fn sanitize_filename(name: &str) -> String {
    let mut out: String = name
        .trim()
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '\u{3c}' | '\u{3e}' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while out.starts_with('.') {
        out.remove(0);
    }
    if out.is_empty() || out.chars().all(|c| c == '_') {
        return String::new();
    }
    out.chars().take(100).collect()
}

/// Last non-empty path segment of a URL (query/fragment stripped),
/// percent-decoded.
fn download_filename_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let segment = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
    percent_decode(segment)
}

/// Percent-decode %XX escapes (bytes preserved), everything else
/// verbatim. Lossy UTF-8 on the final join.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            /* need bytes[i+1] and bytes[i+2] */
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(b) = hex {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) fn run_download_file(args: &Value) -> Result<String, String> {
    use std::io::{Read, Write};
    let url = required_str(args, "url", "download_file")?;
    validate_web_url(url)?;
    let url = url.trim().to_string();
    let dir = dirs::download_dir().ok_or("Could not resolve the user's Downloads folder")?;
    if !dir.is_dir() {
        return Err(format!("Downloads folder not found: {}", dir.display()));
    }
    let name = match args.get("filename").and_then(|v| v.as_str()) {
        Some(f) => {
            let s = sanitize_filename(f);
            if s.is_empty() {
                return Err(format!("Invalid filename '{f}'"));
            }
            s
        }
        None => {
            let derived = sanitize_filename(&download_filename_from_url(&url));
            if derived.is_empty() {
                return Err(
                    "Could not derive a filename from the URL — pass a filename".into()
                );
            }
            derived
        }
    };
    let target = dir.join(&name);
    if target.exists() {
        return Err(format!(
            "'{name}' already exists in Downloads — choose a different filename"
        ));
    }
    let resp = web_get(&url, WEB_DOWNLOAD_TIMEOUT)
        .map_err(|e| format!("Could not fetch {url}: {e}"))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(format!("HTTP {status} from {url}"));
    }
    let ctype = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if ctype.starts_with("text/html") {
        return Err(format!(
            "'{url}' returned a webpage (HTML), not a downloadable file — it may be an \
             error page or a link to a viewer page."
        ));
    }
    if let Some(len) = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > DOWNLOAD_MAX_BYTES {
            return Err(format!(
                "File is {len} bytes; download limit is {DOWNLOAD_MAX_BYTES}"
            ));
        }
    }
    /* LimitReader at cap+1: any server trying to exceed the cap errors
     * out and the partial file is deleted — never a runaway disk write. */
    let mut reader = resp
        .into_body()
        .into_with_config()
        .limit(DOWNLOAD_MAX_BYTES + 1)
        .reader();
    let mut file = fs::File::create(&target)
        .map_err(|e| format!("Could not create {}: {e}", target.display()))?;
    let mut total: u64 = 0;
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                if total > DOWNLOAD_MAX_BYTES {
                    drop(file);
                    let _ = fs::remove_file(&target);
                    return Err(format!(
                        "File exceeds the {DOWNLOAD_MAX_BYTES} byte download limit; aborted"
                    ));
                }
                file.write_all(&chunk[..n])
                    .map_err(|e| format!("Write failed: {e}"))?;
            }
            Err(e) => {
                drop(file);
                let _ = fs::remove_file(&target);
                return Err(format!("Download failed: {e}"));
            }
        }
    }
    let _ = file.flush();
    Ok(format!(
        "Downloaded {name} ({}) to {}",
        fmt_bytes(total),
        target.display()
    ))
}

/* ---------------------------------------------------------------- */
/* web_search — DuckDuckGo "lite" HTML backend (no API key)          */
/* ---------------------------------------------------------------- */

const SEARCH_RESULTS_DEFAULT: u64 = 8;
const SEARCH_RESULTS_MAX: u64 = 15;
const SEARCH_PAGE_MAX_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct WebResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Clean a fragment (title/snippet): strip inline tags, decode entities,
/// collapse whitespace.
fn clean_html_fragment(s: &str) -> String {
    let stripped = HTML_TAG_RE.replace_all(s, " ");
    let decoded = decode_entities(&stripped);
    HTML_WS_RUN_RE
        .replace_all(&decoded, " ")
        .trim()
        .to_string()
}

/// `//duckduckgo.com/l/?uddg=<percent-encoded-target>&rut=…` redirect
/// wrappers carry the real target in the uddg parameter.
fn unwrap_ddg_redirect(href: &str) -> String {
    let h = href.trim();
    for prefix in [
        "//duckduckgo.com/l/?uddg=",
        "https://duckduckgo.com/l/?uddg=",
        "http://duckduckgo.com/l/?uddg=",
    ] {
        if let Some(rest) = h.strip_prefix(prefix) {
            let end = rest.find('&').unwrap_or(rest.len());
            return percent_decode(&rest[..end]);
        }
    }
    if let Some(rest) = h.strip_prefix("//") {
        return format!("https://{rest}");
    }
    h.to_string()
}

/// Parse the DuckDuckGo "lite" result page. Markup verified live
/// 2026-11-14 (probe): result titles in `class='result-link'` anchors
/// (href is the redirect wrapper above), snippets in
/// `class='result-snippet'` cells, in result order. Quote style varies,
/// so the regexes accept both. Snippets are zipped by index — a result
/// without a snippet row can shift alignment (accepted trade-off).
fn parse_ddg_lite(html: &str) -> Vec<WebResult> {
    static LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)\x3ca\b[^>]*class\s*=\s*['"]result-link['"][^>]*>(.*?)\x3c/a\x3e"#)
            .expect("static regex")
    });
    static HREF_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)\x3ca\b[^>]*href\s*=\s*['"]([^'"]+)['"]"#).expect("static regex")
    });
    static SNIP_RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r#"(?is)\x3ctd\b[^>]*class\s*=\s*['"]result-snippet['"][^>]*>(.*?)\x3c/td\x3e"#)
            .expect("static regex")
    });
    let mut results: Vec<WebResult> = Vec::new();
    for m in LINK_RE.find_iter(html) {
        let tag_and_text = m.as_str();
        let url = HREF_RE
            .captures(tag_and_text)
            .and_then(|c| c.get(1))
            .map(|h| unwrap_ddg_redirect(h.as_str()))
            .unwrap_or_default();
        if url.is_empty() {
            continue;
        }
        let title = LINK_RE
            .captures(tag_and_text)
            .and_then(|c| c.get(1))
            .map(|t| clean_html_fragment(t.as_str()))
            .unwrap_or_default();
        results.push(WebResult {
            title,
            url,
            snippet: String::new(),
        });
    }
    let snippets: Vec<String> = SNIP_RE
        .captures_iter(html)
        .filter_map(|c| c.get(1))
        .map(|c| clean_html_fragment(c.as_str()))
        .collect();
    for (i, r) in results.iter_mut().enumerate() {
        if let Some(s) = snippets.get(i) {
            r.snippet = s.clone();
        }
    }
    results
}

pub(crate) fn run_web_search(args: &Value) -> Result<String, String> {
    let query = required_str(args, "query", "web_search")?;
    let query = query.trim();
    if query.is_empty() {
        return Err("query is empty".into());
    }
    let limit = optional_u64(
        args,
        "max_results",
        SEARCH_RESULTS_DEFAULT,
        1,
        SEARCH_RESULTS_MAX,
    ) as usize;
    let url = format!(
        "https://lite.duckduckgo.com/lite/?q={}",
        url_encode_value(query)
    );
    let resp = web_get(&url, WEB_SEARCH_TIMEOUT)
        .map_err(|e| format!("Search failed: {e}"))?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(format!(
            "DuckDuckGo returned HTTP {status} — the search backend may be \
             rate-limiting; try again shortly."
        ));
    }
    let html = resp
        .into_body()
        .into_with_config()
        .limit(SEARCH_PAGE_MAX_BYTES as u64)
        .read_to_string()
        .map_err(|e| format!("Could not read search page: {e}"))?;
    let results = parse_ddg_lite(&html);
    if results.is_empty() {
        return Err(
            "DuckDuckGo returned no parseable results (markup change or bot challenge) — \
             try rephrasing the query."
                .into(),
        );
    }
    let shown = results.len().min(limit);
    let mut out = format!("Top {shown} web results for '{query}':");
    for (i, r) in results.iter().take(shown).enumerate() {
        out.push_str(&format!("\n{}. {}\n   {}", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            out.push_str(&format!("\n   {}", r.snippet));
        }
    }
    Ok(out)
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testkit::reg;
    use serde_json::json;

    /* ---- shared v2 helpers ---- */

    #[test]
    fn percent_decode_sanitize_and_format_helpers() {
        assert_eq!(
            percent_decode("https%3A%2F%2Frust%2Dlang.org%2F"),
            "https://rust-lang.org/"
        );
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%E2%82%AC"), "\u{20AC}");
        assert_eq!(percent_decode("100%"), "100%"); /* dangling escape passes through */
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(sanitize_filename("a\u{3c}b\u{3e}:x?.txt"), "a_b__x_.txt");
        assert_eq!(sanitize_filename("../x"), "_x");
        assert_eq!(sanitize_filename("   "), "");
        assert_eq!(sanitize_filename("___"), "");
        assert_eq!(
            download_filename_from_url("https://x.com/files/report%202026.pdf?dl=1"),
            "report 2026.pdf"
        );
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KB");
        assert_eq!(fmt_bytes(5_600_000), "5.3 MB");
        assert_eq!(truncate_chars("hello", 10), "hello");
        let t = truncate_chars("hello world", 5);
        assert!(t.starts_with("hello") && t.contains("[truncated"), "{t}");
    }

    /* ---- web tools: pure helpers + validation (live network is e2e) ---- */

    #[test]
    fn web_urls_block_private_hosts_and_non_http() {
        for bad in [
            "http://127.0.0.1:8018/health",
            "https://localhost/x",
            "http://10.0.0.5/x",
            "http://192.168.1.10/x",
            "http://172.16.0.1/x",
            "http://169.254.169.254/metadata",
            "http://[::1]:8080/x",
            "http://machine.local/x",
            "ftp://example.com/file",
        ] {
            assert!(validate_web_url(bad).is_err(), "must reject {bad}");
        }
        validate_web_url("https://example.com/page?q=1").unwrap();
        validate_web_url("http://example.com").unwrap();
    }

    #[test]
    fn decode_entities_named_decimal_hex_and_unknown() {
        assert_eq!(decode_entities("a&amp;b"), "a&b");
        assert_eq!(decode_entities("&lt;tag&gt;"), "\u{3c}tag\u{3e}");
        assert_eq!(decode_entities("&quot;hi&quot;"), "\"hi\"");
        assert_eq!(decode_entities("it&#39;s"), "it's");
        assert_eq!(decode_entities("a&nbsp;b"), "a b");
        assert_eq!(decode_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_entities("&unknown;"), "&unknown;");
        assert_eq!(decode_entities("100% & this"), "100% & this");
    }

    /// Angle tags written as escapes — no raw tag text in source.
    const HTML_FIXTURE: &str = "\x3chtml\x3e\x3chead\x3e\x3ctitle\x3eMy Page\x3c/title\x3e\
\x3cstyle\x3e.x{color:red}\x3c/style\x3e\x3c/head\x3e\
\x3cbody\x3e\x3c!-- hidden --\x3e\x3ch1\x3eHello\x3c/h1\x3e\
\x3cp\x3eFirst&nbsp;paragraph with \x3cb\x3ebold\x3c/b\x3e &amp; entities.\x3c/p\x3e\
\x3cscript\x3ealert('no');\x3c/script\x3e\
\x3cul\x3e\x3cli\x3eone\x3c/li\x3e\x3cli\x3etwo\x3c/li\x3e\x3c/ul\x3e\x3c/body\x3e\x3c/html\x3e";

    #[test]
    fn html_to_text_strips_noise_keeps_readable_text() {
        let text = html_to_text(HTML_FIXTURE);
        assert!(text.contains("My Page"), "{text}");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("First paragraph with bold & entities."), "{text}");
        assert!(text.contains("one") && text.contains("two"), "{text}");
        assert!(!text.contains("alert"), "{text}");
        assert!(!text.contains("color:red"), "{text}");
        assert!(!text.contains("hidden"), "{text}");
    }

    #[test]
    fn binary_content_type_classification() {
        assert!(is_binary_content_type("image/png"));
        assert!(is_binary_content_type("application/pdf"));
        assert!(is_binary_content_type("application/octet-stream"));
        assert!(!is_binary_content_type("text/html; charset=utf-8"));
        assert!(!is_binary_content_type("application/json"));
        assert!(!is_binary_content_type("application/xhtml+xml"));
        assert!(!is_binary_content_type(""));
    }

    /* Real markup shape captured live from lite.duckduckgo.com (probe,
     * 2026-11-14): redirect-wrapped hrefs with percent-encoded targets,
     * single-quoted classes, snippet cells in result order. */
    const DDG_FIXTURE: &str = "\x3ctable\x3e\
\x3ctr\x3e\x3ctd\x3e\x3ca rel='nofollow' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Frust%2Dlang.org%2F&amp;rut=abc123' class='result-link'\x3eRust \x3cb\x3eProgramming\x3c/b\x3e Language\x3c/a\x3e\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e&nbsp;&nbsp;&nbsp;\x3c/td\x3e\x3ctd class='result-snippet'\x3eA language empowering everyone to build reliable &amp; efficient software.\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e\x3cspan class='link-text'\x3erust-lang.org\x3c/span\x3e\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e\x3ca rel='nofollow' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2F&amp;rut=def456' class='result-link'\x3eThe Rustdoc book\x3c/a\x3e\x3c/td\x3e\x3c/tr\x3e\
\x3ctr\x3e\x3ctd\x3e&nbsp;&nbsp;&nbsp;\x3c/td\x3e\x3ctd class='result-snippet'\x3eLearn the language &lt;em&gt;for free&lt;/em&gt;\x3c/td\x3e\x3c/tr\x3e\
\x3c/table\x3e";

    #[test]
    fn parse_ddg_lite_extracts_results_redirects_and_snippets() {
        let results = parse_ddg_lite(DDG_FIXTURE);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert_eq!(
            results[0].snippet,
            "A language empowering everyone to build reliable & efficient software."
        );
        assert_eq!(results[1].title, "The Rustdoc book");
        assert_eq!(results[1].url, "https://doc.rust-lang.org/");
        assert_eq!(results[1].snippet, "Learn the language \u{3c}em\u{3e}for free\u{3c}/em\u{3e}");
    }

    #[test]
    fn unwrap_ddg_redirect_variants() {
        assert_eq!(
            unwrap_ddg_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fx.org%2F&rut=z"),
            "https://x.org/"
        );
        assert_eq!(
            unwrap_ddg_redirect("https://duckduckgo.com/l/?uddg=a%20b&rut=z"),
            "a b"
        );
        assert_eq!(
            unwrap_ddg_redirect("//example.com/plain"),
            "https://example.com/plain"
        );
        assert_eq!(
            unwrap_ddg_redirect("https://example.com/full"),
            "https://example.com/full"
        );
    }

    #[test]
    fn web_search_and_fetch_validation_errors() {
        let err = reg().dispatch("web_search", &json!({"query": "   "})).unwrap_err();
        assert!(err.contains("query is empty"), "{err}");
        let err = reg()
            .dispatch("web_fetch", &json!({"url": "http://127.0.0.1:8018/health"}))
            .unwrap_err();
        assert!(err.contains("Local/private"), "{err}");
        let err = reg()
            .dispatch("web_fetch", &json!({"url": "notaurl"}))
            .unwrap_err();
        assert!(err.contains("Only http"), "{err}");
        let err = reg()
            .dispatch(
                "download_file",
                &json!({"url": "https://example.com/f.pdf", "filename": "   "}),
            )
            .unwrap_err();
        assert!(err.contains("Invalid filename"), "{err}");
    }

    /* ---- BUG-001: private-host veto on every request hop ---- */

    #[test]
    fn resolved_private_ips_match_the_literal_host_policy() {
        for bad in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.9",
            "172.31.255.1",
            "192.168.0.1",
            "169.254.169.254",
            "255.255.255.255",
            "0.0.0.0",
        ] {
            let ip: IpAddr = bad.parse().unwrap();
            assert!(is_private_ip(ip), "{bad} must be refused");
        }
        /* 172.32.0.1 sits outside the private /12 — parity guard. */
        assert!(!is_private_ip("172.32.0.1".parse().unwrap()));
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        for bad6 in [
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1234",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            let ip: IpAddr = bad6.parse().unwrap();
            assert!(is_private_ip(ip), "{bad6} must be refused");
        }
        assert!(!is_private_ip("2606:4700::1111".parse().unwrap()));
        assert!(!is_private_ip("::ffff:8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn hop_veto_refuses_private_literals_and_allows_public() {
        for bad in [
            "http://127.0.0.1:8018/health",
            "https://localhost/x",
            "http://10.0.0.5/x",
            "http://192.168.1.10/x",
            "http://[::1]:8080/x",
            "http://machine.local/x",
        ] {
            let uri: Uri = bad.parse().unwrap();
            let msg = private_host_refusal(&uri).unwrap_or_else(|| panic!("must refuse {bad}"));
            assert!(msg.contains("refusing local/private"), "{msg}");
        }
        assert!(private_host_refusal(&"https://example.com/x".parse().unwrap()).is_none());
    }

    #[test]
    fn history_backstop_flags_private_hops() {
        let chain: Vec<Uri> = vec![
            "https://example.com/x".parse().unwrap(),
            "http://127.0.0.1:8018/health".parse().unwrap(),
        ];
        let msg = refused_history_hop(&chain).expect("private hop must be flagged");
        assert!(msg.contains("local/private"), "{msg}");
        let clean: Vec<Uri> = vec!["https://example.com/x".parse().unwrap()];
        assert!(refused_history_hop(&clean).is_none());
    }

    #[test]
    fn web_agent_refuses_private_hosts_before_connecting() {
        /* Drives the REAL production agent (with_parts + veto resolver):
         * ureq's own machinery surfaces the refusal. No DNS lookup and
         * no socket ever happen — the literal veto fires first. */
        let err = WEB_AGENT
            .get("http://127.0.0.1:8018/health")
            .call()
            .unwrap_err();
        assert!(err.to_string().contains("refusing local/private"), "{err}");
    }

    /// Minimal raw-HTTP responder: answers `times` connections with a
    /// canned response (request head drained first). Dependency-free
    /// stand-in for the "fetched page" and "loopback service" servers.
    fn serve_loop(listener: std::net::TcpListener, response: String, times: usize) {
        use std::io::{BufRead, Write};
        for _ in 0..times {
            let Ok((mut sock, _)) = listener.accept() else {
                break;
            };
            {
                let mut reader = std::io::BufReader::new(&sock);
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 {
                    if line.trim().is_empty() {
                        break;
                    }
                    line.clear();
                }
            }
            let _ = sock.write_all(response.as_bytes());
            let _ = sock.flush();
        }
    }

    #[test]
    fn redirect_to_loopback_is_refused_at_every_layer() {
        /* BUG-001 e2e: server A (stand-in for a fetched page) answers
         * 302 -> server B on loopback. Offline constraint: no test can
         * present a genuinely public initial host, so the layers are
         * exercised as (1) the redirect machinery itself works on this
         * fixture, (2) the per-hop veto refuses the Location target,
         * (3) the real production agent refuses the request up front.
         * The resolver re-resolves EVERY hop (ureq call_run), so (2) is
         * the exact decision that gates a redirect hop in production. */
        let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port_a = a.local_addr().unwrap().port();
        let port_b = b.local_addr().unwrap().port();
        let url_a = format!("http://127.0.0.1:{port_a}/");
        let a_resp = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port_b}/x\r\n\
             Content-Length: 0\r\nConnection: close\r\n\r\n"
        );
        let b_resp = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nHELLO";
        let ta = std::thread::spawn(move || serve_loop(a, a_resp, 2));
        let tb = std::thread::spawn(move || serve_loop(b, b_resp.to_string(), 1));
        let t = Duration::from_secs(5);

        /* (1) fixture + ureq's redirect machinery: following A->B
         * succeeds when nothing vetoes it. */
        let follow: ureq::Agent = ureq::Agent::config_builder().build().into();
        let resp = follow
            .get(&url_a)
            .config()
            .timeout_global(Some(t))
            .build()
            .call()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body = resp
            .into_body()
            .into_with_config()
            .limit(4096)
            .read_to_string()
            .unwrap();
        assert_eq!(body, "HELLO");

        /* (2) raw 302 (redirects off) -> the per-hop veto refuses the
         * Location target before any second hop could be resolved. */
        let raw: ureq::Agent = ureq::Agent::config_builder().max_redirects(0).build().into();
        let resp = raw
            .get(&url_a)
            .config()
            .timeout_global(Some(t))
            .build()
            .call()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 302);
        let loc = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let uri: Uri = loc.parse().unwrap();
        let refusal = private_host_refusal(&uri).expect("loopback redirect must be refused");
        assert!(refusal.contains("refusing local/private"), "{refusal}");

        /* (3) the REAL production agent refuses the request itself. */
        let err = WEB_AGENT.get(&url_a).call().unwrap_err();
        assert!(err.to_string().contains("refusing local/private"), "{err}");

        ta.join().unwrap();
        tb.join().unwrap();
    }
}
