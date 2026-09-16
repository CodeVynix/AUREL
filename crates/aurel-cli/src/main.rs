//! `aurel` CLI entry point.
//!
//! Phase 1: dependency-free argument handling ([`args`]) over
//! `std::env::args_os` with an explicit lossy Unicode policy (see
//! [`normalize_args`]), TOML configuration with documented precedence
//! (`aurel-config`), and a `config show` command.
//!
//! Conceptual flow:
//!
//! ```text
//! collect argv (args_os, never panics on non-Unicode)
//!     ↓
//! parse CLI
//!     ↓
//! --help → print help and exit (config/environment untouched)
//! --version → print version and exit (config/environment untouched)
//! bare / no command → print help and exit (config/environment untouched)
//! otherwise
//!     ↓
//! construct live Runtime (vars_os + lossy env policy, no panic)
//!     ↓
//! load configuration
//!     ↓
//! execute command
//! ```
//!
//! All branching lives in [`run_inner`], which takes an optional injectable
//! [`Runtime`] ([`run_with`]) so precedence is testable without touching the
//! real home directory or process environment. Integration tests in `tests/`
//! exercise the compiled binary end to end.

mod args;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use args::{Command, HelpTopic};
use aurel_config::{
    collect_aurel_env, discover_project_file, global_config_file, load, render_show, LoadRequest,
};

/// Exit code for CLI usage errors (unknown flags, bad commands/values).
const EXIT_USAGE_ERROR: i32 = 2;
/// Exit code for configuration and other runtime errors.
const EXIT_RUNTIME_ERROR: i32 = 1;

/// Policy for non-Unicode OS arguments: convert with `to_string_lossy`
/// (unrepresentable sequences become U+FFFD) and continue through the normal
/// parser. This never panics: `args_os` does not validate Unicode and
/// `to_string_lossy` is total. A lossy argument simply fails to match a known
/// flag or command and is handled as a deterministic usage error (exit 2).
fn normalize_args(raw: &[OsString]) -> Vec<String> {
    raw.iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
}

/// Process state [`run`] needs but tests must fake: working directory,
/// relevant directory variables, and the `AUREL_*` environment.
struct Runtime {
    cwd: PathBuf,
    appdata: Option<String>,
    home: Option<String>,
    env: HashMap<String, String>,
}

impl Runtime {
    /// Capture the real process state without panicking on non-Unicode data.
    ///
    /// Environment collection uses `vars_os` plus [`collect_aurel_env`]:
    /// non-Unicode keys are ignored (recognized names are pure ASCII, so an
    /// undecodable key cannot name a real setting) and values are
    /// lossy-converted, flowing into normal validation. Directory lookups
    /// use the non-panicking `var(...).ok()` form.
    fn live() -> Self {
        Runtime {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            appdata: std::env::var("APPDATA").ok(),
            home: std::env::var("HOME").ok(),
            env: collect_aurel_env(std::env::vars_os()),
        }
    }
}

/// Render the top-level help text to `out`.
fn print_help(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())?;
    writeln!(
        out,
        "Autonomous Utility & Reasoning Engine for Logic - Phase 1 CLI + config"
    )?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "    aurel [OPTIONS] [COMMAND]")?;
    writeln!(out)?;
    writeln!(out, "OPTIONS:")?;
    writeln!(out, "    -h, --help              Print this help message")?;
    writeln!(out, "    -V, --version           Print version information")?;
    writeln!(
        out,
        "        --config <path>     Use this config file instead of"
    )?;
    writeln!(
        out,
        "                            discovered global/project files"
    )?;
    writeln!(
        out,
        "        --log-level <level> error|warn|info|debug|trace"
    )?;
    writeln!(out)?;
    writeln!(out, "COMMANDS:")?;
    writeln!(out, "    config show    Print the effective configuration")?;
    writeln!(out)?;
    writeln!(out, "CONFIG FILES (TOML):")?;
    writeln!(out, "    global:  %APPDATA%\\aurel\\config.toml (Windows)")?;
    writeln!(out, "             ~/.config/aurel/config.toml (Linux/WSL)")?;
    writeln!(
        out,
        "    project: nearest .aurel/config.toml at or above the"
    )?;
    writeln!(out, "             working directory")?;
    writeln!(
        out,
        "Precedence: defaults < global < project < env (AUREL_*) < CLI."
    )?;
    Ok(())
}

/// Render the `config` command help text to `out`.
fn print_config_help(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "    aurel config show")?;
    writeln!(out)?;
    writeln!(
        out,
        "Prints the effective TOML configuration after applying"
    )?;
    writeln!(
        out,
        "precedence: defaults < global < project < env (AUREL_*) < CLI."
    )?;
    Ok(())
}

