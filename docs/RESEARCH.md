# Research — input-stack + haptic-trackpad survey (2026-10-14)

Two parallel research threads. Sources inline. `[SPECULATION]` marks unverified
claims. Validates / corrects the L0/L1/L2 tier model in ARCHITECTURE.

## 1. Standards baseline

- Digitizer page `0x0D`: per-contact `Tip Pressure (0x30)` = force path. MS PTP builds on this.
- Haptics page `0x0E` (HUTRR63): `Simple Haptic Controller 0x01`,
  `Waveform List 0x10`, `Auto Trigger 0x20`, `Manual Trigger 0x21`,
  `WAVEFORM_PRESS 0x1006`, `WAVEFORM_RELEASE 0x1007`, vendor `0x2001–0x2FFF`.
  Spec: https://usb.org/sites/default/files/hutrr63b_-_haptics_page_redline_0.pdf
- Linux 6.18: `drivers/hid/hid-haptic.c` + `hid-multitouch` haptic glue.
  Device-controlled default → host-controlled after userspace uploads
  `WAVEFORM PRESS/RELEASE`. Signals `INPUT_PROP_HAPTIC_TOUCHPAD`,
  `INPUT_PROP_PRESSUREPAD`. First target: Elan 2703. Google/ChromeOS-led.
  https://github.com/torvalds/linux/blob/master/drivers/hid/hid-haptic.c ,
  https://lwn.net/Articles/1034290 , https://lwn.net/Articles/1029836 ,
  https://www.phoronix.com/news/Linux-6.18-HID

## 2. Vendor survey

### Apple Magic Trackpad 2 (Lightning + USB-C)
Force Touch, BT + wired. https://support.apple.com/en-us/111884 ,
https://support.apple.com/en-us/121932
- Pressure: proprietary vendor report, not `0x0D/0x0E`. In-kernel
  `hid-magicmouse.c` decodes per-contact `tdata[7]` → `ABS_MT_PRESSURE`
  (range 0..253, `res=0` = arbitrary units, NOT grams), plus
  touch_major/minor, size, orientation.
  https://github.com/torvalds/linux/blob/master/drivers/hid/hid-magicmouse.c ,
  https://github.com/mr-cal/Linux-Magic-Trackpad-2-USB-C-Driver/blob/main/README.md
- Haptic: no standard `0x0E` controller. Reverse-engineered proprietary reports:
  persistent config `0x22/0x23`, one-shot actuator output `0x53`.
  2026 patch (tested `05ac:0324`, Ubuntu 6.17, unmerged upstream):
  https://lkml.iu.edu/2605.1/06157.html ; Windows RE base:
  https://github.com/vitoplantamura/MagicTrackpad2ForWindows
- USB-C PID `05ac:0324` (BT `004c:0324`): kernel quirk bug 219470, fixed in
  LTS 6.1.120+ / 6.6.66+ / 6.12.5+ / 6.13+.
  https://bugzilla.kernel.org/show_bug.cgi?id=219470
- libinput needs `AttrPressureRange` / `AttrTouchSizeRange` quirk. Click stays
  in-device haptic; no host waveform control in mainline.

### Apple MacBook internal pads (Taptic Engine)
Post-2015 = proprietary SPI (`applespi`), older = `bcm5974`/`appletouch` USB.
https://github.com/cb22/macbook12-spi-driver ,
https://www.phoronix.com/news/Linux-5.3-MacBook-Key-Trackpads
Force + actuation handled in-device/firmware. No `0x0D` pressure nor `0x0E`
haptic exposed to host. Off-HID entirely — out of scope for this driver.

### Lenovo ThinkPad haptic (Sensel)
X1 Carbon Gen12/Gen13 Aura, X1 2-in-1 Gen9/10, Z13 Gen2 = Sensel capacitive +
force + haptics.
https://download.lenovo.com/manual/thinkpad_x1_carbon_gen13/user_guide/en/Use_the_Haptic_Touchpad_Trackpad.html ,
https://sensel.com/product
- Windows = MS PTP over I2C-HID. Linux tool drives proprietary HID report
  `0x09` register pipe (force 10–500 g, hysteresis, zones, intensity).
  https://github.com/glutamatt/sensel-touchpad-linux
