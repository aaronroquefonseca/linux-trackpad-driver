#!/usr/bin/env bash
# verify-assets.sh — assert DIR holds the two per-arch release tarballs plus
# SHA256SUMS, with exactly one correct manifest entry each.
#
# Usage: verify-assets.sh X.Y.Z DIR
# Prints the two validated digests (BIN_X86_64_SHA=/BIN_AARCH64_SHA= lines)
# for the renderer. Fails closed on missing files, missing/duplicate manifest
# entries, or digest mismatch — before anything is pushed anywhere.
set -euo pipefail

VERSION="${1:?usage: verify-assets.sh X.Y.Z DIR}"
DIR="${2:?usage: verify-assets.sh X.Y.Z DIR}"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || { echo "verify-assets: bad version '$VERSION' (want X.Y.Z)" >&2; exit 1; }
[ -d "$DIR" ] || { echo "verify-assets: missing dir: $DIR" >&2; exit 1; }
MANIFEST="$DIR/SHA256SUMS"
[ -f "$MANIFEST" ] || { echo "verify-assets: missing $MANIFEST" >&2; exit 1; }

fail() { echo "verify-assets: $1" >&2; exit 1; }

check_one() { # arch_suffix
  local name="linux-trackpad-driver-v${VERSION}-$1-unknown-linux-gnu.tar.gz"
  [ -f "$DIR/$name" ] || fail "missing asset $DIR/$name"
  local entry
  entry="$(awk -v want="$name" '$2 == want { print $1 }' "$MANIFEST")"
  [ -n "$entry" ] || fail "SHA256SUMS has no entry for $name"
  [ "$(printf '%s\n' "$entry" | wc -l)" -eq 1 ] \
    || fail "SHA256SUMS has duplicate entries for $name"
  [[ "$entry" =~ ^[0-9a-f]{64}$ ]] \
    || fail "SHA256SUMS entry for $name is not 64 lowercase hex"
  local got
  got="$(sha256sum -- "$DIR/$name" | awk '{print $1}')"
  [ "$got" = "$entry" ] \
    || fail "digest mismatch for $name (manifest $entry, file $got)"
  printf '%s=%s\n' "$2" "$entry"
}

check_one x86_64 BIN_X86_64_SHA
check_one aarch64 BIN_AARCH64_SHA
