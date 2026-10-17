#!/usr/bin/env bash
# gate-aur-git.sh — decide whether the -git snapshot may publish.
#
# Env in: EVENT_NAME (push|workflow_dispatch), GITHUB_REF (dispatched ref),
# INPUT_PUBLISH (dispatch publish flag), AUR_GIT_VAR (ENABLE_AUR_GIT value),
# SNAPSHOT (40-hex revision being snapshotted).
# Prints short=/snapshot=/publish= lines for $GITHUB_OUTPUT.
#
# Manual retries must target main (--ref main): any other dispatched ref is
# denied outright, so unmerged branches never flow through packaging while
# publication credentials exist. Push events are inherently main pushes
# (branch protection) and carry no ref choice.
set -euo pipefail

EVENT_NAME="${EVENT_NAME:?missing EVENT_NAME}"
GITHUB_REF="${GITHUB_REF:?missing GITHUB_REF}"
SNAPSHOT="${SNAPSHOT:?missing SNAPSHOT}"
AUR_GIT_VAR="${AUR_GIT_VAR:-}"
INPUT_PUBLISH="${INPUT_PUBLISH:-false}"

[[ "$SNAPSHOT" =~ ^[0-9a-f]{40}$ ]] \
  || { echo "gate-aur-git: bad snapshot sha" >&2; exit 1; }
if [ "$EVENT_NAME" = "workflow_dispatch" ] && [ "$GITHUB_REF" != "refs/heads/main" ]; then
  echo "gate-aur-git: manual retries must target main (--ref main); got $GITHUB_REF" >&2
  exit 1
fi

want="false"
if [ "$EVENT_NAME" = "push" ] || [ "$INPUT_PUBLISH" = "true" ]; then
  want="true"
fi
publish="false"
if [ "$want" = "true" ] && [ "$AUR_GIT_VAR" = "true" ]; then
  publish="true"
fi

printf 'short=%s\n' "${SNAPSHOT:0:7}"
printf 'snapshot=%s\n' "$SNAPSHOT"
printf 'publish=%s\n' "$publish"
echo "gate-aur-git: snapshot=$SNAPSHOT publish=$publish (want=$want)" >&2
