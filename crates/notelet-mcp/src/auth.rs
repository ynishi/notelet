//! Token checks for the HTTP transport, and the identity a token carries.
//!
//! A credential is a bearer token. The operator keeps a [`TokenTable`]:
//! sha256 of each token -> the [`Actor`] name it writes as. A presented
//! token is hashed and looked up; a miss is `401`, a hit puts the actor in
//! the request extensions, where [`crate::server`] reads it per tool call
//! and passes it to the service as `updated_by`. So `updated_by` records
//! which credential authenticated a write, and the identity unit is the
//! token: there is no users table, no tool to declare who you are, and no
//! header a client could set to claim to be someone else.
//!
//! The same table authenticates both doors: `Authorization: Bearer` on
//! every request under `/mcp`, and the HTTP Basic password (any user name)
//! on the read-only browser view under `/ui` (see [`crate::ui`]).
//! `/healthz` stays open so a platform health probe needs no secret.
//!
//! The table holds digests, never tokens, so the file or variable that
//! lists them can be read by an operator or a backup without exposing a
//! working credential. The lookup is a hash-map probe on the digest of the
//! presented token: how long it takes can at most reveal bytes of a digest,
//! from which the token cannot be recovered (the preimage of sha256), so no
//! constant-time comparison of the secret itself is needed.
//!
//! Rotation: add a second line for the same actor with the new token's
//! digest, move the client over, remove the old line. Revocation: remove
//! the line and restart; the table is read once at startup and is not
//! reloaded. Sharing one token between clients makes them
//! indistinguishable in `updated_by`; give each client its own.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::Request,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sha2::{Digest, Sha256};

/// Who a token writes as: the `updated_by` of its writes. Cheap to clone;
/// stored in request extensions by [`require_bearer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor(Arc<str>);

impl Actor {
    /// Wrap a name. Callers validate with [`valid_actor`] first.
    pub fn new(name: &str) -> Self {
        Self(Arc::from(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// An actor name is 1..=64 of `[A-Za-z0-9._-]`: safe to print in a log and
/// to write in an entry line.
pub fn valid_actor(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Prefix of generated tokens (`notelet-mcp token`). A digest is bare hex, so
/// a token pasted where a digest belongs is recognisable and refused.
pub const TOKEN_PREFIX: &str = "nlt_";

/// sha256 of a token, the key of the table.
pub fn digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// Digest -> actor. Built once at startup (see [`crate::config`]).
#[derive(Clone, Default)]
pub struct TokenTable(HashMap<[u8; 32], Actor>);

impl std::fmt::Debug for TokenTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TokenTable({} entries)", self.0.len())
    }
}

impl TokenTable {
    /// Add an entry; `false` (and no change) when the digest is already
    /// present, because one token cannot be two identities.
    pub fn insert(&mut self, digest: [u8; 32], actor: Actor) -> bool {
        match self.0.entry(digest) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(actor);
                true
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The actor of a presented token, if its digest is listed.
    pub fn lookup(&self, presented: &str) -> Option<&Actor> {
        self.0.get(&digest(presented))
    }
}

/// The actor of `Authorization: Bearer <token>`.
pub fn bearer_actor<'a>(headers: &HeaderMap, table: &'a TokenTable) -> Option<&'a Actor> {
    let presented = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))?;
    table.lookup(presented)
}

/// The actor of `Authorization: Basic <base64(user:password)>` whose
/// password is a listed token. The user name is ignored: there are no
/// users, only tokens.
pub fn basic_actor<'a>(headers: &HeaderMap, table: &'a TokenTable) -> Option<&'a Actor> {
    let (scheme, encoded) = headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))?;
    if !scheme.eq_ignore_ascii_case("Basic") {
        return None;
    }
    let raw = base64_decode(encoded.trim())?;
    let colon = raw.iter().position(|b| *b == b':')?;
    table.lookup(std::str::from_utf8(&raw[colon + 1..]).ok()?)
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

/// axum middleware: reject with 401 unless the bearer token is listed; on a
/// hit, put its [`Actor`] in the request extensions for the tool handlers.
pub async fn require_bearer(table: Arc<TokenTable>, mut req: Request, next: Next) -> Response {
    match bearer_actor(req.headers(), &table).cloned() {
        Some(actor) => {
            req.extensions_mut().insert(actor);
            next.run(req).await
        }
        None => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response(),
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

    /// `abc` -> alice, `xyz` -> bob.
    fn table() -> TokenTable {
        let mut t = TokenTable::default();
        assert!(t.insert(digest("abc"), Actor::new("alice")));
        assert!(t.insert(digest("xyz"), Actor::new("bob")));
        t
    }

    fn who(a: Option<&Actor>) -> Option<&str> {
        a.map(Actor::as_str)
    }

    #[test]
    fn bearer_hits_and_misses() {
        let t = table();
        assert_eq!(who(bearer_actor(&headers("Bearer abc"), &t)), Some("alice"));
        assert_eq!(who(bearer_actor(&headers("Bearer xyz"), &t)), Some("bob"));
        assert_eq!(who(bearer_actor(&headers("Bearer abd"), &t)), None);
        assert_eq!(who(bearer_actor(&headers("Bearer abcd"), &t)), None);
        assert_eq!(who(bearer_actor(&headers("Bearer "), &t)), None);
        assert_eq!(who(bearer_actor(&headers("bearer abc"), &t)), None);
        assert_eq!(who(bearer_actor(&headers("Basic abc"), &t)), None);
        assert_eq!(who(bearer_actor(&HeaderMap::new(), &t)), None);
        assert_eq!(
            who(bearer_actor(&headers("Bearer abc"), &TokenTable::default())),
            None
        );
    }

    #[test]
    fn basic_takes_the_password_with_any_user() {
        let t = table();
        // "anyone:abc", "x:xyz", "x:abd", "abc" (no colon)
        assert_eq!(
            who(basic_actor(&headers("Basic YW55b25lOmFiYw=="), &t)),
            Some("alice")
        );
        assert_eq!(
            who(basic_actor(&headers("basic YW55b25lOmFiYw"), &t)),
            Some("alice")
        );
        assert_eq!(
            who(basic_actor(&headers("Basic eDp4eXo="), &t)),
            Some("bob")
        );
        assert_eq!(who(basic_actor(&headers("Basic eDphYmQ="), &t)), None);
        assert_eq!(who(basic_actor(&headers("Basic YWJj"), &t)), None);
        assert_eq!(who(basic_actor(&headers("Bearer abc"), &t)), None);
        assert_eq!(who(basic_actor(&headers("Basic !!!!"), &t)), None);
        assert_eq!(who(basic_actor(&headers("Basic"), &t)), None);
        assert_eq!(who(basic_actor(&HeaderMap::new(), &t)), None);
    }

    #[test]
    fn duplicate_digest_is_refused() {
        let mut t = table();
        assert!(!t.insert(digest("abc"), Actor::new("carol")));
        assert_eq!(who(t.lookup("abc")), Some("alice"));
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn actor_names() {
        for ok in ["a", "mcp", "Alice.2_x-y", &"a".repeat(64)] {
            assert!(valid_actor(ok), "{ok}");
        }
        for bad in ["", "a b", "a:b", "é", "a/b", &"a".repeat(65)] {
            assert!(!valid_actor(bad), "{bad}");
        }
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
