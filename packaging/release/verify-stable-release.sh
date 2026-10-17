#!/usr/bin/env bash
# verify-stable-release.sh — verify a public, final release before AUR stable
# publication, binding every artifact to the exact release-build commit.
#
# Usage: verify-stable-release.sh --tag vX.Y.Z --release-json JSON
#   --assets-dir DIR --source-tarball FILE --api-commit SHA
#   --origin-url URL --checkout-dir DIR [--main-ref REF]
#
# JSON is `gh release view --json tagName,isDraft,isPrerelease`; DIR holds the
# downloaded assets (SHA256SUMS, both per-arch tarballs, BUILD_COMMIT);
# API_COMMIT is `gh api repos/.../commits/TAG --jq .sha`; ORIGIN_URL answers
# `git ls-remote` for the tag; CHECKOUT_DIR (full history) answers the
# main-ancestry check (default REF origin/main).
#
# Fails closed on: draft/prerelease/tag-mismatch, missing or mismatched
# manifest entries, BUILD_COMMIT missing from the checksummed manifest or
# differing from the release commit, tag moved since the API read, source
# version drift, or a release commit outside upstream main. Prints validated
# KEY=value lines (VERSION, TAG, COMMIT, BIN_X86_64_SHA, BIN_AARCH64_SHA,
# SOURCE_SHA) for the caller.
set -euo pipefail

TAG=""; JSON=""; ASSETS=""; SOURCE_TAR=""; API_COMMIT=""; ORIGIN=""; CHECKOUT=""
MAIN_REF="origin/main"
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) TAG="${2:-}"; shift 2 ;;
    --release-json) JSON="${2:-}"; shift 2 ;;
    --assets-dir) ASSETS="${2:-}"; shift 2 ;;
    --source-tarball) SOURCE_TAR="${2:-}"; shift 2 ;;
    --api-commit) API_COMMIT="${2:-}"; shift 2 ;;
    --origin-url) ORIGIN="${2:-}"; shift 2 ;;
    --checkout-dir) CHECKOUT="${2:-}"; shift 2 ;;
    --main-ref) MAIN_REF="${2:-}"; shift 2 ;;
    *) echo "verify-stable-release: unknown argument: $1" >&2; exit 1 ;;
  esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
