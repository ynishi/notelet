# Public development

notelet is developed in the open from the first commit. This file says what that means so nothing private leaks and nothing public is a surprise.

## What is public

- The code, the crate docs, the migrations, the guides, the tests, this documentation.
- Issues, pull requests, releases.

## What is never here

- Any deployment's hostname, database URL, token or Fly app name beyond the placeholder in `fly.toml`.
- Any note content. notelet stores notes; this repository stores notelet.
- Session logs, working notes, drafts.

## Boundaries that agents and humans both follow

| action | who |
|---|---|
| edit files, run tests, commit on a branch | agent or human |
| push, open or merge a pull request, tag a release, deploy | human |
| add a dependency | discussed in an issue first |

## Where state lives

A deployment's secrets live in the platform (`fly secrets`) and in the operator's home directory (`~/.notelet/token`). The repository has no place for them, which is the point.

## Leak handling

Two cases, handled differently:

- **A secret** (a token, a database URL, a deployment's hostname or Fly app name, note content) that lands here is treated as published: rotate what can be rotated at once, and rewrite history to remove it.
- **A non-secret** (a local path, a working note, a stray file) is removed in an ordinary commit. Nothing is rotated and history is not rewritten for it; the commit log stays as it is.

Prevention is cheaper, and keeping private values out of the working tree is what notelet itself is for.
