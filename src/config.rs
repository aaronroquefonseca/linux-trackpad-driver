//! Sectioned v1 TOML config schema for the driver.
//!
//! File layout (strict): `schema_version = 1` is required; every table and
//! nested object denies unknown keys (typos fail). Omitted keys fall back
//! to documented defaults; omitted tables fall back wholesale.
//!
//! ```toml
//! schema_version = 1
//! log_level = "info"
//! enhanced_clicks = true
//! [device]
//! polling_rate = 110
//! [pointer]
//! speed = 100
//! [click]
//! sensitivity = 30
//! ```
//!
//! Range-checked fields use a type wider than their valid range (`u16` for
//! byte-ranged values) so [`Config::validate`] can reject out-of-range
//! input with a clear error instead of failing opaquely at TOML parse.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// File schema version. Only `1` is accepted (breaking pre-v1 allowed).
pub const SCHEMA_VERSION: u8 = 1;
/// Requested polling default (Hz): the capped device-maximum intent, ~110 Hz
/// (`X = 0x23 = 35`, `3850/35 ≈ 110`). See [`crate::init`].
pub const DEFAULT_POLLING_HZ: u32 = 110;
/// Device ceiling: config values above this are rejected (the init layer
/// additionally clamps any runtime override as defense in depth).
pub const MAX_POLLING_HZ: u32 = 110;
/// Upper bound for all byte-ranged settings (`sensitivity`, `margin`,
/// `haptic`, `force_offset`, `force_haptic`).
pub const MAX_BYTE_VALUE: u16 = 255;
/// Bounds for pointer speed percent (`100` = 1:1).
pub const MIN_SPEED: u32 = 1;
pub const MAX_SPEED: u32 = 1000;

/// Shared force arming threshold (`sensitivity + force_offset`, `u16` so
/// `255 + offset` never overflows; thresholds above 255 simply never fire
/// against a `u8` pressure byte). Clamps mirror the press path (`1..=255`,
/// offset capped at 255). `None` when `force_offset == 0` (force disabled).
/// Single source for runtime ([`crate::clicks::ClickMachine::force_threshold`])
/// and diagnostics ([`crate::init::usb_applicability_warnings`]) so the two
/// cannot drift.
pub fn force_threshold_value(sensitivity: u16, force_offset: u16) -> Option<u16> {
    if force_offset > 0 {
        Some(
            sensitivity
                .clamp(1, 255)
                .saturating_add(force_offset.min(255)),
        )
    } else {
        None
    }
}

/// Parsed daemon configuration (sectioned v1 schema).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// File schema version. Must be `1`.
    pub schema_version: u8,
    /// `debug|info|warn|error`. Defaults to `info`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Opt-in gate for the entire clicks pipeline. Default ON; init works
    /// identically either way (passthrough via kernel/libinput).
    #[serde(default = "default_enhanced_clicks")]
    pub enhanced_clicks: bool,
    /// USB device tuning (independent of `enhanced_clicks`).
    #[serde(default)]
    pub device: DeviceConfig,
    /// Pointer tuning (requires `enhanced_clicks` to act).
    #[serde(default)]
    pub pointer: PointerConfig,
    /// Press thresholds + standard-level maps (requires `enhanced_clicks`).
    #[serde(default)]
    pub click: ClickConfig,
    /// Haptic output tuning (requires `enhanced_clicks`).
    #[serde(default)]
    pub haptics: HapticsConfig,
    /// Second pressure level (requires `enhanced_clicks`).
    #[serde(default)]
    pub force: ForceConfig,
    /// Touch-but-not-pressed maps (requires `enhanced_clicks`).
    #[serde(default)]
    pub slide: SlideConfig,
    /// Finger-count spoofing (requires `enhanced_clicks` to act).
    #[serde(default)]
    pub spoof: SpoofConfig,
}

/// USB device tuning. Independent of `enhanced_clicks`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct DeviceConfig {
    /// Requested polling Hz, `1..=110` (capped device maximum). Default 110.
    /// Apple USB only; effective floor 16 at init. No BT rate equivalent.
    #[serde(default = "default_polling")]
    pub polling_rate: u32,
}

/// Pointer tuning. Requires `enhanced_clicks` to act.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct PointerConfig {
    /// Pointer speed percent (`100` = 1:1). `1..=1000`. Default 100.
    /// Software delta scaling in the proxy path only, applied exactly once.
    #[serde(default = "default_speed")]
    pub speed: u32,
}

/// Press thresholds + standard-level finger maps. Requires `enhanced_clicks`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct ClickConfig {
    /// Press pressure threshold. `1..=255`. Default 30 (firmer alt 64).
    /// Lower = more sensitive. Device-specific, not portable.
    #[serde(default = "default_sensitivity")]
    pub sensitivity: u16,
    /// Release hysteresis: release at `sensitivity - margin`. `0..=255`.
    /// Default 5.
    #[serde(default = "default_margin")]
    pub margin: u16,
    /// Finger-count press actions; see [`parse_click_action`]. Defaults
    /// `left/right/middle/none/none`. Each also accepts
    /// `{ action = "...", stagger_ms = 0..=500 }`.
    #[serde(default = "default_click_1")]
    pub click_1: ActionBinding,
    #[serde(default = "default_click_2")]
    pub click_2: ActionBinding,
    #[serde(default = "default_click_3")]
    pub click_3: ActionBinding,
    #[serde(default = "default_action_none")]
    pub click_4: ActionBinding,
    #[serde(default = "default_action_none")]
    pub click_5: ActionBinding,
}

/// Haptic output tuning. Requires `enhanced_clicks` to act.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct HapticsConfig {
    /// Haptic output strength, `0` = silent. `0..=255`. Default 2.
    #[serde(default = "default_haptic")]
    pub haptic: u16,
    /// Soft haptic waveform for the standard level. Default true.
    #[serde(default = "default_quiet_click")]
    pub quiet_click: bool,
    /// Force-level haptic override; falls back to [`HapticsConfig::haptic`]
    /// when unset. See [`Config::effective_force_haptic`].
    #[serde(default)]
    pub force_haptic: Option<u16>,
    /// Soft waveform for the force level. Default false.
    #[serde(default)]
    pub force_quiet_click: bool,
}

/// Second pressure level. Requires `enhanced_clicks` to act.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct ForceConfig {
    /// Second-level pressure offset: force fires at
    /// `sensitivity + force_offset`. `0` disables. `0..=255`. Default 0.
    /// Example owner tune `130` with `sensitivity 30` fires at 160.
    #[serde(default)]
    pub force_offset: u16,
    /// Force-level finger-count actions. Defaults all `none`.
    /// Each also accepts `{ action = "...", stagger_ms = 0..=500 }`.
    #[serde(default = "default_action_none")]
    pub force_1: ActionBinding,
    #[serde(default = "default_action_none")]
    pub force_2: ActionBinding,
    #[serde(default = "default_action_none")]
    pub force_3: ActionBinding,
    #[serde(default = "default_action_none")]
    pub force_4: ActionBinding,
    #[serde(default = "default_action_none")]
    pub force_5: ActionBinding,
    /// Level-transition mode on a force edge over an active click:
    /// `"continuous"` (force keys join held click keys) or `"cut"`
    /// (default: release click keys first, then press force keys).
    #[serde(default = "default_transition_mode")]
    pub transition_mode: String,
    /// Modifier-to-button gap for fresh mixed slide combos and cut force
    /// fresh-presses, in milliseconds (`0..=500`, default 50). Extras go
    /// down + SYN first; pointer buttons follow on the monotonic pump
    /// clock, including idle polls. `0` emits both groups in the same poll.
    /// Cut force presses additionally keep their 5 ms release floor.
    /// Selected action objects can override the gap on any mixed binding;
    /// omitted ordinary click/continuous force timing stays immediate.
    #[serde(default = "default_combo_stagger_ms")]
    pub combo_stagger_ms: u16,
}

