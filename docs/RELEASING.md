# Releasing

SOP for cutting a signed release. Releases are git tags + GitHub Release;
crates.io publish is **not** done (metadata is kept ready, see `Cargo.toml`).

## Pipeline

`release-tag.yml` builds on every `vX.Y.Z` tag push (or manual dispatch of an
existing tag) and uploads workflow artifacts. It creates/refreshes a **draft**
GitHub Release only when repo variable `ENABLE_RELEASE_PUBLISH=true` — and it
never publishes: the draft stays a draft until a human clicks Publish.

`aur-stable.yml` runs on the `release.published` event (and only then, or on
an explicit manual retry that re-verifies the release is already public). It
publishes `linux-trackpad-driver` (source) and `linux-trackpad-driver-bin`
(prebuilt) to AUR, gated on repo variable `ENABLE_AUR_STABLE=true` plus the
`AUR_SSH_PRIVATE_KEY` secret.

`aur-git.yml` replaces the old `nightly.yml` (deleted). It pushes
`linux-trackpad-driver-git` after packaging-relevant `main` changes, gated on
repo variable `ENABLE_AUR_GIT=true` plus the same secret. Never per-driver
version bumps: `pkgver()` computes at install time.

Event chain (note the GITHUB_TOKEN gap: automation creates drafts, but a
token-created release does not emit workflow events — so the AUR leg only
starts when a human publishes via web UI or their own token):

```text
git tag -s vX.Y.Z && git push origin vX.Y.Z
  -> release-tag.yml: build matrix, SHA256SUMS, git-cliff excerpt, draft
  -> maintainer reviews the draft, clicks Publish
  -> release.published -> aur-stable.yml: verify public+final,
     download + checksum assets, render via render-aur.sh, push both AUR repos
```

## Prerequisites

- GPG signing key configured (`git config user.signingkey <KEYID>`).
- Clean tree on `main`, CI green.
- Maintainer access to repo settings (see `docs/REPO_SETTINGS.md`).
- For AUR legs: `AUR_SSH_PRIVATE_KEY` secret set (dedicated key registered
  with the AUR account) and the matching `ENABLE_*` variable (below).

## Steps

1. Bump version:
   - `Cargo.toml` (`version = "X.Y.Z"`, keep `Cargo.lock` in sync via `cargo build`).
   - `debian/changelog` (new stanza, `dch -v X.Y.Z-1`).
   - No AUR template edits: stable/bin digests render at publish time,
     `-git` needs no bump (`pkgver()` is dynamic).
2. Update the changelog: `git-cliff --tag vX.Y.Z --strip all -o CHANGELOG.md`.
   Notes style is the git-cliff excerpt from Conventional Commits (see
   `cliff.toml`); this differs from projects using PR-linked notes
   generators — the mechanism is not claimed identical, both yield coherent
   per-release notes.
