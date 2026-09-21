//! Bounded on-disk session store: one JSON file per session.
//!
//! A [`SessionStore`] is bound to one directory (created on open). Files
//! are named `<id>.json` where `id` is a validated [`SessionId`], so IDs
//! can never escape the directory. Writes are atomic (temp file + rename
//! in the same directory); reads are bounded ([`MAX_FILE_BYTES`]) and
//! validated (format version, ID/filename agreement, mode spelling,
//! message-count cap, timestamp order). Anything else is a typed
//! [`SessionError`] — corrupt, missing, incompatible, or stale data fails
//! safely instead of entering the loop.
//!
//! What is stored: history (roles + contents), Plan/Build mode, working
//! directory, project marker, and loop counters. What is never stored:
//! API keys or any other configuration (the store API has no field for
//! them), `AGENTS.md` instructions (reloaded live on every run and every
//! resume), pending proposals, and undo records. A resumed session
//! therefore starts with an empty approval queue: resuming can never
//! execute anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use aurel_model::{Message, Mode};

use crate::{SessionError, SessionId};

/// On-disk format version. Files with any other version load as
/// [`SessionError::Incompatible`] — never guessed at.
pub const SESSION_FORMAT: u32 = 1;

/// Max history messages kept per stored session (the leading compact
/// summary, when present, is preserved plus the newest rest).
pub const MAX_STORED_MESSAGES: usize = 200;

/// Hard ceiling for one session file on read and on write.
pub const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Max messages accepted from a stored file (far above what the saver
/// writes; bounds memory against hand-crafted files).
pub const MAX_LOADED_MESSAGES: usize = 5000;

/// Max sessions summarized for one `@explore` prompt.
pub const MAX_EXPLORE_SESSIONS: usize = 5;

/// Max characters taken from one session into `@explore` context.
pub const MAX_EXPLORE_CHARS_PER_SESSION: usize = 500;

/// Max sessions a `/sessions` listing parses (newest by mtime first).
pub const MAX_LISTED_SESSIONS: usize = 100;

/// The persisted interaction mode. Mirrors [`Mode`] without depending on
/// its memory representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    Build,
    Plan,
}

impl SessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionMode::Build => "build",
            SessionMode::Plan => "plan",
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "build" => Ok(SessionMode::Build),
            "plan" => Ok(SessionMode::Plan),
            _ => Err(format!(
                "unknown mode '{text}' (expected 'build' or 'plan')"
            )),
        }
    }
}

impl From<Mode> for SessionMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Build => SessionMode::Build,
            Mode::Plan => SessionMode::Plan,
        }
    }
}

impl From<SessionMode> for Mode {
    fn from(mode: SessionMode) -> Self {
        match mode {
            SessionMode::Build => Mode::Build,
            SessionMode::Plan => Mode::Plan,
        }
    }
}

/// Everything the store persists for one session. Carries no secrets by
/// construction: there is simply no field for configuration, keys, or
/// instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionData {
    pub id: SessionId,
    /// Creation time (ms since the Unix epoch). Zero means "stamp now" on
    /// save — how new sessions are created.
    pub created_ms: u64,
    /// Last-write time. Stamped by the store on save; the input value is
    /// ignored.
    pub updated_ms: u64,
    pub mode: SessionMode,
    /// Working directory the session runs in (display/audit metadata).
    pub workdir: String,
    /// Project marker kind (`"cargo"`, `"npm"`, …) when detected.
    pub project_kind: Option<String>,
    pub history: Vec<Message>,
    pub iterations_used: u32,
    pub compactions: u32,
}

/// List-row metadata: the file parsed far enough to describe, without
/// loading callers into full histories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMeta {
    pub id: SessionId,
    pub mode: SessionMode,
    pub message_count: usize,
    pub workdir: String,
    pub created_ms: u64,
    pub updated_ms: u64,
}