/// Touch-but-not-pressed maps. Requires `enhanced_clicks` to act.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, default)]
pub struct SlideConfig {
    /// Slide-level finger-count actions (touch-but-not-pressed: contact
    /// present, every pressure at/below the release threshold). Defaults
    /// all `none` (= no action, sliding only moves the cursor).
    /// Each also accepts `{ action = "...", stagger_ms = 0..=500 }`.
    /// Example: `slide_5 = { action = "super+left", stagger_ms = 50 }`.
    /// See [`crate::clicks::ClickMachine`] for handoff semantics.
    #[serde(default = "default_action_none")]
    pub slide_1: ActionBinding,
    #[serde(default = "default_action_none")]
    pub slide_2: ActionBinding,
    #[serde(default = "default_action_none")]
    pub slide_3: ActionBinding,
    #[serde(default = "default_action_none")]
    pub slide_4: ActionBinding,
    #[serde(default = "default_action_none")]
    pub slide_5: ActionBinding,
}

fn default_log_level() -> String {
    "info".to_string()
}
fn default_enhanced_clicks() -> bool {
    true
}
fn default_polling() -> u32 {
    DEFAULT_POLLING_HZ
}
fn default_sensitivity() -> u16 {
    30
}
fn default_margin() -> u16 {
    5
}
fn default_haptic() -> u16 {
    2
}
fn default_quiet_click() -> bool {
    true
}
fn default_speed() -> u32 {
    100
}
fn default_click_1() -> ActionBinding {
    "left".into()
}
fn default_click_2() -> ActionBinding {
    "right".into()
}
fn default_click_3() -> ActionBinding {
    "middle".into()
}
fn default_action_none() -> ActionBinding {
    "none".into()
}
fn default_transition_mode() -> String {
    "cut".to_string()
}
fn default_combo_stagger_ms() -> u16 {
    50
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            polling_rate: default_polling(),
        }
    }
}

impl Default for PointerConfig {
    fn default() -> Self {
        Self {
            speed: default_speed(),
        }
    }
}

impl Default for ClickConfig {
    fn default() -> Self {
        Self {
            sensitivity: default_sensitivity(),
            margin: default_margin(),
            click_1: default_click_1(),
            click_2: default_click_2(),
            click_3: default_click_3(),
            click_4: default_action_none(),
            click_5: default_action_none(),
        }
    }
}

impl Default for HapticsConfig {
    fn default() -> Self {
        Self {
            haptic: default_haptic(),
            quiet_click: default_quiet_click(),
            force_haptic: None,
            force_quiet_click: false,
        }
    }
}

impl Default for ForceConfig {
    fn default() -> Self {
        Self {
            force_offset: 0,
            force_1: default_action_none(),
            force_2: default_action_none(),
            force_3: default_action_none(),
            force_4: default_action_none(),
            force_5: default_action_none(),
            transition_mode: default_transition_mode(),
            combo_stagger_ms: default_combo_stagger_ms(),
        }
    }
}

impl Default for SlideConfig {
    fn default() -> Self {
        Self {
            slide_1: default_action_none(),
            slide_2: default_action_none(),
            slide_3: default_action_none(),
            slide_4: default_action_none(),
            slide_5: default_action_none(),
        }
    }
}

/// Sparse finger-count spoof table: TOML integer keys (`2 = 3`) arrive
/// as strings, so this parses them to `u8`. Unknown key shapes fail
/// here; out-of-range keys/values fail in [`Config::validate`].
fn deserialize_spoof_table<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<u8, u16>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = BTreeMap::<String, u16>::deserialize(deserializer)?;
    raw.into_iter()
        .map(|(k, v)| {
            k.parse::<u8>()
                .map_err(serde::de::Error::custom)
                .map(|key| (key, v))
        })
        .collect()
}

/// Finger-count spoofing (`[spoof]` table). Sparse: set only needed
/// rows (`[spoof.slide]` / `[spoof.click]` / `[spoof.force]` with
/// `N = V`, N = real count `1..=5`, V = virtual `0..=5` where `0` =
/// passthrough). Omitted rows are passthrough. `actions_follow_spoof`
/// swaps action selection to the spoofed count (default false: wire only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SpoofConfig {
    /// Slide-status map: real count → virtual count.
    #[serde(default, deserialize_with = "deserialize_spoof_table")]
    pub slide: BTreeMap<u8, u16>,
    /// Standard-press map: real count → virtual count.
    #[serde(default, deserialize_with = "deserialize_spoof_table")]
    pub click: BTreeMap<u8, u16>,
    /// Force-level map: real count → virtual count.
    #[serde(default, deserialize_with = "deserialize_spoof_table")]
    pub force: BTreeMap<u8, u16>,
    /// When false (default, recommended), `click_N`/`force_N`/`slide_N`
    /// selection uses the real count while the wire always reports the
    /// virtual count (the desktop is fooled via wire always). When true
    /// (advanced alias), selection uses the virtual count too: e.g. real
    /// 5 with `5 = 4` fires `click_4`, reducing combos. Example with
    /// `[spoof.click]` `3 = 1`: real 3 fires `click_3` when false,
    /// `click_1` when true.
    #[serde(default)]
    pub actions_follow_spoof: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            log_level: default_log_level(),
            enhanced_clicks: default_enhanced_clicks(),
            device: DeviceConfig::default(),
            pointer: PointerConfig::default(),
            click: ClickConfig::default(),
            haptics: HapticsConfig::default(),
            force: ForceConfig::default(),
            slide: SlideConfig::default(),
            spoof: SpoofConfig::default(),
        }
    }
}

/// A flat action setting: legacy string or `{ action = "...", stagger_ms = N }`.
/// Missing timing stays distinct from an explicit zero.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionBinding {
    pub action: String,
    pub stagger_ms: Option<u16>,
}

impl From<&str> for ActionBinding {
    fn from(action: &str) -> Self {
        Self {
            action: action.into(),
            stagger_ms: None,
        }
    }
}

impl From<String> for ActionBinding {
    fn from(action: String) -> Self {
        Self {
            action,
            stagger_ms: None,
        }
    }
}

impl<'de> Deserialize<'de> for ActionBinding {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct BindingVisitor;
        impl<'de> serde::de::Visitor<'de> for BindingVisitor {
            type Value = ActionBinding;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an action string or { action = string, stagger_ms = 0..=500 }")
            }
            fn visit_str<E: serde::de::Error>(
                self,
                value: &str,
            ) -> std::result::Result<Self::Value, E> {
                Ok(value.into())
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Expanded {
                    action: String,
                    stagger_ms: Option<u16>,
                }
                let value =
                    Expanded::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(ActionBinding {
                    action: value.action,
                    stagger_ms: value.stagger_ms,
                })
            }
        }
        deserializer.deserialize_any(BindingVisitor)
    }
}

