#!/usr/bin/env bash
# check-release-json.sh — assert a GitHub release JSON describes a PUBLIC,
# non-prerelease release for exactly TAG.
#
# Usage: check-release-json.sh vX.Y.Z RELEASE_JSON
# RELEASE_JSON comes from `gh release view TAG --json tagName,isDraft,isPrerelease`
# in CI, or from a fixture file in tests. Fails closed on draft, prerelease,
# tag mismatch, or unreadable JSON. Reads no network, touches no remotes.
set -euo pipefail

TAG="${1:?usage: check-release-json.sh vX.Y.Z RELEASE_JSON}"
JSON="${2:?usage: check-release-json.sh vX.Y.Z RELEASE_JSON}"

[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
  || { echo "check-release-json: bad tag '$TAG' (want vX.Y.Z)" >&2; exit 1; }
[ -f "$JSON" ] || { echo "check-release-json: missing file: $JSON" >&2; exit 1; }

RELEASE_TAG="$TAG" RELEASE_JSON="$JSON" python3 - <<'EOF'
import json
import os
import sys

with open(os.environ["RELEASE_JSON"], encoding="utf-8") as fh:
    try:
        release = json.load(fh)
    except json.JSONDecodeError as exc:
        print(f"check-release-json: invalid JSON: {exc}", file=sys.stderr)
        sys.exit(1)

want = os.environ["RELEASE_TAG"]
got = release.get("tagName", release.get("tag_name"))
if got != want:
    print(f"check-release-json: tag mismatch: want {want}, got {got!r}",
          file=sys.stderr)
    sys.exit(1)
if release.get("isDraft", release.get("draft", False)) is not False:
    print(f"check-release-json: release {want} is still a draft; "
          "publish it before AUR stable runs", file=sys.stderr)
    sys.exit(1)
if release.get("isPrerelease", release.get("prerelease", False)) is not False:
    print(f"check-release-json: release {want} is a prerelease; "
          "stable AUR only ships final releases", file=sys.stderr)
    sys.exit(1)
print(f"check-release-json: {want} is public and final")
EOF
