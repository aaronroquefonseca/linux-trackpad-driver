# linux-trackpad-driver

Userspace Rust driver for the Apple Magic Trackpad 2 (Lightning + USB-C) on Linux.
Core-init always on (USB feature reports, BT bring-up, reconnect, suspend re-init);
enhanced click proxy (pressure → haptic + remappable actions) is opt-in.
Generic-trackpad support is planned via capability tiers; Apple first.

## Status (2026-10-14, in active development)

- `main`: core-init (USB `02 01` / `21 01` / capped `99 F2 0x23` polling,
  BT single-node, `run`/`reconnect` CLI, lock socket, discovery) + full TOML
  schema + 9B multitouch parser + udev/systemd + mock harness + CI. ~98 tests green.
- Open PRs: `reconnect-bt` (reconnect hardening, in review), `uinput-proxy`
  (click proxy, in progress). Next: `suspend-hook`, `force-click`.
- See `docs/ROADMAP.md`, `docs/ARCHITECTURE.md`, `docs/TASKS.md`, `docs/RESEARCH.md`.

## Hardware

| Device | USB VID:PID | BT VID:PID |
|---|---|---|
| Magic Trackpad 2 (Lightning) | `05ac:0265` | `004c:0265` |
| Magic Trackpad 2 (USB-C) | `05ac:0324` | `004c:0324` |

Polling: device-capped at ~110 Hz (`99 F2 XX`, ≈`3850/XX`; default `XX=0x23`).
Higher requests are clamped, never sent. BT has no rate tuning (USB-first).

## Install

### Arch Linux (AUR)

```sh
yay -S linux-trackpad-driver-git
# or: paru -S linux-trackpad-driver-git
sudo systemctl enable --now linux-trackpad-driver.service
```

Verify:

```sh
ltd doctor
systemctl status linux-trackpad-driver.service
journalctl -u linux-trackpad-driver.service --since "5 min ago"
```

The AUR package (`packaging/arch/PKGBUILD`) ships the full file set:
`/usr/bin/ltd`, the udev rules, all three units (main service, reconnect
template, resume service), the system-sleep hook, and
`/etc/linux-trackpad-driver/config.toml` (a conffile via `backup=`,
preserved on upgrade). Post-install reloads udev + systemd and
re-triggers hidraw `add`; the daemon is never auto-started — enabling
is one explicit step.

### Debian / Ubuntu (.deb)

```sh
sudo dpkg -i linux-trackpad-driver_0.1.0_amd64.deb
sudo apt-get install -f   # only if dpkg reports missing dependencies
```

Post-install steps (the package only reloads daemons; enabling is an
explicit admin step by policy — see `debian/README.Debian`):

```sh
sudo systemctl daemon-reload
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=hidraw --action=add
sudo systemctl enable --now linux-trackpad-driver.service
```

Then verify with `ltd doctor` (below).

### Fedora / other distros (from source)

```sh
cargo build --locked --release
sudo install -m755 target/release/ltd /usr/bin/ltd
```

Then install the rules, units, hook, and default config manually — exact
commands in [`INSTALL.md`](INSTALL.md) — and enable:

```sh
sudo systemctl enable --now linux-trackpad-driver.service
```

## First run: `ltd doctor` checklist

```sh
ltd doctor
ltd doctor -c /etc/linux-trackpad-driver/config.toml
```

`ltd doctor` is read-only: sysfs device scan + config validate + daemon
socket dial. It never sends feature reports, never grabs, never changes
hardware state. Exit 0 = config valid and a trackpad node seen; exit 1
otherwise. Each checklist question maps to one line plus a manual
follow-up:

