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

If something private lands here it is treated as published: rotate what was exposed. History is rewritten only when the maintainer decides it is worth it. Prevention is cheaper, and keeping private values out of the working tree is what notelet itself is for.
