# Onboarding

notelet is a store of Markdown notes addressed by path. There are no
directories, only a naming convention the prefix listing makes useful:

```
projects/<repo>/journal.md      append-only working log, one per repository
projects/<repo>/drafts/<x>.md   design drafts
tasks/<task>/<file>.md          analysis and task notes
lists/<name>.md                 private lists the agent consults
```

A path is slash-separated segments of `[A-Za-z0-9._-]`. Nothing else is
accepted: no spaces, no `..`, no leading slash.

Nested projects keep their `/` as is and may go deep:
`projects/x/sub-a/journal.md` is a fine path. Narrow with a prefix
(`note_list { prefix: "projects/x/sub-a/" }`) rather than flattening the
name.

Sizes (`size` in listings, history and write results) are bytes of UTF-8,
not characters. Cursors are exclusive: a page starts strictly after the
path you pass.

## One pass through the tools

1. **Write a journal line.** `note_append { path: "projects/x/journal.md", text: "2026-10-12 started the migration" }`. The note is created if missing; the text lands on its own line. No version is needed. At session start, read the journal's end with `note_get { path: "projects/x/journal.md", tail_lines: 20, max_bytes: 8000 }` (or similar); `size` and `truncated` tell you it is partial.
2. **Read a document.** `note_get { path: "tasks/t/plan.md" }` returns the body and its `version`. Keep the version. For the end of a long note use `note_get { path, tail_lines: 20, max_bytes: 8000 }`: `tail_lines` selects the last lines, `max_bytes` caps what comes back (cut on a line start where possible), and the answer's `size` (the whole body in bytes) and `truncated` say it is partial. Never `note_put` a body read with `truncated: true`.
3. **Replace a document.** `note_put { path, body, expected_version: <the version you read> }`. If someone wrote in between you get `kind: "Conflict"` with `current_version`; read again, merge, retry. To create a document that must not exist yet, omit `expected_version`. Every write answers `{ path, version, updated_at, size, sha256, changed }`; compare `size` / `sha256` with what you sent instead of reading the body back.
4. **Find things.** `note_list { prefix: "projects/x/" }` for metadata (no bodies); add `filters` to narrow (see `notelet://guides/filters`). `note_search { text: "release-plz" }` for a case-insensitive substring over bodies, with a snippet.
5. **Recover.** `note_history { path }` lists the bodies that `note_put` replaced or `note_delete` removed, newest first; `include_body: false` lists only their metadata.
6. **Leave or back up.** `note_dump { prefix }` returns paths and full bodies, paged (default 20, max 100 per page); written to disk it is a file tree.

## Files instead of context (local mode)

When the operator sets `NOTELET_FILE_ROOT` (an existing directory, outside any repository checkout, since note bodies are private), a body can go through a file instead of through your context. This works over `stdio`, and on a `serve` that is bound to a loopback address and has a token; with no root, or on a hosted server, these parameters and `note_restore` are not listed. File names are relative to the root; absolute names, `..`, a trailing `/` and links that lead out of the root are refused.

- `note_get { path, to_file: "work/doc.md" }` writes the body to the file (overwriting it, unless it holds an unresolved conflict; restore or delete that first) and answers `version`, `size` and `sha256` without `body`. It cannot be combined with `tail_lines` or `max_bytes`: a partial body in a file would later replace the whole note.
- `note_put { path, from_file: "work/doc.md", expected_version }` reads the new body from the file (give exactly one of `body` and `from_file`). Locking is unchanged; compare the answer's `sha256` with the file's.
- When the put loses the lock, `on_conflict: "mark"` rewrites the file in git's marker form with line counts: `ours` is your file, `base` the version you read (when `note_history` still has it), `theirs` the current body. `note_restore { file, source: "ours" | "theirs" | "base" }` turns it back into one side, byte for byte, and answers `expected_version` (the theirs version, for the next put) and `section_version`. A file with notelet's own header lines is never put. See `notelet://guides/locking`.

## Bulk import and export

Moving a whole file tree in or out is not done through MCP tools (it would
pass every body through an agent's context). The operator runs the binary
against the database instead:

```
notelet-mcp import <dir> [--prefix projects/x] [--dry-run]
notelet-mcp export <dir> [--prefix projects/x]
```

`import` maps each file to `<prefix>/<path relative to dir>`, replaces
existing notes at their current version (identical bodies are left
alone), skips hidden files, symlinks, non-UTF-8 files and invalid paths,
and prints `created / updated / unchanged / skipped / failed` counts.
`export` writes every note to `<dir>/<path>`.

## What notelet does not do

Its tools do not render, link, or structure notes. It does not know about users.
It does not search with a query language or an index. If you need any of
those, build them next to notelet on top of `note_dump`, not inside it.

A human can read the store in a browser at `/ui/` on `serve` (read-only, HTTP Basic with a token as the password); it is a projection for people, not a tool, so agents keep using the tools above.

`note_put` and `note_append` record `updated_by`: the actor of the token the client authenticated with (one token per client, chosen by the operator), or the server's fallback name when the server runs without tokens or over stdio. You cannot set it yourself, and two clients sharing one token look the same. `note_delete` keeps the last writer's revision and does not name who deleted.
