# Locking

notelet has one concurrency rule: a `note_put` replaces the body only if
the note's `version` is still the one the caller read. There is no lease,
no explicit lock, no queue.

## The cycle

```
note_get  { path }                        -> { body, version: 7, ... }
...edit the body locally...
note_put  { path, body, expected_version: 7 }
   -> { version: 8 }                      (nobody wrote in between)
   -> { kind: "Conflict", current_version: 9 }   (someone did)
```

On a conflict, read again, merge your change into the current body, and
put with the new version. Do not retry with the old body; that is how a
colleague's paragraph disappears.

## Creating

Omit `expected_version` to say "this note must not exist yet". If it does,
the result is a conflict carrying the current version, which tells you to
read it instead.

## Why append needs no version

`note_append` only adds text at the end, on its own line. Two appends that
race both land; neither removes anything. That is why journals use append
and never put.

## History

Every body that `note_put` replaces is kept under its old version, readable
with `note_history`. Appends do not create history entries: the previous
content is a prefix of the new one, so nothing was lost.
