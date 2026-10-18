//! Exercise the real build script through Cargo, using dependency-free fixtures.
//! Reusing each fixture's target catches stale incremental build identities.
use std::{fs, path::Path, process::Command};

struct Fixture {
    _temp: tempfile::TempDir,
    root: std::path::PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("source");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = 'identity-fixture'\nversion = '0.1.0'\nedition = '2021'\n[workspace]\n",
        )
        .unwrap();
        fs::write(root.join("build.rs"), include_str!("../build.rs")).unwrap();
        fs::write(
            root.join("src/main.rs"),
            format!(
                "{}\nfn main() {{ println!(\"{{}}\", IDENTITY); }}\n",
                include_str!("../src/build_identity.rs").replace("//!", "//")
            ),
        )
        .unwrap();
        fs::write(root.join(".gitignore"), "/target/\n").unwrap();
        Self { _temp: temp, root }
    }

    fn cargo(&self, vars: &[(&str, &str)]) -> std::process::Output {
        let mut cmd = Command::new(env!("CARGO"));
        cmd.current_dir(&self.root)
            .args(["build", "--offline", "--quiet"])
            .env("CARGO_TARGET_DIR", self.root.join("target"))
            .env_remove("LTD_BUILD_CHANNEL")
            .env_remove("LTD_BUILD_COMMIT")
            .env_remove("LTD_BUILD_DATE")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTFLAGS");
        for (key, value) in vars {
            cmd.env(key, value);
        }
        cmd.output().unwrap()
    }

    fn identity(&self, vars: &[(&str, &str)]) -> String {
        let build = self.cargo(vars);
        assert!(build.status.success(), "{build:?}");
        let output = Command::new(self.root.join("target/debug/identity-fixture"))
            .current_dir(self._temp.path())
            .env("PATH", "")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn build_script(&self) -> std::path::PathBuf {
        let script = self._temp.path().join("build-script");
        let out = Command::new("rustc")
            .args(["--edition=2021", "build.rs", "-o"])
            .arg(&script)
            .current_dir(&self.root)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        script
    }

    fn git(&self, args: &[&str]) -> String {
        git(&self.root, args)
    }

    fn init(&self) {
        self.git(&["init", "--quiet"]);
        // Generate Cargo.lock before committing, so generation is not dirt.
        self.identity(&[]);
        self.git(&["add", "."]);
        self.git(&["commit", "--quiet", "-m", "fixture"]);
    }
}

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_AUTHOR_NAME", "Build identity fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Build identity fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn git_available() -> bool {
    let available = Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success());
    if !available {
        eprintln!("Git unavailable: skipping checkout fixture (archives do not require Git)");
    }
    available
}

#[test]
fn archives_channels_overrides_and_validation() {
    let f = Fixture::new();
    assert_eq!(f.identity(&[]), "ltd 0.1.0 (development, commit unknown)");
    let stable = [("LTD_BUILD_CHANNEL", "stable")];
    assert_eq!(f.identity(&stable), "ltd 0.1.0 (stable, commit unknown)");
    let explicit = [
        ("LTD_BUILD_CHANNEL", "stable"),
        ("LTD_BUILD_COMMIT", "b32946218ca3"),
    ];
    assert_eq!(
        f.identity(&explicit),
        "ltd 0.1.0 (stable, commit b32946218ca3)"
    );
    assert_eq!(
        f.identity(&[
            ("LTD_BUILD_CHANNEL", "nightly"),
            ("LTD_BUILD_DATE", "2026-10-18"),
            (
                "LTD_BUILD_COMMIT",
                "18D66664196334E80CB318B23098B412C8C29D8F"
            ),
        ]),
        "ltd 0.1.0 (nightly 2026-10-18, commit 18d666641963)"
    );
    // Removing metadata also invalidates the incremental Cargo build.
    assert_eq!(f.identity(&[]), "ltd 0.1.0 (development, commit unknown)");
    for vars in [
        vec![("LTD_BUILD_CHANNEL", "invalid")],
        vec![("LTD_BUILD_CHANNEL", "nightly")],
        vec![("LTD_BUILD_DATE", "2026-10-18")],
        vec![
            ("LTD_BUILD_CHANNEL", "nightly"),
            ("LTD_BUILD_DATE", "2026-02-29"),
        ],
        vec![
            ("LTD_BUILD_CHANNEL", "nightly"),
            ("LTD_BUILD_DATE", "not-a-date"),
        ],
        vec![("LTD_BUILD_COMMIT", "not-a-source-hash")],
        vec![("LTD_BUILD_COMMIT", "1234567")],
        vec![("LTD_BUILD_COMMIT", "18d666641963\ninjected")],
    ] {
        assert!(!f.cargo(&vars).status.success(), "accepted {vars:?}");
    }
}