/// Render the version line to `out`.
fn print_version(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())
}

/// Build the config [`LoadRequest`] for a parsed command line. An explicit
/// `--config` path replaces global/project discovery (`project_searched` is
/// then false, rendering "not searched"); otherwise both are discovered
/// from the runtime (`project_searched` true, rendering the found path or
/// "searched, none found").
fn build_request(parsed: &args::Parsed, rt: &Runtime) -> LoadRequest {
    let (global_file, project_file, project_searched, explicit_file) = match &parsed.config_path {
        Some(path) => (None, None, false, Some(path.clone())),
        None => (
            global_config_file(rt.appdata.as_deref(), rt.home.as_deref()),
            discover_project_file(&rt.cwd),
            true,
            None,
        ),
    };
    LoadRequest {
        global_file,
        project_file,
        project_searched,
        explicit_file,
        env: rt.env.clone(),
        cli_log_level: parsed.log_level,
    }
}

/// Core CLI logic, testable without process spawning.
///
/// `--help`/`--version`/bare invocations short-circuit before any runtime
/// state (environment, config files) is touched, so they stay independent
/// of configuration/environment failures. Returns a process exit code:
/// `0` on success, `2` on usage error, `1` on configuration/runtime failure
/// or output failure.
fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    run_inner(args, out, err, None)
}

/// [`run`] with an injected [`Runtime`] for tests.
#[cfg(test)]
fn run_with(args: &[String], out: &mut dyn Write, err: &mut dyn Write, rt: &Runtime) -> i32 {
    run_inner(args, out, err, Some(rt))
}

fn run_inner(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    rt: Option<&Runtime>,
) -> i32 {
    let parsed = match args::parse(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            let _ = writeln!(err, "tip: run 'aurel --help' for usage.");
            return EXIT_USAGE_ERROR;
        }
    };

    if let Some(topic) = parsed.help {
        let ok = match topic {
            HelpTopic::Top => print_help(out),
            HelpTopic::Config => print_config_help(out),
        };
        return if ok.is_ok() { 0 } else { 1 };
    }
    if parsed.version {
        return if print_version(out).is_ok() { 0 } else { 1 };
    }

    match parsed.command {
        // Bare invocation: help, without touching config files.
        None => {
            if print_help(out).is_ok() {
                0
            } else {
                1
            }
        }
        // Command path: only here is the live runtime constructed.
        Some(Command::ConfigShow) => match rt {
            Some(injected) => execute_config_show(&parsed, out, err, injected),
            None => {
                let live = Runtime::live();
                execute_config_show(&parsed, out, err, &live)
            }
        },
    }
}

fn execute_config_show(
    parsed: &args::Parsed,
    out: &mut dyn Write,
    err: &mut dyn Write,
    rt: &Runtime,
) -> i32 {
    match load(&build_request(parsed, rt)) {
        Ok(cfg) => {
            if writeln!(out, "{}", render_show(&cfg)).is_ok() {
                0
            } else {
                1
            }
        }
        Err(e) => {
            let _ = writeln!(err, "{e}");
            EXIT_RUNTIME_ERROR
        }
    }
}

