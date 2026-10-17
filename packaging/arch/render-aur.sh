#!/usr/bin/env bash
# render-aur.sh — render publishable AUR PKGBUILD/.SRCINFO for a release.
#
# Inputs (all required): --version X.Y.Z, --source-sha256, --bin-x86_64-sha256,
# --bin-aarch64-sha256, --out DIR. Optional --*-tarball FILE verifies the
# digest against a downloaded artifact and checks its interior layout.
# Outputs: $OUT/{linux-trackpad-driver,linux-trackpad-driver-bin,
# linux-trackpad-driver-git}/{PKGBUILD,.SRCINFO,ltd.install}, staged in a
# temp dir and swapped in only after all three variants succeed.
# Fails closed on bad version/digest, missing inputs, leftover placeholders,
# or unexpected tarball contents; a failure during generation/metadata
# leaves prior output untouched (final install I/O is not transactional).
# Deterministic: same inputs -> same bytes
# (no timestamps; .SRCINFO via `makepkg --printsrcinfo`).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VERSION=""; SOURCE_SHA=""; BIN_X86_SHA=""; BIN_ARM_SHA=""; OUT=""
SOURCE_TARBALL=""; BIN_X86_TARBALL=""; BIN_ARM_TARBALL=""

usage() {
  cat <<'EOF'
Usage: render-aur.sh --version X.Y.Z --source-sha256 HEX \
  --bin-x86_64-sha256 HEX --bin-aarch64-sha256 HEX --out DIR \
  [--source-tarball FILE] [--bin-x86_64-tarball FILE] [--bin-aarch64-tarball FILE]
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION="${2:-}"; shift 2 ;;
    --source-sha256) SOURCE_SHA="${2:-}"; shift 2 ;;
    --bin-x86_64-sha256) BIN_X86_SHA="${2:-}"; shift 2 ;;
    --bin-aarch64-sha256) BIN_ARM_SHA="${2:-}"; shift 2 ;;
    --out) OUT="${2:-}"; shift 2 ;;
    --source-tarball) SOURCE_TARBALL="${2:-}"; shift 2 ;;
    --bin-x86_64-tarball) BIN_X86_TARBALL="${2:-}"; shift 2 ;;
    --bin-aarch64-tarball) BIN_ARM_TARBALL="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "render-aur: unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() { echo "render-aur: error: $1" >&2; exit 1; }

[ -n "$VERSION" ] || fail "missing --version"
[ -n "$SOURCE_SHA" ] || fail "missing --source-sha256"
[ -n "$BIN_X86_SHA" ] || fail "missing --bin-x86_64-sha256"
[ -n "$BIN_ARM_SHA" ] || fail "missing --bin-aarch64-sha256"
[ -n "$OUT" ] || fail "missing --out"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "bad --version '$VERSION' (want X.Y.Z)"
for pair in "source:$SOURCE_SHA" "bin-x86_64:$BIN_X86_SHA" "bin-aarch64:$BIN_ARM_SHA"; do
  [[ "${pair#*:}" =~ ^[0-9a-f]{64}$ ]] || fail "bad ${pair%%:*} sha256 (want 64 lowercase hex, never SKIP)"
done

# Scratch dir for tarball listings; removed on any exit.
WORK_DIR="$(mktemp -d)" || fail "cannot create temp working dir"
trap 'rm -rf -- "$WORK_DIR"' EXIT
LIST_FILE="$WORK_DIR/listing.txt"

