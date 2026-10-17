#!/usr/bin/env bash
# publish-aur.sh — push one rendered AUR tree to its AUR repo.
#
# Usage: publish-aur.sh --pkg PKG --dir RENDERED_DIR --message MSG
#          [--expect-pkgver X.Y.Z] [--remote URL]
#
# RENDERED_DIR holds PKGBUILD + .SRCINFO + ltd.install (render-aur.sh output
# for stable/bin, PKGBUILD-git copy for -git). Only those three managed files
# are ever added; unrelated remote files are preserved. Never force-pushes.
#
# Guards: --expect-pkgver (stable) refuses to push when the remote pkgver is
# newer (stale release event) and reports a noop when rendered output matches
# the remote. First push to an empty/missing remote initializes master.
#
# Auth: ssh:// remotes use AUR_SSH_PRIVATE_KEY from the environment (CI job
# secret), written to a temp file (mode 600) with a pinned known_hosts entry
# and StrictHostKeyChecking; the key is scrubbed on exit and tracing is
# disabled while it is handled. Local-path remotes (tests) need no key.
set -euo pipefail

PKG=""; DIR=""; MESSAGE=""; EXPECT=""; REMOTE=""
while [ $# -gt 0 ]; do
  case "$1" in
    --pkg) PKG="${2:-}"; shift 2 ;;
    --dir) DIR="${2:-}"; shift 2 ;;
    --message) MESSAGE="${2:-}"; shift 2 ;;
    --expect-pkgver) EXPECT="${2:-}"; shift 2 ;;
    --remote) REMOTE="${2:-}"; shift 2 ;;
    *) echo "publish-aur: unknown argument: $1" >&2; exit 2 ;;
  esac
done

[ -n "$PKG" ] || { echo "publish-aur: missing --pkg" >&2; exit 1; }
[ -n "$DIR" ] || { echo "publish-aur: missing --dir" >&2; exit 1; }
[ -n "$MESSAGE" ] || { echo "publish-aur: missing --message" >&2; exit 1; }
REMOTE="${REMOTE:-ssh://aur@aur.archlinux.org/${PKG}.git}"
if [ -n "$EXPECT" ]; then
  [[ "$EXPECT" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || { echo "publish-aur: bad --expect-pkgver '$EXPECT'" >&2; exit 1; }
fi
for managed in PKGBUILD .SRCINFO ltd.install; do
  [ -f "$DIR/$managed" ] || { echo "publish-aur: missing $DIR/$managed" >&2; exit 1; }
done
# Resolve before any cd: callers may pass a relative dir and every copy
# below runs inside the temporary clone.
DIR="$(cd -- "$DIR" && pwd)" || { echo "publish-aur: cannot resolve --dir" >&2; exit 1; }

WORK_DIR="$(mktemp -d)" || { echo "publish-aur: cannot mktemp" >&2; exit 1; }
cleanup() { rm -rf -- "$WORK_DIR"; }
trap cleanup EXIT

GIT_SSH_OPTS=()
if [[ "$REMOTE" == ssh://* ]]; then
  command -v ssh >/dev/null \
    || { echo "publish-aur: ssh client not found (install openssh)" >&2; exit 1; }
  KEY="${AUR_SSH_PRIVATE_KEY:-}"
  [ -n "$KEY" ] || { echo "publish-aur: AUR_SSH_PRIVATE_KEY is unset" >&2; exit 1; }
  case $- in
    *x*) set +x; echo "publish-aur: disabled shell tracing while handling the key" >&2 ;;
  esac
  ( umask 077; printf '%s\n' "$KEY" > "$WORK_DIR/key" )
  KNOWN="${AUR_KNOWN_HOSTS:-aur.archlinux.org ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIEuBKrPzbawxA/k2g6NcyV5jmqwJ2s+zpgZGZ7tpLIcN}"
  printf '%s\n' "$KNOWN" > "$WORK_DIR/known_hosts"
  grep -q '^aur\.archlinux\.org ' "$WORK_DIR/known_hosts" \
    || { echo "publish-aur: known_hosts lacks the aur.archlinux.org entry" >&2; exit 1; }
  cat > "$WORK_DIR/ssh_config" <<EOF
Host aur.archlinux.org
  HostName aur.archlinux.org
  User aur
  IdentityFile $WORK_DIR/key
  IdentitiesOnly yes
  StrictHostKeyChecking yes
  UserKnownHostsFile $WORK_DIR/known_hosts
EOF
  chmod 600 "$WORK_DIR/ssh_config"
  GIT_SSH_OPTS=(-c "core.sshCommand=ssh -F $WORK_DIR/ssh_config")
fi

CLONE_ERR="$WORK_DIR/clone.err"
if git "${GIT_SSH_OPTS[@]}" clone -- "$REMOTE" "$WORK_DIR/repo" 2>"$CLONE_ERR"; then
  :
elif grep -qiE 'not found|does not exist|could not read from remote|repository .* empty|empty repository|access denied|permission denied' "$CLONE_ERR"; then
  # New AUR package (or empty repo): the first authenticated push creates it.
  echo "publish-aur: remote not clonable ($(head -n 1 "$CLONE_ERR")); initializing new repo" >&2
  mkdir -p "$WORK_DIR/repo"
  ( cd "$WORK_DIR/repo" && git init -q -b master && git remote add origin "$REMOTE" )
else
  cat "$CLONE_ERR" >&2
  echo "publish-aur: clone failed" >&2
  exit 1
fi

cd "$WORK_DIR/repo"
git checkout -q -B master 2>/dev/null || true

if [ -n "$EXPECT" ] && git rev-parse -q --verify HEAD >/dev/null; then
  REMOTE_VER="$(git show HEAD:PKGBUILD 2>/dev/null | sed -n 's/^pkgver=//p' | head -n 1)"
  if [ -n "$REMOTE_VER" ] && [ "$REMOTE_VER" != "$EXPECT" ]; then
    NEWEST="$(printf '%s\n%s\n' "$REMOTE_VER" "$EXPECT" | sort -V | tail -n 1)"
    if [ "$NEWEST" != "$EXPECT" ]; then
      echo "publish-aur: remote pkgver $REMOTE_VER is newer than $EXPECT; refusing downgrade" >&2
      exit 1
    fi
  fi
fi

cp -- "$DIR/PKGBUILD" "$DIR/.SRCINFO" "$DIR/ltd.install" ./
git add -- PKGBUILD .SRCINFO ltd.install
if [ -z "$(git status --porcelain)" ]; then
  echo "publish-aur: $PKG already up to date (noop)"
  exit 0
fi

git -c user.name="github-actions[bot]" \
    -c user.email="41898282+github-actions[bot]@users.noreply.github.com" \
    commit -q -m "$MESSAGE"
# Never --force: AUR master is append-only from automation.
git "${GIT_SSH_OPTS[@]}" push origin master
echo "publish-aur: pushed $PKG"
