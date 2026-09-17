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
/// Environment variable overriding `[model] name`.
pub const ENV_MODEL: &str = "AUREL_MODEL";
/// Environment variable overriding `[model] base_url`.
pub const ENV_BASE_URL: &str = "AUREL_BASE_URL";
/// Environment variable providing the model API key. Never logged, never
/// shown: [`render_show`] prints `<redacted>` instead of the value.
pub const ENV_API_KEY: &str = "AUREL_API_KEY";
/// Environment variable overriding `[model] timeout_secs`.
pub const ENV_TIMEOUT_SECS: &str = "AUREL_TIMEOUT_SECS";
/// Environment variable overriding `[model] max_retries`.
pub const ENV_MAX_RETRIES: &str = "AUREL_MAX_RETRIES";
/// Environment variable overriding `[model] streaming` (`true`/`false`).
pub const ENV_STREAMING: &str = "AUREL_STREAMING";
/// Environment variable overriding `[agent] max_iterations`.
pub const ENV_MAX_ITERATIONS: &str = "AUREL_MAX_ITERATIONS";

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

/// Strict boolean spelling shared by env and CLI toggles.
pub fn parse_toggle(text: &str) -> Result<bool, ToggleParseError> {
    match text.to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ToggleParseError),
    }
}

/// Rejection of a non-`true`/`false` toggle spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToggleParseError;

impl fmt::Display for ToggleParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected true or false")
    }
}

impl std::error::Error for ToggleParseError {}

/// Model endpoint settings (`[model]`). Resolved through the same
/// defaults < file < env < CLI precedence as everything else.
///
/// The API key is optional (local servers often need none) and is treated
/// as a secret everywhere: redacted in [`render_show`], redacted in
/// [`fmt::Debug`], never part of any [`ConfigError`].
#[derive(Clone, PartialEq, Eq)]
pub struct ModelSettings {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub timeout_secs: u64,
    pub max_retries: u32,
    pub streaming: bool,
}

impl Default for ModelSettings {
    fn default() -> Self {
        ModelSettings {
            // Local-first placeholder: a common local-server root. The user
            // overrides it; nothing here names a commercial provider.
            name: "default".to_string(),
            base_url: "http://127.0.0.1:11434/v1".to_string(),
            api_key: None,
            timeout_secs: 60,
            max_retries: 1,
            streaming: true,
        }
    }
}

impl fmt::Debug for ModelSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelSettings")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout_secs", &self.timeout_secs)
            .field("max_retries", &self.max_retries)
            .field("streaming", &self.streaming)
            .finish()
    }
}

/// Partial `[model]` table as read from a single TOML file. Every field is
/// optional; unknown fields are rejected.

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelFileConfig {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
    #[serde(default)]
    max_retries: Option<u32>,
    #[serde(default)]
    streaming: Option<bool>,
}

/// Agent run settings (`[agent]`). Only the iteration bound exists at this
/// stage; tool permissions arrive with tools (Phase 4+).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSettings {
    pub max_iterations: u32,
}

impl Default for AgentSettings {
    fn default() -> Self {
        AgentSettings { max_iterations: 5 }
    }
}

/// Partial `[agent]` table as read from a single TOML file. Every field is
/// optional; unknown fields are rejected.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentFileConfig {
    #[serde(default)]
    max_iterations: Option<u32>,
}

/// Partial configuration as read from a single TOML file.
///
/// Every field is optional; a file may set any subset. Unknown fields are
/// rejected so typos fail loudly instead of being silently ignored.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    log_level: Option<LogLevel>,
    #[serde(default)]
    model: Option<ModelFileConfig>,
    #[serde(default)]
    agent: Option<AgentFileConfig>,
}

/// Fully resolved configuration plus where each file layer came from.
/// `Debug` is safe to print: [`ModelSettings`] redacts the API key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveConfig {
    pub log_level: LogLevel,
    pub model: ModelSettings,
    pub agent: AgentSettings,
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
///
/// `Debug` is safe to print: the one secret-bearing variable
/// (`AUREL_API_KEY`) redacts itself.
#[derive(Default)]
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
    /// `--model` override. `None` means not given.
    pub cli_model_name: Option<String>,
    /// `--base-url` override. `None` means not given.
    pub cli_base_url: Option<String>,
    /// `--streaming` override. `None` means not given.
    pub cli_streaming: Option<bool>,
    /// `--max-iterations` override. `None` means not given.
    pub cli_max_iterations: Option<u32>,
}