3. Run gates locally:
   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   packaging/arch/check-aur.sh
   packaging/release/test-release-scripts.sh
   ```
4. Secret scan (must be clean before tagging):
   ```sh
   git log -p --all | grep -inE \
     'api[_-]?key|secret|token|password|BEGIN (RSA |OPENSSH |EC |DSA )?PRIVATE KEY|aws_access|ghp_|github_token'
   ```
   Triage every hit; `key_token_to_code` and similar code identifiers are
   known false positives.
5. Commit the release prep, push, wait for CI green on `main`.
6. Create the **signed** tag (only after the signing key exists; until then
   hold the release):
   ```sh
   git tag -s vX.Y.Z -m "Release vX.Y.Z"
   git show vX.Y.Z --show-signature   # verify signature
   git push origin vX.Y.Z
   ```
   The workflow rejects lightweight tags and logs the `git tag -v` result
   as a warning only — that warning is not cryptographic verification.
   The release stays held until the tag page shows "Verified" and step 9
   is done by a human.
7. Review the **draft** GitHub Release auto-created by `release-tag.yml`
   (artifacts + SHA256SUMS + git-cliff notes): verify, then publish it
   manually. The workflow never publishes a Release itself — draft only.
   Publishing is what arms the AUR stable leg; a published release is
   immutable to automation (reruns fail closed instead of rewriting it).
8. Watch `aur-stable.yml` push both AUR repos (or noop-log if already
   current). A stale/older `release.published` event arriving late refuses
   to downgrade the AUR master.
9. Announce only after the tag signature verifies on GitHub
   (tag page shows "Verified").

## Dry runs (no toggles, no pushes)

```sh
# Rebuild an existing tag, artifacts only (publish=false is the default):
gh workflow run release-tag.yml -f tag=vX.Y.Z -f publish=false
# Stable leg without pushing (verifies publicity + digests, renders, uploads):
gh workflow run aur-stable.yml -f tag=vX.Y.Z -f publish=false
# -git leg without pushing:
gh workflow run aur-git.yml -f publish=false
```

Download a dry-run's evidence with `gh run download <run-id>`.

## Never

- Never re-tag or force-push a published tag; cut a new patch release.
- Never publish to crates.io without a separate decision + audit
  (current policy: do NOT publish).
- Never commit secrets to fix a release — rotate + cut a new release.
- Never let automation publish a Release object (that would skip the AUR
  trigger and the human review in one step).

## Activation (owner, once)

| Setting | Enables |
|---|---|
| Secret `AUR_SSH_PRIVATE_KEY` | AUR pushes (dedicated key, AUR-registered; both stable and -git legs use it). Never commit keys. |
| `ENABLE_RELEASE_PUBLISH=true` | draft GitHub Release creation on `v*` tags |
| `ENABLE_AUR_STABLE=true` | stable AUR push on `release.published` (also needs the secret) |
| `ENABLE_AUR_GIT=true` | -git AUR push on packaging changes (also needs the secret) |

To go back to inert: delete the repo variables (or set them `false`).
Workflows stay inert by default: without the variables every publish leg
skips and runs stop at downloadable artifacts.

Manual retries are safe: identical reruns noop, partial successes resume
(one repo pushed + one pending retries cleanly), older events never
downgrade, published assets are never rewritten.

## Layout

- `.github/workflows/release-tag.yml` — `v*` tags (and manual dispatch):
  strict tag validation, exact-commit checkout, version sync
  (Cargo/Cargo.lock/debian), build matrix (x86_64 + aarch64), `.deb` from
  `debian/` (x86_64), binary tarballs **with both licenses**, `SHA256SUMS`,
  git-cliff excerpt → **draft** GitHub Release (rerun-safe, never public).
- `.github/workflows/aur-stable.yml` — `release.published` (or manual
  retry): verifies the release is public + final, checksums both arch
  tarballs against SHA256SUMS, requires the checksummed BUILD_COMMIT to
  equal the release commit (moved tags fail), requires that commit on
  upstream main, binds the source archive to the tag commit and version,
  renders all three AUR trees via `render-aur.sh` on real Arch tooling, then
  pushes `linux-trackpad-driver` + `linux-trackpad-driver-bin` (master,
  append-only).
- `.github/workflows/aur-git.yml` — `main` pushes touching
  `packaging/arch/**`, `systemd/**`, `udev/**`, `config/default.toml`,
  `Cargo.toml`, `Cargo.lock` (or a manual retry targeting main — other refs
  are denied): regenerates `-git` metadata
  and pushes `linux-trackpad-driver-git` (master, append-only).
- `packaging/arch/PKGBUILD-stable.in` — stable spec (release-tarball
  `source=`, no git dependency). `packaging/arch/PKGBUILD-bin.in` — per-arch
  prebuilt spec. `packaging/arch/PKGBUILD-git` — VCS spec (`pkgver()`
  dynamic). `packaging/arch/.SRCINFO` is generated at render time, never
  committed here.
- `packaging/release/` — `check-version-sync.sh`, `check-release-json.sh`,
  `verify-assets.sh`, `verify-stable-release.sh` (BUILD_COMMIT binding,
  moved-tag, main ancestry), `manage-draft-release.sh` (GH_REPO-bound draft
  create/refresh), `gate-aur-git.sh` (main-ref boundary), `publish-aur.sh`
  (+ offline tests in `test-release-scripts.sh`, run by CI without secrets).
- `cliff.toml` — git-cliff Conventional Commits config. Preview with
  `git-cliff --unreleased --strip all`. Gotcha: never set
  `limit_commits = 0` (means "take zero commits", yields an empty
  changelog); omit the key for unlimited.

## Guardrails (do not weaken without review)

- Publishing requires the `AUR_SSH_PRIVATE_KEY` secret (not yet configured)
  plus the matching toggle; every publish leg checks both. Missing secret
  or toggle → clean stop at artifacts with an explanatory log line, never
  a half-publish.
- Releases are created as **drafts**; a human clicks Publish.
- AUR stable legs only run off public, final releases; drafts and
  prereleases fail closed before any download is trusted.
- AUR stable and -git are separate repos and separate workflows; stable
  never builds from git, -git never touches the stable repos.
- SSH to AUR uses a pinned host key + `StrictHostKeyChecking`, temp key
  files (mode 600, scrubbed on exit), no `ssh-keyscan`, no
  `StrictHostKeyChecking=no`. The secret is only mapped in the push jobs,
  which never run on PRs.
- Never commit `AUR_SSH_PRIVATE_KEY` or any key material.
