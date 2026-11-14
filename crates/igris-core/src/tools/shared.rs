//! Argument/path/format helpers shared across tool families.

use serde_json::Value;

/* ================================================================ */
/* shared path helpers (v1.1 file tools)                             */
/* ================================================================ */

/// The user profile directory ("~"). Falls back to "." so the file tools
/// still behave on exotic setups instead of panicking.
pub(crate) fn home_dir_string() -> String {
    std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into())
}

/// Expand a leading `~` (or `~/`, `~\`) to the user profile directory.
/// Windows APIs accept `/` separators, so the rest is left untouched.
pub(crate) fn expand_home_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed == "~" {
        return home_dir_string();
    }
    if let Some(rest) = trimmed
        .strip_prefix("~/")
        .or_else(|| trimmed.strip_prefix("~\\"))
    {
        return format!("{}\\{}", home_dir_string(), rest);
    }
    trimmed.to_string()
}

/// Pull a required string argument out of the tool-call JSON.
pub(crate) fn required_str<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing required parameter: {key} (tool {tool})"))
}

/// Pull an optional integer argument with a clamped fallback.
pub(crate) fn optional_u64(args: &Value, key: &str, default: u64, lo: u64, hi: u64) -> u64 {
    args.get(key)
        .and_then(|v| v.as_u64())
        .unwrap_or(default)
        .clamp(lo, hi)
}


/// Percent-encode bytes that would break a URL. Values are restricted
/// to RFC 3986 unreserved characters — separators like '&' or '='
/// inside a VALUE must be encoded so they cannot split the query
/// string; template punctuation outside the values is never touched.
/// (Everything the encoder removes is also rejected by
/// `validate_open_url`, so the final URL is always browser-safe.)
pub(crate) fn url_encode_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for byte in s.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}


/// Human byte size ("12 B", "3.4 KB", "5.6 MB", "2.1 GB").
pub(crate) fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n < K {
        format!("{n} B")
    } else if n < K * K {
        format!("{:.1} KB", n / K)
    } else if n < K * K * K {
        format!("{:.1} MB", n / (K * K))
    } else {
        format!("{:.1} GB", n / (K * K * K))
    }
}


/// Shared output ceiling: cap by chars, append a truncation marker.
pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    let total = text.chars().count();
    if total <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str(&format!(
        "\n[truncated: showing first {max_chars} of {total} characters]"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_path_expands_tilde_only() {
        let home = home_dir_string();
        assert_eq!(expand_home_path("~"), home);
        assert_eq!(expand_home_path("~/Documents"), format!("{}\\Documents", home));
        assert_eq!(expand_home_path("~\\notes"), format!("{}\\notes", home));
        assert_eq!(expand_home_path("C:\\plain\\path"), "C:\\plain\\path");
        assert_eq!(expand_home_path(" relative/x "), "relative/x");
    }

    /* ---- file_search ---- */

}