impl fmt::Debug for LoadRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env: HashMap<&String, &str> = self
            .env
            .iter()
            .map(|(k, v)| {
                let shown = if k == ENV_API_KEY { "<redacted>" } else { v };
                (k, shown)
            })
            .collect();
        f.debug_struct("LoadRequest")
            .field("global_file", &self.global_file)
            .field("project_file", &self.project_file)
            .field("project_searched", &self.project_searched)
            .field("explicit_file", &self.explicit_file)
            .field("env", &env)
            .field("cli_log_level", &self.cli_log_level)
            .field("cli_model_name", &self.cli_model_name)
            .field("cli_base_url", &self.cli_base_url)
            .field("cli_streaming", &self.cli_streaming)
            .field("cli_max_iterations", &self.cli_max_iterations)
            .finish()
    }
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
    let mut model = ModelSettings::default();
    let mut agent = AgentSettings::default();
    let mut global_found = false;
    let mut project_found = false;

    if let Some(path) = &req.global_file {
        if let Some(file) = read_optional_file(path)? {
            apply_file(&file, &mut log_level, &mut model, &mut agent);
            global_found = true;
        }
    }

    if let Some(path) = &req.project_file {
        if let Some(file) = read_optional_file(path)? {
            apply_file(&file, &mut log_level, &mut model, &mut agent);
            project_found = true;
        }
    }

    if let Some(path) = &req.explicit_file {
        let file = read_required_file(path)?;
        apply_file(&file, &mut log_level, &mut model, &mut agent);
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
    apply_model_env(&req.env, &mut model)?;
    apply_agent_env(&req.env, &mut agent)?;

    if let Some(level) = req.cli_log_level {
        log_level = level;
    }
    if let Some(name) = &req.cli_model_name {
        model.name = name.clone();
    }
    if let Some(url) = &req.cli_base_url {
        model.base_url = url.clone();
    }
    if let Some(streaming) = req.cli_streaming {
        model.streaming = streaming;
    }
    if let Some(max) = req.cli_max_iterations {
        agent.max_iterations = max;
    }

    Ok(EffectiveConfig {
        log_level,
        model,
        agent,
        global_file: req.global_file.clone(),
        global_found,
        project_file: req.project_file.clone(),
        project_found,
        project_searched: req.project_searched,
        explicit_file: req.explicit_file.clone(),
    })
}

/// Overlay one file's values onto the running resolution.
fn apply_file(
    file: &FileConfig,
    log_level: &mut LogLevel,
    model: &mut ModelSettings,
    agent: &mut AgentSettings,
) {
    if let Some(level) = file.log_level {
        *log_level = level;
    }
    if let Some(table) = &file.model {
        if let Some(name) = &table.name {
            model.name = name.clone();
        }
        if let Some(url) = &table.base_url {
            model.base_url = url.clone();
        }
        if let Some(key) = &table.api_key {
            model.api_key = Some(key.clone());
        }
        if let Some(timeout) = table.timeout_secs {
            model.timeout_secs = timeout;
        }
        if let Some(retries) = table.max_retries {
            model.max_retries = retries;
        }
        if let Some(streaming) = table.streaming {
            model.streaming = streaming;
        }
    }
    if let Some(table) = &file.agent {
        if let Some(max) = table.max_iterations {
            agent.max_iterations = max;
        }
    }
}

/// Overlay `AUREL_*` model variables. The API key is a free-form string and
/// therefore never fails validation — and so never appears in an error.
fn apply_model_env(
    env: &HashMap<String, String>,
    model: &mut ModelSettings,
) -> Result<(), ConfigError> {
    if let Some(value) = env.get(ENV_MODEL) {
        model.name = value.clone();
    }
    if let Some(value) = env.get(ENV_BASE_URL) {
        model.base_url = value.clone();
    }
    if let Some(value) = env.get(ENV_API_KEY) {
        model.api_key = Some(value.clone());
    }
    if let Some(value) = env.get(ENV_TIMEOUT_SECS) {
        model.timeout_secs = value.parse().map_err(|_| ConfigError::InvalidEnv {
            var: ENV_TIMEOUT_SECS.to_string(),
            value: value.clone(),
            message: "expected seconds as an unsigned integer".to_string(),
        })?;
    }
    if let Some(value) = env.get(ENV_MAX_RETRIES) {
        model.max_retries = value.parse().map_err(|_| ConfigError::InvalidEnv {
            var: ENV_MAX_RETRIES.to_string(),
            value: value.clone(),
            message: "expected a retry count as an unsigned integer".to_string(),
        })?;
    }
    if let Some(value) = env.get(ENV_STREAMING) {
        model.streaming = parse_toggle(value).map_err(|e| ConfigError::InvalidEnv {
            var: ENV_STREAMING.to_string(),
            value: value.clone(),
            message: e.to_string(),
        })?;
    }
    Ok(())
}

