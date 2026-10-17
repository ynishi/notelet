//! `notelet-mcp`: the transport binary. It owns nothing but wiring.
//!
//! ```text
//! notelet-mcp serve     # streamable HTTP MCP at http://<bind>/mcp (default), read-only view at /ui/
//! notelet-mcp stdio     # the same server over stdin/stdout, for local clients
//! notelet-mcp migrate   # apply migrations and exit
//! notelet-mcp import <dir> [--prefix <p>] [--dry-run]   # file tree -> notes (operator)
//! notelet-mcp export <dir> [--prefix <p>]               # notes -> file tree (operator)
//! notelet-mcp token <actor>                             # new bearer token + its NOTELET_TOKENS entry
//! ```
//!
//! Configuration is environment only (see [`config`]): a database URL (or,
//! for `serve` and `stdio`, the URL and token of a remote notelet in its
//! place, see [`remote`]), bearer tokens (each mapped to the actor it
//! writes as), a bind address, a fallback actor name. There is no config file,
//! because the host (Fly, systemd, a shell) already has a way to set
//! environment variables and a second mechanism would have to be kept in
//! agreement with it.
//!
//! The server itself lives in [`server`] (its local mode, which needs `NOTELET_FILE_ROOT`,
//! adds the file projection of [`files`]); the bundled guides in
//! [`resources`]; token lookup and the per-token actor in [`auth`]; the
//! operator's bulk
//! `import` / `export` (not MCP tools) in [`transfer`]; the read-only
//! browser view (`/ui`, HTTP Basic, no writes) in [`ui`], rendering through
//! [`render`].

mod auth;
mod config;
mod files;
mod remote;
mod render;
mod resources;
mod server;
mod transfer;
mod ui;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use notelet_core::{NoteRepo, NoteService, PgNoteRepo};

use crate::config::{Backend, Config};
use crate::remote::RemoteNoteRepo;

#[derive(Parser)]
#[command(
    name = "notelet-mcp",
    version,
    about = "notelet: a thin hosted Markdown note store over MCP"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve MCP over streamable HTTP (default).
    Serve,
    /// Serve MCP over stdio (one local client).
    Stdio,
    /// Apply migrations and exit.
    Migrate,
    /// Import every regular file under DIR as a note (operator bulk load).
    Import {
        /// Directory to walk recursively.
        dir: std::path::PathBuf,
        /// Path prefix for every note, e.g. `projects/x`.
        #[arg(long)]
        prefix: Option<String>,
        /// Report what would be created / updated without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Make a bearer token for ACTOR and print it with its NOTELET_TOKENS entry
    /// (needs no database).
    Token {
        /// Name written to `updated_by` by this token: `[A-Za-z0-9._-]{1,64}`.
        actor: String,
    },
    /// Write every note (under --prefix) to DIR as a file tree.
    Export {
        /// Destination directory (created as needed).
        dir: std::path::PathBuf,
        /// Only notes whose path starts with this.
        #[arg(long)]
        prefix: Option<String>,
    },
}

/// The directives added to the filter text unless it already names the
/// target (see the comment in `main`). The match is on the exact target:
/// the text of a directive before its first `=` or `[`, trimmed. A
/// sub-target such as `rmcp::transport` does not count as naming `rmcp`;
/// it stays more specific than the added hold, so it still wins.
fn extra_directives(raw: &str) -> Vec<&'static str> {
    let named: Vec<&str> = raw
        .split(',')
        .map(|d| d.split(['=', '[']).next().unwrap_or("").trim())
        .collect();
    [("rmcp", "rmcp=info"), ("sqlx", "sqlx=warn")]
        .into_iter()
        .filter(|(target, _)| !named.contains(target))
        .map(|(_, d)| d)
        .collect()
}

/// A fresh token: `nlt_` and 32 random bytes from the operating system, as hex.
fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no system randomness: {e}"))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Ok(format!("{}{hex}", auth::TOKEN_PREFIX))
}

