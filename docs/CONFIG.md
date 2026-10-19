# Configuration reference

Source truth: `src/config.rs` (`Config`, `DeviceConfig`, `PointerConfig`,
`ClickConfig`, `HapticsConfig`, `ForceConfig`, `SlideConfig`, `SpoofConfig`,
`Config::validate`, `parse_click_action`), `src/clicks.rs` (thresholds, force
level), `src/init.rs` (`resolve_polling`), `src/daemon.rs` (startup snapshot,
no live reload), `src/proxy.rs` (`ProxySettings`, per-frame `&Config`).
Shipped defaults: `config/default.toml` (= `Config::default()`).

Device tiers: L0 basic no-pressure (pointer `speed` + finger-count remapping);
L1 pressure no host buzz (+`click`/`force`/`slide`); L2 pressure+host buzz all
active (Apple proprietary today, ChromeOS standard planned). Only Apple Magic
Trackpad 2 implemented; others planned.

## Loading

- Explicit `-c <path>` wins, else `$LTD_CONFIG`, else the installed
  `/etc/linux-trackpad-driver/config.toml`, else built-in defaults
  (shared `select_config_source`; every command reports the selected
  source — startup entries log it as the live settings, doctor prints the
  on-disk file it validated; a missing env/installed file falls back to
  defaults, a missing explicit `-c` file errors).
- `schema_version = 1` is required; unknown `schema_version` fails.
- Omitted keys take their defaults; omitted tables fall back wholesale.
  **Unknown keys are rejected at every level** (`deny_unknown_fields` —
  usually a typo, e.g. `[click]` `sensitivty`).
- Present-but-invalid values fail load with the key + accepted range named.
- Device-specific pressure values, not portable.

## Live vs restart

**All keys need a restart** (`sudo systemctl restart linux-trackpad-driver`, or
restart `ltd run`). There is no live reload: the `enhanced_clicks` gate is
evaluated from the startup config (`src/daemon.rs`, `subscribe_click_proxy`),
and the proxy carries a `ProxySettings` snapshot taken at startup
(`src/proxy.rs`). Per-frame tuning reads (e.g. `speed`) read the startup
snapshot, not the file — editing the TOML mid-run changes nothing until
restart (live reload planned).

## Key reference

