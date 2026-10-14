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

```sh
cargo build --release
sudo install -m755 target/release/ltd /usr/bin/ltd
```

Full udev/systemd/permissions notes: [`INSTALL.md`](INSTALL.md).

## Usage

```sh
sudo ltd run                          # daemon: init + idle (passthrough via libinput)
sudo ltd run -c /etc/ltd/config.toml
sudo ltd reconnect /dev/hidrawN       # udev entry / manual re-init
sudo ltd reconnect /dev/hidrawN -f -p 90 -d
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
