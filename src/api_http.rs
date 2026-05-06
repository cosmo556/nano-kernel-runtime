#![allow(dead_code)] // Module is consumed by both bins; each sees a partial surface.

// =============================================================================
// NKR API HTTP layer — parsing, validation, wire format
// =============================================================================
//
// Dep-free module (no cell/vmm/state imports) so it can be linked by both the
// root daemon (nkr) and the unprivileged proxy (nkr-api-server).
//
// The proxy is the only consumer of `read_request` / `HttpResponse::to_wire` —
// it parses HTTP from the TCP listener, validates identifiers, marshals the
// request to an IpcRequest over UDS, then serializes the IpcResponse back into
// HTTP. The daemon does NOT speak HTTP.
// =============================================================================

use std::io::Read;

// =============================================================================
// HTTP wire types
// =============================================================================

#[allow(dead_code)]
pub struct HttpRequest<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub content_type: String,
    pub extra_headers: Vec<(String, String)>,
}

impl HttpResponse {
    pub fn json(status: u16, body: impl serde::Serialize) -> Self {
        let body = serde_json::to_string(&body).unwrap_or_else(|_| "{}".to_string());
        HttpResponse {
            status,
            body,
            content_type: "application/json".to_string(),
            extra_headers: Vec::new(),
        }
    }
    pub fn error(status: u16, err: &str, msg: Option<&str>) -> Self {
        let v = match msg {
            Some(m) => serde_json::json!({"error": err, "message": m}),
            None => serde_json::json!({"error": err}),
        };
        Self::json(status, v)
    }
    pub fn to_wire(&self) -> String {
        let reason = match self.status {
            200 => "OK",
            201 => "Created",
            202 => "Accepted",
            204 => "No Content",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            413 => "Payload Too Large",
            500 => "Internal Server Error",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "OK",
        };
        let mut extras = String::new();
        for (k, v) in &self.extra_headers {
            extras.push_str(&format!("{}: {}\r\n", k, v));
        }
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
            self.status, reason, self.content_type, self.body.len(), extras, self.body
        )
    }
}

// =============================================================================
// HTTP parsing
// =============================================================================

/// Reads HTTP request from a stream. Includes body based on Content-Length.
/// Header block capped at 64 KiB. Body size is enforced by the caller.
pub fn read_request(mut stream: impl Read) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 1024];
    let mut header_end = None;
    while header_end.is_none() {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(idx) = find_double_crlf(&buf) {
            header_end = Some(idx);
            break;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    }
    let hidx = match header_end {
        Some(i) => i,
        None => return None,
    };
    let headers = String::from_utf8_lossy(&buf[..hidx]).to_string();

    let cl = headers
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.trim().eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);

    // Cap body at 1 MiB — any single API body well under this.
    if cl > 1024 * 1024 {
        return None;
    }

    let body_start = hidx + 4;
    let mut body: Vec<u8> = buf[body_start..].to_vec();
    while body.len() < cl {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(cl);
    Some((headers, body))
}

pub fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

pub fn parse_request_line(headers: &str) -> Option<(&str, &str, &str)> {
    let first = headers.lines().next()?;
    let mut it = first.split_whitespace();
    let method = it.next()?;
    let full = it.next()?;
    let (path, query) = match full.split_once('?') {
        Some((p, q)) => (p, q),
        None => (full, ""),
    };
    Some((method, path, query))
}

