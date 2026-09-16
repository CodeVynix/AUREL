//! `aurel-config`: TOML configuration loading with deterministic precedence.
//!
//! Precedence, lowest to highest:
//!
//! ```text
//! built-in defaults
//!     ↓
//! global config file   (%APPDATA%\aurel\config.toml on Windows,
//!                        ~/.config/aurel/config.toml elsewhere)
//!     ↓
//! project config file  (nearest `.aurel/config.toml` at or above the
//!                        working directory)
//!     ↓
//! environment variables (`AUREL_*`)
//!     ↓
//! CLI arguments
//! ```
//!
//! Passing `--config <path>` replaces file discovery: only that file is
//! read (plus environment and CLI above it). Missing files are skipped
//! silently, except an explicit `--config` path, which must exist.
//!
//! The design keeps every input injectable ([`LoadRequest`], [`discover`]
//! helpers take plain values) so precedence is unit-testable without
//! touching the real home directory or process environment.

use std::collections::HashMap;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::{Deserialize, Deserializer};

/// Environment variable overriding `log_level`.
pub const ENV_LOG_LEVEL: &str = "AUREL_LOG_LEVEL";

/// Log verbosity. Spelled lowercase in every source; parsing is
/// case-insensitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    /// Canonical lowercase spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        }
    }

    fn parse_value(text: &str) -> Result<Self, LogLevelParseError> {
        match text.to_ascii_lowercase().as_str() {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            _ => Err(LogLevelParseError),
        }
    }
}

impl fmt::Display for LogLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Rejection of an unknown log-level spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLevelParseError;

impl fmt::Display for LogLevelParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected one of: error, warn, info, debug, trace")
    }
}

impl std::error::Error for LogLevelParseError {}

impl FromStr for LogLevel {
    type Err = LogLevelParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        LogLevel::parse_value(s)
    }
}

impl<'de> Deserialize<'de> for LogLevel {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Partial configuration as read from a single TOML file.
///
/// Every field is optional; a file may set any subset. Unknown fields are
/// rejected so typos fail loudly instead of being silently ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    log_level: Option<LogLevel>,
}

/// Fully resolved configuration plus where each file layer came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub log_level: LogLevel,
    /// Discovered global file path, if the home location was resolvable.
    pub global_file: Option<PathBuf>,
    /// Whether the global file existed and was applied.
    pub global_found: bool,
    /// Discovered project file path, if the search ran and found one.
    /// `None` together with `project_searched == true` means the search ran
    /// and found nothing; with `false` it means no search ran (`--config`).
    pub project_file: Option<PathBuf>,
    /// Whether the project file existed and was applied.
    pub project_found: bool,
    /// Whether project discovery ran at all. Distinguishes "not searched"
    /// (`--config` given) from "searched, none found" in diagnostics.
    pub project_searched: bool,
    /// Explicit `--config` path, if given. A missing explicit file is an
    /// error, so when loading succeeds this file was applied.
    pub explicit_file: Option<PathBuf>,
}

/// Everything [`load`] needs, with environment and filesystem inputs passed
/// as plain values so tests can inject fakes.
#[derive(Debug, Default)]
pub struct LoadRequest {
    /// Global file to read, if any. `None` skips the layer.
    pub global_file: Option<PathBuf>,
    /// Project file to read, if discovery found one. `None` skips the layer;
    /// set `project_searched` to record whether discovery ran.
    pub project_file: Option<PathBuf>,
    /// Whether project discovery ran. `false` (e.g. `--config` replaces
    /// discovery) renders as "not searched"; `true` with no `project_file`
    /// renders as "searched, none found".
    pub project_searched: bool,
    /// Explicit `--config` file. Must exist when set.
    pub explicit_file: Option<PathBuf>,
    /// Injected environment (production passes the filtered process env).
    pub env: HashMap<String, String>,
    /// `--log-level` override. `None` means not given.
    pub cli_log_level: Option<LogLevel>,
}

