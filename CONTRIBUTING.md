# Contributing

notelet is small on purpose. The bar for a change is "does the model need this?", not "would it be convenient?".

## Ground rules

- **The crate docs are the design.** `cargo doc` on `notelet-core` is the design document. A pull request that changes behaviour without changing the relevant doc comment is incomplete.
- **Eight tools, two tables.** Adding a tool, a table or a column needs an issue first that says what cannot be done with `note_dump` plus a tool beside notelet.
- **SQL only in `pg.rs`, values always bound.** See `notelet_core::pg`.
- **Adapters implement the port, nothing more.** `NoteRepo` is the contract; the in-memory adapter must keep matching the Postgres one so the service tests mean something.
- **Nothing private, ever.** No real URLs, tokens, user names or note content, in code, tests, docs or commit messages.

## Workflow

1. Open an issue describing the change in terms of the model (note / revision / filter / page / tool).
2. Branch from `main`. Make the change. Update the doc comments and `CHANGELOG.md` in the same change.
3. `just check` and `just test` must pass. If you touched `pg.rs`, run `just test-pg` against a throwaway database.
4. Open a pull request. A maintainer reviews and merges; contributors do not push to `main`.

## Verification

Claims in a pull request are backed by output. For a change to `pg.rs`, paste the `just test-pg` result. For a change to a tool, paste the tool's JSON response from a real client or from the unit test.

## Releases

Releases will be cut by the maintainer with release-plz through a dispatched workflow (not yet added; see `.github/workflows/ci.yml`); version bumps and changelog entries will come from conventional commits. Contributors do not bump versions.

## Commit messages

Imperative, one line, no trailers that name a tool or model. Example: `Add cursor paging to note_dump`.
