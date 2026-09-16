//! Phase 1 argument grammar (dependency-free, total, deterministic).
//!
//! ```text
//! aurel [GLOBAL FLAGS] [COMMAND] [COMMAND ARGS]
//! ```
//!
//! Global flags (also accepted as `--flag=value`):
//!
//! - `-h`, `--help` — print help and exit 0. Never touches config files.
//! - `-V`, `--version` — print the version and exit 0. Never touches config
//!   files. Combining it with a command is a usage error.
//! - `--config <path>` — replace file discovery with exactly this file.
//! - `--log-level <level>` — `error|warn|info|debug|trace` (case-insensitive).
//!
//! Commands:
//!
//! - `config show` — print the effective configuration and exit 0.
//! - `config --help`, or bare `config` — print command help and exit 0.
//!
//! Rules:
//!
//! - Parsing stops at the first error; every usage error exits with code 2.
//!   Usage errors take precedence over `--help`: help is printed only when
//!   the surrounding command line itself is valid.
//! - Only exact `-h` / `-V` are accepted: no combined short flags, no
//!   abbreviations, no `--` long-prefix matching.
//! - A `--` token ends flag parsing; the next token is a command or error.
//! - A bare invocation (no arguments) prints top-level help and exits 0,
//!   preserving Phase 0 behavior, without reading config files.

use std::fmt;
use std::path::PathBuf;

use aurel_config::LogLevel;

/// Which help text to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    Top,
    Config,
}

/// Parsed subcommand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    ConfigShow,
}

/// Successfully parsed command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parsed {
    pub version: bool,
    pub help: Option<HelpTopic>,
    pub log_level: Option<LogLevel>,
    pub config_path: Option<PathBuf>,
    pub command: Option<Command>,
}