/// `notelet-mcp token <actor>`: print the token for the client and the line
/// for the server's `NOTELET_TOKENS` (the token itself is never stored).
fn print_token(actor: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        auth::valid_actor(actor),
        "actor must match [A-Za-z0-9._-]{{1,64}}"
    );
    let token = new_token()?;
    let hex: String = auth::digest(&token)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    println!("token (give to the client, keep secret): {token}");
    println!("entry (add to NOTELET_TOKENS or NOTELET_TOKENS_FILE): {actor}:{hex}");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stderr only: stdout is the protocol channel in stdio mode, and a
    // daemon's stderr is its log. No ANSI: the reader is a file or journal.
    // `rmcp` is held at `info` because its `debug` events carry whole
    // requests and results, i.e. note bodies: `RUST_LOG=debug` must not
    // print them. `sqlx` is held at `warn` (statement logging is noise; slow
    // statements stay visible). Naming either target in `RUST_LOG` wins.
    let raw = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    let mut filter = tracing_subscriber::EnvFilter::try_new(&raw)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    for d in extra_directives(&raw) {
        if let Ok(d) = d.parse() {
            filter = filter.add_directive(d);
        }
    }
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let cli = Cli::parse();
    if let Some(Cmd::Token { actor }) = &cli.cmd {
        return print_token(actor);
    }
    let (cfg, backend) = Config::from_env()?;

    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Stdio => {
            cfg.check_file_root()?;
            let svc = Arc::new(NoteService::new(open_serving_repo(&backend).await?));
            server::run_stdio(svc, cfg.actor, cfg.file_root.map(files::FileRoot::new)).await
        }
        Cmd::Serve => {
            cfg.check_file_root()?;
            let svc = Arc::new(NoteService::new(open_serving_repo(&backend).await?));
            server::run_http(svc, cfg).await
        }
        Cmd::Migrate => {
            let repo = PgNoteRepo::connect(backend.database_url("migrate")?).await?;
            repo.migrate().await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Cmd::Import {
            dir,
            prefix,
            dry_run,
        } => {
            let repo = PgNoteRepo::connect(backend.database_url("import")?).await?;
            // A dry run only reads; it must not change the schema either.
            if !dry_run {
                repo.migrate().await?;
            }
            let svc = NoteService::new(Arc::new(repo));
            let summary =
                transfer::import(&svc, &dir, prefix.as_deref(), dry_run, &cfg.actor).await?;
            println!("{summary}{}", if dry_run { " (dry run)" } else { "" });
            Ok(())
        }
        Cmd::Export { dir, prefix } => {
            let repo = PgNoteRepo::connect(backend.database_url("export")?).await?;
            let svc = NoteService::new(Arc::new(repo));
            let summary = transfer::export(&svc, &dir, prefix.as_deref()).await?;
            println!("{summary}");
            Ok(())
        }
        Cmd::Token { .. } => unreachable!("handled before the database is opened"),
    }
}

/// The store `serve` and `stdio` run on: Postgres (migrated on start), or
/// the remote notelet, in which case nothing is migrated and no database is
/// touched.
async fn open_serving_repo(backend: &Backend) -> anyhow::Result<Arc<dyn NoteRepo>> {
    Ok(match backend {
        Backend::Database(url) => {
            let repo = PgNoteRepo::connect(url).await?;
            repo.migrate().await?;
            Arc::new(repo)
        }
        Backend::Remote(r) => {
            tracing::info!("using a remote notelet as the store");
            Arc::new(RemoteNoteRepo::connect(&r.url, r.token.as_deref()).await?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{extra_directives, new_token};

    #[test]
    fn tokens_are_prefixed_hex_and_differ() {
        let (a, b) = (new_token().unwrap(), new_token().unwrap());
        let hex = a.strip_prefix("nlt_").expect("prefix");
        assert_eq!(hex.len(), 64);
        assert!(hex.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn both_held_by_default() {
        assert_eq!(extra_directives("info"), vec!["rmcp=info", "sqlx=warn"]);
    }

    #[test]
    fn named_target_is_left_alone() {
        assert_eq!(extra_directives("debug,rmcp=debug"), vec!["sqlx=warn"]);
        assert_eq!(extra_directives("sqlx=trace"), vec!["rmcp=info"]);
        assert_eq!(extra_directives(" rmcp = trace "), vec!["sqlx=warn"]);
    }

    #[test]
    fn sub_targets_do_not_count_as_naming() {
        assert_eq!(
            extra_directives("debug,rmcp::transport=trace"),
            vec!["rmcp=info", "sqlx=warn"]
        );
        assert_eq!(
            extra_directives("sqlx::query=debug"),
            vec!["rmcp=info", "sqlx=warn"]
        );
    }
}
