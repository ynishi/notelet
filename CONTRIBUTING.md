# Contributing

notelet is small on purpose. The bar for a change is "does the model need this?", not "would it be convenient?".

## Ground rules

- **The crate docs are the design.** `cargo doc` on `notelet-core` is the design document. A pull request that changes behaviour without changing the relevant doc comment is incomplete.
- **A new tool, table or column needs an issue first.** The issue says what cannot be done with `note_dump` plus a tool beside notelet.
- **SQL only in `pg.rs`, values always bound.** See `notelet_core::pg`.
- **Adapters implement the port, nothing more.** `NoteRepo` is the contract; the in-memory adapter must keep matching the Postgres one so the service tests mean something.
- **Nothing private, ever.** No real URLs, tokens, user names or note content, in code, tests, docs or commit messages.

## Workflow

1. Open an issue describing the change in terms of the model (note / revision / filter / page / tool).
2. Branch from `main`. Make the change. Update the doc comments in the same change; `CHANGELOG.md` is written by the release PR (see Releases).
3. `just check` and `just test` must pass. If you touched `pg.rs`, run `just test-pg-local` (a throwaway Docker Postgres).
4. Open a pull request. A maintainer reviews and merges; contributors do not push to `main`.

## Verification

Claims in a pull request are backed by output. For a change to `pg.rs`, paste the `just test-pg-local` result. For a change to a tool, paste the tool's JSON response from a real client or from the unit test.

## Releases

Releases are cut by the maintainer with [release-plz](https://release-plz.dev) and [cargo-dist](https://axodotdev.github.io/cargo-dist/), the same layout as [htl](https://github.com/ynishi/htl). Contributors do not bump versions or edit `CHANGELOG.md`. After the first version (published by hand, see below), nothing is published from a local machine.

1. **Dispatch.** The maintainer runs the `Release-plz` workflow on `main`:

   ```sh
   gh workflow run release-plz.yml -R ynishi/notelet                    # auto-merge once checks pass
   gh workflow run release-plz.yml -R ynishi/notelet -f auto_merge=false # review and merge by hand
   ```

   Only a dispatch by the repository owner opens a release PR; a push to `main` never does.
2. **Release PR.** release-plz opens a PR labelled `release` with the version bump in `[workspace.package]` and the new `CHANGELOG.md` section, read from the commits since the last release (`release-plz.toml` says how).
3. **Merge.** When the release PR lands on `main`, the same workflow publishes `notelet-core` and then `notelet-mcp` to crates.io through Trusted Publishing (no registry token is stored in the repository) and pushes the `v<version>` tag. An ordinary merge never publishes.
4. **Binaries.** The tag starts `Release` (`.github/workflows/release.yml`, cargo-dist), which builds `notelet-mcp` for five targets, creates the GitHub Release with the archives and the shell / PowerShell installers, and pushes the Homebrew formula to `ynishi/homebrew-tap`.

The two crates share one version. A commit whose subject starts with `feat:` makes the next release a minor, which on 0.x is the compatibility unit; so does a change to `notelet-core`'s public API. Anything else is a patch. The commit has to touch a file under `crates/` to count.

The one-time setup (crates.io, Trusted Publishing, secrets) is in [docs/release-setup.md](docs/release-setup.md).

## Commit messages

Imperative, one line, no trailers that name a tool or model. Example: `Add cursor paging to note_dump`. Prefix `feat:` when the change should make the next release a minor (see Releases), and `fix:` for a fix, so the changelog files it under Fixed; other subjects land under Other.
