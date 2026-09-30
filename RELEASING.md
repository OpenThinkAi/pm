# Releasing pm

Releases are built by [`dist`](https://opensource.axo.dev/cargo-dist/)
(v0.31.0) in `.github/workflows/release.yml` and are driven entirely by the
version in `Cargo.toml`. Nobody pushes tags, runs `gh release`, or runs
`npm publish` by hand.

## Cutting a release

1. On a branch, bump `version` in `Cargo.toml` and run `cargo build` so
   `Cargo.lock` picks up the new version. Commit both.
2. Land it through the normal stamp flow:

   ```sh
   stamp review --diff main..<branch>
   stamp merge <branch> --into main
   stamp push main
   ```

3. The stamp server mirrors `main` to GitHub. The push to `main` triggers
   `release.yml`, which:
   - **deny** — `cargo deny check` (advisories, licenses, bans, sources).
     `plan`, `build-local-artifacts`, `host` and `publish-npm` all `needs:`
     it, so a violation blocks the release.
   - **plan** — reads the version from `Cargo.toml`. If
     `@openthink/pm@<version>` is already on npm, every later job is
     skipped, so pushes that don't bump the version are no-ops.
   - **build-local-artifacts** — builds `pm` for each target on a native
     GitHub runner (no cross-compilation). Fails if `Cargo.lock` is out of
     date (`cargo fetch --locked`).
   - **build-global-artifacts** — shell installer, npm wrapper package,
     checksums.
   - **host** — generates build provenance attestations for every artifact,
     then creates the `v<version>` GitHub Release (tag included) with the
     binaries and checksums.
   - **publish-npm** — publishes `@openthink/pm@<version>` with npm
     Trusted Publishing (OIDC, no stored token) and `--provenance`.

The npm package is a thin wrapper that downloads the matching platform
binary from the GitHub Release on install.

## Targets

- `aarch64-apple-darwin` (Apple Silicon)
- `x86_64-apple-darwin` (Intel macOS)
- `x86_64-unknown-linux-gnu`
- `x86_64-unknown-linux-musl`

Not yet shipped: ARM Linux (`aarch64-unknown-linux-gnu` / `-musl`) and
Windows. Adding them is a change to `targets` in
`[workspace.metadata.dist]` plus regenerating the workflow (see below); dist
assigns native runners per target.

## One-time setup (repository / package admins)

Listed so they can be checked or recreated.

- **npm Trusted Publisher.** On npmjs.com: `@openthink/pm` → Settings →
  Trusted Publisher → GitHub Actions, with organization `OpenThinkAi`,
  repository `pm`, workflow filename `release.yml`, **environment
  `npm-publish`** (exactly that name; npm then rejects tokens from runs that
  did not go through that environment). Trusted Publishing only works for a package that already exists; a
  brand-new package name needs one initial publish by an npm owner first.
- **GitHub `npm-publish` environment.** `publish-npm` declares
  `environment: npm-publish`. Create it under `OpenThinkAi/pm` → Settings →
  Environments → New environment → `npm-publish`. Optionally add required
  reviewers (a human approval gate before every publish) and restrict
  deployment branches to `main`. Until the environment exists GitHub
  auto-creates it with no rules, and the npm Trusted Publisher (which
  requires this environment name) only accepts runs through it.
- **GitHub repository settings.** `OpenThinkAi/pm` must be public (build
  provenance and anonymous release downloads need it). GitHub is a read-only
  mirror of the stamp server: add the `stamp-mirror-only` ruleset (block
  deletion and non-fast-forward on `main`; bypass for the mirror deploy
  key). Optionally enable immutable releases. None of this is enforced by
  this repo, so check it before relying on it.
- **Bootstrap publish.** Trusted Publishing needs the package to exist, so an
  npm owner publishes `@openthink/pm` once by hand before the first
  automated release.

## Tags and the stamp mirror

`.stamp/mirror.yml` mirrors `main` only; tags are not mirrored and nobody
pushes them. That is fine: the workflow triggers on the push of `main` to
GitHub, and `gh release create v<version> --target <sha>` in the `host` job
creates the tag on GitHub. Do not enable `tags:` mirroring; a mirrored
`v*` tag would race the workflow's own release creation.

## What ships

Only the `pm` binary crate is released. `pm-hub` (deployed to Railway) is
marked `[package.metadata.dist] dist = false` and, with `pm-core` and
`pm-store`, `publish = false`, so `dist plan` lists just the `pm` archives,
installer and npm package. The UI views (`crates/pm/views/`) are compiled
into the binary with `include_str!` (`crates/pm/src/app/views.rs`), so the
release build needs no extra files or build step.

## Dry run

`dist plan` and `dist build --artifacts=local --target <triple>` work
locally (Linux targets via `cargo zigbuild --profile dist`). A full CI
dry run needs GitHub Actions, which only triggers from a push to `main` on
the mirror; with the npm gate, any version not yet on npm will really
publish. To rehearse without publishing, run the workflow from a fork with
`publish-jobs` removed.

## Supply-chain gate (cargo-deny)

`deny.toml` is enforced by the `deny` job in `release.yml`. Run it locally
before bumping a version (needs network for the RustSec advisory DB):

```sh
cargo install --locked cargo-deny --version 0.20.2   # once
cargo deny check
```

Dev-dependencies are deliberately included in every check (no
`exclude-dev`). The only dev-only finding, `yrs` -> `smallstr`
(RUSTSEC-2026-0215, unmaintained), is ignored explicitly with a reason in
`[advisories].ignore`. If a new advisory lands between releases the gate
fails the release: fix or upgrade, or add a reasoned ignore in `deny.toml`
through the normal stamp flow.

## Verifying a release

```sh
# Assets on the GitHub Release
gh release view v<version> --json assets --jq '.assets[].name'

# Build provenance for a downloaded artifact
gh attestation verify pm-x86_64-unknown-linux-gnu.tar.xz \
  --repo OpenThinkAi/pm

# npm package published via OIDC with provenance
npm view @openthink/pm@<version> dist

# Install smoke test
npm install -g @openthink/pm@<version>
pm --version
```

## Recovering from a partial failure

The npm version is the source of truth for "shipped".

- **`publish-npm` failed, GitHub Release exists.** Use "Re-run failed jobs"
  on the workflow run. The build artifacts are reused; nothing is rebuilt.
- **`host` failed before the release was published.** `gh release create`
  uploads to a draft and publishes last, so at most a draft is left behind.
  Delete it (`gh release delete v<version> --yes`), then re-run the
  workflow.
- **A published release is wrong.** If immutable releases are enabled, a
  published release's tag and assets cannot be changed or reused. Bump the
  patch version and release again.

## Maintaining release.yml

`release.yml` is generated by `dist` and then hand-patched. Every patch is
marked with a `HAND-EDIT` or `PATCHED` comment in the file:

- trigger: push to `main` with the npm version gate, instead of tag pushes;
- workflow-level `permissions: contents: read`; `GH_TOKEN` removed from jobs
  that don't call the GitHub API;
- the `Cargo.lock` check in `build-local-artifacts` (dist has no `--locked`
  setting and would otherwise silently update the lockfile);
- `publish-npm`: OIDC (`id-token: write`, no `NODE_AUTH_TOKEN`), Node 22,
  an exact pinned npm (`npx -y npm@<version>`), `--provenance`, and
  `environment: npm-publish` (must match the Trusted Publisher config);
- the `deny` job, and `deny` in the `needs:` of `plan`,
  `build-local-artifacts`, `host` (plus `needs.deny.result == 'success'` in
  its `if`) and `publish-npm`;
- dist install: every `Install dist` / `Install cached dist` step (plan,
  build-local-artifacts, build-global-artifacts, host) runs
  `bash scripts/install-dist.sh` (pinned version, tarball SHA-256 committed
  in the script). dist's generated `curl | sh` installer, the
  `matrix.install_dist.run` step and the `cargo-dist-cache` upload/download
  (which carried one job's binary into the credentialed `host` job) are all
  removed. Do not reintroduce the cache artifact. The container-only rustup
  `curl | sh` step in build-local-artifacts never runs (all targets use
  native runners); if a `container` target is ever added, pin that too.

`allow-dirty = ["ci"]` in `Cargo.toml` stops dist from failing CI on this
drift. Everything else — attestations in the `host` job, SHA-pinned Actions
(`[workspace.metadata.dist.github-action-commits]`), `pr-run-mode = "skip"`
— is dist configuration, so `dist generate` reproduces it.

To upgrade dist or change its config: bump `cargo-dist-version` **and**
`VERSION` plus the four SHA-256s in `scripts/install-dist.sh` (take them from
the release's `sha256.sum`, and confirm by hashing the downloads), run
`dist generate`, then use `git diff` to re-apply the marked patches that the
regeneration dropped. To bump a pinned Action, update its SHA (and `# vX.Y.Z`
comment) in both `github-action-commits` and `release.yml`; resolve a tag
with `gh api repos/<owner>/<repo>/git/ref/tags/<tag>` (dereference annotated
tags to the commit).
