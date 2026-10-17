#!/usr/bin/env bash
# check-aur.sh — CI/local validation for AUR packaging (templates, renderer,
# offline source build). Runs unprivileged (makepkg refuses root). Exits
# nonzero on any failure. Mirrors the `aur` job in .github/workflows/ci.yml.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

for f in packaging/arch/PKGBUILD-git packaging/arch/PKGBUILD-stable.in \
         packaging/arch/PKGBUILD-bin.in packaging/arch/render-aur.sh \
         packaging/arch/ltd.install systemd/system-sleep/ltd-resume; do
  bash -n "$f"
done
if grep -nE '^[[:space:]]*systemctl (enable|start)' packaging/arch/ltd.install; then
  echo 'ltd.install must never enable/start units' >&2
  exit 1
fi

rm -rf /tmp/aur-fixtures /tmp/aur-out /tmp/aur-out2 /tmp/aur-src
mkdir -p /tmp/aur-fixtures
VER="$(grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')"
# Workspace snapshot in tag-tarball layout (checkout may be a .git-less
# archive export, so never rely on `git` here; exclude build output to keep
# the fixture hermetic and identical locally vs CI).
tar --mtime='@0' --exclude='./target' --exclude='./.git' \
  --transform "s,^\\./,linux-trackpad-driver-${VER}/," \
  -cf - . | gzip -n > "/tmp/aur-fixtures/source-${VER}.tar.gz"
cargo build --locked --release
# NOTE: both arch fixtures reuse the native binary for layout checks only;
# this never proves aarch64 build or runtime (x86_64-only CI host).
for arch in x86_64 aarch64; do
  stage="/tmp/aur-fixtures/stage/linux-trackpad-driver-v${VER}-${arch}-unknown-linux-gnu"
  mkdir -p "$stage/config" "$stage/udev" "$stage/systemd/system-sleep"
  cp target/release/ltd "$stage/ltd"
  cp config/default.toml "$stage/config/"
  cp udev/99-linux-trackpad-driver.rules "$stage/udev/"
  cp systemd/linux-trackpad-driver.service \
     systemd/linux-trackpad-driver-reconnect@.service \
     systemd/linux-trackpad-driver-resume.service "$stage/systemd/"
  cp systemd/system-sleep/ltd-resume "$stage/systemd/system-sleep/"
  chmod 755 "$stage/ltd" "$stage/systemd/system-sleep/ltd-resume"
  cp README.md INSTALL.md LICENSE-MIT LICENSE-APACHE "$stage/"
  tar -C /tmp/aur-fixtures/stage -czf \
    "/tmp/aur-fixtures/bin-${VER}-${arch}.tar.gz" \
    "linux-trackpad-driver-v${VER}-${arch}-unknown-linux-gnu"
done
S_SHA="$(sha256sum "/tmp/aur-fixtures/source-${VER}.tar.gz" | awk '{print $1}')"
X_SHA="$(sha256sum "/tmp/aur-fixtures/bin-${VER}-x86_64.tar.gz" | awk '{print $1}')"
A_SHA="$(sha256sum "/tmp/aur-fixtures/bin-${VER}-aarch64.tar.gz" | awk '{print $1}')"
packaging/arch/render-aur.sh --version "$VER" \
  --source-sha256 "$S_SHA" --bin-x86_64-sha256 "$X_SHA" \
  --bin-aarch64-sha256 "$A_SHA" --out /tmp/aur-out \
  --source-tarball "/tmp/aur-fixtures/source-${VER}.tar.gz" \
  --bin-x86_64-tarball "/tmp/aur-fixtures/bin-${VER}-x86_64.tar.gz" \
  --bin-aarch64-tarball "/tmp/aur-fixtures/bin-${VER}-aarch64.tar.gz"
packaging/arch/render-aur.sh --version "$VER" \
  --source-sha256 "$S_SHA" --bin-x86_64-sha256 "$X_SHA" \
  --bin-aarch64-sha256 "$A_SHA" --out /tmp/aur-out2
