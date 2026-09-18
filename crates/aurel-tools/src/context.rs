//! Workspace-bound inspection context: the path sandbox plus the four
//! read-only tools (`read_file`, `list_dir`, `stat`, `search`).
//!
//! A [`ToolContext`] is bound to one workspace root at construction. Every
//! path a tool touches goes through [`ToolContext::resolve`], which joins,
//! canonicalizes (so symlinks resolve to their real targets), and rejects
//! anything outside the root — including `..` traversal, absolute paths
//! elsewhere, and symlinks pointing out.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ToolError;

/// Permission tier of a tool. Phase 5 ships read-only tools only, which are
/// allowed in both Plan and Build modes (Plan forbids *mutation*; reading
/// is never a mutation). Mutating tiers arrive with mutating tools, which
/// must additionally consult the session mode before acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    ReadOnly,
}

impl Permission {
    pub fn as_str(self) -> &'static str {
        match self {
            Permission::ReadOnly => "read-only",
        }
    }
}

/// Static catalog entry backing `/tools` and any future registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolInfo {
    pub name: &'static str,
    pub description: &'static str,
    pub permission: Permission,
}

/// The read-only core toolset, in stable order.
pub fn tool_catalog() -> Vec<ToolInfo> {
    vec![
        ToolInfo {
            name: "read_file",
            description: "Read a text file with line offset/limit (bounded).",
            permission: Permission::ReadOnly,
        },
        ToolInfo {
            name: "list_dir",
            description: "List directory entries, optionally recursive to a bounded depth.",
            permission: Permission::ReadOnly,
        },
        ToolInfo {
            name: "stat",
            description: "Report kind, size, and modification time of one path.",
            permission: Permission::ReadOnly,
        },
        ToolInfo {
            name: "search",
            description: "Substring search over workspace text files (bounded matches).",
            permission: Permission::ReadOnly,
        },
    ]
}

/// Resource bounds honored by every tool. The defaults keep single calls
/// cheap; callers may construct tighter [`Limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Max bytes read from one file by `read_file`.
    pub max_read_bytes: u64,
    /// Max bytes written by one mutation (`create`/`edit`/`overwrite`
    /// payloads, prior snapshots, deleted-file restore).
    pub max_write_bytes: u64,
    /// Max entries returned by one `list_dir` call.
    pub max_list_entries: usize,
    /// Max recursion depth for `list_dir`/`search` (1 = immediate children).
    pub max_depth: u32,
    /// Max matches returned by one `search` call.
    pub max_search_matches: usize,
    /// Files larger than this are skipped by `search`.
    pub max_search_file_bytes: u64,
    /// Match preview lines are clipped to this many characters.
    pub max_match_line_chars: usize,
    /// Max wall-clock time for one shell command execution.
    pub max_command_secs: u64,
    /// Max bytes captured per command output stream (stdout/stderr each);
    /// the rest is drained and discarded so big output cannot deadlock.
    pub max_command_output_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_read_bytes: 256 * 1024,
            max_write_bytes: 256 * 1024,
            max_list_entries: 500,
            max_depth: 8,
            max_search_matches: 100,
            max_search_file_bytes: 256 * 1024,
            max_match_line_chars: 240,
            max_command_secs: 60,
            max_command_output_bytes: 1024 * 1024,
        }
    }
}

/// Directory names never descended into by [`ToolContext::search`].
/// Version-control metadata and build output are noise for code search;
/// everything else (including hidden files) is searched.
const SEARCH_SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".hg", ".svn"];

/// Inspection context bound to one workspace root.
#[derive(Debug, Clone)]
pub struct ToolContext {
    root: PathBuf,
    limits: Limits,
}

impl ToolContext {
    /// Bind to `root`, canonicalized once. The root must exist and be a
    /// directory; anything else is [`ToolError::InvalidPath`].
    pub fn new(root: &Path) -> Result<Self, ToolError> {
        Self::with_limits(root, Limits::default())
    }

