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

## Creating

Omit `expected_version` to say "this note must not exist yet". If it does,
the result is a conflict carrying the current version, which tells you to
read it instead.

## Writing the same body again

A `note_put` whose body is identical to the current one changes nothing:
the result says `changed: false`, the version stays, and no history entry
is written. Retrying a put that may already have landed is therefore safe.

## Deleting

`note_delete { path, expected_version? }`: pass the version you read to
refuse if someone wrote in between; omit it to delete whatever is there
(unlike `note_put`, where omitting it means "create"). The removed body is
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
