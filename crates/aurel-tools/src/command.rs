//! Controlled shell/command execution (`aurel-tools`).
//!
//! Direct process execution only — never a shell. Programs run with an
//! explicit argument vector in an explicit working directory; no pipes,
//! redirects, globs, or expansions are interpreted, because none are
//! invoked. Every run is bounded (wall-clock timeout, per-stream output
//! caps with drain-discard so big output cannot deadlock), cancellable,
//! and reported as a [`CommandResult`] that distinguishes success,
//! nonzero exit, timeout, cancellation, and launch failure.
//!
//! Security boundaries (see `docs/architecture.md` for the full statement):
//!
//! - The working directory must resolve inside the workspace sandbox.
//! - Program identity is resolved explicitly: absolute paths must exist;
//!   bare names search `PATH` (plus `PATHEXT` on Windows) and skip the
//!   current directory. What runs is always shown exactly before approval.
//! - The environment is inherited minus secret variables (see
//!   `SECRET_ENV_VARS`): ordinary variables pass through untouched
//!   (predictable builds), credentials never do. Since no shell ever
//!   interprets arguments, values cannot smuggle expansions either.
//! - Captured output is scrubbed of the configured secret before it is
//!   stored or displayed (independent second defense).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How one command execution ended. Every terminal state is representable;
/// nothing collapses into a generic failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandStatus {
    /// Exit code 0 within budget.
    Success,
    /// Ran to completion with a nonzero exit code (or signal death, where
    /// `exit_code` is then `None`).
    NonZeroExit,
    /// Killed after exceeding the timeout.
    Timeout,
    /// Killed after cooperative cancellation was observed.
    Cancelled,
    /// The process never started (missing program, bad working directory…).
    LaunchFailed,
}

/// What to run: an explicit program plus literal arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRequest {
    /// Program name or path, resolved by [`resolve_program`] at prepare.
    pub program: String,
    /// Literal arguments; never interpreted by any shell.
    pub args: Vec<String>,
    /// Working directory, resolved inside the workspace at prepare.
    pub workdir: PathBuf,
    /// Total wall-clock budget for the run.
    pub timeout: Duration,
    /// Secret value scrubbed from captured output, if any.
    pub redact: Option<String>,
}

/// The complete, bounded record of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    /// Resolved program path that actually ran (or was attempted).
    pub program: String,
    pub status: CommandStatus,
    /// Process exit code; `None` when no code exists (signal death,
    /// launch failure before exec).
    pub exit_code: Option<i32>,
    /// Captured stdout (lossy UTF-8), capped, secret-scrubbed.
    pub stdout: String,
    /// Captured stderr (lossy UTF-8), capped, secret-scrubbed.
    pub stderr: String,
    /// True when either stream exceeded its cap (rest drained+discarded).
    pub truncated: bool,
    /// Wall-clock time consumed.
    pub duration_ms: u64,
    /// Human detail for non-success states (OS message, timeout length…).
    /// Never contains the redacted secret.
    pub detail: String,
}

/// Project build/test commands detected from workspace marker files.
/// First match wins, in the table order below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildCommands {
    /// Short label for review display (e.g. `"cargo"`).
    pub kind: &'static str,
    pub build_program: String,
    pub build_args: Vec<String>,
    pub test_program: String,
    pub test_args: Vec<String>,
}