impl ActionBinding {
    fn parse(&self) -> std::result::Result<ClickAction, String> {
        if self.stagger_ms.is_some_and(|ms| ms > 500) {
            return Err("stagger_ms must be 0..=500 milliseconds".into());
        }
        let mut action = parse_click_action(&self.action)?;
        action.stagger_ms = self.stagger_ms;
        Ok(action)
    }
}

/// A validated click/force action value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickAction {
    /// Original string as written in config.
    pub raw: String,
    /// Selected binding timing; None preserves the applicable legacy path.
    pub stagger_ms: Option<u16>,
    /// Classified meaning.
    pub kind: ActionKind,
}

/// Meaning of a [`ClickAction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionKind {
    /// No action and no haptic feedback.
    None,
    /// No keys sent, but haptic feedback still fires.
    Empty,
    /// One or more `+`-joined key/button names (lowercased, trimmed).
    Keys(Vec<String>),
}

impl ActionKind {
    /// Wire-identity compare: key sets match as sets (order- and
    /// duplicate-insensitive, mirroring proxy same-level dedup:
    /// `left+ctrl`, `ctrl+left`, and `left+left+ctrl` all hold one `left`
    /// and one `ctrl`). `Empty == Empty`; `None` only equals `None`
    /// (a latched slide is never `None`, so handoffs only merge real
    /// holds).
    pub fn is_same_action(&self, other: &Self) -> bool {
        match (self, other) {
            (ActionKind::None, ActionKind::None) => true,
            (ActionKind::Empty, ActionKind::Empty) => true,
            (ActionKind::Keys(a), ActionKind::Keys(b)) => {
                a.iter().collect::<std::collections::HashSet<_>>()
                    == b.iter().collect::<std::collections::HashSet<_>>()
            }
            _ => false,
        }
    }
}

/// Parse a `click_*`/`force_*` value: `left|right|middle|back|forward|none`
/// `|empty` or `+`-joined key names (e.g. `ctrl+c`). Case-insensitive;
/// surrounding whitespace is ignored. Key names cover modifiers
/// (incl. right variants), nav/edit keys (`insert`, `print`, ...), media
/// keys (`playpause`/`playcd`/`pausecd`/`stop`/`next`/`prev`/`mute`/
/// `volumeup`/`volumedown`/`micmute`), browser keys (`browserback` /
/// `browserforward` — `back`/`forward` stay mouse buttons — plus `refresh`,
/// `homepage`, `search`, `favorites`, `bookmarks`, `www`), app keys
/// (`calc`, `mail`, `computer`, `player`, `screenlock`/`lock`), `a`-`z`,
/// `0`-`9`, and `f1`-`f24` (see [`crate::proxy::key_token_to_code`] for
/// codes and the deliberately-excluded list). Combos accept every known
/// key (e.g. `ctrl+refresh`). Returns a descriptive error naming
/// the offending key segment on failure.
pub fn parse_click_action(s: &str) -> std::result::Result<ClickAction, String> {
    let lowered = s.trim().to_lowercase();
    if lowered == "none" {
        return Ok(ClickAction {
            raw: s.to_string(),
            stagger_ms: None,
            kind: ActionKind::None,
        });
    }
    if lowered == "empty" {
        return Ok(ClickAction {
            raw: s.to_string(),
            stagger_ms: None,
            kind: ActionKind::Empty,
        });
    }
    if lowered.is_empty() {
        return Err(
            "empty click action (want left|right|middle|back|forward|none|empty|key-combo)"
                .to_string(),
        );
    }
    let mut keys = Vec::new();
    for part in lowered.split('+') {
        let key = part.trim();
        if key.is_empty() {
            return Err(format!("empty key segment in click action '{s}'"));
        }
        if !is_known_key(key) {
            return Err(format!("unknown key '{key}' in click action '{s}'"));
        }
        keys.push(key.to_string());
    }
    Ok(ClickAction {
        raw: s.to_string(),
        stagger_ms: None,
        kind: ActionKind::Keys(keys),
    })
}

/// Whether `name` (already lowercased + trimmed) is a valid key/button token
/// for a click action combo.
fn is_known_key(name: &str) -> bool {
    matches!(
        name,
        // Mouse buttons.
        "left" | "right" | "middle" | "back" | "forward"
        // Modifiers.
        | "ctrl" | "control" | "lctrl" | "rctrl"
        | "shift" | "lshift" | "rshift"
        | "alt" | "lalt" | "ralt"
        | "meta" | "super" | "cmd" | "command" | "win" | "lmeta" | "rmeta"
        // Navigation / editing.
        | "esc" | "escape" | "enter" | "return" | "space" | "tab"
        | "backspace" | "delete" | "insert" | "print" | "home" | "end"
        | "pageup" | "pgup" | "pagedown" | "pgdn"
        | "up" | "arrow_up" | "down" | "arrow_down"
        | "left_arrow" | "right_arrow"
        // Media (codes in crate::proxy; each header-verified).
        | "volup" | "volumeup" | "voldown" | "volumedown" | "mute" | "micmute"
        | "play" | "playpause" | "playcd" | "pausecd" | "pause"
        | "stop" | "stopcd" | "next" | "nextsong"
        | "prev" | "previous" | "previoussong"
        // Browser (`back`/`forward` stay mouse buttons; browser variants
        // carry the AC Back/Forward codes 158/159).
        | "browserback" | "browser_back" | "browserforward" | "browser_forward"
        | "refresh" | "homepage" | "search" | "favorites" | "bookmarks" | "www"
        // App launchers / system.
        | "calc" | "mail" | "computer" | "player" | "screenlock" | "lock"
        // Letters.
        | "a" | "b" | "c" | "d" | "e" | "f" | "g" | "h" | "i" | "j" | "k"
        | "l" | "m" | "n" | "o" | "p" | "q" | "r" | "s" | "t" | "u" | "v"
        | "w" | "x" | "y" | "z"
        // Digits.
        | "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
        // Function row (codes in crate::proxy; F13..F24 = 183..=194).
        | "f1" | "f2" | "f3" | "f4" | "f5" | "f6"
        | "f7" | "f8" | "f9" | "f10" | "f11" | "f12"
        | "f13" | "f14" | "f15" | "f16" | "f17" | "f18"
        | "f19" | "f20" | "f21" | "f22" | "f23" | "f24"
    )
}