diff -r /tmp/aur-out /tmp/aur-out2
grep -q "sha256sums = $S_SHA" /tmp/aur-out/linux-trackpad-driver/.SRCINFO
grep -q "sha256sums_x86_64 = $X_SHA" /tmp/aur-out/linux-trackpad-driver-bin/.SRCINFO
grep -q "sha256sums_aarch64 = $A_SHA" /tmp/aur-out/linux-trackpad-driver-bin/.SRCINFO
grep -q 'sha256sums = SKIP' /tmp/aur-out/linux-trackpad-driver-git/.SRCINFO

# Regression: archive validation must be exact on adversarial inputs.
RDIR=/tmp/aur-regress
rm -rf "$RDIR"; mkdir -p "$RDIR/src" "$RDIR/nosvc"
# (a) valid source plus 5000 extra files must still render (pipefail/SIGPIPE
# must not turn a real match into failure on long listings).
tar -xzf "/tmp/aur-fixtures/source-${VER}.tar.gz" -C "$RDIR/src"
mkdir -p "$RDIR/src/linux-trackpad-driver-${VER}/stress"
for i in $(seq 1 5000); do echo "filler $i" > "$RDIR/src/linux-trackpad-driver-${VER}/stress/file-$i.txt"; done
tar -C "$RDIR/src" -czf "$RDIR/long-valid.tar.gz" "linux-trackpad-driver-${VER}"
LV_SHA="$(sha256sum "$RDIR/long-valid.tar.gz" | awk '{print $1}')"
packaging/arch/render-aur.sh --version "$VER" \
  --source-sha256 "$LV_SHA" --bin-x86_64-sha256 "$X_SHA" \
  --bin-aarch64-sha256 "$A_SHA" --out "$RDIR/out-long" \
  --source-tarball "$RDIR/long-valid.tar.gz" \
  --bin-x86_64-tarball "/tmp/aur-fixtures/bin-${VER}-x86_64.tar.gz" \
  --bin-aarch64-tarball "/tmp/aur-fixtures/bin-${VER}-aarch64.tar.gz"
# (b) a forbidden early ../ entry in a long listing must be rejected and must
# never materialize as output.
EROOT="$RDIR/evil-root"; mkdir -p "$EROOT"
cp -r "/tmp/aur-fixtures/stage/linux-trackpad-driver-v${VER}-x86_64-unknown-linux-gnu" "$EROOT/"
for i in $(seq 1 5000); do echo "pad $i" > "$EROOT/linux-trackpad-driver-v${VER}-x86_64-unknown-linux-gnu/pad-$i.txt"; done
# tar(1) strips leading ../ on create, so craft the evil member with
# python (verbatim name, first entry, long listing after it).
STAGE_COPY="$EROOT/linux-trackpad-driver-v${VER}-x86_64-unknown-linux-gnu" \
TOP_DIR="linux-trackpad-driver-v${VER}-x86_64-unknown-linux-gnu" \
EVIL_TAR="$RDIR/evil.tar.gz" python3 - <<'EOF'
import io, os, tarfile
stage = os.environ["STAGE_COPY"]
top = os.environ["TOP_DIR"]
with tarfile.open(os.environ["EVIL_TAR"], "w:gz") as tf:
    data = b"evil\n"
    ti = tarfile.TarInfo("../outside-archive-root")
    ti.size = len(data)
    ti.mode = 0o644
    tf.addfile(ti, io.BytesIO(data))
    di = tarfile.TarInfo(top)
    di.type = tarfile.DIRTYPE
    di.mode = 0o755
    di.mtime = 0
    tf.addfile(di)
    for root, dirs, files in os.walk(stage):
        dirs.sort()
        rel = os.path.relpath(root, stage)
        for d in dirs:
            arc = os.path.join(top, rel, d) if rel != "." else os.path.join(top, d)
            di = tarfile.TarInfo(arc)
            di.type = tarfile.DIRTYPE
            di.mode = 0o755
            di.mtime = 0
            tf.addfile(di)
        for name in sorted(files):
            path = os.path.join(root, name)
            arc = os.path.join(top, rel, name) if rel != "." else os.path.join(top, name)
            ti = tf.gettarinfo(path, arcname=arc)
            ti.mtime = 0
            with open(path, "rb") as f:
                tf.addfile(ti, f)
