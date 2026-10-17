# Repo Settings Checklist (pre-public)

Apply once before the repo goes public, then keep enforced. GitHub UI:
Settings → Branches / Rules / Actions / Security as noted.

## Branch protection (`main`)

- [ ] Require a pull request before merging (no direct pushes).
- [ ] Require approvals: ≥ 1.
- [ ] Dismiss stale approvals when new commits are pushed.
- [ ] Require status checks to pass before merging:
  - [ ] `ci` (fmt + clippy + test)
  - [ ] `deb` (packaging validation)
  - [ ] `aur` (AUR template/render/offline-build validation)
  - [ ] `release-scripts` (release/AUR script syntax + offline guards)
- [ ] Require branches to be up to date before merging.
- [ ] Do not allow bypassing the above settings (no bypass list).
- [ ] Block force pushes and deletions on `main`.

## CI

- [ ] `.github/workflows/ci.yml` `ci` + `deb` + `aur` + `release-scripts` jobs required (see above).
- [ ] Dependabot alerts enabled (cargo); `Cargo.lock` committed.

## Releases

- [ ] Signed-tag SOP followed for every release: `docs/RELEASING.md`.
- [ ] Tags named `vX.Y.Z`, GPG-signed (`git tag -s`), "Verified" on GitHub.
- [ ] crates.io: do NOT publish (metadata kept ready only).
- [ ] Release activation (post-review, owner): secret `AUR_SSH_PRIVATE_KEY`
      plus variables `ENABLE_RELEASE_PUBLISH`, `ENABLE_AUR_STABLE`,
      `ENABLE_AUR_GIT` (`docs/RELEASING.md`); workflows are inert without them.

## Secret inventory (2026-10-14 scan)

- [x] `git log -p --all` scanned for
      `api[_-]?key|secret|token|password|BEGIN ... PRIVATE KEY|aws_access|ghp_|github_token`.
      Result: **clean** — only false positives (`key_token_to_code`
      key-code mapping identifiers in source).
- [x] No `.env`, credentials, or private keys tracked; `.gitignore`
      covers `.env`/`.env.*`.
- [ ] Actions secret `AUR_SSH_PRIVATE_KEY` set at activation (dedicated
      AUR-registered key; CI/release jobs only — never PR builds).

## Visibility

- [ ] Repo public; `SECURITY.md` reporting email confirmed.
- [ ] Issue/PR templates present (`.github/ISSUE_TEMPLATE/`, `PULL_REQUEST_TEMPLATE.md`).
- [ ] License files present (`LICENSE-MIT`, `LICENSE-APACHE`) and
      `Cargo.toml` SPDX expression matches.