/// One other session's retrievable context for `@explore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub mode: SessionMode,
    pub message_count: usize,
    pub updated_ms: u64,
    pub workdir: String,
    /// Compact summary when the session was compacted, else its first
    /// user message, else `None`. Already clipped to
    /// [`MAX_EXPLORE_CHARS_PER_SESSION`] characters.
    pub blurb: Option<String>,
}

/// The exact on-disk shape. Private: callers work in [`SessionData`], so
/// the file format can evolve behind [`SESSION_FORMAT`] without touching
/// call sites.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct StoredFile {
    format: u32,
    id: String,
    created_ms: u64,
    updated_ms: u64,
    mode: String,
    workdir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project_kind: Option<String>,
    history: Vec<Message>,
    #[serde(default)]
    iterations_used: u32,
    #[serde(default)]
    compactions: u32,
}

/// Milliseconds since the Unix epoch (0 when the clock misbehaves — a
/// display/sort value, never a security decision).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// Short relative age for listings (`12s`, `3m`, `5h`, `9d`).
pub fn describe_age(updated_ms: u64) -> String {
    let now = now_ms();
    let secs = now.saturating_sub(updated_ms) / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A session directory: `<dir>/<id>.json`, created on open.
#[derive(Debug, Clone)]
pub struct SessionStore {
    dir: PathBuf,
}

impl SessionStore {
    /// Bind to `dir`, creating it (and parents) when missing. A missing or
    /// uncreatable directory is a typed error — the loop then continues
    /// without persistence rather than failing the session.
    pub fn open(dir: &Path) -> Result<Self, SessionError> {
        fs::create_dir_all(dir).map_err(|e| SessionError::Io {
            path: dir.display().to_string(),
            message: format!("cannot create sessions directory: {e}"),
        })?;
        // Prove writability now (a read-only mount should fail here, not
        // mid-session at the first autosave).
        let probe = dir.join(format!(".aurel-probe-{}", std::process::id()));
        fs::write(&probe, b"probe").map_err(|e| SessionError::Io {
            path: dir.display().to_string(),
            message: format!("sessions directory is not writable: {e}"),
        })?;
        let _ = fs::remove_file(&probe);
        Ok(SessionStore {
            dir: dir.to_path_buf(),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn file(&self, id: &SessionId) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Mint a fresh, collision-free ID. Writes nothing: the file appears
    /// on the first save, so abandoned sessions leave no litter.
    pub fn create_id(&self) -> SessionId {
        for _ in 0..100 {
            let id = SessionId::generate();
            if !self.file(&id).exists() {
                return id;
            }
        }
        // Practically unreachable (100 random collisions); still total:
        // fall back to a counter-suffixed ID outside the normal shape is
        // worse than waiting out the clock, so sleep 1s and retry once.
        std::thread::sleep(std::time::Duration::from_secs(1));
        self.create_id()
    }

    /// Persist `data`: truncate history to [`MAX_STORED_MESSAGES`]
    /// (preserving a leading compact summary), stamp `updated_ms`, and
    /// write atomically. Oversized payloads shed oldest messages first;
    /// only a single un-storable message fails as [`SessionError::TooLarge`].
    /// The live `data.history` is never modified — truncation applies to
    /// the stored copy only.
    pub fn save(&self, data: &SessionData) -> Result<(), SessionError> {
        let now = now_ms();
        let created = if data.created_ms == 0 {
            now
        } else {
            data.created_ms
        };
        let history = truncate_history(&data.history);
        let mut stored = StoredFile {
            format: SESSION_FORMAT,
            id: data.id.as_str().to_string(),
            created_ms: created,
            updated_ms: now,
            mode: data.mode.as_str().to_string(),
            workdir: data.workdir.clone(),
            project_kind: data.project_kind.clone(),
            history,
            iterations_used: data.iterations_used,
            compactions: data.compactions,
        };
        // Shed oldest messages until the file fits (the summary goes last:
        // it is the cheapest context per byte).
        loop {
            let bytes = serde_json::to_vec(&stored).map_err(|e| SessionError::Io {
                path: self.file(&data.id).display().to_string(),
                message: format!("cannot encode session: {e}"),
            })?;
            if bytes.len() as u64 <= MAX_FILE_BYTES {
                return self.write_bytes(&data.id, &bytes);
            }
            if stored.history.len() <= 1 {
                return Err(SessionError::TooLarge {
                    path: self.file(&data.id).display().to_string(),
                    limit: format!("{MAX_FILE_BYTES} bytes"),
                });
            }
            if is_compact_summary(&stored.history[0]) && stored.history.len() > 2 {
                stored.history.remove(1);
            } else {
                stored.history.remove(0);
            }
        }
    }

    fn write_bytes(&self, id: &SessionId, bytes: &[u8]) -> Result<(), SessionError> {
        let dest = self.file(id);
        let io_error = |message: String| SessionError::Io {
            path: dest.display().to_string(),
            message,
        };
        let serial = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
        let temp = self
            .dir
            .join(format!(".aurel-tmp-{}-{serial}.json", std::process::id()));
        fs::write(&temp, bytes).map_err(|e| {
            let _ = fs::remove_file(&temp);
            io_error(format!("cannot stage session file: {e}"))
        })?;
        fs::rename(&temp, &dest).map_err(|e| {
            let _ = fs::remove_file(&temp);
            io_error(format!("cannot publish session file: {e}"))
        })?;
        Ok(())
    }

    /// Load a session by full ID or unambiguous prefix. Missing files,
    /// ambiguous prefixes, incompatible versions, and corrupt content are
    /// distinct typed errors — never a guess, never a panic.
    pub fn load(&self, id_or_prefix: &str) -> Result<SessionData, SessionError> {
        let id = self.resolve(id_or_prefix)?;
        let path = self.file(&id);
        let bytes = fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SessionError::NotFound {
                    id: id.as_str().to_string(),
                }
            } else {
                SessionError::Io {
                    path: path.display().to_string(),
                    message: format!("cannot read session file: {e}"),
                }
            }
        })?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(SessionError::TooLarge {
                path: path.display().to_string(),
                limit: format!("{MAX_FILE_BYTES} bytes"),
            });
        }
        let stored: StoredFile =
            serde_json::from_slice(&bytes).map_err(|e| SessionError::Corrupt {
                path: path.display().to_string(),
                reason: format!("invalid session JSON: {}", clip_error(&e.to_string())),
            })?;
        self.validate(&path, &id, stored)
    }

    fn validate(
        &self,
        path: &Path,
        id: &SessionId,
        stored: StoredFile,
    ) -> Result<SessionData, SessionError> {
        let corrupt = |reason: String| SessionError::Corrupt {
            path: path.display().to_string(),
            reason,
        };
        if stored.format != SESSION_FORMAT {
            return Err(SessionError::Incompatible {
                path: path.display().to_string(),
                found: stored.format,
            });
        }
        if stored.id != id.as_str() {
            return Err(corrupt(format!(
                "session ID '{}' does not match its filename",
                clip_inline(&stored.id)
            )));
        }
        if stored.history.len() > MAX_LOADED_MESSAGES {
            return Err(corrupt(format!(
                "session holds {} messages (max {MAX_LOADED_MESSAGES})",
                stored.history.len()
            )));
        }
        if stored.updated_ms < stored.created_ms {
            return Err(corrupt("session timestamps run backwards".to_string()));
        }
        let mode = SessionMode::parse(&stored.mode).map_err(corrupt)?;
        Ok(SessionData {
            id: id.clone(),
            created_ms: stored.created_ms,
            updated_ms: stored.updated_ms,
            mode,
            workdir: stored.workdir,
            project_kind: stored.project_kind,
            history: stored.history,
            iterations_used: stored.iterations_used,
            compactions: stored.compactions,
        })
    }

    /// Resolve a full ID or unambiguous prefix to an ID. Malformed input
    /// fails as [`SessionError::InvalidId`] before touching the directory
    /// (prefixes use a looser charset: lowercase hex and digits only, plus
    /// dashes in ID positions — still no separators, so no traversal).
    pub fn resolve(&self, id_or_prefix: &str) -> Result<SessionId, SessionError> {
        if let Ok(id) = SessionId::parse(id_or_prefix) {
            return Ok(id);
        }
        if id_or_prefix.is_empty()
            || id_or_prefix.len() > 24
            || !id_or_prefix.bytes().all(|b| {
                b.is_ascii_digit()
                    || b == b'-'
                    || (b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            })
            || id_or_prefix.contains(' ')
            || id_or_prefix.contains('/')
            || id_or_prefix.contains('\\')
            || id_or_prefix.contains('.')
        {
            return Err(SessionError::InvalidId(id_or_prefix.to_string()));
        }
        let mut matches = Vec::new();
        let entries = fs::read_dir(&self.dir).map_err(|e| SessionError::Io {
            path: self.dir.display().to_string(),
            message: format!("cannot list sessions: {e}"),
        })?;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".json") {
                if stem.starts_with(id_or_prefix) {
                    matches.push(stem.to_string());
                }
            }
        }
        matches.sort();
        match matches.len() {
            0 => Err(SessionError::NotFound {
                id: id_or_prefix.to_string(),
            }),
            1 => SessionId::parse(&matches[0]),
            _ => Err(SessionError::Ambiguous {
                prefix: id_or_prefix.to_string(),
                candidates: matches.into_iter().take(8).collect(),
            }),
        }
    }

    /// Newest-first metadata for `/sessions` (capped at
    /// [`MAX_LISTED_SESSIONS`]). Unparseable files are skipped so one bad
    /// file can never hide the rest; use [`SessionStore::load`] for strict
    /// per-session errors.
    pub fn list(&self) -> Vec<SessionMeta> {
        let mut entries: Vec<(String, u64)> = Vec::new();
        let read = match fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(_) => return Vec::new(),
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".json") || name.starts_with('.') {
                continue;
            }
            let stem = name.trim_end_matches(".json").to_string();
            if SessionId::parse(&stem).is_err() {
                continue;
            }
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
                .unwrap_or(0);
            entries.push((stem, mtime));
        }
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        entries.truncate(MAX_LISTED_SESSIONS);
        let mut metas = Vec::new();
        for (stem, _) in entries {
            let id = match SessionId::parse(&stem) {
                Ok(id) => id,
                Err(_) => continue,
            };
            let data: SessionData = match self.load(id.as_str()) {
                Ok(data) => data,
                Err(_) => continue,
            };
            metas.push(SessionMeta {
                id,
                mode: data.mode,
                message_count: data.history.len(),
                workdir: data.workdir,
                created_ms: data.created_ms,
                updated_ms: data.updated_ms,
            });
        }
        metas
    }

    /// Retrieval context for one `@explore` prompt: at most
    /// [`MAX_EXPLORE_SESSIONS`] other sessions (newest first), each reduced
    /// to a clipped blurb. The current session is excluded — its history is
    /// already in the request.
    pub fn summaries(&self, exclude: &SessionId) -> Vec<SessionSummary> {
        self.list()
            .into_iter()
            .filter(|meta| meta.id != *exclude)
            .take(MAX_EXPLORE_SESSIONS)
            .filter_map(|meta| {
                let data = self.load(meta.id.as_str()).ok()?;
                Some(SessionSummary {
                    id: meta.id,
                    mode: meta.mode,
                    message_count: data.history.len(),
                    updated_ms: meta.updated_ms,
                    workdir: meta.workdir,
                    blurb: blurb_of(&data.history),
                })
            })
            .collect()
    }
}

