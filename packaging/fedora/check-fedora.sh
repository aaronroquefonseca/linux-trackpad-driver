#!/usr/bin/env bash
# check-fedora.sh — CI/local validation for Fedora/COPR packaging.
#
# Runs anywhere with bash + rpm tooling; systemd-rpm-macros and rpmlint
# are optional (macro fallbacks are defined inline, rpmlint is advisory).
# Exits nonzero on any failure. Mirrors the `copr` job in
# .github/workflows/copr.yml.
#
# Checks: spec Version matches Cargo.toml/Cargo.lock/debian/changelog head;
# License tags name MIT and Apache-2.0; every installed source exists in the
# tree; the payload matches debian/*.install member-for-member (modulo
# install-root prefixes) with the config shipped via %config(noreplace) as
# debian/rules does (never via a .install rename); scriptlets never
# enable/start units; `rpmspec -q` parses; `rpmbuild -bs` produces an SRPM
# from a tree snapshot (proves Source0 layout + spec end to end).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

SPEC=packaging/fedora/linux-trackpad-driver.spec

fail() { echo "check-fedora: error: $1" >&2; exit 1; }

[ -f "$SPEC" ] || fail "missing $SPEC"

# 1. Version sync: spec == Cargo.toml == Cargo.lock == debian/changelog head
#    (same contract as packaging/release/check-version-sync.sh).
spec_ver="$(sed -n 's/^Version:[[:space:]]*//p' "$SPEC" | head -n 1)"
[[ "$spec_ver" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || fail "bad Version '$spec_ver' (want X.Y.Z)"
cargo_ver="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
[ "$cargo_ver" = "$spec_ver" ] \
  || fail "Cargo.toml version $cargo_ver != spec Version $spec_ver"
lock_ver="$(grep -A1 '^name = "linux-trackpad-driver"$' Cargo.lock \
  | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)"
[ "$lock_ver" = "$spec_ver" ] \
  || fail "Cargo.lock version $lock_ver != spec Version $spec_ver"
head -n 1 debian/changelog | grep -q "($spec_ver" \
  || fail "debian/changelog head does not match $spec_ver"

# 2. License tags: dual MIT OR Apache-2.0 (matches Cargo.toml + debian/copyright).
grep -q '^License:.*MIT' "$SPEC" || fail "License tag missing MIT"
grep -q '^License:.*Apache-2.0' "$SPEC" || fail "License tag missing Apache-2.0"
[ -f LICENSE-MIT ] && [ -f LICENSE-APACHE ] \
  || fail "license files missing from tree"

# 3. Every installed source exists (binary excluded: built, not in tree —
#    same carve-out as the `deb` job in .github/workflows/ci.yml).
while read -r src _; do
  case "$src" in ''|\#*) continue;; esac
  case "$src" in target/*) continue;; esac
  test -e "$src" || fail "missing install source: $src"
done < debian/linux-trackpad-driver.install
for src in config/default.toml udev/99-linux-trackpad-driver.rules \
    systemd/linux-trackpad-driver.service \
    systemd/linux-trackpad-driver-reconnect@.service \
    systemd/linux-trackpad-driver-resume.service \
    systemd/system-sleep/ltd-resume; do
  test -e "$src" || fail "missing spec source: $src"
done
grep -q 'install -Dm644 config/default.toml debian/linux-trackpad-driver/etc/linux-trackpad-driver/config.toml' debian/rules \
  || fail "debian/rules config install line changed; re-check spec parity"
grep -q '%config(noreplace).*config.toml' "$SPEC" \
  || fail "spec must ship config.toml as %config(noreplace)"

# 4. Payload parity with debian/*.install: same 6 members (binary, udev
#    rule, 3 units, sleep hook) modulo install-root prefixes. The spec
#    spells paths with %{name}, so expand it before matching.
spec_expanded="$(sed 's/%{name}/linux-trackpad-driver/g' "$SPEC")"
for member in 'ltd' \
    'udev/99-linux-trackpad-driver.rules' \
    'systemd/linux-trackpad-driver.service' \
    'systemd/linux-trackpad-driver-reconnect@.service' \
    'systemd/linux-trackpad-driver-resume.service' \
    'systemd/system-sleep/ltd-resume'; do
  printf '%s\n' "$spec_expanded" | grep -qF "$member" \
    || fail "spec missing payload member: $member"
done

# 5. Policy: units ship disabled — scriptlets must never enable/start.
#    Only scan scriptlet bodies (%post/%preun/%postun), skipping comments:
#    %description/README text legitimately shows the enable command.
if awk '/^%(post|preun|postun)( |$)/{in_s=1; next} /^%/{in_s=0} in_s' "$SPEC" \
    | grep -vE '^[[:space:]]*#' | grep -nE 'systemctl +(enable|start)'; then
  fail 'spec scriptlets must never enable/start units'
fi

# 6. Parse check. systemd-rpm-macros may be absent (e.g. Ubuntu CI runner):
#    define its path macros with standard values so the parse is hermetic.
if command -v rpmspec >/dev/null 2>&1; then
  rpmspec -q \
    -D "_unitdir /usr/lib/systemd/system" \
    -D "_udevrulesdir /usr/lib/udev/rules.d" \
    -D "_licensedir /usr/share/licenses" \
    "$SPEC" >/dev/null || fail "rpmspec -q failed to parse $SPEC"
else
  echo 'check-fedora: warning: rpmspec not found, parse check skipped' >&2
fi

# 7. End-to-end SRPM build from a tree snapshot in tag-tarball layout
#    (checkout may be a .git-less archive export, so never rely on `git`
#    here — same rule as packaging/arch/check-aur.sh).
if command -v rpmbuild >/dev/null 2>&1; then
  WORK="$(mktemp -d)" || fail "cannot create temp working dir"
  # shellcheck disable=SC2064 # $WORK is fixed at trap-install time.
  trap "rm -rf -- \"$WORK\"" EXIT
  mkdir -p "$WORK/SOURCES" "$WORK/SRPMS"
  tar --mtime='@0' --exclude='./target' --exclude='./.git' \
    --transform "s,^\\./,linux-trackpad-driver-${spec_ver}/," \
    -cf - . | gzip -n > "$WORK/SOURCES/linux-trackpad-driver-${spec_ver}.tar.gz"
  rpmbuild -bs \
    --define "_topdir $WORK" \
    --define "_sourcedir $WORK/SOURCES" \
    --define "_srcrpmdir $WORK/SRPMS" \
    "$SPEC" >/dev/null || fail "rpmbuild -bs failed"
  ls "$WORK"/SRPMS/*.src.rpm >/dev/null || fail "no SRPM produced"
  echo "check-fedora: SRPM ok: $(basename "$WORK"/SRPMS/*.src.rpm)"
  rm -rf -- "$WORK"
  trap - EXIT
else
  echo 'check-fedora: warning: rpmbuild not found, SRPM build skipped' >&2
fi

# 8. rpmlint when available — advisory only (mirrors the lintian advisory
#    step for .debs in .github/workflows/ci.yml).
if command -v rpmlint >/dev/null 2>&1; then
  rpmlint "$SPEC" || echo '::warning ::rpmlint reported tags'
else
  echo 'check-fedora: warning: rpmlint not found, lint skipped' >&2
fi

echo 'check-fedora: OK'
