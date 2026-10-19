# Install notes — udev rules + systemd units

Binary: `ltd` (built via `cargo build --release`; binary path
`target/release/ltd`). The rules and units below assume it is installed as
`/usr/bin/ltd`. Adjust paths if installed elsewhere.

```sh
cargo build --release
sudo install -m755 target/release/ltd /usr/bin/ltd
```

First-run check (read-only — scan + config + socket, never touches HW):

```sh
ltd doctor
```

## Fedora (`.rpm` from GitHub Releases)

RPMs for Fedora (`x86_64`) are intended to ship with future
[GitHub Releases](https://github.com/aaronroquefonseca/linux-trackpad-driver/releases).
The existing `v0.1.0` release does not contain an RPM:

```sh
sudo dnf install ./linux-trackpad-driver-*.rpm
sudo systemctl enable --now linux-trackpad-driver.service
```

For other arches, or to inspect every step, use the generic source build
below (`cargo build --release`, then install the binary, rules, units, hook
and default config with the commands in each section below). No Fedora
dependency list is given here on purpose: the driver itself links no extra
system libraries (all dependencies in `Cargo.toml` are pure Rust plus `libc`).

## udev rules

```sh
sudo install -m644 udev/99-linux-trackpad-driver.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules
# Re-trigger for an already-plugged device (or unplug/replug):
sudo udevadm trigger --subsystem-match=hidraw --action=add
# Watch it fire:
udevadm monitor --udev --subsystem-match=hidraw
```

Rules match `ACTION=="add", SUBSYSTEM=="hidraw"` gated on the four
VID:PID combos (`05ac`/`004c` × `0265`/`0324`) via the physical USB
parent's `ATTRS{idVendor}`/`ATTRS{idProduct}`. They never run `ltd`
inside udev `RUN` — each match only sets `TAG+="systemd"` +
`ENV{SYSTEMD_WANTS}` for the reconnect template unit (see
`docs/RESEARCH.md` §6: long RUN blocks udev; and the systemd#37063 trap —
no `ATTRS{idVendor}` matching on `input` event nodes; none lives here).

Manual reconnect without replug (daemon running):

```sh
sudo systemctl start 'linux-trackpad-driver-reconnect@hidraw2.service'
# or directly:
sudo ltd reconnect /dev/hidraw2
# explicit forcing while the daemon is stopped:
sudo ltd reconnect /dev/hidraw2 -f
```

## systemd units

```sh
sudo install -m644 systemd/linux-trackpad-driver.service \
  systemd/linux-trackpad-driver-reconnect@.service /etc/systemd/system/
sudo install -Dm644 config/default.toml /etc/linux-trackpad-driver/config.toml  # edit this copy; absent → built-in defaults
sudo systemctl daemon-reload
sudo systemctl enable --now linux-trackpad-driver.service
systemctl status linux-trackpad-driver.service
```

- Main unit: `Type=simple`, `Restart=on-failure`, `RuntimeDirectory=`
  creates `/run/linux-trackpad-driver` for `driver.sock` (socket source of
  truth: `src/lock.rs` `DRIVER_SOCK`; `$LTD_SOCKET` overrides for
  tests/unprivileged runs).
- Reconnect template: `Type=oneshot`, runs `ltd reconnect /dev/%I`.

## Activation contract (off means off)

The main service is the only automatic activator. Installing, disabling,
or stopping it leaves the trackpad hardware alone:

| Path | Daemon stopped | Daemon running |
|---|---|---|
| udev hotplug → `reconnect@` one-shot (no `-f`) | no-op, exit 0 | re-inits node |
| sleep hook / resume unit → plain `ltd resume` | no-op, exit 0 | re-verifies + re-inits on mismatch |
| `ltd reconnect /dev/hidrawN -f`, `ltd reconnect /dev/hidrawN -d`, `ltd resume -f` | runs as requested (explicit manual forcing) | runs as requested |

Background one-shots check the live daemon socket and return before any
node wait, scan, or feature-report write, so a queued unit can neither
fail on missing hardware nor touch it. Packaging never triggers hardware
either: install/upgrade scriptlets only reload udev/systemd (no
`udevadm trigger`, no enable/start — same policy on Arch, Debian, and
Fedora).

Manual `udevadm trigger --action=add` above therefore only re-inits once
the service runs; with the service stopped the queued one-shots exit 0
without touching hardware — use `-f` to force while stopped.

## Config

Switching between `-git`/stable/`-bin` removes the old package first, so pacman
parks your config at `config.toml.pacsave` and installs a fresh default. Copy it
back (`sudo cp /etc/linux-trackpad-driver/config.toml.pacsave /etc/linux-trackpad-driver/config.toml`)
and restart (`sudo systemctl restart linux-trackpad-driver.service`).

## Suspend/resume re-init

Trackpad 2 USB drops out of multitouch mode across suspend (report `0x02`
shrinks 21→8 bytes; clicks survive, motion lost until re-init — see
`docs/RESEARCH.md` §6). Two layers re-send the USB sequence from multitouch
enable; either suffices alone:

```sh
sudo install -m755 systemd/system-sleep/ltd-resume /usr/lib/systemd/system-sleep/ltd-resume
sudo install -m644 systemd/linux-trackpad-driver-resume.service /etc/systemd/system/
sudo systemctl daemon-reload
# Manual re-init without sleeping (or to test the path):
sudo ltd resume
# explicit forcing while the daemon is stopped:
sudo ltd resume -f
systemctl status linux-trackpad-driver-resume.service
```

- System side (primary): the `system-sleep` hook runs `ltd resume` on the
  `post` branch of every sleep type (`suspend`, `hibernate`,
  `hybrid-sleep`, `suspend-then-hibernate`); `pre` is a no-op. The hook is
  system-side only — `user.slice` is frozen while sleep hooks run, so it
  never touches user sessions (no `--user` units, no session D-Bus).
  Sleep hooks inherit PID1's environment, so the hook sets and exports
  `LTD_CONFIG=/etc/linux-trackpad-driver/config.toml` (same path as the
  units) before exec — otherwise resume would re-init at defaults while
  the daemon verifies against the system rate. Hook runtime is bounded
  (~1 s node wait + backoff retries per node); failures are logged and
  swallowed, the hook always exits 0 so a driver hiccup never perturbs
  the sleep cycle.
- Resume unit: `Type=oneshot`, `After=` the sleep targets, runs
  `ltd resume`. There is no `resume.target` to bind to (`WantedBy=` sleep
  targets would run *before* suspend), so the hook is the post-resume
  trigger; the unit covers manual runs plus a harmless idempotent pass at
  boot when enabled.
- Daemon side (belt-and-braces): the running daemon re-verifies each known
  USB node every 30 s (`GET_FEATURE` read-back of the polling report) and
  re-inits on mismatch; newly appeared nodes get full init, vanished nodes
  emit `Detached`.
- Hotplug: the resident subscribes to kernel HID/hidraw/input events before
  its boot scan. Supported trackpad events coalesce for 100 ms; the daemon
  checks the queue every 100 ms, so ready nodes normally start reconciliation
  within 200 ms, plus bounded hardware init/attachment time. The reconnect
  helper still prepares hardware only; its success does not mean the
  resident has attached. No notification connection to the lock socket is
  required.
- Late readiness: event bursts settle for one second. Pending init/grab/source
  setup retries after 250 ms, then 500 ms, then at most 800 ms between
  attempts, for up to ten seconds after an event. A late touch or input node
  wakes discovery again. Known USB feature interfaces cannot take ownership
  ahead of If1; unknown layouts keep their existing fallback. A late actuator
  sibling updates the haptic target without re-grabbing a healthy touch
  session. Healthy sessions/siblings produce no extra attachment events or
  periodic hardware readbacks on these fast passes.
- Recovery/fallback: failed full USB setup blocks attachment until the full
  sequence succeeds, even if polling later matches. Dead USB sessions verify
  before reattaching; Bluetooth keeps native bindings intact. Gate-off never
  requests a proxy grab. Instance replacement tears down old ownership before
  initialization, including a name reused between scans. Monitor startup or
  receive failure and exhausted readiness retries are logged; the 30-second
  full scan/readback remains the fallback for missed events and later
  recovery. Idle/full verification cadence is unchanged.

No udev `change` rule is needed: resume re-enumeration already emits `add`,
which the rules above hand to the reconnect template; the hook + daemon
tick cover the no-re-enumeration case (node persists, only mode is lost).
Hook, tick, and udev interleave benignly: every path sends the same
idempotent multitouch-first sequence, so a double re-init is harmless.

## hwdb note (libinput quirks, optional)

Pressure/size ranges for correct palm/thumb classification come from
libinput quirks, not this driver. If needed per model, add a local hwdb
override (never edit `/usr/lib/udev/hwdb.d`), then:

```sh
sudo systemd-hwdb update
sudo udevadm trigger /dev/input/eventX
```

See `docs/RESEARCH.md` §3 (`AttrPressureRange`, `AttrPalmPressureThreshold`).

## Permissions (input/hidraw groups)

- The system daemon runs as root → hidraw access needs nothing extra.
- Manual/unprivileged runs need read+write on `/dev/hidraw*`. Options:
  - run with `sudo -E` (keeps `$LTD_SOCKET` override), or
  - rely on logind `uaccess` ACLs on the seat (check with
    `getfacl /dev/hidrawN`), or
  - on distros grouping hidraw under `input`, add the user:
    `sudo usermod -aG input $USER` then re-login.
- `$LTD_SOCKET` lets unprivileged runs keep the socket in user-writable
  space, e.g. `LTD_SOCKET=$XDG_RUNTIME_DIR/ltd/driver.sock ltd run`.

## Environment overrides (click proxy)

Normally unset — the daemon resolves everything itself:

- `LTD_EVDEV_NODE=/dev/input/eventN`: pin the physical evdev node for the
  grab. Without it, each hidraw node auto-resolves via the sysfs
  parentage walk, then a name+multitouch-caps scan. The name scan never
  backs a grab (untrusted: no parentage proof), so input-less feature
  interfaces stay passthrough until the touch interface attaches.
- `LTD_UINPUT_NODE=/dev/uinput`: uinput control node for the virtual
  device (default `/dev/uinput`).
- `LTD_HIDRAW_SYSFS=/path/to/class/hidraw`: override the hidraw sysfs class
  dir (default `/sys/class/hidraw`) for hermetic runs. Tests set this to an
  empty temp dir so `ltd reconnect` / `ltd resume` subprocesses never
  discover host hardware; child processes inherit it via env.
