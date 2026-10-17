# Filters and paging

`note_list` narrows with a prefix and a list of predicates. There is no
query language: each predicate is `{ field, op, value }`, and predicates
are ANDed.

## Fields

| field | compared as |
|---|---|
| `path` | text |
| `updated_by` | text |
| `updated_at` | timestamp; the value must be RFC 3339 (`2026-10-12T00:00:00Z`) |

## Operators

`eq` `ne` `gt` `lt` `gte` `lte` compare the whole value. `like` and
`ilike` (case-insensitive) treat the value as a SQL LIKE pattern: `%` is
any run of characters, `_` is one character, `\` escapes either. They do
not apply to `updated_at`.

```
{ "field": "path", "op": "like", "value": "projects/%/journal.md" }
{ "field": "updated_by", "op": "ne", "value": "bot" }
{ "field": "updated_at", "op": "gte", "value": "2026-10-01T00:00:00Z" }
```

Values are always bound as parameters, never spliced into SQL, so a value
can contain anything; only `like` / `ilike` interpret `%` and `_`.

## Prefix and search text are literal

The `prefix` of `note_list` / `note_search` / `note_dump` and the `text` of
`note_search` are escaped before use: a `%` in them matches a `%`.

## Paging

Results are ordered by path. A page carries `next_cursor` when more rows
follow; pass it back as `cursor` to continue. The cursor is exclusive: it
is the last path of the page, and the next page starts strictly after it,
so paging is stable while notes are being added. A `limit` outside
1..=1000 (default 100) is refused with `kind: "Validation"`, not clamped;
`note_dump` pages are 1..=100 (default 20) because
every item carries a body, and `note_history` takes 1..=50 (default 20). `size` is in bytes (UTF-8).
