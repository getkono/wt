# Releasing `wt`

Releases are automated with [release-plz](https://release-plz.dev) and driven by
[Conventional Commits](https://www.conventionalcommits.org). You normally never
tag or publish by hand — you merge a generated release PR and the rest happens in
CI.

## How it works

Everything is in [`.github/workflows/release-plz.yml`](.github/workflows/release-plz.yml)
and configured by [`release-plz.toml`](release-plz.toml).

1. **You merge feature PRs to `master`** using Conventional Commit messages
   (`feat:`, `fix:`, `feat!:`/`BREAKING CHANGE:` for majors, etc.). Commit
   messages are enforced — see [Conventional commits](#conventional-commits).
2. **release-plz opens/updates a "release PR"** that bumps the version in
   `Cargo.toml` and prepends a section to [`CHANGELOG.md`](CHANGELOG.md), derived
   from the commits since the last release.
3. **You merge the release PR.** release-plz then:
   - creates the `v{version}` git tag and a GitHub Release (notes taken from the
     changelog), and
   - fans out (in the same workflow run) to build prebuilt binaries and refresh
     the Homebrew formula.
4. **Binaries** are cross-compiled for macOS (arm64 + x86_64) and Linux musl
   (arm64 + x86_64) and attached to the GitHub Release.
5. **Homebrew** — the `update-tap` job renders [`.github/homebrew/wt.rb`](.github/homebrew/wt.rb)
   with the release version and the four tarball SHA-256s and commits it to
   [`getkono/homebrew-tap`](https://github.com/getkono/homebrew-tap) as
   `Formula/wt.rb`.

The crate is published to crates.io as `kono-wt` (the binary stays `wt`); see
[crates.io publishing](#cratesio-publishing).

## Required repository secrets

Add these under **Settings → Secrets and variables → Actions**:

| Secret | Required? | Purpose |
| --- | --- | --- |
| `HOMEBREW_TAP_TOKEN` | **Yes**, for Homebrew | A token (fine-grained PAT or classic with `repo`/`contents:write`) that can **push to `getkono/homebrew-tap`**. Without it, the `update-tap` job logs a notice and skips — the release itself still succeeds. |
| `RELEASE_PLZ_TOKEN` | Optional | A PAT used by release-plz instead of the built-in `GITHUB_TOKEN`. Needed only so that **CI runs on the release PR** (the built-in token can't trigger workflows) and so the Release shows the maintainer as author. Binaries and the tap work without it. |
| `CARGO_REGISTRY_TOKEN` | **No** | Not needed: the release job publishes to crates.io via [Trusted Publishing](#cratesio-publishing) (OIDC), so there is no long-lived registry token. |

The previously documented `SENDIT_DEPLOY_KEY` is **no longer needed**: `sendit`
is now fetched as a public HTTPS git dependency.

## Cutting the first release (v1.0.0)

`Cargo.toml` is already at `1.0.0` and there is no `v1.0.0` tag yet, so the
release flow will mint it. To initiate the current version manually:

1. Add the `HOMEBREW_TAP_TOKEN` secret (otherwise the tap step is skipped).
2. Merge this PR to `master`. The **Release-plz** workflow runs automatically;
   because `1.0.0` has no tag, the `release` job creates the `v1.0.0` tag +
   GitHub Release, then the build and tap jobs run.
   - Or trigger it on demand: **Actions → Release-plz → Run workflow** on
     `master` (the workflow's `workflow_dispatch`).
3. Verify:
   - `v1.0.0` appears under **Releases** with four `wt-*.tar.gz` assets, and
   - `Formula/wt.rb` is updated in `getkono/homebrew-tap`.
4. Install to confirm:
   ```bash
   brew install getkono/tap/wt
   wt --version
   ```

## Conventional commits

Commit messages must follow Conventional Commits. This is enforced in three
places, all using [`convco`](https://github.com/convco/convco) (provisioned by
mise):

- **`commit-msg` git hook** (via hk) — rejects a non-conforming message as you
  commit.
- **`pre-push` git hook** (via hk) — re-checks the whole branch (`commit-check`).
- **CI** — the `Conventional commits` job in [`ci.yml`](.github/workflows/ci.yml)
  checks every commit in a pull request.

Use the standard types: `feat`, `fix`, `docs`, `refactor`, `perf`, `test`,
`build`, `ci`, `chore`, `revert` (note: `docs:`, not `doc:`). `feat!:` or a
`BREAKING CHANGE:` footer triggers a major bump.

Check locally before pushing:

```bash
mise run commit-check    # lint origin/master..HEAD
```

## crates.io publishing

The crate is published as **`kono-wt`** — the bare `wt` name is an unrelated
2020 placeholder at `0.0.0`. The `[package]` name is `kono-wt`, while explicit
`[lib]` and `[[bin]]` sections keep both targets named `wt`, so `cargo install
kono-wt` installs a `wt` binary and the public API is still imported as `wt::…`.

Publishing is **on** (`publish = true` in `release-plz.toml`). The release job
authenticates with crates.io via [Trusted Publishing](https://release-plz.dev/docs/extra/trusted-publishing)
(OIDC): `release-plz.yml` grants the release job `id-token: write`, so there is
no `CARGO_REGISTRY_TOKEN` secret. `semver_check` stays `false` — `kono-wt` is
shipped only as a binary, so API checks would just add noise.

This was unblocked once `sendit` (`getkono/sendit`) was published to crates.io
and `wt`'s dependency moved from `{ git = … }` to a registry version
(`sendit = "x.y.z"`); crates.io rejects crates with git/path dependencies.

One-time setup (already done for `kono-wt`, recorded here for reference):

1. Publish the first version manually so the crate exists and is owned by the
   maintainer (Trusted Publishing is configured against an existing crate):
   ```bash
   cargo publish -p kono-wt   # uses your local `cargo login` token
   ```
2. On crates.io → `kono-wt` → **Trusted Publishing**, add the publisher
   `getkono/wt` with the `release-plz.yml` workflow. From then on every release
   PR merge publishes the new version automatically over OIDC.