check_tarball() { # kind, file, want_sha, arch_or_empty
  local kind="$1" file="$2" want="$3" marker="$4"
  [ -f "$file" ] || fail "missing $kind tarball: $file"
  local got
  got="$(sha256sum -- "$file" | awk '{print $1}')"
  [ "$got" = "$want" ] || fail "$kind digest mismatch for $file"
  tar -tzf "$file" > "$LIST_FILE" || fail "$kind is not a readable gzip tarball: $file"
  # NOTE: grep reads the saved listing file, never a pipe. Under `pipefail`,
  # `echo "$list" | grep -q` can report failure AFTER a real match: grep
  # exits on the first hit while the producer still holds pending lines, the
  # producer dies on SIGPIPE, and pipefail propagates that as failure (or a
  # forbidden early entry can hide behind exit-zero). File input has no
  # producer, so matches are exact.
  grep -Eq '(^|/)\.\.(/|$)|^/' "$LIST_FILE" && fail "$kind contains absolute or parent paths"
  if [ "$kind" = "source" ]; then
    local prefix="linux-trackpad-driver-${VERSION}/"
    for member in "Cargo.toml" "Cargo.lock" "src/main.rs" "config/default.toml" \
        "udev/99-linux-trackpad-driver.rules" "systemd/linux-trackpad-driver.service" \
        "systemd/linux-trackpad-driver-reconnect@.service" \
        "systemd/linux-trackpad-driver-resume.service" \
        "systemd/system-sleep/ltd-resume" "LICENSE-MIT" "LICENSE-APACHE"; do
      grep -qxF "${prefix}${member}" "$LIST_FILE" || fail "source tarball missing ${prefix}${member}"
    done
  else
    local prefix="linux-trackpad-driver-v${VERSION}-${marker}-unknown-linux-gnu/"
    for member in "ltd" "config/default.toml" "udev/99-linux-trackpad-driver.rules" \
        "systemd/linux-trackpad-driver.service" "systemd/linux-trackpad-driver-reconnect@.service" \
        "systemd/linux-trackpad-driver-resume.service" "systemd/system-sleep/ltd-resume" \
        "LICENSE-MIT" "LICENSE-APACHE" "README.md" "INSTALL.md"; do
      grep -qxF "${prefix}${member}" "$LIST_FILE" || fail "bin tarball ($marker) missing ${prefix}${member}"
    done
  fi
}

[ -n "$SOURCE_TARBALL" ] && check_tarball source "$SOURCE_TARBALL" "$SOURCE_SHA" ""
[ -n "$BIN_X86_TARBALL" ] && check_tarball bin "$BIN_X86_TARBALL" "$BIN_X86_SHA" "x86_64"
[ -n "$BIN_ARM_TARBALL" ] && check_tarball bin "$BIN_ARM_TARBALL" "$BIN_ARM_SHA" "aarch64"

command -v makepkg >/dev/null 2>&1 || fail "makepkg not found"

# Stage the whole tree, then swap it in only after all three variants
# render: a failure during generation/metadata leaves any prior output
# untouched. Once generation succeeds the staged tree replaces the output;
# final install I/O errors (rm/mv) are not transactional.
STAGE="$WORK_DIR/out"
mkdir -p -- "$STAGE/linux-trackpad-driver" "$STAGE/linux-trackpad-driver-bin" "$STAGE/linux-trackpad-driver-git"

sed -e "s/@PKGVER@/${VERSION}/g" -e "s/@SOURCE_SHA256@/${SOURCE_SHA}/g" \
  "$SCRIPT_DIR/PKGBUILD-stable.in" > "$STAGE/linux-trackpad-driver/PKGBUILD"
sed -e "s/@PKGVER@/${VERSION}/g" -e "s/@SHA256_X86_64@/${BIN_X86_SHA}/g" \
  -e "s/@SHA256_AARCH64@/${BIN_ARM_SHA}/g" \
  "$SCRIPT_DIR/PKGBUILD-bin.in" > "$STAGE/linux-trackpad-driver-bin/PKGBUILD"
cp -- "$SCRIPT_DIR/PKGBUILD-git" "$STAGE/linux-trackpad-driver-git/PKGBUILD"
for d in linux-trackpad-driver linux-trackpad-driver-bin linux-trackpad-driver-git; do
  cp -- "$SCRIPT_DIR/ltd.install" "$STAGE/$d/ltd.install"
done

if grep -RnE '@(PKGVER|SOURCE_SHA256|SHA256_X86_64|SHA256_AARCH64)@' \
    "$STAGE/linux-trackpad-driver/PKGBUILD" "$STAGE/linux-trackpad-driver-bin/PKGBUILD" >/dev/null; then
  fail "placeholder leaked into rendered stable/bin PKGBUILD"
fi
if grep -n "^sha256sums.*SKIP" \
    "$STAGE/linux-trackpad-driver/PKGBUILD" "$STAGE/linux-trackpad-driver-bin/PKGBUILD" >/dev/null; then
  fail "SKIP leaked into rendered stable/bin sha256sums (immutable archives need real digests)"
fi

for d in linux-trackpad-driver linux-trackpad-driver-bin linux-trackpad-driver-git; do
  ( cd "$STAGE/$d" && makepkg --printsrcinfo > .SRCINFO )
done

rm -rf -- "$OUT"
mkdir -p -- "$(dirname -- "$OUT")"
mv -- "$STAGE" "$OUT"

echo "render-aur: wrote $OUT/{linux-trackpad-driver,linux-trackpad-driver-bin,linux-trackpad-driver-git}/"
