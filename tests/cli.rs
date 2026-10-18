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
                .env("LTD_CONFIG", config)
                .output()
                .unwrap();

            assert_eq!(output.status.code(), Some(0), "{flag}: {output:?}");
            assert_eq!(
                output.stdout,
                format!("ltd {}\n", env!("CARGO_PKG_VERSION")).as_bytes()
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
