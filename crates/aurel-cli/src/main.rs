//! `aurel` CLI entry point.
//!
//! Phase 1: dependency-free argument handling ([`args`]) over
//! `std::env::args_os` with an explicit lossy Unicode policy (see
//! [`normalize_args`]), TOML configuration with documented precedence
//! (`aurel-config`), and a `config show` command. All branching lives in
//! [`run`], which takes an injectable [`Runtime`] so precedence is testable
//! without touching the real home directory or process environment.
//! Integration tests in `tests/` exercise the compiled binary end to end.

mod args;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use args::{Command, HelpTopic};
use aurel_config::{
    discover_project_file, global_config_file, load, render_show, LoadRequest, ENV_LOG_LEVEL,
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
    /// Capture the real process state.
    fn live() -> Self {
        let env: HashMap<String, String> = std::env::vars()
            .filter(|(k, _)| k == ENV_LOG_LEVEL || k.starts_with("AUREL_"))
            .collect();
        Runtime {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            appdata: std::env::var("APPDATA").ok(),
            home: std::env::var("HOME").ok(),
            env,
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
    writeln!(out, "aurel-config {}", aurel_core::version())?;
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
/// `--config` path replaces global/project discovery; otherwise both are
/// discovered from the runtime.
fn build_request(parsed: &args::Parsed, rt: &Runtime) -> LoadRequest {
    let (global_file, project_file, explicit_file) = match &parsed.config_path {
        Some(path) => (None, None, Some(path.clone())),
        None => (
            global_config_file(rt.appdata.as_deref(), rt.home.as_deref()),
            discover_project_file(&rt.cwd),
            None,
        ),
    };
    LoadRequest {
        global_file,
        project_file,
        explicit_file,
        env: rt.env.clone(),
        cli_log_level: parsed.log_level,
    }
}

/// Core CLI logic, testable without process spawning.
///
/// `--help`/`--version` short-circuit before any config file is read. A bare
/// invocation prints top-level help (Phase 0 behavior) without reading
/// config. Returns a process exit code: `0` on success, `2` on usage error,
/// `1` on configuration/runtime failure or output failure.
fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write, rt: &Runtime) -> i32 {
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
        Some(Command::ConfigShow) => match load(&build_request(&parsed, rt)) {
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
        },
    }
}

fn main() {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args = normalize_args(&raw);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let code = run(&args, &mut out, &mut err, &Runtime::live());
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
        let code = run(&owned, &mut out, &mut err, rt);
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
    fn non_unicode_argument_is_handled_without_panic() {
        // Regression test: the args_os -> lossy conversion path must be total.
        let raw = vec![non_unicode_arg()];
        let args = normalize_args(&raw);
        assert_eq!(args.len(), 1);
        let rt = test_runtime();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&args, &mut out, &mut err, &rt);
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