impl Config {
    /// Validate every user-facing value; each failure names the key and the
    /// accepted range/set.
    ///
    /// Syntax/range layer only: polling `1..=110` passes here while USB init
    /// needs `>= 16`, and extreme-but-in-range thresholds pass while being
    /// effectively unreachable. That hardware applicability is
    /// [`crate::init::check_usb_polling_usable`], shared by USB startup
    /// (via [`crate::daemon::validate_polling_rate`] /
    /// [`crate::init::resolve_polling`]) and doctor (via the doctor-facing
    /// [`crate::init::validate_config_for_usb`] wrapper) so the two can
    /// never diverge again.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            anyhow::bail!(
                "unsupported schema_version {} (want {SCHEMA_VERSION})",
                self.schema_version
            );
        }
        match self.log_level.as_str() {
            "debug" | "info" | "warn" | "error" => {}
            other => anyhow::bail!("invalid log_level '{other}' (want debug|info|warn|error)"),
        }
        if self.device.polling_rate == 0 || self.device.polling_rate > MAX_POLLING_HZ {
            anyhow::bail!(
                "invalid [device] polling_rate {} (want 1..={MAX_POLLING_HZ}; device max ~110 Hz, 99 F2 0x23)",
                self.device.polling_rate
            );
        }
        if self.click.sensitivity == 0 || self.click.sensitivity > MAX_BYTE_VALUE {
            anyhow::bail!(
                "invalid [click] sensitivity {} (want 1..={MAX_BYTE_VALUE})",
                self.click.sensitivity
            );
        }
        if self.click.margin > MAX_BYTE_VALUE {
            anyhow::bail!(
                "invalid [click] margin {} (want 0..={MAX_BYTE_VALUE})",
                self.click.margin
            );
        }
        if self.haptics.haptic > MAX_BYTE_VALUE {
            anyhow::bail!(
                "invalid [haptics] haptic {} (want 0..={MAX_BYTE_VALUE}, 0 = silent)",
                self.haptics.haptic
            );
        }
        if self.pointer.speed < MIN_SPEED || self.pointer.speed > MAX_SPEED {
            anyhow::bail!(
                "invalid [pointer] speed {} (want {MIN_SPEED}..={MAX_SPEED}, 100 = 1:1)",
                self.pointer.speed
            );
        }
        for (key, value) in [
            ("[click] click_1", &self.click.click_1),
            ("[click] click_2", &self.click.click_2),
            ("[click] click_3", &self.click.click_3),
            ("[click] click_4", &self.click.click_4),
            ("[click] click_5", &self.click.click_5),
            ("[force] force_1", &self.force.force_1),
            ("[force] force_2", &self.force.force_2),
            ("[force] force_3", &self.force.force_3),
            ("[force] force_4", &self.force.force_4),
            ("[force] force_5", &self.force.force_5),
            ("[slide] slide_1", &self.slide.slide_1),
            ("[slide] slide_2", &self.slide.slide_2),
            ("[slide] slide_3", &self.slide.slide_3),
            ("[slide] slide_4", &self.slide.slide_4),
            ("[slide] slide_5", &self.slide.slide_5),
        ] {
            value
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid {key}: {e}"))?;
        }
        for (table, map) in [
            ("[spoof.slide]", &self.spoof.slide),
            ("[spoof.click]", &self.spoof.click),
            ("[spoof.force]", &self.spoof.force),
        ] {
            for (real, virtual_n) in map {
                if !(1..=5).contains(real) {
                    anyhow::bail!("invalid {table}.{real} (want real count 1..=5)");
                }
                if *virtual_n > 5 {
                    anyhow::bail!(
                        "invalid {table}.{real} {virtual_n} (want 0..=5, 0 = passthrough)"
                    );
                }
            }
        }
        match self.force.transition_mode.as_str() {
            "continuous" | "cut" => {}
            other => {
                anyhow::bail!("invalid [force] transition_mode '{other}' (want continuous|cut)")
            }
        }
        if self.force.combo_stagger_ms > 500 {
            anyhow::bail!(
                "invalid [force] combo_stagger_ms {} (want 0..=500)",
                self.force.combo_stagger_ms
            );
        }
        if self.force.force_offset > MAX_BYTE_VALUE {
            anyhow::bail!(
                "invalid [force] force_offset {} (want 0..={MAX_BYTE_VALUE}, 0 = disabled)",
                self.force.force_offset
            );
        }
        if let Some(h) = self.haptics.force_haptic {
            if h > MAX_BYTE_VALUE {
                anyhow::bail!(
                    "invalid [haptics] force_haptic {h} (want 0..={MAX_BYTE_VALUE}; omit to use haptic)"
                );
            }
        }
        Ok(())
    }

    /// Standard-level action for finger count `n` (`1..=5`).
    pub fn click_action(&self, n: u8) -> Result<ClickAction> {
        let raw = match n {
            1 => &self.click.click_1,
            2 => &self.click.click_2,
            3 => &self.click.click_3,
            4 => &self.click.click_4,
            5 => &self.click.click_5,
            _ => anyhow::bail!("finger count {n} out of range (want 1..=5)"),
        };
        raw.parse()
            .map_err(|e| anyhow::anyhow!("invalid [click] click_{n}: {e}"))
    }

    /// Force-level action for finger count `n` (`1..=5`).
    pub fn force_action(&self, n: u8) -> Result<ClickAction> {
        let raw = match n {
            1 => &self.force.force_1,
            2 => &self.force.force_2,
            3 => &self.force.force_3,
            4 => &self.force.force_4,
            5 => &self.force.force_5,
            _ => anyhow::bail!("finger count {n} out of range (want 1..=5)"),
        };
        raw.parse()
            .map_err(|e| anyhow::anyhow!("invalid [force] force_{n}: {e}"))
    }

    /// True when at least one `slide_N` maps an action (string-level
    /// check, no parse: anything but `none` — modulo case/whitespace —
    /// counts, including values that would fail validation, so an invalid
    /// value still reaches the parse path and errors instead of being
    /// silently skipped). The click machine uses this to skip slide
    /// selection entirely in the common all-`none` case.
    pub fn any_slide_mapped(&self) -> bool {
        [
            &self.slide.slide_1,
            &self.slide.slide_2,
            &self.slide.slide_3,
            &self.slide.slide_4,
            &self.slide.slide_5,
        ]
        .iter()
        .any(|s| !s.action.trim().eq_ignore_ascii_case("none"))
    }

    /// Slide-level action for finger count `n` (`1..=5`).
    pub fn slide_action(&self, n: u8) -> Result<ClickAction> {
        let raw = match n {
            1 => &self.slide.slide_1,
            2 => &self.slide.slide_2,
            3 => &self.slide.slide_3,
            4 => &self.slide.slide_4,
            5 => &self.slide.slide_5,
            _ => anyhow::bail!("finger count {n} out of range (want 1..=5)"),
        };
        raw.parse()
            .map_err(|e| anyhow::anyhow!("invalid [slide] slide_{n}: {e}"))
    }

    /// Raw spoof cell for (`status`, real count `n`): `0` = passthrough
    /// (omitted row). `status` is `slide`/`click`/`force`; anything else
    /// returns `0`. Values above 5 can only come from an unvalidated
    /// `Config` (validation rejects them): debug builds assert, release
    /// falls back to `0` (passthrough) rather than emitting a bogus count.
    pub fn spoof_cell(&self, status: &str, n: u8) -> u16 {
        let map = match status {
            "slide" => &self.spoof.slide,
            "click" => &self.spoof.click,
            "force" => &self.spoof.force,
            _ => return 0,
        };
        let cell = map.get(&n).copied().unwrap_or(0);
        if cell > 5 {
            debug_assert!(
                false,
                "spoof cell {cell} out of range (validate() must reject 6..)"
            );
            0
        } else {
            cell
        }
    }

    /// Virtual count the computer sees for (`status`, real count `n`):
    /// the spoof cell when set (`1..=5`), else the real count.
    pub fn spoofed_count(&self, status: &str, real: u8) -> u8 {
        let cell = self.spoof_cell(status, real);
        if (1..=5).contains(&cell) {
            cell as u8
        } else {
            real
        }
    }

    /// Count used for `*_N` action selection: the spoofed count when
    /// [`SpoofConfig::actions_follow_spoof`] is on, else the real count.
    pub fn select_count(&self, status: &str, real: u8) -> u8 {
        if self.spoof.actions_follow_spoof {
            self.spoofed_count(status, real)
        } else {
            real
        }
    }

    /// True when a force edge cuts level-1/slide keys first (`"cut"`, default);
    /// false is `"continuous"` (force keys join held click keys).
    pub fn is_cut_transition(&self) -> bool {
        self.force.transition_mode == "cut"
    }

    /// Whether the second pressure level is armed (`force_offset > 0`).
    pub fn force_enabled(&self) -> bool {
        self.force.force_offset > 0
    }

    /// Haptic strength for the force level: explicit `force_haptic` override,
    /// else the standard [`HapticsConfig::haptic`] value.
    pub fn effective_force_haptic(&self) -> u16 {
        self.haptics.force_haptic.unwrap_or(self.haptics.haptic)
    }
}

