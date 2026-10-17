#!/usr/bin/env bash
# check-version-sync.sh — fail unless Cargo.toml, Cargo.lock and
# debian/changelog agree on the release VERSION (X.Y.Z).
#
# Usage: check-version-sync.sh X.Y.Z [REPO_ROOT]
# REPO_ROOT defaults to the checkout containing this script. Reads only the
# tag's tree (callers check out the exact tag commit first), never main.
set -euo pipefail

VERSION="${1:?usage: check-version-sync.sh X.Y.Z [REPO_ROOT]}"
ROOT="${2:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || { echo "check-version-sync: bad version '$VERSION' (want X.Y.Z)" >&2; exit 1; }

fail() { echo "check-version-sync: $1" >&2; exit 1; }

cargo_ver="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[ -n "$cargo_ver" ] || fail "no version in $ROOT/Cargo.toml"
[ "$cargo_ver" = "$VERSION" ] \
  || fail "Cargo.toml version $cargo_ver != tag version $VERSION (bump before tagging)"

lock_ver="$(grep -A1 '^name = "linux-trackpad-driver"$' "$ROOT/Cargo.lock" \
  | sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)"
[ -n "$lock_ver" ] || fail "package linux-trackpad-driver missing in $ROOT/Cargo.lock"
[ "$lock_ver" = "$VERSION" ] \
  || fail "Cargo.lock version $lock_ver != tag version $VERSION (run cargo build, commit Cargo.lock)"

changelog_head="$(head -n 1 "$ROOT/debian/changelog")"
[[ "$changelog_head" =~ ^linux-trackpad-driver\ \("$VERSION"(-[0-9]+)?\) ]] \
  || fail "debian/changelog head '$changelog_head' does not match $VERSION (dch -v $VERSION-1)"

echo "check-version-sync: Cargo.toml=$cargo_ver Cargo.lock=$lock_ver changelog ok ($VERSION)"
