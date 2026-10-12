//! Bearer-token check for the HTTP transport. One token, compared in
//! constant time, on every request under `/mcp`. `/healthz` stays open so
//! a platform health probe needs no secret.

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
    let (a, b) = (presented.as_bytes(), token.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
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
}
