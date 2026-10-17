# Tools

Every tool the server offers, generated from the tool router. Run `just gen-docs` after changing a tool. The last four columns are the tool annotations in `tools/list`: `-` means the hint is not set because it only applies to a tool that is not read-only.

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
| `note_search` | Find notes whose body contains the text (case-insensitive substring, no wildcards) under a prefix. | | yes | - | - | no |
