# Architecture — linux-trackpad-driver (target)

> Coordinator spec. Agents reimplement from this; do not copy reference code.

## Crate layout (proposed, base-init may adjust with justification)

```
linux-trackpad-driver/
  Cargo.toml            # workspace
  crates/
    ltd-core/           # config, logging, error types, daemon lifecycle
    ltd-init/           # USB/BT init: feature reports, polling, BT single-node
    ltd-input/          # TouchSource trait + AppleHidraw; EvdevGeneric stub
    ltd-clicks/         # pressure state machine, haptics, 1-5 finger map, force
    ltd-uinput/         # evdev grab + uinput emit (clicks only, never init)
    ltd-daemon/         # `run` / `reconnect` CLI, udev hook entry, signals
  udev/                 # 99-*.rules (add-only, hidraw, VID:PID-gated)
  systemd/              # service unit (simple, Restart=on-failure)
  config/               # default TOML
  docs/
```

Single binary is acceptable for Phase 0/1 if workspace feels heavy —
decision belongs to `base-init`, recorded here afterwards.

## Layering

```
hidraw/evdev ──> ltd-init (always on) ──> kernel/libinput passthrough (default path)
                        │
                        └──> ltd-input ──> ltd-clicks ──> ltd-uinput (opt-in only)
                                                  ▲
                                            config (enhanced_clicks=false default)
```

- `ltd-init` has zero deps on `ltd-clicks` / `ltd-uinput`.
- Clicks subscribe to init's device-ready events; never drive init.
- Clicks failure mode: log, release grab, passthrough continues.

## TouchSource trait (contract, names may evolve)

```rust
// Illustrative only — implementing agent owns final signatures.
pub enum CapabilityTier { L0, L1, L2 }

pub struct TouchFrame {
    pub fingers: Vec<Finger>,
    pub timestamp_ms: u64,
}

pub struct Finger {
    pub id: u8,
    pub pressure: u8,      // RAW device byte (Apple: 0..=253 valid, 254/255
                           // reserved passthrough — see RESEARCH §6). NOT
                           // normalized: clicks thresholds operate on this
                           // scale; uinput stage emits old-service
                           // normalized 60/800/800 per down contact.
    pub contact: bool,
}

pub trait TouchSource {
    fn tier(&self) -> CapabilityTier;
    /// L1a (area-proxy, res==0) vs L1b (true grams, res!=0). Apple MT2 = Arbitrary.
    fn pressure_units(&self) -> PressureUnits;
    fn poll_frame(&mut self) -> anyhow::Result<Option<TouchFrame>>;
}
```

- `AppleHidraw`: hidraw 9-byte blocks, pressure @ offset 7, multitouch decode.
- `EvdevGeneric`: stub in Phase 1 (returns tier + `unimplemented` poll with
  a clear error, plus mock path for tests). Full impl Phase 2.

### Phase-1 note (apple-parse): adopted tiers + `pressure_units()`

`CapabilityTier` uses the `docs/RESEARCH.md` §4 split
(`L0/L1a/L1b/L2s/L2p`), and `TouchSource` gains
`fn pressure_units() -> PressureUnits` (`Arbitrary` vs `Grams`) so the
L1a/L1b distinction is machine-checked. Apple MT2 reports
`tier() == L2p` (proprietary device-autonomous pad) with
`pressure_units() == Arbitrary`: pressure @ offset 7 is a nominal 0..253
byte with `res == 0` — never grams/newtons, never `EV_FF`/`FF_HAPTIC`.
Decode-only: `poll_frame` errs (transport belongs to `uinput-proxy`);
the caller logs and drops the failed frame, never aborts the read loop.
`TouchFrame.clicks` is raw `report[1]`: button state is `clicks & 1`;
bits 6..7 are device-timestamp bits, not buttons.

## Click state machine (spec)

- Per-finger pressure vs `sensitivity` (press) / `sensitivity - margin` (release hysteresis).
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

## Config schema (target, `config-schema` task owns final)

```toml
log_level = "info"        # debug|info|warn|error
polling_rate = 110        # requested Hz, CAPPED: device max demonstrated ~111
                          # (0x99 report `99 F2 XX`, ≈3850/XX; XX=0x23 → ~111 Hz).
                          # Never request above device max; verify by measurement.
                          # BT rate equivalent unknown — USB-first.
enhanced_clicks = false   # opt-in gate for entire clicks pipeline
sensitivity = 64          # 1..=255
margin = 5
haptic = 5                # 0..=255, 0 = silent
quiet_click = false
speed = 100
click_1 = "left"          # left|right|middle|back|forward|none|empty|keys
click_2 = "right"
click_3 = "middle"
click_4 = "none"
click_5 = "none"
force_offset = 0          # 0 disables force level
# force_haptic, force_quiet_click, force_1..=force_5 optional
```

New key vs reference: `enhanced_clicks` (explicit opt-in gate).

## Daemon CLI

