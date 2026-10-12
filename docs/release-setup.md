# Release setup (once)

What has to exist before the release flow in [CONTRIBUTING.md](../CONTRIBUTING.md#releases) works. All of it is done by the maintainer, by hand; nothing here runs in CI.

## 1. Secrets and repository settings, before the release workflows reach `main`

`release-plz.yml` runs its release job on every push to `main` and mints a GitHub App token first, so without these secrets that job fails on each push.

- **GitHub App** (the same kind htl uses): an App with *Contents* and *Pull requests* read/write, installed on `ynishi/notelet`. Store its client ID as `APP_ID` and its private key as `APP_PRIVATE_KEY` in the repository's Actions secrets. The release PR and the tag are made with this App's token because a PR or a tag pushed with `GITHUB_TOKEN` starts no workflow.
- **`HOMEBREW_TAP_TOKEN`**: a token that can push to `ynishi/homebrew-tap` (a fine-grained token with *Contents* read/write on that one repository), stored as an Actions secret. `release.yml` uses it to commit `Formula/notelet-mcp.rb`.
- **Merge settings**: allow merge commits (`gh pr merge --auto --merge`) and auto-merge, for the `auto_merge` input. [unverified] GitHub may refuse to arm auto-merge on a PR that has no required status check pending; if the arming step fails, dispatch with `-f auto_merge=false` and merge by hand.

## 2. First publish, by hand

Trusted Publishing is configured on a crate that already exists and cannot create one: "To get started with Trusted Publishing, you'll need to publish your first release manually" ([crates.io development update, 2025-07](https://blog.rust-lang.org/2025/07/11/crates-io-development-update-2025-07/)). So `0.1.0` is published once with an API token, and every later version comes from the workflow. Publishing the real `0.1.0` (rather than an empty placeholder crate) also gives release-plz a release to count from, which is what `changelog_include = ["notelet-core"]` in `release-plz.toml` needs.

1. On `main`, move the entries under `## [Unreleased]` in `CHANGELOG.md` to a `## [0.1.0] - <date>` section, leave an empty `## [Unreleased]` above it, and commit. [unverified] release-plz inserts new sections next to `## [Unreleased]` and is not documented to fold hand-written entries already under it into the next version.
2. Create a crates.io API token scoped to `publish-new`, then from a clean checkout of that commit:

   ```sh
   cargo publish -p notelet-core
   cargo publish -p notelet-mcp
   ```

   `notelet-core` first: `notelet-mcp` depends on it by version.
3. Revoke the token.

## 3. Trusted Publishing

On crates.io, for **each** of `notelet-core` and `notelet-mcp`: Settings → Trusted Publishing → add a GitHub publisher with

| field | value |
|---|---|
| repository owner | `ynishi` |
| repository name | `notelet` |
| workflow filename | `release-plz.yml` |
| environment | (empty; the workflow sets none) |

## 4. Binaries for 0.1.0 (optional)

The hand publish pushes no tag, so `0.1.0` has no GitHub Release. To build one, push the tag at the commit that was published; `release.yml` does the rest:

```sh
git tag v0.1.0 <commit>
git push origin v0.1.0
```

Skipping this is fine: the first dispatched release is the first one with binaries.

## 5. First dispatch

After the next change worth releasing has landed on `main`:

```sh
gh workflow run release-plz.yml -R ynishi/notelet -f auto_merge=false
```

`auto_merge=false` the first time, so the release PR (version, `CHANGELOG.md` section) is read before it is merged. Merging it publishes and tags as described in CONTRIBUTING.
