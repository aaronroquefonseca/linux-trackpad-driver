# TESTING.md — real-trackpad session checklist

Owner-run hardware checklist for the Apple Magic Trackpad 2
(Lightning `05ac:0265` / `004c:0265`, USB-C `05ac:0324` / `004c:0324`).
Docs-only; behavior reference is [`README.md`](README.md),
install reference is [`INSTALL.md`](INSTALL.md),
defaults live in [`config/default.toml`](config/default.toml).

Conventions: every command is copy-pasteable on Arch.
Install tools once:

```sh
sudo pacman -S --needed usbutils bluez bluez-utils libinput evtest hid-tools
```

Debug logs come from `log_level = "debug"` in the *selected* config —
`RUST_LOG` is ignored (`config::init_logging` in `src/config.rs`).
Selection is `-c <file>` first, then nonempty `LTD_CONFIG`, then built-in
defaults; the shipped service sets `LTD_CONFIG=/etc/linux-trackpad-driver/config.toml`,
so a bare `sudo ltd run` without `-c`/`LTD_CONFIG` runs at defaults.

Service path (uses the installed file):

```sh
sudo sed -i 's/^log_level *=.*/log_level = "debug"/' /etc/linux-trackpad-driver/config.toml
grep -E '^log_level' /etc/linux-trackpad-driver/config.toml  # expect debug
sudo systemctl restart linux-trackpad-driver.service
journalctl -u linux-trackpad-driver.service 2>&1 | tee /tmp/ltd-run.log
```

Manual-run path (same file, explicit selection):

```sh
grep -E '^log_level' /etc/linux-trackpad-driver/config.toml  # expect debug
sudo ltd run -c /etc/linux-trackpad-driver/config.toml 2>&1 | tee /tmp/ltd-run.log
```

A listed socket (`ls /run/linux-trackpad-driver/driver.sock`, `ltd doctor`
socket line) only observes a listener — it does not prove the proxy attached.

Replace `/dev/hidrawN` / `/dev/input/eventX` with your nodes throughout.

## 0. Pre-flight

- [ ] Build from this worktree is installed:

```sh
cargo build --release
sudo install -m755 target/release/ltd /usr/bin/ltd
ltd --help
```

- [ ] udev + systemd units installed per [`INSTALL.md`](INSTALL.md),
      daemon enabled:

```sh
systemctl status linux-trackpad-driver.service
udevadm monitor --udev --subsystem-match=hidraw &
# replug trackpad, confirm an add event fires, then kill monitor
```

- [ ] Units (unprivileged log peek) installed; socket exists while running:

```sh
ls -l /run/linux-trackpad-driver/driver.sock
```

- [ ] Record device IDs. USB:

```sh
lsusb -d 05ac:0265 -v 2>/dev/null | head -30
lsusb -d 05ac:0324 -v 2>/dev/null | head -30
cat /sys/class/hidraw/hidraw*/device/uevent | grep -E 'HID_ID|HID_NAME'
```

BT:

```sh
bluetoothctl show
bluetoothctl devices Paired
bluetoothctl info <MAC>
```

- [ ] Note which hidraw nodes are the trackpad vs siblings:

```sh
for n in /sys/class/hidraw/hidraw*; do
  echo "== $n"; cat "$n/device/uevent" 2>/dev/null | grep -E 'HID_ID|HID_NAME'
done
ls -l /dev/hidraw*
```

- [ ] Note the libinput event node(s) for the trackpad:

```sh
sudo libinput list-devices | grep -A8 -i 'trackpad\|05ac\|004c'
```

Expected: one pointer + touch device per physical trackpad, no `ltd`
uinput proxy node yet (`ATTR{name}` match only matters once the
proxy lands; see trap note in [`INSTALL.md`](INSTALL.md)).

## 1. Baseline WITHOUT driver (do this first, every session)

Stop everything driver-owned, then characterize stock behavior.
This is the "did we regress passthrough" reference.

