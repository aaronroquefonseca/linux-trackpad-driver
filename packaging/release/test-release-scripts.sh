#!/usr/bin/env bash
# test-release-scripts.sh — offline guards for the release/AUR pipeline.
#
# No secrets, no network, no real AUR, no project tags: every git fixture
# lives in a disposable dir (mktemp) and uses fake versions (9.x). Run from
# the repo root: packaging/release/test-release-scripts.sh
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
REL="$REPO_ROOT/packaging/release"
WF="$REPO_ROOT/.github/workflows"
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
printf '{"tag_name":"v9.9.9","draft":false,"prerelease":false}' > "$TMP_BASE/public-snake.json"
must "REST snake_case public final passes" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/public-snake.json"
printf '{"tagName":"v9.9.9","tag_name":"v9.9.9","isDraft":false,"draft":false,"isPrerelease":false,"prerelease":false}' > "$TMP_BASE/public-dual.json"
must "consistent dual aliases pass" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/public-dual.json"
printf '{"tag_name":"v9.9.9","isDraft":false,"prerelease":false}' > "$TMP_BASE/public-mixed.json"
must "mixed aliases pass" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/public-mixed.json"
printf '{"tagName":"v9.9.9","isDraft":true,"isPrerelease":false}' > "$TMP_BASE/draft.json"
expect_fail "draft release rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/draft.json"
printf '{"tagName":"v9.9.9","isDraft":false,"isPrerelease":true}' > "$TMP_BASE/pre.json"
expect_fail "prerelease rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/pre.json"
printf '{"tagName":"v9.9.8","isDraft":false,"isPrerelease":false}' > "$TMP_BASE/other.json"
expect_fail "tag mismatch rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/other.json"
printf '{"tagName":"v9.9.9"}' > "$TMP_BASE/bare.json"
expect_fail "bare tag without publicity fields rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/bare.json"
printf '{"tagName":"v9.9.9","isDraft":false}' > "$TMP_BASE/nopre.json"
expect_fail "missing prerelease field rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/nopre.json"
printf '{"tagName":"v9.9.9","isPrerelease":false}' > "$TMP_BASE/nodraft.json"
expect_fail "missing draft field rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/nodraft.json"
printf '{"tagName":"v9.9.9","isDraft":null,"isPrerelease":false}' > "$TMP_BASE/nulldraft.json"
expect_fail "null draft rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/nulldraft.json"
printf '{"tagName":"v9.9.9","isDraft":false,"isPrerelease":null}' > "$TMP_BASE/nullpre.json"
expect_fail "null prerelease rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/nullpre.json"
printf '{"tagName":"v9.9.9","isDraft":"false","isPrerelease":false}' > "$TMP_BASE/strdraft.json"
expect_fail "string draft rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/strdraft.json"
printf '{"tagName":"v9.9.9","isDraft":false,"isPrerelease":"false"}' > "$TMP_BASE/strpre.json"
expect_fail "string prerelease rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/strpre.json"
printf '{"tagName":"v9.9.9","isDraft":0,"isPrerelease":false}' > "$TMP_BASE/numdraft.json"
expect_fail "numeric draft rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/numdraft.json"
printf '{"tagName":"v9.9.9","isDraft":false,"isPrerelease":0}' > "$TMP_BASE/numpre.json"
expect_fail "numeric prerelease rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/numpre.json"
printf '{"tagName":"v9.9.9","isDraft":false,"draft":true,"isPrerelease":false}' > "$TMP_BASE/conflictdraft.json"
expect_fail "conflicting draft aliases rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/conflictdraft.json"
printf '{"tagName":"v9.9.9","isDraft":false,"isPrerelease":false,"prerelease":true}' > "$TMP_BASE/conflictpre.json"
expect_fail "conflicting prerelease aliases rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/conflictpre.json"
printf '[]' > "$TMP_BASE/array.json"
expect_fail "non-object JSON rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/array.json"
printf 'null' > "$TMP_BASE/null.json"
expect_fail "null JSON rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/null.json"
printf '{"tagName":null,"isDraft":false,"isPrerelease":false}' > "$TMP_BASE/nulltag.json"
expect_fail "null tag rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/nulltag.json"
printf '{"tagName":"v9.9.9","tag_name":"v9.9.8","isDraft":false,"isPrerelease":false}' > "$TMP_BASE/conflicttag.json"
expect_fail "conflicting tag aliases rejected" "$REL/check-release-json.sh" v9.9.9 "$TMP_BASE/conflicttag.json"

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
# Regression (review P1): a relative --dir must resolve before the script
# cds into the temporary clone. r2/9.9.10 is already published, so this
# must succeed as a noop from a different working directory.
mkdir -p "$TMP_BASE/relwork"
rm -rf "$TMP_BASE/relwork/rendered"
cp -r "$TMP_BASE/r2" "$TMP_BASE/relwork/rendered"
if ! ( cd "$TMP_BASE/relwork" && "$REL/publish-aur.sh" \
  --pkg linux-trackpad-driver --dir rendered \
  --message "test relative dir" --expect-pkgver 9.9.10 --remote "$REMOTE" \
  >/dev/null 2>&1 ); then
  echo "FAIL: relative --dir resolves outside the clone" >&2; exit 1
