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

## Embedded build identity

Cargo embeds identity at compile time; `--version` / `-V`, the first line of
`doctor`, and the daemon startup log share it. `Cargo.toml` remains the semantic
release version; snapshots do not change it or package versions/tags/assets.

Build inputs (environment variables, never runtime CLI flags):

| Variable | Contract |
|---|---|
| `LTD_BUILD_CHANNEL` | `development` (default), `stable`, or `nightly` |
| `LTD_BUILD_COMMIT` | Real source commit, 12–64 hex digits; displayed as 12 lowercase digits. Explicit `unknown` disables Git fallback. Otherwise use checkout HEAD, then `unknown`. |
| `LTD_BUILD_DATE` | Required only for `nightly`: explicit calendar date `YYYY-MM-DD`. Rejected for other channels. Never taken from the clock. |

Invalid explicit metadata fails the build. Example for a future dated snapshot:

```sh
LTD_BUILD_CHANNEL=nightly LTD_BUILD_DATE=2026-10-18 \
  LTD_BUILD_COMMIT=18d66664196334e80cb318b23098b412c8c29d8f cargo build --locked
# ltd 0.1.0 (nightly 2026-10-18, commit 18d666641963)
```

No nightly workflow or package is provided. AUR `-git` explicitly selects
`development`; stable AUR source builds select `stable`. Debian and Fedora
packaging default to `stable`, with an environment override for development
builds. Release jobs supply the exact preflight-resolved tag commit to all
binary, Debian, and RPM builds, including source archives without `.git`.
Regular CI supplies its checked-out commit and uses `development`.

Stable tag archives have no Git history and may correctly print
`ltd 0.1.0 (stable, commit unknown)`. Packagers can supply the verified source
commit with `LTD_BUILD_COMMIT`; never derive it from a tarball checksum or an
unrelated parent repository. Prebuilt packages preserve their binary's identity.

Git discovery requires `.git` in the package root (directory or worktree file).
Tracked edits, staged changes, and non-ignored untracked files add `, dirty`,
even when a commit override is supplied. Ignored output is excluded; the
build script only emits Cargo metadata and writes no source files. Without
working Git, dirt cannot be determined and only supplied metadata is available;
`dirty` reports detected local changes, rather than certifying a clean tree.
For archive builds, packagers are responsible for the supplied commit's accuracy.

Cargo re-runs the small metadata probe on every build so a new commit, dirty
change, clean transition, or environment override updates the binary without
`cargo clean`. This also recompiles the crate, trading a small rebuild cost for
fresh identity. No runtime Git calls, build paths, or wall-clock timestamps are
included in the identity.

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
2. Prepare only the new release section:
   ```sh
   git-cliff --unreleased --strip all | head -100
   git-cliff --unreleased --tag vX.Y.Z --strip all > /tmp/release-details.md
   ```
   Insert that section above the existing entries in `CHANGELOG.md`, then
   write **5–10 manual user-facing bullets** under `### Highlights`, above
   the generated `### Details`. Follow the Highlights process below.
   Preserve every published section byte-for-byte, including `## [0.1.0]`.
   Never regenerate the full file with `-o CHANGELOG.md` for past tags.
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
   (artifacts + SHA256SUMS + git-cliff Details): copy the reviewed Highlights
   from `CHANGELOG.md` above Details, verify the notes and assets, then
   publish it manually. If the build is rerun, reapply Highlights after the
   final draft refresh. The workflow only creates or refreshes drafts.
   Publishing is what arms the AUR stable leg; a published release is
   immutable to automation (reruns fail closed instead of rewriting it).
8. Watch `aur-stable.yml` push both AUR repos (or noop-log if already
   current). A stale/older `release.published` event arriving late refuses
   to downgrade the AUR master.
9. Announce only after the tag signature verifies on GitHub
   (tag page shows "Verified").

## Commit discipline

Squash-merge PRs into `main` and use the PR title as the squash commit
subject. CI runs commitlint on the title (including title edits); individual
work-in-progress commits are not linted. Before merging, check the final
squash subject and body in GitHub so the validated title reaches history.

Use `<type>(<scope>): <description>` with a **required scope** identifying
the affected area, for example `fix(proxy): restore release clicks`.
Allowed types are `feat`, `fix`, `sec`, `docs`, `test`, `perf`, `refactor`,
`ci`, `build`, `chore`, `revert`, and `style`. Use `sec(scope): ...` for
security fixes so they appear in Security.

For incompatible behavior, use `feat(config)!: ...` or `fix(proxy)!: ...`
(the scoped form of `feat!` / `fix!`) and explain the migration in a
`BREAKING CHANGE:` footer in the squash body. Breaking entries render first
with `[!BREAKING]`, even if their type or scope normally matches a noise
filter. Other process-only subjects/scopes beginning with `coordinator`,
`wave`, `status`, `merged`, `dispatched`, `verdict`, `relay`, or `settled`,
and `chore(merge)` commits, are omitted from Details. Reserve these for
coordination; describe user changes with an area scope.

## Highlights process

For each future release, a maintainer reviews the generated Details and
writes 5–10 concise Highlights describing user benefits, visible fixes,
security changes, and any required migration. Combine related commits,
link useful PRs, and omit internal coordination. Review the bullets against
the diff and keep them above Details in both `CHANGELOG.md` and the draft
GitHub Release. The generator supplies Details only; it cannot write or
preserve manual Highlights when a draft is refreshed.

Use this layout for the new section (replace the illustrative bullets with
5–10 reviewed bullets):

```markdown
## [X.Y.Z] - YYYY-MM-DD

### Highlights

- Describe a user-visible improvement and its impact.
- Explain any compatibility change and how to migrate.

### Details

#### Breaking Changes

- Generated entry (scope) ([short-sha](commit-url)) [!BREAKING]

#### Features

- Generated entry (scope) ([short-sha](commit-url))

Full diff: https://github.com/aaronroquefonseca/linux-trackpad-driver/compare/vA...vB
```

Details groups are Breaking Changes, Security, Features, Bug Fixes,
Documentation, Tests, Performance, Refactoring, Continuous Integration,
and Miscellaneous; empty groups are omitted. Entries link to their commits
and existing `(#N)` references link to PRs. The per-release Full diff line
compares the previous tag with the new tag (`HEAD` in unreleased previews).
The first release has no comparison link because no previous tag exists.

Keep published history unchanged: these conventions apply only to new
release sections. In particular, do not retrofit Highlights, links, noise
filtering, or group ordering into the published `0.1.0` entry.

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

- Publishing requires the `AUR_SSH_PRIVATE_KEY` secret plus the matching
  `ENABLE_*` variable; every publish leg checks both. The secret is already
  provisioned, while all `ENABLE_*` activation variables remain unset until
  release review and signing setup pass. Missing secret or variable →
  clean stop at artifacts with an explanatory log line, never a half-publish.
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