| key | type | range | default | requires | effect |
| --- | --- | --- | --- | --- | --- |
| `schema_version` | integer | `1` | `1` | always | File schema version. Missing or unknown fails. |
| `log_level` | string enum | `debug\|info\|warn\|error` | `"info"` | always | Logging filter. |
| `enhanced_clicks` | bool | `true\|false` | `true` | always | Opt-in gate for the entire clicks pipeline (see Gate semantics). |
| `device` | table | — | — | always | USB device tuning (independent of `enhanced_clicks`). |
| `device.polling_rate` | integer (Hz) | `16..=110` request | `110` | always | Requested Apple USB polling rate. Capped posture: values `> 110` rejected; init clamps runtime overrides; `< 16` rejected at init. No BT rate equivalent. |
| `pointer` | table | — | — | `enhanced_clicks` | Pointer tuning. |
| `pointer.speed` | integer (percent) | `1..=1000` | `100` | `enhanced_clicks` | Pointer speed percent (`100` = 1:1). Software delta scaling in the proxy path only, applied exactly once. |
| `click` | table | — | — | `enhanced_clicks` | Press thresholds + standard maps. |
| `click.sensitivity` | integer | `1..=255` | `30` | `enhanced_clicks` | Press pressure threshold (Apple nominal `0..=253`). **Lower = more sensitive.** Firmer alt `64`. Touch presence itself is separate (device flag OR pressure `> 10`). |
| `click.margin` | integer | `0..=255` | `5` | `enhanced_clicks` | Release hysteresis: release at `sensitivity - margin`, saturating at 0. |
| `click.click_1` | string or action object | see Action values | `"left"` | `enhanced_clicks` | 1-finger press action. |
| `click.click_2` | string or action object | see Action values | `"right"` | `enhanced_clicks` | 2-finger press action. |
| `click.click_3` | string or action object | see Action values | `"middle"` | `enhanced_clicks` | 3-finger press action. |
| `click.click_4` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 4-finger press action. |
| `click.click_5` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 5-finger press action. |
| `haptics` | table | — | — | `enhanced_clicks` | Haptic output tuning. |
| `haptics.haptic` | integer | `0..=255` | `2` | `enhanced_clicks` | Haptic output strength. `0` = silent. Actuator pressure byte = `sensitivity`, intensity = `haptic`. |
| `haptics.quiet_click` | bool | `true\|false` | `true` | `enhanced_clicks` | Soft haptic waveform for the standard level. |
| `haptics.force_haptic` | integer, optional | `0..=255`, or omit | omitted (= `haptic`) | `enhanced_clicks` | Force-level haptic override. Omitted = inherit `haptic` (`effective_force_haptic`). |
| `haptics.force_quiet_click` | bool | `true\|false` | `false` | `enhanced_clicks` | Soft waveform for the force level. |
| `force` | table | — | — | `enhanced_clicks` | Second pressure level. |
| `force.force_offset` | integer | `0..=255` | `0` | `enhanced_clicks` | Second-level offset: force fires at `sensitivity + force_offset`. `0` **disables** the force level. Keep `sensitivity + force_offset <= 253`. Example `130` with `30` fires at `160`. |
| `force.force_1` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 1-finger force-level action. |
| `force.force_2` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 2-finger force-level action. |
| `force.force_3` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 3-finger force-level action. |
| `force.force_4` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 4-finger force-level action. |
| `force.force_5` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 5-finger force-level action. |
| `force.transition_mode` | string enum | `continuous\|cut` | `"cut"` | `enhanced_clicks` | Force-edge behavior over an active click. `continuous`: force keys join held click keys. `cut` (default): release click keys first, then press force keys. |
| `force.combo_stagger_ms` | integer (ms) | `0..=500` | `50` | `enhanced_clicks` | Fallback gap for fresh mixed slides and cut-force fresh presses when the selected binding omits `stagger_ms`. |
| `slide` | table | — | — | `enhanced_clicks` | Touch-but-not-pressed maps. |
| `slide.slide_1` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 1-finger slide action (contact present, every pressure at/below `sensitivity - margin`). Fires/holds while sliding; silent (never pulses). |
| `slide.slide_2` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 2-finger slide action (same handoff/release semantics). |
| `slide.slide_3` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 3-finger slide action (same handoff/release semantics). |
| `slide.slide_4` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 4-finger slide action (same handoff/release semantics). |
| `slide.slide_5` | string or action object | see Action values | `"none"` | `enhanced_clicks` | 5-finger slide action (same handoff/release semantics). |
| `spoof` | table | — | empty (= all passthrough) | `enhanced_clicks` | Finger-count spoofing (what the computer sees). TOML-safe: `actions_follow_spoof` first, then sparse subtables. |
| `spoof.actions_follow_spoof` | bool | `true\|false` | `false` | `enhanced_clicks` | Action-selection source. `false` (recommended): actions use the real count while the wire always reports the virtual count. `true` (advanced): selection uses the virtual count too. |
| `spoof.slide` | map `1..=5 → 0..=5` (`0` = passthrough) | real → virtual count | empty | `enhanced_clicks` | Virtual count while sliding. Omitted = real. |
| `spoof.click` | map `1..=5 → 0..=5` (`0` = passthrough) | real → virtual count | empty | `enhanced_clicks` | Virtual count under a standard press. Omitted = real. |
| `spoof.force` | map `1..=5 → 0..=5` (`0` = passthrough) | real → virtual count | empty | `enhanced_clicks` | Virtual count while the force level holds. Omitted = real. |

