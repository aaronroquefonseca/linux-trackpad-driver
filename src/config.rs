//! Minimal config for Phase 0.
//!
//! Full TOML schema (click maps, force levels, validation errors) belongs to
//! the Phase-1 `config-schema` task. This file only carries what core-init
//! needs: log level, requested polling rate, and the opt-in gate for the
//! future clicks pipeline (parsed now, acted on later).

use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

/// Requested polling rate default (Hz). Capped device-maximum intent:
/// ~110 Hz (`X = 0x23 = 35`, `3850/35 ≈ 110`). Never request above the
/// demonstrated device ceiling — higher modes are unverified and the
/// device may NAK/ignore them (see [`crate::init`]).
pub const DEFAULT_POLLING_HZ: u32 = 110;

/// Parsed daemon configuration (Phase-0 subset).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// `debug|info|warn|error`. Defaults to `info`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Requested polling Hz, capped at device maximum (~110). Defaults to 110.
    #[serde(default = "default_polling")]
    pub polling_rate: u32,
    /// Opt-in gate for the entire clicks pipeline. Default OFF; init works
    /// identically either way (passthrough via kernel/libinput).
    #[serde(default)]
    pub enhanced_clicks: bool,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_polling() -> u32 {
    DEFAULT_POLLING_HZ
}

impl Default for Config {
    fn default() -> Self {
        Self {
            log_level: default_log_level(),
            polling_rate: default_polling(),
            enhanced_clicks: false,
        }
    }
}

impl Config {
    /// Validate user-facing values; returns a human-readable error.
    pub fn validate(&self) -> Result<()> {
        match self.log_level.as_str() {
            "debug" | "info" | "warn" | "error" => {}
            other => anyhow::bail!("invalid log_level '{other}' (want debug|info|warn|error)"),
        }
        if self.polling_rate == 0 || self.polling_rate > u16::MAX as u32 {
            anyhow::bail!(
                "invalid polling_rate {} (want 1..=65535)",
                self.polling_rate
            );
        }
        Ok(())
    }
}

/// Load config from `path`, or defaults when `None`. Missing file with an
/// explicit path is an error; `None` never fails.
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
        assert_eq!(c.polling_rate, 110);
        assert!(!c.enhanced_clicks);
        assert_eq!(c.log_level, "info");
        c.validate().unwrap();
    }

    #[test]
    fn rejects_bad_values() {
        let bad = Config {
            log_level: "verbose".into(),
            ..Config::default()
        };
        assert!(bad.validate().is_err());
        let bad = Config {
            polling_rate: 0,
            ..Config::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn parses_minimal_toml() {
        let c: Config = toml::from_str("polling_rate = 90\nenhanced_clicks = false\n").unwrap();
        assert_eq!(c.polling_rate, 90);
        c.validate().unwrap();
    }

    #[test]
    fn none_path_gives_defaults() {
        assert_eq!(load_or_default(None).unwrap(), Config::default());
    }
}
