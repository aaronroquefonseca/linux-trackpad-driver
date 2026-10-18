#!/usr/bin/env bash
# manage-draft-release.sh — create or refresh the draft GitHub Release for a
# tag, never publishing it.
#
# Usage: manage-draft-release.sh --tag vX.Y.Z --commit SHA --dist-dir DIR
# Env: GH_REPO (owner/repo, required — fresh jobs have no checkout, so `gh`
# cannot infer the repository), GH_TOKEN via `gh` auth as usual.
#
# DIR holds the release files: both per-arch tarballs, CHANGELOG-<TAG>.md,
# at least one .deb, at least one .rpm, and SHA256SUMS. Rerun-safe: refreshing an existing draft
# re-uploads with --clobber; an existing PUBLISHED (or prerelease) release
# fails closed before anything is mutated. Verifies the release is still a
# draft afterwards.
set -euo pipefail

TAG=""; COMMIT=""; DIST=""
while [ $# -gt 0 ]; do
  case "$1" in
    --tag) TAG="${2:-}"; shift 2 ;;
    --commit) COMMIT="${2:-}"; shift 2 ;;
    --dist-dir) DIST="${2:-}"; shift 2 ;;
    *) echo "manage-draft-release: unknown argument: $1" >&2; exit 1 ;;
  esac
done

[ -n "$TAG" ] || { echo "manage-draft-release: missing --tag" >&2; exit 1; }
[ -n "$COMMIT" ] || { echo "manage-draft-release: missing --commit" >&2; exit 1; }
[ -n "$DIST" ] || { echo "manage-draft-release: missing --dist-dir" >&2; exit 1; }
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || { echo "manage-draft-release: bad tag '$TAG' (want vX.Y.Z)" >&2; exit 1; }
[[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]] \
  || { echo "manage-draft-release: bad commit (want 40 hex)" >&2; exit 1; }
GH_REPO="${GH_REPO:-}"
[ -n "$GH_REPO" ] \
  || { echo "manage-draft-release: GH_REPO (owner/repo) is required" >&2; exit 1; }
[ -d "$DIST" ] || { echo "manage-draft-release: missing dir: $DIST" >&2; exit 1; }

NOTES="$DIST/CHANGELOG-${TAG}.md"
SUMS="$DIST/SHA256SUMS"
files=(
  "$DIST/linux-trackpad-driver-${TAG}-x86_64-unknown-linux-gnu.tar.gz"
  "$DIST/linux-trackpad-driver-${TAG}-aarch64-unknown-linux-gnu.tar.gz"
  "$NOTES"
  "$SUMS"
  "$DIST/BUILD_COMMIT"
)
for deb in "$DIST"/*.deb; do
  files+=("$deb")
done
for rpm in "$DIST"/*.rpm; do
  files+=("$rpm")
done
for f in "${files[@]}"; do
  [ -f "$f" ] || { echo "manage-draft-release: missing release file: $f" >&2; exit 1; }
done
[ -f "$NOTES" ] || { echo "manage-draft-release: missing notes: $NOTES" >&2; exit 1; }

if gh release view "$TAG" --repo "$GH_REPO" \
    --json isDraft --jq .isDraft >/dev/null; then
  draft="$(gh release view "$TAG" --repo "$GH_REPO" --json isDraft --jq .isDraft)"
  prerelease="$(gh release view "$TAG" --repo "$GH_REPO" --json isPrerelease --jq .isPrerelease)"
  if [ "$prerelease" = "true" ]; then
    echo "manage-draft-release: release $TAG is marked prerelease; stable pipeline ships finals only" >&2
    exit 1
  fi
  if [ "$draft" != "true" ]; then
    # Rerun guard: a published release is immutable to automation.
    echo "manage-draft-release: release $TAG is already published; refusing to mutate public assets" >&2
    exit 1
  fi
  echo "manage-draft-release: refreshing existing draft $TAG"
  gh release upload "$TAG" --repo "$GH_REPO" --clobber "${files[@]}"
  gh release edit "$TAG" --repo "$GH_REPO" --title "$TAG" --notes-file "$NOTES"
else
  echo "manage-draft-release: creating draft $TAG at $COMMIT"
  gh release create "$TAG" --repo "$GH_REPO" "${files[@]}" \
    --target "$COMMIT" \
    --title "$TAG" --notes-file "$NOTES" \
    --draft
fi
is_draft="$(gh release view "$TAG" --repo "$GH_REPO" --json isDraft --jq .isDraft)"
[ "$is_draft" = "true" ] \
  || { echo "manage-draft-release: release $TAG did not stay a draft" >&2; exit 1; }
echo "manage-draft-release: draft $TAG ready for human review; nothing published"
