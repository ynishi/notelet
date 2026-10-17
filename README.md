# notelet

A thin, hosted store for Markdown notes that coding agents read and write over MCP. One table of notes keyed by path, one append-only history table, optimistic locking, a filter that is a thin wrapper over SQL, and the tools listed below. Nothing else.

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

The list is generated from the tool router (also served as `notelet://guides/tools`); run `just gen-docs` after changing a tool.

<!-- tools:begin -->
| tool | does | local only |
|---|---|---|
| `note_append` | Append text to the end of a note (created if missing), on its own line. | |
| `note_delete` | Remove a note. | |
| `note_dump` | Paths and full bodies under a prefix, ordered by path and paged: default 20, max 100 (bodies are heavy); pass next_cursor as cursor (exclusive). | |
| `note_get` | Read one note: path, body (Markdown), version, updated_at, updated_by, size, truncated. | |
| `note_history` | Bodies that note_put replaced or note_delete removed for this path, newest first (default 20, max 50). | |
| `note_list` | List notes (path, version, updated_at, updated_by, size in bytes (UTF-8); no body) under a prefix, filtered by ANDed predicates {field, op, value}. | |
| `note_put` | Create or replace a note's whole body with optimistic locking. | |
| `note_search` | Find notes whose body contains the text (case-insensitive substring, no wildcards) under a prefix. | |
<!-- tools:end -->

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
export NOTELET_TOKEN=...                        # one shared token; or NOTELET_TOKEN_FILE=~/.notelet/token
export NOTELET_TOKENS_FILE=~/.notelet/tokens    # one token per client (below); or NOTELET_TOKENS="alice:<sha256> bob:<sha256>"
export NOTELET_BIND=127.0.0.1:8490              # default; non-loopback requires a token
export RUST_LOG=info                            # log level; default info
notelet-mcp serve                               # streamable HTTP at /mcp, /healthz open
notelet-mcp stdio                               # one local client over stdin/stdout
notelet-mcp migrate                             # apply migrations and exit
notelet-mcp import ./notes --prefix projects/x  # file tree -> notes (--dry-run to preview)
notelet-mcp export ./backup                     # notes -> file tree
notelet-mcp token alice                         # new token for a client + its NOTELET_TOKENS entry (no database needed)
```

### One token per client

`updated_by` records which credential authenticated a write. The identity is the token: no users table, no tool or header to declare a name. Give each client its own token and map it to an actor name:

1. `notelet-mcp token alice` prints two lines. The `token` (`nlt_` and 64 hex digits) goes to the client (its `Authorization: Bearer`, see below); the `entry` line, `alice:<sha256 of the token>`, goes to the server.
2. The server reads entries from `NOTELET_TOKENS` or, if `NOTELET_TOKENS_FILE` is set, from that file (it wins). Entries are separated by any whitespace; a line starting with `#` is a comment, in the file and in the variable alike, and a leading BOM is ignored. Pasting a token (`nlt_...`) instead of its digest is refused at startup. The server stores digests, never tokens. An actor (also `NOTELET_ACTOR`) is `[A-Za-z0-9._-]{1,64}`; a digest listed twice stops the server at startup.
3. Rotate by adding a second entry for the same actor, moving the client to the new token, then removing the old entry. Revoke by removing the entry and restarting: the list is read once at startup.

The single `NOTELET_TOKEN` / `NOTELET_TOKEN_FILE` still works and joins the same table, written as `NOTELET_ACTOR` (default `mcp`); `NOTELET_ACTOR` is also the actor for `stdio`, `import` and a loopback `serve` without tokens. Clients that share one token are indistinguishable in `updated_by`. `note_delete` records no actor: the kept revision names the last writer.

Moving a running deployment from the single token to one token per client, without a moment where a client is refused:

1. Run a version that reads `NOTELET_TOKENS` (0.3.0 or later), still with the single token set.
2. Issue a token per client and add all the entries to `NOTELET_TOKENS` next to the single token; restart. Both credentials now work.
3. Move each client to its own token. Until it moves, its writes keep the `NOTELET_ACTOR` name.
4. When every client has moved, remove the single token and restart. A client still holding it now gets `401`.

Migrations are embedded and applied on start.

Logs go to stderr (`stdout` is the protocol in `stdio` mode). `RUST_LOG` selects the level (default `info`). `rmcp` is held at `info` and `sqlx` at `warn` unless `RUST_LOG` names them, because rmcp's `debug` output includes whole requests and results.

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

## Browser view

`serve` also answers a read-only HTML view of the store under `/ui/`, for a person who wants to read notes without an MCP client:

| route (GET) | shows |
|---|---|
| `/ui/` | top-level prefixes (`projects/`, `tasks/`, `lists/` and any other that exists) and a search form |
| `/ui/tree/<prefix>` | one page of `note_list` under the prefix (path, size in bytes, updated_at), the prefixes below it, a `next` link |
| `/ui/note/<path>` | the note rendered from Markdown, with version, updated_at and size |
| `/ui/search?q=&prefix=` | `note_search` hits with snippets |

When tokens are configured, the browser asks for HTTP Basic credentials: any user name, any listed token as the password (looked up by its sha256 digest; a failure is `401` with `WWW-Authenticate: Basic realm="notelet", charset="UTF-8"`). Without tokens (loopback binds only) there is no credential, so both `/mcp` and `/ui` accept only a loopback `Host` value (`localhost`, `127.0.0.1` or `[::1]`, any port) and answer `403` otherwise. `/mcp` keeps its bearer check and `/healthz` stays open.

The view is a projection, not part of the model: it calls the same `get` / `list` / `search` the tools do, has no route that writes, and adds no tool. Raw HTML in a note is shown as text, links keep only `http`, `https`, `mailto` or relative URLs, images are shown as links rather than loaded, pages carry no script and no external asset, and every page is sent with `Cache-Control: no-store`.

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
