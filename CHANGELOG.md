# Changelog

All notable changes to this project are documented here. The format follows Keep a Changelog; versions follow Semantic Versioning.

## [Unreleased]

## [0.8.0](https://github.com/ynishi/notelet/compare/v0.7.0...v0.8.0) - 2026-10-21

### Added

- cache reads in remote mode behind NOTELET_REMOTE_CACHE

### Fixed

- rename the remote read cache to LocalCache and confine it to stdio

## [0.7.0](https://github.com/ynishi/notelet/compare/v0.6.0...v0.7.0) - 2026-10-19

### Added

- *(ui)* group path matches above body hits in search

## [0.6.0](https://github.com/ynishi/notelet/compare/v0.5.0...v0.6.0) - 2026-10-18

### Added

- match the search text against the note path as well as the body
- bundle a Claude Code plugin that links the notes a session wrote to /ui
- show a copyable path@version#lines reference for each section in /ui

### Other

- run the Postgres integration test on a throwaway Docker container

## [0.5.0](https://github.com/ynishi/notelet/compare/v0.4.0...v0.5.0) - 2026-10-17

### Added

- declare tool annotations and state allowed values in input schemas
- keep the store's contract under concurrent writes

### Fixed

- keep notes private and intact through import / export

## [0.4.0](https://github.com/ynishi/notelet/compare/v0.3.0...v0.4.0) - 2026-10-17

### Added

- project a note onto a local file in note_get / note_put, with git-style conflict files and note_restore
- add a remote backend for serve and stdio

### Fixed

- make the actor span test independent of other tests' tracing state

### Other

- Generate the tool list in the docs from the tool router

## [0.3.0](https://github.com/ynishi/notelet/compare/v0.2.0...v0.3.0) - 2026-10-15

### Added

- record the authenticating token's actor in updated_by

### Other

- Describe moving a deployment from the single token to one token per client

## [0.2.0](https://github.com/ynishi/notelet/compare/v0.1.1...v0.2.0) - 2026-10-13

### Added

- keep the cause in CoreError and log through tracing instead of eprintln
- add tail_lines and max_bytes to note_get for reading the end of a note
- read-only browser view at /ui with HTTP Basic auth

### Fixed

- breadcrumb links show only their own segment; deploy-dev takes the app name
- loopback Host check on /ui without a token, plus boundary tests

## [0.1.1](https://github.com/ynishi/notelet/compare/v0.1.0...v0.1.1) - 2026-10-12

### Other

- Shorten the notelet-mcp description so the Homebrew formula passes brew style
- Add the notelet umbrella crate

## [0.1.0] - 2026-10-12

### Added
- `notelet-core`: `Note` / `Revision` / `Filter` / `Page` domain, `NoteRepo` port, `NoteService`, Postgres adapter with optimistic locking and revisions, in-memory adapter, embedded migrations.
- `notelet-mcp`: streamable HTTP and stdio transports, bearer middleware, eight tools (`note_get`, `note_put`, `note_append`, `note_delete`, `note_list`, `note_search`, `note_history`, `note_dump`), three guide resources.
- `justfile`, CI (fmt / clippy / test), Fly configuration placeholders, Dockerfile.
- `note_delete { path, expected_version? }` keeps the removed body as the last revision, readable through `note_history`; without `expected_version` it deletes whatever is there.
- `note_history` takes `include_body` (default `true`); `false` returns metadata with the size in bytes.
- `notelet-mcp import <dir> [--prefix] [--dry-run]` and `notelet-mcp export <dir> [--prefix]`: operator bulk transfer between a file tree and the store (not MCP tools).

### Changed
- `note_put`, `note_append` and `note_delete` answer a `WriteOutcome`: `path`, `version`, `updated_at`, `size` (bytes), `sha256`, `changed`.
- `note_put` with a body identical to the current one is a no-op (`changed: false`, no version bump, no revision).
- `note_list` and `note_history` report `size` in bytes (UTF-8) in both adapters.
- `note_dump` pages default to 20 and are capped at 100.
- A conflict error carries `expected_version` (`null` on create) next to `current_version`, and its message reads `expected none` / `expected 7`.
- A note re-created at a path that still has revisions starts one past the highest of them, so versions never repeat within a path.
