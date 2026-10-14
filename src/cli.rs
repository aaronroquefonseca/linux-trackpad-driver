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
    pub config_path: Option<String>,
}

/// Parsed `resume` invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct ResumeArgs {
    pub polling_override: Option<u32>,
    pub config_path: Option<String>,
}

/// Top-level command after parsing.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(RunArgs),
    Reconnect(ReconnectArgs),
    Resume(ResumeArgs),
}

/// Parse failure: usage/help text vs. a real error. Help exits 0.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseFailure {
    pub message: String,
    pub is_help: bool,
}

impl From<String> for ParseFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            is_help: false,
        }
    }
}

/// Top-level usage text (also the `--help` payload, which exits 0).
fn top_usage(prog: &str) -> ParseFailure {
    ParseFailure {
        message: format!(
            "usage:\n  {prog} run [-c config]\n  {prog} reconnect <hidraw-dev> [-f] [-p Hz] [-d] [-c config]\n  {prog} resume [-p Hz] [-c config]"
        ),
        is_help: true,
    }
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
/// Help output returns `Err(ParseFailure { is_help: true })` so [`dispatch`]
/// can exit 0 for it and 1 for real errors.
pub fn parse_args(args: &[String]) -> Result<Command, ParseFailure> {
    let prog = args.first().map_or("ltd", String::as_str);
    let rest = args.get(1..).unwrap_or(&[]);
    let Some(cmd) = rest.first() else {
        return Err(top_usage(prog));
    };
    match cmd.as_str() {
        "-h" | "--help" | "help" => Err(top_usage(prog)),
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
                        return Err(ParseFailure {
                            message: format!("usage: {prog} run [-c config]"),
                            is_help: true,
                        });
                    }
                    other => return Err(format!("run: unknown option '{other}'").into()),
                }
            }
            Ok(Command::Run(RunArgs { config_path }))
        }
        "reconnect" => {
            let devnode = rest
                .get(1)
                .ok_or_else(|| {
                    format!("usage: {prog} reconnect <hidraw-dev> [-f] [-p Hz] [-d] [-c config]")
                })?
                .clone();
            if devnode == "-h" || devnode == "--help" {
                return Err(ParseFailure {
                    message: format!(
                        "usage: {prog} reconnect <hidraw-dev> [-f] [-p Hz] [-d] [-c config]"
                    ),
                    is_help: true,
                });
            }
            let mut force = false;
            let mut disengage = false;
            let mut polling_override: Option<u32> = None;
            let mut config_path: Option<String> = None;
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
                    "-c" | "--config" => {
                        i += 1;
                        let v = rest
                            .get(i)
                            .ok_or_else(|| "reconnect: -c/--config requires a value".to_string())?;
                        config_path = Some(v.clone());
                        i += 1;
                    }
                    other => {
                        return Err(format!("reconnect: unknown option '{other}'").into());
                    }
                }
            }
            Ok(Command::Reconnect(ReconnectArgs {
                devnode,
                force,
                polling_override,
                disengage,
                config_path,
            }))
        }
        "resume" => {
            let mut polling_override: Option<u32> = None;
            let mut config_path: Option<String> = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-p" | "--polling-rate" => {
                        i += 1;
                        let v = rest.get(i).ok_or_else(|| {
                            "resume: -p/--polling-rate requires a value".to_string()
                        })?;
                        polling_override = Some(parse_polling_value(v)?);
                        i += 1;
                    }
                    "-c" | "--config" => {
                        i += 1;
                        let v = rest
                            .get(i)
                            .ok_or_else(|| "resume: -c/--config requires a value".to_string())?;
                        config_path = Some(v.clone());
                        i += 1;
                    }
                    "-h" | "--help" => {
                        return Err(ParseFailure {
                            message: format!("usage: {prog} resume [-p Hz] [-c config]"),
                            is_help: true,
                        });
                    }
                    other => {
                        return Err(format!("resume: unknown option '{other}'").into());
                    }
                }
            }
            Ok(Command::Resume(ResumeArgs {
                polling_override,
                config_path,
            }))
        }
        other => Err(format!("unknown command '{other}'; see '{prog} help'").into()),
    }
}

