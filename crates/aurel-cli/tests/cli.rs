//! Integration tests for the `aurel` binary (Phase 0).
//!
//! These tests spawn the compiled binary via `env!("CARGO_BIN_EXE_aurel")`
//! and assert on exit codes plus stdout/stderr. Standard library only.

use std::process::Command;

fn aurel() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aurel"))
}

/// Expected `--version` line, derived from the shared version API rather than
/// hard-coded, so the display and the test cannot silently drift.
fn expected_version_line() -> String {
    format!("aurel {}", aurel_core::version())
}

#[test]
fn version_long_flag() {
    let output = aurel().arg("--version").output().expect("spawn aurel");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
    let expected = expected_version_line();
    assert!(
        stdout.contains(&expected),
        "stdout must contain {expected:?}, got: {stdout:?}"
    );
}

#[test]
fn version_short_flag() {
    let output = aurel().arg("-V").output().expect("spawn aurel");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
    let expected = expected_version_line();
    assert!(stdout.contains(&expected), "got: {stdout:?}");
}

#[test]
fn help_long_flag() {
    let output = aurel().arg("--help").output().expect("spawn aurel");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
    assert!(stdout.contains("USAGE:"), "got: {stdout:?}");
    assert!(stdout.contains("--version"), "got: {stdout:?}");
}

#[test]
fn help_short_flag() {
    let output = aurel().arg("-h").output().expect("spawn aurel");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
    assert!(stdout.contains("USAGE:"), "got: {stdout:?}");
}

#[test]
fn no_args_prints_help_and_exits_zero() {
    let output = aurel().output().expect("spawn aurel");
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).expect("stdout UTF-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr UTF-8");
    assert!(stdout.contains("USAGE:"), "got: {stdout:?}");
    assert!(stderr.is_empty(), "stderr must be empty, got: {stderr:?}");
}

#[test]
fn unknown_argument_exits_with_code_2() {
    let output = aurel().arg("--wat").output().expect("spawn aurel");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).expect("stderr UTF-8");
    assert!(
        stderr.contains("--wat"),
        "stderr must echo the bad flag, got: {stderr:?}"
    );
}
