# Install notes — udev rules + systemd units

Binary: `ltd` (built via `cargo build --release`; binary path
`target/release/ltd`). The rules and units below assume it is installed as
`/usr/bin/ltd`. Adjust paths if installed elsewhere.

```sh
cargo build --release
sudo install -m755 target/release/ltd /usr/bin/ltd
```

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
sudo systemctl daemon-reload
sudo systemctl enable --now linux-trackpad-driver.service
systemctl status linux-trackpad-driver.service
```

- Main unit: `Type=simple`, `Restart=on-failure`, `RuntimeDirectory=`
  creates `/run/linux-trackpad-driver` for `driver.sock` (socket source of
  truth: `src/lock.rs` `DRIVER_SOCK`; `$LTD_SOCKET` overrides for
  tests/unprivileged runs).
- Reconnect template: `Type=oneshot`, runs `ltd reconnect /dev/%I`.

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
- Resume unit: `Type=oneshot`, `After=` the sleep targets, runs
  `ltd resume`. There is no `resume.target` to bind to (`WantedBy=` sleep
  targets would run *before* suspend), so the hook is the post-resume
  trigger; the unit covers manual runs plus a harmless idempotent pass at
  boot when enabled.
- Daemon side (belt-and-braces): the running daemon re-verifies each known
  USB node every 30 s (`GET_FEATURE` read-back of the polling report) and
  re-inits on mismatch; newly appeared nodes get full init, vanished nodes
  emit `Detached`. Clicks re-attaches via the device-ready hub after init,
  never before.

No udev `change` rule is needed: resume re-enumeration already emits `add`,
which the rules above hand to the reconnect template; the hook + daemon
tick cover the no-re-enumeration case (node persists, only mode is lost).

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