    pub fn with_limits(root: &Path, limits: Limits) -> Result<Self, ToolError> {
        let real = root.canonicalize().map_err(|_| {
            ToolError::InvalidPath(format!(
                "workspace root is not a usable directory: '{}'",
                root.display()
            ))
        })?;
        if !real.is_dir() {
            return Err(ToolError::InvalidPath(format!(
                "workspace root is not a directory: '{}'",
                root.display()
            )));
        }
        Ok(ToolContext { root: real, limits })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Resolve caller-supplied `user` to an absolute path inside the root.
    ///
    /// Relative paths join onto the root; absolute paths are accepted only
    /// when they land inside it. The nearest existing ancestor is
    /// canonicalized (resolving every symlink on the way) and the remainder
    /// re-appended, so `..` segments, absolute escapes, and symlinks
    /// pointing out all fail closed with [`ToolError::OutsideWorkspace`].
    /// Missing targets resolve fine (callers then report `NotFound`);
    /// there is no tilde expansion.
    pub fn resolve(&self, user: &str) -> Result<PathBuf, ToolError> {
        if user.is_empty() {
            return Err(ToolError::InvalidPath("path must not be empty".into()));
        }
        if user.contains('\0') {
            return Err(ToolError::InvalidPath("path contains a NUL byte".into()));
        }
        let requested = Path::new(user);
        let joined = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };
        // Walk up to the nearest existing ancestor; `file_name` never
        // yields `.` or `..`, so stripped components are plain names.
        let mut missing: Vec<std::ffi::OsString> = Vec::new();
        let mut anchor = joined.as_path();
        loop {
            if anchor.exists() {
                break;
            }
            match (anchor.file_name(), anchor.parent()) {
                (Some(name), Some(parent)) => {
                    missing.push(name.to_os_string());
                    anchor = parent;
                }
                _ => {
                    return Err(ToolError::NotFound {
                        path: user.to_string(),
                    });
                }
            }
        }
        let mut real = anchor.canonicalize().map_err(|e| ToolError::Io {
            path: user.to_string(),
            message: e.to_string(),
        })?;
        for component in missing.iter().rev() {
            real.push(component);
        }
        if !real.starts_with(&self.root) {
            return Err(ToolError::OutsideWorkspace {
                path: user.to_string(),
            });
        }
        Ok(real)
    }

    /// Read a text file: lines `[offset, offset + limit)`, 0-based offset.
    /// At most [`Limits::max_read_bytes`] are read; larger files set
    /// `truncated`. NUL bytes mean binary ([`ToolError::BinaryFile`]).
    pub fn read_file(&self, path: &str, offset: u64, limit: u64) -> Result<Readout, ToolError> {
        if limit == 0 {
            return Err(ToolError::InvalidPath("limit must be at least 1".into()));
        }
        let real = self.resolve(path)?;
        let meta = fs::symlink_metadata(&real).map_err(|e| map_missing(&real, path, e))?;
        if !meta.file_type().is_file() {
            return Err(ToolError::NotFile {
                path: display(&real),
            });
        }
        let file = fs::File::open(&real).map_err(|e| ToolError::Io {
            path: display(&real),
            message: e.to_string(),
        })?;
        let mut buffer = Vec::new();
        use std::io::Read;
        file.take(self.limits.max_read_bytes + 1)
            .read_to_end(&mut buffer)
            .map_err(|e| ToolError::Io {
                path: display(&real),
                message: e.to_string(),
            })?;
        let truncated = buffer.len() as u64 > self.limits.max_read_bytes;
        buffer.truncate(self.limits.max_read_bytes as usize);
        if buffer.contains(&0) {
            return Err(ToolError::BinaryFile {
                path: display(&real),
            });
        }
        let text = String::from_utf8_lossy(&buffer);
        let lines: Vec<&str> = text.split('\n').collect();
        let total_lines = lines.len() as u64;
        let selected: Vec<&str> = lines
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .collect();
        Ok(Readout {
            path: display(&real),
            content: selected.join("\n"),
            truncated,
            total_lines,
        })
    }

