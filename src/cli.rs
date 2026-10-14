//! CLI parsing + dispatch for `ltd run` / `ltd reconnect`.
//!
//! Pure parse functions (no I/O) so unit tests cover arg handling.
//! `dispatch` performs I/O and returns a process [`ExitCode`].

use std::process::ExitCode;

use crate::{config, daemon};

/// Parsed `run` invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct RunArgs {
    pub config_path: Option<String>,
}

/// Parsed `reconnect` invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct ReconnectArgs {
    pub devnode: String,
    pub force: bool,
    pub polling_override: Option<u32>,
    pub disengage: bool,
}

/// Top-level command after parsing.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(RunArgs),
    Reconnect(ReconnectArgs),
}

fn parse_polling_value(s: &str) -> Result<u32, String> {
    s.parse::<u32>()
        .map_err(|_| format!("invalid polling rate: '{s}'"))
        .and_then(|v| {
            if v == 0 || v > u16::MAX as u32 {
                Err(format!("invalid polling rate: '{s}' (want 1..=65535)"))
            } else {
                Ok(v)
            }
        })
}

/// Parse argv (including program name at index 0) into a [`Command`].
pub fn parse_args(args: &[String]) -> Result<Command, String> {
    let prog = args.first().map_or("ltd", String::as_str);
    let rest = args.get(1..).unwrap_or(&[]);
    let Some(cmd) = rest.first() else {
        return Err(format!("usage: {prog} <run|reconnect> [args...]"));
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => Err(format!(
            "usage:\n  {prog} run [-c config]\n  {prog} reconnect <hidraw-dev> [-f] [-p Hz] [-d]"
        )),
        "run" => {
            let mut config_path: Option<String> = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-c" | "--config" => {
                        i += 1;
                        let v = rest
                            .get(i)
                            .ok_or_else(|| "run: -c/--config requires a value".to_string())?;
                        config_path = Some(v.clone());
                        i += 1;
                    }
                    "-h" | "--help" => {
                        return Err(format!("usage: {prog} run [-c config]"));
                    }
                    other => return Err(format!("run: unknown option '{other}'")),
                }
            }
            Ok(Command::Run(RunArgs { config_path }))
        }
        "reconnect" => {
            let devnode = rest
                .get(1)
                .ok_or_else(|| format!("usage: {prog} reconnect <hidraw-dev> [-f] [-p Hz] [-d]"))?
                .clone();
            if devnode == "-h" || devnode == "--help" {
                return Err(format!(
                    "usage: {prog} reconnect <hidraw-dev> [-f] [-p Hz] [-d]"
                ));
            }
            let mut force = false;
            let mut disengage = false;
            let mut polling_override: Option<u32> = None;
            let mut i = 2;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-f" | "--force" => {
                        force = true;
                        i += 1;
                    }
                    "-d" | "--disengage" => {
                        disengage = true;
                        i += 1;
                    }
                    "-p" | "--polling-rate" => {
                        i += 1;
                        let v = rest.get(i).ok_or_else(|| {
                            "reconnect: -p/--polling-rate requires a value".to_string()
                        })?;
                        polling_override = Some(parse_polling_value(v)?);
                        i += 1;
                    }
                    other => return Err(format!("reconnect: unknown option '{other}'")),
                }
            }
            Ok(Command::Reconnect(ReconnectArgs {
                devnode,
                force,
                polling_override,
                disengage,
            }))
        }
        other => Err(format!("unknown command '{other}'; see '{prog} help'")),
    }
}

/// Parse + execute. Never panics on missing hardware; hardware errors map to
/// log output + nonzero exit, not panics.
pub fn dispatch(args: &[String]) -> anyhow::Result<ExitCode> {
    let cmd = parse_args(args).map_err(anyhow::Error::msg)?;
    match cmd {
        Command::Run(run) => {
            let cfg = config::load_or_default(run.config_path.as_deref())?;
            config::init_logging(&cfg.log_level);
            match daemon::run_daemon(&cfg) {
                Ok(()) => Ok(ExitCode::SUCCESS),
                Err(e) => {
                    log::error!("run failed: {e:#}");
                    Ok(ExitCode::from(1))
                }
            }
        }
        Command::Reconnect(rc) => {
            let cfg = config::load_or_default(None)?;
            config::init_logging(&cfg.log_level);
            match daemon::run_reconnect(
                &rc.devnode,
                rc.force,
                rc.polling_override,
                rc.disengage,
                &cfg,
            ) {
                Ok(()) => Ok(ExitCode::SUCCESS),
                Err(e) => {
                    log::error!("reconnect failed: {e:#}");
                    Ok(ExitCode::from(1))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn run_defaults() {
        assert_eq!(
            parse_args(&argv(&["ltd", "run"])),
            Ok(Command::Run(RunArgs { config_path: None }))
        );
    }

    #[test]
    fn run_with_config() {
        assert_eq!(
            parse_args(&argv(&["ltd", "run", "-c", "/tmp/x.toml"])),
            Ok(Command::Run(RunArgs {
                config_path: Some("/tmp/x.toml".into())
            }))
        );
    }

    #[test]
    fn reconnect_full_flags() {
        assert_eq!(
            parse_args(&argv(&[
                "ltd",
                "reconnect",
                "/dev/hidraw2",
                "-f",
                "-p",
                "500",
                "-d"
            ])),
            Ok(Command::Reconnect(ReconnectArgs {
                devnode: "/dev/hidraw2".into(),
                force: true,
                polling_override: Some(500),
                disengage: true,
            }))
        );
    }

    #[test]
    fn reconnect_requires_device() {
        assert!(parse_args(&argv(&["ltd", "reconnect"])).is_err());
    }

    #[test]
    fn reconnect_rejects_bad_polling() {
        assert!(parse_args(&argv(&["ltd", "reconnect", "/dev/hidraw0", "-p", "abc"])).is_err());
        assert!(parse_args(&argv(&["ltd", "reconnect", "/dev/hidraw0", "-p", "0"])).is_err());
    }

    #[test]
    fn unknown_command_errors() {
        assert!(parse_args(&argv(&["ltd", "frobnicate"])).is_err());
    }
}
