# notelet

A thin, hosted store for Markdown notes that coding agents read and write over MCP. One table of notes keyed by path, one append-only history table, optimistic locking, a filter that is a thin wrapper over SQL, and the tools listed below. Nothing else.

## Why

Agents accumulate working state: journals, design drafts, private lists. Keep it as files inside a repository checkout and it leaks (an agent quotes a path or a paragraph into a public file) and scatters (every host, worktree and archive ends up with a different copy). Keep it in a feature-rich knowledge tool and the tool costs more to maintain than the notes are worth. notelet is the minimum in between, and it is reachable from every harness and account you use because it is just an HTTP endpoint with a bearer token.

## What it is

```
Note        path -> body (Markdown), version, updated_at, updated_by
Revision    the body a put replaced or a delete removed, keyed by (path, version)
Filter      field (path | updated_at | updated_by) x op (eq ne like ilike gt lt gte lte) x value
Page        limit (1..=1000; dump 1..=100; history 1..=50; out of range is refused) + exclusive keyset cursor
```

Paths are slash-separated segments of `[A-Za-z0-9._-]`; the tree (`projects/<repo>/journal.md`, `tasks/<task>/...`, `lists/<name>.md`) is a naming convention served by the prefix listing, not a directory object.

The list is generated from the tool router (also served as `notelet://guides/tools`); run `just gen-docs` after changing a tool.

<!-- tools:begin -->
| tool | does | local only | read-only | destructive | idempotent | open world |
|---|---|---|---|---|---|---|
| `note_append` | Append text to the end of a note (created if missing), on its own line. | | no | no | no | no |
| `note_delete` | Remove a note. | | no | yes | yes | no |
| `note_dump` | Paths and full bodies under a prefix, ordered by path and paged: default 20, max 100 (bodies are heavy; a limit outside 1..=100 is refused with kind=Validation); pass next_cursor as cursor (exclusive). | | yes | - | - | no |
| `note_get` | Read one note: path, body (Markdown), version, updated_at, updated_by, size, truncated. | `to_file` | yes (local: no) | - (local: yes) | - (local: yes) | no |
| `note_history` | Bodies that note_put replaced or note_delete removed for this path, newest first (default 20, max 50; a limit outside 1..=50 is refused with kind=Validation). | | yes | - | - | no |
| `note_list` | List notes (path, version, updated_at, updated_by, size in bytes (UTF-8); no body) under a prefix, filtered by ANDed predicates {field, op, value}. | | yes | - | - | no |
| `note_put` | Create or replace a note's whole body with optimistic locking. | `from_file`, `on_conflict` | no | yes | yes | no |
| `note_restore` | Rewrite a conflict file (written by note_put with on_conflict="mark") to one section's body, exactly as it was before marking; local mode only. | the tool | no | yes | yes | no |
| `note_search` | Find notes whose path or body contains the text (case-insensitive substring, no wildcards) under a prefix. | | yes | - | - | no |
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

Requirements: a Postgres database, or (for `serve` and `stdio` only) another notelet to forward to. Local development points at a hosted dev database; there is no local Docker setup to maintain.

