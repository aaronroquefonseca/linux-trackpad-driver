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

Manual reconnect without replug:

```sh
sudo systemctl start 'linux-trackpad-driver-reconnect@hidraw2.service'
# or directly:
sudo ltd reconnect /dev/hidraw2
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
- Re-attach path: the `ltd resume` one-shot re-arms hardware only (its hub
  would reach zero listeners, so it emits nothing). Clicks re-attaches on
  the daemon hub: each 30 s pass also checks enhanced-session health.
  Failed attachments and sessions released after source or output loss retry even
  when USB polling still matches or a Bluetooth node keeps its name.
  These attachment-only retries send no hardware init reports. Failed
  hardware init or required USB re-init remains pending and blocks
  attachment until full setup succeeds, even if polling later matches.
  A replaced devnode instance is detached before fresh init, including
  when its name is reused between scans.
  Healthy sessions and siblings receive no attachment retries; gate-off
  keeps native passthrough. Recovery starts on a later pass, with init and
  attachment work adding to the scan interval.

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
