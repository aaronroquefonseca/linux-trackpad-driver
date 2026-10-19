//! Read-only diagnostics, deliberately separate from startup/reconnect policy.
//!
//! Config *selection* is shared with startup
//! ([`crate::config::select_config_source_with_installed`]): explicit `-c` wins, else
//! nonempty `$LTD_CONFIG`, else the installed system file. Doctor always
//! reports the selected on-disk file (or the missing path it fell back
//! from), never the daemon's in-memory config.

use std::{io, path::Path};

use anyhow::{Context, Result};

use crate::config::{self, Config};

const UNIX_TABLE: &str = "/proc/net/unix";

/// Check the selected on-disk config, not the daemon's in-memory config.
/// The selection matrix is the shared [`config::select_config_source_with_installed`];
/// only the read/validate wrapper differs from startup loading.
pub fn check_config(explicit: Option<&str>) -> (String, Result<()>) {
    check_config_with(
        explicit,
        config::config_path_from_env().as_deref(),
        config::SYSTEM_CONFIG_PATH,
        config::read_config_text,
    )
}

fn check_config_with(
    explicit: Option<&str>,
    env: Option<&str>,
    installed: &str,
    read: impl FnOnce(&str) -> io::Result<Option<String>>,
) -> (String, Result<()>) {
    // Shared selection matrix with startup loading: explicit `-c` wins,
    // else nonempty `$LTD_CONFIG`, else the installed system file.
    let selected = config::select_config_source_with_installed(explicit, env, installed);
    let path = selected.path.as_str();
    match read(path) {
        Ok(None) if selected.kind != config::ConfigSourceKind::Explicit => {
            (config::fallback_config_source(path, selected.kind), Ok(()))
        }
        text => {
            let result = (|| {
                let text = text
                    .with_context(|| format!("reading config '{path}'"))?
                    .with_context(|| format!("config '{path}' does not exist"))?;
                let cfg: Config =
                    toml::from_str(&text).with_context(|| format!("parsing config '{path}'"))?;
                cfg.validate()
            })();
            (config::loaded_config_source(path), result)
        }
    }
}

/// A kernel-table observation, never an authenticated daemon health check.
#[derive(Debug, PartialEq, Eq)]
pub enum SocketStatus {
    Listening,
    NotObserved,
    Unknown(String),
}

pub fn socket_status(path: &str) -> SocketStatus {
    socket_status_from_table(path, std::fs::read_to_string(UNIX_TABLE))
}

fn socket_status_from_table(path: &str, table: io::Result<String>) -> SocketStatus {
    if !Path::new(path).is_absolute() || path.contains(['\n', '\r', '\0']) {
        return SocketStatus::Unknown("requires an absolute, single-line filesystem path".into());
    }
    match table {
        Ok(table) => match listening_in_table(path, &table) {
            Ok(true) => SocketStatus::Listening,
            Ok(false) => SocketStatus::NotObserved,
            Err(e) => SocketStatus::Unknown(format!("{UNIX_TABLE}: {e}")),
        },
        Err(e) => SocketStatus::Unknown(format!("cannot read {UNIX_TABLE}: {e}")),
    }
}