/// Detect build/test commands from marker files directly inside `dir`.
/// Returns `None` when nothing is recognized — a clean rejection, not an
/// error. Marker table: `Cargo.toml` → cargo, `package.json` → npm,
/// `go.mod` → go, `Makefile` → make.
pub fn detect_build_commands(dir: &Path) -> Option<BuildCommands> {
    // (marker, kind, build argv, test argv)
    const TABLE: &[(&str, &str, &[&str], &[&str])] = &[
        (
            "Cargo.toml",
            "cargo",
            &["cargo", "build"],
            &["cargo", "test"],
        ),
        (
            "package.json",
            "npm",
            &["npm", "run", "build"],
            &["npm", "test"],
        ),
        (
            "go.mod",
            "go",
            &["go", "build", "./..."],
            &["go", "test", "./..."],
        ),
        ("Makefile", "make", &["make"], &["make", "test"]),
    ];
    for (marker, kind, build, test) in TABLE {
        if dir.join(marker).is_file() {
            let split = |argv: &[&str]| {
                (
                    argv[0].to_string(),
                    argv[1..].iter().map(|arg| (*arg).to_string()).collect(),
                )
            };
            let (build_program, build_args) = split(build);
            let (test_program, test_args) = split(test);
            return Some(BuildCommands {
                kind,
                build_program,
                build_args,
                test_program,
                test_args,
            });
        }
    }
    None
}

