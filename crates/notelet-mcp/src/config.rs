//! Environment configuration. Every value has one source; nothing is read
//! from a file except the token, and only when the operator points at one.
//!
//! | variable | meaning | default |
//! |---|---|---|
//! | `NOTELET_DATABASE_URL` (or `DATABASE_URL`) | Postgres connection string | required |
//! | `NOTELET_TOKEN` | bearer token every `/mcp` request must carry | none |
//! | `NOTELET_TOKEN_FILE` | read the token from this file instead (trailing newline stripped) | none |
//! | `NOTELET_BIND` | `host:port` for `serve` | `127.0.0.1:8490` |
//! | `NOTELET_ACTOR` | value written to `updated_by` for every write through this process | `mcp` |
//!
//! A non-loopback bind without a token is refused at startup: a hosted
//! instance must never be reachable without `Authorization: Bearer`.

use std::net::SocketAddr;

/// Resolved configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub database_url: String,
    pub token: Option<String>,
    pub bind: SocketAddr,
    pub actor: String,
}

impl Config {
    /// Default bind address: loopback, so a bare `serve` is never exposed.
    pub const DEFAULT_BIND: &'static str = "127.0.0.1:8490";

    /// Read the environment. Fails on a missing database URL, a malformed
    /// bind address, an unreadable token file, or an exposed bind without a
    /// token.
    pub fn from_env() -> anyhow::Result<Self> {
        let database_url = std::env::var("NOTELET_DATABASE_URL")
            .or_else(|_| std::env::var("DATABASE_URL"))
            .map_err(|_| anyhow::anyhow!("NOTELET_DATABASE_URL (or DATABASE_URL) is required"))?;

        let token = match std::env::var("NOTELET_TOKEN_FILE") {
            Ok(path) if !path.is_empty() => {
                let raw = std::fs::read_to_string(&path)
                    .map_err(|e| anyhow::anyhow!("reading NOTELET_TOKEN_FILE {path}: {e}"))?;
                Some(raw.trim_end_matches(['\n', '\r']).to_string())
            }
            _ => std::env::var("NOTELET_TOKEN").ok(),
        }
        .filter(|t| !t.is_empty());

        let bind_raw = std::env::var("NOTELET_BIND").unwrap_or_else(|_| Self::DEFAULT_BIND.into());
        let bind: SocketAddr = bind_raw
            .parse()
            .map_err(|e| anyhow::anyhow!("NOTELET_BIND {bind_raw:?} is not host:port: {e}"))?;

        if !bind.ip().is_loopback() && token.is_none() {
            anyhow::bail!(
                "refusing to bind non-loopback address {bind} without NOTELET_TOKEN \
                 (set the token, or bind a loopback address)"
            );
        }

        let actor = std::env::var("NOTELET_ACTOR")
            .ok()
            .filter(|a| !a.is_empty())
            .unwrap_or_else(|| "mcp".into());

        Ok(Self {
            database_url,
            token,
            bind,
            actor,
        })
    }
}