EOF
EV_SHA="$(sha256sum "$RDIR/evil.tar.gz" | awk '{print $1}')"
if packaging/arch/render-aur.sh --version "$VER" \
  --source-sha256 "$S_SHA" --bin-x86_64-sha256 "$EV_SHA" \
  --bin-aarch64-sha256 "$A_SHA" --out "$RDIR/out-evil" \
  --source-tarball "/tmp/aur-fixtures/source-${VER}.tar.gz" \
  --bin-x86_64-tarball "$RDIR/evil.tar.gz" \
  --bin-aarch64-tarball "/tmp/aur-fixtures/bin-${VER}-aarch64.tar.gz" 2>/dev/null; then
  echo 'renderer accepted archive with ../ entry' >&2; exit 1
fi
test ! -e "$RDIR/out-evil"
# (c) a source archive missing any installed service file must be rejected
# even when its digest is correctly supplied.
for unit in linux-trackpad-driver-reconnect@.service linux-trackpad-driver-resume.service; do
  rm -rf "$RDIR/nosvc/linux-trackpad-driver-${VER}"
  tar -xzf "/tmp/aur-fixtures/source-${VER}.tar.gz" -C "$RDIR/nosvc"
  rm "$RDIR/nosvc/linux-trackpad-driver-${VER}/systemd/$unit"
  ( cd "$RDIR/nosvc" && tar -czf "$RDIR/nosvc.tar.gz" "linux-trackpad-driver-${VER}" )
  NS_SHA="$(sha256sum "$RDIR/nosvc.tar.gz" | awk '{print $1}')"
  if packaging/arch/render-aur.sh --version "$VER" \
    --source-sha256 "$NS_SHA" --bin-x86_64-sha256 "$X_SHA" \
    --bin-aarch64-sha256 "$A_SHA" --out "$RDIR/out-nosvc" \
    --source-tarball "$RDIR/nosvc.tar.gz" \
    --bin-x86_64-tarball "/tmp/aur-fixtures/bin-${VER}-x86_64.tar.gz" \
    --bin-aarch64-tarball "/tmp/aur-fixtures/bin-${VER}-aarch64.tar.gz" 2>/dev/null; then
    echo "renderer accepted source missing systemd/$unit" >&2; exit 1
  fi
done
# Regression: a render that fails mid-way must preserve prior output
# (staged swap, not rm-then-write).
mkdir -p "$RDIR/fakebin" "$RDIR/keep/linux-trackpad-driver"
printf '#!/bin/sh\nexit 1\n' > "$RDIR/fakebin/makepkg"
chmod +x "$RDIR/fakebin/makepkg"
echo 'sentinel-prior-output' > "$RDIR/keep/linux-trackpad-driver/PKGBUILD"
if PATH="$RDIR/fakebin:$PATH" packaging/arch/render-aur.sh --version "$VER" \
  --source-sha256 "$S_SHA" --bin-x86_64-sha256 "$X_SHA" \
  --bin-aarch64-sha256 "$A_SHA" --out "$RDIR/keep" \
  --source-tarball "/tmp/aur-fixtures/source-${VER}.tar.gz" \
  --bin-x86_64-tarball "/tmp/aur-fixtures/bin-${VER}-x86_64.tar.gz" \
  --bin-aarch64-tarball "/tmp/aur-fixtures/bin-${VER}-aarch64.tar.gz" 2>/dev/null; then
  echo 'renderer succeeded with failing makepkg' >&2; exit 1
fi
grep -q sentinel-prior-output "$RDIR/keep/linux-trackpad-driver/PKGBUILD" \
  || { echo 'failed render destroyed prior output' >&2; exit 1; }
test ! -e "$RDIR/keep/linux-trackpad-driver/.SRCINFO"

rm -rf /tmp/aur-src
mkdir -p /tmp/aur-src
tar -xzf "/tmp/aur-fixtures/source-${VER}.tar.gz" -C /tmp/aur-src
cd "/tmp/aur-src/linux-trackpad-driver-${VER}"
export RUSTUP_TOOLCHAIN=stable
cargo fetch --locked --target "$(rustc -vV | sed -n 's|host: ||p')"
export CARGO_NET_OFFLINE=true CARGO_TARGET_DIR=/tmp/aur-src/target
cargo build --frozen --release --all-features
cargo test --frozen --all-features
echo 'check-aur: OK'