    /// List directory entries, `depth` levels deep (1 = immediate children).
    /// Entries sort by name; the walk stops at [`Limits::max_list_entries`]
    /// with `truncated` set. Symlinks are listed, never descended into;
    /// unreadable subdirectories are skipped.
    pub fn list_dir(&self, path: &str, depth: u32) -> Result<DirListing, ToolError> {
        if depth == 0 {
            return Err(ToolError::InvalidPath("depth must be at least 1".into()));
        }
        if depth > self.limits.max_depth {
            return Err(ToolError::InvalidPath(format!(
                "depth {depth} exceeds the limit of {}",
                self.limits.max_depth
            )));
        }
        let real = self.resolve(path)?;
        if !real.is_dir() {
            if !real.exists() {
                return Err(ToolError::NotFound {
                    path: display(&real),
                });
            }
            return Err(ToolError::NotDirectory {
                path: display(&real),
            });
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut stack = vec![(real.clone(), 1u32)];
        while let Some((dir, level)) = stack.pop() {
            let read = match fs::read_dir(&dir) {
                Ok(read) => read,
                Err(_) => continue,
            };
            let mut children: Vec<PathBuf> = Vec::new();
            for entry in read.flatten() {
                children.push(entry.path());
            }
            children.sort_by_key(|child| file_name(child));
            for child in children {
                if entries.len() >= self.limits.max_list_entries {
                    truncated = true;
                    break;
                }
                let file_type = match fs::symlink_metadata(&child) {
                    Ok(meta) => meta.file_type(),
                    Err(_) => continue,
                };
                let kind = if file_type.is_symlink() {
                    EntryKind::Symlink
                } else if file_type.is_dir() {
                    EntryKind::Dir
                } else if file_type.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                };
                let size_bytes = fs::symlink_metadata(&child).map(|m| m.len()).unwrap_or(0);
                entries.push(DirEntry {
                    path: child.display().to_string(),
                    name: file_name(&child),
                    kind,
                    size_bytes,
                });
                if kind == EntryKind::Dir && level < depth {
                    stack.push((child, level + 1));
                }
            }
            if truncated {
                break;
            }
        }
        Ok(DirListing {
            path: display(&real),
            entries,
            truncated,
        })
    }

    /// Report kind, size, and modification time of one path.
    pub fn stat(&self, path: &str) -> Result<FileStat, ToolError> {
        let real = self.resolve(path)?;
        let meta = fs::symlink_metadata(&real).map_err(|e| map_missing(&real, path, e))?;
        let file_type = meta.file_type();
        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Dir
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        let modified_secs = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        Ok(FileStat {
            path: display(&real),
            kind,
            size_bytes: meta.len(),
            modified_secs,
        })
    }

    /// Substring search over workspace text files under `opts.dir`.
    /// Skips [`SEARCH_SKIP_DIRS`], all symlinks, binary files, and files
    /// larger than [`Limits::max_search_file_bytes`]; stops at
    /// [`Limits::max_search_matches`] with `truncated` set. `should_cancel`
    /// (usually the request's [`CancelFlag`](aurel_model::CancelFlag)
    /// probe) is checked per entry; a set flag yields
    /// [`ToolError::Cancelled`].
    pub fn search(
        &self,
        pattern: &str,
        opts: &SearchOptions,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<SearchResults, ToolError> {
        if pattern.is_empty() {
            return Err(ToolError::InvalidPath(
                "search pattern must not be empty".into(),
            ));
        }
        let root = self.resolve(&opts.dir)?;
        if !root.is_dir() {
            if !root.exists() {
                return Err(ToolError::NotFound {
                    path: display(&root),
                });
            }
            return Err(ToolError::NotDirectory {
                path: display(&root),
            });
        }
        let needle = if opts.case_insensitive {
            pattern.to_lowercase()
        } else {
            pattern.to_string()
        };
        let extensions: Vec<String> = opts
            .extensions
            .iter()
            .map(|ext| ext.trim_start_matches('.').to_lowercase())
            .collect();
        let mut matches = Vec::new();
        let mut truncated = false;
        let mut stack = vec![(root, 0u32)];
        'walk: while let Some((dir, level)) = stack.pop() {
            if should_cancel.is_some_and(|cancel| cancel()) {
                return Err(ToolError::Cancelled);
            }
            let read = match fs::read_dir(&dir) {
                Ok(read) => read,
                Err(_) => continue,
            };
            let mut children: Vec<PathBuf> = Vec::new();
            for entry in read.flatten() {
                children.push(entry.path());
            }
            children.sort();
            for child in children {
                if matches.len() >= self.limits.max_search_matches {
                    truncated = true;
                    break 'walk;
                }
                if should_cancel.is_some_and(|cancel| cancel()) {
                    return Err(ToolError::Cancelled);
                }
                let file_type = match fs::symlink_metadata(&child) {
                    Ok(meta) => meta.file_type(),
                    Err(_) => continue,
                };
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    if level < self.limits.max_depth && !is_skipped_dir(&child) {
                        stack.push((child, level + 1));
                    }
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                if !extensions.is_empty() {
                    let actual = child
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .unwrap_or("")
                        .to_lowercase();
                    if !extensions.iter().any(|wanted| wanted == &actual) {
                        continue;
                    }
                }
                if let Some(found) =
                    search_file(&child, &needle, opts.case_insensitive, self.limits)?
                {
                    for item in found {
                        if matches.len() >= self.limits.max_search_matches {
                            truncated = true;
                            break 'walk;
                        }
                        matches.push(item);
                    }
                }
            }
        }
        Ok(SearchResults { matches, truncated })
    }
}