```sh
sudo systemctl stop linux-trackpad-driver.service
sudo pkill -f 'ltd run' || true
ls /run/linux-trackpad-driver/driver.sock 2>&1 || echo "socket gone (good)"
```

- [ ] Pointer moves, 1-finger click works, 2-finger right-click works,
      2-finger scroll works under plain libinput.
- [ ] Record baseline event flow (10 s circular finger motion, count rate):

```sh
sudo evtest /dev/input/eventX > /tmp/evtest-baseline.log 2>&1 &
 sleep 10; sudo kill %1; wait 2>/dev/null
grep -c 'ABS_MT_POSITION' /tmp/evtest-baseline.log
grep -m2 'Testing\|Input device' /tmp/evtest-baseline.log
```

Note the count in the results table (§9). Repeat the same motion
after init (§2) — rate should match or rise to the polling cap,
never drop.

- [ ] Record baseline quirks state (paste back if clicks misclassify later):

```sh
sudo libinput quirks list /dev/input/eventX
```

## 2. USB init matrix (passthrough baseline: set `enhanced_clicks = false`)

Shipped default is `enhanced_clicks = true`, so set the gate off explicitly
for this section: init only, no grab, no proxy. Set + verify with:

```sh
sudo sed -i 's/^enhanced_clicks *=.*/enhanced_clicks = false/' /etc/linux-trackpad-driver/config.toml
grep -E '^enhanced_clicks' /etc/linux-trackpad-driver/config.toml  # expect false
```

### 2a. `run` from cold plug

- [ ] Unplug USB. Start daemon (debug config selected explicitly), then plug:

```sh
sudo ltd run -c /etc/linux-trackpad-driver/config.toml 2>&1 | tee /tmp/ltd-usb-run.log
# needs log_level = "debug" in that file (see top of this checklist)
```

- [ ] Expect in log (good-lines, see §7):

```text
lock acquired at /run/linux-trackpad-driver/driver.sock
config: polling 110 Hz, enhanced_clicks=false
found 1 trackpad node(s)
USB init: multitouch 02 01, host-click 21 01, polling [99 F2 23] (requested 110 Hz → effective 110 Hz, device cap ~110 Hz)
USB init on /dev/hidrawN accepted (iface ..., polling [99 F2 23])
```

- [ ] Pointer + multitouch work immediately (2-finger scroll, 3-finger gestures
      as configured in the compositor). No duplicate cursors.

### 2b. `reconnect` on live daemon (udev path)

- [ ] With daemon running, force the udev entry manually:

```sh
sudo ltd reconnect /dev/hidrawN 2>&1 | tee /tmp/ltd-reconnect.log
systemctl status 'linux-trackpad-driver-reconnect@hidrawN.service' || true
```

- [ ] Expect same `USB init ... accepted` line, no error. Input keeps working.

### 2c. Replug while daemon runs (tick + udev race)

- [ ] Unplug 3 s, replug, wait ≤ 35 s (daemon tick is 30 s + udev `add` path).
      Input returns without restarting the daemon.
- [ ] Log shows either the udev `reconnect` init or
      `resume tick: initialized newly appeared /dev/hidrawN`, at most once
      per replug — no per-second re-init storm.

### 2d. Polling verify (rate measurement)

Same 10 s circular motion as §1, on the same `/dev/input/eventX`:

```sh
sudo evtest /dev/input/eventX > /tmp/evtest-110hz.log 2>&1 &
 sleep 10; sudo kill %1; wait 2>/dev/null
grep -c 'ABS_MT_POSITION' /tmp/evtest-110hz.log
awk '/Event: time/{print $3}' /tmp/evtest-110hz.log | head -3
```

- [ ] Event count ≈ baseline or higher, consistent with ~110 Hz
      (`99 F2 23`, `3850/35 ≈ 110`). Requests above 110 are clamped,
      never sent — verify cheaply:

```sh
sudo ltd reconnect /dev/hidrawN -f -p 500 2>&1 | grep -iE 'clamp|polling|accepted'
# expect: "requested 500 Hz above device cap; clamped to ~110 Hz (X=0x23)"
# wire byte stays 0x23, never below 0x23
```

- [ ] Read-back verify path exercised: with `log_level = "debug"` in the
      selected config, look for
      absence of `polling verify mismatch` / `verify unreadable` warnings
      on a healthy device. Either warning once is fine; repeating every
      30 s tick is a finding (paste back, §8).

### 2e. Disengage path

```sh
sudo ltd reconnect /dev/hidrawN -d 2>&1 | tee /tmp/ltd-disengage.log
# expect: "USB disengage: host-click 21 00"
```

- [ ] Passthrough continues, daemon still owns the socket.

## 3. BT matrix

USB-first scope: BT is bring-up + init verification, no rate tuning.

- [ ] Pair via `bluetoothctl` (`scan on`, `pair <MAC>`, `connect <MAC>`),
      find the BT hidraw node (`HID_ID=0005:0000004C:...` in the §0 loop).
- [ ] `run` with only BT connected. Expect, and nothing else on the wire:

```text
BT /dev/hidrawN: single-node path, no init dance — device ready
```

(no `02 01` / `21 01` / `99 F2` reports on BT, ever).

- [ ] With `enhanced_clicks=false`: pointer works via kernel
      passthrough. Log shows `proxy: gate off, passthrough` at debug level;
      both hidraw and physical evdev nodes remain present.
- [ ] Double-input check: slow diagonal drag in `evtest` on the physical
      node — each physical motion produces exactly one event stream.
      Duplicated streams = finding (paste `evtest` excerpt + `dmesg` tail).
- [ ] With `enhanced_clicks=true`: the proxy attaches per device `Ready`
      (see README Troubleshooting + `docs/CONFIG.md` gate semantics).
      Check for `proxy: grabbed` after source/virtual pointer setup. The
      physical evdev node and hidraw node must remain present: suppression
      uses a reversible evdev grab, never a driver unbind.
- [ ] Stop the daemon: native input resumes after its grab closes. With
      the daemon stopped, run BT `reconnect` (including `-f`) and `resume`:
      native input remains available, and driver bindings stay intact.
- [ ] With the enhanced gate off, restart the daemon and repeat reconnect
      and resume: native input remains available throughout. Bluetooth is
      still experimental; record model/kernel/compositor and results for #42.

## 4. Click-proxy matrix (`enhanced_clicks = true`)

Gate on, daemon restarted. Proxy attaches per `Ready`; failed attachment
leaves native input available on USB and BT. Neither init nor the one-shot
helpers unbind the Bluetooth driver (see README Troubleshooting).

```sh
sudo sed -i 's/^enhanced_clicks *=.*/enhanced_clicks = true/' /etc/linux-trackpad-driver/config.toml
grep -E '^enhanced_clicks' /etc/linux-trackpad-driver/config.toml  # expect true
sudo systemctl restart linux-trackpad-driver.service
journalctl -u linux-trackpad-driver.service -f | tee /tmp/ltd-proxy.log
```

- [ ] Attach line present: `proxy: grabbed /dev/hidrawN (uinput virtual node up)`.
      A new uinput node appears in `sudo libinput list-devices`.
- [ ] Two-node classification (owner hardware gate — merges wait on this):
      find both virtual nodes (`Linux Trackpad Driver` + `Linux Trackpad
      Driver Extras`) via `sudo libinput list-devices`, then check each
      node's `Handlers=` line in
      `/proc/bus/input/devices`:
      - pointer node (`Linux Trackpad Driver`): `Handlers=` shows
        `mouseN eventM` with NO `kbd` — this is the regression pin
        (a `kbd` there means keyboard codes leaked back onto the pointer
        node and the cursor will stay dead);
      - extras node (`Linux Trackpad Driver Extras`): keyboard outlet,
        `kbd` there is expected and harmless.
      Then: cursor moves on finger motion (pointer node), and a mapped
      combo (e.g. `click_1 = "ctrl+c"`) still types (extras node).