/// Usage failure. Displayed to stderr; the caller exits with code 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    UnknownFlag(String),
    MissingValue { flag: &'static str },
    InvalidValue { flag: &'static str, value: String },
    UnknownCommand(String),
    UnexpectedCommandArgument { arg: String },
    VersionWithCommand,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnknownFlag(flag) => write!(f, "error: unknown flag '{flag}'"),
            ParseError::MissingValue { flag } => {
                write!(f, "error: flag '{flag}' requires a value")
            }
            ParseError::InvalidValue { flag, value } => write!(
                f,
                "error: invalid value '{value}' for '{flag}': expected one of: \
                 error, warn, info, debug, trace"
            ),
            ParseError::UnknownCommand(cmd) => {
                write!(f, "error: unknown command '{cmd}' (expected 'config')")
            }
            ParseError::UnexpectedCommandArgument { arg } => write!(
                f,
                "error: unexpected argument '{arg}' for 'config' (expected 'show')"
            ),
            ParseError::VersionWithCommand => {
                write!(f, "error: '--version' cannot be combined with a command")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Split `--name=value` into parts; a bare `--name` yields `(name, None)`.
fn split_long(token: &str) -> (&str, Option<&str>) {
    match token.find('=') {
        Some(i) => (&token[..i], Some(&token[i + 1..])),
        None => (token, None),
    }
}

/// Parse already-normalized (lossy `OsString`) arguments, excluding argv[0].
pub fn parse(args: &[String]) -> Result<Parsed, ParseError> {
    let mut out = Parsed::default();
    let mut iter = args.iter().peekable();
    let mut in_command = false;
    let mut command_seen = false;
    let mut flags_done = false;

    while let Some(token) = iter.next() {
        // Global-flag zone.
        if !in_command && !flags_done {
            if token == "--" {
                flags_done = true;
                continue;
            }
            if token.starts_with('-') {
                let (name, inline) = split_long(token);
                match name {
                    "-h" | "--help" if inline.is_none() => {
                        out.help = Some(HelpTopic::Top);
                        continue;
                    }
                    "-V" | "--version" if inline.is_none() => {
                        out.version = true;
                        continue;
                    }
                    "--config" => {
                        let value = match inline {
                            Some(v) => v.to_string(),
                            None => iter
                                .next()
                                .cloned()
                                .ok_or(ParseError::MissingValue { flag: "--config" })?,
                        };
                        out.config_path = Some(PathBuf::from(value));
                        continue;
                    }
                    "--log-level" => {
                        let value = match inline {
                            Some(v) => v.to_string(),
                            None => iter.next().cloned().ok_or(ParseError::MissingValue {
                                flag: "--log-level",
                            })?,
                        };
                        out.log_level =
                            Some(value.parse().map_err(|_| ParseError::InvalidValue {
                                flag: "--log-level",
                                value,
                            })?);
                        continue;
                    }
                    _ => return Err(ParseError::UnknownFlag(token.clone())),
                }
            }
            // First non-flag token: a command word (or an error).
            in_command = true;
        }

        // Command zone.
        if !command_seen {
            command_seen = true;
            match token.as_str() {
                "config" => continue,
                other => return Err(ParseError::UnknownCommand(other.to_string())),
            }
        } else {
            match token.as_str() {
                "show" => {
                    if out.command.is_some() {
                        return Err(ParseError::UnexpectedCommandArgument { arg: token.clone() });
                    }
                    out.command = Some(Command::ConfigShow);
                }
                "-h" | "--help" => {
                    out.help = Some(HelpTopic::Config);
                }
                _ => {
                    return Err(ParseError::UnexpectedCommandArgument { arg: token.clone() });
                }
            }
        }
    }

    if out.version && command_seen {
        return Err(ParseError::VersionWithCommand);
    }
    // Bare `config` (no subcommand, no explicit help) prints the command
    // help, consistent with `config --help`.
    if command_seen && out.command.is_none() && out.help.is_none() {
        out.help = Some(HelpTopic::Config);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_string()).collect()
    }

    #[test]
    fn empty_is_bare_help_request() {
        assert_eq!(parse(&[]), Ok(Parsed::default()));
    }

    #[test]
    fn help_and_version_flags() {
        assert_eq!(
            parse(&args(&["--help"])).expect("help").help,
            Some(HelpTopic::Top)
        );
        assert_eq!(parse(&args(&["-h"])).expect("h").help, Some(HelpTopic::Top));
        assert!(parse(&args(&["--version"])).expect("version").version);
        assert!(parse(&args(&["-V"])).expect("V").version);
    }

    #[test]
    fn config_show_parses() {
        let parsed = parse(&args(&["config", "show"])).expect("config show");
        assert_eq!(parsed.command, Some(Command::ConfigShow));
    }

    #[test]
    fn bare_config_requests_config_help() {
        let parsed = parse(&args(&["config"])).expect("bare config");
        assert_eq!(parsed.help, Some(HelpTopic::Config));
        assert_eq!(parsed.command, None);
    }

    #[test]
    fn usage_error_beats_help() {
        // The parser stops at the first error: an invalid command line exits
        // 2 even when --help is present.
        assert!(parse(&args(&["--help", "--wat"])).is_err());
        assert!(parse(&args(&["--wat", "--help"])).is_err());
    }

    #[test]
    fn config_help_selects_config_topic() {
        let parsed = parse(&args(&["config", "--help"])).expect("config help");
        assert_eq!(parsed.help, Some(HelpTopic::Config));
        assert_eq!(parsed.command, None);
    }

    #[test]
    fn global_flags_accept_space_and_equals_forms() {
        let parsed = parse(&args(&["--config", "a.toml", "config", "show"])).expect("space");
        assert_eq!(parsed.config_path, Some(PathBuf::from("a.toml")));
        let parsed = parse(&args(&["--config=b.toml", "--log-level=DEBUG"])).expect("equals");
        assert_eq!(parsed.config_path, Some(PathBuf::from("b.toml")));
        assert_eq!(parsed.log_level, Some(LogLevel::Debug));
    }

    #[test]
    fn invalid_inputs_are_usage_errors() {
        assert!(matches!(
            parse(&args(&["--wat"])),
            Err(ParseError::UnknownFlag(_))
        ));
        assert!(matches!(
            parse(&args(&["-hV"])),
            Err(ParseError::UnknownFlag(_))
        ));
        assert!(matches!(
            parse(&args(&["--config"])),
            Err(ParseError::MissingValue { .. })
        ));
        assert!(matches!(
            parse(&args(&["--log-level", "chatty"])),
            Err(ParseError::InvalidValue { .. })
        ));
        assert!(matches!(
            parse(&args(&["frobnicate"])),
            Err(ParseError::UnknownCommand(_))
        ));
        assert!(matches!(
            parse(&args(&["config", "bogus"])),
            Err(ParseError::UnexpectedCommandArgument { .. })
        ));
        assert!(matches!(
            parse(&args(&["config", "show", "extra"])),
            Err(ParseError::UnexpectedCommandArgument { .. })
        ));
        assert!(matches!(
            parse(&args(&["--version", "config", "show"])),
            Err(ParseError::VersionWithCommand)
        ));
    }

    #[test]
    fn dashdash_end_flags_then_command() {
        let parsed = parse(&args(&["--", "config", "show"])).expect("--");
        assert_eq!(parsed.command, Some(Command::ConfigShow));
    }
}