/// Configuration failure. Every variant carries the context needed for a
/// clean user-facing message; see [`fmt::Display`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// A discovered file exists but cannot be read (permissions, etc.).
    /// A merely absent file is skipped, not an error.
    UnreadableFile { path: PathBuf, message: String },
    /// The explicit `--config` path does not exist.
    ExplicitFileMissing { path: PathBuf },
    /// A file is not valid TOML, has a wrong type, or sets unknown fields.
    MalformedFile { path: PathBuf, message: String },
    /// An `AUREL_*` variable has an invalid value.
    InvalidEnv {
        var: String,
        value: String,
        message: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::UnreadableFile { path, message } => write!(
                f,
                "error: cannot read config file '{}': {message}",
                path.display()
            ),
            ConfigError::ExplicitFileMissing { path } => write!(
                f,
                "error: config file not found: '{}' (from --config)",
                path.display()
            ),
            ConfigError::MalformedFile { path, message } => write!(
                f,
                "error: invalid TOML in config file '{}': {message}",
                path.display()
            ),
            ConfigError::InvalidEnv {
                var,
                value,
                message,
            } => {
                write!(f, "error: invalid value '{value}' for {var}: {message}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// Read an optional file layer: absent → `Ok(None)`, present → parsed.
fn read_optional_file(path: &Path) -> Result<Option<FileConfig>, ConfigError> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::UnreadableFile {
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
        Ok(text) => parse_file(path, &text).map(Some),
    }
}

/// Read a required (`--config`) file layer: absent is an error.
fn read_required_file(path: &Path) -> Result<FileConfig, ConfigError> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => Err(ConfigError::ExplicitFileMissing {
            path: path.to_path_buf(),
        }),
        Err(e) => Err(ConfigError::UnreadableFile {
            path: path.to_path_buf(),
            message: e.to_string(),
        }),
        Ok(text) => parse_file(path, &text),
    }
}

