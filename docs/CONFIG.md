# Configuration reference

Source truth: `src/config.rs` (`Config`, `Config::validate`, `parse_click_action`),
`src/clicks.rs` (thresholds, force level), `src/init.rs` (`resolve_polling`),
`src/daemon.rs` (startup snapshot, no live reload), `src/proxy.rs`
(`ProxySettings`, per-frame `&Config`). Shipped defaults:
`config/default.toml` (= `Config::default()`).

## Loading

- Explicit `-c <path>` wins, else `$LTD_CONFIG`, else built-in defaults
  (`load_or_default_with_env`; a missing env-pointed file falls back to
  defaults, a missing explicit `-c` file errors).
- Omitted keys take their defaults; **unknown keys are rejected**
  (`deny_unknown_fields` — usually a typo, e.g. `sensitivty`).
- Present-but-invalid values fail load with the key + accepted range named.

## Live vs restart

**All keys need a restart** (`systemctl restart linux-trackpad-driver`, or
restart `ltd run`). There is no live reload: the `enhanced_clicks` gate is
evaluated from the startup config (`src/daemon.rs`, `subscribe_click_proxy`),
and the proxy carries a `ProxySettings` snapshot taken at startup
(`src/proxy.rs`). Per-frame tuning reads (e.g. `speed`) read the startup
snapshot, not the file — editing the TOML mid-run changes nothing until
restart.

## Key reference

