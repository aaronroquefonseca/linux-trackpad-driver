use std::{env, path::Path, process::Command};

fn metadata(name: &str) -> Option<String> {
    env::var(name).ok()
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .current_dir(root)
        // A caller's repository selection must not identify a source archive
        // (or another worktree) as the caller's checkout.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())?
        .map(|s| s.trim().to_owned())
}

fn valid_date(date: &str) -> bool {
    let bytes = date.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return false;
    }
    let year: u32 = date[..4].parse().unwrap();
    let month: u32 = date[5..7].parse().unwrap();
    let day: u32 = date[8..].parse().unwrap();
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    let days = match month {
        4 | 6 | 9 | 11 => 30,
        2 => 28 + u32::from(leap),
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => return false,
    };
    year > 0 && day > 0 && day <= days
}

fn main() {
    // Git refs/index alone cannot track unstaged edits, new files, or dirty ->
    // clean transitions. This intentionally absent path makes Cargo re-probe
    // each build, including linked worktrees, without generating source files.
    println!(
        "cargo:rerun-if-changed={}/identity-probe-always",
        env::var("OUT_DIR").unwrap()
    );
    for name in ["LTD_BUILD_CHANNEL", "LTD_BUILD_COMMIT", "LTD_BUILD_DATE"] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let channel = metadata("LTD_BUILD_CHANNEL").unwrap_or_else(|| "development".into());
    assert!(
        matches!(channel.as_str(), "stable" | "development" | "nightly"),
        "LTD_BUILD_CHANNEL must be stable, development, or nightly"
    );
    let date = metadata("LTD_BUILD_DATE");
    let channel = if channel == "nightly" {
        let date = date.expect("nightly requires an explicit LTD_BUILD_DATE (YYYY-MM-DD)");
        assert!(valid_date(&date), "LTD_BUILD_DATE must be YYYY-MM-DD");
        format!("nightly {date}")
    } else {
        assert!(date.is_none(), "LTD_BUILD_DATE is only valid for nightly");
        channel
    };

    let root = env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&root);
    // Require our own .git (directory OR worktree file). Never walk up into
    // an unrelated parent repository when building an extracted archive.
    let own_git = root.join(".git").exists()
        && git(root, &["rev-parse", "--show-toplevel"])
            .and_then(|top| Path::new(&top).canonicalize().ok())
            == root.canonicalize().ok();
    let commit = metadata("LTD_BUILD_COMMIT")
        .or_else(|| own_git.then(|| git(root, &["rev-parse", "HEAD"])).flatten())
        .unwrap_or_else(|| "unknown".into());
    assert!(
        commit == "unknown"
            || ((12..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())),
        "LTD_BUILD_COMMIT must be 12..64 hex digits or unknown"
    );
    let commit = if commit == "unknown" {
        commit
    } else {
        commit[..12].to_ascii_lowercase()
    };
    let dirty = own_git
        && git(root, &["status", "--porcelain", "--untracked-files=normal"])
            .is_some_and(|status| !status.is_empty());
    let suffix = if dirty { ", dirty" } else { "" };
    println!("cargo:rustc-env=LTD_BUILD_DETAILS={channel}, commit {commit}{suffix}");
}
