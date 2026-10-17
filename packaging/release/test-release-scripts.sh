#!/usr/bin/env bash
# test-release-scripts.sh — offline guards for the release/AUR pipeline.
#
# No secrets, no network, no real AUR, no project tags: every git fixture
# lives in a disposable dir (mktemp) and uses fake versions (9.x). Run from
# the repo root: packaging/release/test-release-scripts.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REL="$REPO_ROOT/packaging/release"
TMP_BASE="$(mktemp -d)"
trap 'rm -rf -- "$TMP_BASE"' EXIT

pass=0
ok() { pass=$((pass + 1)); echo "ok: $1"; }
must() { # description, command... — success required, never silently skipped
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then
    ok "$desc"
  else
    echo "FAIL: $desc" >&2; exit 1
  fi
}
must_pipe() { # description, needle, haystack-var-name
  local desc="$1" needle="$2" hay="$3"
  if printf '%s\n' "$hay" | grep -q "$needle"; then
    ok "$desc"
  else
    echo "FAIL: $desc" >&2; exit 1
  fi
}
expect_fail() { # description, command...
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then
    echo "FAIL: $desc (expected failure, got success)" >&2; exit 1
  fi
  ok "$desc"
}

# 1. version sync -----------------------------------------------------------
FIX="$TMP_BASE/sync"; mkdir -p "$FIX/debian"
printf '[package]\nname = "linux-trackpad-driver"\nversion = "9.9.9"\n' > "$FIX/Cargo.toml"
printf '[[package]]\nname = "linux-trackpad-driver"\nversion = "9.9.9"\n' > "$FIX/Cargo.lock"
printf 'linux-trackpad-driver (9.9.9-1) unstable; urgency=medium\n' > "$FIX/debian/changelog"
must "version sync accepts matching tree" "$REL/check-version-sync.sh" 9.9.9 "$FIX"
printf '[package]\nname = "linux-trackpad-driver"\nversion = "9.9.8"\n' > "$FIX/Cargo.toml"
expect_fail "version sync rejects Cargo.toml drift" "$REL/check-version-sync.sh" 9.9.9 "$FIX"

# 2. release publicity gate --------------------------------------------------
printf '{"tagName":"v9.9.9","isDraft":false,"isPrerelease":false}' > "$TMP_BASE/public.json"
must "public final release passes" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/public.json"
printf '{"tagName":"v9.9.9","isDraft":true,"isPrerelease":false}' > "$TMP_BASE/draft.json"
expect_fail "draft release rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/draft.json"
printf '{"tagName":"v9.9.9","isDraft":false,"isPrerelease":true}' > "$TMP_BASE/pre.json"
expect_fail "prerelease rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/pre.json"
printf '{"tagName":"v9.9.8","isDraft":false,"isPrerelease":false}' > "$TMP_BASE/other.json"
expect_fail "tag mismatch rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/other.json"

# 3. asset manifest gate ------------------------------------------------------
ASSETS="$TMP_BASE/assets"; mkdir -p "$ASSETS"
echo "fake x86_64 payload" > "$ASSETS/linux-trackpad-driver-v9.9.9-x86_64-unknown-linux-gnu.tar.gz"
echo "fake aarch64 payload" > "$ASSETS/linux-trackpad-driver-v9.9.9-aarch64-unknown-linux-gnu.tar.gz"
( cd "$ASSETS" && sha256sum -- linux-trackpad-driver-v9.9.9-*.tar.gz > SHA256SUMS )
OUT="$("$REL/verify-assets.sh" 9.9.9 "$ASSETS")" || { echo "FAIL: manifest accepts exact assets" >&2; exit 1; }
ok "manifest accepts exact assets"
must_pipe "x86_64 digest reported" '^BIN_X86_64_SHA=[0-9a-f]\{64\}$' "$OUT"
must_pipe "aarch64 digest reported" '^BIN_AARCH64_SHA=[0-9a-f]\{64\}$' "$OUT"
echo "tampered" > "$ASSETS/linux-trackpad-driver-v9.9.9-x86_64-unknown-linux-gnu.tar.gz"
expect_fail "digest mismatch fails before push" "$REL/verify-assets.sh" 9.9.9 "$ASSETS"
rm "$ASSETS/linux-trackpad-driver-v9.9.9-aarch64-unknown-linux-gnu.tar.gz"
expect_fail "missing asset fails" "$REL/verify-assets.sh" 9.9.9 "$ASSETS"

