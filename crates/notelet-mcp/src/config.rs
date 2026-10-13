//! Environment configuration. Every value has one source; nothing is read
//! from a file except the tokens, and only when the operator points at one.
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `NOTELET_DATABASE_URL` (or `DATABASE_URL`) | Postgres connection string | required |
//! | `NOTELET_TOKENS` | credentials, one per client: entries `<actor>:<sha256 hex of the token>` separated by whitespace; a request's token selects the actor written to `updated_by` | none |
//! | `NOTELET_TOKENS_FILE` | read the entries from this file instead (wins over `NOTELET_TOKENS`); same format | none |
//! | `NOTELET_TOKEN` | one bearer token, written as `NOTELET_ACTOR`; joins the table above | none |
//! | `NOTELET_TOKEN_FILE` | read that token from this file instead (trailing newline stripped) | none |
//! | `NOTELET_BIND` | `host:port` for `serve` | `127.0.0.1:8490` |
//! | `NOTELET_ACTOR` | (same name rule as actors, checked at startup) `updated_by` for `NOTELET_TOKEN`, and for writes with no credential (`stdio`, `import`, a loopback `serve` without tokens) | `mcp` |
//! | `RUST_LOG` | log filter, read in `main` (`rmcp` is held at `info`, `sqlx` at `warn`, unless named) | `info` |
//!
//! The token table is the union of `NOTELET_TOKENS[_FILE]` and the single
//! `NOTELET_TOKEN[_FILE]`. Lines starting with `#` are comments in the
//! list, whether it comes from the file or from the variable, and a leading
//! UTF-8 BOM is ignored. Startup fails on a malformed entry (the actor
//! must match `[A-Za-z0-9._-]{1,64}`, the digest is exactly 64 hex digits)
//! and on a digest listed twice, since one token cannot be two identities;
//! the same actor on several entries is fine and is how a token is rotated
//! (see [`crate::auth`]). `notelet-mcp token <actor>` makes a token and its
//! entry line.
//!
//! A non-loopback bind with an empty table is refused at startup: a hosted
//! instance must never be reachable without `Authorization: Bearer`.

use std::net::SocketAddr;

use anyhow::Context;

use crate::auth::{digest, valid_actor, Actor, TokenTable, TOKEN_PREFIX};

/// Resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    /// Credential -> actor. Empty means no authentication (loopback only).
    pub tokens: TokenTable,
    pub bind: SocketAddr,
    /// The actor for writes that carry no credential: `stdio`, `import`, a
    /// tokenless loopback `serve`; also the actor of `NOTELET_TOKEN`.
    pub actor: String,
}

impl Config {
    /// Default bind address: loopback, so a bare `serve` is never exposed.
    pub const DEFAULT_BIND: &'static str = "127.0.0.1:8490";

    /// Read the environment. Fails on a missing database URL, a malformed
    /// bind address, an unreadable token file, a malformed or duplicated
    /// token entry, or an exposed bind without any token.
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("NOTELET_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .context("NOTELET_DATABASE_URL (or DATABASE_URL) is required")?;

        let actor = fallback_actor(std::env::var("NOTELET_ACTOR").ok())?;

        let mut tokens = TokenTable::default();
        let list = match std::env::var("NOTELET_TOKENS_FILE") {
            Ok(path) if !path.is_empty() => Some((
                "NOTELET_TOKENS_FILE",
                std::fs::read_to_string(&path)
                    .with_context(|| format!("reading NOTELET_TOKENS_FILE {path}"))?,
            )),
            _ => std::env::var("NOTELET_TOKENS")
                .ok()
                .map(|v| ("NOTELET_TOKENS", v)),
        };
        if let Some((source, text)) = list {
            parse_entries(&text, source, &mut tokens)?;
        }

        let single = match std::env::var("NOTELET_TOKEN_FILE") {
            Ok(path) if !path.is_empty() => {
                let raw = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading NOTELET_TOKEN_FILE {path}"))?;
                Some(raw.trim_end_matches(['\n', '\r']).to_string())
            }
            _ => std::env::var("NOTELET_TOKEN").ok(),
        }
        .filter(|t| !t.is_empty());
        if let Some(token) = single {
            add_single_token(&mut tokens, &token, &actor)?;
        }

        let bind_raw = std::env::var("NOTELET_BIND").unwrap_or_else(|_| Self::DEFAULT_BIND.into());
        let bind: SocketAddr = bind_raw
            .parse()
            .with_context(|| format!("NOTELET_BIND {bind_raw:?} is not host:port"))?;

        if !bind.ip().is_loopback() && tokens.is_empty() {
            anyhow::bail!(
                "refusing to bind non-loopback address {bind} without a token \
                 (set NOTELET_TOKENS, NOTELET_TOKENS_FILE or NOTELET_TOKEN, \
                 or bind a loopback address)"
            );
        }

        Ok(Self {
            database_url,
            tokens,
            bind,
            actor,
        })
    }
}