#[test]
fn worktree_revision_dirty_and_clean_transitions() {
    if !git_available() {
        return;
    }
    let mut f = Fixture::new();
    f.init();
    let commit = f.git(&["rev-parse", "HEAD"]);
    let clean = format!("ltd 0.1.0 (development, commit {})", &commit[..12]);
    assert_eq!(f.identity(&[]), clean);
    assert_eq!(f.identity(&[]), clean, "build output must stay ignored");

    let worktree = f._temp.path().join("linked");
    f.git(&[
        "worktree",
        "add",
        "--quiet",
        "--detach",
        worktree.to_str().unwrap(),
    ]);
    f.root = worktree;
    assert!(f.root.join(".git").is_file());
    assert_eq!(f.identity(&[]), clean);
    let main = f.root.join("src/main.rs");
    let original = fs::read_to_string(&main).unwrap();
    fs::write(&main, format!("{original}\n// local edit\n")).unwrap();
    assert_eq!(f.identity(&[]), clean.replace(')', ", dirty)"));
    // Explicit source metadata must not hide local edits.
    assert!(f
        .identity(&[("LTD_BUILD_COMMIT", &commit)])
        .ends_with(", dirty)"));
    f.git(&["add", "src/main.rs"]);
    assert!(f.identity(&[]).ends_with(", dirty)"), "staged edits");
    f.git(&["commit", "--quiet", "-m", "change source"]);
    let next = f.git(&["rev-parse", "HEAD"]);
    let clean = format!("ltd 0.1.0 (development, commit {})", &next[..12]);
    assert_eq!(f.identity(&[]), clean, "commit changed, same target dir");
    f.git(&["commit", "--quiet", "--allow-empty", "-m", "revision only"]);
    let next = f.git(&["rev-parse", "HEAD"]);
    let clean = format!("ltd 0.1.0 (development, commit {})", &next[..12]);
    assert_eq!(f.identity(&[]), clean, "only HEAD changed");
    assert_eq!(
        f.identity(&[("LTD_BUILD_COMMIT", "unknown")]),
        "ltd 0.1.0 (development, commit unknown)"
    );
    fs::write(f.root.join("src/new.rs"), "// new source\n").unwrap();
    assert!(f.identity(&[]).ends_with(", dirty)"), "untracked source");
    fs::remove_file(f.root.join("src/new.rs")).unwrap();
    assert_eq!(f.identity(&[]), clean, "dirty -> clean, no tracked edit");
}

#[test]
fn archive_does_not_borrow_parent_repository() {
    if !git_available() {
        return;
    }
    let f = Fixture::new();
    git(f._temp.path(), &["init", "--quiet"]);
    fs::write(f._temp.path().join("parent.txt"), "unrelated").unwrap();
    git(f._temp.path(), &["add", "parent.txt"]);
    git(f._temp.path(), &["commit", "--quiet", "-m", "parent"]);
    assert_eq!(f.identity(&[]), "ltd 0.1.0 (development, commit unknown)");
    assert_eq!(
        f.identity(&[("GIT_DIR", f._temp.path().join(".git").to_str().unwrap())]),
        "ltd 0.1.0 (development, commit unknown)"
    );
}

#[test]
#[cfg(unix)]
fn non_unicode_explicit_metadata_is_rejected() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let f = Fixture::new();
    let script = f.build_script();
    for name in ["LTD_BUILD_CHANNEL", "LTD_BUILD_COMMIT", "LTD_BUILD_DATE"] {
        let out = Command::new(&script)
            .env("CARGO_MANIFEST_DIR", &f.root)
            .env("OUT_DIR", f._temp.path())
            .env_remove("LTD_BUILD_CHANNEL")
            .env_remove("LTD_BUILD_COMMIT")
            .env_remove("LTD_BUILD_DATE")
            .env(name, OsString::from_vec(b"private-metadata-\xff".to_vec()))
            .output()
            .unwrap();
        assert!(!out.status.success(), "accepted non-Unicode {name}");
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(stderr.contains(&format!("{name} must be valid Unicode")));
        assert!(!stderr.contains("private-metadata"), "{stderr}");
        assert!(!String::from_utf8(out.stdout)
            .unwrap()
            .contains("cargo:rustc-env=LTD_BUILD_DETAILS="));
    }
}

#[test]
fn missing_git_is_honest_and_explicit_metadata_still_works() {
    let f = Fixture::new();
    fs::create_dir(f.root.join(".git")).unwrap();
    // Compile the dependency-free build script directly, then run it without
    // Git. Hiding Git from Cargo itself would also hide the compiler/linker.
    let script = f.build_script();
    for commit in [None, Some("18d66664196334e80cb318b23098b412c8c29d8f")] {
        let mut cmd = Command::new(&script);
        cmd.env("PATH", "")
            .env("CARGO_MANIFEST_DIR", &f.root)
            .env("OUT_DIR", f._temp.path())
            .env_remove("LTD_BUILD_CHANNEL")
            .env_remove("LTD_BUILD_COMMIT")
            .env_remove("LTD_BUILD_DATE");
        if let Some(commit) = commit {
            cmd.env("LTD_BUILD_COMMIT", commit);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{out:?}");
        let stdout = String::from_utf8(out.stdout).unwrap();
        let expected = if commit.is_some() {
            "18d666641963"
        } else {
            "unknown"
        };
        assert!(
            stdout.contains(&format!(
                "cargo:rustc-env=LTD_BUILD_DETAILS=development, commit {expected}\n"
            )),
            "{stdout}"
        );
    }
}