- [ ] Gate-off fallback (sanity): set gate back to `false`, restart —
      expect `proxy: gate off, passthrough for /dev/hidrawN`, no grab,
      pointer works. Then restore `true` and restart before continuing:

```sh
sudo sed -i 's/^enhanced_clicks *=.*/enhanced_clicks = true/' /etc/linux-trackpad-driver/config.toml
sudo systemctl restart linux-trackpad-driver.service
```
- [ ] 1–5 finger actions with defaults
      (`click_1=left click_2=right click_3=middle click_4/5=none`):
      press with N fingers over `sensitivity`, confirm action per row in §9.
      `none` = swallowed, no action, no haptic.
- [ ] Hysteresis feel: press just over threshold, ease off slowly —
      release fires below press point (`sensitivity - margin`), no chatter
      at the edge. Try `margin = 0` (chattery, expected) vs `margin = 10`
      (sticky release, expected), then restore `margin = 5`.
- [ ] Haptic levels: `haptic = 0` (silent click), `2` (default),
      `40` (strong). Each press still emits its action; only feel changes.
- [ ] `quiet_click = true`: softer waveform at same strength.
- [ ] Force levels: set `force_offset = 30`, `force_1 = "middle"` (rest `none`).
      Light press → standard action; press harder → force action + second
      haptic (`force_haptic` unset = reuse `haptic`). Release fully clears both.
- [ ] Grab release on stop: `sudo systemctl stop linux-trackpad-driver.service`
      → `proxy: releasing grab (...)` + `shutdown complete`; physical
      passthrough resumes, no stuck keys (type in a terminal immediately).
- [ ] LibreOffice '-' capture (owner bug stays OPEN until this lands):
      with the proxy attached, reproduce clicks typing '-' in any LO app
      while recording the PROXY node (find it via `sudo libinput
      list-devices`, match the `ltd` uinput name), then paste the
      offending `EV_KEY` line(s):

```sh
sudo evemu-record /dev/input/eventX > /tmp/evemu-lo-minus.log 2>&1 &
# click in Writer until '-' appears, then: sudo kill %1
grep -E 'EV_KEY.*(value 1|pressed)' /tmp/evemu-lo-minus.log | sort | uniq -c | sort -rn | head -20
# and/or: sudo libinput debug-events --device /dev/input/eventX > /tmp/libinput-lo-minus.log 2>&1 &
```

      Expected once fixed: no `KEY_MINUS` (code 12) or any other key
      press attributable to a click. Until that capture names the actual
      offending code, the cause is UNPROVEN (`BTN_LEFT` = 272 cannot
      produce `KEY_MINUS` = 12; a wrong key-table mapping cured by the
      F-key audit is one hypothesis, not a finding). Paste the top-20
      table + 10 raw lines around the first '-' as the finding (§8).
- [ ] Proxy-never-swallows rule: kill the pump path if testable, or unplug
      mid-press — held keys release, pointer returns within one tick.

## 5. Suspend/resume

Primary path is the system-sleep hook (`ltd resume` on `post`); the
30 s daemon tick is the backstop. Either suffices alone.

```sh
# hook installed per INSTALL.md; manual drill without sleeping:
sudo ltd resume 2>&1 | tee /tmp/ltd-resume.log
```

- [ ] Expect `resume re-init: re-sending multitouch enable first ...`
      then `resume: /dev/hidrawN re-init accepted (iface ..., polling [99 F2 23])`
      and `resume: pass done (1 re-init, 0 up-to-date, 0 errors)`
      (or `up-to-date` when nothing drifted — both pass).
