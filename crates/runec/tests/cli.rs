//! Integration smoke tests for the `runec` binary: drive the real CLI on a real
//! example file and assert on its captured stdout/exit status. These tests never
//! block on stdin or loop forever.

use std::path::PathBuf;
use std::process::Command;

/// Absolute path to the compiled `runec` binary under test.
fn runec_bin() -> PathBuf {
    // Cargo sets CARGO_BIN_EXE_<name> for integration tests of a bin crate.
    PathBuf::from(env!("CARGO_BIN_EXE_runec"))
}

/// Absolute path to `examples/<name>` at the workspace root.
fn example(name: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR = .../crates/runec ; examples live two levels up.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // workspace root
    p.push("examples");
    p.push(name);
    p
}

#[test]
fn run_milestone_example_prints_expected() {
    let out = Command::new(runec_bin())
        .arg("run")
        .arg(example("milestone.rune"))
        .output()
        .expect("failed to spawn runec");

    assert!(
        out.status.success(),
        "runec run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines, vec!["44", "12", "12"]);
}

#[test]
fn run_nonexistent_file_fails_cleanly() {
    let out = Command::new(runec_bin())
        .arg("run")
        .arg("definitely_not_a_real_file_12345.rune")
        .output()
        .expect("failed to spawn runec");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot read"));
}

#[test]
fn unknown_subcommand_fails_with_usage() {
    let out = Command::new(runec_bin())
        .arg("flibber")
        .output()
        .expect("failed to spawn runec");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown command"));
}
