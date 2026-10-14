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
    pub click_1: String,
    #[serde(default = "default_click_2")]
    pub click_2: String,
    #[serde(default = "default_click_3")]
    pub click_3: String,
    #[serde(default = "default_action_none")]
    pub click_4: String,
    #[serde(default = "default_action_none")]
    pub click_5: String,
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
    pub force_1: String,
    #[serde(default = "default_action_none")]
    pub force_2: String,
    #[serde(default = "default_action_none")]
    pub force_3: String,
    #[serde(default = "default_action_none")]
    pub force_4: String,
    #[serde(default = "default_action_none")]
    pub force_5: String,
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
fn default_click_1() -> String {
    "left".to_string()
}
fn default_click_2() -> String {
    "right".to_string()
}
fn default_click_3() -> String {
    "middle".to_string()
}
fn default_action_none() -> String {
    "none".to_string()
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
        }
    }
}

/// A validated click/force action value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClickAction {
    /// Original string as written in config.
    pub raw: String,
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

/// Parse a `click_*`/`force_*` value: `left|right|middle|back|forward|none`
/// `|empty` or `+`-joined key names (e.g. `ctrl+c`). Case-insensitive;
/// surrounding whitespace is ignored. Returns a descriptive error naming
/// the offending key segment on failure.
pub fn parse_click_action(s: &str) -> std::result::Result<ClickAction, String> {
    let lowered = s.trim().to_lowercase();
    if lowered == "none" {
        return Ok(ClickAction {
            raw: s.to_string(),
            kind: ActionKind::None,
        });
    }
    if lowered == "empty" {
        return Ok(ClickAction {
            raw: s.to_string(),
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
        | "backspace" | "delete" | "home" | "end"
        | "pageup" | "pgup" | "pagedown" | "pgdn"
        | "up" | "arrow_up" | "down" | "arrow_down"
        | "left_arrow" | "right_arrow"
        // Media.
        | "volup" | "voldown" | "mute" | "play" | "pause"
        // Letters.
        | "a" | "b" | "c" | "d" | "e" | "f" | "g" | "h" | "i" | "j" | "k"
        | "l" | "m" | "n" | "o" | "p" | "q" | "r" | "s" | "t" | "u" | "v"
        | "w" | "x" | "y" | "z"
        // Digits.
        | "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"
        // Function row.
        | "f1" | "f2" | "f3" | "f4" | "f5" | "f6"
        | "f7" | "f8" | "f9" | "f10" | "f11" | "f12"
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
        ] {
            parse_click_action(value)
                .map_err(|e| anyhow::anyhow!("invalid {key} '{value}': {e}"))?;
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
        parse_click_action(raw).map_err(|e| anyhow::anyhow!("invalid click_{n} '{raw}': {e}"))
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
        parse_click_action(raw).map_err(|e| anyhow::anyhow!("invalid force_{n} '{raw}': {e}"))
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
        assert_eq!(c.click_1, "left");
        assert_eq!(c.click_2, "right");
        assert_eq!(c.click_3, "middle");
        assert_eq!(c.click_4, "none");
        assert_eq!(c.click_5, "none");
        assert_eq!(c.force_offset, 0);
        assert_eq!(c.force_haptic, None);
        assert!(!c.force_quiet_click);
        for f in [&c.force_1, &c.force_2, &c.force_3, &c.force_4, &c.force_5] {
            assert_eq!(f, "none");
        }
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
            "meta+space",
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
    fn rejects_bad_force_actions() {
        let bad = Config {
            force_2: "bogus-key".into(),
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
    fn phase0_subset_still_parses() {
        // Backward compat: files with only the Phase-0 keys take defaults
        // for everything else.
        let c: Config = toml::from_str("polling_rate = 90\nenhanced_clicks = false\n").unwrap();
        assert_eq!(c.polling_rate, 90);
        assert_eq!(c.sensitivity, 64);
        assert_eq!(c.click_1, "left");
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
