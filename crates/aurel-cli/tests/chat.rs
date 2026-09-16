//! Integration tests for `aurel chat` (Phase 2).
//!
//! Provider wire behavior is covered in `aurel-model`; these tests cover the
//! CLI layer: help, message acquisition, flag/config plumbing, and clean
//! failure paths. The only network used is a guaranteed-closed loopback
//! port, so failures are fast and deterministic. Standard library only.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn aurel() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aurel"))
}

fn test_root(name: &str) -> PathBuf {
    clean_env();
    let dir = std::env::temp_dir().join(format!("aurel-chat-it-{}-{name}", std::process::id()));
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

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(path, text).expect("write test file");
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
/// Refused connections are machine-timing-dependent (seconds on some
/// Windows hosts); a reset is instant and maps to the same `Connection`
/// error, keeping the suite fast and deterministic.
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
fn chat_help() {
    let root = test_root("help");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(aurel().current_dir(&work).arg("chat").arg("--help"), &home)
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
    assert!(stdout.contains("MESSAGE"), "got: {stdout:?}");
    assert!(
        stdout.contains("never inspects repositories"),
        "got: {stdout:?}"
    );
}

#[test]
fn chat_without_message_and_empty_stdin_is_exit_2() {
    let root = test_root("no-message");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(aurel().current_dir(&work).arg("chat"), &home)
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
fn chat_reads_piped_stdin() {
    use std::io::Write;
    let root = test_root("piped-stdin");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");
    // Single attempt: retries are covered in `aurel-model`.
    let (port, server) = refuse_once();

    // The message reaches the provider layer: with nowhere to send it, the
    // failure must be a connection error — not a usage error.
    let mut child = isolated(
        aurel()
            .current_dir(&work)
            .env("AUREL_MAX_RETRIES", "0")
            .arg("--base-url")
            .arg(format!("http://127.0.0.1:{port}/v1"))
            .arg("--streaming=false")
            .arg("chat"),
        &home,
    )
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn aurel");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"piped hello")
        .expect("write stdin");
    let output = child.wait_with_output().expect("wait");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        stdout_text(&output)
    );
    let stderr = stderr_text(&output);
    assert!(
        stderr.contains("cannot reach model endpoint"),
        "piped message must reach the provider layer, got: {stderr:?}"
    );
    assert!(!stderr.contains("no message"), "got: {stderr:?}");
    server.join().expect("server finishes");
}

#[test]
fn chat_unreachable_endpoint_is_exit_1() {
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
            .arg("chat")
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
fn chat_model_flags_flow_into_config_show() {
    let root = test_root("model-flags");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--model")
            .arg("cli-model")
            .arg("--base-url=http://cli:9/v1")
            .arg("--streaming=false")
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
    assert!(stdout.contains("name = \"cli-model\""), "got: {stdout:?}");
    assert!(
        stdout.contains("base_url = \"http://cli:9/v1\""),
        "got: {stdout:?}"
    );
    assert!(stdout.contains("streaming = false"), "got: {stdout:?}");
}

#[test]
fn chat_bad_streaming_flag_is_exit_2() {
    let root = test_root("bad-streaming");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--streaming")
            .arg("maybe")
            .arg("chat")
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
        stderr_text(&output).contains("maybe"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn chat_invalid_model_config_is_exit_1() {
    let root = test_root("invalid-model-config");
    let home = root.join("home");
    let work = root.join("work");
    write_file(
        &work.join(".aurel").join("config.toml"),
        "[model]\ntimeout_secs = \"soon\"\n",
    );

    let output = isolated(aurel().current_dir(&work).arg("chat").arg("hi"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        stdout_text(&output)
    );
    assert!(
        stderr_text(&output).contains("invalid TOML"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn chat_empty_model_name_is_exit_1() {
    let root = test_root("empty-model");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--model")
            .arg("")
            .arg("chat")
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
        stderr_text(&output).contains("model must not be empty"),
        "got: {:?}",
        stderr_text(&output)
    );
}
