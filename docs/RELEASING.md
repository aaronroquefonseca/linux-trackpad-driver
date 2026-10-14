# Releasing

SOP for cutting a signed release. Releases are git tags + GitHub Release;
crates.io publish is **not** done (metadata is kept ready, see `Cargo.toml`).

## Prerequisites

- GPG signing key configured (`git config user.signingkey <KEYID>`).
- Clean tree on `main`, CI green.
- Maintainer access to repo settings (see `docs/REPO_SETTINGS.md`).

## Steps

1. Bump version:
   - `Cargo.toml` (`version = "X.Y.Z"`, keep `Cargo.lock` in sync via `cargo build`).
   - `debian/changelog` (new stanza, `dch -v X.Y.Z-1`).
2. Update `docs/ROADMAP.md` status line if needed.
3. Run gates locally:
   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets -- -D warnings
   cargo test
   ```
4. Secret scan (must be clean before tagging):
   ```sh
   git log -p --all | grep -inE \
     'api[_-]?key|secret|token|password|BEGIN (RSA |OPENSSH |EC |DSA )?PRIVATE KEY|aws_access|ghp_|github_token'
   ```
   Triage every hit; `key_token_to_code` and similar code identifiers are
   known false positives.
5. Commit the release prep, push, wait for CI green on `main`.
6. Create the **signed** tag:
   ```sh
   git tag -s vX.Y.Z -m "Release vX.Y.Z"
   git show vX.Y.Z --show-signature   # verify signature
   git push origin vX.Y.Z
   ```
7. Draft the GitHub Release from the tag: summarize user-visible changes,
   link PRs, attach the `.deb` built by CI if applicable.
8. Announce only after the tag signature verifies on GitHub
   (tag page shows "Verified").

## Never

- Never re-tag or force-push a published tag; cut a new patch release.
- Never publish to crates.io without a separate decision + audit
  (current policy: do NOT publish).
- Never commit secrets to fix a release — rotate + cut a new release.