/// The retrievable gist of one history: its compact summary when compacted,
/// else its first user message — clipped, single-line, never secrets (the
/// store holds none).
fn blurb_of(history: &[Message]) -> Option<String> {
    if history.is_empty() {
        return None;
    }
    if is_compact_summary(&history[0]) {
        return Some(single_line(
            &history[0].content,
            MAX_EXPLORE_CHARS_PER_SESSION,
        ));
    }
    history
        .iter()
        .find(|message| message.role == aurel_model::Role::User)
        .map(|message| single_line(&message.content, MAX_EXPLORE_CHARS_PER_SESSION))
}

/// True for the `system` message [`Agent::compact`](aurel_model::Agent::compact)
/// prepends (the only system message the loop itself writes first).
fn is_compact_summary(message: &Message) -> bool {
    message.role == aurel_model::Role::System && message.content.starts_with("Session summary: ")
}

fn single_line(text: &str, max_chars: usize) -> String {
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > max_chars {
        let clipped: String = flat.chars().take(max_chars).collect();
        format!("{clipped}…")
    } else {
        flat
    }
}

fn clip_inline(text: &str) -> String {
    single_line(text, 120)
}

fn clip_error(text: &str) -> String {
    single_line(text, 300)
}

/// Stored-copy history truncation: keep a leading compact summary plus the
/// newest messages, capped at [`MAX_STORED_MESSAGES`]. The caller's vector
/// is never modified.
fn truncate_history(history: &[Message]) -> Vec<Message> {
    if history.len() <= MAX_STORED_MESSAGES {
        return history.to_vec();
    }
    let mut kept = Vec::with_capacity(MAX_STORED_MESSAGES);
    let mut rest = history;
    if !history.is_empty() && is_compact_summary(&history[0]) {
        kept.push(history[0].clone());
        rest = &history[1..];
    }
    let take = MAX_STORED_MESSAGES - kept.len();
    let start = rest.len().saturating_sub(take);
    kept.extend_from_slice(&rest[start..]);
    kept
}