/// Map a filesystem error on a resolved path: missing → `NotFound`,
/// anything else → `Io`. (`user` is only a fallback label.)
fn map_missing(real: &Path, user: &str, error: std::io::Error) -> ToolError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ToolError::NotFound {
            path: display(real),
        }
    } else {
        ToolError::Io {
            path: user.to_string(),
            message: error.to_string(),
        }
    }
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn is_skipped_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SEARCH_SKIP_DIRS.contains(&name))
}

/// Search one file; `None` when the file is skipped (too large, binary).
/// Match lines are clipped to `limits.max_match_line_chars`.
fn search_file(
    path: &Path,
    needle: &str,
    case_insensitive: bool,
    limits: Limits,
) -> Result<Option<Vec<Match>>, ToolError> {
    let size = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size > limits.max_search_file_bytes {
        return Ok(None);
    }
    let bytes = fs::read(path).map_err(|e| ToolError::Io {
        path: display(path),
        message: e.to_string(),
    })?;
    if bytes.contains(&0) {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let haystack;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let matched = if case_insensitive {
            haystack = line.to_lowercase();
            haystack.contains(needle)
        } else {
            line.contains(needle)
        };
        if matched {
            found.push(Match {
                path: display(path),
                line_no: (index + 1) as u64,
                line: clip_chars(line, limits.max_match_line_chars),
            });
        }
    }
    Ok(Some(found))
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let clipped: String = text.chars().take(max_chars).collect();
        format!("{clipped}…")
    } else {
        text.to_string()
    }
}

/// One file read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readout {
    pub path: String,
    pub content: String,
    /// True when the file extended past [`Limits::max_read_bytes`].
    pub truncated: bool,
    /// Lines counted in the (possibly capped) buffer.
    pub total_lines: u64,
}

/// Directory entry kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One directory entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub path: String,
    pub name: String,
    pub kind: EntryKind,
    /// `lstat` size in bytes (meaningful for regular files).
    pub size_bytes: u64,
}

/// A directory listing, entries sorted by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirListing {
    pub path: String,
    pub entries: Vec<DirEntry>,
    /// True when [`Limits::max_list_entries`] cut the walk short.
    pub truncated: bool,
}

/// Kind/size/mtime of one path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    pub kind: EntryKind,
    pub size_bytes: u64,
    /// Seconds since the Unix epoch, when the platform reports one.
    pub modified_secs: Option<u64>,
}

/// Search scope: root directory, optional extension filter, case handling.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchOptions {
    /// Directory to search, relative to the workspace root (`"."` = root).
    pub dir: String,
    /// File extensions without dots (`["rs", "md"]`); empty means all files.
    pub extensions: Vec<String>,
    pub case_insensitive: bool,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub path: String,
    /// 1-based line number.
    pub line_no: u64,
    /// Line text, clipped to [`Limits::max_match_line_chars`].
    pub line: String,
}

