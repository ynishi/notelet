# Locking

notelet has one concurrency rule: a `note_put` replaces the body only if
the note's `version` is still the one the caller read. There is no lease,
no explicit lock, no queue.

## The cycle

```
note_get  { path }                        -> { body, version: 7, ... }
...edit the body locally...
note_put  { path, body, expected_version: 7 }
   -> { version: 8, changed: true, sha256, ... }   (nobody wrote in between)
   -> { kind: "Conflict", expected_version: 7, current_version: 9 }   (someone did)
```

Never `note_put` a body you read with `truncated: true` (a `note_get` with
`tail_lines` / `max_bytes` that returned only the end): it would replace the
whole note with the fragment. Journals are extended with `note_append`.

On a conflict, read again, merge your change into the current body, and
put with the new version. Do not retry with the old body; that is how a
colleague's paragraph disappears.

## Conflicts through files (local mode)

With `note_put { path, from_file, expected_version, on_conflict: "mark" }` a conflict also rewrites `from_file`, using git's words with three differences: whole bodies are compared (no hunks), nothing is merged for you, and nothing is stored beyond the marker lines in the file.

```
<<<<<<< ours (local, 12 lines)
<the whole file you sent>
||||||| base (v7, 3 lines)
<the body of v7, the version you read>
=======
<the current body>
>>>>>>> theirs (v9, 4 lines)
```

Each header counts the lines of its section (a line ends at `\n`; `\r\n` counts as one line too), and the parser reads exactly that many, so a body may contain any line, `=======` and git markers included. A body that does not end with a newline gets one for the marker's sake and its header says `, no-eol` (or `, no-eol-cr` when the body itself ends with `\r`); `note_restore` removes it again, also when an editor has converted the file to CRLF; for sections without `no-eol` the restore after such a conversion returns the CRLF endings the editor wrote. The base section is left out when `expected_version` was omitted or that revision is no longer in `note_history` (versions are never reused, so a version names one body). The file is replaced atomically; if it changed while the put ran, it is not marked.

The error is still `kind: "Conflict"` with `current_version`, plus `marked: true`, `file`, and `base_version` when a base section was written (`marked: false` and the reason in `message` when the file could not be marked).

Resolve it by editing the file, or by `note_restore { file, source }` with `"ours"`, `"theirs"` or `"base"`, which gives back that body exactly. The answer has `expected_version`, always the theirs version (`current_version`): pass it to the next put. It also has `section_version`, the version the restored body belongs to: theirs or base, null for ours. Restoring ours and putting overwrites theirs, as `git checkout --ours` does; restore theirs and edit if you want to keep their work. A file whose headers or counts do not fit is refused and left unchanged. `note_put` refuses a `from_file` with a line that starts with `<<<<<<< ours (local`, `||||||| base (v` or `>>>>>>> theirs (v`; a bare `=======` or a quoted git conflict is fine.

## Creating

Omit `expected_version` to say "this note must not exist yet". If it does,
the result is a conflict carrying the current version, which tells you to
read it instead. Two creates racing for a new path end the same way: one
wins, the other gets that conflict.

## Writing the same body again

A `note_put` whose body is identical to the current one changes nothing:
the result says `changed: false`, the version stays, and no history entry
is written. Retrying a put that may already have landed is therefore safe.

## Deleting

`note_delete { path, expected_version }`: `expected_version` is required, the
version you read; the delete is refused with `Conflict` if someone wrote in
between (a call without it is `Validation`). The removed body is
kept as the last entry of `note_history`; that entry names the last writer
of the body, not who deleted it (the deleter is not recorded). A note created again at that path
continues from the next version, so history never has two entries with the
same version.

## Why append needs no version

`note_append` only adds text at the end, on its own line. Two appends that
race both land; neither removes anything. That is why journals use append
and never put.

## History

Every body that `note_put` replaces or `note_delete` removes is kept under
its old version, readable with `note_history`. Appends do not create history entries: the previous
content is a prefix of the new one, so nothing was lost.
