//! Environment configuration. Every value has one source; nothing is read
//! from a file except the tokens, and only when the operator points at one.
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `NOTELET_DATABASE_URL` (or `DATABASE_URL`) | Postgres connection string | required, unless `NOTELET_REMOTE_URL` is set |
//! | `NOTELET_REMOTE_URL` | MCP endpoint of another notelet (`http(s)://host/mcp`): `serve` and `stdio` use it as their store instead of Postgres (see [`crate::remote`]) | none |
//! | `NOTELET_REMOTE_TOKEN` | bearer token presented to the remote; it selects the actor the remote records | none |
//! | `NOTELET_REMOTE_TOKEN_FILE` | read that token from this file instead (wins over `NOTELET_REMOTE_TOKEN`; trailing newline stripped) | none |
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
//! The store is one of two [`Backend`]s. With `NOTELET_REMOTE_URL` set there
//! is no database: startup fails if a database URL is set as well (two
//! stores is a mistake, not a preference), and only `serve` and `stdio`
//! run; `migrate`, `import` and `export` need the database and fail with an
//! error naming the variable to unset. A global `DATABASE_URL` counts as a
//! database URL, so it makes a remote start refuse; `NOTELET_DATABASE_URL=`
//! set to empty takes precedence over it and masks it (empty is unset).
//! The remote token variables are read only when `NOTELET_REMOTE_URL` is set. `NOTELET_REMOTE_TOKEN[_FILE]` is
//! separate from `NOTELET_TOKEN[_FILE]`: the first is the credential this
//! process presents upstream, the second is one it accepts from its clients.
//!
//! A non-loopback bind with an empty table is refused at startup: a hosted
//! instance must never be reachable without `Authorization: Bearer`.

use std::net::SocketAddr;

use anyhow::Context;

use crate::auth::{digest, valid_actor, Actor, TokenTable, TOKEN_PREFIX};

/// Where the notes live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// Postgres at this connection string.
    Database(String),
    /// Another notelet, reached over MCP.
    Remote(RemoteConfig),
}

/// The remote notelet of [`Backend::Remote`].
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    pub url: String,
    /// Bearer token for the remote; `None` for a remote without tokens.
    pub token: Option<String>,
}

impl std::fmt::Debug for RemoteConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteConfig")
            .field("url", &"<set>")
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Backend {
    /// The connection string, for the commands that need the database
    /// (`migrate`, `import`, `export`). `command` names the caller in the
    /// error when the backend is remote.
    pub fn database_url(&self, command: &str) -> anyhow::Result<&str> {
        match self {
            Self::Database(url) => Ok(url),
            Self::Remote(_) => anyhow::bail!(
                "`{command}` needs the database, but NOTELET_REMOTE_URL is set \
                 (unset it and set NOTELET_DATABASE_URL; only `serve` and `stdio` \
                 can use a remote)"
            ),
        }
    }
}

/// Choose the backend from the raw values. Empty strings count as unset.
fn resolve_backend(
    database_url: Option<String>,
    remote_url: Option<String>,
    remote_token: Option<String>,
) -> anyhow::Result<Backend> {
    let database_url = database_url.filter(|v| !v.is_empty());
    let remote_url = remote_url.filter(|v| !v.is_empty());
    match (database_url, remote_url) {
        (Some(_), Some(_)) => anyhow::bail!(
            "NOTELET_REMOTE_URL and NOTELET_DATABASE_URL (or DATABASE_URL) are both set: \
             choose one store"
        ),
        (Some(url), None) => Ok(Backend::Database(url)),
        (None, Some(url)) => {
            anyhow::ensure!(
                url.starts_with("http://") || url.starts_with("https://"),
                "NOTELET_REMOTE_URL must start with http:// or https://"
            );
            Ok(Backend::Remote(RemoteConfig {
                url,
                token: remote_token.filter(|t| !t.is_empty()),
            }))
        }
        (None, None) => anyhow::bail!(
            "NOTELET_DATABASE_URL (or DATABASE_URL) is required \
             (or NOTELET_REMOTE_URL for `serve` and `stdio`)"
        ),
    }
}