fn listening_in_table(path: &str, table: &str) -> Result<bool> {
    let mut lines = table.split_terminator('\n');
    anyhow::ensure!(
        lines
            .next()
            .map(|h| h.split_ascii_whitespace().collect::<Vec<_>>())
            == Some(vec![
                "Num", "RefCount", "Protocol", "Flags", "Type", "St", "Inode", "Path"
            ]),
        "unrecognized header"
    );
    anyhow::ensure!(table.ends_with('\n'), "truncated table");
    let mut found = false;
    for line in lines {
        let mut rest = line;
        let mut fields = [""; 7];
        for field in &mut fields {
            rest = rest.trim_start_matches([' ', '\t']);
            let end = rest.find([' ', '\t']).unwrap_or(rest.len());
            *field = &rest[..end];
            rest = &rest[end..];
            anyhow::ensure!(!field.is_empty(), "incomplete socket row");
        }
        // The kernel emits one separating space, then the pathname verbatim.
        // Do not split/trim the pathname: embedded and trailing spaces matter.
        let name = if rest.is_empty() {
            ""
        } else {
            rest.strip_prefix(' ')
                .context("invalid pathname separator")?
        };
        let number = fields[0]
            .strip_suffix(':')
            .context("invalid socket number")?;
        u64::from_str_radix(number, 16).context("invalid socket number")?;
        u64::from_str_radix(fields[1], 16).context("invalid reference count")?;
        let protocol = u32::from_str_radix(fields[2], 16).context("invalid protocol")?;
        let flags = u32::from_str_radix(fields[3], 16).context("invalid flags")?;
        let kind = u16::from_str_radix(fields[4], 16).context("invalid socket type")?;
        let state = u8::from_str_radix(fields[5], 16).context("invalid socket state")?;
        fields[6].parse::<u64>().context("invalid inode")?;
        // Linux net/unix/af_unix.c: unix_seq_show exposes TCP_LISTEN as
        // __SO_ACCEPTCON (0x10000); SOCK_STREAM=1, SS_UNCONNECTED=1.
        found |= name == path && protocol == 0 && flags & 0x10000 != 0 && kind == 1 && state == 1;
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Num       RefCount Protocol Flags    Type St Inode Path\n";

    fn row(flags: &str, kind: &str, state: &str, name: &str) -> String {
        format!("0000000032fac27a: 00000002 00000000 {flags} {kind} {state} 18153423 {name}\n")
    }

    #[test]
    fn doctor_socket_requires_exact_listener_not_connected_queued_or_prefix() {
        let path = "/tmp/driver socket.sock ";
        for (flags, kind, state) in [
            ("00000000", "0001", "01"), // bound, not listening
            ("00000000", "0001", "02"), // queued
            ("00000000", "0001", "03"), // connected
            ("00010000", "0002", "01"), // datagram
            ("00010000", "0005", "01"), // seqpacket listener
            ("00010000", "0001", "03"), // wrong state
        ] {
            let table = format!("{HEADER}{}", row(flags, kind, state, path));
            assert!(!listening_in_table(path, &table).unwrap());
        }
        let listener = row("00010000", "0001", "01", path);
        let table = format!("{HEADER}{listener}");
        assert!(listening_in_table(path, &table).unwrap());
        for other in [
            "/tmp/driver",
            "/tmp/driver socket.sock",
            "/tmp/driver socket.sock extra",
        ] {
            assert!(!listening_in_table(other, &table).unwrap());
        }
        // A matching listener can coexist with rows for accepted/queued peers.
        let table = format!(
            "{HEADER}{}{listener}{}",
            row("00000000", "0001", "02", path),
            row("00000000", "0001", "03", path)
        );
        assert!(listening_in_table(path, &table).unwrap());
        assert!(!listening_in_table(path, HEADER).unwrap());
        assert!(!listening_in_table(
            path,
            &format!("{HEADER}0000: 00000002 00000000 00000000 0001 03 123\n")
        )
        .unwrap());
    }

    #[test]
    fn doctor_socket_unavailable_or_malformed_is_unknown_even_after_a_match() {
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::PermissionDenied] {
            assert!(matches!(
                socket_status_from_table("/sock", Err(io::Error::from(kind))),
                SocketStatus::Unknown(_)
            ));
        }
        let good = row("00010000", "0001", "01", "/sock");
        for table in [
            String::new(),
            "wrong header\n".into(),
            format!("{HEADER}incomplete\n"),
            format!("{HEADER}{good}malformed\n"),
            format!("{HEADER}{}", good.trim_end()),
            format!("{HEADER}0000: 2 0 invalid 0001 01 1 /sock\n"),
            format!("{HEADER}0000: 2 0 00010000 0001 01 invalid /sock\n"),
        ] {
            assert!(matches!(
                socket_status_from_table("/sock", Ok(table)),
                SocketStatus::Unknown(_)
            ));
        }
        for path in [
            "",
            "relative.sock",
            "@abstract",
            "/sock\nname",
            "/sock\0name",
        ] {
            assert!(matches!(
                socket_status_from_table(path, Ok(HEADER.into())),
                SocketStatus::Unknown(_)
            ));
        }
    }

    #[test]
    fn doctor_socket_fixture_is_read_only_without_connect_permission() {
        use std::os::unix::{
            fs::PermissionsExt,
            net::{UnixListener, UnixStream},
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("driver socket.sock");
        let path = path.to_str().unwrap();
        assert_eq!(socket_status(path), SocketStatus::NotObserved);
        std::fs::write(path, "stale non-socket file").unwrap();
        assert_eq!(socket_status(path), SocketStatus::NotObserved);
        std::fs::remove_file(path).unwrap();
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        for mode in [0o600, 0o000] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
            if mode == 0 && unsafe { libc::geteuid() } != 0 {
                assert_eq!(
                    UnixStream::connect(path).unwrap_err().kind(),
                    io::ErrorKind::PermissionDenied
                );
            }
            for _ in 0..16 {
                assert_eq!(socket_status(path), SocketStatus::Listening);
            }
            assert_eq!(
                listener.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                mode
            );
        }
        drop(listener);
        assert!(Path::new(path).exists()); // stale socket inode remains
        assert_eq!(socket_status(path), SocketStatus::NotObserved);
    }

    #[test]
    fn doctor_config_precedence_and_on_disk_labels() {
        for (explicit, env, expected) in [
            (Some("explicit"), Some("env"), "explicit"),
            (Some("explicit"), None, "explicit"),
            (None, Some("env"), "env"),
            (None, Some(""), "installed"),
            (None, None, "installed"),
        ] {
            let (source, result) = check_config_with(explicit, env, "installed", |path| {
                assert_eq!(path, expected);
                Ok(Some("schema_version = 1\n".into()))
            });
            result.unwrap();
            assert_eq!(source, format!("{expected}; on disk"));
        }
    }

    #[test]
    fn startup_loading_and_doctor_share_selection_and_sources() {
        // Same inputs must select the same file and report the same source
        // in startup loading and doctor (finding #17: no per-command drift).
        for (explicit, env) in [
            (Some("explicit"), Some("env")),
            (Some("explicit"), None),
            (None, Some("env")),
            (None, Some("")),
            (None, None),
        ] {
            let selected = config::select_config_source_with_installed(explicit, env, "installed");
            let (source, result) = check_config_with(explicit, env, "installed", |path| {
                assert_eq!(path, selected.path);
                Ok(Some("schema_version = 1\n".into()))
            });
            result.unwrap();
            assert_eq!(source, config::loaded_config_source(&selected.path));
        }
        // Missing-file fallback reports identical defaults sources.
        for (env, kind) in [
            (Some("env"), config::ConfigSourceKind::Env),
            (None, config::ConfigSourceKind::Installed),
        ] {
            let selected = config::select_config_source_with_installed(None, env, "installed");
            assert_eq!(selected.kind, kind);
            let (source, result) = check_config_with(None, env, "installed", |path| {
                assert_eq!(path, selected.path);
                Ok(None)
            });
            result.unwrap();
            assert_eq!(
                source,
                config::fallback_config_source(&selected.path, selected.kind)
            );
            let (cfg, loader_source) = config::load_selected_config_with_installed(
                None,
                env,
                "/nonexistent-ltd-installed-xyz.toml",
            )
            .unwrap();
            assert_eq!(cfg, Config::default());
            assert!(loader_source.starts_with("<built-in defaults>"));
        }
    }

    #[test]
    fn doctor_config_missing_fallback_names_actual_defaults_and_reason() {
        for (env, selection, path) in [
            (Some("env"), "LTD_CONFIG", "env"),
            (None, "installed", "installed"),
        ] {
            let (source, result) = check_config_with(None, env, "installed", |p| {
                assert_eq!(p, path);
                Ok(None)
            });
            result.unwrap();
            assert_eq!(
                source,
                format!("<built-in defaults>; {selection} config missing: {path}")
            );
        }
        let (source, result) =
            check_config_with(Some("explicit"), Some("env"), "installed", |_| Ok(None));
        assert!(result.is_err());
        assert_eq!(source, "explicit; on disk");
    }

    #[test]
    fn doctor_config_selected_invalid_or_unreadable_never_falls_back() {
        for (explicit, env) in [
            (Some("selected"), Some("env")),
            (None, Some("selected")),
            (None, None),
        ] {
            for text in [
                "bad TOML",
                "schema_version = 1\n[device]\npolling_rate = 0\n",
            ] {
                let (source, result) =
                    check_config_with(explicit, env, "selected", |_| Ok(Some(text.into())));
                assert!(result.is_err());
                assert_eq!(source, "selected; on disk");
            }
            let (source, result) = check_config_with(explicit, env, "selected", |_| {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            });
            assert_eq!(
                result
                    .unwrap_err()
                    .downcast_ref::<io::Error>()
                    .unwrap()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert_eq!(source, "selected; on disk");
        }
    }

    #[test]
    fn doctor_config_real_files_and_dangling_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let installed = dir.path().join("config.toml");
        let installed = installed.to_str().unwrap();
        let (source, result) = check_config_with(None, None, installed, config::read_config_text);
        result.unwrap();
        assert!(source.starts_with("<built-in defaults>"));
        std::fs::write(installed, "schema_version = 1\n").unwrap();
        let (source, result) = check_config_with(None, None, installed, config::read_config_text);
        result.unwrap();
        assert_eq!(source, format!("{installed}; on disk"));
        std::fs::remove_file(installed).unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), installed).unwrap();
        let (source, result) = check_config_with(None, None, installed, config::read_config_text);
        assert!(result.is_err());
        assert_eq!(source, format!("{installed}; on disk"));
        std::fs::remove_file(installed).unwrap();
        std::fs::create_dir(installed).unwrap();
        assert!(
            check_config_with(None, None, installed, config::read_config_text)
                .1
                .is_err()
        );
    }
}
