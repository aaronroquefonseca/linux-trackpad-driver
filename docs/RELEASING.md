# Releasing

SOP for cutting a signed release. Releases are git tags + GitHub Release;
crates.io publish is **not** done (metadata is kept ready, see `Cargo.toml`).

Release automation (workflows, AUR specs, changelog config) is **ready but
inert**: nothing can publish until the owner completes the [enable
checklist](#5-minute-enable-checklist-owner-at-public-launch) below. Until
then tag pushes only build (workflow artifacts), and the AUR jobs never run
(`if: false`).

## Prerequisites

- GPG signing key configured (`git config user.signingkey <KEYID>`).
- Clean tree on `main`, CI green.
- Maintainer access to repo settings (see `docs/REPO_SETTINGS.md`).

## Steps

1. Bump version:
   - `Cargo.toml` (`version = "X.Y.Z"`, keep `Cargo.lock` in sync via `cargo build`).
   - `debian/changelog` (new stanza, `dch -v X.Y.Z-1`).
   - `packaging/arch/PKGBUILD` (`pkgver=X.Y.Z`, regenerate `.SRCINFO`).
2. Update the changelog: `git-cliff --tag vX.Y.Z --strip all -o CHANGELOG.md`.
3. Update `docs/ROADMAP.md` status line if needed.
4. Run gates locally:
   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
5. Secret scan (must be clean before tagging):
   ```sh
   git log -p --all | grep -inE \
     'api[_-]?key|secret|token|password|BEGIN (RSA |OPENSSH |EC |DSA )?PRIVATE KEY|aws_access|ghp_|github_token'
   ```
   Triage every hit; `key_token_to_code` and similar code identifiers are
   known false positives.
6. Commit the release prep, push, wait for CI green on `main`.
7. Create the **signed** tag:
   ```sh
   git tag -s vX.Y.Z -m "Release vX.Y.Z"
   git show vX.Y.Z --show-signature   # verify signature
   git push origin vX.Y.Z
   ```
8. Review the **draft** GitHub Release auto-created by `release-tag.yml`
   (artifacts + SHA256SUMS + git-cliff notes): verify, then publish it
   manually. The workflow never publishes a Release itself — draft only.
9. Announce only after the tag signature verifies on GitHub
   (tag page shows "Verified").

## Never

- Never re-tag or force-push a published tag; cut a new patch release.
- Never publish to crates.io without a separate decision + audit
  (current policy: do NOT publish).
- Never commit secrets to fix a release — rotate + cut a new release.

## 5-minute enable checklist (owner, at public launch)

- [ ] **1. AUR SSH key secret (~2 min).**
      Generate a dedicated key, register the public half with your AUR
      account (`https://aur.archlinux.org/account/`), store the private half:
      ```sh
      ssh-keygen -t ed25519 -f ~/.ssh/aur-release -C "ltd-release" -N ""
      # add ~/.ssh/aur-release.pub to your AUR account, then:
      gh secret set AUR_SSH_PRIVATE_KEY < ~/.ssh/aur-release
      ```
      Secret name is exact: **`AUR_SSH_PRIVATE_KEY`**. Both AUR jobs
      (`aur-stable` in `release-tag.yml`, `publish-nightly` in
      `nightly.yml`) reference this name. Never commit keys.
- [ ] **2. Workflow toggles (~1 min).** Repo Settings > Variables > New:
      | Variable | Enables |
      |---|---|
      | `ENABLE_RELEASE_PUBLISH=true` | draft GitHub Release creation on `v*` tags |
      | `ENABLE_AUR_STABLE=true` | stable AUR push (also needs step 3) |
      | `ENABLE_NIGHTLY_PUBLISH=true` | -nightly AUR push on main (also needs step 3) |
- [ ] **3. Arm the two AUR jobs (~1 min).** Each is hard-disabled with
      `if: false` plus a comment naming the secret + toggle:
      - `release-tag.yml`, job `aur-stable`: flip to
        `if: needs.preflight.outputs.aur == 'true'`.
      - `nightly.yml`, job `publish-nightly`: flip to
        `if: needs.preflight.outputs.publish == 'true'`.
- [ ] **4. Create the -nightly AUR repo once (~1 min).** The first push to a
      new AUR package must come from your own key (the bot cannot create
      the repo):
      ```sh
      git clone ssh://aur@aur.archlinux.org/linux-trackpad-driver-nightly.git
      cp packaging/arch/PKGBUILD-nightly linux-trackpad-driver-nightly/PKGBUILD
      cp packaging/arch/ltd.install linux-trackpad-driver-nightly/
      cd linux-trackpad-driver-nightly
      makepkg --printsrcinfo > .SRCINFO && git add PKGBUILD ltd.install .SRCINFO
      git commit -m "initial nightly snapshot" && git push
      ```
- [ ] **5. First tag procedure (~1 min + CI time).** Follow Steps above
      (note step 7: tags are signed). Stable AUR picks up the tag on the
      next `release-tag.yml` run (pkgver bump + checksum from the draft
      Release's SHA256SUMS).

To go back to inert: delete the repo variables (or set them `false`) and
revert the two `if:` conditions to `false`. The `preflight` jobs always
print which outputs are live, so any run's log shows inert vs armed.

## Layout

- `.github/workflows/release-tag.yml` — `v*` tags (and manual dispatch):
  build matrix (x86_64 + aarch64), `.deb` from `debian/` (x86_64),
  `.tar.gz` (binary + systemd units + udev rules + default config),
  `SHA256SUMS`, git-cliff excerpt → **draft** GitHub Release.
  AUR stable push present but `if: false`.
- `.github/workflows/nightly.yml` — `main` pushes: computes the would-be
  `pkgver`, prints it, stops. Armed form bumps `PKGBUILD-nightly` and
  pushes to the **separate** repo `linux-trackpad-driver-nightly`.
- `packaging/arch/PKGBUILD` — stable spec (release-tarball `source=`,
  no git dependency). `packaging/arch/.SRCINFO` regenerated per release.
- `packaging/arch/PKGBUILD-nightly` — nightly template (VCS `source=`,
  `pkgver()`); copied to `PKGBUILD` by the nightly job.
- `cliff.toml` — git-cliff Conventional Commits config. Preview with
  `git-cliff --unreleased --strip all`. Gotcha: never set
  `limit_commits = 0` (means "take zero commits", yields an empty
  changelog); omit the key for unlimited.

## Guardrails (do not weaken without review)

- Publishing requires secrets that do not exist yet; every publish step
  also checks its `ENABLE_*` variable. Missing secret or toggle → clean
  skip with an explanatory log line, never a half-publish.
- Releases are created as **drafts**; a human clicks Publish.
- AUR stable and -nightly are separate repos and separate jobs; stable
  never builds from git, nightly never touches the stable repo.
- Never commit `AUR_SSH_PRIVATE_KEY` or any key material.