/// Overlay the `AUREL_MAX_ITERATIONS` variable. Range checks belong to the
/// agent loop, which reports them with run context; loading stays total.
fn apply_agent_env(
    env: &HashMap<String, String>,
    agent: &mut AgentSettings,
) -> Result<(), ConfigError> {
    if let Some(value) = env.get(ENV_MAX_ITERATIONS) {
        agent.max_iterations = value.parse().map_err(|_| ConfigError::InvalidEnv {
            var: ENV_MAX_ITERATIONS.to_string(),
            value: value.clone(),
            message: "expected an iteration count as an unsigned integer".to_string(),
        })?;
    }
    Ok(())
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
/// The API key renders as `<redacted>` when set and `<unset>` otherwise —
/// the real value is never printed. Free-form strings are TOML-escaped by
/// [`toml_string`] (user input can contain quotes); fixed-domain fields
/// render directly.
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
    out.push_str("\n[model]\n");
    out.push_str(&format!("name = {}\n", toml_string(&cfg.model.name)));
    out.push_str(&format!(
        "base_url = {}\n",
        toml_string(&cfg.model.base_url)
    ));
    out.push_str(&format!(
        "api_key = {}\n",
        if cfg.model.api_key.is_some() {
            "\"<redacted>\"".to_string()
        } else {
            "\"<unset>\"".to_string()
        }
    ));
    out.push_str(&format!("timeout_secs = {}\n", cfg.model.timeout_secs));
    out.push_str(&format!("max_retries = {}\n", cfg.model.max_retries));
    out.push_str(&format!("streaming = {}\n", cfg.model.streaming));
    out.push_str("\n[agent]\n");
    out.push_str(&format!("max_iterations = {}\n", cfg.agent.max_iterations));
    out
}

