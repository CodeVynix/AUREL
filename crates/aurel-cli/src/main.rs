//! `aurel` CLI entry point (Phase 0).
//!
//! Dependency-free argument handling over `std::env::args`.
//! Only `--version`/`-V` and `--help`/`-h` exist. All logic lives in [`run`]
//! so it is unit-testable without spawning a subprocess. Integration tests
//! in `tests/cli.rs` exercise the compiled binary end to end.

use std::io::Write;

/// Exit code for CLI usage errors (unknown flags).
const EXIT_USAGE_ERROR: i32 = 2;

/// Render the Phase 0 help text to `out`.
fn print_help(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())?;
    writeln!(
        out,
        "Autonomous Utility & Reasoning Engine for Logic - Phase 0 foundation"
    )?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "    aurel [OPTIONS]")?;
    writeln!(out)?;
    writeln!(out, "OPTIONS:")?;
    writeln!(out, "    -h, --help       Print this help message")?;
    writeln!(out, "    -V, --version    Print version information")?;
    Ok(())
}

/// Render the version line to `out`.
fn print_version(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())
}

/// Core CLI logic, testable without process spawning.
///
/// * `args` — arguments excluding the program name (i.e. `env::args().skip(1)`).
/// * Returns a process exit code: `0` on success, `2` on usage error,
///   `1` if writing output itself failed.
fn run(args: &[String], out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    if args.is_empty() {
        return if print_help(out).is_ok() { 0 } else { 1 };
    }

    match args[0].as_str() {
        "-h" | "--help" => {
            if print_help(out).is_ok() {
                0
            } else {
                1
            }
        }
        "-V" | "--version" => {
            if print_version(out).is_ok() {
                0
            } else {
                1
            }
        }
        unknown => {
            let _ = writeln!(err, "error: unexpected argument '{unknown}'");
            let _ = writeln!(err, "tip: run 'aurel --help' for usage.");
            EXIT_USAGE_ERROR
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let code = run(&args, &mut out, &mut err);
    // Ensure buffered output is flushed before exiting with a code.
    let _ = out.flush();
    let _ = err.flush();
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_to_string(args: &[&str]) -> (i32, String, String) {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run(&owned, &mut out, &mut err);
        (
            code,
            String::from_utf8(out).expect("stdout must be UTF-8"),
            String::from_utf8(err).expect("stderr must be UTF-8"),
        )
    }

    #[test]
    fn no_args_prints_help_to_stdout() {
        let (code, out, err) = run_to_string(&[]);
        assert_eq!(code, 0);
        assert!(out.contains("USAGE:"), "help must contain USAGE");
        assert!(err.is_empty());
    }

    #[test]
    fn help_flag_prints_help() {
        for flag in ["--help", "-h"] {
            let (code, out, err) = run_to_string(&[flag]);
            assert_eq!(code, 0, "flag {flag}");
            assert!(out.contains("USAGE:"), "flag {flag}");
            assert!(err.is_empty());
        }
    }

    #[test]
    fn version_flag_prints_version() {
        for flag in ["--version", "-V"] {
            let (code, out, err) = run_to_string(&[flag]);
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
        let (code, _out, err) = run_to_string(&["--wat"]);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("--wat"));
    }
}
