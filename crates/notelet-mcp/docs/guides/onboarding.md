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

## One pass through the tools

1. **Write a journal line.** `note_append { path: "projects/x/journal.md", text: "2026-10-12 started the migration" }`. The note is created if missing; the text lands on its own line. No version is needed.
2. **Read a document.** `note_get { path: "tasks/t/plan.md" }` returns the body and its `version`. Keep the version.
3. **Replace a document.** `note_put { path, body, expected_version: <the version you read> }`. If someone wrote in between you get `kind: "Conflict"` with `current_version`; read again, merge, retry. To create a document that must not exist yet, omit `expected_version`.
4. **Find things.** `note_list { prefix: "projects/x/" }` for metadata (no bodies); add `filters` to narrow (see `notelet://guides/filters`). `note_search { text: "release-plz" }` for a case-insensitive substring over bodies, with a snippet.
5. **Recover.** `note_history { path }` lists the bodies that `note_put` replaced, newest first.
6. **Leave or back up.** `note_dump { prefix }` returns paths and full bodies, paged; written to disk it is a file tree.

## What notelet does not do

It does not render, link, or structure notes. It does not know about users.
It does not search with a query language or an index. If you need any of
those, build them next to notelet on top of `note_dump`, not inside it.
