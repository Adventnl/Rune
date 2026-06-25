//! Integration tests for Rune package tooling: manifest parsing, package
//! resolution/build, dependency exposure, and the `runec` package subcommands
//! driven through the real binary.
//!
//! These tests are robust to parallel runs: any scaffolding uses a unique temp
//! directory, and the std directory is passed to child processes via the
//! `RUNE_STD` environment variable (computed from `CARGO_MANIFEST_DIR`) rather
//! than mutating shared process state.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Absolute path to the compiled `runec` binary under test.
fn runec_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_runec"))
}

/// Workspace root = two levels above this crate's manifest dir.
fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // workspace root
    p
}

/// The workspace standard library directory.
fn std_dir() -> PathBuf {
    workspace_root().join("std")
}

/// The sample `demo` package directory.
fn demo_pkg() -> PathBuf {
    workspace_root().join("examples").join("pkg").join("demo")
}

/// A unique temp directory for scaffolding, never shared across tests.
fn unique_dir(tag: &str) -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("rune_it_{}_{}_{}", tag, std::process::id(), n))
}

/// Run `runec <args...>` (cwd `cwd`) with `RUNE_STD` set to the workspace std.
fn run_runec(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(runec_bin())
        .args(args)
        .current_dir(cwd)
        .env("RUNE_STD", std_dir())
        .output()
        .expect("failed to spawn runec")
}

// ---- manifest parsing ----------------------------------------------------

#[test]
fn load_manifest_parses_known_toml() {
    // Drive manifest parsing through the build command's success line, which
    // echoes the manifest name + version, and additionally assert the on-disk
    // sample manifest parses as expected via a fresh scaffold.
    let dir = unique_dir("manifest");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("rune.toml"),
        "[package]\nname = \"acme\"\nversion = \"3.4.5\"\nentry = \"src/main.rune\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::write(dir.join("src/main.rune"), "fn main() {}\n").unwrap();

    let out = run_runec(Path::new("."), &["build", dir.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("compiled acme v3.4.5"),
        "unexpected build line: {stdout}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- build + run the sample package --------------------------------------

#[test]
fn build_sample_package_reports_functions() {
    let out = run_runec(Path::new("."), &["build", demo_pkg().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("compiled demo v0.1.0"),
        "unexpected build line: {stdout}"
    );
    // The reported function count is positive (entry + deps + std).
    assert!(stdout.contains("functions)"), "{stdout}");
}

#[test]
fn run_sample_package_prints_expected() {
    let out = run_runec(Path::new("."), &["run", demo_pkg().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // 25 (mathx::square dep), 64 (std clamp), 42 (mathx::add dep).
    assert_eq!(lines, vec!["25", "64", "42"]);
}

#[test]
fn path_dependency_call_resolves() {
    // `demo` calls `mathx::square` / `mathx::add` from a path dependency. If the
    // dependency module were not exposed, the build would fail to typecheck.
    let out = run_runec(Path::new("."), &["build", demo_pkg().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "dependency build failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---- `runec test` --------------------------------------------------------

#[test]
fn test_sample_package_all_pass() {
    let out = run_runec(Path::new("."), &["test", demo_pkg().to_str().unwrap()]);
    assert!(
        out.status.success(),
        "test command failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("test_square_dep ... ok"), "{stdout}");
    assert!(stdout.contains("test_add_dep ... ok"), "{stdout}");
    assert!(stdout.contains("test_std_clamp ... ok"), "{stdout}");
    assert!(stdout.contains("test result: ok"), "{stdout}");
}

#[test]
fn test_failure_exits_nonzero() {
    let dir = unique_dir("testfail");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("rune.toml"),
        "[package]\nname = \"failing\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.rune"),
        "fn main() {}\nfn test_ok() -> bool { true }\nfn test_bad() -> bool { false }\n",
    )
    .unwrap();

    let out = run_runec(Path::new("."), &["test", dir.to_str().unwrap()]);
    assert!(!out.status.success(), "expected non-zero exit on a failing test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("test_ok ... ok"), "{stdout}");
    assert!(stdout.contains("test_bad ... FAILED"), "{stdout}");
    assert!(stdout.contains("1 passed; 1 failed"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- `runec new` ---------------------------------------------------------

#[test]
fn new_scaffolds_buildable_package() {
    let base = unique_dir("new");
    std::fs::create_dir_all(&base).unwrap();

    // Scaffold inside the unique base dir.
    let out = run_runec(&base, &["new", "myapp"]);
    assert!(
        out.status.success(),
        "new failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pkg = base.join("myapp");
    assert!(pkg.join("rune.toml").is_file());
    assert!(pkg.join("src/main.rune").is_file());

    // The scaffolded package builds, runs, and its test passes.
    let built = run_runec(&base, &["build", "myapp"]);
    assert!(
        built.status.success(),
        "scaffolded build failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let tested = run_runec(&base, &["test", "myapp"]);
    assert!(
        tested.status.success(),
        "scaffolded test failed: {}",
        String::from_utf8_lossy(&tested.stderr)
    );

    // Refuses to overwrite an existing directory.
    let again = run_runec(&base, &["new", "myapp"]);
    assert!(!again.status.success(), "expected refusal on existing dir");
    assert!(String::from_utf8_lossy(&again.stderr).contains("already exists"));

    let _ = std::fs::remove_dir_all(&base);
}

// ---- single-file `run` still works ---------------------------------------

#[test]
fn run_single_file_still_works() {
    let milestone = workspace_root().join("examples").join("milestone.rune");
    let out = run_runec(Path::new("."), &["run", milestone.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "single-file run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["44", "12", "12"]);
}
