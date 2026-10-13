# Changelog

All notable changes to this project are documented here. The format follows Keep a Changelog; versions follow Semantic Versioning.

## [Unreleased]

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
