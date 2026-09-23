//! `aurel-session`: persistent interactive sessions for AUREL.
//!
//! One JSON file per session under a store directory ([`SessionStore`]),
//! named `<id>.json` with a validated [`SessionId`]. Stored: conversation
//! history, Plan/Build mode, working directory, project marker, and loop
//! counters. Never stored: API keys or any configuration (no field
//! exists), `AGENTS.md` instructions (reloaded live), pending proposals,
//! or undo records — so resuming a session can never execute anything.
//!
//! All failure modes are typed ([`SessionError`]): invalid IDs, missing
//! sessions, ambiguous prefixes, corrupt content, incompatible versions,
//! oversized payloads, and I/O failures. Nothing here panics on stored
//! data; nothing logs secrets (there are none to log).
//!
//! Only `serde`/`serde_json` beyond `aurel-model` — both already in the
//! workspace graph, no new crates.io entries.

mod id;
mod store;

pub use id::SessionId;
pub use store::{
    describe_age, explore_context, now_ms, SessionData, SessionMeta, SessionMode, SessionStore,
    SessionSummary, MAX_EXPLORE_CHARS_PER_SESSION, MAX_EXPLORE_SESSIONS, MAX_FILE_BYTES,
    MAX_LISTED_SESSIONS, MAX_LOADED_MESSAGES, MAX_STORED_MESSAGES, SESSION_COUNT_WARN_THRESHOLD,
    SESSION_FORMAT,
};

/// What went wrong inside the session store. Every fallible operation
/// returns this: no panics on bad input, no collapsed generic failure.
/// Messages name files and IDs; file contents and secrets never appear.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// The ID or prefix is malformed (wrong shape, path separators,
    /// traversal attempts, NUL bytes).
    InvalidId(String),
    /// No session file matches.
    NotFound { id: String },
    /// A prefix matches several sessions; the caller must disambiguate.
    Ambiguous {
        prefix: String,
        candidates: Vec<String>,
    },
    /// The file exists but its content is unusable (bad JSON, ID/filename
    /// mismatch, unknown mode, backwards timestamps, overfull history).
    Corrupt { path: String, reason: String },
    /// The file uses an unknown format version — never guessed at.
    Incompatible { path: String, found: u32 },
    /// A file or payload exceeds [`MAX_FILE_BYTES`].
    TooLarge { path: String, limit: String },
    /// Filesystem failure with the OS message attached.
    Io { path: String, message: String },
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::InvalidId(text) => {
                write!(f, "error: invalid session id: '{text}'")
            }
            SessionError::NotFound { id } => {
                write!(f, "error: no such session: '{id}' (see /sessions)")
            }
            SessionError::Ambiguous { prefix, candidates } => {
                write!(
                    f,
                    "error: session prefix '{prefix}' matches several sessions: {} (use a longer prefix)",
                    candidates.join(", ")
                )
            }
            SessionError::Corrupt { path, reason } => {
                write!(f, "error: corrupt session file '{path}': {reason}")
            }
            SessionError::Incompatible { path, found } => {
                write!(
                    f,
                    "error: session file '{path}' uses format version {found} (this build reads version {SESSION_FORMAT}; the session cannot be resumed)"
                )
            }
            SessionError::TooLarge { path, limit } => {
                write!(f, "error: session file '{path}' exceeds the {limit} limit")
            }
            SessionError::Io { path, message } => {
                write!(f, "error: cannot access '{path}': {message}")
            }
        }
    }
}

impl std::error::Error for SessionError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_name_the_session() {
        let text = SessionError::NotFound { id: "abc".into() }.to_string();
        assert!(text.contains("abc"), "got: {text:?}");
        assert!(text.starts_with("error:"));
        assert!(text.contains("/sessions"));
    }

    #[test]
    fn session_mode_round_trips_through_spellings() {
        assert_eq!(SessionMode::Build.as_str(), "build");
        assert_eq!(SessionMode::Plan.as_str(), "plan");
        assert_eq!(SessionMode::parse("build"), Ok(SessionMode::Build));
        assert_eq!(SessionMode::parse("plan"), Ok(SessionMode::Plan));
        assert!(SessionMode::parse("turbo").is_err());
        assert!(SessionMode::parse("").is_err());
        assert_eq!(
            SessionMode::from(aurel_model::Mode::Build),
            SessionMode::Build
        );
        assert_eq!(
            aurel_model::Mode::from(SessionMode::Plan),
            aurel_model::Mode::Plan
        );
    }

    #[test]
    fn describe_age_handles_fresh_and_skewed_clocks() {
        let now = now_ms();
        assert_eq!(describe_age(now), "0s");
        assert_eq!(describe_age(now.saturating_sub(90_000)), "1m");
        assert_eq!(describe_age(now.saturating_sub(5 * 3_600_000)), "5h");
        assert_eq!(describe_age(now.saturating_sub(3 * 86_400_000)), "3d");
        // Future timestamps (clock skew) saturate instead of wrapping.
        assert_eq!(describe_age(now.saturating_add(60_000)), "0s");
    }
}