- `ltd run [-c config]` — init loop + optional clicks pipeline.
- `ltd reconnect <hidraw-dev> [-f] [-p Hz] [-d]` — udev entry; idempotent;
  `-f` proceeds when daemon socket absent; `-d` disengage path.
- Single-instance lock via `/run/linux-trackpad-driver/driver.sock`
  (name TBD by base-init; must not collide with old `magic-trackpad-driver` path
  unless migration is handled).

## Udev / systemd / suspend

- udev: `ACTION=="add", SUBSYSTEM=="hidraw"`, VID:PID-gated
  (`05ac`/`004c` × `0265`/`0324`), runs `reconnect /dev/%k`. No bind/unbind tricks.
- systemd: `Type=simple`, `Restart=on-failure`, `RuntimeDirectory=`.
- suspend/resume: re-run init sequence on resume; clicks pipeline re-attaches,
  never reorders init.

## Phase-0 decision record (base-init, 2026-10-14)

- **Single crate, not workspace.** Binary `ltd` (`src/main.rs` + modules
  `cli/config/discovery/init/lock/daemon`) instead of the proposed
  `crates/ltd-*` workspace. Rationale: base is small enough that a workspace
  adds scaffolding without isolation benefit; the hard isolation rule
  (init has zero clicks deps) is enforced at module level plus a buildable,
  tested stub hook (`DeviceReadyHub`). Split into `ltd-core/ltd-init/...`
  when the clicks pipeline lands (Phase 1 `uinput-proxy` or later).
- **Polling posture corrected (coordinator directive).** Default request is
  the capped device maximum ~110 Hz (`X = 0x23 = 35`, `3850/35 ≈ 110`) —
  never 500 Hz. `resolve_polling` maps the raw request to `X`, then floors
  the wire byte at `0x23`, so requests above ~110 Hz still place `X = 0x23`
  on the wire (unverified divisor range; device may NAK/ignore `X < 35`).
  `theoretical_hz` reports the raw request's meaning pre-floor; over-cap
  requests warn at the daemon layer. Config default `polling_rate = 110`;
  requests below ~16 Hz are rejected (`X` saturates at 255).
- **Ordering.** `reconnect`/`run` wait for the hidraw node (2 s poll), send
  each report with backoff retry (5 attempts; sleeps 25/50/100/200 ms),
  retry the whole USB sequence up to 3× on transient (`ENOENT`, node
  mid-bind) only — `EINVAL`/`ENOTTY`/`EACCES` fail fast at both layers —
  and best-effort verify the polling report via `HIDIOCGFEATURE`. Resume
  MUST re-run the sequence from multitouch enable
  (report `0x02` 21→8 byte regression across power transition).
- **BT scope.** Single-node bring-up only; no init reports, no rate tuning.
  No exclusivity vs `hid-magicmouse` — double-input handling (unbind or
  evdev-grab behind the opt-in flag) belongs to Phase 1.
- **Socket.** `/run/linux-trackpad-driver/driver.sock` (never the legacy
  `magic-trackpad-driver` path; legacy constant kept only as a refusal
  guard). `$LTD_SOCKET` overrides for tests/unprivileged runs.
- **Downstream contract note.** Future proxy node: `res == 0`, arbitrary
  0..253 range, Type-B slots + tracking IDs; no grams/newtons claims, no
  `EV_FF`/`FF_HAPTIC` advertisement (no 2026 libinput/Wayland consumer).

## Review follow-ups (base-init, 2026-10-14, verdict REQUEST-CHANGES → fixed)

- **L0 ioctl dir.** `HIDIOCSFEATURE`/`HIDIOCGFEATURE` are
  `_IOC(_IOC_WRITE|_IOC_READ, ...)` (dir = 3); the initial code used 1/2 and
  would have failed with `ENOTTY` on hardware. Pinned to `0xC0024806` /
  `0xC0034806` / `0xC0034807`, cross-checked against the host
  `linux/hidraw.h` in tests and a compiled-C ground truth.
- **Polling floor.** Requests below ~16 Hz (`X` saturates at 255) are
  rejected instead of silently rounding up ~15×. `theoretical_hz` reflects
  the raw request pre-cap; over-cap requests warn at the daemon layer.
- **Fail-closed reconnect.** Unrecognized nodes are rejected, never defaulted
  to USB; BT engage waits for the node like USB; BT disengage emits
  `Detached`, not `Ready`.
- **Retry policy.** Backoff retries `ENOENT` (node mid-bind) only; `EINVAL` /
  `ENOTTY` / `EACCES` fail fast at both per-report and whole-sequence layers.
- **Lock.** Stale-socket removal only on refused/missing connect; anything
  else (e.g. `EACCES`) bails and preserves the file.
- **Hub.** Listeners run on lock-free snapshots; mutex poison recovered, not
  panicked; re-entrant subscribe safe.
- Sibling `hw_mock` tests updated where they pinned pre-review behavior
  (sub-16 sweep, EINVAL-retry expectations); transient tests now use `ENOENT`.
