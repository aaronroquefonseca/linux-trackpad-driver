# AUR packaging — maintainer contract

Three AUR packages, three repos. Nothing in this directory is published
directly; the release workflow renders publishable trees via `render-aur.sh`.

| AUR repo | Source | Build |
|---|---|---|
| `linux-trackpad-driver` | Tag tarball `.../archive/refs/tags/v$pkgver.tar.gz` | `cargo build --frozen` (this dir) |
| `linux-trackpad-driver-bin` | Per-arch release tarballs (see below) | None (prebuilt `ltd`) |
| `linux-trackpad-driver-git` | `git+https://...` main (SKIP checksum) | `cargo build --frozen` |

## Files

- `PKGBUILD-stable.in` — template, placeholders `@PKGVER@ @SOURCE_SHA256@`.
  Never publish as-is (no `SKIP` for immutable archives, ever).
- `PKGBUILD-bin.in` — template, placeholders
  `@PKGVER@ @SHA256_X86_64@ @SHA256_AARCH64@`, arch-specific
  `source_$CARCH`/`sha256sums_$CARCH`. Never publish as-is.
- `PKGBUILD-git` — final as-is. Copy to `PKGBUILD` in the `-git` AUR repo.
- `ltd.install` — shared by all three. Reloads udev/systemd, prints help.
  Never enables or starts the daemon (same policy as `debian/`).
- `render-aur.sh` — deterministic renderer, no deps beyond `bash`, `makepkg`.

## Renderer interface (for the release-workflow PR)

```sh
packaging/arch/render-aur.sh --version 0.1.0 \
  --source-sha256 <64hex> --bin-x86_64-sha256 <64hex> \
  --bin-aarch64-sha256 <64hex> --out dist/aur \
  [--source-tarball FILE] [--bin-x86_64-tarball FILE] \
  [--bin-aarch64-tarball FILE]
```

- Fails closed: bad `X.Y.Z`, non-hex/`SKIP` digests, missing inputs,
  digest mismatch, unexpected tarball contents, leftover placeholders,
  missing `makepkg`. Renders into a temp staging dir and swaps it over the
  output dir (creating absent parents) only after all three variants
  succeed, so a failure during generation/metadata leaves prior output
  untouched. Once generation succeeds the staged tree replaces the output;
  final install I/O errors are not transactional.
- Output: `dist/aur/{linux-trackpad-driver,linux-trackpad-driver-bin,
  linux-trackpad-driver-git}/{PKGBUILD,.SRCINFO,ltd.install}`,
  `.SRCINFO` via `makepkg --printsrcinfo`. Deterministic (re-run diffs clean).
- `--*-tarball` (optional but recommended in CI): verifies the digest and
  checks interior layout, see below.

## Release artifact contract (`release-tag.yml`)

- Tag `vX.Y.Z` (signed). Stable source URL needs no upload: the GitHub
  tag tarball extracts to `linux-trackpad-driver-X.Y.Z/` with `Cargo.lock`
  and both `LICENSE-*` at root.
- Binary assets (exact names):
  `linux-trackpad-driver-vX.Y.Z-{x86_64,aarch64}-unknown-linux-gnu.tar.gz`,
  top dir of the same name, containing: `ltd` (0755),
  `config/default.toml`, `udev/99-linux-trackpad-driver.rules`,
  `systemd/*.service` (3 units), `systemd/system-sleep/ltd-resume` (0755),
  `README.md`, `INSTALL.md`, **`LICENSE-MIT`, `LICENSE-APACHE`**.
  The release job stages both licenses (bin `package()` installs them and
  fixtures prove it).
- No absolute paths or `..` in any tarball; no root-owned surprises
  (release job builds as runner user; `package()` sets modes explicitly).

## Install manifest (all variants, same set)

`/usr/bin/ltd` 0755 · `/etc/linux-trackpad-driver/config.toml` 0644
(`backup=`) · `/usr/lib/udev/rules.d/…` 0644 (never `/etc/udev/rules.d`) ·
`/usr/lib/systemd/system/…` 0644 (3 units, none enabled by install) ·
`/usr/lib/systemd/system-sleep/ltd-resume` 0755 ·
`/usr/share/licenses/$pkgname/{LICENSE-MIT,LICENSE-APACHE}` 0644.
Runtime deps `gcc-libs glibc systemd` (verified via `ldd`: libgcc_s, libc,
libm); `makedepends=cargo` (+ `git` for `-git`); mutual
`conflicts` across all three, `provides=(linux-trackpad-driver)` on `-bin`
and `-git`. Config behavior untouched.

## Embedded binary identity

`-git` builds embed the `development` channel, the checkout commit (12 digits),
and `dirty` for local changes. Stable source builds embed `stable` and may use
`commit unknown` because tag archives lack Git history. A verified
`LTD_BUILD_COMMIT` can supply the source commit without Git. The `-bin` package
preserves the release binary's identity. This does not alter `pkgver()` or the
Cargo semantic version. See [the metadata contract](../../docs/RELEASING.md#embedded-build-identity).

## VCS versioning

`pkgver()` tries `git describe --long --tags` (strips `v`), else falls back
to `r<rev-list-count>.<short-hash>`. Pre-tag builds look like `r211.6ebc164`
and sort below the first tag build `0.1.0.r0.g…` (Arch `r`-delimiter rule),
so the version never goes backward at the first release. Static `pkgver` in
`PKGBUILD-git` is refreshed to HEAD at release-prep time only.

## Validation (this PR)

- `bash -n` on all PKGBUILDs, `ltd.install`, `render-aur.sh`.
- `makepkg --printsrcinfo` for all three variants (CI `aur` job).
- Fixture roundtrip: `git archive` source tarball → `cargo fetch --locked`
  → `cargo build/test --frozen` with `CARGO_NET_OFFLINE=true` (no network
  in `build()` after `prepare()`); bin fixtures in release layout with
  modes/ownership listing; renderer determinism (`diff -r` clean);
  fail-closed probes (bad version, `SKIP`, missing input).
- Limits (explicit): no `v0.1.0` tag exists yet, so fixture SHAs stand in
  for release digests; the aarch64 fixture reuses the x86_64 binary for
  layout checks only; GitHub tag-tarball bytes (and hence its SHA) only
  exist after the tag push.

## Push (release workflows, not this directory)

Publishing is done by automation once armed (see `docs/RELEASING.md`);
nothing here is pushed by hand. Per variant repo, the flow copies the
rendered `PKGBUILD` + `.SRCINFO` + `ltd.install`, commits on `master`, and
pushes append-only (never force). The first authenticated push from the
registered AUR key creates each package — no separate manual bootstrap is
needed. Shared helper: `packaging/release/publish-aur.sh` (pinned AUR host
key, temp key file, noop/downgrade guards, unrelated files preserved).

- Stable + bin: `.github/workflows/aur-stable.yml` runs on
  `release.published` (or a trusted manual retry that re-verifies the
  release is public): verify → render via `render-aur.sh` → push
  `linux-trackpad-driver` + `linux-trackpad-driver-bin`.
- VCS: `.github/workflows/aur-git.yml` runs on packaging-relevant `main`
  pushes (or manual retry): regenerate `.SRCINFO` via
  `makepkg --printsrcinfo`, push `linux-trackpad-driver-git`.