fi
ok "relative --dir resolves outside the clone"
# ssh:// remotes fail closed when no ssh client exists (Arch git does not
# depend on openssh), instead of dying mid-clone.
HIDE="$TMP_BASE/hide"; mkdir -p "$HIDE"
BIN_BASH="$(command -v bash)"
for tool in mktemp grep chmod; do
  ln -sf "$(command -v "$tool")" "$HIDE/$tool"
done
if PATH="$HIDE" AUR_SSH_PRIVATE_KEY=dummy "$BIN_BASH" "$REL/publish-aur.sh" \
  --pkg linux-trackpad-driver --dir "$TMP_BASE/r1" \
  --message "test no ssh" --expect-pkgver 9.9.9 \
  --remote ssh://aur@aur.archlinux.org/linux-trackpad-driver.git \
  >/dev/null 2>"$TMP_BASE/ssh-err.txt"; then
  echo "FAIL: ssh client guard fails closed without openssh" >&2; exit 1
fi
grep -q "ssh client not found" "$TMP_BASE/ssh-err.txt" \
  || { echo "FAIL: ssh guard names the missing client" >&2; exit 1; }
ok "ssh client guard fails closed without openssh"

# 5. draft manager against a stubbed gh (empty cwd, no git inference) ---------
DRAFT_TAG="v9.9.9"
DRAFT_COMMIT="$(printf 'c%.0s' $(seq 40))"
mk_draft_dist() { # dir
  mkdir -p "$1"
  echo x86 > "$1/linux-trackpad-driver-v9.9.9-x86_64-unknown-linux-gnu.tar.gz"
  echo arm > "$1/linux-trackpad-driver-v9.9.9-aarch64-unknown-linux-gnu.tar.gz"
  echo notes > "$1/CHANGELOG-v9.9.9.md"
  echo deb > "$1/fake_9.9.9_amd64.deb"
  echo rpm > "$1/fake_9.9.9_x86_64.rpm"
  printf 'c%.0s' $(seq 40) > "$1/BUILD_COMMIT"
  ( cd "$1" && sha256sum -- * > SHA256SUMS )
}
FAKEBIN="$TMP_BASE/fakebin"; mkdir -p "$FAKEBIN"
cat > "$FAKEBIN/gh" <<'STUB'
#!/usr/bin/env bash
# Minimal gh stub: backs `release view/upload/edit/create` with a state file
# so the draft manager can be exercised with no network and no git repo.
echo "$@" >> "$FAKE_GH_CALLS"
repo_ok=0
args=( "$@" )
for (( i=0; i<${#args[@]}; i++ )); do
  if [ "${args[$i]}" = "--repo" ] && [ "${args[$((i+1))]:-}" = "$FAKE_GH_REPO" ]; then
    repo_ok=1
  fi
done
[ "$repo_ok" = "1" ] || { echo "stub gh: missing --repo $FAKE_GH_REPO" >&2; exit 3; }
state="$(cat "$FAKE_GH_STATE_FILE")"
case "${args[1]:-}" in
  view)
    [ "$state" = "missing" ] && { echo "release not found" >&2; exit 1; }
    case "${args[${#args[@]}-1]}" in
      .isDraft) [ "$state" = "draft" ] && echo "true" || echo "false" ;;
      .isPrerelease) [ "$state" = "prerelease" ] && echo "true" || echo "false" ;;
      *) echo "true" ;;
    esac
    ;;
  upload)
    [ "$state" = "draft" ] || { echo "stub gh: upload outside draft" >&2; exit 1; }
    [[ " $* " == *" --clobber "* ]] || { echo "stub gh: upload without --clobber" >&2; exit 1; }
    ;;
  edit) [ "$state" = "draft" ] || { echo "stub gh: edit outside draft" >&2; exit 1; } ;;
  create)
    [ "$state" = "missing" ] || { echo "stub gh: create over existing" >&2; exit 1; }
    [[ " $* " == *" --draft "* ]] || { echo "stub gh: create without --draft" >&2; exit 1; }
    [[ " $* " == *" --target $FAKE_GH_COMMIT "* ]] \
      || { echo "stub gh: create without exact --target" >&2; exit 1; }
    printf 'draft' > "$FAKE_GH_STATE_FILE"
    ;;
  *) echo "stub gh: unknown subcommand" >&2; exit 2 ;;
