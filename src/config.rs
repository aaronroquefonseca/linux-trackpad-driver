//! Full TOML config schema for the driver.
//!
//! Phase-1 `config-schema` owner. Covers every key in
//! [`docs/ARCHITECTURE.md`](../../../docs/ARCHITECTURE.md) §"Config schema":
//! log level, capped polling rate, the `enhanced_clicks` opt-in gate,
//! pressure/haptic/speed tuning, 1–5 finger click maps (buttons, `none`,
//! `empty`, `+`-joined key combos), and the optional force-click level.
//!
//! Range-checked fields use a type wider than their valid range (`u16` for
//! byte-ranged values) so [`Config::validate`] can reject out-of-range
//! input with a clear error instead of failing opaquely at TOML parse.
//! (`u8` fields would make e.g. `sensitivity = 300` an unrepresentable
//! value that never reaches validation.)

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

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

/// Parsed daemon configuration (full schema).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `debug|info|warn|error`. Defaults to `info`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Requested polling Hz, `1..=110` (capped device maximum). Default 110.
    #[serde(default = "default_polling")]
    pub polling_rate: u32,
    /// Opt-in gate for the entire clicks pipeline. Default OFF; init works
    /// identically either way (passthrough via kernel/libinput).
    #[serde(default)]
    pub enhanced_clicks: bool,
    /// Press pressure threshold. `1..=255`. Default 64.
    #[serde(default = "default_sensitivity")]
    pub sensitivity: u16,
    /// Release hysteresis: release at `sensitivity - margin`. `0..=255`.
    /// Default 5.
    #[serde(default = "default_margin")]
    pub margin: u16,
    /// Haptic output strength, `0` = silent. `0..=255`. Default 5.
    #[serde(default = "default_haptic")]
    pub haptic: u16,
    /// Soft haptic waveform. Default false.
    #[serde(default)]
    pub quiet_click: bool,
    /// Pointer speed percent (`100` = 1:1). `1..=1000`. Default 100.
    #[serde(default = "default_speed")]
    pub speed: u32,
    /// Finger-count press actions; see [`parse_click_action`]. Defaults
    /// `left/right/middle/none/none`.
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
    /// Second-level pressure offset: force fires at
    /// `sensitivity + force_offset`. `0` disables. `0..=255`. Default 0.
    #[serde(default)]
    pub force_offset: u16,
    /// Force-level haptic override; falls back to [`Config::haptic`] when
    /// unset. See [`Config::effective_force_haptic`].
    #[serde(default)]
    pub force_haptic: Option<u16>,
    /// Soft waveform for the force level. Default false.
    #[serde(default)]
    pub force_quiet_click: bool,
    /// Force-level finger-count actions. Defaults all `none`.
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
    /// Slide-level finger-count actions (touch-but-not-pressed: contact
    /// present, every pressure at/below the release threshold). Defaults
    /// all `none` (= no action, current behavior: sliding only moves the
    /// cursor). See [`crate::clicks::ClickMachine`] for handoff semantics.
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
    /// Finger-count spoofing: virtual count reported to the computer when
    /// the real touching count is N under a status (`0` = passthrough,
    /// report the real count; `1..=5` = spoof). Fifteen keys cover
    /// slide/click/force × 1..5 (e.g. `spoof_click_2 = 3`: a real
    /// two-finger press reports three contacts). The uinput touch stream
    /// (slots, `BTN_TOOL_*`) always uses the spoofed count; action
    /// selection uses the real count unless [`Config::spoof_select`].
    /// All default `0` (passthrough: zero behavior change).
    #[serde(default)]
    pub spoof_slide_1: u16,
    #[serde(default)]
    pub spoof_slide_2: u16,
    #[serde(default)]
    pub spoof_slide_3: u16,
    #[serde(default)]
    pub spoof_slide_4: u16,
    #[serde(default)]
    pub spoof_slide_5: u16,
    #[serde(default)]
    pub spoof_click_1: u16,
    #[serde(default)]
    pub spoof_click_2: u16,
    #[serde(default)]
    pub spoof_click_3: u16,
    #[serde(default)]
    pub spoof_click_4: u16,
    #[serde(default)]
    pub spoof_click_5: u16,
    #[serde(default)]
    pub spoof_force_1: u16,
    #[serde(default)]
    pub spoof_force_2: u16,
    #[serde(default)]
    pub spoof_force_3: u16,
    #[serde(default)]
    pub spoof_force_4: u16,
    #[serde(default)]
    pub spoof_force_5: u16,
    /// When true, `click_N`/`force_N`/`slide_N` selection uses the spoofed
    /// count instead of the real one. Default false: spoofing only changes
    /// what the computer sees, never which action fires.
    #[serde(default)]
    pub spoof_select: bool,
    /// Level-transition mode on a force edge over an active click:
    /// `"continuous"` (default, current behavior: level-1 keys stay held,
    /// force keys join) or `"cut"` (release ALL level-1/slide keys first
    /// with a SYN, then press the force combo fresh — so
    /// `force_1 = left+super` starts a window drag instead of extending a
    /// held left). Global single key; see [`crate::clicks::ClickMachine`].
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