/// Build the `@explore` context block: project metadata plus other
/// sessions' blurbs. Bounded by construction; rides one request only and
/// is never stored in history.
pub fn explore_context(
    current_id: &SessionId,
    current_mode: SessionMode,
    workdir: &Path,
    others: &[SessionSummary],
) -> String {
    let mut block =
        String::from("[cross-session context for this prompt only — not stored in history]\n");
    block.push_str(&format!("project: {}\n", workdir.display()));
    block.push_str(&format!(
        "current session: {current_id} ({})\n",
        current_mode.as_str()
    ));
    if others.is_empty() {
        block.push_str("other sessions: none saved yet\n");
    } else {
        block.push_str(&format!("other sessions ({}):\n", others.len()));
        for summary in others {
            block.push_str(&format!(
                "- {} ({}, {} msgs, {}): {}\n",
                summary.id.as_str(),
                summary.mode.as_str(),
                summary.message_count,
                describe_age(summary.updated_ms),
                summary
                    .blurb
                    .as_deref()
                    .unwrap_or("(no retrievable summary)")
            ));
        }
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-session-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn store(name: &str) -> SessionStore {
        let dir = test_dir(name);
        SessionStore::open(&dir).expect("store opens")
    }

    fn sample_data(store: &SessionStore) -> SessionData {
        SessionData {
            id: store.create_id(),
            created_ms: 0,
            updated_ms: 0,
            mode: SessionMode::Build,
            workdir: "C:\\proj".to_string(),
            project_kind: Some("cargo".to_string()),
            history: vec![
                Message::user("first question"),
                Message::assistant("first answer"),
            ],
            iterations_used: 2,
            compactions: 0,
        }
    }

    #[test]
    fn save_load_round_trip_preserves_state() {
        let store = store("round-trip");
        let data = sample_data(&store);
        store.save(&data).expect("save");
        let loaded = store.load(data.id.as_str()).expect("load");
        assert_eq!(loaded.id, data.id);
        assert_eq!(loaded.mode, SessionMode::Build);
        assert_eq!(loaded.history, data.history);
        assert_eq!(loaded.workdir, "C:\\proj");
        assert_eq!(loaded.project_kind.as_deref(), Some("cargo"));
        assert_eq!(loaded.iterations_used, 2);
        assert!(loaded.created_ms > 0 && loaded.updated_ms >= loaded.created_ms);
    }

    #[test]
    fn missing_and_ambiguous_loads_are_typed() {
        let store = store("missing");
        let err = store.load("20260918-120301-deadbeef").expect_err("missing");
        assert!(matches!(err, SessionError::NotFound { .. }), "got: {err:?}");
        assert!(err.to_string().contains("/sessions"));

        // Two sessions sharing a prefix: full IDs resolve, the prefix is
        // ambiguous, a longer unique prefix resolves.
        for name in ["20260918-120301-aa010001", "20260918-120301-aa020001"] {
            let id = SessionId::parse(name).expect("fixture ID");
            store
                .save(&SessionData {
                    id,
                    created_ms: 1,
                    updated_ms: 0,
                    mode: SessionMode::Plan,
                    workdir: "w".to_string(),
                    project_kind: None,
                    history: Vec::new(),
                    iterations_used: 0,
                    compactions: 0,
                })
                .expect("save fixture");
        }
        assert!(store.load("20260918-120301-aa010001").is_ok());
        let err = store.load("20260918-120301-aa").expect_err("ambiguous");
        assert!(
            matches!(err, SessionError::Ambiguous { .. }),
            "got: {err:?}"
        );
        assert!(
            store.load("20260918-120301-aa01").is_ok(),
            "unique prefix resolves"
        );
    }

    #[test]
    fn corrupt_and_incompatible_files_fail_safely() {
        let store = store("corrupt");
        let dir = store.dir().to_path_buf();
        // Not JSON at all.
        let bad_id = "20260918-120301-bad0bad0";
        fs::write(dir.join(format!("{bad_id}.json")), b"{not json").expect("write");
        let err = store.load(bad_id).expect_err("malformed");
        assert!(matches!(err, SessionError::Corrupt { .. }), "got: {err:?}");

        // Wrong format version.
        let old_id = "20260918-120301-01d00001";
        fs::write(
            dir.join(format!("{old_id}.json")),
            br#"{"format":0,"id":"20260918-120301-01d00001","created_ms":1,"updated_ms":2,"mode":"build","workdir":"w","history":[]}"#,
        )
        .expect("write");
        let err = store.load(old_id).expect_err("versioned");
        assert!(
            matches!(err, SessionError::Incompatible { .. }),
            "got: {err:?}"
        );

        // ID/filename mismatch.
        let mismatch_id = "20260918-120301-c0ffee00";
        fs::write(
            dir.join(format!("{mismatch_id}.json")),
            br#"{"format":1,"id":"20260918-120301-deadbeef","created_ms":1,"updated_ms":2,"mode":"build","workdir":"w","history":[]}"#,
        )
        .expect("write");
        let err = store.load(mismatch_id).expect_err("mismatch");
        assert!(matches!(err, SessionError::Corrupt { .. }), "got: {err:?}");

        // Unknown mode.
        let mode_id = "20260918-120301-500d0000";
        fs::write(
            dir.join(format!("{mode_id}.json")),
            br#"{"format":1,"id":"20260918-120301-500d0000","created_ms":1,"updated_ms":2,"mode":"turbo","workdir":"w","history":[]}"#,
        )
        .expect("write");
        let err = store.load(mode_id).expect_err("mode");
        assert!(matches!(err, SessionError::Corrupt { .. }), "got: {err:?}");

        // Backwards timestamps.
        let time_id = "20260918-120301-71e00000";
        fs::write(
            dir.join(format!("{time_id}.json")),
            br#"{"format":1,"id":"20260918-120301-71e00000","created_ms":9,"updated_ms":3,"mode":"build","workdir":"w","history":[]}"#,
        )
        .expect("write");
        let err = store.load(time_id).expect_err("timestamps");
        assert!(matches!(err, SessionError::Corrupt { .. }), "got: {err:?}");

        // Oversized file.
        let big_id = "20260918-120301-b1600000";
        let big = vec![b'x'; (MAX_FILE_BYTES + 1) as usize];
        fs::write(dir.join(format!("{big_id}.json")), big).expect("write");
        let err = store.load(big_id).expect_err("oversized");
        assert!(matches!(err, SessionError::TooLarge { .. }), "got: {err:?}");

        // Listing skips every bad file instead of failing.
        let listed = store.list();
        assert!(listed.is_empty(), "bad files must not list: {listed:?}");
    }

    #[test]
    fn history_truncation_preserves_summary_and_bounds_disk() {
        let store = store("truncate");
        let mut history = vec![Message::system("Session summary: big project")];
        for i in 0..(MAX_STORED_MESSAGES + 50) {
            history.push(Message::user(format!("question {i}")));
            history.push(Message::assistant(format!("answer {i}")));
        }
        let live_len = history.len();
        let mut data = sample_data(&store);
        data.history = history;
        store.save(&data).expect("save truncates");
        // Live history untouched.
        assert_eq!(data.history.len(), live_len);
        let loaded = store.load(data.id.as_str()).expect("load");
        assert_eq!(loaded.history.len(), MAX_STORED_MESSAGES);
        assert_eq!(loaded.history[0].content, "Session summary: big project");
        assert!(loaded
            .history
            .last()
            .expect("last")
            .content
            .contains("answer"));
    }

    #[test]
    fn secrets_never_reach_disk() {
        // The save path never reads configuration or the environment: even
        // with a live key in the process, the file must not contain it.
        let sentinel = "sk-aurel-test-sentinel-9f8e7d6c5b4a";
        std::env::set_var("AUREL_API_KEY", sentinel);
        let store = store("secrets");
        let data = sample_data(&store);
        store.save(&data).expect("save");
        std::env::remove_var("AUREL_API_KEY");
        let raw = fs::read_to_string(store.dir().join(format!("{}.json", data.id))).expect("read");
        assert!(!raw.contains(sentinel), "secret leaked to disk");
        assert!(!raw.to_lowercase().contains("api_key"), "got: {raw:?}");
    }

    #[test]
    fn ids_are_filename_safe_and_prefixes_reject_traversal() {
        let store = store("traversal");
        for hostile in [
            "../evil",
            "..",
            "",
            "20260918-120301-ABCDEF12",
            "a/b",
            "a\\b",
            "a b",
        ] {
            assert!(store.load(hostile).is_err(), "must reject {hostile:?}");
        }
        // Outside files are never read: a valid session file placed next
        // to (not inside) the store is invisible to it.
        let outside = test_dir("traversal-outside");
        fs::create_dir_all(&outside).expect("mkdir");
        let data = sample_data(&store);
        let elsewhere = SessionStore::open(&outside).expect("second store");
        elsewhere.save(&data).expect("save elsewhere");
        assert!(store.load(data.id.as_str()).is_err());
    }

    #[test]
    fn list_orders_newest_first_and_summaries_exclude_current() {
        let store = store("list");
        let mut first = sample_data(&store);
        first.history = vec![Message::user("alpha topic")];
        store.save(&first).expect("save first");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut second = sample_data(&store);
        second.mode = SessionMode::Plan;
        second.history = vec![
            Message::system("Session summary: beta work"),
            Message::user("follow-up"),
        ];
        store.save(&second).expect("save second");

        let listed = store.list();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, second.id, "newest first");
        assert_eq!(listed[0].mode, SessionMode::Plan);

        let summaries = store.summaries(&second.id);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, first.id);
        assert!(summaries[0].blurb.as_deref() == Some("alpha topic"));

        let block = explore_context(
            &second.id,
            SessionMode::Plan,
            Path::new("C:\\proj"),
            &summaries,
        );
        assert!(block.contains("not stored in history"), "got: {block:?}");
        assert!(block.contains(first.id.as_str()), "got: {block:?}");
        assert!(block.contains("alpha topic"), "got: {block:?}");
    }

    #[test]
    fn store_open_fails_typed_on_unusable_dirs() {
        // A file (not a directory) cannot host a store.
        let dir = test_dir("open-bad");
        fs::create_dir_all(&dir).expect("mkdir");
        let file = dir.join("blocker");
        fs::write(&file, b"x").expect("write");
        assert!(SessionStore::open(&file.join("sub")).is_err());
    }
}