- [ ] Real sleep: `systemctl suspend`, wait 15 s, wake. Within ~35 s:
      multitouch works (2-finger scroll = the dropout canary; clicks
      surviving while motion is dead = the §6 RESEARCH regression).
- [ ] No storm: `journalctl -b -u linux-trackpad-driver.service | grep -ciE 're-init|mismatch|unreadable'`
      stays small (single digits). Re-init every 30 s tick forever = finding.
- [ ] BT after resume: pointer works; log shows `resume: BT ... policy re-applied`
      at most once per pass, no `Ready` spam.

## 6. Failure drills (expect graceful, never panic)

| Drill | Command | Expect |
|---|---|---|
| kill -9 daemon | `sudo pkill -9 -f 'ltd run'; sudo ltd run` (restart) | socket re-acquired, init re-runs, no stale-socket error beyond one `removing stale socket` warn |
| unplug mid-grab (proxy on) | unplug during press | `Detached` path, `proxy: releasing grab`, no stuck keys, replug re-inits |
| permission-denied run | `LTD_SOCKET=$XDG_RUNTIME_DIR/ltd/driver.sock ltd run` as user w/o hidraw rw | clean `HIDIOCSFEATURE ... permission denied` error + nonzero exit, no panic; `getfacl /dev/hidrawN` pasted back |
| unknown node | `sudo ltd reconnect /dev/hidraw999 -f` | `not a recognized trackpad` error, nothing sent to hardware |
| below-floor rate | `sudo ltd reconnect /dev/hidrawN -f -p 5` | rejected (`below device floor ~16 Hz`), init untouched |

## 7. What-good-looks-like (log excerpts)

Healthy USB `run` (debug):

```text
lock acquired at /run/linux-trackpad-driver/driver.sock
config: polling 110 Hz, enhanced_clicks=false
found 1 trackpad node(s)
USB init: multitouch 02 01, host-click 21 01, polling [99 F2 23] (requested 110 Hz → effective 110 Hz, device cap ~110 Hz)
USB init on /dev/hidraw2 accepted (iface Some(1), polling [99 F2 23])
```

Healthy BT `run`:

```text
BT /dev/hidraw5: single-node path, no init dance — device ready
```

Healthy resume one-shot:

```text
resume re-init: re-sending multitouch enable first (21→8 byte dropout fix)
resume: /dev/hidraw2 re-init accepted (iface Some(1), polling [99 F2 23])
resume: pass done (1 re-init, 0 up-to-date, 0 errors)
```

Healthy proxy attach/detach:

```text
proxy: grabbed /dev/hidraw2 (uinput virtual node up)
proxy: releasing grab (/dev/hidraw2)
shutdown complete
```

Smells (paste back as findings, §8): `polling verify mismatch` repeating,
`verify unreadable` every tick, `init failed for ...: {EACCES|ENOTTY|EINVAL}`,
`resume tick: re-init failed`, double `Ready` without `Detached` between,
any panic backtrace (must never happen — file as P0).

## 8. What to paste back as findings

One block per failing/odd row in §9. `RUST_LOG` is ignored — the log
excerpt below only exists when the run used `log_level = "debug"` in the
selected config (see top of this checklist). Name the selected file and how
it was selected (`-c <file>` vs service `LTD_CONFIG` vs defaults); a socket
line alone does not prove the proxy attached.

```sh
# 1. build identity + config source (selected file, not just its contents)
ltd --version; uname -r; libinput --version
ltd doctor  # or: ltd doctor -c <same file used for the run>
echo "LTD_CONFIG=$LTD_CONFIG"
cat /etc/linux-trackpad-driver/config.toml
# 2. device identity + transport
lsusb -d 05ac:0265; lsusb -d 05ac:0324
bluetoothctl info <MAC> 2>/dev/null | head -20
cat /sys/class/hidraw/hidrawN/device/uevent
# 3. kernel view
dmesg | tail -40
sudo udevadm info -a /dev/hidrawN | head -60
# 4. rates
wc -l /tmp/evtest-baseline.log /tmp/evtest-110hz.log
# 5. debug logs: startup identity + selected source/evdev/actuator + session state
#    (trim to the interesting 50 lines; keep the `starting`, `config:`, `found`,
#    `evdev:`/`haptic: ... actuator`, `USB init`/`BT`, `proxy:` lines intact)
grep -E 'starting|config:|found .* trackpad|evdev:|actuator|USB init|BT |resume|proxy|polling|verify|failed|error' /tmp/ltd-run.log | tail -50
```