# 4. AUR push guards against mocked bare remotes ------------------------------
mk_rendered() { # dir pkgver
  mkdir -p "$1"
  printf 'pkgname=%s\npkgver=%s\npkgrel=1\n' "$2" "$3" > "$1/PKGBUILD"
  printf 'pkgbase = %s\n\tpkgver = %s\n' "$2" "$3" > "$1/.SRCINFO"
  printf '# install stub\n' > "$1/ltd.install"
}
REMOTE="$TMP_BASE/aur-remote.git"
git init -q --bare "$REMOTE"
mk_rendered "$TMP_BASE/r1" linux-trackpad-driver 9.9.9
must "first push to empty remote succeeds" "$REL/publish-aur.sh" \
  --pkg linux-trackpad-driver --dir "$TMP_BASE/r1" \
  --message "test 9.9.9" --expect-pkgver 9.9.9 --remote "$REMOTE"
[ "$(git --git-dir="$REMOTE" rev-parse --abbrev-ref HEAD)" = "master" ] \
  || { echo "FAIL: first push establishes master" >&2; exit 1; }
ok "first push establishes master"
BEFORE="$(git --git-dir="$REMOTE" rev-parse HEAD)"
must "identical retry is a noop" "$REL/publish-aur.sh" \
  --pkg linux-trackpad-driver --dir "$TMP_BASE/r1" \
  --message "test 9.9.9 retry" --expect-pkgver 9.9.9 --remote "$REMOTE"
[ "$(git --git-dir="$REMOTE" rev-parse HEAD)" = "$BEFORE" ] \
  || { echo "FAIL: noop pushes no new commit" >&2; exit 1; }
ok "noop pushes no new commit"
# Unrelated remote files survive automation.
git clone -q "$REMOTE" "$TMP_BASE/probe" 2>/dev/null
( cd "$TMP_BASE/probe" && echo keep > NOTES && git add NOTES \
  && git -c user.name=t -c user.email=t@t commit -qm notes && git push -q origin master )
must "push preserves unrelated remote files" "$REL/publish-aur.sh" \
  --pkg linux-trackpad-driver --dir "$TMP_BASE/r1" \
  --message "test 9.9.9 again" --expect-pkgver 9.9.9 --remote "$REMOTE"
git clone -q "$REMOTE" "$TMP_BASE/probe2" 2>/dev/null
[ -f "$TMP_BASE/probe2/NOTES" ] \
  || { echo "FAIL: unrelated file intact after sync" >&2; exit 1; }
ok "unrelated file intact after sync"
mk_rendered "$TMP_BASE/r2" linux-trackpad-driver 9.9.10
must "newer version pushes" "$REL/publish-aur.sh" \
  --pkg linux-trackpad-driver --dir "$TMP_BASE/r2" \
  --message "test 9.9.10" --expect-pkgver 9.9.10 --remote "$REMOTE"
expect_fail "stale event (older version) refused" "$REL/publish-aur.sh" \
  --pkg linux-trackpad-driver --dir "$TMP_BASE/r1" \
  --message "test stale 9.9.9" --expect-pkgver 9.9.9 --remote "$REMOTE"
[ "$(git --git-dir="$REMOTE" show HEAD:PKGBUILD | sed -n 's/^pkgver=//p')" = "9.9.10" ] \
  || { echo "FAIL: refused push leaves remote untouched" >&2; exit 1; }
ok "refused push leaves remote untouched"
expect_fail "force-push never attempted (push is append-only)" \
  grep -nE 'git .*push.*--force' "$REL/publish-aur.sh"

# 5. workflow contracts ---------------------------------------------------------
WF="$REPO_ROOT/.github/workflows"
for required in release-tag.yml aur-stable.yml aur-git.yml; do
  [ -f "$WF/$required" ] || { echo "FAIL: missing $WF/$required" >&2; exit 1; }
done
[ ! -f "$WF/nightly.yml" ] || { echo "FAIL: obsolete nightly.yml still present" >&2; exit 1; }
ok "obsolete nightly.yml is gone"
assert_grep() { # file pattern description
  if grep -q "$2" "$1"; then
    ok "$3"
  else
    echo "FAIL: $3 ($1 lacks: $2)" >&2; exit 1
  fi
}
assert_absent() { # file extended-pattern description
  if grep -qnE "$2" "$1"; then
    echo "FAIL: $3 ($1 matches: $2)" >&2; exit 1
  fi
  ok "$3"
}
assert_grep "$WF/aur-git.yml" 'packaging/arch/\*\*' "aur-git watches packaging tree"
assert_absent "$WF/aur-git.yml" '^\s*-\s.*src/\*\*' \
  "aur-git fires on packaging changes, not driver commits"
for wf in release-tag.yml aur-stable.yml aur-git.yml; do
  assert_absent "$WF/$wf" '^  pull_request:' \
    "$wf never triggers on pull_request"