- No evidence of standard `0x0E` exposure on shipping firmware.
  Device-autonomous click; host sees pointer only. Suspend/resume breakage
  reported; FW updates Windows-only.

### Dell XPS seamless haptic / HP Spectre-EliteBook / ASUS ErgoSense
- Dell XPS 13/14/16 seamless glass: PTP path standard, no public HID desc,
  no `0x0E` claim. Pointer works, no force tuning on Linux.
- HP Spectre x360 / EliteBook haptic: Windows PTP presumed, Synaptics/ELAN
  supply; Linux needs ACPI fixes (≥6.7), haptic tuning absent.
  https://github.com/jmole/hp-spectre-linux/blob/main/README.md
- ASUS ErgoSense (Zenbook/Vivobook): mechanical, NOT haptic. ELAN/PTP over
  I2C-HID, contact area only. L0.
- Framework 13/16 shipping pads (`PIXA3854`): verified NO `ABS_PRESSURE` /
  `ABS_MT_PRESSURE` — L0 by design.
  https://community.frame.work/t/framework-16-touchpad-pressure/79464
- Framework 13 Pro (2026, piezo + Boréas): HID descriptor unpublished;
  `hid-recorder` dump needed before tiering. Treat as unknown.
  https://frame.work/blog/introducing-framework-laptop-13-pro

## 3. Linux stack status

- **Kernel `hid-multitouch` / `hid-apple`:** Touch-Bar quirks queued 6.17;
  Apple Silicon dockchannel-HID (`apple,t8122/t8112-dockchannel-hid`) in
  `hid.git for-next`, not stable — don't depend on it.
  Init-byte precedent: MT2 USB `[0x02,0x01]`; MT2 BT / Mouse2 `[0xF1,0x02,0x01]`;
  Wellspring mode-switch via USB control. Userspace equiv = hidraw
  `HIDIOCSFEATURE`. EIO-after-switch on some devices = treat as success +
  delayed retry. https://github.com/torvalds/linux/blob/master/drivers/hid/hid-magicmouse.c
- **libinput 1.31–1.32:** no host-haptic click synthesis. `INPUT_PROP_PRESSUREPAD`
  (renamed from `HAPTIC_TOUCHPAD` Nov 2025) = buttonless force/haptic pad;
  `+FF_HAPTIC` only if userspace-controllable. Kernel 6.18 adds
  `CONFIG_HID_HAPTIC` + `FF_HAPTIC 0x4f`. Pressure semantics: `res==0` →
  arbitrary size-proxy; `res!=0` → units/gram true force. Wrong `res` + missing
  quirk → palm/thumb misclassification. Per-model quirks via
  `AttrPressureRange`, `AttrPalmPressureThreshold`, hwdb overrides.
  https://wayland.freedesktop.org/libinput/doc/latest/device-quirks.html
- **evdev MT:** MT-B slots (`ABS_MT_SLOT`, `input_mt_report_slot_state`,
  `input_mt_sync_frame`); must emit `BTN_TOUCH` or `ABS_PRESSURE` or final
  `SYN_REPORT` is dropped. Keep `res=0` on virtual device unless shipping a
  libinput quirk + true-gram calibration.
  https://docs.kernel.org/input/multi-touch-protocol.html
- **uinput click proxy:** stable (`UI_DEV_SETUP` + `UI_ABS_SETUP` + `UI_DEV_CREATE`,
  libevdev wrapper recommended). Emits MT slots + `BTN_LEFT/BTN_TOUCH` + `SYN`.
  Gap: no evidence libinput/Wayland consumes `FF_HAPTIC` from uinput in 2026 —
  do NOT promise host-triggered Taptic actuation via uinput. Physical click
  stays device-controlled unless Apple host-trigger is proven on hardware.
  https://docs.kernel.org/input/uinput.html
- **hidraw:** `HIDIOCSFEATURE`/`HIDIOCGFEATURE` stable since 2011, buf[0] =
  report ID. Kernel driver already sends same SET_REPORT on probe → userspace
  must order AFTER probe + resume/replug, needs node discovery + root/`hidraw`
  perms. BT HIDP hidraw exclusivity vs driver binding less documented.
