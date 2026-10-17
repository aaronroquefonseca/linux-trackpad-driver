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
git archive --format=tar --prefix="linux-trackpad-driver-${VER}/" HEAD \
  | gzip -n > "/tmp/aur-fixtures/source-${VER}.tar.gz"
cargo build --locked --release
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