/// A token from `file_var` (trailing newline stripped; wins) or else `var`.
fn token_from_env(file_var: &str, var: &str) -> anyhow::Result<Option<String>> {
    Ok(match std::env::var(file_var) {
        Ok(path) if !path.is_empty() => {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {file_var} {path}"))?;
            Some(raw.trim_end_matches(['\n', '\r']).to_string())
        }
        _ => std::env::var(var).ok(),
    }
    .filter(|t| !t.is_empty()))
}

/// Resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
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

    /// Read the environment. Fails on neither a database URL nor a remote
    /// URL, on both, a malformed bind address, an unreadable token file, a
    /// malformed or duplicated token entry, or an exposed bind without any
    /// token.
    pub fn from_env() -> anyhow::Result<(Self, Backend)> {
        let database_url = std::env::var("NOTELET_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .ok();
        let remote_url = std::env::var("NOTELET_REMOTE_URL")
            .ok()
            .filter(|u| !u.is_empty());
        // the remote token (and its file) is read only for a remote start, so
        // a stale file cannot fail a database-backed one
        let remote_token = if remote_url.is_some() {
            token_from_env("NOTELET_REMOTE_TOKEN_FILE", "NOTELET_REMOTE_TOKEN")?
        } else {
            None
        };
        let backend = resolve_backend(database_url, remote_url, remote_token)?;

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

        let single = token_from_env("NOTELET_TOKEN_FILE", "NOTELET_TOKEN")?;
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

        Ok((
            Self {
                tokens,
                bind,
                actor,
            },
            backend,
        ))
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

    fn some(s: &str) -> Option<String> {
        Some(s.to_string())
    }

    #[test]
    fn remote_and_database_together_refuse_to_start() {
        let err = resolve_backend(some("pg://x"), some("http://127.0.0.1:1/mcp"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("both set"), "{err}");
        assert!(
            !err.contains("pg://x") && !err.contains("127.0.0.1"),
            "{err}"
        );
    }

    #[test]
    fn remote_alone_needs_no_database_url() {
        let b =
            resolve_backend(None, some("http://127.0.0.1:1/mcp"), some("s3cr3t-value")).unwrap();
        let Backend::Remote(r) = b else {
            panic!("expected remote")
        };
        assert_eq!(r.url, "http://127.0.0.1:1/mcp");
        assert_eq!(r.token.as_deref(), Some("s3cr3t-value"));
        // the token and url never show in Debug
        let dbg = format!("{r:?}");
        assert!(
            !dbg.contains("s3cr3t-value") && !dbg.contains("127.0.0.1"),
            "{dbg}"
        );
        // empty values are unset
        let b = resolve_backend(some(""), some("http://h/mcp"), some("")).unwrap();
        assert!(matches!(
            b,
            Backend::Remote(RemoteConfig { token: None, .. })
        ));
    }

    #[test]
    fn a_store_is_required_and_a_remote_url_must_be_http() {
        let err = resolve_backend(None, None, None).unwrap_err().to_string();
        assert!(err.contains("NOTELET_DATABASE_URL"), "{err}");
        assert!(resolve_backend(None, some("ftp://h"), None).is_err());
        assert_eq!(
            resolve_backend(some("pg://x"), None, None).unwrap(),
            Backend::Database("pg://x".into())
        );
    }

    #[test]
    fn database_commands_fail_clearly_on_a_remote() {
        let remote = resolve_backend(None, some("http://h/mcp"), None).unwrap();
        for cmd in ["migrate", "import", "export"] {
            let err = remote.database_url(cmd).unwrap_err().to_string();
            assert!(
                err.contains(cmd) && err.contains("NOTELET_REMOTE_URL"),
                "{err}"
            );
        }
        let db = Backend::Database("pg://x".into());
        assert_eq!(db.database_url("import").unwrap(), "pg://x");
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