For a silent five-finger window drag, use `slide_5 = "super+left"` under
`[slide]`, `[spoof.slide]` with `5 = 1`, `[spoof] actions_follow_spoof = false`,
`click_5 = "none"` under `[click]`, and `force_5 = "none"` under `[force]`.

## Gate semantics (`enhanced_clicks`)

- `false`: device works via init alone — plain kernel/libinput passthrough.
  The proxy still subscribes but short-circuits to passthrough; all `[pointer]` /
  `[click]` / `[haptics]` / `[force]` / `[slide]` / `[spoof]` tuning is parsed
  but **not acted on**. `[device]` still acts.
- `true` (default): proxy grabs the physical node once the pump is ready and
  emits the click/button/key pipeline. Grab failure degrades to passthrough
  with a log.
- Symptom of gate-off: custom `[click]` `click_2 = "ctrl+c"` does nothing, no
  haptic fires, pointer still moves fine. Fix: set `enhanced_clicks = true` and
  **restart**.

## Action values (`click_*`, `force_*`, `slide_*`)

- `none`: no action, no haptic. `empty`: no keys, but haptic still fires on the button levels (`click_*`, `force_*`); `slide_* = empty` stays silent (prefer `none`).
- `left | right | middle | back | forward`: mouse buttons.
- Anything else: `+`-joined key combo (e.g. `ctrl+c`). Case-insensitive;
  every segment must be a known key.
- Full name table (138 names, key codes, deliberately-excluded tokens):
  **`docs/KEYS.md`** (generated from `src/proxy.rs` + `src/config.rs` +
  kernel headers — read it, not duplicated here).

## Per-binding timing

All fifteen `click_N`, `force_N`, and `slide_N` settings accept the existing
string or an inline action object under their owning table. The object requires
`action` and optionally accepts `stagger_ms`, an integer in **0..=500
milliseconds**; unknown fields, wrong types, missing actions and invalid ranges
fail with the binding named.

```toml
schema_version = 1
enhanced_clicks = true
[click]
click_4 = { action = "super+right", stagger_ms = 0 }
[force]
force_1 = { action = "super+left", stagger_ms = 50 }
[slide]
slide_5 = { action = "super+left", stagger_ms = 50 }
```

- **Positive override:** a fresh mixed action reports its extras group first,
  then holds its pointer buttons down after at least the requested gap.
- **Explicit `0`:** no timed wait. Extras are emitted before pointer buttons in
  the same poll. Slides and cut fresh presses retain the SYN between the groups.
- **Omitted timing:** a string and `{ action = "..." }` are equivalent. Fresh
  mixed slides and cut-force fresh presses use `combo_stagger_ms` (default 50).
- **Cut floor:** when cut releases a first-level hold to restart force, its
  independent **5 ms release/repress floor** applies before the extras group.
- **Single-group actions:** keyboard-only `alt+f4` and pointer-only `left` have
  no division to delay.

## Examples

Defaults (mirrors `config/default.toml`):

```toml
schema_version = 1
log_level = "info"
enhanced_clicks = true
[device]
polling_rate = 110
[pointer]
speed = 100
[click]
sensitivity = 30
margin = 5
click_1 = "left"
click_2 = "right"
click_3 = "middle"
click_4 = "none"
click_5 = "none"
[haptics]
haptic = 2
quiet_click = true
force_quiet_click = false
[force]
force_offset = 0
force_1 = "none"
force_2 = "none"
force_3 = "none"
force_4 = "none"
force_5 = "none"
transition_mode = "cut"
combo_stagger_ms = 50
[slide]
slide_1 = "none"
slide_2 = "none"
slide_3 = "none"
slide_4 = "none"
slide_5 = "none"
[spoof]
actions_follow_spoof = false
```

Light tune (lighter press, softer click — starting point, verify on hardware):

```toml
schema_version = 1
enhanced_clicks = true
[click]
sensitivity = 30
margin = 5
click_1 = "left"
click_2 = "right"
click_3 = "middle"
[haptics]
haptic = 2
quiet_click = true
[pointer]
speed = 100
```