pub fn parse_headers(headers: &str) -> Vec<(String, String)> {
    headers
        .lines()
        .skip(1)
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

pub fn query_get(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

// =============================================================================
// Identifier validation (defense in depth: runs on proxy AND daemon)
// =============================================================================

/// Validates that an identifier (nkr_name, cell name) contains only safe
/// characters: alphanum + '-' + '_' + '.'. Rejects spaces, newlines, quotes,
/// backticks, $, parens → blocks YAML, shell, SQL, log injection at the root.
/// Max 64 chars. Empty = invalid.
pub fn is_safe_identifier(s: &str) -> bool {
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

/// Validates a DNS (domain). Allowed chars: alphanum + '-' + '.'. Max 253.
/// Rejects leading '-' (argv injection into certbot/nginx) and ".."
/// (defense-in-depth even though '/' is already excluded).
pub fn is_safe_dns(s: &str) -> bool {
    if s.is_empty() || s.len() > 253 {
        return false;
    }
    if s.starts_with('-') || s.starts_with('.') || s.contains("..") {
        return false;
    }
    s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

/// `addons_path` has legitimate ',' and '/', but never newlines/quotes/
/// backticks/$/NUL. Max 1024.
pub fn is_safe_addons_path(s: &str) -> bool {
    if s.len() > 1024 {
        return false;
    }
    !s.bytes()
        .any(|b| matches!(b, b'\n' | b'\r' | b'"' | b'\'' | b'`' | b'$' | 0))
}

/// Validates a git remote URL. Accepts `git@github.com:owner/repo[.git]` (SSH)
/// or `https://github.com/owner/repo[.git]` (HTTPS). Rejects shell metacharacters
/// and any other scheme to contain the blast radius of a compromised panel.
/// Extend the whitelist (gitlab, bitbucket, self-hosted) by editing the prefix
/// list here — keep validation central.
pub fn is_safe_git_url(s: &str) -> bool {
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    // No shell meta, no whitespace control.
    if s.bytes().any(|b| matches!(b, b'\n' | b'\r' | b'\t' | b' ' | b'"' | b'\''
                                     | b'`' | b'$' | b'|' | b'&' | b';' | b'<' | b'>'
                                     | b'(' | b')' | b'{' | b'}' | b'\\' | 0)) {
        return false;
    }
    // Accepted shapes.
    s.starts_with("git@github.com:")
        || s.starts_with("https://github.com/")
        || s.starts_with("git@gitlab.com:")
        || s.starts_with("https://gitlab.com/")
}

/// Git ref: branch name, tag, or commit sha. Alphanumeric + `._-/`. Max 128.
pub fn is_safe_git_ref(s: &str) -> bool {
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    // Disallow leading `-` (avoids `--upload-pack=...` injection).
    if s.starts_with('-') {
        return false;
    }
    s.bytes().all(|b| b.is_ascii_alphanumeric()
        || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

/// Body size limits for the new panel-ops endpoints.
pub const GIT_BODY_LIMIT: usize = 64 * 1024;
pub const PYLIBS_BODY_LIMIT: usize = 128 * 1024;

// =============================================================================
// Constant-time comparison
// =============================================================================

/// Constant-time comparison of two byte slices. No short-circuit,
/// doesn't return on the first differing byte → no timing leak.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Validates the `Authorization: Bearer <token>` header against `NKR_API_TOKEN`.
/// If the env var is unset/empty, passes (dev mode). Caller returns the response
/// to the HTTP peer on Err.
pub fn check_auth(headers: &[(String, String)]) -> Result<(), HttpResponse> {
    let expected = match std::env::var("NKR_API_TOKEN") {
        Ok(t) if !t.is_empty() => t,
        _ => return Ok(()),
    };
    let got = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Authorization"))
        .map(|(_, v)| v.trim_start_matches("Bearer ").trim().to_string())
        .unwrap_or_default();
    if !ct_eq(got.as_bytes(), expected.as_bytes()) {
        return Err(HttpResponse::error(
            401,
            "unauthorized",
            Some("Authorization: Bearer <token> requerido"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_safe_dns_rejects_argv_injection() {
        // Real attack vectors from the audit: a leading `-` lets certbot
        // interpret the argument as a flag (root RCE via --post-hook).
        assert!(!is_safe_dns("-d"));
        assert!(!is_safe_dns("--config=/etc/passwd"));
        assert!(!is_safe_dns("-evil.com"));
        assert!(!is_safe_dns(".hidden"));
        assert!(!is_safe_dns("foo..bar"));
        assert!(!is_safe_dns(""));
        assert!(!is_safe_dns(&"a".repeat(254)));
        // Legitimate inputs still pass.
        assert!(is_safe_dns("client.example.com"));
        assert!(is_safe_dns("a.b.c"));
        assert!(is_safe_dns("tenant-42.systemouts.com"));
    }

    #[test]
    fn is_safe_identifier_blocks_path_traversal_chars() {
        assert!(!is_safe_identifier(""));
        assert!(!is_safe_identifier("a/b"));
        assert!(!is_safe_identifier("a b"));
        assert!(!is_safe_identifier("a;b"));
        assert!(!is_safe_identifier(&"x".repeat(65)));
        assert!(is_safe_identifier("company_client-tst-1"));
    }

    #[test]
    fn ct_eq_basic() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"abcd"));
        assert!(ct_eq(b"", b""));
    }
}
