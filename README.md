# linux-trackpad-driver

Userspace Rust driver for the Apple Magic Trackpad 2 (Lightning + USB-C) on Linux.
Device setup always runs (USB feature reports, Bluetooth bring-up, reconnect,
suspend re-init). The pressure click engine (clicks, haptics, force levels,
touch actions) is on by default and configured in one file.

Currently, only Apple Magic Trackpad 2 support is implemented. Support for
other trackpads is planned.

## Features

- Configurable USB polling rate (up to ~110 Hz).
- Adjustable click sensitivity and haptic feedback.
- Two pressure levels (normal and Force Touch) with independent actions.
- Configurable 1–5 finger clicks and keyboard shortcuts.
- Touch-without-press actions (slide mode).
- Finger-count remapping for gesture support.
- Adjustable pointer speed.
- Automatic reconnection and suspend/resume handling.
- Fully userspace: no custom kernel modules, no DKMS.

## Install

### Arch Linux (AUR)

Recommended for most users (fast, stable, prebuilt):

```sh
yay -S linux-trackpad-driver-bin
```

Pick exactly one — the three variants conflict and replace one another:

- `linux-trackpad-driver` — stable source build via Cargo.
- `linux-trackpad-driver-bin` — stable prebuilt `x86_64`/`aarch64` (recommended).
- `linux-trackpad-driver-git` — latest `main` snapshot, compiled at install.

```sh
yay -S linux-trackpad-driver      # source build
yay -S linux-trackpad-driver-bin  # prebuilt (recommended)
yay -S linux-trackpad-driver-git  # main snapshot
```