/// Load config from `path`, or defaults when `None`. Missing file with an
/// explicit path is an error; `None` never fails. Unknown keys are rejected
/// at every level (likely typos); omitted keys/tables take documented
/// defaults. `schema_version = 1` is required in files.
pub fn load_or_default(path: Option<&str>) -> Result<Config> {
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = std::fs::read_to_string(Path::new(path))
        .with_context(|| format!("reading config '{path}'"))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parsing config '{path}'"))?;
    cfg.validate()?;
    Ok(cfg)
}

/// Env var naming the config file for contexts that cannot take `-c`
/// (udev/systemd one-shots run `ltd reconnect /dev/%I` with no room for
/// per-call flags). An explicit `-c` path always wins over this.
pub const CONFIG_ENV: &str = "LTD_CONFIG";

/// Config path from the environment, if set and non-empty.
pub fn config_path_from_env() -> Option<String> {
    std::env::var(CONFIG_ENV).ok().filter(|v| !v.is_empty())
}

/// Load config for entry points: explicit `-c` path wins, else
/// `$LTD_CONFIG`, else defaults.
///
/// A *missing* env-pointed file falls back to defaults with a log line
/// (hotplug must keep working on machines with no system config installed);
/// a present-but-unparsable file is still an error, as is a missing
/// explicit `-c` file.
pub fn load_or_default_with_env(explicit: Option<&str>) -> Result<Config> {
    if let Some(path) = explicit {
        return load_or_default(Some(path));
    }
    match config_path_from_env() {
        None => Ok(Config::default()),
        Some(path) if !Path::new(&path).exists() => {
            log::info!("config env {CONFIG_ENV}='{path}' missing; using defaults");
            Ok(Config::default())
        }
        Some(path) => load_or_default(Some(&path)),
    }
}