| Question | `ltd doctor` line | Manual follow-up |
|---|---|---|
| Is a device seen? | `[ok] device seen: /dev/hidrawN …` | `ls /sys/class/hidraw`, `dmesg \| grep -i hid` |
| Is the hidraw node there? | `(present …)` per node | `ls -l /dev/hidrawN` |
| Is the driver bound yet? | `(driver bound …)` per node | fresh `add` may need a second: probe/bind race is normal |
| Did the udev rule fire? | — (rule hands off to systemd) | `udevadm monitor --udev --subsystem-match=hidraw`, then replug; `systemctl status 'linux-trackpad-driver-reconnect@hidrawN.service'` |
| Is the daemon active? | `[ok] daemon socket live …` / `[..] socket absent …` | `systemctl is-active linux-trackpad-driver.service` |
| How to read logs? | — | `journalctl -u linux-trackpad-driver.service --since "5 min ago"` |

## Troubleshooting

| Symptom | Check | Fix |
|---|---|---|
| `doctor`: no trackpad seen | cable seated? BT paired? `ls /sys/class/hidraw`; `dmesg \| grep -i hid` | replug / finish BT pairing, re-run `ltd doctor` |
| `doctor`: config invalid | line names the key + accepted range | edit `/etc/linux-trackpad-driver/config.toml` (unknown keys are rejected — usually a typo); re-run `ltd doctor -c <file>` first |
| Permission denied on a manual run | `getfacl /dev/hidrawN`; `groups` | `sudo -E ltd …` (keeps `$LTD_SOCKET`/`$LTD_CONFIG`); or on `input`-grouped distros `sudo usermod -aG input $USER` + re-login. The system daemon runs as root and needs nothing extra. |
| Udev rule not firing | `udevadm monitor --udev --subsystem-match=hidraw` while replugging; `udevadm test /sys/class/hidraw/hidrawN` | `sudo udevadm control --reload-rules` + `sudo udevadm trigger --subsystem-match=hidraw --action=add`; confirm the rules file exists under `/usr/lib/udev/rules.d/` (packaged) or `/etc/udev/rules.d/` (manual). Note: reconnect without a live daemon disengages unless `-f` — start the main service first. |
| Double input on Bluetooth | `sudo evtest` on the trackpad node vs `journalctl` | hidraw coexists with `hid-magicmouse` on BT: the kernel keeps emitting evdev unless suppressed. Prefer USB, or set `enhanced_clicks = true` so the proxy grabs the node (grab suppresses the kernel duplicate). |
| Motion lost after suspend (clicks survive) | `journalctl -u linux-trackpad-driver-resume.service`; `sudo ltd resume` manually | reinstall the system-sleep hook as executable (`INSTALL.md`); the daemon tick re-inits drifted nodes within ~30 s as backup. |

Full udev/systemd/permissions notes: [`INSTALL.md`](INSTALL.md).

## Usage

```sh
sudo ltd run                          # daemon: init + idle (passthrough via libinput)
sudo ltd run -c /etc/linux-trackpad-driver/config.toml
sudo ltd reconnect /dev/hidrawN       # udev entry / manual re-init
sudo ltd reconnect /dev/hidrawN -f -p 90 -d
ltd doctor                            # read-only first-run checklist (scan + config + socket)
```

Config: [`config/default.toml`](config/default.toml). Key gate:

```toml
enhanced_clicks = false   # default OFF: pure passthrough, init only
```

Set `true` to enable the click proxy (pressure thresholds, haptics,
1–5 finger action maps, force second level). Proxy failure always falls back
to passthrough; init never depends on clicks.

## Capability tiers

L0 (no pressure/haptic) / L1a (area-proxy) / L1b (true force) /
L2s (standard `0x0E` haptics) / L2p (proprietary haptic: Apple, Sensel…).
Apple MT2 = L2p with L1a-class arbitrary pressure units. Details: `docs/RESEARCH.md`.

## Development

```sh
cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test
```

Conventional Commits, small PRs, review policy in `docs/ROADMAP.md`.
Reference implementation (read-only, never copied):
`/home/aaronroque/Projects/AppleTrackpadUserSpaceApp` (outside this repo).