/// Parse + execute. Never panics on missing hardware; hardware errors map to
/// log output + nonzero exit, not panics. `--help` prints to stdout, exit 0.
pub fn dispatch(args: &[String]) -> anyhow::Result<ExitCode> {
    let cmd = match parse_args(args) {
        Ok(cmd) => cmd,
        Err(failure) if failure.is_help => {
            println!("{}", failure.message);
            return Ok(ExitCode::SUCCESS);
        }
        Err(failure) => return Err(anyhow::Error::msg(failure.message)),
    };
    match cmd {
        Command::Run(run) => {
            // Like reconnect/resume: $LTD_CONFIG (set by the main unit) wins
            // over defaults so the daemon, the hook, and the tick verify
            // against the same rate. Explicit -c still wins over env.
            let cfg = config::load_or_default_with_env(run.config_path.as_deref())?;
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
            // No -c on the udev command line: fall back to $LTD_CONFIG (set
            // by the reconnect template unit) so hotplug honors the system
            // config, including enhanced_clicks.
            let cfg = config::load_or_default_with_env(rc.config_path.as_deref())?;
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
        Command::Resume(rs) => {
            // System-sleep hook entry: same $LTD_CONFIG fallback as the
            // reconnect unit (the hook passes no -c either).
            let cfg = config::load_or_default_with_env(rs.config_path.as_deref())?;
            config::init_logging(&cfg.log_level);
            match daemon::run_resume(&cfg, rs.polling_override) {
                Ok(report) if !report.failed() => Ok(ExitCode::SUCCESS),
                Ok(report) => {
                    log::error!("resume failed for {} node(s)", report.errors.len());
                    Ok(ExitCode::from(1))
                }
                Err(e) => {
                    log::error!("resume failed: {e:#}");
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
                config_path: None,
            }))
        );
    }

    #[test]
    fn resume_defaults_and_flags() {
        assert_eq!(
            parse_args(&argv(&["ltd", "resume"])),
            Ok(Command::Resume(ResumeArgs {
                polling_override: None,
                config_path: None,
            }))
        );
        assert_eq!(
            parse_args(&argv(&["ltd", "resume", "-p", "90", "-c", "/tmp/x.toml"])),
            Ok(Command::Resume(ResumeArgs {
                polling_override: Some(90),
                config_path: Some("/tmp/x.toml".into()),
            }))
        );
        assert!(parse_args(&argv(&["ltd", "resume", "-p"])).is_err());
        assert!(parse_args(&argv(&["ltd", "resume", "-p", "abc"])).is_err());
        assert!(parse_args(&argv(&["ltd", "resume", "-p", "0"])).is_err());
        assert!(parse_args(&argv(&["ltd", "resume", "-f"])).is_err());
        assert!(
            parse_args(&argv(&["ltd", "resume", "--help"]))
                .unwrap_err()
                .is_help
        );
    }

    #[test]
    fn reconnect_with_config() {
        assert_eq!(
            parse_args(&argv(&[
                "ltd",
                "reconnect",
                "/dev/hidraw2",
                "-c",
                "/tmp/x.toml"
            ])),
            Ok(Command::Reconnect(ReconnectArgs {
                devnode: "/dev/hidraw2".into(),
                force: false,
                polling_override: None,
                disengage: false,
                config_path: Some("/tmp/x.toml".into()),
            }))
        );
        assert!(parse_args(&argv(&["ltd", "reconnect", "/dev/hidraw2", "-c"])).is_err());
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
        let err = parse_args(&argv(&["ltd", "frobnicate"])).unwrap_err();
        assert!(!err.is_help);
    }

    #[test]
    fn help_flags_are_help_not_errors() {
        for args in [
            vec!["ltd"],
            vec!["ltd", "help"],
            vec!["ltd", "--help"],
            vec!["ltd", "run", "--help"],
            vec!["ltd", "reconnect", "--help"],
        ] {
            let err = parse_args(&argv(&args)).unwrap_err();
            assert!(err.is_help, "args {args:?} should be help");
        }
    }

    #[test]
    fn dispatch_help_exits_zero() {
        assert_eq!(
            dispatch(&argv(&["ltd", "--help"])).unwrap(),
            ExitCode::SUCCESS
        );
    }
}