fn default_log_level() -> String {
    "info".to_string()
}
fn default_polling() -> u32 {
    DEFAULT_POLLING_HZ
}
fn default_sensitivity() -> u16 {
    64
}
fn default_margin() -> u16 {
    5
}
fn default_haptic() -> u16 {
    5
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
    "continuous".to_string()
}
fn default_combo_stagger_ms() -> u16 {
    50
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            polling_rate: default_polling(),
            enhanced_clicks: false,
            sensitivity: default_sensitivity(),
            margin: default_margin(),
            haptic: default_haptic(),
            quiet_click: false,
            speed: default_speed(),
            click_1: default_click_1(),
            click_2: default_click_2(),
            click_3: default_click_3(),
            click_4: default_action_none(),
            click_5: default_action_none(),
            force_offset: 0,
            force_haptic: None,
            force_quiet_click: false,
            force_1: default_action_none(),
            force_2: default_action_none(),
            force_3: default_action_none(),
            force_4: default_action_none(),
            force_5: default_action_none(),
            slide_1: default_action_none(),
            slide_2: default_action_none(),
            slide_3: default_action_none(),
            slide_4: default_action_none(),
            slide_5: default_action_none(),
            spoof_slide_1: 0,
            spoof_slide_2: 0,
            spoof_slide_3: 0,
            spoof_slide_4: 0,
            spoof_slide_5: 0,
            spoof_click_1: 0,
            spoof_click_2: 0,
            spoof_click_3: 0,
            spoof_click_4: 0,
            spoof_click_5: 0,
            spoof_force_1: 0,
            spoof_force_2: 0,
            spoof_force_3: 0,
            spoof_force_4: 0,
            spoof_force_5: 0,
            spoof_select: false,
            transition_mode: default_transition_mode(),
            combo_stagger_ms: default_combo_stagger_ms(),
        }
    }
}