- **systemd/udev:** identity via `60-input-id.rules` + `70-touchpad.hwdb`;
  libinput 1.32 prefers `ID_INTEGRATION` (systemd ≥v260). uinput caveat
  (systemd #37063): virtual `devices/virtual/input/...` has no parent
  `ATTRS{idVendor}` — match by `SUBSYSTEM==input` / `ATTRS{name}` /
  `ENV{ID_INPUT_*}` or set phys/vendor at create. Daemon pattern: rule sets
  `TAG+="systemd"` + `SYSTEMD_WANTS`, never long-run in `RUN`.
  https://github.com/systemd/systemd/issues/37063
- **Suspend/resume:** re-init REQUIRED — feature/mode state volatile
  (bcm5974 re-switches on open; magicmouse re-sends on probe). Userspace must
  watch hidraw add/remove + udev `changed` + sleep hooks, re-open +
  re-`SET_FEATURE` with backoff. No kernel persistence for userspace reports.

## 4. Tier-model verdict (CORRECTION ADOPTED, confirmed by T3 deep survey)

Base L0/L1/L2 holds as shorthand but needs splits. Adopted tiers:

| Tier | Pressure | Haptic | Members |
|------|----------|--------|---------|
| L0 | none (binary click, area-only) | none | Framework 13/16-shipping, ASUS ErgoSense |
| L1a | area-proxy (`res=0`) | none | most PTP I2C-HID pads |
| L1b | true force (grams/N, `res!=0`) | none | calibrated pads, Sensel `0x09` path |
| L2s | `Tip Pressure` + standard `0x0E` | host-controlled | Elan 2703 / kernel 6.18 `hid-haptic` |
| L2p | proprietary pressure | device-autonomous | Apple MT2 (`0x22/0x23+0x53`), Sensel shipping FW |

Rules derived:
- Apple MT2 = L2p (pressure channel is L1a-class arbitrary units, NOT L1b
  grams). Internal MacBooks off-HID (out of scope).
- Lenovo/Dell/HP haptic hardware ships as L1-or-L0 software on Linux
  (device-autonomous click, no host waveforms, no force units to userspace).
- Do NOT conflate L1a/L1b — thresholds break. Do NOT gate host-haptic UI on
  `ABS_MT_PRESSURE` alone; gate on `INPUT_PROP_HAPTIC_TOUCHPAD`/`PRESSUREPAD`
  + `0x0E` waveforms + physical units + transport (HID vs SPI) + mode
  (device- vs host-controlled).
- `TouchSource` trait: add `pressure_units()` (arbitrary vs grams) alongside
  `tier()` so L1a/L1b is machine-checked. ARCHITECTURE.md amended: tiers + `pressure_units()` adopted (see `src/apple_parse.rs`).

## 5. Plan verdicts (per-thread)

- USB feature-report init: SUPPORTS plan. Mirrors `magicmouse_enable_multitouch()`
  + `bcm5974_wellspring_mode()`. `[SPECULATION]`: exact host-click/polling bytes
  per Apple PID need `hid-feature` + usbmon capture; don't reuse MT2 bytes blindly.
- BT single-node path: PLAUSIBLE, WEAKER. Kernel treats BT separately;
  BlueZ+HIDP+hidraw exclusivity and reconnect timing unverified in 2025–2026
  sources. Needs prototype + `btmon`/hidraw trace.
- evdev-grab + uinput proxy behind opt-in default-off: SUPPORTS with caution.
  `EVIOCGRAB` suppresses duplicate libinput events; risks: hides native quirks
  unless re-implemented, `input`-group perms, Wayland re-enumeration race,
  resume re-grab. Opt-in default-off confirmed correct.

## 6. T3 deep-survey deltas (2026-10-14, threads `research-haptic`, `research-kernel`)

Full reports live in the owning threads' final messages. Load-bearing deltas
merged here; tier verdict + plan verdicts below supersede §4–§5 where they differ.

### Polling posture CORRECTED (was §5 "request 500 + clamp")
Independent USB-C measurement: `0x99` = `99 f2 XX`, `≈3850/XX` Hz, max tested
`XX=0x23` → ~111 Hz; no higher device mode demonstrated; table beyond `XX=35`
unverified (may NAK/ignore). Posture: request CAPPED value (~110), clamp to
device max, verify by measurement — never 500 Hz. BT equivalent unknown.
https://github.com/benjaminwestern/linux-apple-trackpad ,
https://github.com/mr-cal/Linux-Magic-Trackpad-2-USB-C-Driver/issues/26
ARCHITECTURE amended; the init owner was notified in-flight.

### hidraw ordering: defer-retry-verify (new detail)
Kernel probe sends its own init; userspace `HIDIOCSFEATURE` at `add` races it.
Watch udev `add`/`change` + `bind`, wait for node + driver bind, backoff-retry,
verify via `HIDIOCGFEATURE`, re-apply on resume. Precedent: July 2026 kernel
`.reset_resume` workqueue patch (inline too early).
https://patchew.org/linux/20260725203446.35001-1-ckhordiasma@gmail.com

### Resume regression (load-bearing, new)
Trackpad 2 USB drops multitouch across power transition (report `0x02`
21→8 bytes; clicks survive, motion lost until replug). Fix = re-enable
multitouch from `.reset_resume` via workqueue. Userspace `reconnect` + resume
hook MUST re-send multitouch enable. Same patch link.

### BT: no exclusivity (REFUTED exclusivity assumption)
hidraw coexists with `hid-magicmouse` on BT; kernel keeps emitting evdev unless
handled (unbind driver for VID:PID, or evdev-grab physical node behind opt-in
flag), else double-input. https://docs.kernel.org/hid/hidraw.html
Scope BT to bring-up + init verification first (USB-first for rate tuning).

### uinput contract (hardened)
Proxy node: `res==0`, arbitrary 0..253, Type-B slots + tracking IDs, NO
grams/newtons claim, NO `EV_FF`/`FF_HAPTIC` advertisement (no 2026
libinput/Wayland consumer; series unmerged Jul–Oct 2025 Denose/Czubak).
https://lwn.net/Articles/1034290 ,
https://www.spinics.net/lists/kernel/msg5810859.html

### udev trap (confirmed)
systemd#37063: `ATTRS{idVendor}` never matches uinput event nodes. Match by
`ATTRS{name}`/`ATTR` on `input` parent or `SUBSYSTEM=="input", ATTR{name}`.
https://github.com/systemd/systemd/issues/37063

### Sleep hooks (new constraint)
`/usr/lib/systemd/system-sleep/` executables run `pre/post`; `user.slice`
frozen during hooks → re-init must be system-side, no user-session IPC.
https://www.freedesktop.org/software/systemd/man/latest/systemd-suspend.service.html

### Tier verdict (confirmed + extended)
L0/L1a/L1b/L2s/L2p HOLDS. Add **mode axis** (device- vs host-controlled) and
**transport axis** (HID vs SPI): Apple internal pads off-HID; Dell XPS proves
L2p silicon can expose L0/L1 software on Linux (Synaptics manual-trigger vs
kernel auto-trigger-only gap → userspace daemon workaround exists:
https://github.com/basecamp/omarchy/pull/5534). Framework 13 Pro (BOS1921):
likely L2p via HID feature reports (`framework_tool --haptic-intensity`,
PR FrameworkComputer/framework-system#332); `hid-recorder` dump still needed.
Gate host-haptic UI on `PRESSUREPAD + FF_HAPTIC + 0x0E waveforms + units`,
never on `ABS_MT_PRESSURE` alone.

### Per-plan-element verdicts (final, supersede §5 prose)
- (a) USB feature-report init — SUPPORTS (defer-retry-verify).
- (b) BT single-node — PLAUSIBLE-WEAKER (no exclusivity; USB-first).
- (c) evdev-grab + uinput proxy, opt-in default-off — SUPPORTS (res==0, no FF).
- (d) udev/systemd — PLAUSIBLE-WEAKER (#37063 trap; match by name/ATTR).
- (e) suspend/resume re-init — SUPPORTS (system-side, re-send enable; precedent).
