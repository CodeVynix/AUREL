//! Integration tests for Phase 1 configuration (precedence + errors).
//!
//! Each test spawns the compiled binary via `env!("CARGO_BIN_EXE_aurel")`
//! with an isolated home directory and working directory under the system
//! temp dir, so the real user config and repository state cannot leak in.
//! Standard library only.

use std::path::{Path, PathBuf};
use std::process::Command;

fn aurel() -> Command {
    Command::new(env!("CARGO_BIN_EXE_aurel"))
}

fn test_root(name: &str) -> PathBuf {
    clean_env();
    let dir = std::env::temp_dir().join(format!("aurel-cli-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create test root");
    dir
}

fn write_file(path: &Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dirs");
    }
    std::fs::write(path, text).expect("write test file");
}

/// Point the global-config lookup at an empty dir. Callers set per-test
/// `AUREL_*` variables themselves (after this runs); tests that need a clean
/// slate drop the ambient variable with `std::env::remove_var` first.
fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    let home_str = home.to_str().expect("temp path is UTF-8");
    #[cfg(windows)]
    cmd.env("APPDATA", home_str);
    #[cfg(not(windows))]
    cmd.env("HOME", home_str);
    cmd
}

/// Global config path for the current platform under `home`.
fn global_path(home: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        home.join("aurel").join("config.toml")
    }
    #[cfg(not(windows))]
    {
        home.join(".config").join("aurel").join("config.toml")
    }
}

fn stdout_text(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout UTF-8")
}

/// Drop any ambient `AUREL_LOG_LEVEL` so tests observe only their own setup.
fn clean_env() {
    std::env::remove_var("AUREL_LOG_LEVEL");
}

fn stderr_text(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr UTF-8")
}