Package pages: [`linux-trackpad-driver`](https://aur.archlinux.org/packages/linux-trackpad-driver),
[`linux-trackpad-driver-bin`](https://aur.archlinux.org/packages/linux-trackpad-driver-bin),
[`linux-trackpad-driver-git`](https://aur.archlinux.org/packages/linux-trackpad-driver-git).
Without an AUR helper, clone the package repo and run `makepkg -si`
([AUR usage](https://wiki.archlinux.org/title/Arch_User_Repository#Installing_and_upgrading_packages)).

The package ships the default config, udev rules, and systemd units. The
config at `/etc/linux-trackpad-driver/config.toml` is preserved on
upgrade — edit that copy. Units ship disabled by design; opt in explicitly:

```sh
sudo systemctl enable --now linux-trackpad-driver.service
```

Then verify with `ltd doctor` (see Troubleshooting limits below).

### Debian / Ubuntu (`.deb`)

`.deb` files (`x86_64`) are attached to each
[GitHub Release](https://github.com/aaronroquefonseca/linux-trackpad-driver/releases):

```sh
sudo apt install ./linux-trackpad-driver_*_amd64.deb
```

Then enable and verify as above
(`sudo systemctl enable --now linux-trackpad-driver.service`, `ltd doctor`).

### From source (other distros)

```sh
git clone https://github.com/aaronroquefonseca/linux-trackpad-driver
cd linux-trackpad-driver
cargo build --locked --release
sudo install -m755 target/release/ltd /usr/bin/ltd
```

Then install the rules, units, hook and default config — exact commands in
[`INSTALL.md`](INSTALL.md) — and enable:

```sh
sudo systemctl enable --now linux-trackpad-driver.service
```

Verify with `ltd doctor` (below).

Maintainer release process: [`docs/RELEASING.md`](docs/RELEASING.md).

## Supported hardware and testing status

| Device | USB VID:PID | BT VID:PID |
|---|---|---|
| Magic Trackpad 2 (Lightning) | `05ac:0265` | `004c:0265` |
| Magic Trackpad 2 (USB-C) | `05ac:0324` | `004c:0324` |

- Personally tested: USB-C model over USB only.
- Lightning model: supported in the implementation, not yet verified on hardware.
- Bluetooth: implemented but not yet tested with this rewrite. The original
driver had good Bluetooth compatibility, so it is expected to work — but
bugs are possible. See issue #42.

Polling is an Apple-trackpad USB setting (default 110 Hz, usable 16–110).
It is independent of the click engine and ignored on Bluetooth and other pads.

## Configuration

Full reference: [`config/default.toml`](config/default.toml) and
[`docs/CONFIG.md`](docs/CONFIG.md). The shipped defaults are a conventional
trackpad map (1-finger left, 2-finger right, 3-finger middle; force and slide
actions opt-in, off by default) with a light press (threshold 30) and soft
feedback (strength 2, quiet waveform) — my preference. If clicks feel too
eager or too faint, try the firmer classic tune:

```toml
[click]
sensitivity = 64
[haptics]
haptic = 5
quiet_click = false
```

You only need the lines you want to change; omitted keys fall back to the
documented defaults. The key gate:

```toml
schema_version = 1
enhanced_clicks = true   # false: kernel passthrough, device setup still runs
```

`[device]` settings work on their own; `[pointer]`, `[click]`,
`[haptics]`, `[force]`, `[slide]` and `[spoof]` need `enhanced_clicks = true`.
USB proxy failure falls back to passthrough (grab released); on Bluetooth
with `enhanced_clicks = true` the HID driver is unbound (sticky until
re-bind/replug, proxy grabs evdev), so a failed enhanced path may need
re-bind/replug — that BT path is not yet hardware-verified (issue #42).
Device setup never depends on the click engine.

Apply changes with:

```sh
sudo systemctl restart linux-trackpad-driver
```

(Restart is required for now; live reload is planned.)

Upgrading from a pre-v1 config: the flat layout was replaced by sections,
so old files fail with `unknown field`. Rewrite yours against the shipped
default — values carry over one-to-one.

## Usage

```sh
sudo ltd run                                   # daemon: setup + click engine
sudo ltd run -c /etc/linux-trackpad-driver/config.toml
sudo ltd reconnect /dev/hidrawN                # udev entry / manual re-init
sudo ltd reconnect /dev/hidrawN -f -p 90 -d
ltd doctor                                     # read-only checklist
```

## Troubleshooting

`ltd doctor` is read-only: device scan + config validate + daemon socket
dial. Exit 0 means the config is valid and a trackpad node was seen — it
does not by itself mean the daemon is running; check
`systemctl is-active linux-trackpad-driver.service` for that.

| Symptom | Check | Fix |
|---|---|---|
| `doctor`: no trackpad seen | cable seated? BT paired? `ls /sys/class/hidraw`; `dmesg \| grep -i hid` | replug / finish BT pairing, re-run `ltd doctor` |
| `doctor`: config invalid | line names the key + accepted range | edit `/etc/linux-trackpad-driver/config.toml` (unknown keys are rejected — usually a typo); validate with `ltd doctor -c <file>` first |
| Edited config, nothing changed | service restarted? | `sudo systemctl restart linux-trackpad-driver.service` (required for now) |
| Double input on Bluetooth | `sudo evtest` on the trackpad node vs `journalctl` | on BT the kernel node stays live alongside hidraw; with `enhanced_clicks = true` (the default) the driver unbinds the BT HID driver (sticky until re-bind/replug) and the proxy grabs evdev to suppress the duplicate — a failed enhanced path may need re-bind/replug; this path is not yet hardware-verified with this rewrite (issue #42) |
| Motion lost after suspend (clicks survive) | `journalctl -u linux-trackpad-driver-resume.service`; `sudo ltd resume` manually | reinstall the system-sleep hook as executable (`INSTALL.md`); the daemon tick re-inits drifted nodes within ~30 s as backup |

Full udev/systemd/permissions notes: [`INSTALL.md`](INSTALL.md).

## Device support tiers and future compatibility

Planned tiers for future trackpads (only Apple Magic Trackpad 2 is
implemented today):

- **L0** — basic trackpads without pressure detection: pointer speed and
finger-count remapping.
- **L1** — pressure detection, no software-controlled haptic feedback:
pressure clicks, force levels and touch actions work; haptic settings stay
configured but inactive.
- **L2** — pressure plus software-controlled haptic feedback: everything
works. Apple uses a proprietary channel; ChromeOS forcepads define the
open standard variant (planned).

Unsupported features remain configured but inactive. Details:
`docs/RESEARCH.md`.

## Development

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Conventional Commits, small focused PRs (squash-merged), two reviewers on
substantial changes. Capability research: `docs/RESEARCH.md`.

## Hardware safety and disclaimer

This is an unofficial, reverse-engineered driver, not affiliated with or endorsed by Apple.

The driver exposes low-level hardware settings, including values beyond those available through Apple's standard configuration interface. These settings have been functionally tested, but their long-term effects, particularly at extreme values, have not been fully evaluated. Hardware damage or excessive wear cannot be ruled out.

The default settings are recommended. Adjust hardware-related values gradually and use extreme settings with caution.

This software is provided **as is, without warranty**. To the extent permitted by applicable law, the authors and contributors disclaim liability for hardware damage, data loss, or other damages arising from its use. See the MIT and Apache-2.0 licenses for the applicable terms.

## License

Dual-licensed under MIT OR Apache-2.0 — see [`LICENSE-MIT`](LICENSE-MIT)
and [`LICENSE-APACHE`](LICENSE-APACHE). No per-file license headers: the
repo-level license files + `Cargo.toml` SPDX expression are authoritative.