/// Best-effort logging init; safe to call twice (tests, reconnect entry).
pub fn init_logging(level: &str) {
    let filter = match level {
        "debug" => log::LevelFilter::Debug,
        "warn" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    };
    let _ = env_logger::Builder::new()
        .filter_level(filter)
        .format_timestamp_secs()
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_toml(body: &str) -> String {
        format!("schema_version = 1\n{body}")
    }

    fn toml_in(section: &str, key: &str, value: &str) -> String {
        format!("schema_version = 1\n[{section}]\n{key} = {value}\n")
    }

    #[test]
    fn defaults_match_v1_spec() {
        let c = Config::default();
        assert_eq!(c.schema_version, 1);
        assert_eq!(c.log_level, "info");
        assert!(c.enhanced_clicks);
        assert_eq!(c.device.polling_rate, 110);
        assert_eq!(c.pointer.speed, 100);
        assert_eq!(c.click.sensitivity, 30);
        assert_eq!(c.click.margin, 5);
        assert_eq!(c.click.click_1.action, "left");
        assert_eq!(c.click.click_2.action, "right");
        assert_eq!(c.click.click_3.action, "middle");
        assert_eq!(c.click.click_4.action, "none");
        assert_eq!(c.click.click_5.action, "none");
        assert_eq!(c.haptics.haptic, 2);
        assert!(c.haptics.quiet_click);
        assert_eq!(c.haptics.force_haptic, None);
        assert!(!c.haptics.force_quiet_click);
        assert_eq!(c.force.force_offset, 0);
        for f in [
            &c.force.force_1,
            &c.force.force_2,
            &c.force.force_3,
            &c.force.force_4,
            &c.force.force_5,
        ] {
            assert_eq!(f.action, "none");
        }
        for s in [
            &c.slide.slide_1,
            &c.slide.slide_2,
            &c.slide.slide_3,
            &c.slide.slide_4,
            &c.slide.slide_5,
        ] {
            assert_eq!(s.action, "none");
        }
        assert!(c.spoof.slide.is_empty());
        assert!(c.spoof.click.is_empty());
        assert!(c.spoof.force.is_empty());
        assert!(!c.spoof.actions_follow_spoof);
        assert_eq!(c.force.transition_mode, "cut");
        assert!(c.is_cut_transition());
        c.validate().unwrap();
        assert!(!c.force_enabled());
        assert_eq!(c.effective_force_haptic(), 2);
    }

    #[test]
    fn schema_version_required_and_rejected_when_unknown() {
        // Missing version fails at parse (required field).
        let missing: std::result::Result<Config, _> = toml::from_str("enhanced_clicks = true\n");
        assert!(missing.is_err());
        // Wrong version parses but fails validation.
        for v in [0, 2, 99] {
            let cfg: Config = toml::from_str(&format!("schema_version = {v}\n")).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains("schema_version"), "v={v}: {err}");
        }
        let ok: Config = toml::from_str("schema_version = 1\n").unwrap();
        ok.validate().unwrap();
    }

    #[test]
    fn omission_equals_defaults() {
        let bare: Config = toml::from_str("schema_version = 1\n").unwrap();
        bare.validate().unwrap();
        assert_eq!(bare, Config::default());
        // Omitted tables fall back wholesale.
        let partial: Config =
            toml::from_str("schema_version = 1\n[click]\nsensitivity = 48\n").unwrap();
        partial.validate().unwrap();
        assert_eq!(partial.click.sensitivity, 48);
        assert_eq!(partial.click.margin, 5);
        assert_eq!(partial.click.click_1.action, "left");
        assert_eq!(partial.device.polling_rate, 110);
        assert_eq!(
            partial,
            Config {
                click: ClickConfig {
                    sensitivity: 48,
                    ..ClickConfig::default()
                },
                ..Config::default()
            }
        );
    }

    #[test]
    fn nested_typo_rejected_everywhere() {
        // Top-level typo.
        assert!(toml::from_str::<Config>("schema_version = 1\nsensitivty = 30\n").is_err());
        // Section typos.
        for (section, key) in [
            ("device", "poling_rate"),
            ("pointer", "speeed"),
            ("click", "sensitivty"),
            ("click", "click_6"),
            ("haptics", "haptik"),
            ("force", "force_6"),
            ("force", "transition_modes"),
            ("slide", "slide_6"),
            ("spoof", "bogus"),
        ] {
            let text = format!("schema_version = 1\n[{section}]\n{key} = 1\n");
            assert!(
                toml::from_str::<Config>(&text).is_err(),
                "{section}.{key} must be rejected"
            );
        }
        // Nested binding object typo.
        let bad: std::result::Result<Config, _> = toml::from_str(
            "schema_version = 1\n[click]\nclick_1 = { action = \"left\", typo = 1 }\n",
        );
        assert!(bad.is_err());
        // Spoof sub-table typo.
        let bad: std::result::Result<Config, _> =
            toml::from_str("schema_version = 1\n[spoof.bogus]\n1 = 2\n");
        assert!(bad.is_err());
        // Legacy flat keys are gone (breaking, pre-v1): unknown field.
        for flat in [
            "schema_version = 1\nsensitivity = 30\n",
            "schema_version = 1\npolling_rate = 110\n",
            "schema_version = 1\nspoof_click_2 = 3\n",
        ] {
            assert!(
                toml::from_str::<Config>(flat).is_err(),
                "{flat:?} must fail"
            );
        }
    }

    #[test]
    fn rejects_bad_log_level() {
        let bad = Config {
            log_level: "verbose".into(),
            ..Config::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn rejects_polling_zero_and_above_cap() {
        for rate in [0, 111, 500, 65535] {
            let mut c = Config::default();
            c.device.polling_rate = rate;
            assert!(c.validate().is_err(), "rate {rate} must be rejected");
        }
        for rate in [1, 60, 110] {
            let mut c = Config::default();
            c.device.polling_rate = rate;
            assert!(c.validate().is_ok(), "rate {rate} must be accepted");
        }
    }

    #[test]
    fn rejects_sensitivity_outside_1_255() {
        for v in [0, 256, 1000] {
            let mut c = Config::default();
            c.click.sensitivity = v;
            assert!(c.validate().is_err(), "sensitivity {v} must be rejected");
        }
        for v in [1, 30, 64, 255] {
            let mut c = Config::default();
            c.click.sensitivity = v;
            assert!(c.validate().is_ok(), "sensitivity {v} must be accepted");
        }
    }

    #[test]
    fn rejects_margin_above_255() {
        let mut bad = Config::default();
        bad.click.margin = 256;
        assert!(bad.validate().is_err());
        let mut ok = Config::default();
        ok.click.margin = 0;
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn rejects_haptic_above_255() {
        let mut bad = Config::default();
        bad.haptics.haptic = 300;
        assert!(bad.validate().is_err());
        let mut ok = Config::default();
        ok.haptics.haptic = 0;
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn rejects_speed_outside_1_1000() {
        for v in [0, 1001, 10_000] {
            let mut c = Config::default();
            c.pointer.speed = v;
            assert!(c.validate().is_err(), "speed {v} must be rejected");
        }
        for v in [1, 100, 1000] {
            let mut c = Config::default();
            c.pointer.speed = v;
            assert!(c.validate().is_ok(), "speed {v} must be accepted");
        }
    }

    #[test]
    fn rejects_bad_click_values() {
        for bad_value in ["doubleclick", "ctrl++c", "ctrl+", "", "left click"] {
            let mut c = Config::default();
            c.click.click_1 = bad_value.into();
            assert!(
                c.validate().is_err(),
                "click_1 '{bad_value}' must be rejected"
            );
        }
    }

    #[test]
    fn accepts_click_buttons_and_key_combos() {
        for good in [
            "left",
            "right",
            "middle",
            "back",
            "forward",
            "none",
            "empty",
            "LEFT",
            "ctrl+c",
            "Shift + F5",
            "Shift + F15",
            "f13",
            "f24",
            "meta+space",
            "insert",
            "print",
            "volumeup",
            "volumedown",
            "micmute",
            "playpause",
            "playcd",
            "pausecd",
            "stop",
            "stopcd",
            "next",
            "nextsong",
            "prev",
            "previoussong",
            "browserback",
            "browser_back",
            "browserforward",
            "browser_forward",
            "refresh",
            "homepage",
            "search",
            "favorites",
            "bookmarks",
            "www",
            "calc",
            "mail",
            "computer",
            "player",
            "screenlock",
            "lock",
            "ctrl+refresh",
            "ctrl+playpause",
            "shift+browserback",
        ] {
            let mut c = Config::default();
            c.click.click_4 = good.into();
            assert!(c.validate().is_ok(), "click_4 '{good}' must be accepted");
        }
        let combo = parse_click_action("ctrl+c").unwrap();
        assert_eq!(
            combo.kind,
            ActionKind::Keys(vec!["ctrl".to_string(), "c".to_string()])
        );
        assert_eq!(parse_click_action("none").unwrap().kind, ActionKind::None);
        assert_eq!(parse_click_action("EMPTY").unwrap().kind, ActionKind::Empty);
    }

    #[test]
    fn same_action_compares_wire_identity() {
        let keys = |s: &str| parse_click_action(s).unwrap().kind;
        assert!(keys("left+ctrl").is_same_action(&keys("ctrl+left")));
        assert!(keys("left+left+ctrl").is_same_action(&keys("ctrl+left")));
        assert!(!keys("left+ctrl").is_same_action(&keys("left+shift")));
        assert!(!keys("left").is_same_action(&keys("left+ctrl")));
        assert!(keys("empty").is_same_action(&keys("EMPTY")));
        assert!(!keys("empty").is_same_action(&keys("left")));
        assert!(!keys("none").is_same_action(&keys("empty")));
    }

    #[test]
    fn accepts_f13_through_f24_and_rejects_f25() {
        for n in 13..=24 {
            let name = format!("f{n}");
            assert!(parse_click_action(&name).is_ok(), "{name} must be accepted");
        }
        assert!(parse_click_action("f25").is_err());
        assert!(parse_click_action("f0").is_err());
    }

    #[test]
    fn rejects_bad_force_actions() {
        let mut c = Config::default();
        c.force.force_2 = "bogus-key".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn slide_actions_validate_like_click() {
        let mut bad = Config::default();
        bad.slide.slide_2 = "bogus-key".into();
        assert!(bad.validate().is_err());
        let mut ok = Config::default();
        ok.slide.slide_1 = "left".into();
        ok.slide.slide_2 = "ctrl+c".into();
        ok.slide.slide_3 = "empty".into();
        assert!(ok.validate().is_ok());
        assert_eq!(
            ok.slide_action(2).unwrap().kind,
            ActionKind::Keys(vec!["ctrl".to_string(), "c".to_string()])
        );
        assert_eq!(ok.slide_action(4).unwrap().kind, ActionKind::None);
        assert!(ok.slide_action(0).is_err());
        assert!(ok.slide_action(6).is_err());
    }

    #[test]
    fn any_slide_mapped_short_circuits_all_none() {
        assert!(!Config::default().any_slide_mapped());
        let mut mapped = Config::default();
        mapped.slide.slide_3 = "middle".into();
        assert!(mapped.any_slide_mapped());
        let mut noneish = Config::default();
        noneish.slide.slide_1 = " NONE ".into();
        assert!(!noneish.any_slide_mapped());
        let mut bad = Config::default();
        bad.slide.slide_1 = "bogus-key".into();
        assert!(bad.any_slide_mapped());
        let mut m = crate::clicks::ClickMachine::new();
        assert!(m
            .update(
                &[crate::apple_parse::FingerContact {
                    id: 1,
                    x: 0,
                    y: 0,
                    size: 10,
                    orientation: 0,
                    touch_major: 8,
                    touch_minor: 6,
                    pressure: 20,
                    contact: true,
                }],
                0,
                &bad
            )
            .is_err());
    }

    #[test]
    fn rejects_spoof_outside_0_5() {
        for v in [6, 9, 255] {
            let mut spoof = SpoofConfig::default();
            spoof.click.insert(2, v);
            let c = Config {
                spoof,
                ..Config::default()
            };
            assert!(
                c.validate().is_err(),
                "spoof.click 2 = {v} must be rejected"
            );
        }
        for v in [0, 1, 3, 5] {
            let mut spoof = SpoofConfig::default();
            spoof.click.insert(2, v);
            spoof.slide.insert(1, v);
            spoof.force.insert(5, v);
            let c = Config {
                spoof,
                ..Config::default()
            };
            assert!(c.validate().is_ok(), "spoof = {v} must be accepted");
        }
        for bad_key in [0, 6, 255] {
            let mut spoof = SpoofConfig::default();
            spoof.click.insert(bad_key, 3);
            let c = Config {
                spoof,
                ..Config::default()
            };
            assert!(
                c.validate().is_err(),
                "spoof key {bad_key} must be rejected"
            );
        }
    }

    #[test]
    fn spoof_cells_default_passthrough() {
        let c = Config::default();
        assert_eq!(c.spoofed_count("click", 2), 2);
        assert_eq!(c.select_count("click", 2), 2);
        let mut spoof = SpoofConfig::default();
        spoof.click.insert(2, 3);
        let c = Config {
            spoof,
            ..Config::default()
        };
        c.validate().unwrap();
        assert_eq!(c.spoofed_count("click", 2), 3);
        assert_eq!(c.select_count("click", 2), 2);
        assert_eq!(c.spoofed_count("click", 1), 1);
        assert_eq!(c.spoofed_count("force", 2), 2);
        let c = Config {
            spoof: SpoofConfig {
                actions_follow_spoof: true,
                ..c.spoof.clone()
            },
            ..c
        };
        assert_eq!(c.select_count("click", 2), 3);
    }

    #[test]
    fn spoof_sparse_toml_omitted_is_passthrough() {
        let c: Config = toml::from_str(&minimal_toml(
            "[spoof.click]\n2 = 3\n[spoof.slide]\n2 = 3\n",
        ))
        .unwrap();
        c.validate().unwrap();
        assert_eq!(c.spoofed_count("click", 2), 3);
        assert_eq!(c.spoofed_count("click", 1), 1);
        assert_eq!(c.spoofed_count("slide", 2), 3);
        assert_eq!(c.spoofed_count("slide", 1), 1);
        assert_eq!(c.spoofed_count("force", 2), 2);
        assert!(!c.spoof.actions_follow_spoof);
        let c: Config = toml::from_str(&minimal_toml("[spoof.click]\n2 = 0\n")).unwrap();
        c.validate().unwrap();
        assert_eq!(c.spoofed_count("click", 2), 2);
        let c: Config = toml::from_str(&minimal_toml("[spoof.click]\n2 = 6\n")).unwrap();
        assert!(c.validate().is_err());
        let bad: std::result::Result<Config, _> =
            toml::from_str(&minimal_toml("[spoof.click]\nfoo = 3\n"));
        assert!(bad.is_err());
        let unknown: std::result::Result<Config, _> = toml::from_str(&minimal_toml(
            "[spoof.click]\n2 = 3\n[spoof.bogus]\n1 = 2\n",
        ));
        assert!(unknown.is_err());
        // New key parses both values (TOML-safe: parent first).
        for value in ["true", "false"] {
            let c: Config = toml::from_str(&minimal_toml(&format!(
                "[spoof]\nactions_follow_spoof = {value}\n"
            )))
            .unwrap();
            c.validate().unwrap();
            assert_eq!(c.spoof.actions_follow_spoof, value == "true");
        }
    }

    #[test]
    fn transition_mode_validates() {
        let c = Config::default();
        assert!(c.is_cut_transition());
        let mut continuous = Config::default();
        continuous.force.transition_mode = "continuous".into();
        assert!(continuous.validate().is_ok());
        assert!(!continuous.is_cut_transition());
        let mut cut = Config::default();
        cut.force.transition_mode = "cut".into();
        assert!(cut.validate().is_ok());
        assert!(cut.is_cut_transition());
        let mut bad = Config::default();
        bad.force.transition_mode = "restart".into();
        assert!(bad.validate().is_err());
        let mut bad = Config::default();
        bad.force.transition_mode = "CUT".into();
        assert!(bad.validate().is_err(), "mode is exact lowercase");
    }

    #[test]
    fn combo_stagger_ms_defaults_and_validates() {
        assert_eq!(Config::default().force.combo_stagger_ms, 50);
        for ok in [0, 50, 500] {
            let mut c = Config::default();
            c.force.combo_stagger_ms = ok;
            assert!(c.validate().is_ok(), "{ok} must validate");
        }
        let mut bad = Config::default();
        bad.force.combo_stagger_ms = 501;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn rejects_force_offset_and_haptic_out_of_range() {
        let mut bad = Config::default();
        bad.force.force_offset = 256;
        assert!(bad.validate().is_err());
        let mut bad = Config::default();
        bad.haptics.force_haptic = Some(999);
        assert!(bad.validate().is_err());
        let mut ok = Config::default();
        ok.force.force_offset = 0;
        ok.haptics.force_haptic = Some(0);
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn force_override_fallback() {
        let c = Config::default();
        assert_eq!(c.effective_force_haptic(), c.haptics.haptic);
        let mut c = Config::default();
        c.haptics.haptic = 5;
        c.haptics.force_haptic = Some(9);
        c.validate().unwrap();
        assert_eq!(c.effective_force_haptic(), 9);
        let mut armed = Config::default();
        armed.force.force_offset = 20;
        assert!(armed.force_enabled());
    }

    #[test]
    fn force_threshold_value_single_source() {
        // Disabled stays silent.
        assert_eq!(force_threshold_value(30, 0), None);
        // Owner tune: 30 + 130 = 160.
        assert_eq!(force_threshold_value(30, 130), Some(160));
        // Clamps mirror the press path: sensitivity 0 -> 1, offset capped at 255.
        assert_eq!(force_threshold_value(0, 300), Some(1 + 255));
        // Unvalidated max never overflows: 255 + 255 = 510.
        assert_eq!(force_threshold_value(255, 255), Some(510));
    }

    #[test]
    fn click_and_force_accessors() {
        let c = Config::default();
        assert_eq!(
            c.click_action(1).unwrap().kind,
            ActionKind::Keys(vec!["left".to_string()])
        );
        assert_eq!(c.click_action(4).unwrap().kind, ActionKind::None);
        assert_eq!(c.force_action(5).unwrap().kind, ActionKind::None);
        assert!(c.click_action(0).is_err());
        assert!(c.click_action(6).is_err());
        assert!(c.force_action(6).is_err());
    }

    #[test]
    fn parses_full_sectioned_toml() {
        let text = minimal_toml(
            r#"
log_level = "debug"
enhanced_clicks = true
[device]
polling_rate = 90
[pointer]
speed = 120
[click]
sensitivity = 80
margin = 8
click_1 = "left"
click_2 = "ctrl+c"
click_3 = "middle"
click_4 = "back"
click_5 = "empty"
[haptics]
haptic = 7
quiet_click = true
force_haptic = 9
force_quiet_click = true
[force]
force_offset = 20
force_1 = "forward"
force_2 = "none"
force_3 = "none"
force_4 = "none"
force_5 = "none"
transition_mode = "cut"
combo_stagger_ms = 50
[slide]
slide_1 = "none"
"#,
        );
        let c: Config = toml::from_str(&text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.log_level, "debug");
        assert_eq!(c.device.polling_rate, 90);
        assert!(c.enhanced_clicks);
        assert!(c.force_enabled());
        assert_eq!(c.effective_force_haptic(), 9);
        assert_eq!(
            c.click_action(2).unwrap().kind,
            ActionKind::Keys(vec!["ctrl".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn parses_gesture_power_toml() {
        let text = minimal_toml(
            r#"
[click]
click_1 = "left"
[force]
force_offset = 20
force_1 = "left+super"
transition_mode = "cut"
[slide]
slide_1 = "left"
slide_2 = "ctrl+c"
[spoof.click]
2 = 3
[spoof.slide]
1 = 2
[spoof.force]
1 = 1
[spoof]
actions_follow_spoof = true
"#,
        );
        let c: Config = toml::from_str(&text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.slide.slide_1.action, "left");
        assert_eq!(c.spoofed_count("click", 2), 3);
        assert_eq!(c.select_count("click", 2), 3);
        assert!(c.is_cut_transition());
    }

    fn binding_sections() -> Vec<(&'static str, &'static str)> {
        ["click", "force", "slide"]
            .into_iter()
            .flat_map(|level| (1..=5).map(move |n| (level, n)))
            .map(|(level, n)| {
                let name: &'static str = Box::leak(format!("{level}_{n}").into_boxed_str());
                (level, name)
            })
            .collect()
    }

    #[test]
    fn all_bindings_accept_objects_and_omission_equals_string() {
        for (section, name) in binding_sections() {
            for action in [
                "left",
                "super+left",
                "alt+f4",
                "control+right",
                "none",
                "empty",
            ] {
                let legacy: Config =
                    toml::from_str(&toml_in(section, name, &format!("\"{action}\""))).unwrap();
                let expanded: Config = toml::from_str(&toml_in(
                    section,
                    name,
                    &format!("{{ action = \"{action}\" }}"),
                ))
                .unwrap();
                legacy.validate().unwrap();
                expanded.validate().unwrap();
                assert_eq!(
                    legacy, expanded,
                    "{section}.{name}: omitted timing stays omitted"
                );
                for ms in [0, 50, 500] {
                    let timed: Config = toml::from_str(&toml_in(
                        section,
                        name,
                        &format!("{{ action = \"{action}\", stagger_ms = {ms} }}"),
                    ))
                    .unwrap();
                    timed.validate().unwrap();
                    let n: u8 = name.split_once('_').unwrap().1.parse().unwrap();
                    let parsed = match section {
                        "click" => timed.click_action(n),
                        "force" => timed.force_action(n),
                        _ => timed.slide_action(n),
                    }
                    .unwrap();
                    assert_eq!(parsed.stagger_ms, Some(ms), "{section}.{name}");
                    assert_eq!(parsed.kind, parse_click_action(action).unwrap().kind);
                }
            }
        }
    }

    #[test]
    fn bad_binding_objects_name_the_binding_and_problem() {
        for (section, name) in binding_sections() {
            for (value, problem) in [
                ("{}", "action"),
                ("{ stagger_ms = 50 }", "action"),
                ("{ action = 3 }", "string"),
                ("{ action = true }", "string"),
                ("{ action = [] }", "string"),
                ("{ action = \"left\", stagger_ms = -1 }", "u16"),
                ("{ action = \"left\", stagger_ms = 501 }", "0..=500"),
                ("{ action = \"left\", stagger_ms = 65536 }", "u16"),
                ("{ action = \"left\", stagger_ms = \"50\" }", "u16"),
                ("{ action = \"left\", stagger_ms = 1.5 }", "u16"),
                ("{ action = \"left\", stagger_ms = true }", "u16"),
                ("{ action = \"left\", stagger_ms = {} }", "u16"),
                ("{ action = \"left\", typo = 50 }", "unknown field"),
                ("{ action = \"ctrl++c\" }", "empty key"),
                ("{ action = \"bogus\" }", "unknown key"),
                ("{ action = \"\" }", "empty click"),
                ("50", "action string"),
                ("[]", "action string"),
            ] {
                let text = format!("schema_version = 1\n[{section}]\n{name} = {value}\n");
                let result = toml::from_str::<Config>(&text)
                    .map_err(anyhow::Error::from)
                    .and_then(|cfg| cfg.validate());
                let error = result.unwrap_err().to_string();
                assert!(error.contains(name), "missing binding: {error}");
                assert!(error.contains(problem), "missing {problem}: {error}");
            }
        }
    }

    #[test]
    fn documentation_examples_use_the_real_config_reader() {
        let doc = include_str!("../docs/CONFIG.md");
        for block in doc.split("```toml\n").skip(1) {
            let text = block.split("```").next().unwrap();
            let cfg: Config = toml::from_str(text).unwrap_or_else(|e| panic!("{text}: {e}"));
            cfg.validate().unwrap_or_else(|e| panic!("{text}: {e}"));
        }
    }

    #[test]
    fn shipped_default_toml_validates() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/config/default.toml");
        let cfg = load_or_default(Some(path)).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn none_path_gives_defaults() {
        assert_eq!(load_or_default(None).unwrap(), Config::default());
    }

    #[test]
    fn env_config_propagates_non_default_values() {
        let _env_guard = crate::test_env::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ltd.toml");
        std::fs::write(
            &path,
            "schema_version = 1\nenhanced_clicks = true\n[device]\npolling_rate = 90\n",
        )
        .unwrap();
        let old = std::env::var(CONFIG_ENV).ok();
        unsafe { std::env::set_var(CONFIG_ENV, &path) };
        let cfg = load_or_default_with_env(None).unwrap();
        match old {
            Some(v) => unsafe { std::env::set_var(CONFIG_ENV, v) },
            None => unsafe { std::env::remove_var(CONFIG_ENV) },
        }
        assert!(cfg.enhanced_clicks);
        assert_eq!(cfg.device.polling_rate, 90);
    }

    #[test]
    fn explicit_path_wins_over_env() {
        let _env_guard = crate::test_env::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("env.toml");
        let cli_path = dir.path().join("cli.toml");
        std::fs::write(&env_path, "schema_version = 1\nenhanced_clicks = true\n").unwrap();
        std::fs::write(&cli_path, "schema_version = 1\nenhanced_clicks = false\n").unwrap();
        let old = std::env::var(CONFIG_ENV).ok();
        unsafe { std::env::set_var(CONFIG_ENV, &env_path) };
        let cfg = load_or_default_with_env(Some(cli_path.to_str().unwrap())).unwrap();
        match old {
            Some(v) => unsafe { std::env::set_var(CONFIG_ENV, v) },
            None => unsafe { std::env::remove_var(CONFIG_ENV) },
        }
        assert!(!cfg.enhanced_clicks);
    }

    #[test]
    fn missing_env_file_falls_back_to_defaults() {
        let _env_guard = crate::test_env::lock_env();
        let old = std::env::var(CONFIG_ENV).ok();
        unsafe { std::env::set_var(CONFIG_ENV, "/nonexistent-ltd-config-xyz.toml") };
        let cfg = load_or_default_with_env(None).unwrap();
        match old {
            Some(v) => unsafe { std::env::set_var(CONFIG_ENV, v) },
            None => unsafe { std::env::remove_var(CONFIG_ENV) },
        }
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn missing_explicit_file_errors() {
        assert!(load_or_default(Some("/nonexistent-ltd-config-xyz.toml")).is_err());
    }

    #[test]
    fn invalid_file_values_fail_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "schema_version = 1\n[device]\npolling_rate = 500\n").unwrap();
        assert!(load_or_default(Some(path.to_str().unwrap())).is_err());
        let path = dir.path().join("badkey.toml");
        std::fs::write(
            &path,
            "schema_version = 1\n[click]\nclick_1 = \"frobnicate\"\n",
        )
        .unwrap();
        assert!(load_or_default(Some(path.to_str().unwrap())).is_err());
    }
}