Force-heavy setup (firm second level for extra actions):

```toml
schema_version = 1
enhanced_clicks = true
[click]
sensitivity = 30
margin = 6
click_1 = "left"
click_2 = "right"
click_3 = "middle"
[haptics]
haptic = 2
force_haptic = 7
[force]
force_offset = 130
force_1 = "ctrl+c"
force_2 = "ctrl+v"
force_3 = "middle"
```

Gesture powers (slide + spoof + cut — verify on hardware):

```toml
schema_version = 1
enhanced_clicks = true
[click]
sensitivity = 30
margin = 5
click_1 = "left"
[haptics]
haptic = 2
[force]
force_offset = 20
force_1 = "left+super"
transition_mode = "cut"
combo_stagger_ms = 50
[slide]
slide_1 = "left"
slide_2 = "middle"
[spoof.click]
2 = 3
```

Opt-in Back/Forward on a firmer press (sensitivity `30`, force at `160`):

```toml
schema_version = 1
enhanced_clicks = true
[click]
sensitivity = 30
click_1 = "left"
click_2 = "right"
click_3 = "middle"
[force]
force_offset = 130
force_2 = "back"
force_3 = "forward"
force_4 = "alt+f4"
```

Window menu under GNOME (one-finger force, cut restarts the ordinary hold):

```toml
schema_version = 1
enhanced_clicks = true
[click]
sensitivity = 30
[force]
force_offset = 130
transition_mode = "cut"
force_1 = { action = "super+right", stagger_ms = 50 }
```

Owner force-drag + silent five-finger slide-drag at 50 ms, menu immediate:

```toml
schema_version = 1
enhanced_clicks = true
[device]
polling_rate = 110
[pointer]
speed = 100
[click]
sensitivity = 30
margin = 5
click_1 = "left"
click_2 = "right"
click_3 = "middle"
click_4 = { action = "super+right", stagger_ms = 0 }
click_5 = "none"
[haptics]
haptic = 2
quiet_click = true
force_haptic = 1
force_quiet_click = false
[force]
force_offset = 130
transition_mode = "cut"
combo_stagger_ms = 50
force_1 = { action = "super+left", stagger_ms = 50 }
force_2 = "back"
force_3 = "forward"
force_4 = "alt+f4"
force_5 = "none"
[slide]
slide_5 = { action = "super+left", stagger_ms = 50 }
[spoof.slide]
5 = 1
[spoof]
actions_follow_spoof = false
```

## Troubleshooting

- **Custom clicks do nothing, pointer fine** → gate off. Set
  `enhanced_clicks = true` + restart. (Also check you restarted at all —
  no live reload.)
- **Clicks don't fire under firm press** → threshold too high: **lower
  `[click]` `sensitivity`** (e.g. 64 → 30) + restart.
- **Clicks flap / double-fire / release too early** → **raise `[click]` `margin`**
  (e.g. 5 → 8) + restart.
- **Clicks fire too easily** → raise `[click]` `sensitivity` + restart.
- **Force level never fires** → `[force]` `force_offset = 0` disables it (set > 0),
  or `sensitivity + force_offset > 253` can never fire + restart.
- **No haptic but buttons work** → `[haptics]` `haptic = 0` = silent (same for
  `force_haptic = Some(0)`), or the action is `none` (use `empty` for
  haptic-without-keys).
- **`invalid [device] polling_rate` / init floor error** → config accepts
  `16..=110` at init, `1..=110` at parse with `> 110` rejected; stay in `16..=110`.
- **`unknown field` on load** → typo (`deny_unknown_fields` at every level);
  compare spelling against the table above.
- **Edited config, nothing changed** → restart required for every key
  (no live reload). Check precedence: `-c` beats `$LTD_CONFIG` beats the
  installed file beats defaults; compare the `config source` line in the
  logs against the source `ltd doctor` validated.
- **`unsupported schema_version`** → file is pre-v1 or future; set
  `schema_version = 1`.
