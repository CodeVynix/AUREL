//! Project instructions (`AGENTS.md`): starter template, guarded creation,
//! upward discovery, and bounded loading.
//!
//! The file is plain Markdown owned by the user. AUREL only ever reads it
//! (or creates it once via [`init_agents_md`]); editing stays with the
//! user and future editing tools. Discovery walks upward so a nested
//! working directory inherits its project's instructions; the loader caps
//! size so a runaway file cannot flood model context.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ToolError;

/// Project instructions filename, looked up by [`discover_agents_md`].
pub const AGENTS_MD: &str = "AGENTS.md";

/// Max bytes loaded from one `AGENTS.md`; larger files truncate with a
/// marker instead of failing the run.
pub const MAX_INSTRUCTIONS_BYTES: u64 = 64 * 1024;

/// Loaded project instructions: source path plus content for the agent run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInstructions {
    pub path: PathBuf,
    pub content: String,
    /// True when the file extended past [`MAX_INSTRUCTIONS_BYTES`].
    pub truncated: bool,
}

/// Outcome of [`init_agents_md`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitOutcome {
    /// Created a starter file where none existed.
    Created(PathBuf),
    /// Replaced an existing file under explicit `--force`.
    Overwritten(PathBuf),
    /// Left an existing file untouched.
    AlreadyExists(PathBuf),
}

/// Starter template with the project name filled in. Short on purpose: a
/// template the user will actually edit beats an exhaustive one they will
/// not. Sections mirror what agent runs need most (build, test,
/// conventions), and the tail records how AUREL itself behaves here.
pub fn starter_template(project_name: &str) -> String {
    format!(
        "# AGENTS.md — project instructions for AUREL\n\
         \n\
         > Loaded as project context for every agent run in this workspace.\n\
         > Kept separate from conversation history. Keep it short, factual,\n\
         > and current.\n\
         \n\
         ## Project\n\
         \n\
         - Name: {project_name}\n\
         - What it does: <one or two sentences>\n\
         - Main language / stack: <e.g. Rust workspace>\n\
         \n\
         ## Build & test\n\
         \n\
         - Build: <command, e.g. `cargo build --workspace`>\n\
         - Test: <command, e.g. `cargo test --workspace`>\n\
         - Lint/format: <commands>\n\
         \n\
         ## Conventions\n\
         \n\
         - <coding style, commit style, review rules>\n\
         - <what must never be done here>\n\
         \n\
         ## Notes for the agent\n\
         \n\
         - Prefer small, reviewable changes; verify with builds and tests.\n\
         - Never commit secrets; never claim unverified work.\n\
         - In Plan mode propose only — do not mutate.\n"
    )
}

/// Create `AGENTS.md` with the starter template inside `dir`.
///
/// Creates only when absent; an existing file is left untouched and
/// reported as [`InitOutcome::AlreadyExists`] unless `force` explicitly
/// permits replacement. `dir` itself must exist and be a directory.
pub fn init_agents_md(dir: &Path, force: bool) -> Result<InitOutcome, ToolError> {
    if !dir.exists() {
        return Err(ToolError::NotFound {
            path: dir.display().to_string(),
        });
    }
    if !dir.is_dir() {
        return Err(ToolError::NotDirectory {
            path: dir.display().to_string(),
        });
    }
    let target = dir.join(AGENTS_MD);
    if target.exists() && !force {
        return Ok(InitOutcome::AlreadyExists(target));
    }
    let existed = target.exists();
    let name = dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_string());
    fs::write(&target, starter_template(&name)).map_err(|e| ToolError::Io {
        path: target.display().to_string(),
        message: e.to_string(),
    })?;
    if existed {
        Ok(InitOutcome::Overwritten(target))
    } else {
        Ok(InitOutcome::Created(target))
    }
}

/// Search `start` and its ancestors for `AGENTS.md`, nearest first.
/// Bounded (64 levels), stops at the filesystem root. Returns `None` when
/// no ancestor holds the file — the normal case for fresh checkouts.
pub fn discover_agents_md(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    for _ in 0..64 {
        let candidate = dir.join(AGENTS_MD);
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = dir.parent()?;
    }
    None
}

/// Load and bound one `AGENTS.md` file. Missing files and directories map
/// to [`ToolError::NotFound`] / [`ToolError::NotFile`]; oversized files
/// truncate with a marker rather than failing the run.
pub fn load_agents_md(path: &Path) -> Result<ProjectInstructions, ToolError> {
    let label = path.display().to_string();
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ToolError::NotFound {
                path: label.clone(),
            }
        } else {
            ToolError::Io {
                path: label.clone(),
                message: e.to_string(),
            }
        }
    })?;
    if !meta.file_type().is_file() {
        return Err(ToolError::NotFile { path: label });
    }
    let bytes = fs::read(path).map_err(|e| ToolError::Io {
        path: label.clone(),
        message: e.to_string(),
    })?;
    let truncated = bytes.len() as u64 > MAX_INSTRUCTIONS_BYTES;
    let kept = bytes
        .get(..MAX_INSTRUCTIONS_BYTES as usize)
        .unwrap_or(&bytes);
    let mut content = String::from_utf8_lossy(kept).into_owned();
    if truncated {
        content.push_str("\n[…truncated at 65536 bytes]");
    }
    Ok(ProjectInstructions {
        path: path.to_path_buf(),
        content,
        truncated,
    })
}