fn main() {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args = normalize_args(&raw);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    // Note: the live Runtime is constructed lazily inside `run`, only on the
    // command path — help/version/bare never touch environment or config.
    let code = run(&args, &mut out, &mut err);
    // Ensure buffered output is flushed before exiting with a code.
    let _ = out.flush();
    let _ = err.flush();
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime() -> Runtime {
        Runtime {
            cwd: std::env::temp_dir().join(format!("aurel-cli-test-{}-noroot", std::process::id())),
            appdata: None,
            home: None,
            env: HashMap::new(),
        }
    }

    fn run_to_string(args: &[&str], rt: &Runtime) -> (i32, String, String) {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with(&owned, &mut out, &mut err, rt);
        (
            code,
            String::from_utf8(out).expect("stdout must be UTF-8"),
            String::from_utf8(err).expect("stderr must be UTF-8"),
        )
    }

    #[test]
    fn no_args_prints_help_to_stdout() {
        let rt = test_runtime();
        let (code, out, err) = run_to_string(&[], &rt);
        assert_eq!(code, 0);
        assert!(out.contains("USAGE:"), "help must contain USAGE");
        assert!(err.is_empty());
    }

    #[test]
    fn help_flag_prints_help() {
        let rt = test_runtime();
        for flag in ["--help", "-h"] {
            let (code, out, err) = run_to_string(&[flag], &rt);
            assert_eq!(code, 0, "flag {flag}");
            assert!(out.contains("USAGE:"), "flag {flag}");
            assert!(err.is_empty());
        }
    }

    #[test]
    fn version_flag_prints_version() {
        let rt = test_runtime();
        for flag in ["--version", "-V"] {
            let (code, out, err) = run_to_string(&[flag], &rt);
            assert_eq!(code, 0, "flag {flag}");
            assert!(
                out.contains(&format!("aurel {}", aurel_core::version())),
                "flag {flag}"
            );
            assert!(err.is_empty());
        }
    }

    #[test]
    fn unknown_argument_returns_exit_code_2() {
        let rt = test_runtime();
        let (code, _out, err) = run_to_string(&["--wat"], &rt);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("--wat"));
    }

    #[test]
    fn unknown_command_returns_exit_code_2() {
        let rt = test_runtime();
        let (code, _out, err) = run_to_string(&["frobnicate"], &rt);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("frobnicate"));
    }

    #[test]
    fn version_with_command_returns_exit_code_2() {
        let rt = test_runtime();
        let (code, _out, _err) = run_to_string(&["--version", "config", "show"], &rt);
        assert_eq!(code, EXIT_USAGE_ERROR);
    }

    #[test]
    fn config_show_prints_effective_config() {
        let dir = std::env::temp_dir().join(format!("aurel-cli-test-{}-show", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = \"debug\"\n").expect("test config");
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env: HashMap::new(),
        };
        let (code, out, err) = run_to_string(&["config", "show"], &rt);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("log_level = \"debug\""), "got: {out:?}");
        assert!(out.contains(&dir.display().to_string()), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_log_level_beats_file_in_config_show() {
        let dir =
            std::env::temp_dir().join(format!("aurel-cli-test-{}-override", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = \"error\"\n").expect("test config");
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env: HashMap::new(),
        };
        let (code, out, err) = run_to_string(&["--log-level", "trace", "config", "show"], &rt);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("log_level = \"trace\""), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_config_reports_exit_code_1() {
        let dir = std::env::temp_dir().join(format!("aurel-cli-test-{}-bad", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = [oops\n").expect("test config");
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env: HashMap::new(),
        };
        let (code, _out, err) = run_to_string(&["config", "show"], &rt);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        assert!(err.contains("invalid TOML"), "got: {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn help_version_and_bare_ignore_broken_runtime_state() {
        // Help/version/bare paths never touch environment or config files,
        // so they succeed even when the runtime state would fail loading.
        let dir =
            std::env::temp_dir().join(format!("aurel-cli-test-{}-broken-rt", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = [oops\n").expect("test config");
        let mut env = HashMap::new();
        env.insert("AUREL_LOG_LEVEL".to_string(), "chatty".to_string());
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env,
        };
        for argv in [
            vec![],
            vec!["--help"],
            vec!["-h"],
            vec!["--version"],
            vec!["-V"],
            vec!["config"],
            vec!["config", "--help"],
        ] {
            let (code, _out, _err) = run_to_string(&argv, &rt);
            assert_eq!(code, 0, "argv {argv:?} must not touch broken runtime state");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_config_prints_config_help() {
        let rt = test_runtime();
        let (code, out, err) = run_to_string(&["config"], &rt);
        assert_eq!(code, 0);
        assert!(out.contains("aurel config show"), "got: {out:?}");
        assert!(err.is_empty());
    }

    #[test]
    fn non_unicode_argument_is_handled_without_panic() {
        // Regression test: the args_os -> lossy conversion path must be total.
        let raw = vec![non_unicode_arg()];
        let args = normalize_args(&raw);
        assert_eq!(args.len(), 1);
        let rt = test_runtime();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with(&args, &mut out, &mut err, &rt);
        // Lossy output matches no known flag or command, so it is a usage
        // error — reaching this assertion proves no panic occurred.
        assert_eq!(code, EXIT_USAGE_ERROR);
    }

    /// Non-Unicode OS input for the current platform (bytes that are invalid
    /// UTF-8 and would make `std::env::args()` panic).
    #[cfg(unix)]
    fn non_unicode_arg() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xFF, b'x'])
    }

    /// Non-Unicode OS input for the current platform (an unpaired surrogate,
    /// which is not valid Unicode).
    #[cfg(windows)]
    fn non_unicode_arg() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xD800, b'x' as u16])
    }

    #[cfg(not(any(unix, windows)))]
    fn non_unicode_arg() -> OsString {
        // Fallback: no portable invalid-Unicode constructor here, so exercise
        // the conversion path with a plain argument instead.
        OsString::from("--wat")
    }
}
