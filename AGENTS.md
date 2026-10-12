# Agent instructions

This repository is public. Everything in it is readable by anyone.

- Never write a hostname, database URL, token, user name, or any note content here. Tests use throwaway values; the Postgres integration test takes its URL from the environment and is skipped without it.
- The code documentation is the design document. A change in behaviour changes the doc comment in the same commit, starting with the crate-level doc of `notelet-core` if the model moved.
- Two crates, one table of notes, one of revisions, eight tools. Before adding anything, read "What is deliberately absent" in `notelet_core`'s crate doc; most additions belong next to notelet, on top of `note_dump`, not inside it.
- SQL lives only in `notelet-core/src/pg.rs`. Identifiers come from the `Field::column` / `Op::sql` allowlists; every value is a bound parameter. Never interpolate input into SQL text.
- `just check` and `just test` must pass before a commit. Do not run `cargo test --workspace`; use the recipes, which target crates.
- Pushing, releasing, deploying and opening pull requests are done by a human. `just deploy*` recipes exist for that human.

Read `CONTRIBUTING.md` and `PUBLIC_DEVELOPMENT.md` before changing anything.
