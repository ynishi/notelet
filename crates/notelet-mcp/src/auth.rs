//! Token checks for the HTTP transport. One token, compared in constant
//! time: as `Authorization: Bearer` on every request under `/mcp`, and as
//! the HTTP Basic password on the read-only browser view under `/ui` (see
//! [`crate::ui`]). `/healthz` stays open so a platform health probe needs
//! no secret.

use std::sync::Arc;

use axum::{
    extract::Request,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// `Authorization: Bearer <token>` equals `token`, without early exit on
/// the first differing byte.
pub fn bearer_matches(headers: &HeaderMap, token: &str) -> bool {
    let Some(presented) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
    else {
        return false;
    };
    ct_eq(presented.as_bytes(), token.as_bytes())
}

/// Byte equality without early exit on the first differing byte. The
/// length is not secret (a mismatch returns at once).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `Authorization: Basic <base64(user:password)>` whose password equals
/// `token` (constant time). The user name is ignored: there are no users,
/// only the one token.
pub fn basic_matches(headers: &HeaderMap, token: &str) -> bool {
    let Some((scheme, encoded)) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
    else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let Some(raw) = base64_decode(encoded.trim()) else {
        return false;
    };
    let Some(colon) = raw.iter().position(|b| *b == b':') else {
        return false;
    };
    ct_eq(&raw[colon + 1..], token.as_bytes())
}

/// Standard-alphabet base64 with optional `=` padding; `None` on anything
/// else. Small enough not to be worth a dependency.
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    }
    let s = s.trim_end_matches('=').as_bytes();
    if s.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        let bytes = acc.to_be_bytes();
        out.extend_from_slice(&bytes[1..chunk.len()]);
    }
    Some(out)
}

/// axum middleware: reject with 401 unless the bearer matches.
pub async fn require_bearer(token: Arc<str>, req: Request, next: Next) -> Response {
    if bearer_matches(req.headers(), &token) {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(v: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, HeaderValue::from_str(v).unwrap());
        h
    }

    #[test]
    fn matches_only_exact_bearer() {
        assert!(bearer_matches(&headers("Bearer abc"), "abc"));
        assert!(!bearer_matches(&headers("Bearer abd"), "abc"));
        assert!(!bearer_matches(&headers("Bearer abcd"), "abc"));
        assert!(!bearer_matches(&headers("Basic abc"), "abc"));
        assert!(!bearer_matches(&HeaderMap::new(), "abc"));
    }

    #[test]
    fn basic_matches_password_only() {
        // "anyone:abc", "x:abd", "abc" (no colon)
        assert!(basic_matches(&headers("Basic YW55b25lOmFiYw=="), "abc"));
        assert!(basic_matches(&headers("basic YW55b25lOmFiYw"), "abc"));
        assert!(!basic_matches(&headers("Basic eDphYmQ="), "abc"));
        assert!(!basic_matches(&headers("Basic YWJj"), "abc"));
        assert!(!basic_matches(&headers("Bearer abc"), "abc"));
        assert!(!basic_matches(&headers("Basic !!!!"), "abc"));
        assert!(!basic_matches(&HeaderMap::new(), "abc"));
    }

    #[test]
    fn base64_round_trips_known_values() {
        assert_eq!(base64_decode("").unwrap(), b"");
        assert_eq!(base64_decode("Zg==").unwrap(), b"f");
        assert_eq!(base64_decode("Zm8=").unwrap(), b"fo");
        assert_eq!(base64_decode("Zm9v").unwrap(), b"foo");
        assert_eq!(base64_decode("Zm9vYmFy").unwrap(), b"foobar");
        assert!(base64_decode("Z").is_none());
    }
}
