# Changelog

All notable changes to this project are documented here. The format follows Keep a Changelog; versions follow Semantic Versioning.

## [Unreleased]

### Added
- `notelet-core`: `Note` / `Revision` / `Filter` / `Page` domain, `NoteRepo` port, `NoteService`, Postgres adapter with optimistic locking and revisions, in-memory adapter, embedded migrations.
- `notelet-mcp`: streamable HTTP and stdio transports, bearer middleware, seven tools (`note_get`, `note_put`, `note_append`, `note_list`, `note_search`, `note_history`, `note_dump`), three guide resources.
- `justfile`, CI (fmt / clippy / test), Fly configuration placeholders, Dockerfile.
