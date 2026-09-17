//! Integration tests for `aurel agent` (Phase 3).
//!
//! The loop itself is covered with a scripted provider in `aurel-model`;
//! these tests cover the CLI layer: help, message acquisition, flag/config
//! plumbing, and clean failure paths. Failures use an instantly-reset
//! loopback stub so they stay fast and deterministic. Standard library only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn aurel() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aurel"))
}

fn test_root(name: &str) -> PathBuf {
    clean_env();
    let dir = std::env::temp_dir().join(format!("aurel-agent-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test root");
    dir
}

fn clean_env() {
    for var in [
        "AUREL_LOG_LEVEL",
        "AUREL_MODEL",
        "AUREL_BASE_URL",
        "AUREL_API_KEY",
        "AUREL_TIMEOUT_SECS",
        "AUREL_MAX_RETRIES",
        "AUREL_STREAMING",
        "AUREL_MAX_ITERATIONS",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        std::env::remove_var(var);
    }
}

/// Point global discovery at an empty dir.
fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    let home_str = home.to_str().expect("temp path is UTF-8");
    #[cfg(windows)]
    cmd.env("APPDATA", home_str);
    #[cfg(not(windows))]
    cmd.env("HOME", home_str);
    cmd
}

/// A stub that accepts exactly one connection and closes it immediately.
/// Refused connections are machine-timing-dependent; a reset is instant and
/// maps to the same `Connection` error.
fn refuse_once() -> (u16, std::thread::JoinHandle<()>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            drop(stream);
        }
    });
    (port, handle)
}

fn stdout_text(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout UTF-8")
}

fn stderr_text(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr UTF-8")
}

#[test]
fn agent_help() {
    let root = test_root("help");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(aurel().current_dir(&work).arg("agent").arg("--help"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("USAGE:"), "got: {stdout:?}");
    assert!(stdout.contains("max-iterations"), "got: {stdout:?}");
    assert!(stdout.contains("no tools"), "got: {stdout:?}");
}

#[test]
fn agent_without_message_and_empty_stdin_is_exit_2() {
    let root = test_root("no-message");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(aurel().current_dir(&work).arg("agent"), &home)
        .stdin(Stdio::null())
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}",
        stdout_text(&output)
    );
    assert!(
        stderr_text(&output).contains("no message"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn agent_unreachable_endpoint_is_exit_1() {
    let root = test_root("unreachable");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");
    let (port, server) = refuse_once();

    let output = isolated(
        aurel()
            .current_dir(&work)
            .env("AUREL_MAX_RETRIES", "0")
            .arg("--base-url")
            .arg(format!("http://127.0.0.1:{port}/v1"))
            .arg("--streaming=false")
            .arg("agent")
            .arg("hello"),
        &home,
    )
    .output()
    .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        stdout_text(&output)
    );
    assert!(
        stderr_text(&output).contains("cannot reach model endpoint"),
        "got: {:?}",
        stderr_text(&output)
    );
    server.join().expect("server finishes");
}

#[test]
fn agent_max_iterations_flag_flows_into_config_show() {
    let root = test_root("max-iterations-flag");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--max-iterations")
            .arg("7")
            .arg("config")
            .arg("show"),
        &home,
    )
    .output()
    .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("max_iterations = 7"), "got: {stdout:?}");
}

#[test]
fn agent_bad_max_iterations_flag_is_exit_2() {
    let root = test_root("bad-max-iterations");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--max-iterations")
            .arg("0")
            .arg("agent")
            .arg("hi"),
        &home,
    )
    .output()
    .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}",
        stdout_text(&output)
    );
    assert!(
        stderr_text(&output).contains("--max-iterations"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn agent_invalid_max_iterations_env_is_exit_1() {
    let root = test_root("invalid-max-iterations-env");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .env("AUREL_MAX_ITERATIONS", "many")
            .arg("agent")
            .arg("hi"),
        &home,
    )
    .output()
    .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        stdout_text(&output)
    );
    assert!(
        stderr_text(&output).contains("AUREL_MAX_ITERATIONS"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn agent_over_cap_max_iterations_is_exit_1() {
    let root = test_root("over-cap");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .env("AUREL_MAX_ITERATIONS", "101")
            .arg("agent")
            .arg("hi"),
        &home,
    )
    .output()
    .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        stdout_text(&output)
    );
    assert!(
        stderr_text(&output).contains("max_iterations"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn version_with_agent_is_exit_2() {
    let root = test_root("version-with-agent");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--version")
            .arg("agent")
            .arg("hi"),
        &home,
    )
    .output()
    .expect("spawn aurel");
    assert_eq!(output.status.code(), Some(2));
}