/// `NOTELET_ACTOR`: unset or empty is `mcp`; anything else must be a valid
/// actor name, since it is written to `updated_by` like a token's actor.
fn fallback_actor(raw: Option<String>) -> anyhow::Result<String> {
    let actor = raw
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "mcp".into());
    anyhow::ensure!(
        valid_actor(&actor),
        "NOTELET_ACTOR must match [A-Za-z0-9._-]{{1,64}}"
    );
    Ok(actor)
}

/// The single `NOTELET_TOKEN` as one table entry, written as `actor`.
fn add_single_token(table: &mut TokenTable, token: &str, actor: &str) -> anyhow::Result<()> {
    if !table.insert(digest(token), Actor::new(actor)) {
        anyhow::bail!("NOTELET_TOKEN is also listed in NOTELET_TOKENS (one token, two entries)");
    }
    Ok(())
}

/// Parse whitespace-separated `<actor>:<sha256 hex>` entries into `table`.
/// A line whose first non-blank character is `#` is a comment. `source`
/// names the variable in error messages, which give the entry's position
/// (from 1) and the rule it broke, never its text.
fn parse_entries(text: &str, source: &str, table: &mut TokenTable) -> anyhow::Result<()> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let entries = text
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .flat_map(str::split_whitespace);
    for (i, entry) in entries.enumerate() {
        let n = i + 1;
        let (actor, hex) = entry
            .split_once(':')
            .with_context(|| format!("{source} entry {n}: expected <actor>:<sha256 hex>"))?;
        if !valid_actor(actor) {
            anyhow::bail!("{source} entry {n}: actor must match [A-Za-z0-9._-]{{1,64}}");
        }
        if hex.starts_with(TOKEN_PREFIX) {
            anyhow::bail!(
                "{source} entry {n}: this is a token, put its sha256 here (see `notelet-mcp token`)"
            );
        }
        let digest = parse_digest(hex)
            .with_context(|| format!("{source} entry {n}: digest must be 64 hex digits"))?;
        if !table.insert(digest, Actor::new(actor)) {
            anyhow::bail!("{source} entry {n}: digest already listed");
        }
    }
    Ok(())
}