/// Discover from `dir` and load the nearest `AGENTS.md`, if any.
/// `Ok(None)` means absent (run without instructions); errors mean the
/// file exists but cannot be used — callers warn and continue.
pub fn load_instructions_for_dir(dir: &Path) -> Result<Option<ProjectInstructions>, ToolError> {
    match discover_agents_md(dir) {
        None => Ok(None),
        Some(path) => load_agents_md(&path).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-tools-init-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parents");
        }
        let mut file = fs::File::create(path).expect("create file");
        file.write_all(content).expect("write file");
    }

    #[test]
    fn template_names_the_project() {
        let text = starter_template("demo-proj");
        assert!(text.contains("# AGENTS.md"), "got: {text:?}");
        assert!(text.contains("demo-proj"), "got: {text:?}");
        assert!(text.contains("Plan mode"), "got: {text:?}");
    }

    #[test]
    fn init_creates_only_when_absent() {
        let dir = test_dir("init-create");
        match init_agents_md(&dir, false).expect("init") {
            InitOutcome::Created(path) => {
                assert_eq!(path, dir.join(AGENTS_MD));
                let text = fs::read_to_string(&path).expect("read back");
                assert!(text.contains(&dir.file_name().unwrap().to_string_lossy().into_owned()));
            }
            other => panic!("expected Created, got {other:?}"),
        }
        // Second run refuses without force.
        match init_agents_md(&dir, false).expect("init again") {
            InitOutcome::AlreadyExists(path) => assert_eq!(path, dir.join(AGENTS_MD)),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
        // Content untouched by the refusal.
        let before = fs::read_to_string(dir.join(AGENTS_MD)).expect("read");
        assert!(before.contains("# AGENTS.md"));
    }

    #[test]
    fn init_force_overwrites_explicitly() {
        let dir = test_dir("init-force");
        write(&dir.join(AGENTS_MD), b"custom content");
        match init_agents_md(&dir, true).expect("force") {
            InitOutcome::Overwritten(path) => {
                let text = fs::read_to_string(&path).expect("read back");
                assert!(text.contains("# AGENTS.md"), "got: {text:?}");
                assert!(!text.contains("custom content"));
            }
            other => panic!("expected Overwritten, got {other:?}"),
        }
    }

    #[test]
    fn init_rejects_missing_or_file_dir() {
        let missing = std::env::temp_dir().join(format!(
            "aurel-tools-no-such-dir-{}-init",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&missing);
        assert!(matches!(
            init_agents_md(&missing, false),
            Err(ToolError::NotFound { .. })
        ));
        let dir = test_dir("init-filedir");
        let file = dir.join("plain.txt");
        write(&file, b"x");
        assert!(matches!(
            init_agents_md(&file, false),
            Err(ToolError::NotDirectory { .. })
        ));
    }

    #[test]
    fn discover_prefers_nearest_and_finds_nothing_when_absent() {
        let dir = test_dir("discover");
        write(&dir.join(AGENTS_MD), b"outer\n");
        let inner = dir.join("a").join("b");
        fs::create_dir_all(&inner).expect("mkdir");
        assert_eq!(discover_agents_md(&inner), Some(dir.join(AGENTS_MD)));
        assert_eq!(discover_agents_md(&dir), Some(dir.join(AGENTS_MD)));
        // Nearest wins over an ancestor.
        write(&dir.join("a").join(AGENTS_MD), b"inner\n");
        assert_eq!(
            discover_agents_md(&inner),
            Some(dir.join("a").join(AGENTS_MD))
        );
        assert_eq!(discover_agents_md(&test_dir("discover-empty")), None);
    }

    #[test]
    fn load_reads_and_truncates_large_files() {
        let dir = test_dir("load");
        write(&dir.join(AGENTS_MD), b"# hello\n");
        let loaded = load_agents_md(&dir.join(AGENTS_MD)).expect("load");
        assert_eq!(loaded.content, "# hello\n");
        assert!(!loaded.truncated);
        assert_eq!(loaded.path, dir.join(AGENTS_MD));

        let big = vec![b'z'; 70 * 1024];
        write(&dir.join("big").join(AGENTS_MD), &big);
        let loaded = load_agents_md(&dir.join("big").join(AGENTS_MD)).expect("load big");
        assert!(loaded.truncated);
        assert!(loaded.content.contains("[…truncated at 65536 bytes]"));
        assert!(loaded.content.len() < 70 * 1024);

        assert!(matches!(
            load_agents_md(&dir.join("missing.md")),
            Err(ToolError::NotFound { .. })
        ));
        assert!(matches!(
            load_agents_md(&dir),
            Err(ToolError::NotFile { .. })
        ));
    }

    #[test]
    fn load_for_dir_returns_none_when_absent() {
        let dir = test_dir("load-none");
        assert_eq!(load_instructions_for_dir(&dir).expect("absent"), None);
    }
}
