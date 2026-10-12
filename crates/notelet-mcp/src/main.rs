//! `notelet-mcp`: the transport binary. It owns nothing but wiring.
//!
//! ```text
//! notelet-mcp serve     # streamable HTTP MCP at http://<bind>/mcp (default), read-only view at /ui/
//! notelet-mcp stdio     # the same server over stdin/stdout, for local clients
//! notelet-mcp migrate   # apply migrations and exit
//! notelet-mcp import <dir> [--prefix <p>] [--dry-run]   # file tree -> notes (operator)
//! notelet-mcp export <dir> [--prefix <p>]               # notes -> file tree (operator)
//! ```
//!
//! Configuration is environment only (see [`config`]): a database URL, a
//! bearer token, a bind address, an actor name. There is no config file,
//! because the host (Fly, systemd, a shell) already has a way to set
//! environment variables and a second mechanism would have to be kept in
//! agreement with it.
//!
//! The server itself lives in [`server`]; the bundled guides in
//! [`resources`]; bearer checking in [`auth`]; the operator's bulk
//! `import` / `export` (not MCP tools) in [`transfer`]; the read-only
//! browser view (`/ui`, HTTP Basic, no writes) in [`ui`], rendering through
//! [`render`].

mod auth;
mod config;
mod render;
mod resources;
mod server;
mod transfer;
mod ui;

use std::sync::Arc;

use clap::{Parser, Subcommand};
use notelet_core::{NoteService, PgNoteRepo};

use crate::config::Config;

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
    /// Write every note (under --prefix) to DIR as a file tree.
    Export {
        /// Destination directory (created as needed).
        dir: std::path::PathBuf,
        /// Only notes whose path starts with this.
        #[arg(long)]
        prefix: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // stderr only: stdout is the protocol channel in stdio mode, and a
    // daemon's stderr is its log. No ANSI: the reader is a file or journal.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();

    let cli = Cli::parse();
    let cfg = Config::from_env()?;

    let repo = PgNoteRepo::connect(&cfg.database_url).await?;
    match cli.cmd.unwrap_or(Cmd::Serve) {
        Cmd::Migrate => {
            repo.migrate().await?;
            tracing::info!("migrations applied");
            Ok(())
        }
        Cmd::Stdio => {
            repo.migrate().await?;
            let svc = Arc::new(NoteService::new(Arc::new(repo)));
            server::run_stdio(svc, cfg.actor).await
        }
        Cmd::Import {
            dir,
            prefix,
            dry_run,
        } => {
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
            let svc = NoteService::new(Arc::new(repo));
            let summary = transfer::export(&svc, &dir, prefix.as_deref()).await?;
            println!("{summary}");
            Ok(())
        }
        Cmd::Serve => {
            repo.migrate().await?;
            let svc = Arc::new(NoteService::new(Arc::new(repo)));
            server::run_http(svc, cfg).await
        }
    }
}
