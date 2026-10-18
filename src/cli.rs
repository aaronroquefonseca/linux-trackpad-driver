//! CLI parsing + dispatch for `ltd`.
//!
//! Pure parse functions (no I/O) so unit tests cover arg handling.
//! `dispatch` performs I/O and returns a process [`ExitCode`].

use std::process::ExitCode;

use crate::{config, daemon, discovery, lock};

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

/// Parsed `doctor` invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct DoctorArgs {
    pub config_path: Option<String>,
}

/// Top-level command after parsing.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Version,
    Run(RunArgs),
    Reconnect(ReconnectArgs),
    Resume(ResumeArgs),
    Doctor(DoctorArgs),
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
            "usage:\n  {prog} run [-c config]\n  {prog} reconnect <hidraw-dev> [-f] [-p Hz] [-d] [-c config]\n  {prog} resume [-p Hz] [-c config]\n  {prog} doctor [-c config]\n  {prog} --version | -V"
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

/// One scanned node plus read-only status for [`doctor_report`].
/// All fields come from sysfs/`/dev` stat checks — nothing here sends
/// reports or changes hardware state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorNode {
    pub devnode: String,
    pub vendor_id: u16,
    pub product_id: u16,
    pub transport: String,
    pub node_present: bool,
    pub driver_bound: bool,
}