/// A flat action setting: legacy string or `{ action = "...", stagger_ms = N }`.
/// Missing timing deliberately stays distinct from an explicit zero.
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
    pub fn validate(&self) -> Result<()> {
        match self.log_level.as_str() {
            "debug" | "info" | "warn" | "error" => {}
            other => anyhow::bail!("invalid log_level '{other}' (want debug|info|warn|error)"),
        }
        if self.polling_rate == 0 || self.polling_rate > MAX_POLLING_HZ {
            anyhow::bail!(
                "invalid polling_rate {} (want 1..={MAX_POLLING_HZ}; device max ~110 Hz, 99 F2 0x23)",
                self.polling_rate
            );
        }
        if self.sensitivity == 0 || self.sensitivity > MAX_BYTE_VALUE {
            anyhow::bail!(
                "invalid sensitivity {} (want 1..={MAX_BYTE_VALUE})",
                self.sensitivity
            );
        }
        if self.margin > MAX_BYTE_VALUE {
            anyhow::bail!("invalid margin {} (want 0..={MAX_BYTE_VALUE})", self.margin);
        }
        if self.haptic > MAX_BYTE_VALUE {
            anyhow::bail!(
                "invalid haptic {} (want 0..={MAX_BYTE_VALUE}, 0 = silent)",
                self.haptic
            );
        }
        if self.speed < MIN_SPEED || self.speed > MAX_SPEED {
            anyhow::bail!(
                "invalid speed {} (want {MIN_SPEED}..={MAX_SPEED}, 100 = 1:1)",
                self.speed
            );
        }
        for (key, value) in [
            ("click_1", &self.click_1),
            ("click_2", &self.click_2),
            ("click_3", &self.click_3),
            ("click_4", &self.click_4),
            ("click_5", &self.click_5),
            ("force_1", &self.force_1),
            ("force_2", &self.force_2),
            ("force_3", &self.force_3),
            ("force_4", &self.force_4),
            ("force_5", &self.force_5),
            ("slide_1", &self.slide_1),
            ("slide_2", &self.slide_2),
            ("slide_3", &self.slide_3),
            ("slide_4", &self.slide_4),
            ("slide_5", &self.slide_5),
        ] {
            value
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid {key}: {e}"))?;
        }
        for (key, value) in [
            ("spoof_slide_1", self.spoof_slide_1),
            ("spoof_slide_2", self.spoof_slide_2),
            ("spoof_slide_3", self.spoof_slide_3),
            ("spoof_slide_4", self.spoof_slide_4),
            ("spoof_slide_5", self.spoof_slide_5),
            ("spoof_click_1", self.spoof_click_1),
            ("spoof_click_2", self.spoof_click_2),
            ("spoof_click_3", self.spoof_click_3),
            ("spoof_click_4", self.spoof_click_4),
            ("spoof_click_5", self.spoof_click_5),
            ("spoof_force_1", self.spoof_force_1),
            ("spoof_force_2", self.spoof_force_2),
            ("spoof_force_3", self.spoof_force_3),
            ("spoof_force_4", self.spoof_force_4),
            ("spoof_force_5", self.spoof_force_5),
        ] {
            if value > 5 {
                anyhow::bail!("invalid {key} {value} (want 0..=5, 0 = passthrough)");
            }
        }
        match self.transition_mode.as_str() {
            "continuous" | "cut" => {}
            other => anyhow::bail!("invalid transition_mode '{other}' (want continuous|cut)"),
        }
        if self.combo_stagger_ms > 500 {
            anyhow::bail!(
                "invalid combo_stagger_ms {} (want 0..=500)",
                self.combo_stagger_ms
            );
        }
        if self.force_offset > MAX_BYTE_VALUE {
            anyhow::bail!(
                "invalid force_offset {} (want 0..={MAX_BYTE_VALUE}, 0 = disabled)",
                self.force_offset
            );
        }
        if let Some(h) = self.force_haptic {
            if h > MAX_BYTE_VALUE {
                anyhow::bail!(
                    "invalid force_haptic {h} (want 0..={MAX_BYTE_VALUE}; omit to use haptic)"
                );
            }
        }
        Ok(())
    }

    /// Standard-level action for finger count `n` (`1..=5`).
    pub fn click_action(&self, n: u8) -> Result<ClickAction> {
        let raw = match n {
            1 => &self.click_1,
            2 => &self.click_2,
            3 => &self.click_3,
            4 => &self.click_4,
            5 => &self.click_5,
            _ => anyhow::bail!("finger count {n} out of range (want 1..=5)"),
        };
        raw.parse()
            .map_err(|e| anyhow::anyhow!("invalid click_{n}: {e}"))
    }

    /// Force-level action for finger count `n` (`1..=5`).
    pub fn force_action(&self, n: u8) -> Result<ClickAction> {
        let raw = match n {
            1 => &self.force_1,
            2 => &self.force_2,
            3 => &self.force_3,
            4 => &self.force_4,
            5 => &self.force_5,
            _ => anyhow::bail!("finger count {n} out of range (want 1..=5)"),
        };
        raw.parse()
            .map_err(|e| anyhow::anyhow!("invalid force_{n}: {e}"))
    }

    /// True when at least one `slide_N` maps an action (string-level
    /// check, no parse: anything but `none` — modulo case/whitespace —
    /// counts, including values that would fail validation, so an invalid
    /// value still reaches the parse path and errors instead of being
    /// silently skipped). The click machine uses this to skip slide
    /// selection entirely in the common all-`none` case.
    pub fn any_slide_mapped(&self) -> bool {
        [
            &self.slide_1,
            &self.slide_2,
            &self.slide_3,
            &self.slide_4,
            &self.slide_5,
        ]
        .iter()
        .any(|s| !s.action.trim().eq_ignore_ascii_case("none"))
    }

    /// Slide-level action for finger count `n` (`1..=5`).
    pub fn slide_action(&self, n: u8) -> Result<ClickAction> {
        let raw = match n {
            1 => &self.slide_1,
            2 => &self.slide_2,
            3 => &self.slide_3,
            4 => &self.slide_4,
            5 => &self.slide_5,
            _ => anyhow::bail!("finger count {n} out of range (want 1..=5)"),
        };
        raw.parse()
            .map_err(|e| anyhow::anyhow!("invalid slide_{n}: {e}"))
    }

    /// Raw spoof cell for (`status`, real count `n`): `0` = passthrough.
    /// `status` is `slide`/`click`/`force`; anything else returns `0`.
    /// Values above 5 can only come from an unvalidated `Config` (validation
    /// rejects them): debug builds assert, release falls back to `0`
    /// (passthrough) rather than emitting a bogus count.
    pub fn spoof_cell(&self, status: &str, n: u8) -> u16 {
        let cell = match (status, n) {
            ("slide", 1) => self.spoof_slide_1,
            ("slide", 2) => self.spoof_slide_2,
            ("slide", 3) => self.spoof_slide_3,
            ("slide", 4) => self.spoof_slide_4,
            ("slide", 5) => self.spoof_slide_5,
            ("click", 1) => self.spoof_click_1,
            ("click", 2) => self.spoof_click_2,
            ("click", 3) => self.spoof_click_3,
            ("click", 4) => self.spoof_click_4,
            ("click", 5) => self.spoof_click_5,
            ("force", 1) => self.spoof_force_1,
            ("force", 2) => self.spoof_force_2,
            ("force", 3) => self.spoof_force_3,
            ("force", 4) => self.spoof_force_4,
            ("force", 5) => self.spoof_force_5,
            _ => 0,
        };
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
    /// [`Config::spoof_select`] is on, else the real count.
    pub fn select_count(&self, status: &str, real: u8) -> u8 {
        if self.spoof_select {
            self.spoofed_count(status, real)
        } else {
            real
        }
    }

    /// True when a force edge cuts level-1/slide keys first (`"cut"`);
    /// false is `"continuous"` (default: keys stay held, force joins).
    pub fn is_cut_transition(&self) -> bool {
        self.transition_mode == "cut"
    }

    /// Whether the second pressure level is armed (`force_offset > 0`).
    pub fn force_enabled(&self) -> bool {
        self.force_offset > 0
    }

    /// Haptic strength for the force level: explicit `force_haptic` override,
    /// else the standard [`Config::haptic`] value.
    pub fn effective_force_haptic(&self) -> u16 {
        self.force_haptic.unwrap_or(self.haptic)
    }
}

