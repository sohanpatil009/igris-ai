//! Web search via real APIs (Week 3.1).
//!
//! Order of preference: Brave Search (`BRAVE_SEARCH_API_KEY`), then Tavily
//! (`TAVILY_API_KEY`). If neither key is present, fall back to opening a
//! browser search (handled by the caller).

use serde_json::Value;

#[derive(Debug)]
pub enum SearchError {
    NoKey,
    Http(String),
    Parse(String),
    Timeout,
}

pub fn search_summary(query: &str, count: usize) -> Result<String, SearchError> {
    if let Ok(key) = std::env::var("BRAVE_SEARCH_API_KEY") {
        if !key.trim().is_empty() {
            return brave(query, count, key.trim());
        }
    }
    if let Ok(key) = std::env::var("TAVILY_API_KEY") {
        if !key.trim().is_empty() {
            return tavily(query, count, key.trim());
        }
    }
    Err(SearchError::NoKey)
}

fn brave(query: &str, count: usize, key: &str) -> Result<String, SearchError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| SearchError::Http(e.to_string()))?;
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding_simple(query),
        count
    );
    let resp = client
        .get(&url)
        .header("X-Subscription-Token", key)
        .header("Accept", "application/json")
        .send()
        .map_err(|e| SearchError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SearchError::Http(format!("brave HTTP {}", resp.status())));
    }
    let v: Value = resp.json().map_err(|e| SearchError::Parse(e.to_string()))?;
    let mut out = String::new();
    if let Some(results) = v
        .get("web")
        .and_then(|x| x.get("results"))
        .and_then(|x| x.as_array())
    {
        for r in results.iter().take(count) {
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            let url = r.get("url").and_then(Value::as_str).unwrap_or("");
            let snip = r.get("description").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("• {title}\n  {url}\n  {snip}\n"));
        }
    }
    if out.is_empty() {
        Err(SearchError::Parse("no results".to_string()))
    } else {
        Ok(out)
    }
}

fn tavily(query: &str, count: usize, key: &str) -> Result<String, SearchError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| SearchError::Http(e.to_string()))?;
    let payload = serde_json::json!({
        "api_key": key,
        "query": query,
        "search_depth": "basic",
        "max_results": count
    });
    let resp = client
        .post("https://api.tavily.com/search")
        .json(&payload)
        .send()
        .map_err(|e| SearchError::Http(e.to_string()))?;
    if !resp.status().is_success() {
        return Err(SearchError::Http(format!("tavily HTTP {}", resp.status())));
    }
    let v: Value = resp.json().map_err(|e| SearchError::Parse(e.to_string()))?;
    let mut out = String::new();
    if let Some(results) = v.get("results").and_then(Value::as_array) {
        for r in results.iter().take(count) {
            let title = r.get("title").and_then(Value::as_str).unwrap_or("");
            let url = r.get("url").and_then(Value::as_str).unwrap_or("");
            let snip = r.get("content").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("• {title}\n  {url}\n  {snip}\n"));
        }
    }
    if out.is_empty() {
        Err(SearchError::Parse("no results".to_string()))
    } else {
        Ok(out)
    }
}

fn urlencoding_simple(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}
