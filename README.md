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

### Fedora (`.rpm`)

Starting with the first release built after RPM packaging was added, `.rpm`
files (`x86_64`) will be available in
[GitHub Releases](https://github.com/aaronroquefonseca/linux-trackpad-driver/releases).
The existing `v0.1.0` release does not contain an RPM.

```sh
sudo dnf install ./linux-trackpad-driver-*.rpm
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
USB proxy failure falls back to passthrough (grab released). Bluetooth keeps
native input available until an enhanced proxy session successfully grabs the
physical evdev node; failed attach and one-shot setup leave driver bindings
untouched. Bluetooth remains experimental and has not been hardware-verified
with this rewrite (issue #42). Device setup never depends on the click engine.

A pointer uinput write failure releases the physical grab, retires both
virtual nodes (including held keys and contacts), and marks the proxy session
unattached. An extras-only failure retires the extras keyboard node and
continues pointer motion/buttons; shortcuts are dropped until that node is
created again. Partial writes and interrupted writes are completed before a
frame is considered delivered.

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
sudo ltd reconnect /dev/hidrawN                # udev entry / manual re-init (daemon running)
sudo ltd reconnect -f /dev/hidrawN -p 90       # explicit forcing while the daemon is stopped
sudo ltd reconnect /dev/hidrawN -d             # explicit manual disengage (host-click off)
sudo ltd resume -f                             # explicit resume forcing while stopped
ltd doctor                                     # read-only checklist
ltd --version                                  # embedded build identity (alias: -V)
```

`ltd --version` needs no root privileges or configuration and does not access hardware
or Git at runtime. It works outside the source checkout. For example:

```text
ltd 0.1.0 (stable, commit b32946218ca3)
ltd 0.1.0 (development, commit 18d666641963)
```

Local Cargo builds and AUR `-git` use `development`; release packages use
`stable`. Local changes add `, dirty` before the closing parenthesis. When
source metadata is unavailable, the commit is `unknown`. `ltd doctor` starts
with the same identity, and daemon startup logs it too. See
[build metadata](docs/RELEASING.md#embedded-build-identity) for archive builds.

## Troubleshooting

Run `ltd doctor` as your ordinary user; no sudo is needed. It reads device
metadata, validates config, and checks `/proc/net/unix` for an exact-path
listening stream socket without connecting or requiring socket write access.
`LTD_SOCKET` selects a custom socket path. Socket output is an observation in
the caller's network namespace, not proof of daemon identity or responsiveness.
No observed listener does not establish that the service is stopped (for
example, it may be in another namespace). Unavailable or malformed metadata
and unsupported paths, such as relative socket paths, report **unknown**.
Kernel pathnames can also outlive an unlink or refer to another mount
namespace; this diagnostic does not authenticate the process or resolve such
pathname aliases.

Config selection is `-c <file>` first, then nonempty `LTD_CONFIG`, then
`/etc/linux-trackpad-driver/config.toml`. If the environment-selected or
installed file is missing, doctor reports built-in defaults and the missing
path; a missing explicit file or an invalid/unreadable selected file is an
error. Files are checked **on disk**, so edits may differ from the running
daemon's config until restart. `ltd doctor -c <file>` validates a specific file.

Exit 0 means the config is valid and a trackpad node was seen. Socket status
is informational; check `systemctl is-active linux-trackpad-driver.service`
for service status.

| Symptom | Check | Fix |
|---|---|---|
| `doctor`: no trackpad seen | cable seated? BT paired? `ls /sys/class/hidraw`; `dmesg \| grep -i hid` | replug / finish BT pairing, re-run `ltd doctor` |
| `doctor`: config invalid | line names the key + accepted range | edit `/etc/linux-trackpad-driver/config.toml` (unknown keys are rejected — usually a typo); validate with `ltd doctor -c <file>` first |
| Edited config, nothing changed | service restarted? | `sudo systemctl restart linux-trackpad-driver.service` (required for now) |
| Double input on Bluetooth | `sudo evtest` on the trackpad node vs `journalctl` | the enhanced proxy suppresses duplicates with a reversible evdev grab after its source and virtual pointer are ready; init, reconnect and resume never unbind the HID driver. Check for the `proxy: grabbed` log; BT remains experimental (issue #42) |
| Motion lost after suspend (clicks survive) | `journalctl -u linux-trackpad-driver-resume.service`; `sudo ltd resume` manually (daemon running) or `sudo ltd resume -f` while stopped | reinstall the system-sleep hook as executable (`INSTALL.md`); the daemon tick re-inits drifted nodes within ~30 s as backup |

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