esac
STUB
chmod +x "$FAKEBIN/gh"
run_draft() { # state dir
  printf '%s' "$1" > "$TMP_BASE/gh-state"
  : > "$TMP_BASE/gh-calls"
  ( cd "$TMP_BASE/empty-cwd" 2>/dev/null || mkdir -p "$TMP_BASE/empty-cwd"; cd "$TMP_BASE/empty-cwd"
    FAKE_GH_STATE_FILE="$TMP_BASE/gh-state" FAKE_GH_CALLS="$TMP_BASE/gh-calls" \
    FAKE_GH_REPO="testowner/testrepo" FAKE_GH_COMMIT="$DRAFT_COMMIT" \
    PATH="$FAKEBIN:$PATH" GH_REPO="testowner/testrepo" \
    "$REL/manage-draft-release.sh" --tag "$DRAFT_TAG" --commit "$DRAFT_COMMIT" --dist-dir "$2" )
}
DWORK="$TMP_BASE/dwork"; mk_draft_dist "$DWORK"
mkdir -p "$TMP_BASE/empty-cwd"
# Finding-3 shape: no GH_REPO (fresh job, empty cwd) must fail naming GH_REPO.
if ( cd "$TMP_BASE/empty-cwd" && PATH="$FAKEBIN:$PATH" \
  "$REL/manage-draft-release.sh" --tag "$DRAFT_TAG" --commit "$DRAFT_COMMIT" \
  --dist-dir "$DWORK" 2>"$TMP_BASE/norepo-err.txt" ); then
  echo "FAIL: draft manager requires GH_REPO" >&2; exit 1
fi
grep -q "GH_REPO" "$TMP_BASE/norepo-err.txt" \
  || { echo "FAIL: missing GH_REPO names the variable" >&2; exit 1; }
ok "draft manager requires GH_REPO (no repo inference)"
run_draft missing "$DWORK" >/dev/null 2>&1 \
  || { echo "FAIL: draft creation from scratch" >&2; exit 1; }
grep -q "create $DRAFT_TAG" "$TMP_BASE/gh-calls" && ok "missing release is created, never published"
grep -q -- "--draft" "$TMP_BASE/gh-calls" \
  || { echo "FAIL: creation stays a draft" >&2; exit 1; }
ok "creation stays a draft"
[ "$(cat "$TMP_BASE/gh-state")" = "draft" ] \
  || { echo "FAIL: created release ends as draft" >&2; exit 1; }
ok "created release ends as draft"
grep -q -- "\.deb" "$TMP_BASE/gh-calls" \
  || { echo "FAIL: creation uploads .deb" >&2; exit 1; }
ok "creation uploads .deb"
grep -q -- "\.rpm" "$TMP_BASE/gh-calls" \
  || { echo "FAIL: creation uploads .rpm" >&2; exit 1; }
