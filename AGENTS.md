# Agent instructions

This repository is public. Everything in it is readable by anyone.

- Never write a hostname, database URL, token, user name, or any note content here. Tests use throwaway values; the Postgres integration test takes its URL from the environment and is skipped without it.
- The code documentation is the design document. A change in behaviour changes the doc comment in the same commit, starting with the crate-level doc of `notelet-core` if the model moved.
- Two crates, one table of notes, one of revisions, eight tools. Before adding anything, read "What is deliberately absent" in `notelet_core`'s crate doc; most additions belong next to notelet, on top of `note_dump`, not inside it.
- SQL lives only in `notelet-core/src/pg.rs`. Identifiers come from the `Field::column` / `Op::sql` allowlists; every value is a bound parameter. Never interpolate input into SQL text.
- `just check` and `just test` must pass before a commit. Do not run `cargo test --workspace`; use the recipes, which target crates.
- Pushing, releasing, deploying and opening pull requests are done by a human. `just deploy*` recipes exist for that human.

Read `CONTRIBUTING.md` and `PUBLIC_DEVELOPMENT.md` before changing anything.

## Who does what in an agent session

- Implementation goes to a Sonnet subagent on a topic branch. The main session hands it the spec (the draft written for the issue), the files it may touch, and the verify commands (`just check`, `just test`); it does not implement in the main context.
- Review goes to an Opus subagent that reads the diff against the spec and reports findings; it does not edit. Findings go back to the implementer, not to the main context's own editor.
- The main session integrates: it writes the draft, dispatches, relays findings, and reports. Commits, pushes and pull requests stay with the human as above.
- Push and pull request are handed over as a script, never typed into the chat. The agent writes the script (`set -euo pipefail`, `cd` into the repo, absolute paths; it runs `just check`, `just test` and `just test-pg` first so a failure stops before `git push`, then `git push -u origin <branch>` and `gh pr create --body-file`) and the PR body, and gives the human one line to run it. The agent does not run it.
- The commands a session hands to the human go the same way, whatever their length: a script file with an absolute path, one line to run it.