| key | type | range | default | effect |
| --- | --- | --- | --- | --- |
| `log_level` | string enum | `debug\|info\|warn\|error` | `"info"` | Logging filter. Anything else fails validation. |
| `polling_rate` | integer (Hz) | `1..=110` request; **effective floor 16** (see below) | `110` | Requested USB polling rate. Capped posture: default is the demonstrated device max (~110 Hz, `99 F2 X` with `X = 0x23 = 35`, `3850/35 ≈ 110`). Values `> 110` are rejected by config validation; the init layer additionally clamps runtime overrides, so `X >= 0x23` is always what hits the wire. Requests `< 16` pass config validation but are rejected at init (`MIN_EXPRESSIBLE_HZ`; `X` saturates at 255). No BT rate equivalent; USB-first. |
| `enhanced_clicks` | bool | `true\|false` | `false` | Opt-in gate for the **entire** clicks pipeline (see Gate semantics). |
| `sensitivity` | integer | `1..=255` | `64` | Press pressure threshold (raw pressure byte, Apple nominal `0..=253`). **Lower = more sensitive.** Touch presence itself is separate: device flag OR pressure `> 10` drives the cursor without clicking. |
| `margin` | integer | `0..=255` | `5` | Release hysteresis: release fires at `sensitivity - margin`, saturating at 0. Larger = stickier hold, less flapping. |
| `haptic` | integer | `0..=255` | `5` | Haptic output strength. `0` = silent (no pulse sent). Actuator pressure byte = `sensitivity`, intensity = `haptic`. |
| `quiet_click` | bool | `true\|false` | `false` | Soft haptic waveform for the standard level (amplitude 255 / pattern 0 vs normal 120 / 6). |
| `speed` | integer (percent) | `1..=1000` | `100` | Pointer speed percent (`100` = 1:1). Software delta scaling in the proxy path only (`prev + (cur - prev) * speed/100` per finger); init untouched, applied exactly once. |
| `click_1` | string or action object | see Action values | `"left"` | 1-finger press action. |
| `click_2` | string or action object | see Action values | `"right"` | 2-finger press action. |
| `click_3` | string or action object | see Action values | `"middle"` | 3-finger press action. |
| `click_4` | string or action object | see Action values | `"none"` | 4-finger press action. |
| `click_5` | string or action object | see Action values | `"none"` | 5-finger press action. |
| `force_offset` | integer | `0..=255` | `0` | Second-level pressure offset: force fires at `sensitivity + force_offset`. `0` **disables** the force level entirely. Computed in `u16` (up to 510); a threshold above 255 never fires since pressure is a `u8` — keep `sensitivity + force_offset <= 253`. Force release mirrors the first level at `sensitivity + force_offset - margin` (saturating at 0), so force always drops at or before the first level. |
| `force_haptic` | integer, optional | `0..=255`, or omit | omitted (= `haptic`) | Force-level haptic override. `Some(0)` = silent force pulse; omitted = falls back to `haptic` (`effective_force_haptic`). |
| `force_quiet_click` | bool | `true\|false` | `false` | Soft waveform for the force level. |
| `force_1` | string or action object | see Action values | `"none"` | 1-finger force-level action. |
| `force_2` | string or action object | see Action values | `"none"` | 2-finger force-level action. |
| `force_3` | string or action object | see Action values | `"none"` | 3-finger force-level action. |
| `force_4` | string or action object | see Action values | `"none"` | 4-finger force-level action. |
| `force_5` | string or action object | see Action values | `"none"` | 5-finger force-level action. |
| `slide_1` | string or action object | see Action values | `"none"` | 1-finger slide action (touch-but-not-pressed: contact present, every pressure at/below `sensitivity - margin`). Fires/holds while sliding; releases on liftoff, count change (re-selects when the new count maps an action), or press-edge handoff (identical slide/press key sets merge seamlessly, else slide keys release first, one SYN). Haptic: never — slide is just touching, so no down-pulse and no release up-pulse, not even for keys/`empty` (`slide_* = empty` is valid-but-silent: keys-nothing + haptic-nothing; prefer `none`; old configs keep working silently). Only the button levels (`click_*`, `force_*`) buzz. Arming delay: the release frame of a click never arms slide (the latch is still held at selection time) — slide fires on the next light frame while fingers stay down. |
| `slide_2` | string or action object | see Action values | `"none"` | 2-finger slide action (same handoff/release semantics as `slide_1`). |
| `slide_3` | string or action object | see Action values | `"none"` | 3-finger slide action (same handoff/release semantics as `slide_1`). |
| `slide_4` | string or action object | see Action values | `"none"` | 4-finger slide action (same handoff/release semantics as `slide_1`). |
| `slide_5` | string or action object | see Action values | `"none"` | 5-finger slide action (same handoff/release semantics as `slide_1`). |
| `spoof_slide_1` | integer | `0..=5` (`0` = passthrough) | `0` | Virtual finger count the computer sees when 1 finger really touches while sliding (no press latch). `0` = report the real count. Virtual contacts keep the real centroid with the requested count on a tiny fixed pattern (pair ±18 x, 3+ evenly on a 24-unit ring). Slots, `BTN_TOOL_*`, and `BTN_TOUCH` follow the virtual count; action selection still uses the real count unless `spoof_select = true`. |
| `spoof_slide_2` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_slide_1`, for a real count of 2 while sliding. |
| `spoof_slide_3` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_slide_1`, for a real count of 3 while sliding. |
| `spoof_slide_4` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_slide_1`, for a real count of 4 while sliding. |
| `spoof_slide_5` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_slide_1`, for a real count of 5 while sliding. |
| `spoof_click_1` | integer | `0..=5` (`0` = passthrough) | `0` | Virtual finger count the computer sees when 1 finger really touches under a standard press latch (same pattern/selection rules as `spoof_slide_1`). E.g. `spoof_click_2 = 3`: a real two-finger press reports three contacts. |
| `spoof_click_2` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_click_1`, for a real count of 2 under a standard press latch. |
| `spoof_click_3` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_click_1`, for a real count of 3 under a standard press latch. |
| `spoof_click_4` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_click_1`, for a real count of 4 under a standard press latch. |
| `spoof_click_5` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_click_1`, for a real count of 5 under a standard press latch. |
| `spoof_force_1` | integer | `0..=5` (`0` = passthrough) | `0` | Virtual finger count the computer sees when 1 finger really touches while the force level holds (same pattern/selection rules as `spoof_slide_1`). |
| `spoof_force_2` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_force_1`, for a real count of 2 while the force level holds. |
| `spoof_force_3` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_force_1`, for a real count of 3 while the force level holds. |
| `spoof_force_4` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_force_1`, for a real count of 4 while the force level holds. |
| `spoof_force_5` | integer | `0..=5` (`0` = passthrough) | `0` | Same as `spoof_force_1`, for a real count of 5 while the force level holds. |
| `spoof_select` | bool | `true\|false` | `false` | When `true`, `click_N`/`force_N`/`slide_N` selection uses the spoofed count instead of the real one (the edge's finger count reports it too). `false` (default): spoofing only changes what the computer sees, never which action fires. |
| `transition_mode` | string enum | `continuous\|cut` | `"continuous"` | Force-edge behavior over an active click. `continuous` (default, current behavior): level-1 keys stay held, force keys join (shared codes down once, up once). `cut`: release ALL level-1/slide wire keys first (with SYN), then press the force combo fresh — shared codes show up+SYN+down, so `force_1 = left+super` starts a window drag instead of extending a held left; easing back below the force release re-presses the still-latched level-1 keys. Cut intentionally releases and restarts `LEFT` even while the physical button remains held. A force edge that absorbs a same-action slide handoff skips the cut (seamless merge wins for that frame). Tradeoff: continuous keeps drags alive across the force edge; cut restarts the gesture (dumb consumers may read the shared-code flutter as click-release-click). |
| `combo_stagger_ms` | integer (ms) | `0..=500` | `50` | Fallback gap for fresh mixed slides and cut-force fresh presses when the selected binding omits `stagger_ms`. Extras go down + SYN, then pointer buttons after the gap. Deadlines advance on idle polls; contacts/motion keep flowing. `0` means no timed wait. Cut retains its separate 5 ms release floor. Ordinary clicks, continuous force and single-group actions stay immediate unless a mixed binding explicitly overrides timing. See Per-binding timing. |

For a silent five-finger window drag, use `slide_5 = "super+left"`,
`spoof_slide_5 = 1`, `spoof_select = false`, `click_5 = "none"`, and
`force_5 = "none"`. Real five fingers select the action while one contact
is emitted; Super is reported before left by `combo_stagger_ms` (default
50 ms). Slide down/release never request haptics. Pressing through the
click threshold into `none` ends the slide; this remains a touch-level
action, not a pressure-independent gesture. Liftoff or a different action
cancels any pending buttons and releases the old action's modifiers.
Identical-action count changes and press handoffs preserve the original
hold and deadline, including when the button has not yet landed.

## Gate semantics (`enhanced_clicks`)

- `false` (default): device works via init alone — plain kernel/libinput
  passthrough. The proxy still subscribes but short-circuits to passthrough;
  all `click_*` / `force_*` / `slide_*` / `spoof_*` / `sensitivity` /
  `margin` / `haptic` / `quiet_click` / `speed` / `transition_mode` tuning
  is parsed but **not acted on**.
- `true`: proxy grabs the physical node once the pump is ready (real
  `TouchSource` + draining pump + created uinput node) and emits the
  click/button/key pipeline. Grab failure degrades to passthrough with a log.
- Symptom of gate-off: custom `click_2 = "ctrl+c"` (or any click/force map)
  does nothing, no haptic fires, pointer still moves fine. Fix: set
  `enhanced_clicks = true` and **restart**.

## Action values (`click_*`, `force_*`, `slide_*`)

- `none`: no action, no haptic. `empty`: no keys, but haptic still fires on the button levels (`click_*`, `force_*`); `slide_* = empty` stays silent (valid-but-silent — prefer `none`).
- `left | right | middle | back | forward`: mouse buttons (`back`/`forward`
  = `BTN_SIDE`/`BTN_EXTRA`, the long-standing `click_4`/`click_5` mapping).
- Anything else: `+`-joined key combo (e.g. `ctrl+c`, `ctrl+refresh`).
  Case-insensitive, surrounding whitespace ignored; every segment must be a
  known key or the whole value is rejected with the offending segment named.
- Full name table (138 names, key codes, deliberately-excluded tokens):
  **`docs/KEYS.md`** (generated from `src/proxy.rs` + `src/config.rs` +
  kernel headers — read it, not duplicated here).

## Per-binding timing

All fifteen `click_N`, `force_N`, and `slide_N` settings accept the existing
string or an inline action object. The object requires `action` and optionally
accepts `stagger_ms`, an integer in **0..=500 milliseconds**; unknown fields,
wrong types, missing actions and invalid ranges fail with the binding named.
Action strings, aliases, `none` and `empty` keep their existing meaning.

```toml
# Edits to individual existing bindings, not extra duplicate assignments:
slide_5 = { action = "super+left", stagger_ms = 50 }
force_1 = { action = "super+left", stagger_ms = 50 }
click_4 = { action = "super+left", stagger_ms = 0 }
force_4 = "alt+f4"
```

These bindings can use the same key string with different timing. The selected
real/virtual finger count (according to `spoof_select`) selects **both** action
and timing. No Apple-specific section or nested configuration is needed.

- **Positive override:** a fresh mixed action reports its extras group
  (keyboard keys/modifiers) and `SYN_REPORT` first, then holds its pointer
  buttons down after at least the requested gap. This works for ordinary
  clicks, continuous force, slides and cut-force fresh presses. Contacts and
  motion continue; deadlines advance on the pump's idle polls (about 2 ms),
  so delivery can be later than the deadline.
- **Explicit `0`:** no timed modifier-to-button wait. Extras are emitted before
  pointer buttons in the same poll. Slides and cut fresh presses retain the
  SYN between the groups; ordinary click/continuous force use their trailing
  frame SYN. The pointer and extras are separate uinput nodes: this does
  **not** promise physically simultaneous delivery or cross-node atomicity.
- **Omitted timing:** a string and `{ action = "..." }` are equivalent.
  Fresh mixed slides and cut-force fresh presses use `combo_stagger_ms`
  (default 50). Ordinary clicks and continuous non-cut force presses retain
  immediate behavior. Expanding a legacy string alone never adds a delay.
- **Cut floor:** when cut releases a first-level hold to restart force, its
  independent **5 ms release/repress floor** applies before the extras group.
  `stagger_ms = 0` removes only the extra modifier/button wait.
- **Single-group actions:** keyboard-only `alt+f4` and pointer-only `left`
  have no extras-to-pointer division, so an override adds no delay. A cut
  restart still respects its release floor. Routing is unchanged: `left`,
  `right` and `middle` use the pointer node; `back`/`forward` share extras
  with keyboard keys. Combos wholly on extras (e.g. `super+back`) have no
  inter-node division to delay.

Actions are **held**, not macros or repeated key taps: keys release with their
level. Existing held/shared keys do not re-press to apply a different timing.
Identical-action slide count changes and press handoffs retain the original
hold and pending deadline; another level sharing a pending button inherits
that deadline even if its override differs. Independent pending buttons keep
separate deadlines. An ended action returning starts a fresh deadline.
Lift, different-action replacement, gate/session teardown and force disarm
cancel their pending ownership before deadlines run, including a lift at the
exact deadline. `none`/`empty` and haptic decisions are unchanged; slides
never pulse on down or release.

## Examples

Defaults (mirrors `config/default.toml`):

```toml
log_level = "info"
polling_rate = 110
enhanced_clicks = false
sensitivity = 64
margin = 5
haptic = 5
quiet_click = false
speed = 100
click_1 = "left"
click_2 = "right"
click_3 = "middle"
click_4 = "none"
click_5 = "none"
force_offset = 0
force_quiet_click = false
force_1 = "none"
force_2 = "none"
force_3 = "none"
force_4 = "none"
force_5 = "none"
```

Light tune (lighter press, softer click — starting point, verify on hardware):

```toml
enhanced_clicks = true
sensitivity = 48
margin = 5
haptic = 3
quiet_click = true
speed = 100
click_1 = "left"
click_2 = "right"
click_3 = "middle"
```

Force-heavy setup (firm second level for extra actions):

```toml
enhanced_clicks = true
sensitivity = 64
margin = 6
haptic = 5
click_1 = "left"
click_2 = "right"
click_3 = "middle"
force_offset = 32
force_haptic = 7
force_quiet_click = false
force_1 = "ctrl+c"
force_2 = "ctrl+v"
force_3 = "middle"
```

Gesture powers (slide + spoof + cut — verify on hardware):

```toml
enhanced_clicks = true
sensitivity = 64
margin = 5
haptic = 5
click_1 = "left"
force_offset = 20
force_1 = "left+super"
slide_1 = "left"
slide_2 = "middle"
spoof_click_2 = 3
transition_mode = "cut"
combo_stagger_ms = 50
```

### Opt-in flat-config use cases

Each block below is an **edit/alternative** to the same keys in your existing
flat file. Replace those assignments; do not append duplicate keys or tables.
The shipped defaults still leave force disabled and window closing unmapped.

A stronger two/three-finger press can send browser Back/Forward while ordinary
one/two/three-finger clicks remain left/right/middle. An optional stronger
four-finger press sends Alt+F4 to close the focused window; this is an explicit
opt-in, and the application decides how to handle it:

```toml
enhanced_clicks = true
sensitivity = 30
force_offset = 130
click_1 = "left"
click_2 = "right"
click_3 = "middle"
force_2 = "back"
force_3 = "forward"
force_4 = "alt+f4"
```

For a window menu under GNOME's default modifier/button settings, this is an
alternative one-finger force binding (cut restarts the ordinary left hold):

```toml
enhanced_clicks = true
sensitivity = 30
force_offset = 130
transition_mode = "cut"
force_1 = { action = "super+right", stagger_ms = 50 }
```

GNOME's [window-manager schema](https://github.com/GNOME/gsettings-desktop-schemas/blob/master/schemas/org.gnome.desktop.wm.preferences.gschema.xml.in)
defines Super as the default window-click modifier: left moves, middle resizes,
and right opens the window menu. `resize-with-right-button = true` swaps the
right/middle operations, so use `super+middle` for the menu in that case.
GNOME's [window movement help](https://help.gnome.org/gnome-help/shell-windows-states.html)
also describes Super-dragging from anywhere inside a window. These are
source-backed GNOME mappings; owner testing on Niri does not constitute
GNOME hardware verification. Desktop settings and compositor support matter.

This complete opt-in example keeps the owner's force-drag and silent
five-finger slide-drag at 50 ms, while another mixed binding is immediate:

```toml
enhanced_clicks = true
polling_rate = 110
sensitivity = 30
margin = 5
haptic = 2
quiet_click = true
speed = 100
click_1 = "left"
click_2 = "right"
click_3 = "middle"
click_4 = { action = "super+right", stagger_ms = 0 }
click_5 = "none"
force_offset = 130
force_haptic = 1
force_quiet_click = false
transition_mode = "cut"
combo_stagger_ms = 50
force_1 = { action = "super+left", stagger_ms = 50 }
force_2 = "back"
force_3 = "forward"
force_4 = "alt+f4"
force_5 = "none"
slide_5 = { action = "super+left", stagger_ms = 50 }
spoof_slide_5 = 1
spoof_select = false
```

Force-drag starts from a window's interior by holding Super before left; cut
releases the ordinary click first. The slide binding selects on five **real**
fingers while reporting one virtual contact; `click_5`/`force_5` remain `none`.
Touch and slide release are silent, and pressing into the `none` click ends
the slide. The `click_4` menu assignment above is optional; leave it `none`
to keep the owner's existing four-finger click behavior. No live file is
changed by these examples.

Alternatively, omit timing to retain the existing global fallback on the
mixed slide/cut-force paths (these two forms are equivalent):

```toml
combo_stagger_ms = 50
transition_mode = "cut"
slide_5 = "super+left"
force_1 = { action = "super+left" }
```

## Troubleshooting

- **Custom clicks do nothing, pointer fine** → gate off. Set
  `enhanced_clicks = true` + restart. (Also check you restarted at all —
  no live reload.)
- **Clicks don't fire under firm press** → threshold too high: **lower
  `sensitivity`** (e.g. 64 → 48) + restart. Light touches only move the
  cursor by design (touch floor 10 vs click threshold `sensitivity`).
- **Clicks flap / double-fire / release too early** → **raise `margin`**
  (e.g. 5 → 8) + restart. Release sits at `sensitivity - margin`.
- **Clicks fire too easily** → raise `sensitivity` + restart.
- **Force level never fires** → `force_offset = 0` disables it (set > 0),
  or `sensitivity + force_offset > 253` can never fire (pressure is a `u8`;
  lower the offset) + restart.
- **No haptic but buttons work** → `haptic = 0` = silent (same for
  `force_haptic = Some(0)`), or the action is `none` (no haptic by design;
  use `empty` for haptic-without-keys).
- **`invalid polling_rate` / init floor error** → config accepts `1..=110`
  but init rejects `< 16` (`MIN_EXPRESSIBLE_HZ`); `> 110` is rejected at
  validation. Stay in `16..=110`.
- **`unknown field` on load** → typo (`deny_unknown_fields`); compare key
  spelling against the table above.
- **Edited config, nothing changed** → restart required for every key
  (no live reload). Check precedence too: `-c` beats `$LTD_CONFIG` beats
  defaults.