Unknown behaviors (new PID, new report bytes, new dropout shape) additionally
need a raw capture — `hid-recorder` (Arch: `hid-tools` package):

```sh
sudo hid-recorder > /tmp/hid-recorder-mt2.txt 2>&1
# wiggle + click with 1..3 fingers for ~15 s, Ctrl-C, attach the file
```

Do NOT paste full `evtest` logs inline (they're huge) — attach counts +
first 3 timestamps (§2d) plus any anomalous excerpt only.

## 9. Results table template (owner fills in)

Copy into the PR/issue reply, one row per check.

| # | Check | Expected | Result | Notes (counts, log line, excerpt) |
|---|---|---|---|---|
| 0a | build + install | `ltd --help` prints usage | | |
| 0b | udev add fires | monitor shows hidraw add | | |
| 0c | IDs recorded | VID:PID + hidrawN + eventX | | `<paste>` |
| 1a | baseline moves/clicks/scroll | works w/o driver | | |
| 1b | baseline rate | count = ___ | | |
| 2a | USB run init | `02 01 / 21 01 / 99 F2 23` accepted | | |
| 2b | reconnect live | init accepted, input kept | | |
| 2c | replug ≤35 s | input returns, no storm | | |
| 2d | 110 Hz rate | count = ___ (≈/≥ baseline) | | |
| 2d2 | -p 500 clamp | warn, wire stays 0x23 | | |
| 2e | disengage | `21 00`, passthrough kept | | |
| 3a | BT bring-up | no-init-dance Ready | | |
| 3b | BT passthrough | pointer works, no unbind | | |
| 3c | BT no double-input | single stream | | |
| 4a | proxy attach | `grabbed ... uinput up` | | |
| 4b | click_1..5 | L/R/M/none/none | | |
| 4c | hysteresis | clean edge, margin 5 | | |
| 4d | haptic 0/2/40 | silent/default/strong | | |
| 4e | quiet_click | softer waveform | | |
| 4f | force level | 2nd action + haptic, clean release | | |
| 4g | stop releases grab | passthrough, no stuck keys | | |
| 4h | LO '-' capture (bug OPEN) | evemu top-20 + offending EV_KEY, or clean | | |
| 5a | `ltd resume` manual | re-init accepted, pass done | | |
| 5b | sleep+wake MT | 2-finger scroll survives | | |
| 5c | no log storm | single-digit re-init lines | | |
| 6a | kill -9 restart | re-acquire + re-init | | |
| 6b | unplug mid-grab | release, replug heals | | |
| 6c | user-run perms | clean EACCES, no panic | | |
| 6d | unknown node | rejected, nothing sent | | |
| 6e | -p 5 rejected | below-floor error | | |

## Build identity regression coverage

`cargo test --test build_identity` builds dependency-free temporary Cargo
projects with the real build script. It covers linked worktrees, commit and
staged/unstaged/untracked dirty transitions in the same target directory,
archive isolation from parent repositories, missing Git, explicit channel /
commit / nightly date metadata, invalid inputs, and incremental overrides.
`tests/cli.rs` runs `--version` and `-V` outside the checkout with no Git in
`PATH` and invalid/missing config; doctor identity is checked without hardware
by the report-rendering unit test. `packaging/arch/check-aur.sh` checks the
stable source archive's embedded channel during its offline build.
Checkout fixture tests skip when Git is unavailable; archive tests still run.
