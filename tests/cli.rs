use std::process::Command;

#[test]
fn version_succeeds_without_loading_config() {
    let dir = tempfile::tempdir().unwrap();
    let invalid_config = dir.path().join("invalid.toml");
    std::fs::write(&invalid_config, "this is not valid TOML").unwrap();
    let missing_config = dir.path().join("missing.toml");

    for config in [&invalid_config, &missing_config] {
        for flag in ["--version", "-V"] {
            let output = Command::new(env!("CARGO_BIN_EXE_ltd"))
                .arg(flag)
                .current_dir(dir.path())
                .env("PATH", "")
                .env("LTD_BUILD_CHANNEL", "ignored-at-runtime")
                .env("LTD_BUILD_COMMIT", "ignored-at-runtime")
                .env("LTD_CONFIG", config)
                .output()
                .unwrap();

            assert_eq!(output.status.code(), Some(0), "{flag}: {output:?}");
            assert_eq!(
                output.stdout,
                format!(
                    "ltd {} ({})\n",
                    env!("CARGO_PKG_VERSION"),
                    env!("LTD_BUILD_DETAILS")
                )
                .as_bytes()
            );
            assert!(output.stderr.is_empty(), "{flag}: {output:?}");
        }
    }
}

#[test]
fn help_lists_version_and_existing_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_ltd"))
        .arg("--help")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    for entry in ["--version", "-V", "run", "reconnect", "resume", "doctor"] {
        assert!(stdout.contains(entry), "help missing {entry}: {stdout}");
    }
}

#[test]
fn invalid_arguments_still_fail() {
    let unknown_command_error = format!(
        "unknown command '--bogus'; see '{} help'",
        env!("CARGO_BIN_EXE_ltd")
    );
    for (args, error) in [
        (
            vec!["--version", "--bogus"],
            "--version: unexpected argument '--bogus'",
        ),
        (vec!["-V", "run"], "-V: unexpected argument 'run'"),
        (vec!["--bogus"], unknown_command_error.as_str()),
        (vec!["run", "--version"], "run: unknown option '--version'"),
        (
            vec!["reconnect", "/dev/hidraw0", "-V"],
            "reconnect: unknown option '-V'",
        ),
        (
            vec!["resume", "-p", "0"],
            "invalid polling rate: '0' (want 1..=65535)",
        ),
        (vec!["doctor", "-c"], "doctor: -c/--config requires a value"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ltd"))
            .args(&args)
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(1), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{args:?}: {output:?}");
        assert_eq!(output.stderr, format!("ltd: {error}\n").as_bytes());
    }
}

// Bound doctor regressions so an accidental blocking connect cannot hang CI.
fn doctor_output(
    args: &[&str],
    config: &std::path::Path,
    socket: &std::path::Path,
) -> std::process::Output {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    let mut child = Command::new(env!("CARGO_BIN_EXE_ltd"))
        .arg("doctor")
        .args(args)
        .env("LTD_CONFIG", config)
        .env("LTD_SOCKET", socket)
        .env("PATH", "") // doctor requires no systemctl or other command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("doctor timed out: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn doctor_observes_private_listener_repeatedly_without_connections() {
    use std::os::unix::{fs::PermissionsExt, net::UnixListener};
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "schema_version = 1\n").unwrap();
    let socket = dir.path().join("private driver.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o000)).unwrap();
    for _ in 0..4 {
        let output = doctor_output(&[], &config, &socket);
        assert!(output.stderr.is_empty(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(
            stdout.contains(&format!(
                "[ok] config valid ({}; on disk)",
                config.display()
            )),
            "{stdout}"
        );
        assert!(
            stdout.contains(&format!(
                "[ok] listening stream socket observed ({};",
                socket.display()
            )),
            "{stdout}"
        );
        assert!(!stdout.contains("systemctl enable"), "{stdout}");
    }
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0
    );
    drop(listener);
    let stdout = String::from_utf8(doctor_output(&[], &config, &socket).stdout).unwrap();
    assert!(
        stdout.contains("no listening stream socket observed"),
        "{stdout}"
    );
}

#[test]
fn doctor_config_precedence_errors_and_fallback_are_visible() {
    let dir = tempfile::tempdir().unwrap();
    let valid = dir.path().join("valid.toml");
    let invalid = dir.path().join("invalid.toml");
    let missing = dir.path().join("missing.toml");
    let socket = dir.path().join("absent.sock");
    std::fs::write(&valid, "schema_version = 1\n").unwrap();
    std::fs::write(&invalid, "schema_version = 99\n").unwrap();
    for (args, env, expected, failure) in [
        (
            vec!["-c", valid.to_str().unwrap()],
            &invalid,
            format!("[ok] config valid ({}; on disk)", valid.display()),
            false,
        ),
        (
            vec![],
            &invalid,
            format!("[FAIL] config invalid ({}; on disk)", invalid.display()),
            true,
        ),
        (
            vec!["-c", missing.to_str().unwrap()],
            &valid,
            format!("[FAIL] config invalid ({}; on disk)", missing.display()),
            true,
        ),
        (
            vec![],
            &missing,
            format!(
                "[ok] config valid (<built-in defaults>; LTD_CONFIG config missing: {})",
                missing.display()
            ),
            false,
        ),
    ] {
        let output = doctor_output(&args, env, &socket);
        assert!(output.stderr.is_empty(), "{output:?}");
        if failure {
            assert_eq!(output.status.code(), Some(1));
        } // No-device failures are independent of config validity.
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(&expected), "{stdout}");
        assert!(!stdout.contains("daemon socket absent"), "{stdout}");
    }
}