fn parse_file(path: &Path, text: &str) -> Result<FileConfig, ConfigError> {
    toml::from_str(text).map_err(|e| ConfigError::MalformedFile {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

/// Resolve `req` into an [`EffectiveConfig`] following the documented
/// precedence: defaults < global < project < explicit < env < CLI.
pub fn load(req: &LoadRequest) -> Result<EffectiveConfig, ConfigError> {
    let mut log_level = LogLevel::default();
    let mut global_found = false;
    let mut project_found = false;

    if let Some(path) = &req.global_file {
        if let Some(file) = read_optional_file(path)? {
            if let Some(level) = file.log_level {
                log_level = level;
            }
            global_found = true;
        }
    }

    if let Some(path) = &req.project_file {
        if let Some(file) = read_optional_file(path)? {
            if let Some(level) = file.log_level {
                log_level = level;
            }
            project_found = true;
        }
    }

    if let Some(path) = &req.explicit_file {
        let file = read_required_file(path)?;
        if let Some(level) = file.log_level {
            log_level = level;
        }
    }

    if let Some(value) = req.env.get(ENV_LOG_LEVEL) {
        log_level = value
            .parse()
            .map_err(|e: LogLevelParseError| ConfigError::InvalidEnv {
                var: ENV_LOG_LEVEL.to_string(),
                value: value.clone(),
                message: e.to_string(),
            })?;
    }

    if let Some(level) = req.cli_log_level {
        log_level = level;
    }

    Ok(EffectiveConfig {
        log_level,
        global_file: req.global_file.clone(),
        global_found,
        project_file: req.project_file.clone(),
        project_found,
        project_searched: req.project_searched,
        explicit_file: req.explicit_file.clone(),
    })
}

/// Environment conversion policy: collect the `AUREL_*` subset of raw OS
/// environment entries without panicking on non-Unicode data.
///
/// - Keys must decode as Unicode to participate; a non-Unicode key is
///   ignored. This is safe because every recognized name is pure ASCII: an
///   undecodable key cannot name a real setting.
/// - Values use lossy conversion (`to_string_lossy`): undecodable sequences
///   become U+FFFD and flow into normal validation, so a bad value is a
///   deterministic [`ConfigError::InvalidEnv`], never a panic.
///
/// Takes the raw iterator (production passes `std::env::vars_os()`) so the
/// policy is unit-testable with platform-constructed non-Unicode values.
pub fn collect_aurel_env(
    vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
) -> HashMap<String, String> {
    vars.filter_map(|(key, value)| {
        let key = key.into_string().ok()?;
        if key == ENV_LOG_LEVEL || key.starts_with("AUREL_") {
            Some((key, value.to_string_lossy().into_owned()))
        } else {
            None
        }
    })
    .collect()
}

/// Resolve the global config path from already-read directory values.
///
/// `appdata` is `%APPDATA%` (used on Windows), `home` is `$HOME` (used
/// elsewhere). Returns `None` when the relevant value is absent or empty.
/// Taking plain values keeps this unit-testable without process env access.
pub fn global_config_file(appdata: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let _ = home;
        appdata
            .filter(|s| !s.is_empty())
            .map(|dir| Path::new(dir).join("aurel").join("config.toml"))
    }
    #[cfg(not(windows))]
    {
        let _ = appdata;
        home.filter(|s| !s.is_empty()).map(|dir| {
            Path::new(dir)
                .join(".config")
                .join("aurel")
                .join("config.toml")
        })
    }
}

/// Search `start` and its ancestors for `.aurel/config.toml`, nearest first.
///
/// The walk is bounded (64 levels) and stops at the filesystem root.
/// Returns `None` when no ancestor holds the file.
pub fn discover_project_file(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    for _ in 0..64 {
        let candidate = dir.join(".aurel").join("config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
    None
}

/// Render the effective configuration for `aurel config show`: `#` comment
/// lines describing each file layer, then the settings as TOML.
///
/// The value domain of every rendered field is a fixed lowercase word list,
/// so the manual rendering cannot produce invalid quoting.
pub fn render_show(cfg: &EffectiveConfig) -> String {
    let mut out = String::from("# aurel effective configuration (TOML)\n");
    out.push_str(&show_source_line(
        "global",
        &cfg.global_file,
        cfg.global_found,
    ));
    out.push_str(&show_project_line(cfg));
    if let Some(path) = &cfg.explicit_file {
        out.push_str(&format!("# explicit: {}\n", path.display()));
    }
    out.push_str(&format!("log_level = \"{}\"\n", cfg.log_level.as_str()));
    out
}

fn show_source_line(kind: &str, path: &Option<PathBuf>, found: bool) -> String {
    match path {
        None => format!("# {kind}: not searched\n"),
        Some(p) if found => format!("# {kind}: {}\n", p.display()),
        Some(p) => format!("# {kind}: {} (not found)\n", p.display()),
    }
}

/// Project layer diagnostic with three honest states: the discovered path
/// when found, "searched, none found" when discovery ran empty, and
/// "not searched" when discovery was replaced (explicit `--config`).
fn show_project_line(cfg: &EffectiveConfig) -> String {
    match (&cfg.project_file, cfg.project_found, cfg.project_searched) {
        (Some(path), true, _) => format!("# project: {}\n", path.display()),
        (Some(path), false, _) => format!("# project: {} (not found)\n", path.display()),
        (None, _, true) => "# project: searched, none found\n".to_string(),
        (None, _, false) => "# project: not searched\n".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-config-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, text).expect("write test config");
    }

    fn request() -> LoadRequest {
        LoadRequest::default()
    }

    #[test]
    fn defaults_apply_when_nothing_is_set() {
        let cfg = load(&request()).expect("load defaults");
        assert_eq!(cfg.log_level, LogLevel::Info);
        assert!(!cfg.global_found && !cfg.project_found);
    }

    #[test]
    fn precedence_is_global_project_env_cli() {
        let dir = test_dir("precedence");
        let global = dir.join("global.toml");
        let project = dir.join("project.toml");
        write(&global, "log_level = \"error\"\n");
        write(&project, "log_level = \"warn\"\n");

        // Global alone.
        let mut req = request();
        req.global_file = Some(global.clone());
        assert_eq!(load(&req).expect("global").log_level, LogLevel::Error);

        // Project beats global.
        req.project_file = Some(project.clone());
        assert_eq!(load(&req).expect("project").log_level, LogLevel::Warn);

        // Env beats files.
        req.env.insert(ENV_LOG_LEVEL.into(), "debug".into());
        assert_eq!(load(&req).expect("env").log_level, LogLevel::Debug);

        // CLI beats env.
        req.cli_log_level = Some(LogLevel::Trace);
        assert_eq!(load(&req).expect("cli").log_level, LogLevel::Trace);
    }

    #[test]
    fn explicit_file_is_applied_and_reports_itself() {
        let dir = test_dir("explicit");
        let path = dir.join("custom.toml");
        write(&path, "log_level = \"debug\"\n");
        let mut req = request();
        req.explicit_file = Some(path.clone());
        let cfg = load(&req).expect("explicit");
        assert_eq!(cfg.log_level, LogLevel::Debug);
        assert_eq!(cfg.explicit_file, Some(path));
    }

    #[test]
    fn missing_explicit_file_is_an_error() {
        let mut req = request();
        req.explicit_file = Some(PathBuf::from("does-not-exist-aurel-test.toml"));
        let err = load(&req).expect_err("must fail");
        assert!(matches!(err, ConfigError::ExplicitFileMissing { .. }));
        assert!(err.to_string().contains("--config"));
    }

    #[test]
    fn absent_optional_files_are_skipped() {
        let mut req = request();
        req.global_file = Some(PathBuf::from("nope-global-aurel-test.toml"));
        req.project_file = Some(PathBuf::from("nope-project-aurel-test.toml"));
        let cfg = load(&req).expect("absent files are fine");
        assert_eq!(cfg.log_level, LogLevel::Info);
        assert!(!cfg.global_found && !cfg.project_found);
    }

    #[test]
    fn malformed_toml_reports_path() {
        let dir = test_dir("malformed");
        let path = dir.join("bad.toml");
        write(&path, "log_level = [unclosed\n");
        let mut req = request();
        req.project_file = Some(path.clone());
        let err = load(&req).expect_err("must fail");
        assert!(matches!(err, ConfigError::MalformedFile { .. }));
        assert!(err.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn unknown_field_is_rejected() {
        let dir = test_dir("unknown-field");
        let path = dir.join("typo.toml");
        write(&path, "log_levle = \"debug\"\n");
        let mut req = request();
        req.project_file = Some(path);
        assert!(
            matches!(load(&req), Err(ConfigError::MalformedFile { .. })),
            "typo'd keys must not be silently ignored"
        );
    }

    #[test]
    fn invalid_file_value_reports_path() {
        let dir = test_dir("invalid-value");
        let path = dir.join("bad-value.toml");
        write(&path, "log_level = \"verbose\"\n");
        let mut req = request();
        req.project_file = Some(path);
        let err = load(&req).expect_err("must fail");
        assert!(err.to_string().contains("error, warn, info, debug, trace"));
    }

    #[test]
    fn invalid_env_value_is_an_error() {
        let mut req = request();
        req.env.insert(ENV_LOG_LEVEL.into(), "chatty".into());
        let err = load(&req).expect_err("must fail");
        assert!(matches!(err, ConfigError::InvalidEnv { .. }));
        assert!(err.to_string().contains("AUREL_LOG_LEVEL"));
    }

    #[test]
    fn log_level_parsing_is_case_insensitive() {
        assert_eq!("DEBUG".parse(), Ok(LogLevel::Debug));
        assert_eq!("Warn".parse(), Ok(LogLevel::Warn));
        assert!("loud".parse::<LogLevel>().is_err());
    }

    #[test]
    fn global_path_resolution_prefers_platform_convention() {
        let resolved = global_config_file(Some("C:\\Users\\t\\AppData\\Roaming"), Some("/home/t"));
        let resolved = resolved.expect("resolvable");
        #[cfg(windows)]
        assert_eq!(
            resolved,
            PathBuf::from("C:\\Users\\t\\AppData\\Roaming")
                .join("aurel")
                .join("config.toml")
        );
        #[cfg(not(windows))]
        assert_eq!(
            resolved,
            PathBuf::from("/home/t")
                .join(".config")
                .join("aurel")
                .join("config.toml")
        );
        assert!(global_config_file(Some(""), Some("")).is_none());
        assert!(global_config_file(None, None).is_none());
    }

    #[test]
    fn project_discovery_prefers_nearest_and_stops() {
        let dir = test_dir("discovery");
        let outer = dir.join(".aurel").join("config.toml");
        let inner_dir = dir.join("a").join("b");
        let inner = dir.join("a").join(".aurel").join("config.toml");
        write(&outer, "log_level = \"error\"\n");
        write(&inner, "log_level = \"debug\"\n");
        std::fs::create_dir_all(&inner_dir).expect("inner dir");

        assert_eq!(discover_project_file(&inner_dir), Some(inner));
        assert_eq!(discover_project_file(&dir), Some(outer));
        assert_eq!(discover_project_file(&test_dir("discovery-empty")), None);
    }

    #[test]
    fn render_show_contains_settings_and_sources() {
        let cfg = EffectiveConfig {
            log_level: LogLevel::Debug,
            global_file: Some(PathBuf::from("/g/config.toml")),
            global_found: false,
            project_file: Some(PathBuf::from("/p/.aurel/config.toml")),
            project_found: true,
            project_searched: true,
            explicit_file: None,
        };
        let text = render_show(&cfg);
        assert!(text.contains("log_level = \"debug\""));
        assert!(text.contains("# global: /g/config.toml (not found)"));
        assert!(text.contains("# project: /p/.aurel/config.toml"));
    }

    #[test]
    fn render_show_distinguishes_project_search_states() {
        // Searched and empty: must not claim "not searched".
        let searched_empty = EffectiveConfig {
            log_level: LogLevel::Info,
            global_file: Some(PathBuf::from("/g/config.toml")),
            global_found: false,
            project_file: None,
            project_found: false,
            project_searched: true,
            explicit_file: None,
        };
        let text = render_show(&searched_empty);
        assert!(
            text.contains("# project: searched, none found"),
            "got: {text:?}"
        );
        assert!(!text.contains("not searched"), "got: {text:?}");

        // Discovery replaced by --config: honestly "not searched".
        let not_searched = EffectiveConfig {
            project_searched: false,
            explicit_file: Some(PathBuf::from("/c/custom.toml")),
            ..searched_empty.clone()
        };
        let text = render_show(&not_searched);
        assert!(text.contains("# project: not searched"), "got: {text:?}");
        assert!(text.contains("# explicit: /c/custom.toml"), "got: {text:?}");
    }

    /// Non-Unicode OS string for the current platform.
    #[cfg(unix)]
    fn non_unicode_os(text: &[u8]) -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(text.to_vec())
    }

    /// Non-Unicode OS string for the current platform (an unpaired
    /// surrogate, which is not valid Unicode).
    #[cfg(windows)]
    fn non_unicode_os(_text: &[u8]) -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xD800, b'x' as u16])
    }

    #[cfg(not(any(unix, windows)))]
    fn non_unicode_os(_text: &[u8]) -> std::ffi::OsString {
        std::ffi::OsString::from("--wat")
    }

    #[test]
    fn env_collection_never_panics_on_non_unicode_data() {
        // Regression test: the vars_os -> filter/lossy path must be total.
        let vars = vec![
            (
                std::ffi::OsString::from("AUREL_LOG_LEVEL"),
                non_unicode_os(b"\xff"),
            ),
            (non_unicode_os(b"\xff"), std::ffi::OsString::from("debug")),
            (
                std::ffi::OsString::from("UNRELATED"),
                non_unicode_os(b"\xff"),
            ),
            (
                std::ffi::OsString::from("AUREL_LOG_LEVEL_OK"),
                std::ffi::OsString::from("warn"),
            ),
        ];
        // Reaching the assertions proves no panic occurred.
        let env = collect_aurel_env(vars.into_iter());
        // Non-Unicode key: ignored, never surfaces under a decoded name.
        assert!(!env.keys().any(|k| k.contains('\u{FFFD}')));
        // Unrelated names (even non-Unicode values) are not collected.
        assert!(!env.contains_key("UNRELATED"));
        assert_eq!(env.get("AUREL_LOG_LEVEL_OK"), Some(&"warn".to_string()));
        #[cfg(any(unix, windows))]
        {
            // Lossy value is present and deterministically invalid, so
            // loading rejects it with InvalidEnv instead of panicking.
            let value = env.get(ENV_LOG_LEVEL).expect("lossy value collected");
            assert!(value.contains('\u{FFFD}'), "got: {value:?}");
            let req = LoadRequest {
                env,
                ..LoadRequest::default()
            };
            assert!(matches!(load(&req), Err(ConfigError::InvalidEnv { .. })));
        }
    }
}
