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

Suspend/resume re-init is owned by the `suspend-hook` task (system-side
hook re-sending multitouch enable); these units do not cover it.

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