done
assert_grep "$WF/aur-stable.yml" 'types: \[published\]' "aur-stable runs on release published"
assert_grep "$WF/aur-stable.yml" 'ENABLE_AUR_STABLE' "aur-stable gated on ENABLE_AUR_STABLE"
assert_grep "$WF/aur-git.yml" 'ENABLE_AUR_GIT' "aur-git gated on ENABLE_AUR_GIT"
assert_grep "$WF/release-tag.yml" 'persist-credentials: false' "release checkout keeps no credentials"
assert_grep "$WF/release-tag.yml" 'LICENSE-APACHE' "release tarball stages both licenses"
assert_grep "$REL/publish-aur.sh" 'aur\.archlinux\.org ssh-ed25519' "AUR host key pinned, no ssh-keyscan"

# 6. real makepkg payload check for the prebuilt variant -------------------------
if [ "$(id -u)" -eq 0 ]; then
  echo "skip: makepkg payload check needs a non-root user (CI runs it as builder)"
else
  command -v makepkg >/dev/null || { echo "FAIL: makepkg required" >&2; exit 1; }
  PAY="$TMP_BASE/payload"; mkdir -p "$PAY/stage"
  top="linux-trackpad-driver-v9.9.9-x86_64-unknown-linux-gnu"
  mkdir -p "$PAY/stage/$top/config" "$PAY/stage/$top/udev" \
    "$PAY/stage/$top/systemd/system-sleep"
  echo "fake ltd" > "$PAY/stage/$top/ltd"
  echo "cfg" > "$PAY/stage/$top/config/default.toml"
  echo "rules" > "$PAY/stage/$top/udev/99-linux-trackpad-driver.rules"
  for unit in linux-trackpad-driver.service \
    linux-trackpad-driver-reconnect@.service \
    linux-trackpad-driver-resume.service; do
    printf '[Unit]\nDescription=test\n' > "$PAY/stage/$top/systemd/$unit"
  done
  printf '#!/bin/sh\nexit 0\n' > "$PAY/stage/$top/systemd/system-sleep/ltd-resume"
  for doc in README.md INSTALL.md LICENSE-MIT LICENSE-APACHE; do
    echo "$doc" > "$PAY/stage/$top/$doc"
  done
  tar -C "$PAY/stage" -czf "$PAY/bin-x86.tar.gz" "$top"
  arm_top="linux-trackpad-driver-v9.9.9-aarch64-unknown-linux-gnu"
  cp -r "$PAY/stage/$top" "$PAY/stage/$arm_top"
  tar -C "$PAY/stage" -czf "$PAY/bin-arm.tar.gz" "$arm_top"
  BXS="$(sha256sum "$PAY/bin-x86.tar.gz" | awk '{print $1}')"
  BAS="$(sha256sum "$PAY/bin-arm.tar.gz" | awk '{print $1}')"
  SRC_TAR="$PAY/src.tar.gz"
  # Working-tree snapshot (never relies on .git: CI checkout may be a
  # .git-less archive export when git is missing at checkout time).
  ( cd "$REPO_ROOT" && tar --mtime='@0' \
      --transform 's,^\./,linux-trackpad-driver-9.9.9/,' \
      -czf "$SRC_TAR" ./Cargo.toml ./Cargo.lock ./src ./config ./udev \
      ./systemd ./README.md ./INSTALL.md ./LICENSE-MIT ./LICENSE-APACHE )
  SRC_SHA="$(sha256sum "$SRC_TAR" | awk '{print $1}')"
  if ! "$REPO_ROOT/packaging/arch/render-aur.sh" --version 9.9.9 \
    --source-sha256 "$SRC_SHA" --bin-x86_64-sha256 "$BXS" \
    --bin-aarch64-sha256 "$BAS" --out "$PAY/aur" \
    --source-tarball "$SRC_TAR" \
    --bin-x86_64-tarball "$PAY/bin-x86.tar.gz" \
    --bin-aarch64-tarball "$PAY/bin-arm.tar.gz" >/dev/null; then
    echo "FAIL: renderer accepts the release-layout fixtures" >&2; exit 1
  fi
  ok "renderer accepts the release-layout fixtures"
  BIN_PKG="$PAY/aur/linux-trackpad-driver-bin"
  cp "$PAY/bin-x86.tar.gz" "$PAY/bin-arm.tar.gz" "$BIN_PKG/"
  sed -i "s|https://github.com/aaronroquefonseca/linux-trackpad-driver/releases/download/v\$pkgver/linux-trackpad-driver-v\$pkgver-x86_64-unknown-linux-gnu.tar.gz|bin-x86.tar.gz|; s|https://github.com/aaronroquefonseca/linux-trackpad-driver/releases/download/v\$pkgver/linux-trackpad-driver-v\$pkgver-aarch64-unknown-linux-gnu.tar.gz|bin-arm.tar.gz|" \
    "$BIN_PKG/PKGBUILD"
  if ! ( cd "$BIN_PKG" && makepkg --noconfirm --noprogressbar >/dev/null 2>&1 ); then
    echo "FAIL: real makepkg builds the -bin payload" >&2; exit 1
  fi
  ok "real makepkg builds the -bin payload"
fi

echo "release-scripts: all $pass checks passed"
