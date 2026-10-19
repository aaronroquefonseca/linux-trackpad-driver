#!/usr/bin/env bash
# check-release-json.sh — assert a GitHub release JSON describes a PUBLIC,
# non-prerelease release for exactly TAG.
#
# Usage: check-release-json.sh vX.Y.Z RELEASE_JSON
# RELEASE_JSON comes from `gh release view TAG --json tagName,isDraft,isPrerelease`
# in CI, or from a fixture file in tests. Fails closed on absent, null,
# conflicting, or malformed draft/prerelease metadata, tag mismatch,
# non-object JSON, or unreadable JSON. Reads no network, touches no remotes.
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


def fail(msg):
    print(f"check-release-json: {msg}", file=sys.stderr)
    sys.exit(1)


with open(os.environ["RELEASE_JSON"], encoding="utf-8") as fh:
    try:
        release = json.load(fh)
    except json.JSONDecodeError as exc:
        fail(f"invalid JSON: {exc}")

want = os.environ["RELEASE_TAG"]
if not isinstance(release, dict):
    fail(f"malformed release metadata: want JSON object, got {type(release).__name__}")

# Tag: accept the `gh release view` camelCase shape or the REST snake_case
# shape, but require an explicit match; conflicting aliases fail closed.
has_tag = "tagName" in release
has_tag_snake = "tag_name" in release
if not has_tag and not has_tag_snake:
    fail(f"tag mismatch: want {want}, got missing tagName/tag_name")
got = release.get("tagName", release.get("tag_name"))
if has_tag and has_tag_snake and release["tagName"] != release["tag_name"]:
    fail(f"conflicting tag fields: tagName={release['tagName']!r} "
         f"!= tag_name={release['tag_name']!r}")
if got != want:
    fail(f"tag mismatch: want {want}, got {got!r}")


def require_final(*names, label, still_msg):
    present = [name for name in names if name in release]
    if not present:
        fail(f"{label} metadata absent (want explicit {'/'.join(names)}: false); "
             f"refusing to assume {want} is public and final)")
    if len(present) == 2:
        first, second = release[present[0]], release[present[1]]
        if first != second:
            fail(f"conflicting {label} metadata: {present[0]}={first!r} "
                 f"!= {present[1]}={second!r}")
    for name in present:
        value = release[name]
        if value is True:
            fail(f"release {want} is {still_msg}")
        if value is not False:
            fail(f"malformed {label} metadata: {name} must be boolean false, "
                 f"got {value!r}")


require_final("isDraft", "draft", label="draft",
              still_msg="still a draft; publish it before AUR stable runs")
require_final("isPrerelease", "prerelease", label="prerelease",
              still_msg="a prerelease; stable AUR only ships final releases")
print(f"check-release-json: {want} is public and final")
EOF