/// Search matches in walk order with a truncation flag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchResults {
    pub matches: Vec<Match>,
    /// True when [`Limits::max_search_matches`] cut the walk short.
    pub truncated: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-tools-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create test root");
        dir
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parents");
        }
        let mut file = fs::File::create(path).expect("create file");
        file.write_all(content).expect("write file");
    }

    fn context(name: &str) -> (PathBuf, ToolContext) {
        let root = test_root(name);
        let context = ToolContext::new(&root).expect("context builds");
        (root, context)
    }

    fn default_opts(dir: &str) -> SearchOptions {
        SearchOptions {
            dir: dir.to_string(),
            extensions: Vec::new(),
            case_insensitive: false,
        }
    }

    #[test]
    fn catalog_lists_four_read_only_tools() {
        let catalog = tool_catalog();
        let names: Vec<&str> = catalog.iter().map(|tool| tool.name).collect();
        assert_eq!(names, ["read_file", "list_dir", "stat", "search"]);
        assert!(catalog
            .iter()
            .all(|tool| tool.permission == Permission::ReadOnly));
        assert!(catalog.iter().all(|tool| !tool.description.is_empty()));
    }

    #[test]
    fn resolve_rejects_traversal_and_outside_absolutes() {
        let (root, context) = context("resolve-jail");
        write(&root.join("inner.txt"), b"hi");
        assert!(context.resolve("inner.txt").is_ok());
        assert!(context.resolve(".").is_ok());
        assert!(matches!(
            context.resolve("../evil"),
            Err(ToolError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            context.resolve("sub/../../evil"),
            Err(ToolError::OutsideWorkspace { .. })
        ));
        let outside = test_root("resolve-outside");
        let absolute = outside.join("x.txt");
        write(&absolute, b"x");
        assert!(matches!(
            context.resolve(&absolute.display().to_string()),
            Err(ToolError::OutsideWorkspace { .. })
        ));
        // Absolute paths inside the root are fine.
        let inside = root.join("inner.txt");
        assert!(context.resolve(&inside.display().to_string()).is_ok());
        assert!(matches!(
            context.resolve(""),
            Err(ToolError::InvalidPath(_))
        ));
    }

    #[test]
    fn resolve_blocks_symlink_escape() {
        let (root, context) = context("resolve-symlink");
        let outside = test_root("resolve-symlink-outside");
        write(&outside.join("secret.txt"), b"secret");
        let link = root.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.txt"), &link).expect("symlink");
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_file(outside.join("secret.txt"), &link).is_err() {
                // Creating symlinks needs privileges Windows CI may lack;
                // without a link there is nothing to escape through.
                return;
            }
        }
        #[cfg(not(any(unix, windows)))]
        return;
        assert!(matches!(
            context.resolve("link.txt"),
            Err(ToolError::OutsideWorkspace { .. })
        ));
        assert!(matches!(
            context.read_file("link.txt", 0, 10),
            Err(ToolError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn read_file_offsets_limits_and_truncates() {
        let (root, context) = context("read");
        write(&root.join("a.txt"), b"one\ntwo\nthree\nfour\n");
        let readout = context.read_file("a.txt", 0, 10).expect("read");
        assert_eq!(readout.content, "one\ntwo\nthree\nfour\n");
        assert_eq!(readout.total_lines, 5);
        assert!(!readout.truncated);
        let readout = context.read_file("a.txt", 1, 2).expect("window");
        assert_eq!(readout.content, "two\nthree");
        let readout = context.read_file("a.txt", 99, 10).expect("past end");
        assert_eq!(readout.content, "");
        assert!(matches!(
            context.read_file("a.txt", 0, 0),
            Err(ToolError::InvalidPath(_))
        ));
        assert!(matches!(
            context.read_file("missing.txt", 0, 10),
            Err(ToolError::NotFound { .. })
        ));
        assert!(matches!(
            context.read_file(".", 0, 10),
            Err(ToolError::NotFile { .. })
        ));
        write(&root.join("bin.dat"), b"\x00\x01\x02");
        assert!(matches!(
            context.read_file("bin.dat", 0, 10),
            Err(ToolError::BinaryFile { .. })
        ));
    }

    #[test]
    fn read_file_truncates_at_byte_cap() {
        let (root, context) = context("read-cap");
        let big = vec![b'a'; 300 * 1024];
        write(&root.join("big.txt"), &big);
        let readout = context.read_file("big.txt", 0, u64::MAX).expect("read");
        assert!(readout.truncated);
        assert_eq!(readout.content.len(), 256 * 1024);
    }

    #[test]
    fn list_dir_sorts_and_bounds_depth() {
        let (root, context) = context("list");
        write(&root.join("b.txt"), b"b");
        write(&root.join("a.txt"), b"a");
        write(&root.join("sub").join("deep.txt"), b"d");
        let listing = context.list_dir(".", 1).expect("list");
        let names: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["a.txt", "b.txt", "sub"]);
        assert!(!listing.truncated);
        assert!(listing
            .entries
            .iter()
            .all(|e| e.kind == EntryKind::File || e.kind == EntryKind::Dir));
        let listing = context.list_dir(".", 2).expect("recursive");
        assert!(listing.entries.iter().any(|e| e.name == "deep.txt"));
        assert!(matches!(
            context.list_dir(".", 0),
            Err(ToolError::InvalidPath(_))
        ));
        assert!(matches!(
            context.list_dir("a.txt", 1),
            Err(ToolError::NotDirectory { .. })
        ));
        assert!(matches!(
            context.list_dir("missing", 1),
            Err(ToolError::NotFound { .. })
        ));
    }

    #[test]
    fn list_dir_caps_entries() {
        let (root, _) = context("list-cap");
        for i in 0..10 {
            write(&root.join(format!("f{i:02}.txt")), b"x");
        }
        let tight = Limits {
            max_list_entries: 3,
            ..Limits::default()
        };
        let context = ToolContext::with_limits(&root, tight).expect("context");
        let listing = context.list_dir(".", 1).expect("list");
        assert_eq!(listing.entries.len(), 3);
        assert!(listing.truncated);
    }

    #[test]
    fn stat_reports_kind_and_size() {
        let (root, context) = context("stat");
        write(&root.join("five.txt"), b"12345");
        fs::create_dir_all(root.join("sub")).expect("mkdir");
        let file = context.stat("five.txt").expect("stat file");
        assert_eq!(file.kind, EntryKind::File);
        assert_eq!(file.size_bytes, 5);
        assert!(file.modified_secs.is_some());
        let dir = context.stat(".").expect("stat dir");
        assert_eq!(dir.kind, EntryKind::Dir);
        assert!(matches!(
            context.stat("missing.txt"),
            Err(ToolError::NotFound { .. })
        ));
    }

    #[test]
    fn search_finds_matches_with_options() {
        let (root, context) = context("search");
        write(&root.join("a.rs"), b"fn alpha() {}\n// ALPHA note\n");
        write(&root.join("b.md"), b"alpha beta\n");
        write(&root.join("sub").join("c.rs"), b"nothing here\n");
        let results = context
            .search("alpha", &default_opts("."), None)
            .expect("search");
        assert!(!results.truncated);
        assert_eq!(results.matches.len(), 2);
        assert_eq!(results.matches[0].line_no, 1);
        assert!(results.matches[0].path.ends_with("a.rs"));
        // Extension filter.
        let mut opts = default_opts(".");
        opts.extensions = vec!["md".to_string()];
        let results = context.search("alpha", &opts, None).expect("filtered");
        assert_eq!(results.matches.len(), 1);
        assert!(results.matches[0].path.ends_with("b.md"));
        // Case-insensitive.
        let mut opts = default_opts(".");
        opts.case_insensitive = true;
        let results = context.search("ALPHA", &opts, None).expect("ci");
        assert_eq!(results.matches.len(), 3);
        // Empty pattern rejected.
        assert!(matches!(
            context.search("", &default_opts("."), None),
            Err(ToolError::InvalidPath(_))
        ));
    }

    #[test]
    fn search_skips_junk_and_caps_matches() {
        let (root, context) = context("search-skip");
        write(&root.join(".git").join("packed.txt"), b"needle\n");
        write(&root.join("target").join("out.txt"), b"needle\n");
        write(&root.join("bin.dat"), b"needle\x00more\n");
        write(&root.join("ok.txt"), b"a needle here\n");
        let results = context
            .search("needle", &default_opts("."), None)
            .expect("search");
        assert_eq!(results.matches.len(), 1);
        assert!(results.matches[0].path.ends_with("ok.txt"));

        let tight = Limits {
            max_search_matches: 2,
            ..Limits::default()
        };
        let root2 = test_root("search-cap");
        for i in 0..5 {
            write(&root2.join(format!("f{i}.txt")), b"needle\n");
        }
        let context = ToolContext::with_limits(&root2, tight).expect("context");
        let results = context
            .search("needle", &default_opts("."), None)
            .expect("search");
        assert_eq!(results.matches.len(), 2);
        assert!(results.truncated);
    }

    #[test]
    fn search_honors_cancellation() {
        let (root, context) = context("search-cancel");
        write(&root.join("a.txt"), b"needle\n");
        let err = context
            .search("needle", &default_opts("."), Some(&|| true))
            .expect_err("cancelled");
        assert_eq!(err, ToolError::Cancelled);
    }

    #[test]
    fn context_rejects_bad_root() {
        assert!(ToolContext::new(Path::new("")).is_err());
        let missing = std::env::temp_dir().join("aurel-tools-no-such-root-xyz");
        let _ = fs::remove_dir_all(&missing);
        assert!(ToolContext::new(&missing).is_err());
    }
}