/// Load config from `path`, or defaults when `None`. Missing file with an
/// explicit path is an error; `None` never fails. Unknown keys are rejected
/// (likely typos); omitted keys take documented defaults, so Phase-0
/// subset files (`log_level`, `polling_rate`, `enhanced_clicks`) keep
/// parsing unchanged.
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

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.log_level, "info");
        assert_eq!(c.polling_rate, 110);
        assert!(!c.enhanced_clicks);
        assert_eq!(c.sensitivity, 64);
        assert_eq!(c.margin, 5);
        assert_eq!(c.haptic, 5);
        assert!(!c.quiet_click);
        assert_eq!(c.speed, 100);
        assert_eq!(c.click_1.action, "left");
        assert_eq!(c.click_2.action, "right");
        assert_eq!(c.click_3.action, "middle");
        assert_eq!(c.click_4.action, "none");
        assert_eq!(c.click_5.action, "none");
        assert_eq!(c.force_offset, 0);
        assert_eq!(c.force_haptic, None);
        assert!(!c.force_quiet_click);
        for f in [&c.force_1, &c.force_2, &c.force_3, &c.force_4, &c.force_5] {
            assert_eq!(f.action, "none");
        }
        for s in [&c.slide_1, &c.slide_2, &c.slide_3, &c.slide_4, &c.slide_5] {
            assert_eq!(s.action, "none");
        }
        for v in [
            c.spoof_slide_1,
            c.spoof_slide_2,
            c.spoof_slide_3,
            c.spoof_slide_4,
            c.spoof_slide_5,
            c.spoof_click_1,
            c.spoof_click_2,
            c.spoof_click_3,
            c.spoof_click_4,
            c.spoof_click_5,
            c.spoof_force_1,
            c.spoof_force_2,
            c.spoof_force_3,
            c.spoof_force_4,
            c.spoof_force_5,
        ] {
            assert_eq!(v, 0);
        }
        assert!(!c.spoof_select);
        assert_eq!(c.transition_mode, "continuous");
        assert!(!c.is_cut_transition());
        c.validate().unwrap();
        assert!(!c.force_enabled());
        // No override → falls back to standard haptic.
        assert_eq!(c.effective_force_haptic(), 5);
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
            let bad = Config {
                polling_rate: rate,
                ..Config::default()
            };
            assert!(bad.validate().is_err(), "rate {rate} must be rejected");
        }
        // Boundary still accepted.
        for rate in [1, 60, 110] {
            let ok = Config {
                polling_rate: rate,
                ..Config::default()
            };
            assert!(ok.validate().is_ok(), "rate {rate} must be accepted");
        }
    }

    #[test]
    fn rejects_sensitivity_outside_1_255() {
        for v in [0, 256, 1000] {
            let bad = Config {
                sensitivity: v,
                ..Config::default()
            };
            assert!(bad.validate().is_err(), "sensitivity {v} must be rejected");
        }
        for v in [1, 64, 255] {
            let ok = Config {
                sensitivity: v,
                ..Config::default()
            };
            assert!(ok.validate().is_ok(), "sensitivity {v} must be accepted");
        }
    }

    #[test]
    fn rejects_margin_above_255() {
        let bad = Config {
            margin: 256,
            ..Config::default()
        };
        assert!(bad.validate().is_err());
        let ok = Config {
            margin: 0,
            ..Config::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn rejects_haptic_above_255() {
        let bad = Config {
            haptic: 300,
            ..Config::default()
        };
        assert!(bad.validate().is_err());
        // 0 = silent is valid.
        let ok = Config {
            haptic: 0,
            ..Config::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn rejects_speed_outside_1_1000() {
        for v in [0, 1001, 10_000] {
            let bad = Config {
                speed: v,
                ..Config::default()
            };
            assert!(bad.validate().is_err(), "speed {v} must be rejected");
        }
        for v in [1, 100, 1000] {
            let ok = Config {
                speed: v,
                ..Config::default()
            };
            assert!(ok.validate().is_ok(), "speed {v} must be accepted");
        }
    }

    #[test]
    fn rejects_bad_click_values() {
        for bad_value in ["doubleclick", "ctrl++c", "ctrl+", "", "left click"] {
            let bad = Config {
                click_1: bad_value.into(),
                ..Config::default()
            };
            assert!(
                bad.validate().is_err(),
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
            let ok = Config {
                click_4: good.into(),
                ..Config::default()
            };
            assert!(ok.validate().is_ok(), "click_4 '{good}' must be accepted");
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
        let bad = Config {
            force_2: "bogus-key".into(),
            ..Config::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn slide_actions_validate_like_click() {
        let bad = Config {
            slide_2: "bogus-key".into(),
            ..Config::default()
        };
        assert!(bad.validate().is_err());
        let ok = Config {
            slide_1: "left".into(),
            slide_2: "ctrl+c".into(),
            slide_3: "empty".into(),
            ..Config::default()
        };
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
        let mapped = Config {
            slide_3: "middle".into(),
            ..Config::default()
        };
        assert!(mapped.any_slide_mapped());
        // Case/whitespace variants of none still count as unmapped…
        let noneish = Config {
            slide_1: " NONE ".into(),
            ..Config::default()
        };
        assert!(!noneish.any_slide_mapped());
        // …while invalid values count as mapped so the parse path still
        // errors instead of silently skipping them.
        let bad = Config {
            slide_1: "bogus-key".into(),
            ..Config::default()
        };
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
            let bad = Config {
                spoof_click_2: v,
                ..Config::default()
            };
            assert!(
                bad.validate().is_err(),
                "spoof_click_2 = {v} must be rejected"
            );
        }
        for v in [0, 1, 3, 5] {
            let ok = Config {
                spoof_click_2: v,
                spoof_slide_1: v,
                spoof_force_5: v,
                ..Config::default()
            };
            assert!(ok.validate().is_ok(), "spoof = {v} must be accepted");
        }
    }

    #[test]
    fn spoof_cells_default_passthrough() {
        let c = Config::default();
        assert_eq!(c.spoofed_count("click", 2), 2);
        assert_eq!(c.select_count("click", 2), 2);
        let c = Config {
            spoof_click_2: 3,
            ..Config::default()
        };
        c.validate().unwrap();
        // Wire sees 3; selection still sees the real 2 unless spoof_select.
        assert_eq!(c.spoofed_count("click", 2), 3);
        assert_eq!(c.select_count("click", 2), 2);
        assert_eq!(c.spoofed_count("click", 1), 1);
        assert_eq!(c.spoofed_count("force", 2), 2);
        let c = Config {
            spoof_select: true,
            ..c
        };
        assert_eq!(c.select_count("click", 2), 3);
    }

    #[test]
    fn transition_mode_validates() {
        let c = Config::default();
        assert!(!c.is_cut_transition());
        let cut = Config {
            transition_mode: "cut".into(),
            ..Config::default()
        };
        assert!(cut.validate().is_ok());
        assert!(cut.is_cut_transition());
        let bad = Config {
            transition_mode: "restart".into(),
            ..Config::default()
        };
        assert!(bad.validate().is_err());
        let bad = Config {
            transition_mode: "CUT".into(),
            ..Config::default()
        };
        assert!(bad.validate().is_err(), "mode is exact lowercase");
    }

    #[test]
    fn combo_stagger_ms_defaults_and_validates() {
        assert_eq!(Config::default().combo_stagger_ms, 50);
        for ok in [0, 50, 500] {
            let c = Config {
                combo_stagger_ms: ok,
                ..Config::default()
            };
            assert!(c.validate().is_ok(), "{ok} must validate");
        }
        let bad = Config {
            combo_stagger_ms: 501,
            ..Config::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn rejects_force_offset_and_haptic_out_of_range() {
        let bad = Config {
            force_offset: 256,
            ..Config::default()
        };
        assert!(bad.validate().is_err());
        let bad = Config {
            force_haptic: Some(999),
            ..Config::default()
        };
        assert!(bad.validate().is_err());
        // 0 disables the force level; explicit 0 haptic override is valid.
        let ok = Config {
            force_offset: 0,
            force_haptic: Some(0),
            ..Config::default()
        };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn force_override_fallback() {
        let c = Config::default();
        assert_eq!(c.effective_force_haptic(), c.haptic);
        let c = Config {
            haptic: 5,
            force_haptic: Some(9),
            ..Config::default()
        };
        c.validate().unwrap();
        assert_eq!(c.effective_force_haptic(), 9);
        let armed = Config {
            force_offset: 20,
            ..Config::default()
        };
        assert!(armed.force_enabled());
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
    fn parses_full_toml() {
        let text = r#"
log_level = "debug"
polling_rate = 90
enhanced_clicks = true
sensitivity = 80
margin = 8
haptic = 7
quiet_click = true
speed = 120
click_1 = "left"
click_2 = "ctrl+c"
click_3 = "middle"
click_4 = "back"
click_5 = "empty"
force_offset = 20
force_haptic = 9
force_quiet_click = true
force_1 = "forward"
force_2 = "none"
force_3 = "none"
force_4 = "none"
force_5 = "none"
"#;
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.log_level, "debug");
        assert_eq!(c.polling_rate, 90);
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
        let text = r#"
slide_1 = "left"
slide_2 = "ctrl+c"
slide_3 = "empty"
spoof_click_2 = 3
spoof_slide_1 = 2
spoof_force_1 = 1
spoof_select = true
transition_mode = "cut"
"#;
        let c: Config = toml::from_str(text).unwrap();
        c.validate().unwrap();
        assert_eq!(c.slide_1.action, "left");
        assert_eq!(c.spoofed_count("click", 2), 3);
        assert_eq!(c.select_count("click", 2), 3);
        assert!(c.is_cut_transition());
    }

    #[test]
    fn phase0_subset_still_parses() {
        // Backward compat: files with only the Phase-0 keys take defaults
        // for everything else.
        let c: Config = toml::from_str("polling_rate = 90\nenhanced_clicks = false\n").unwrap();
        assert_eq!(c.polling_rate, 90);
        assert_eq!(c.sensitivity, 64);
        assert_eq!(c.click_1.action, "left");
        assert_eq!(c.force_offset, 0);
        c.validate().unwrap();
    }

    #[test]
    fn unknown_keys_rejected() {
        let r = toml::from_str::<Config>("sensitivty = 64\n");
        assert!(r.is_err());
    }

    #[test]
    fn invalid_file_values_fail_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "polling_rate = 500\n").unwrap();
        assert!(load_or_default(Some(path.to_str().unwrap())).is_err());
        let path = dir.path().join("badkey.toml");
        std::fs::write(&path, "click_1 = \"frobnicate\"\n").unwrap();
        assert!(load_or_default(Some(path.to_str().unwrap())).is_err());
    }

    fn binding_names() -> Vec<String> {
        ["click", "force", "slide"]
            .into_iter()
            .flat_map(|level| (1..=5).map(move |n| format!("{level}_{n}")))
            .collect()
    }

    #[test]
    fn all_bindings_accept_objects_and_omission_equals_legacy_string() {
        for name in binding_names() {
            for action in [
                "left",
                "super+left",
                "alt+f4",
                "control+right",
                "none",
                "empty",
            ] {
                let legacy: Config = toml::from_str(&format!("{name} = \"{action}\"")).unwrap();
                let expanded: Config =
                    toml::from_str(&format!("{name} = {{ action = \"{action}\" }}")).unwrap();
                legacy.validate().unwrap();
                expanded.validate().unwrap();
                assert_eq!(legacy, expanded, "{name}: omitted timing must stay omitted");
                for ms in [0, 50, 500] {
                    let timed: Config = toml::from_str(&format!(
                        "{name} = {{ action = \"{action}\", stagger_ms = {ms} }}"
                    ))
                    .unwrap();
                    timed.validate().unwrap();
                    let (level, n) = name.split_once('_').unwrap();
                    let n = n.parse().unwrap();
                    let parsed = match level {
                        "click" => timed.click_action(n),
                        "force" => timed.force_action(n),
                        _ => timed.slide_action(n),
                    }
                    .unwrap();
                    assert_eq!(parsed.stagger_ms, Some(ms), "{name}");
                    assert_eq!(parsed.kind, parse_click_action(action).unwrap().kind);
                }
            }
        }
    }

    #[test]
    fn bad_binding_objects_name_the_binding_and_problem() {
        for name in binding_names() {
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
                let result = toml::from_str::<Config>(&format!("{name} = {value}"))
                    .map_err(anyhow::Error::from)
                    .and_then(|cfg| cfg.validate());
                let error = result.unwrap_err().to_string();
                assert!(error.contains(&name), "missing binding: {error}");
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
        // The reconnect path loads via env (udev passes no -c): a non-default
        // value in the env-pointed file must reach the caller.
        // Holds the shared env lock: set_var racing any concurrent env
        // read is unsound under the threaded harness.
        let _env_guard = crate::test_env::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ltd.toml");
        std::fs::write(&path, "enhanced_clicks = true\npolling_rate = 90\n").unwrap();
        let old = std::env::var(CONFIG_ENV).ok();
        unsafe { std::env::set_var(CONFIG_ENV, &path) };
        let cfg = load_or_default_with_env(None).unwrap();
        match old {
            Some(v) => unsafe { std::env::set_var(CONFIG_ENV, v) },
            None => unsafe { std::env::remove_var(CONFIG_ENV) },
        }
        assert!(cfg.enhanced_clicks);
        assert_eq!(cfg.polling_rate, 90);
    }

    #[test]
    fn explicit_path_wins_over_env() {
        // Serialized with all other env tests (see above).
        let _env_guard = crate::test_env::lock_env();
        let dir = tempfile::tempdir().unwrap();
        let env_path = dir.path().join("env.toml");
        let cli_path = dir.path().join("cli.toml");
        std::fs::write(&env_path, "enhanced_clicks = true\n").unwrap();
        std::fs::write(&cli_path, "enhanced_clicks = false\n").unwrap();
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
        // Serialized with all other env tests (see above).
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
}