#[test]
fn show_defaults_when_no_config_exists() {
    let root = test_root("defaults");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&home).expect("home");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(aurel().current_dir(&work).arg("config").arg("show"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    let stdout = stdout_text(&output);
    assert!(stdout.contains("log_level = \"info\""), "got: {stdout:?}");
    assert!(stdout.contains("not found"), "got: {stdout:?}");
}

#[test]
fn project_config_is_picked_up() {
    let root = test_root("project");
    let home = root.join("home");
    let work = root.join("work");
    write_file(
        &work.join(".aurel").join("config.toml"),
        "log_level = \"debug\"\n",
    );

    let output = isolated(aurel().current_dir(&work).arg("config").arg("show"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    assert!(
        stdout_text(&output).contains("log_level = \"debug\""),
        "got: {:?}",
        stdout_text(&output)
    );
}

#[test]
fn global_config_is_picked_up() {
    let root = test_root("global");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");
    write_file(&global_path(&home), "log_level = \"error\"\n");

    let output = isolated(aurel().current_dir(&work).arg("config").arg("show"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    assert!(
        stdout_text(&output).contains("log_level = \"error\""),
        "got: {:?}",
        stdout_text(&output)
    );
}

#[test]
fn project_beats_global() {
    let root = test_root("project-beats-global");
    let home = root.join("home");
    let work = root.join("work");
    write_file(&global_path(&home), "log_level = \"error\"\n");
    write_file(
        &work.join(".aurel").join("config.toml"),
        "log_level = \"warn\"\n",
    );

    let output = isolated(aurel().current_dir(&work).arg("config").arg("show"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(0),
        "stderr: {}",
        stderr_text(&output)
    );
    assert!(
        stdout_text(&output).contains("log_level = \"warn\""),
        "got: {:?}",
        stdout_text(&output)
    );
}

#[test]
fn env_beats_files() {
    let root = test_root("env-beats-files");
    let home = root.join("home");
    let work = root.join("work");
    write_file(&global_path(&home), "log_level = \"error\"\n");
    write_file(
        &work.join(".aurel").join("config.toml"),
        "log_level = \"warn\"\n",
    );

    let output = isolated(
        aurel()
            .current_dir(&work)
            .env("AUREL_LOG_LEVEL", "trace")
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
    assert!(
        stdout_text(&output).contains("log_level = \"trace\""),
        "got: {:?}",
        stdout_text(&output)
    );
}

#[test]
fn cli_flag_beats_env() {
    let root = test_root("cli-beats-env");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .env("AUREL_LOG_LEVEL", "warn")
            .arg("--log-level=error")
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
    assert!(
        stdout_text(&output).contains("log_level = \"error\""),
        "got: {:?}",
        stdout_text(&output)
    );
}

#[test]
fn explicit_config_replaces_discovery() {
    let root = test_root("explicit");
    let home = root.join("home");
    let work = root.join("work");
    write_file(&global_path(&home), "log_level = \"error\"\n");
    write_file(
        &work.join(".aurel").join("config.toml"),
        "log_level = \"warn\"\n",
    );
    let custom = root.join("custom.toml");
    write_file(&custom, "log_level = \"debug\"\n");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--config")
            .arg(&custom)
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
    assert!(stdout.contains("log_level = \"debug\""), "got: {stdout:?}");
    assert!(stdout.contains("not searched"), "got: {stdout:?}");
    assert!(
        stdout.contains(&custom.display().to_string()),
        "got: {stdout:?}"
    );
}

#[test]
fn missing_explicit_config_is_exit_1() {
    let root = test_root("explicit-missing");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--config")
            .arg(root.join("nope.toml"))
            .arg("config")
            .arg("show"),
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
        stderr_text(&output).contains("not found"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn malformed_toml_is_exit_1_with_path() {
    let root = test_root("malformed");
    let home = root.join("home");
    let work = root.join("work");
    let proj = work.join(".aurel").join("config.toml");
    write_file(&proj, "log_level = [oops\n");

    let output = isolated(aurel().current_dir(&work).arg("config").arg("show"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout: {}",
        stdout_text(&output)
    );
    let stderr = stderr_text(&output);
    assert!(stderr.contains("invalid TOML"), "got: {stderr:?}");
    assert!(
        stderr.contains(&proj.display().to_string()),
        "got: {stderr:?}"
    );
}

#[test]
fn unknown_field_is_exit_1() {
    let root = test_root("unknown-field");
    let home = root.join("home");
    let work = root.join("work");
    write_file(
        &work.join(".aurel").join("config.toml"),
        "log_levle = \"debug\"\n",
    );

    let output = isolated(aurel().current_dir(&work).arg("config").arg("show"), &home)
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
fn invalid_env_value_is_exit_1() {
    let root = test_root("invalid-env");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .env("AUREL_LOG_LEVEL", "chatty")
            .arg("config")
            .arg("show"),
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
        stderr_text(&output).contains("AUREL_LOG_LEVEL"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn bad_cli_log_level_is_exit_2() {
    let root = test_root("bad-cli-level");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel().current_dir(&work).arg("--log-level").arg("chatty"),
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
        stderr_text(&output).contains("chatty"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn unknown_command_is_exit_2() {
    let root = test_root("unknown-command");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(aurel().current_dir(&work).arg("frobnicate"), &home)
        .output()
        .expect("spawn aurel");
    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout: {}",
        stdout_text(&output)
    );
    assert!(
        stderr_text(&output).contains("frobnicate"),
        "got: {:?}",
        stderr_text(&output)
    );
}

#[test]
fn version_with_command_is_exit_2() {
    let root = test_root("version-with-command");
    let home = root.join("home");
    let work = root.join("work");
    std::fs::create_dir_all(&work).expect("work");

    let output = isolated(
        aurel()
            .current_dir(&work)
            .arg("--version")
            .arg("config")
            .arg("show"),
        &home,
    )
    .output()
    .expect("spawn aurel");
    assert_eq!(output.status.code(), Some(2));
}