/// Resolve a program name to an executable path without a shell.
///
/// - Names containing a path separator (either slash) are treated as paths:
///   relative ones join the current directory context implicitly by being
///   used as given, but must exist as files; absolute ones must exist too.
///   (Workspace containment for *explicit user paths* is enforced by the
///   caller via [`crate::ToolContext::resolve`]; bare names below never
///   touch the filesystem outside `PATH`.)
/// - Bare names search `path_dirs` in order. On Windows each `PATHEXT`
///   suffix is tried. The current directory is never implicitly searched.
/// - On Unix, candidates must have at least one execute bit; on Windows
///   existence suffices (executability is by extension).
pub fn resolve_program(name: &str, path_dirs: &[PathBuf]) -> Result<PathBuf, crate::ToolError> {
    use crate::ToolError;
    if name.is_empty() {
        return Err(ToolError::InvalidPath(
            "program name must not be empty".into(),
        ));
    }
    if name.contains('\0') {
        return Err(ToolError::InvalidPath(
            "program name contains a NUL byte".into(),
        ));
    }
    if name.contains('/') || name.contains('\\') {
        let candidate = PathBuf::from(name);
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
        return Err(ToolError::NotFound {
            path: format!("program not found: '{name}'"),
        });
    }
    for dir in path_dirs {
        #[cfg(windows)]
        {
            for candidate in windows_candidates(dir, name) {
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        #[cfg(not(windows))]
        {
            let candidate = dir.join(name);
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }
    Err(ToolError::NotFound {
        path: format!("program not found on PATH: '{name}'"),
    })
}

#[cfg(windows)]
fn windows_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut candidates = vec![dir.join(name)];
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    // Avoid `.NAME.EXE` when the name already carries an extension.
    let has_extension = Path::new(name).extension().is_some();
    if !has_extension {
        for extension in extensions.split(';') {
            let extension = extension.trim();
            if extension.is_empty() {
                continue;
            }
            candidates.push(dir.join(format!("{name}{extension}")));
        }
    }
    candidates
}

/// True for regular files that the OS would execute (execute bit on Unix,
/// existence on Windows — extensions are handled by the caller).
pub(crate) fn is_executable_file(path: &Path) -> bool {
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        Err(_) => return false,
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `PATH` directories from the process environment, in order.
pub fn system_path_dirs() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect())
        .unwrap_or_default()
}

/// Scrub a secret value (bare and `Bearer ` forms) from text. Empty or
/// missing secrets pass text through untouched (an empty needle would
/// match everywhere).
pub fn scrub_secret(text: &str, secret: Option<&str>) -> String {
    let Some(key) = secret.filter(|key| !key.is_empty()) else {
        return text.to_string();
    };
    let bearer = format!("Bearer {key}");
    text.replace(&bearer, "Bearer <redacted>")
        .replace(key, "<redacted>")
}

/// Environment variables never inherited by child commands, by exact
/// (ASCII case-insensitive) name. Only genuine secret-bearers belong here;
/// every other variable passes through untouched so builds keep working.
/// Output redaction stays a separate, independent defense.
const SECRET_ENV_VARS: &[&str] = &["AUREL_API_KEY"];

/// True when an environment key names a filtered secret. Byte-exact and
/// total (no Unicode decoding involved): only an exact ASCII
/// case-insensitive match filters, so near-misses like `AUREL_API_KEY_2`
/// pass through.
fn is_secret_env(key: &std::ffi::OsStr) -> bool {
    SECRET_ENV_VARS.iter().any(|denied| {
        key.as_encoded_bytes()
            .eq_ignore_ascii_case(denied.as_bytes())
    })
}

/// Run one command to completion: bounded, cancellable, secret-scrubbed.
///
/// - The child environment is the parent's minus secret variables (see
///   `SECRET_ENV_VARS`): ordinary variables pass through, credentials
///   never do.
/// - `stdin` is always null: commands that wait on input fail fast instead
///   of hanging the loop.
/// - Output streams are pumped by scoped reader threads into capped
///   buffers; beyond the cap bytes are drained and discarded (no deadlock,
///   bounded memory), setting `truncated`.
/// - The wait loop polls every 10 ms: deadline expiry kills and reports
///   [`CommandStatus::Timeout`], a set `should_cancel` kills and reports
///   [`CommandStatus::Cancelled`].
/// - Never panics on process behavior; every outcome is a `CommandResult`.
pub fn run_command(
    request: &CommandRequest,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> CommandResult {
    let started = Instant::now();
    let mut result = CommandResult {
        program: request.program.clone(),
        status: CommandStatus::LaunchFailed,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        truncated: false,
        duration_ms: 0,
        detail: String::new(),
    };
    if should_cancel.is_some_and(|cancel| cancel()) {
        result.status = CommandStatus::Cancelled;
        result.detail = "cancelled before start".to_string();
        return result;
    }
    let mut command = Command::new(&request.program);
    command
        .args(&request.args)
        .current_dir(&request.workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Filtered inheritance: ordinary variables pass through, secrets do
    // not. `env_clear` + explicit re-add keeps the rule total — including
    // for odd keys `vars()`-style iteration might otherwise smuggle past.
    command.env_clear();
    for (key, value) in std::env::vars_os() {
        if !is_secret_env(&key) {
            command.env(key, value);
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            result.detail = format!("spawn failed: {error}");
            return result;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let cap = crate::Limits::default().max_command_output_bytes;
    // Scoped readers: joined before return, never detached, no 'static.
    let (captured, status) = std::thread::scope(|scope| {
        let stdout_handle =
            stdout.map(|stream| scope.spawn(move || read_capped_stream(stream, cap)));
        let stderr_handle =
            stderr.map(|stream| scope.spawn(move || read_capped_stream(stream, cap)));
        let mut status = CommandStatus::Success;
        loop {
            if should_cancel.is_some_and(|cancel| cancel()) {
                let _ = child.kill();
                status = CommandStatus::Cancelled;
                break;
            }
            if started.elapsed() >= request.timeout {
                let _ = child.kill();
                status = CommandStatus::Timeout;
                break;
            }
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }
        // Reap exactly once; a kill above guarantees prompt return.
        let exit = child.wait().ok();
        let (out_bytes, out_truncated) = stdout_handle
            .map(|handle| handle.join().unwrap_or_default())
            .unwrap_or_default();
        let (err_bytes, err_truncated) = stderr_handle
            .map(|handle| handle.join().unwrap_or_default())
            .unwrap_or_default();
        if status == CommandStatus::Success && !exit.is_some_and(|exit| exit.success()) {
            status = CommandStatus::NonZeroExit;
        }
        // Completed runs report their code; killed processes keep None
        // (platform kill codes would mislead).
        if matches!(status, CommandStatus::Success | CommandStatus::NonZeroExit) {
            result.exit_code = exit.and_then(|exit| exit.code());
        }
        ((out_bytes, out_truncated, err_bytes, err_truncated), status)
    });
    let (out_bytes, out_truncated, err_bytes, err_truncated) = captured;
    result.status = status;
    result.truncated = out_truncated || err_truncated;
    result.stdout = scrub_secret(
        &String::from_utf8_lossy(&out_bytes),
        request.redact.as_deref(),
    );
    result.stderr = scrub_secret(
        &String::from_utf8_lossy(&err_bytes),
        request.redact.as_deref(),
    );
    if result.status == CommandStatus::Timeout {
        result.detail = format!("exceeded {}s timeout", request.timeout.as_secs());
    }
    result.duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    result
}

/// Read a stream fully, keeping at most `cap` bytes and draining the rest
/// so the writer never blocks forever. Returns bytes plus truncation flag.
fn read_capped_stream(stream: impl Read, cap: usize) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    let mut stream = stream;
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                let room = cap.saturating_sub(kept.len());
                if room > 0 {
                    kept.extend_from_slice(&chunk[..read.min(room)]);
                }
                if kept.len() >= cap {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (kept, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_request(program: &str, args: &[&str]) -> CommandRequest {
        CommandRequest {
            program: program.to_string(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
            workdir: std::env::temp_dir(),
            timeout: Duration::from_secs(10),
            redact: None,
        }
    }

    #[test]
    fn resolve_program_rejects_garbage() {
        assert!(resolve_program("", &[]).is_err());
        assert!(resolve_program("has\0nul", &[]).is_err());
        assert!(resolve_program("no-such-program-aurel-xyz", &[]).is_err());
        // Absolute missing paths fail, never silently.
        #[cfg(unix)]
        assert!(resolve_program("/no/such/binary-aurel", &[]).is_err());
        #[cfg(windows)]
        assert!(resolve_program("C:\\no\\such\\binary-aurel.exe", &[]).is_err());
    }

    #[test]
    fn resolve_program_finds_bare_names_in_order() {
        let first = std::env::temp_dir().join(format!("aurel-path-a-{}", std::process::id()));
        let second = std::env::temp_dir().join(format!("aurel-path-b-{}", std::process::id()));
        std::fs::create_dir_all(&first).expect("mkdir");
        std::fs::create_dir_all(&second).expect("mkdir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let target = second.join("myprog");
            std::fs::write(&target, b"#!/bin/sh\n").expect("write");
            let mut permissions = std::fs::metadata(&target).expect("meta").permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&target, permissions).expect("chmod");
            // Found in the second dir; a non-executable same-name file in
            // the first dir must not shadow it.
            std::fs::write(first.join("myprog"), b"plain\n").expect("write");
            assert_eq!(
                resolve_program("myprog", &[first.clone(), second.clone()]).expect("resolve"),
                target
            );
            assert!(resolve_program("myprog", &[first]).is_err());
        }
        #[cfg(windows)]
        {
            let target = second.join("myprog.exe");
            std::fs::write(&target, b"MZ").expect("write");
            // PATHEXT lookup finds the extensioned file from a bare name.
            // Compare case-insensitively: the candidate carries PATHEXT's
            // casing while the file may not.
            let found =
                resolve_program("myprog", &[first.clone(), second.clone()]).expect("resolve");
            assert_eq!(
                found.to_string_lossy().to_lowercase(),
                target.to_string_lossy().to_lowercase()
            );
        }
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn detect_build_commands_matches_markers_in_order() {
        let root = std::env::temp_dir().join(format!("aurel-detect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        assert_eq!(detect_build_commands(&root), None);
        std::fs::write(root.join("Makefile"), "all:\n").expect("write");
        std::fs::write(root.join("package.json"), "{}\n").expect("write");
        let found = detect_build_commands(&root).expect("npm wins over make");
        assert_eq!(found.kind, "npm");
        assert_eq!(found.test_program, "npm");
        assert_eq!(found.test_args, vec!["test".to_string()]);
        std::fs::write(root.join("Cargo.toml"), "[package]\n").expect("write");
        let found = detect_build_commands(&root).expect("cargo wins over npm");
        assert_eq!(found.kind, "cargo");
        assert_eq!(found.build_args, vec!["build".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scrub_secret_redacts_both_forms() {
        assert_eq!(
            scrub_secret("key sk-abc here", Some("sk-abc")),
            "key <redacted> here"
        );
        assert_eq!(
            scrub_secret("auth Bearer sk-abc!", Some("sk-abc")),
            "auth Bearer <redacted>!"
        );
        assert_eq!(scrub_secret("plain", None), "plain");
        assert_eq!(scrub_secret("plain", Some("")), "plain");
    }

    #[test]
    fn secret_env_matching_is_exact_but_case_blind() {
        use std::ffi::OsStr;
        assert!(is_secret_env(OsStr::new("AUREL_API_KEY")));
        assert!(is_secret_env(OsStr::new("aurel_api_key")));
        assert!(is_secret_env(OsStr::new("Aurel_Api_Key")));
        assert!(!is_secret_env(OsStr::new("AUREL_MODEL")));
        assert!(!is_secret_env(OsStr::new("AUREL_API_KEY_2")));
        assert!(!is_secret_env(OsStr::new("X_AUREL_API_KEY")));
        assert!(!is_secret_env(OsStr::new("")));
        assert!(!is_secret_env(OsStr::new("PATH")));
    }

    #[test]
    fn child_env_filters_secrets_keeps_ordinary() {
        // A command dumping its own environment proves the filter
        // end-to-end (redact=None, so only the inheritance rule is at work).
        // Save/restore: never clobber real developer environment entries.
        let saved_marker = std::env::var_os("AUREL_TEST_MARKER_XYZ");
        let saved_key = std::env::var_os("AUREL_API_KEY");
        std::env::set_var("AUREL_TEST_MARKER_XYZ", "marker-value-123");
        std::env::set_var("AUREL_API_KEY", "sk-test-must-not-leak");
        let result = run_command(&env_dump_request(), None);
        match saved_marker {
            Some(value) => std::env::set_var("AUREL_TEST_MARKER_XYZ", value),
            None => std::env::remove_var("AUREL_TEST_MARKER_XYZ"),
        }
        match saved_key {
            Some(value) => std::env::set_var("AUREL_API_KEY", value),
            None => std::env::remove_var("AUREL_API_KEY"),
        }
        assert_eq!(result.status, CommandStatus::Success, "got: {result:?}");
        assert!(
            result.stdout.contains("marker-value-123"),
            "ordinary vars must pass through: {:?}",
            &result.stdout[..result.stdout.len().min(200)]
        );
        assert!(
            !result.stdout.contains("sk-test-must-not-leak"),
            "credential leaked into child env: {:?}",
            &result.stdout[..result.stdout.len().min(500)]
        );
    }

    /// A request dumping the child's own environment. `env` on Unix;
    /// `cmd /C set` on Windows with an explicitly resolved `cmd.exe`
    /// (a builtin, not a PATH program — no shell lookup rules involved).
    #[cfg(unix)]
    fn env_dump_request() -> CommandRequest {
        let program = resolve_program("env", &system_path_dirs()).expect("env fixture present");
        CommandRequest {
            program: program.display().to_string(),
            args: Vec::new(),
            workdir: std::env::temp_dir(),
            timeout: std::time::Duration::from_secs(20),
            redact: None,
        }
    }

    /// A request dumping the child's own environment. `env` on Unix;
    /// `cmd /C set` on Windows with an explicitly resolved `cmd.exe`
    /// (a builtin, not a PATH program — no shell lookup rules involved).
    #[cfg(windows)]
    fn env_dump_request() -> CommandRequest {
        let system = std::env::var("SystemRoot").unwrap_or("C:\\Windows".to_string());
        CommandRequest {
            program: format!("{system}\\System32\\cmd.exe"),
            args: vec!["/C".to_string(), "set".to_string()],
            workdir: std::env::temp_dir(),
            timeout: std::time::Duration::from_secs(20),
            redact: None,
        }
    }

    /// A request dumping the child's own environment. `env` on Unix;
    /// `cmd /C set` on Windows with an explicitly resolved `cmd.exe`
    /// (a builtin, not a PATH program — no shell lookup rules involved).
    #[cfg(not(any(unix, windows)))]
    fn env_dump_request() -> CommandRequest {
        panic!("no env-dump fixture for this platform")
    }

    #[test]
    fn run_cargo_version_succeeds() {
        // `cargo` drives the dev environment by definition, so it is the
        // one portable success fixture (no shell involved).
        let result = run_command(&test_request("cargo", &["--version"]), None);
        assert_eq!(result.status, CommandStatus::Success);
        assert!(result.exit_code == Some(0));
        assert!(result.stdout.contains("cargo"), "got: {:?}", result.stdout);
        assert!(!result.truncated);
    }

    #[test]
    fn run_missing_program_is_launch_failure() {
        let result = run_command(&test_request("aurel-no-such-program-xyz", &[]), None);
        assert_eq!(result.status, CommandStatus::LaunchFailed);
        assert_eq!(result.exit_code, None);
        assert!(!result.detail.is_empty());
    }

    #[test]
    fn run_nonzero_exit_is_typed() {
        let result = run_command(&test_request("cargo", &["--bad-flag-aurel-xyz"]), None);
        assert_eq!(result.status, CommandStatus::NonZeroExit);
        assert!(result.exit_code.is_some_and(|code| code != 0));
        assert!(
            !result.stderr.is_empty(),
            "cargo explains bad flags on stderr"
        );
    }

    #[test]
    fn run_truncates_big_output() {
        // `cargo --help` exceeds a 64-byte cap deterministically.
        let big = b"0123456789".repeat(20);
        let (kept, truncated) = read_capped_stream(&big[..], 64);
        assert!(truncated);
        assert_eq!(kept.len(), 64);
        assert_eq!(&kept[..], &big[..64]);
        let (kept, truncated) = read_capped_stream(&big[..], 10_000);
        assert!(!truncated);
        assert_eq!(kept, big);
    }

    #[test]
    fn run_timeout_kills_and_reports() {
        let slow: CommandRequest = {
            #[cfg(unix)]
            {
                CommandRequest {
                    program: "/bin/sleep".to_string(),
                    args: vec!["5".to_string()],
                    workdir: std::env::temp_dir(),
                    timeout: Duration::from_millis(300),
                    redact: None,
                }
            }
            #[cfg(windows)]
            {
                // `ping -n 6` takes ~5s with no shell involved.
                CommandRequest {
                    program: "ping".to_string(),
                    args: vec!["-n".to_string(), "6".to_string(), "127.0.0.1".to_string()],
                    workdir: std::env::temp_dir(),
                    timeout: Duration::from_millis(300),
                    redact: None,
                }
            }
        };
        // Resolve like production does so PATH lookup is also covered.
        let resolved = if std::path::Path::new(&slow.program).is_absolute() {
            slow.program.clone()
        } else {
            resolve_program(&slow.program, &system_path_dirs())
                .expect("slow fixture present")
                .display()
                .to_string()
        };
        let mut slow = slow;
        slow.program = resolved;
        let result = run_command(&slow, None);
        assert_eq!(result.status, CommandStatus::Timeout, "got: {result:?}");
        assert!(result.detail.contains("timeout"));
    }

    #[test]
    fn run_cancel_before_start_never_spawns() {
        let result = run_command(&test_request("cargo", &["--version"]), Some(&|| true));
        assert_eq!(result.status, CommandStatus::Cancelled);
        assert!(result.detail.contains("before start"));
        assert!(result.stdout.is_empty() && result.stderr.is_empty());
    }
}
