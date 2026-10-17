# Tools

Every tool the server offers, generated from the tool router. Run `just gen-docs` after changing a tool.

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
