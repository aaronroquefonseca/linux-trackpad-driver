# Architecture — linux-trackpad-driver

Single crate: binary `ltd` (`src/main.rs` + modules `apple_parse`, `cli`,
`clicks`, `config`, `daemon`, `discovery`, `evdev`, `evdev_generic`,
`hw_mock`, `init`, `lock`, `proxy`, `pump`, `resume`, `test_env`).

## Layering

```
hidraw/evdev ──> init (always on) ──> kernel/libinput passthrough (fallback path)
                      │
                      └──> input ──> clicks ──> uinput (gated by enhanced_clicks)
                                                ▲
                                          config (enhanced_clicks=true default)
```

- `init` has zero deps on clicks/uinput (`src/init.rs` imports std + anyhow only).
- Clicks subscribes to init's device-ready events (`DeviceReadyHub` in
  `src/daemon.rs`); never drives init.
- Clicks failure mode: log, release grab, passthrough continues.
- Bluetooth bring-up is single-node: no init reports, no rate tuning
  (`init_bluetooth` in `src/init.rs`).
- Polling is an Apple-USB-only setting (default 110 Hz, usable 16..110):
  requests above ~110 Hz still place `X = 0x23` on the wire and warn;
  requests below ~16 Hz are rejected instead of rounding up.

## Touch input contract

- `TouchSource` trait (`src/apple_parse.rs`): `tier()` + `pressure_units()`
  (`Arbitrary` vs `Grams`) + `poll_frame()`; Apple MT2 reports `L2p`/`Arbitrary`,
  pressure is the raw byte at 9-byte-block offset 7 (0..=253 valid, arbitrary units).
- `TouchFrame { fingers: Vec<FingerContact>, clicks /* raw report[1], button = clicks & 1 */, timestamp_ms /* always 0 */ }`.
- `EvdevGeneric` (`src/evdev_generic.rs`) is an L0/L1a stub; full decode is future work.

## Click state machine (spec)

- Per-finger pressure vs `sensitivity` (press) / `sensitivity - margin` (release hysteresis).
- Timed debounce holds a first-level-only release until 30 ms after its press;
  a force pressure dip holds until 30 ms after the last force-held report.
  Idle polls finish these observed releases before pending combo presses,
  even when no further HID report arrives. Idle time preserves contacts still
  held in the last report and never retries or creates press/force/slide edges.
- Finger-count at press edge selects action `click_1..=click_5`
  (`left/right/middle/none/empty`/key-combo).
- `force_offset > 0` arms second level at `sensitivity + force_offset`
  with `force_1..=force_5`, optional `force_haptic` override.
- Force releases at `sensitivity + force_offset - margin` (saturating);
  since that is ≥ the first release, force always drops at or before the
  first level. First and force latches are independent and coexist (both key
  sets held; a code held by both stays down until both release); a drop below
  the first release clears both. Disabling mid-hold clears force (proxy releases).
- One fresh edge per frame from idle (old `Released` admits `Pressed` only):
  a deep jump latches first this report, force follows next report while held
  deep; an already-latched first level takes the force edge immediately.
- `haptic` 0..=255 output strength; `quiet_click` selects soft waveform.
- `speed` scales pointer motion only in proxy path; never touches init.

## Config schema (v1 sectioned)

Device tiers: L0 basic no-pressure (pointer `speed` + finger-count remapping);
L1 pressure no host buzz (+`click`/`force`/`slide`); L2 pressure+host buzz all
active (Apple proprietary today, ChromeOS standard planned). Only Apple Magic
Trackpad 2 implemented; others planned. `[device]` independent of
`enhanced_clicks`; `[pointer]`/`[click]`/`[haptics]`/`[force]`/`[slide]`/
`[spoof]` require `enhanced_clicks=true` (unsupported stay saved+inactive).
Omitted keys fall back to defaults; device-specific pressure values, not
portable. Unknown keys error. Apply: `sudo systemctl restart
linux-trackpad-driver` (required for now; live reload planned).

```toml
schema_version = 1
log_level = "info"        # debug|info|warn|error
enhanced_clicks = true    # opt-in gate for entire clicks pipeline
[device]
polling_rate = 110        # Apple USB only, 16..110
[pointer]
speed = 100               # 1..1000
[click]
sensitivity = 30          # 1..255, firmer alt 64
margin = 5                # 0..255
click_1 = "left"          # left|right|middle|back|forward|none|empty|keys
click_2 = "right"         # (+ per-binding stagger_ms objects)
click_3 = "middle"
click_4 = "none"
click_5 = "none"
[haptics]
haptic = 2                # 0..255, 0 = silent
quiet_click = true
# force_haptic omit = inherit haptic
force_quiet_click = false
[force]
force_offset = 0          # 0 disables; e.g. 130 -> 160 with sensitivity 30
force_1 = "none"          # force_1..5 + transition_mode + combo_stagger_ms
transition_mode = "cut"   # continuous|cut
combo_stagger_ms = 50     # 0..500
[slide]
slide_1 = "none"          # slide_1..5, e.g. slide_5 super+left
[spoof]
actions_follow_spoof = false  # FIRST under [spoof]
# [spoof.slide]/[spoof.click]/[spoof.force] sparse: omitted = real,
# 0 = passthrough, 1..5 = virtual; wire always virtual
```

Strict v1: `schema_version = 1` required, `deny_unknown_fields` everywhere
incl. nested, unknown version errors.

## Daemon CLI

- `ltd run [-c config]` — init loop + optional clicks pipeline.
- `ltd reconnect <hidraw-dev> [-f] [-p Hz] [-d]` — udev entry; idempotent.
  Activation contract: without a live daemon socket and without `-f`/`-d`
  it leaves the hardware alone (exit 0), so installed-but-disabled is off;
  `-f` forces init, `-d` disengages (explicit manual forms).
- `ltd resume [-f] [-p Hz] [-c config]` — re-run init after suspend (also via
  sleep hook). Plain `resume` without a live daemon is an inactive no-op;
  `-f` forces re-init while the daemon is stopped.
- `ltd doctor [-c config]` — ordinary-user, read-only checklist: device metadata,
  on-disk config validation, and listening stream socket metadata from
  `/proc/net/unix` in the caller's network namespace (no socket connection).
  Doctor alone falls back to the installed `/etc/linux-trackpad-driver/config.toml`
  when neither `-c` nor nonempty `$LTD_CONFIG` selects a file.
- Single-instance lock via `/run/linux-trackpad-driver/driver.sock`
  (`$LTD_SOCKET` overrides for tests/unprivileged runs).

## Udev / systemd / suspend

- udev: `ACTION=="add", SUBSYSTEM=="hidraw"`, VID:PID-gated
  (`05ac`/`004c` × `0265`/`0324`), requests `linux-trackpad-driver-reconnect@%k.service`
  via `SYSTEMD_WANTS` (the service runs `reconnect /dev/%I`). No bind/unbind tricks.
- systemd: `Type=simple`, `Restart=on-failure`, `RuntimeDirectory=`.
- suspend/resume: re-run init sequence on resume (USB restarts from multitouch
  enable — report `0x02` shrinks 21→8 bytes across power transitions);
  clicks pipeline re-attaches, never reorders init.