/// Render the `doctor` first-run checklist. Pure (no I/O) so unit tests
/// cover the formatting.
///
/// Returns `(report, ok)`: `ok` is false when the config is invalid or no
/// trackpad node was seen. The socket state is informational only — a
/// stopped daemon is the expected pre-enable state, not a failure.
pub fn doctor_report(
    config_source: &str,
    config_error: Option<&str>,
    nodes: &[DoctorNode],
    socket_path: &str,
    socket_live: bool,
) -> (String, bool) {
    let mut out = format!("{}\n", crate::build_identity::IDENTITY);
    let mut ok = true;
    match config_error {
        None => out.push_str(&format!("[ok] config valid ({config_source})\n")),
        Some(e) => {
            out.push_str(&format!("[FAIL] config invalid ({config_source}): {e}\n"));
            ok = false;
        }
    }
    if nodes.is_empty() {
        out.push_str("[FAIL] no trackpad seen: no 05ac:0265/0324 or 004c:0265/0324 hidraw node\n");
        ok = false;
    } else {
        for n in nodes {
            let presence = if n.node_present { "present" } else { "MISSING" };
            let bind = if n.driver_bound {
                "driver bound"
            } else {
                "driver not (yet) bound"
            };
            out.push_str(&format!(
                "[ok] device seen: {} {:04x}:{:04x} ({}, {presence}, {bind})\n",
                n.devnode, n.vendor_id, n.product_id, n.transport
            ));
            if !n.node_present {
                ok = false;
            }
        }
    }
    if socket_live {
        out.push_str(&format!("[ok] daemon socket live ({socket_path})\n"));
    } else {
        out.push_str(&format!(
            "[..] daemon socket absent ({socket_path}); enable with: systemctl enable --now linux-trackpad-driver.service\n"
        ));
    }
    (out, ok)
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
        "--version" | "-V" => {
            if let Some(other) = rest.get(1) {
                return Err(format!("{cmd}: unexpected argument '{other}'").into());
            }
            Ok(Command::Version)
        }
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
        "doctor" => {
            let mut config_path: Option<String> = None;
            let mut i = 1;
            while i < rest.len() {
                match rest[i].as_str() {
                    "-c" | "--config" => {
                        i += 1;
                        let v = rest
                            .get(i)
                            .ok_or_else(|| "doctor: -c/--config requires a value".to_string())?;
                        config_path = Some(v.clone());
                        i += 1;
                    }
                    "-h" | "--help" => {
                        return Err(ParseFailure {
                            message: format!("usage: {prog} doctor [-c config]"),
                            is_help: true,
                        });
                    }
                    other => {
                        return Err(format!("doctor: unknown option '{other}'").into());
                    }
                }
            }
            Ok(Command::Doctor(DoctorArgs { config_path }))
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
        Command::Version => {
            println!("{}", crate::build_identity::IDENTITY);
            Ok(ExitCode::SUCCESS)
        }
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
        Command::Doctor(doc) => {
            // Read-only by construction: sysfs scan + stat checks + socket
            // dial only. Never sends feature reports, never grabs.
            let source = doc
                .config_path
                .clone()
                .or_else(config::config_path_from_env)
                .unwrap_or_else(|| "<built-in defaults>".to_string());
            let config_error = config::load_or_default_with_env(doc.config_path.as_deref())
                .err()
                .map(|e| format!("{e:#}"));
            let nodes: Vec<DoctorNode> = match discovery::scan_trackpads() {
                Ok(devs) => devs
                    .iter()
                    .map(|d| DoctorNode {
                        devnode: d.devnode.clone(),
                        vendor_id: d.vendor_id,
                        product_id: d.product_id,
                        transport: format!("{:?}", d.transport),
                        node_present: std::path::Path::new(&d.devnode).exists(),
                        driver_bound: discovery::driver_bound_for_node(&d.devnode),
                    })
                    .collect(),
                Err(e) => {
                    eprintln!("ltd doctor: sysfs scan failed: {e:#}");
                    Vec::new()
                }
            };
            let sock = lock::socket_path();
            let live = lock::daemon_socket_present(&sock);
            let (report, ok) = doctor_report(&source, config_error.as_deref(), &nodes, &sock, live);
            print!("{report}");
            if ok {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
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
            vec!["ltd", "doctor", "--help"],
        ] {
            let err = parse_args(&argv(&args)).unwrap_err();
            assert!(err.is_help, "args {args:?} should be help");
        }
    }

    #[test]
    fn doctor_defaults_and_flags() {
        assert_eq!(
            parse_args(&argv(&["ltd", "doctor"])),
            Ok(Command::Doctor(DoctorArgs { config_path: None }))
        );
        assert_eq!(
            parse_args(&argv(&["ltd", "doctor", "-c", "/tmp/x.toml"])),
            Ok(Command::Doctor(DoctorArgs {
                config_path: Some("/tmp/x.toml".into()),
            }))
        );
        assert!(parse_args(&argv(&["ltd", "doctor", "-c"])).is_err());
        assert!(parse_args(&argv(&["ltd", "doctor", "-f"])).is_err());
    }

    fn doctor_node() -> DoctorNode {
        DoctorNode {
            devnode: "/dev/hidraw2".to_string(),
            vendor_id: 0x05ac,
            product_id: 0x0324,
            transport: "Usb".to_string(),
            node_present: true,
            driver_bound: true,
        }
    }

    #[test]
    fn doctor_report_healthy() {
        let (report, ok) = doctor_report(
            "<built-in defaults>",
            None,
            &[doctor_node()],
            "/run/linux-trackpad-driver/driver.sock",
            true,
        );
        assert!(ok);
        assert_eq!(report.lines().next(), Some(crate::build_identity::IDENTITY));
        assert!(report.contains("[ok] config valid"));
        assert!(report.contains("/dev/hidraw2"));
        assert!(report.contains("05ac:0324"));
        assert!(report.contains("daemon socket live"));
    }

    #[test]
    fn doctor_report_no_device_fails_but_socket_absent_only_warns() {
        let (report, ok) = doctor_report("<built-in defaults>", None, &[], "/sock", false);
        assert!(!ok);
        assert!(report.contains("[FAIL] no trackpad seen"));
        assert!(report.contains("[..] daemon socket absent"));

        // Socket absent alone is not a failure (expected pre-enable state).
        let (_, ok) = doctor_report(
            "<built-in defaults>",
            None,
            &[doctor_node()],
            "/sock",
            false,
        );
        assert!(ok);
    }

    #[test]
    fn doctor_report_bad_config_and_missing_node_fail() {
        let (report, ok) = doctor_report(
            "/tmp/bad.toml",
            Some("invalid polling_rate 500 (want 1..=110)"),
            &[],
            "/sock",
            false,
        );
        assert!(!ok);
        assert!(report.contains("[FAIL] config invalid"));

        let mut gone = doctor_node();
        gone.node_present = false;
        let (_, ok) = doctor_report("<built-in defaults>", None, &[gone], "/sock", true);
        assert!(!ok);
    }

    #[test]
    fn dispatch_help_exits_zero() {
        assert_eq!(
            dispatch(&argv(&["ltd", "--help"])).unwrap(),
            ExitCode::SUCCESS
        );
    }
}
