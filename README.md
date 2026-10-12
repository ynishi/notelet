# notelet

A thin, hosted store for Markdown notes that coding agents read and write over MCP. One table of notes keyed by path, one append-only history table, optimistic locking, a filter that is a thin wrapper over SQL, and eight tools. Nothing else.

## Why

Agents accumulate working state: journals, design drafts, private lists. Keep it as files inside a repository checkout and it leaks (an agent quotes a path or a paragraph into a public file) and scatters (every host, worktree and archive ends up with a different copy). Keep it in a feature-rich knowledge tool and the tool costs more to maintain than the notes are worth. notelet is the minimum in between, and it is reachable from every harness and account you use because it is just an HTTP endpoint with a bearer token.

## What it is

```
Note        path -> body (Markdown), version, updated_at, updated_by
Revision    the body a put replaced or a delete removed, keyed by (path, version)
Filter      field (path | updated_at | updated_by) x op (eq ne like ilike gt lt gte lte) x value
Page        limit (1..=1000; dump 1..=100) + exclusive keyset cursor
```

Paths are slash-separated segments of `[A-Za-z0-9._-]`; the tree (`projects/<repo>/journal.md`, `tasks/<task>/...`, `lists/<name>.md`) is a naming convention served by the prefix listing, not a directory object.

| tool | does |
|---|---|
| `note_get` | read one note with its version |
| `note_put` | create or replace the whole body; `expected_version` must match or you get `Conflict` with `current_version`; an identical body is a no-op (`changed: false`) |
| `note_append` | add a line at the end, no version needed (journals, or uploading a large body in chunks) |
| `note_delete` | remove a note; its body stays readable through `note_history` |
| `note_list` | metadata (size in bytes) under a prefix, filtered, paged |
| `note_search` | case-insensitive substring over bodies, with a snippet |
| `note_history` | bodies that `note_put` replaced or `note_delete` removed (`include_body: false` for metadata only) |
| `note_dump` | paths and bodies under a prefix: a file tree, for backup or for leaving |

Writes answer `{ path, version, updated_at, size, sha256, changed }` so a caller can verify a round trip without reading the body back.

Bulk loading is not a tool: the operator runs `notelet-mcp import <dir> [--prefix <p>] [--dry-run]` and `notelet-mcp export <dir> [--prefix <p>]` against the database directly.

The guides the server publishes as MCP resources (`notelet://guides/onboarding`, `locking`, `filters`) are the usage documentation; this README does not repeat them.

## Install

```sh
cargo install notelet-mcp          # binary: notelet-mcp
```

Or a prebuilt binary from the [latest release](https://github.com/ynishi/notelet/releases/latest)
— macOS (Apple Silicon, Intel), Linux (aarch64, x86_64) and Windows (x86_64). The
installers put `notelet-mcp` in `$CARGO_HOME/bin`, where `cargo install` would:

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ynishi/notelet/releases/latest/download/notelet-mcp-installer.sh | sh
brew install ynishi/tap/notelet-mcp
powershell -ExecutionPolicy Bypass -c "irm https://github.com/ynishi/notelet/releases/latest/download/notelet-mcp-installer.ps1 | iex"
```

## Run

Requirements: a Postgres database. Local development points at a hosted dev database; there is no local Docker setup to maintain.

```sh
export NOTELET_DATABASE_URL=postgres://...      # or DATABASE_URL
export NOTELET_TOKEN=...                        # or NOTELET_TOKEN_FILE=~/.notelet/token
export NOTELET_BIND=127.0.0.1:8490              # default; non-loopback requires a token
notelet-mcp serve                               # streamable HTTP at /mcp, /healthz open
notelet-mcp stdio                               # one local client over stdin/stdout
notelet-mcp migrate                             # apply migrations and exit
notelet-mcp import ./notes --prefix projects/x  # file tree -> notes (--dry-run to preview)
notelet-mcp export ./backup                     # notes -> file tree
```

Migrations are embedded and applied on start.

### Clients

Claude Code (user scope; `headersHelper` runs a command at connect time and merges its JSON output into the request headers, so the token is never written into a config file; see the [MCP documentation](https://code.claude.com/docs/en/mcp)):

```json
{ "mcpServers": { "notelet": {
    "type": "http", "url": "https://<host>/mcp",
    "headersHelper": "sh -c 'printf \"{\\\"Authorization\\\":\\\"Bearer %s\\\"}\" \"$(tr -d \"\\n\" < $HOME/.notelet/token)\"'"
} } }
```

Codex (`~/.codex/config.toml`; `bearer_token_env_var` sends the named environment variable as `Authorization: Bearer`, see the [config reference](https://learn.chatgpt.com/docs/config-file/config-reference)):

```toml
[mcp_servers.notelet]
url = "https://<host>/mcp"
bearer_token_env_var = "NOTELET_TOKEN"
```

## Layout

```
crates/notelet        umbrella: re-exports notelet-core under the project name (depend on this from library code)
crates/notelet-core   domain, port (NoteRepo), service, Postgres adapter, in-memory adapter, migrations
crates/notelet-mcp    the binary: config from env, bearer middleware, rmcp tools + resources, HTTP / stdio
```

Design notes are in the crate docs (`cargo doc --open`), starting at `notelet_core`. The code documentation is the design document; there is no separate one to drift.

## Develop

```sh
just check          # fmt + clippy
just test           # unit tests (no database needed)
NOTELET_TEST_DATABASE_URL=postgres://... just test-pg   # Postgres integration test
just dev            # run `serve` against the dev database in the environment
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