ok "creation uploads .rpm"
# RPMs are optional: a dist without any .rpm must still create the draft.
DNORPM="$TMP_BASE/dnorpm"; mk_draft_dist "$DNORPM"; rm "$DNORPM"/*.rpm
( cd "$DNORPM" && sha256sum -- * > SHA256SUMS )
run_draft missing "$DNORPM" >/dev/null 2>&1 \
  || { echo "FAIL: draft creation without rpm" >&2; exit 1; }
ok "draft creation without rpm succeeds"
grep -q -- "\.deb" "$TMP_BASE/gh-calls" \
  || { echo "FAIL: rpm-less creation still uploads .deb" >&2; exit 1; }
ok "rpm-less creation still uploads .deb"
run_draft draft "$DWORK" >/dev/null 2>&1 \
  || { echo "FAIL: draft refresh" >&2; exit 1; }
grep -q "upload $DRAFT_TAG" "$TMP_BASE/gh-calls" \
  || { echo "FAIL: draft refresh re-uploads" >&2; exit 1; }
grep -q "edit $DRAFT_TAG" "$TMP_BASE/gh-calls" \
  || { echo "FAIL: draft refresh updates notes" >&2; exit 1; }
if grep -q "create $DRAFT_TAG" "$TMP_BASE/gh-calls"; then
  echo "FAIL: refresh must not recreate" >&2; exit 1
fi
ok "draft refresh re-uploads with clobber, no recreate"
expect_fail "published release is never mutated" run_draft published "$DWORK"
if grep -qE "upload $DRAFT_TAG|edit $DRAFT_TAG|create $DRAFT_TAG" "$TMP_BASE/gh-calls"; then
  echo "FAIL: published path made no mutating calls" >&2; exit 1
fi
ok "published path made no mutating calls"
expect_fail "prerelease is rejected" run_draft prerelease "$DWORK"
DBROKEN="$TMP_BASE/dbroken"; mk_draft_dist "$DBROKEN"; rm "$DBROKEN"/fake_*.deb
: > "$TMP_BASE/gh-calls"
if run_draft missing "$DBROKEN" >/dev/null 2>&1; then
  echo "FAIL: missing asset fails before any gh call" >&2; exit 1
fi
[ -s "$TMP_BASE/gh-calls" ] && { echo "FAIL: no gh calls on missing asset" >&2; exit 1; }
ok "missing asset fails before any gh call"

# 6. stable verification binds artifacts to the release commit ---------------
SV="$TMP_BASE/sverify"; mkdir -p "$SV"
git init -q -b main "$SV/upstream"
( cd "$SV/upstream" \
  && printf '[package]\nname = "linux-trackpad-driver"\nversion = "9.9.9"\n' > Cargo.toml \
  && git add Cargo.toml \
  && git -c user.email=t@t -c user.name=t commit -qm "release 9.9.9" \
  && git tag v9.9.9 )
SHA_A="$(git --git-dir="$SV/upstream/.git" rev-parse HEAD)"
git clone -q "$SV/upstream" "$SV/checkout" 2>/dev/null
mk_sv_assets() { # dir version build_sha
  mkdir -p "$1"
  echo "x86 $2 $3" > "$1/linux-trackpad-driver-v$2-x86_64-unknown-linux-gnu.tar.gz"
  echo "arm $2 $3" > "$1/linux-trackpad-driver-v$2-aarch64-unknown-linux-gnu.tar.gz"
  printf '%s' "$3" > "$1/BUILD_COMMIT"
  ( cd "$1" && sha256sum -- linux-trackpad-driver-v*.tar.gz BUILD_COMMIT > SHA256SUMS )
}
mk_src_tar() { # out version
  rm -rf "$SV/srctree" && mkdir -p "$SV/srctree/linux-trackpad-driver-$2"
  printf '[package]\nname = "linux-trackpad-driver"\nversion = "%s"\n' "$2" \
    > "$SV/srctree/linux-trackpad-driver-$2/Cargo.toml"
  tar -C "$SV/srctree" -czf "$1" "linux-trackpad-driver-$2"
}
sv_json() { # out tag draft prerelease
  printf '{"tagName":"%s","isDraft":%s,"isPrerelease":%s}' "$2" "$3" "$4" > "$1"
}
run_sv() { # assets json src api build-extra-args...
  "$REL/verify-stable-release.sh" --tag "$SV_TAG" --release-json "$1" \
    --assets-dir "$2" --source-tarball "$3" --api-commit "$4" \
    --origin-url "$SV/upstream" --checkout-dir "$SV/checkout" --main-ref origin/main
}
SV_TAG="v9.9.9"
mk_sv_assets "$SV/a-ok" 9.9.9 "$SHA_A"
mk_src_tar "$SV/src-ok.tar.gz" 9.9.9
sv_json "$SV/j-ok.json" v9.9.9 false false
SV_OUT="$(run_sv "$SV/j-ok.json" "$SV/a-ok" "$SV/src-ok.tar.gz" "$SHA_A")" \
  || { echo "FAIL: matching release verifies" >&2; exit 1; }
printf '%s\n' "$SV_OUT" | grep -q "^COMMIT=$SHA_A\$" \
  || { echo "FAIL: verified commit reported" >&2; exit 1; }
ok "matching release verifies and reports its commit"
# Reviewer repro: binaries built from A, tag + source moved to B.
( cd "$SV/upstream" && echo moved > moved.txt && git add moved.txt \
  && git -c user.email=t@t -c user.name=t commit -qm "later" \
  && git tag -f v9.9.9 >/dev/null )
SHA_B="$(git --git-dir="$SV/upstream/.git" rev-parse HEAD)"
mk_src_tar "$SV/src-b.tar.gz" 9.9.9
expect_fail "moved tag (binaries A, source B) refused" \
  run_sv "$SV/j-ok.json" "$SV/a-ok" "$SV/src-b.tar.gz" "$SHA_B"
# BUILD_COMMIT missing or tampered fails before any push.
SV_NOBC="$SV/a-nobc"; mk_sv_assets "$SV_NOBC" 9.9.9 "$SHA_A"; rm "$SV_NOBC/BUILD_COMMIT"
expect_fail "missing BUILD_COMMIT refused" \
  run_sv "$SV/j-ok.json" "$SV_NOBC" "$SV/src-ok.tar.gz" "$SHA_A"
SV_TAMP="$SV/a-tamp"; mk_sv_assets "$SV_TAMP" 9.9.9 "$SHA_A"
printf 'd%.0s' $(seq 40) > "$SV_TAMP/BUILD_COMMIT"
expect_fail "tampered BUILD_COMMIT refused" \
  run_sv "$SV/j-ok.json" "$SV_TAMP" "$SV/src-ok.tar.gz" "$SHA_A"
# Tag on a side branch never reaches main.
( cd "$SV/upstream" && git checkout -q -b side "$SHA_A" \
  && echo side > side.txt && git add side.txt \
  && git -c user.email=t@t -c user.name=t commit -qm "side" \
  && git tag v9.9.8 && git checkout -q main )
SHA_S="$(git --git-dir="$SV/upstream/.git" rev-parse side)"
SV_TAG="v9.9.8"
mk_sv_assets "$SV/a-side" 9.9.8 "$SHA_S"
sv_json "$SV/j-side.json" v9.9.8 false false
expect_fail "side-branch release refused" \
  run_sv "$SV/j-side.json" "$SV/a-side" "$SV/src-ok.tar.gz" "$SHA_S"
SV_TAG="v9.9.9"
sv_json "$SV/j-draft.json" v9.9.9 true false
expect_fail "draft release refused by stable verify" \
  run_sv "$SV/j-draft.json" "$SV/a-ok" "$SV/src-ok.tar.gz" "$SHA_A"
mk_src_tar "$SV/src-drift.tar.gz" 9.9.8
expect_fail "source version drift refused" \
  run_sv "$SV/j-ok.json" "$SV/a-ok" "$SV/src-drift.tar.gz" "$SHA_A"

# 7. -git gate: trusted main ref, no secret outside the push ---------------
GOOD_SHA="$(printf 'b%.0s' $(seq 40))"
run_gate() { # event ref input-publish toggle snapshot
  EVENT_NAME="$1" GITHUB_REF="$2" INPUT_PUBLISH="$3" AUR_GIT_VAR="$4" \
    SNAPSHOT="$5" "$REL/gate-aur-git.sh"
}
GATE_OUT="$(run_gate push refs/heads/main false true "$GOOD_SHA" 2>/dev/null)" \
  || { echo "FAIL: push gate allows" >&2; exit 1; }
printf '%s\n' "$GATE_OUT" | grep -q '^publish=true$' \
  || { echo "FAIL: armed push publishes" >&2; exit 1; }
printf '%s\n' "$GATE_OUT" | grep -q "^short=${GOOD_SHA:0:7}\$" \
  || { echo "FAIL: short sha reported" >&2; exit 1; }
ok "armed push publishes with short sha"
GATE_OUT="$(run_gate push refs/heads/main false '' "$GOOD_SHA" 2>/dev/null)" \
  || { echo "FAIL: unarmed push gate allows" >&2; exit 1; }
printf '%s\n' "$GATE_OUT" | grep -q '^publish=false$' \
  || { echo "FAIL: unarmed push stays dry" >&2; exit 1; }
ok "unarmed push stays dry"
GATE_OUT="$(run_gate workflow_dispatch refs/heads/main true true "$GOOD_SHA" 2>/dev/null)" \
  || { echo "FAIL: main retry gate allows" >&2; exit 1; }
printf '%s\n' "$GATE_OUT" | grep -q '^publish=true$' \
  || { echo "FAIL: main retry publishes" >&2; exit 1; }
ok "manual retry on main publishes when armed"
GATE_OUT="$(run_gate workflow_dispatch refs/heads/main false true "$GOOD_SHA" 2>/dev/null)" \
  || { echo "FAIL: dry retry gate allows" >&2; exit 1; }
printf '%s\n' "$GATE_OUT" | grep -q '^publish=false$' \
  || { echo "FAIL: publish=false stays dry" >&2; exit 1; }
ok "publish=false stays dry"
# Reviewer repro: dispatch against an unmerged branch with publish=true and
# the toggle set must be denied, not publish=true.
expect_fail "feature-branch manual retry denied" \
  run_gate workflow_dispatch refs/heads/unmerged-packaging true true "$GOOD_SHA"
expect_fail "tag-ref manual retry denied" \
  run_gate workflow_dispatch refs/tags/v9.9.9 true true "$GOOD_SHA"
expect_fail "bad snapshot rejected" \
  run_gate push refs/heads/main false true deadbeef
# The private key is mapped only in the gated push step, never job-wide, so
# dry-run metadata generation cannot see publication credentials.
[ "$(grep -c 'AUR_SSH_PRIVATE_KEY: ${{ secrets' "$WF/aur-git.yml")" = "1" ] \
  || { echo "FAIL: key mapped exactly once (push step)" >&2; exit 1; }
awk '/name: push -git repo/{seen=1} /AUR_SSH_PRIVATE_KEY: /{if (!seen) exit 1}' \
  "$WF/aur-git.yml" \
  || { echo "FAIL: key scoped to the gated push step" >&2; exit 1; }
ok "key scoped to the gated push step"

# 8. workflow authorization contracts (evaluates the real `if:` logic) --------
must "workflow authorization contracts" python3 "$REL/test-workflow-auth.py"

# 9. workflow file contracts --------------------------------------------------
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
assert_grep "$WF/release-tag.yml" 'GH_REPO' "draft job binds GH_REPO (no repo inference)"
assert_grep "$WF/release-tag.yml" 'manage-draft-release.sh' "draft logic lives in the tested script"
assert_grep "$WF/release-tag.yml" 'LICENSE-APACHE' "release tarball stages both licenses"
assert_grep "$WF/aur-stable.yml" 'include-hidden-files' "rendered AUR tree keeps hidden metadata"
assert_grep "$WF/aur-git.yml" 'include-hidden-files' "-git evidence keeps hidden metadata"
assert_grep "$WF/aur-git.yml" 'packaging/release/publish-aur.sh' "-git watches its shared publisher"
assert_grep "$WF/aur-git.yml" 'openssh' "-git installs an ssh client"
assert_grep "$WF/aur-stable.yml" 'openssh' "stable push installs an ssh client"
assert_grep "$REL/publish-aur.sh" 'aur\.archlinux\.org ssh-ed25519' "AUR host key pinned, no ssh-keyscan"

# 10. real makepkg payload check for the prebuilt variant ------------------------
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