/// Quote a free-form string as a TOML basic string. Display-only, but exact:
/// every character that needs escaping is escaped.
fn toml_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
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
            model: ModelSettings::default(),
            agent: AgentSettings::default(),
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
            model: ModelSettings::default(),
            agent: AgentSettings::default(),
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

    #[test]
    fn model_defaults_apply() {
        let cfg = load(&request()).expect("load defaults");
        assert_eq!(cfg.model.name, "default");
        assert_eq!(cfg.model.base_url, "http://127.0.0.1:11434/v1");
        assert_eq!(cfg.model.api_key, None);
        assert_eq!(cfg.model.timeout_secs, 60);
        assert_eq!(cfg.model.max_retries, 1);
        assert!(cfg.model.streaming);
        assert_eq!(cfg.agent.max_iterations, 5);
    }

    #[test]
    fn agent_precedence_is_file_env_cli() {
        let dir = test_dir("agent-precedence");
        let project = dir.join("project.toml");
        write(&project, "[agent]\nmax_iterations = 2\n");
        let mut req = request();
        req.project_file = Some(project);
        req.project_searched = true;

        assert_eq!(load(&req).expect("file").agent.max_iterations, 2);

        req.env.insert(ENV_MAX_ITERATIONS.into(), "7".into());
        assert_eq!(load(&req).expect("env").agent.max_iterations, 7);

        req.cli_max_iterations = Some(3);
        assert_eq!(load(&req).expect("cli").agent.max_iterations, 3);
    }

    #[test]
    fn invalid_max_iterations_env_names_its_variable() {
        let mut req = request();
        req.env.insert(ENV_MAX_ITERATIONS.into(), "many".into());
        let err = load(&req).expect_err("must fail");
        assert!(matches!(err, ConfigError::InvalidEnv { .. }), "{err:?}");
        assert!(err.to_string().contains(ENV_MAX_ITERATIONS), "{err:?}");
    }

    #[test]
    fn unknown_agent_field_is_rejected() {
        let dir = test_dir("agent-unknown-field");
        let path = dir.join("typo.toml");
        write(&path, "[agent]\nmax_iters = 2\n");
        let mut req = request();
        req.project_file = Some(path);
        assert!(
            matches!(load(&req), Err(ConfigError::MalformedFile { .. })),
            "typo'd agent keys must not be silently ignored"
        );
    }

    #[test]
    fn show_reports_agent_settings() {
        let text = render_show(&load(&request()).expect("defaults"));
        assert!(text.contains("[agent]"), "got: {text:?}");
        assert!(text.contains("max_iterations = 5"), "got: {text:?}");
    }

    #[test]
    fn model_precedence_is_file_env_cli() {
        let dir = test_dir("model-precedence");
        let project = dir.join("project.toml");
        write(
            &project,
            "[model]\nname = \"file-model\"\nbase_url = \"http://file:1\"\n\
             timeout_secs = 10\nmax_retries = 0\nstreaming = false\n\
             api_key = \"file-key\"\n",
        );
        let mut req = request();
        req.project_file = Some(project);
        req.project_searched = true;

        let cfg = load(&req).expect("file");
        assert_eq!(cfg.model.name, "file-model");
        assert_eq!(cfg.model.api_key, Some("file-key".to_string()));
        assert!(!cfg.model.streaming);

        req.env.insert(ENV_MODEL.into(), "env-model".into());
        req.env.insert(ENV_API_KEY.into(), "env-key".into());
        req.env.insert(ENV_STREAMING.into(), "true".into());
        let cfg = load(&req).expect("env");
        assert_eq!(cfg.model.name, "env-model");
        assert_eq!(cfg.model.api_key, Some("env-key".to_string()));
        assert!(cfg.model.streaming);

        req.cli_model_name = Some("cli-model".into());
        req.cli_base_url = Some("http://cli:2".into());
        req.cli_streaming = Some(false);
        let cfg = load(&req).expect("cli");
        assert_eq!(cfg.model.name, "cli-model");
        assert_eq!(cfg.model.base_url, "http://cli:2");
        assert!(!cfg.model.streaming);
        // CLI has no key flag (history-safe): the env key survives.
        assert_eq!(cfg.model.api_key, Some("env-key".to_string()));
    }

    #[test]
    fn invalid_model_env_values_name_their_variable() {
        for (var, value) in [
            (ENV_TIMEOUT_SECS, "soon"),
            (ENV_MAX_RETRIES, "many"),
            (ENV_STREAMING, "maybe"),
        ] {
            let mut req = request();
            req.env.insert(var.into(), value.into());
            let err = load(&req).expect_err("must fail");
            assert!(matches!(err, ConfigError::InvalidEnv { .. }), "{err:?}");
            assert!(err.to_string().contains(var), "{err:?}");
        }
    }

    #[test]
    fn unknown_model_field_is_rejected() {
        let dir = test_dir("model-unknown-field");
        let path = dir.join("typo.toml");
        write(&path, "[model]\nmodel_name = \"x\"\n");
        let mut req = request();
        req.project_file = Some(path);
        assert!(
            matches!(load(&req), Err(ConfigError::MalformedFile { .. })),
            "typo'd model keys must not be silently ignored"
        );
    }

    #[test]
    fn show_redacts_the_api_key() {
        let dir = test_dir("redaction");
        let path = dir.join("secret.toml");
        write(&path, "[model]\napi_key = \"sk-live-EXAMPLE-12345\"\n");
        let mut req = request();
        req.project_file = Some(path);
        let cfg = load(&req).expect("load");
        let text = render_show(&cfg);
        assert!(text.contains("api_key = \"<redacted>\""), "got: {text:?}");
        assert!(
            !text.contains("sk-live-EXAMPLE-12345"),
            "key leaked: {text:?}"
        );

        let plain = load(&request()).expect("defaults");
        let text = render_show(&plain);
        assert!(text.contains("api_key = \"<unset>\""), "got: {text:?}");
    }

    #[test]
    fn debug_formatting_never_shows_the_key() {
        let mut req = request();
        req.env
            .insert(ENV_API_KEY.into(), "sk-live-EXAMPLE-999".into());
        let cfg = load(&req).expect("load");
        for shown in [
            format!("{:?}", cfg),
            format!("{:?}", cfg.model),
            format!("{:?}", req),
        ] {
            assert!(!shown.contains("sk-live-EXAMPLE-999"), "leak: {shown:?}");
            assert!(shown.contains("<redacted>"), "missing marker: {shown:?}");
        }
    }

    #[test]
    fn toggle_parsing_is_strict_but_case_insensitive() {
        assert_eq!(parse_toggle("true"), Ok(true));
        assert_eq!(parse_toggle("FALSE"), Ok(false));
        assert!(parse_toggle("yes").is_err());
        assert!(parse_toggle("1").is_err());
    }

    #[test]
    fn show_escapes_freeform_values() {
        let mut cfg = load(&request()).expect("defaults");
        cfg.model.name = "weird \" \\ name".to_string();
        let text = render_show(&cfg);
        assert!(
            text.contains("name = \"weird \\\" \\\\ name\""),
            "got: {text:?}"
        );
        // Round-trips as valid TOML.
        let reparsed: toml::Value = text
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .parse()
            .expect("show output must stay valid TOML");
        assert_eq!(reparsed["model"]["name"].as_str(), Some("weird \" \\ name"));
    }
}