[ -n "$TAG" ] && [ -n "$JSON" ] && [ -n "$ASSETS" ] && [ -n "$SOURCE_TAR" ] \
  && [ -n "$API_COMMIT" ] && [ -n "$ORIGIN" ] && [ -n "$CHECKOUT" ] \
  || { echo "verify-stable-release: missing required arguments" >&2; exit 1; }
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || { echo "verify-stable-release: bad tag '$TAG'" >&2; exit 1; }
[[ "$API_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
  || { echo "verify-stable-release: bad api commit" >&2; exit 1; }
VERSION="${TAG#v}"

# Public + final, or stop before anything is trusted.
"$SCRIPT_DIR/check-release-json.sh" "$TAG" "$JSON" >/dev/null

# Both per-arch tarballs checksummed against the release manifest.
OUT="$("$SCRIPT_DIR/verify-assets.sh" "$VERSION" "$ASSETS")"
BIN_X86="$(printf '%s\n' "$OUT" | sed -n 's/^BIN_X86_64_SHA=//p')"
BIN_ARM="$(printf '%s\n' "$OUT" | sed -n 's/^BIN_AARCH64_SHA=//p')"
[[ "$BIN_X86" =~ ^[0-9a-f]{64}$ ]] || { echo "verify-stable-release: bad x86_64 digest" >&2; exit 1; }
[[ "$BIN_ARM" =~ ^[0-9a-f]{64}$ ]] || { echo "verify-stable-release: bad aarch64 digest" >&2; exit 1; }

# Immutable build metadata: the release job records the exact commit it
# built inside the checksummed manifest, so binaries stay bound to the commit
# that produced them even if the tag later moves.
BUILD_FILE="$ASSETS/BUILD_COMMIT"
[ -f "$BUILD_FILE" ] || { echo "verify-stable-release: missing BUILD_COMMIT asset" >&2; exit 1; }
BUILD_COMMIT="$(tr -d '[:space:]' < "$BUILD_FILE")"
[[ "$BUILD_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
  || { echo "verify-stable-release: bad BUILD_COMMIT (want 40 hex)" >&2; exit 1; }
MANIFEST_ENTRY="$(awk -v want="BUILD_COMMIT" '$2 == want { print $1 }' "$ASSETS/SHA256SUMS")"
[ -n "$MANIFEST_ENTRY" ] \
  || { echo "verify-stable-release: SHA256SUMS has no BUILD_COMMIT entry" >&2; exit 1; }
[ "$(sha256sum -- "$BUILD_FILE" | awk '{print $1}')" = "$MANIFEST_ENTRY" ] \
  || { echo "verify-stable-release: BUILD_COMMIT digest mismatch" >&2; exit 1; }
[ "$BUILD_COMMIT" = "$API_COMMIT" ] \
  || { echo "verify-stable-release: BUILD_COMMIT $BUILD_COMMIT != release commit $API_COMMIT" >&2; exit 1; }

# The tag must still point at the release commit (moved tags fail here).
LS_OUT="$(git ls-remote "$ORIGIN" "refs/tags/${TAG}^{}" "refs/tags/${TAG}")"
PEELED="$(printf '%s\n' "$LS_OUT" | awk '$2 ~ /\^\{\}$/ {print $1}')"
TAG_COMMIT="${PEELED:-$(printf '%s\n' "$LS_OUT" | awk '$2 !~ /\^\{\}$/ {print $1}' | head -n 1)}"
[[ "$TAG_COMMIT" =~ ^[0-9a-f]{40}$ ]] \
  || { echo "verify-stable-release: cannot resolve tag $TAG at $ORIGIN" >&2; exit 1; }
[ "$TAG_COMMIT" = "$API_COMMIT" ] \
  || { echo "verify-stable-release: tag $TAG moved (upstream $TAG_COMMIT != release $API_COMMIT)" >&2; exit 1; }

# The release commit must belong to upstream main (no side-branch releases).
git -C "$CHECKOUT" merge-base --is-ancestor "$API_COMMIT" "$MAIN_REF" \
  || { echo "verify-stable-release: $API_COMMIT is not reachable from $MAIN_REF" >&2; exit 1; }

# Source archive carries the release version (beyond the renderer layout check).
[ -f "$SOURCE_TAR" ] || { echo "verify-stable-release: missing source tarball" >&2; exit 1; }
SOURCE_SHA="$(sha256sum -- "$SOURCE_TAR" | awk '{print $1}')"
[[ "$SOURCE_SHA" =~ ^[0-9a-f]{64}$ ]] || { echo "verify-stable-release: bad source digest" >&2; exit 1; }
SRC_TMP="$(mktemp -d)" || { echo "verify-stable-release: cannot mktemp" >&2; exit 1; }
trap 'rm -rf -- "$SRC_TMP"' EXIT
tar -xzf "$SOURCE_TAR" -C "$SRC_TMP"
SRC_CARGO="$SRC_TMP/linux-trackpad-driver-${VERSION}/Cargo.toml"
[ -f "$SRC_CARGO" ] || { echo "verify-stable-release: source archive lacks Cargo.toml" >&2; exit 1; }
SRC_VER="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$SRC_CARGO" | head -n 1)"
[ "$SRC_VER" = "$VERSION" ] \
  || { echo "verify-stable-release: source Cargo.toml $SRC_VER != tag $VERSION" >&2; exit 1; }

printf 'VERSION=%s\n' "$VERSION"
printf 'TAG=%s\n' "$TAG"
printf 'COMMIT=%s\n' "$API_COMMIT"
printf 'BIN_X86_64_SHA=%s\n' "$BIN_X86"
printf 'BIN_AARCH64_SHA=%s\n' "$BIN_ARM"
printf 'SOURCE_SHA=%s\n' "$SOURCE_SHA"
echo "verify-stable-release: $TAG public+final, artifacts bound to $API_COMMIT" >&2