/// Exactly 64 hex digits, either case, to 32 bytes.
fn parse_digest(hex: &str) -> Option<[u8; 32]> {
    let b = hex.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nibble = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut out = [0u8; 32];
    for (o, pair) in out.iter_mut().zip(b.chunks(2)) {
        *o = nibble(pair[0])? << 4 | nibble(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_of(token: &str) -> String {
        digest(token).iter().map(|b| format!("{b:02x}")).collect()
    }

    fn parse(text: &str) -> anyhow::Result<TokenTable> {
        let mut t = TokenTable::default();
        parse_entries(text, "T", &mut t)?;
        Ok(t)
    }

    #[test]
    fn entries_split_on_any_whitespace_and_skip_comments() {
        let text = format!(
            "# clients\nalice:{}  bob:{}\n\n   # indented comment\ncarol:{}\n",
            hex_of("a"),
            hex_of("b").to_uppercase(),
            hex_of("c")
        );
        let t = parse(&text).unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t.lookup("a").unwrap().as_str(), "alice");
        assert_eq!(t.lookup("b").unwrap().as_str(), "bob");
        assert_eq!(t.lookup("c").unwrap().as_str(), "carol");
        assert!(t.lookup("d").is_none());
        assert!(parse("").unwrap().is_empty());
        assert!(parse("# only a comment\n").unwrap().is_empty());
    }

    #[test]
    fn one_actor_on_several_entries_is_rotation() {
        let t = parse(&format!("alice:{} alice:{}", hex_of("old"), hex_of("new"))).unwrap();
        assert_eq!(t.lookup("old").unwrap().as_str(), "alice");
        assert_eq!(t.lookup("new").unwrap().as_str(), "alice");
    }

    #[test]
    fn malformed_entries_name_their_position() {
        let good = hex_of("a");
        for (text, pos) in [
            ("nocolon".to_string(), 1),
            (format!("alice:{good} bad actor:{good}"), 2),
            (format!("alice:{good} b@d:{good}"), 2),
            (format!(":{good}"), 1),
            ("alice:abcd".to_string(), 1),
            (format!("alice:{}zz", &good[..62]), 1),
            (format!("{}:{good}", "a".repeat(65)), 1),
        ] {
            let err = parse(&text).unwrap_err().to_string();
            assert!(err.contains(&format!("entry {pos}")), "{text}: {err}");
            assert!(!err.contains(&good), "digest echoed: {err}");
        }
    }

    #[test]
    fn fallback_actor_is_validated() {
        assert_eq!(fallback_actor(None).unwrap(), "mcp");
        assert_eq!(fallback_actor(Some(String::new())).unwrap(), "mcp");
        assert_eq!(fallback_actor(Some("dev.1".into())).unwrap(), "dev.1");
        for bad in ["a b", "a:b", "é", &"a".repeat(65)] {
            let err = fallback_actor(Some(bad.into())).unwrap_err().to_string();
            assert!(err.contains("NOTELET_ACTOR"), "{err}");
            assert!(!err.contains(bad));
        }
    }

    #[test]
    fn leading_bom_is_ignored() {
        let t = parse(&format!("\u{feff}# c\nalice:{}", hex_of("a"))).unwrap();
        assert_eq!(t.lookup("a").unwrap().as_str(), "alice");
        let t = parse(&format!("\u{feff}alice:{}", hex_of("a"))).unwrap();
        assert_eq!(t.len(), 1);
    }

    #[test]
    fn a_pasted_token_is_refused_without_echo() {
        let tok = format!("{TOKEN_PREFIX}{}", "ab".repeat(32));
        let err = parse(&format!("alice:{tok}")).unwrap_err().to_string();
        assert!(
            err.contains("entry 1") && err.contains("this is a token"),
            "{err}"
        );
        assert!(!err.contains(&tok));
    }

    #[test]
    fn single_token_joins_the_table_as_the_fallback_actor() {
        let mut t = parse(&format!("alice:{}", hex_of("a"))).unwrap();
        add_single_token(&mut t, "legacy", "mcp").unwrap();
        assert_eq!(t.lookup("legacy").unwrap().as_str(), "mcp");
        assert_eq!(t.lookup("a").unwrap().as_str(), "alice");
        // the same token also listed: refused
        assert!(add_single_token(&mut t, "a", "mcp").is_err());
    }

    #[test]
    fn duplicate_digest_is_an_error_even_for_one_actor() {
        let h = hex_of("a");
        let err = parse(&format!("alice:{h} bob:{h}"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("entry 2") && err.contains("already listed"),
            "{err}"
        );
        assert!(parse(&format!("alice:{h} alice:{}", h.to_uppercase())).is_err());
    }
}