```sh
export NOTELET_DATABASE_URL=postgres://...      # or DATABASE_URL
export NOTELET_TOKEN=...                        # one shared token; or NOTELET_TOKEN_FILE=~/.notelet/token
export NOTELET_TOKENS_FILE=~/.notelet/tokens    # one token per client (below); or NOTELET_TOKENS="alice:<sha256> bob:<sha256>"
export NOTELET_BIND=127.0.0.1:8490              # default; non-loopback requires a token
export NOTELET_FILE_ROOT=/path/to/work          # an existing directory outside any repository checkout: switches on local mode (to_file / from_file / note_restore) for stdio, and for a loopback serve with a token; unset = no file access
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

### Using another notelet as the store

Instead of a database, `serve` and `stdio` can forward every read and write to a hosted notelet over MCP, so a laptop keeps its own endpoint (`serve` also keeps `/ui`) without a database or the database URL:

```sh
export NOTELET_REMOTE_URL=https://<host>/mcp      # instead of NOTELET_DATABASE_URL
export NOTELET_REMOTE_TOKEN_FILE=~/.notelet/token # or NOTELET_REMOTE_TOKEN; the file wins
notelet-mcp stdio
```

Setting both a remote URL and a database URL stops startup; a global `DATABASE_URL` counts, so set `NOTELET_DATABASE_URL=` to empty to mask it. `migrate`, `import` and `export` need the database and fail if `NOTELET_REMOTE_URL` is set; no migrations run in remote mode. Writes are recorded under the actor of the remote token: the remote cannot be told another name, so give each client machine its own token there (the local `NOTELET_TOKENS` / `NOTELET_TOKEN` only decide who may call this process; `NOTELET_ACTOR` has no effect on the remote). The process holds one MCP session to the remote for all its local sessions (re-opened if the remote expires it), does not start if the remote is unreachable (except `stdio` with the local cache below on), and gives up on a connect or a call after 30 seconds (not configurable; the cancel rmcp posts after a timed-out call is bounded by 2 seconds, so a stalled call costs about 32 seconds, and N concurrent stalled calls about 30 seconds plus 2 per call, since cancels go out one at a time). A transport error message names the remote URL (for your own log and client); the token never appears. `NOTELET_REMOTE_TOKEN` is the credential this process presents; `NOTELET_TOKEN` is one it accepts. `append` and `delete` take two calls to the remote (a read-back, and a read before the delete), so they are not atomic with other writers beyond what the remote's version check gives.

#### Local cache

A remote read takes one round trip (about 1-2 s), which is slow for a client that reads notes on the hot path. `NOTELET_LOCAL_CACHE=true` keeps a local copy of the remote's notes and answers `note_get`, `note_list`, `note_search` and `note_dump` from it. It works only under `stdio` with `NOTELET_REMOTE_URL`: that is the local client (such as Claude Code) talking to the hosted notelet. `serve` does not use it, and with a database backend the switch is unused. Only `stdio` with a remote reads the variable, as a boolean (`true` or `false`; unset or empty is `false`; under `stdio` with a remote anything else stops startup). Every other command ignores it. It is off by default because it writes note bodies to disk.

```sh
export NOTELET_REMOTE_URL=https://<host>/mcp
export NOTELET_REMOTE_TOKEN_FILE=~/.notelet/token
export NOTELET_LOCAL_CACHE=true                   # unset or false = off
notelet-mcp stdio
```

- **Writes** go to the remote, and when it accepts them the copy is updated, so a client reads its own writes at once. After a `note_put` the actor (`updated_by`) is blank in the cache until the next poll lists the note (up to 30 seconds): `note_get` shows it empty, and a `note_list` filtered on `updated_by` does not match the note yet. An unreachable remote fails the write; nothing is queued. A version conflict refreshes that note in the copy, so the retry reads the current version. `note_history` is not cached.
- **Other clients' writes** show up after at most one poll (every 30 s) plus a round trip. A delete by another client shows up after at most about 5 minutes (every 10th poll lists all paths), or at once when a read or write meets `NotFound`. Tool answers do not say how old the data is.
- **Start** does not wait for the remote: with a snapshot on disk, reads are answered from it at once and the copy refills in the background; without one, reads go to the remote until the first fill. A call that needs the remote while it is not connected (a read before the first fill, `note_history`, a write) connects first, sharing the attempt with the background task, so it waits for that connect (30 seconds at most) rather than failing at once. One lost call fails only that call, as without the cache. While the remote is unreachable the background task retries with a back-off (30 s doubling to 5 minutes) that never delays a call, reads of a filled copy keep working with the last synced state, and writes fail.
- **On disk**: `$XDG_CACHE_HOME/notelet/<key>/` (else `$HOME/.cache/notelet/<key>/`, on macOS `~/Library/Caches/notelet/<key>/`), where `<key>` is the first 16 hex digits of the sha256 of the remote URL, so the host name is not in the path. The directory is created `0700` and its files `0600` (an existing one is tightened). One process holds a lock on `<dir>/lock` and writes `notes.json` (at most every 30 s, after a change); the others read it once at start and keep their copy in memory. A final write happens when `stdio` ends at stdin EOF; on a signal up to 30 seconds of changes are not written, which loses nothing (the writes are on the remote) and the next start's relist repairs it. Delete the directory to start over.

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

On a note page every heading has an `id`, a `¶` link to it, and right after it a reference of its section, `notelet:<path>@v<version>#L<first>-L<last> "<heading line>"`; the whole note's `notelet:<path>@v<version>` is at the top. Each is a `<code>` element that one click selects, for pasting to an agent. Lines are 1-based lines of the body as `note_get` returns it; `notelet://guides/onboarding` says how an agent resolves one (including a version that `note_history` does not hold because only `note_append` moved the note on: appends add lines at the end, so the range holds as given). It is plain text: no script, same Content-Security-Policy.

The view is a projection, not part of the model: it calls the same `get` / `list` / `search` the tools do, has no route that writes, and adds no tool. Raw HTML in a note is shown as text, links keep only `http`, `https`, `mailto` or relative URLs, images are shown as links rather than loaded, pages carry no script and no external asset, and every page is sent with `Cache-Control: no-store`.

## Claude Code plugin

`notelet-links` shows a link above the prompt to each note the session wrote, and `/notes` lists them. It runs on the client only.

```
/plugin marketplace add ynishi/notelet
/plugin install notelet-links@notelet
```

See [plugins/notelet-links/README.md](plugins/notelet-links/README.md).

## Layout

```
crates/notelet        umbrella: re-exports notelet-core under the project name (depend on this from library code)
crates/notelet-core   domain, port (NoteRepo), service, Postgres adapter, in-memory adapter, migrations
crates/notelet-mcp    the binary: config from env, bearer middleware, rmcp tools + resources, HTTP / stdio
plugins/notelet-links Claude Code plugin: links to the notes a session wrote (client side only)
```

Design notes are in the crate docs (`cargo doc --open`), starting at `notelet_core`. The code documentation is the design document; there is no separate one to drift.

## Develop

```sh
just check           # fmt + clippy
just test            # unit tests (no database needed)
just test-pg-local   # Postgres integration test on a throwaway Docker Postgres
just pg-up / pg-down # start / remove that container by hand (test-pg uses it unless NOTELET_TEST_DATABASE_URL is set)
just dev             # run `serve` against the dev database in the environment
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [PUBLIC_DEVELOPMENT.md](PUBLIC_DEVELOPMENT.md).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
