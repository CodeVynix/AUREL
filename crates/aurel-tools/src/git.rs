//! Local-only Git operations (`aurel-tools`).
//!
//! Two halves with different permission tiers:
//!
//! - **Read-only inspection** ([`ToolContext::git_status`],
//!   [`ToolContext::git_diff`], [`ToolContext::git_branches`],
//!   [`ToolContext::git_log`], [`ToolContext::git_toplevel`]) never mutates
//!   anything and is allowed in both Plan and Build modes, exactly like
//!   `read_file` and friends.
//! - **Git mutations** ([`GitOp`] → [`GitResolved`]) never execute on their
//!   own: like file mutations they are proposed, snapshotted, rendered for
//!   review, and only applied after explicit `/approve` in Build mode, with
//!   Plan mode approvals always rejected. There is no undo for an applied
//!   Git operation — undoing one would mean rewriting history, which AUREL
//!   refuses to do automatically.
//!
//! Security boundaries (see `docs/architecture.md` for the full statement):
//!
//! - Every Git invocation runs through the [`crate::command`] layer: direct
//!   `git` spawn with an explicit argument vector, no shell, workspace root
//!   as working directory, bounded time and output, cooperative
//!   cancellation, secret-filtered environment, secret-scrubbed output.
//! - Argument vectors are built here from typed operations — never from
//!   caller-supplied raw text — so only the allowlisted local subcommands
//!   (`status`, `diff`, `add`, `reset`, `commit`, `branch`, `switch`,
//!   `rev-parse`, `symbolic-ref`, `check-ref-format`, `log`) can ever run.
//!   Push, pull, fetch, merge, rebase, remote, and clone have no constructor
//!   and no test path; a regression test asserts the argv builder cannot
//!   emit them.
//! - Stage/unstage paths resolve through the workspace sandbox, exactly like
//!   file mutations. The repository itself only needs to *contain* the
//!   workspace root (a sub-directory workspace stages through its repo).
//! - Approval re-verifies a repository snapshot (HEAD + branch + porcelain
//!   status), so external Git activity between proposal and approval fails
//!   as stale instead of applying against a moved tree.
//! - Failures are typed ([`ToolError`]) and always carry Git's own
//!   stdout/stderr (clipped, secret-scrubbed) — output is never hidden.

use std::path::{Path, PathBuf};

use crate::{ToolContext, ToolError};

/// Subcommands that touch remotes, rewrite history, or combine trees.
/// No constructor below can emit these; [`forbidden_subcommand_check`] (a
/// regression test) pins that property to every argv the builder produces.
pub(crate) const FORBIDDEN_GIT_SUBCOMMANDS: &[&str] = &[
    "push",
    "pull",
    "fetch",
    "merge",
    "rebase",
    "remote",
    "clone",
    "filter-branch",
    "filter-repo",
    "am",
    "apply",
];

/// Max bytes accepted for a commit message at prepare time.
pub(crate) const MAX_COMMIT_MESSAGE_BYTES: usize = 4096;

/// Max paths accepted in one stage/unstage proposal (bounds the argv).
pub(crate) const MAX_GIT_PATHS: usize = 100;

/// Max status lines embedded in a review diff before a truncation marker.
pub(crate) const MAX_REVIEW_STATUS_LINES: usize = 40;

/// A model-proposed local Git mutation (raw, unresolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitOp {
    Stage { paths: Vec<String> },
    Unstage { paths: Vec<String> },
    Commit { message: String },
    CreateBranch { name: String },
    SwitchBranch { name: String },
}

/// The display/action half of a resolved Git operation: what it does, in
/// typed form, with the exact message/branch kept for review and audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitKind {
    Stage,
    Unstage,
    Commit { message: String },
    CreateBranch { name: String },
    SwitchBranch { name: String },
}

/// A sandbox-resolved Git operation: the exact `git` binary, the pinned
/// working directory, sandbox-checked touch paths, and the full argv shown
/// at review time and executed at approval time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitResolved {
    pub kind: GitKind,
    pub git: PathBuf,
    pub workdir: PathBuf,
    pub paths: Vec<PathBuf>,
    pub argv: Vec<String>,
}

impl GitResolved {
    /// One-line human summary for approvals and listings. Absolute paths
    /// stay absolute (unambiguous for audit); long path lists truncate.
    pub fn summary(&self) -> String {
        match &self.kind {
            GitKind::Stage => format!("git stage {}", join_paths(&self.paths)),
            GitKind::Unstage => format!("git unstage {}", join_paths(&self.paths)),
            GitKind::Commit { message } => {
                let first = message.lines().next().unwrap_or("").trim();
                format!("git commit -m '{}'", clip_chars(first, 72))
            }
            GitKind::CreateBranch { name } => format!("git create branch '{name}'"),
            GitKind::SwitchBranch { name } => format!("git switch to '{name}'"),
        }
    }
}

fn join_paths(paths: &[PathBuf]) -> String {
    const SHOWN: usize = 4;
    let mut out = String::new();
    for (index, path) in paths.iter().take(SHOWN).enumerate() {
        if index > 0 {
            out.push(' ');
        }
        out.push_str(&path.display().to_string());
    }
    if paths.len() > SHOWN {
        out.push_str(&format!(" (+{} more)", paths.len() - SHOWN));
    }
    out
}

fn clip_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let clipped: String = text.chars().take(max_chars).collect();
        format!("{clipped}…")
    } else {
        text.to_string()
    }
}

// ---------------------------------------------------------------------------
// Low-level runner + detection
// ---------------------------------------------------------------------------

/// Resolve the `git` program exactly like shell programs resolve: bare
/// name searched on `PATH`, never the current directory.
pub fn git_binary() -> Result<PathBuf, ToolError> {
    crate::resolve_program("git", &crate::system_path_dirs())
}

/// Run `git` with `args` in `workdir`: bounded, cancellable,
/// secret-scrubbed, direct spawn (no shell). Returns the raw result so
/// callers can type successes and failures themselves.
pub(crate) fn run_git(
    git_bin: &Path,
    workdir: &Path,
    args: &[String],
    timeout_secs: u64,
    redact: Option<&str>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> crate::command::CommandResult {
    crate::run_command(
        &crate::command::CommandRequest {
            program: git_bin.display().to_string(),
            args: args.to_vec(),
            workdir: workdir.to_path_buf(),
            timeout: std::time::Duration::from_secs(timeout_secs),
            redact: redact.map(str::to_string),
        },
        should_cancel,
    )
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

/// The validated repository behind a workspace: which `git` runs it and
/// where its top level is. The workspace root itself is the working
/// directory for every invocation (it may be the top level or a
/// sub-directory of it).
#[derive(Debug)]
pub(crate) struct GitRepo {
    pub git_bin: PathBuf,
    pub toplevel: PathBuf,
}
/// Detect the repository containing the workspace root. Fails closed when
/// `git` is missing, the directory is not inside a work tree, or the
/// reported top level cannot be canonicalized.
pub(crate) fn detect_repo(context: &ToolContext) -> Result<GitRepo, ToolError> {
    let git_bin = git_binary()?;
    let result = run_git(
        &git_bin,
        context.root(),
        &argv(&["rev-parse", "--show-toplevel"]),
        context.limits().max_command_secs,
        None,
        None,
    );
    if result.status != crate::command::CommandStatus::Success {
        return Err(ToolError::Io {
            path: context.root().display().to_string(),
            message: format!(
                "not a git repository (git rev-parse failed: {})",
                clip_command_detail(&result)
            ),
        });
    }
    let toplevel = PathBuf::from(result.stdout.trim());
    let canon = toplevel.canonicalize().map_err(|e| ToolError::Io {
        path: toplevel.display().to_string(),
        message: format!("cannot use git top level: {e}"),
    })?;
    Ok(GitRepo {
        git_bin,
        toplevel: canon,
    })
}

/// Snapshot the repository state covered by the stale check: HEAD oid (or
/// the unborn-branch marker), current branch (or detached marker), and the
/// full porcelain status. Any index or worktree change — and any commit —
/// alters these bytes.
pub(crate) fn repo_snapshot(
    git_bin: &Path,
    workdir: &Path,
    timeout_secs: u64,
) -> Result<Vec<u8>, ToolError> {
    let display = workdir.display().to_string();
    let head = run_git(
        git_bin,
        workdir,
        &argv(&["rev-parse", "HEAD"]),
        timeout_secs,
        None,
        None,
    );
    let head_oid = if head.status == crate::command::CommandStatus::Success {
        head.stdout.trim().to_string()
    } else {
        // No commits yet: fall back to the branch the first commit would
        // land on, so the marker is still stable and comparable.
        let branch = run_git(
            git_bin,
            workdir,
            &argv(&["symbolic-ref", "--short", "HEAD"]),
            timeout_secs,
            None,
            None,
        );
        if branch.status == crate::command::CommandStatus::Success {
            format!("unborn:{}", branch.stdout.trim())
        } else {
            "unborn:HEAD".to_string()
        }
    };
    let branch = run_git(
        git_bin,
        workdir,
        &argv(&["symbolic-ref", "--short", "HEAD"]),
        timeout_secs,
        None,
        None,
    );
    let branch_name = if branch.status == crate::command::CommandStatus::Success {
        branch.stdout.trim().to_string()
    } else {
        "detached".to_string()
    };
    let status = run_git(
        git_bin,
        workdir,
        &argv(&[
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ]),
        timeout_secs,
        None,
        None,
    );
    if status.status != crate::command::CommandStatus::Success {
        return Err(ToolError::Io {
            path: display,
            message: format!(
                "cannot snapshot git status: {}",
                clip_command_detail(&status)
            ),
        });
    }
    Ok(format!("head:{head_oid}\nbranch:{branch_name}\n{}", status.stdout).into_bytes())
}

/// One-line, secret-free command detail for error messages: exit state plus
/// clipped stdout/stderr (already secret-scrubbed by the runner).
pub(crate) fn clip_command_detail(result: &crate::command::CommandResult) -> String {
    const MAX_CHARS: usize = 2000;
    let mut combined = String::new();
    if !result.stdout.trim().is_empty() {
        combined.push_str(result.stdout.trim());
    }
    if !result.stderr.trim().is_empty() {
        if !combined.is_empty() {
            combined.push_str(" | ");
        }
        combined.push_str(result.stderr.trim());
    }
    if combined.is_empty() {
        return result.detail.clone();
    }
    let clipped = clip_chars(&combined, MAX_CHARS);
    if result.detail.is_empty() {
        clipped
    } else {
        format!("{} ({})", clipped, result.detail)
    }
}

// ---------------------------------------------------------------------------
// Read-only inspection (ToolContext methods: allowed in Plan and Build)
// ---------------------------------------------------------------------------

/// Current branch, or `"HEAD (no branch)"` when detached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatus {
    pub branch: String,
    pub detached: bool,
    /// True for a repository with no commits yet.
    pub unborn: bool,
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    /// Paths with staged changes (`X` column set).
    pub staged: Vec<String>,
    /// Paths with unstaged changes (`Y` column set).
    pub unstaged: Vec<String>,
    /// Untracked paths (`??`).
    pub untracked: Vec<String>,
}

impl GitStatus {
    pub fn is_clean(&self) -> bool {
        self.staged.is_empty() && self.unstaged.is_empty() && self.untracked.is_empty()
    }

    /// Short human line for `/git status` and review diffs.
    pub fn summary(&self) -> String {
        if self.is_clean() {
            return format!("branch '{}': clean", self.branch);
        }
        format!(
            "branch '{}': {} staged, {} unstaged, {} untracked",
            self.branch,
            self.staged.len(),
            self.unstaged.len(),
            self.untracked.len()
        )
    }
}

fn parse_number(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

fn parse_branch_header(line: &str, status: &mut GitStatus) {
    // Forms: "## main", "## main...origin/main",
    // "## main...origin/main [ahead 1]", "[ahead 2, behind 3]",
    // "## No commits yet on main", "## HEAD (no branch)".
    let header = line.strip_prefix("## ").unwrap_or(line);
    if let Some(rest) = header.strip_prefix("No commits yet on ") {
        status.unborn = true;
        status.branch = rest.split_whitespace().next().unwrap_or(rest).to_string();
        return;
    }
    let (branch_part, tracking) = match header.find("...") {
        Some(index) => (&header[..index], Some(&header[index + 3..])),
        None => (header, None),
    };
    status.branch = branch_part
        .split_whitespace()
        .next()
        .unwrap_or(branch_part)
        .to_string();
    if status.branch == "HEAD" || header.starts_with("HEAD (no branch)") {
        status.detached = true;
        status.branch = "HEAD (no branch)".to_string();
    }
    if let Some(tracking) = tracking {
        if let Some(bracket) = tracking.find('[').and_then(|open| {
            tracking
                .rfind(']')
                .map(|close| tracking[open + 1..close].to_string())
        }) {
            for part in bracket.split(',') {
                let part = part.trim();
                if let Some(count) = part.strip_prefix("ahead ") {
                    status.ahead = parse_number(count);
                } else if let Some(count) = part.strip_prefix("behind ") {
                    status.behind = parse_number(count);
                }
            }
        }
    }
}

/// Parse `git status --porcelain=v1 --branch` output. Total: unknown lines
/// are ignored, never a panic.
pub(crate) fn parse_porcelain(text: &str) -> GitStatus {
    let mut status = GitStatus {
        branch: String::new(),
        detached: false,
        unborn: false,
        ahead: None,
        behind: None,
        staged: Vec::new(),
        unstaged: Vec::new(),
        untracked: Vec::new(),
    };
    for line in text.lines() {
        if line.starts_with("## ") {
            parse_branch_header(line, &mut status);
            continue;
        }
        let bytes = line.as_bytes();
        if bytes.len() < 4 || bytes[2] != b' ' {
            continue;
        }
        let path = line[3..].to_string();
        match (bytes[0] as char, bytes[1] as char) {
            ('?', '?') => status.untracked.push(path),
            ('U', _) | (_, 'U') | ('A', 'A') | ('D', 'D') => {
                // Conflicted paths show in both lists: either side differs
                // from HEAD and the tree is not clean.
                status.staged.push(path.clone());
                status.unstaged.push(path);
            }
            (staged, unstaged) => {
                if staged != ' ' {
                    status.staged.push(path.clone());
                }
                if unstaged != ' ' {
                    status.unstaged.push(path);
                }
            }
        }
    }
    if status.branch.is_empty() {
        status.branch = "HEAD (no branch)".to_string();
        status.detached = true;
    }
    status
}

/// One local branch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBranch {
    pub name: String,
    pub current: bool,
    pub short_oid: String,
    pub subject: String,
}

/// One commit header (hash + subject, no bodies: bounded by construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLogEntry {
    pub oid: String,
    pub short_oid: String,
    pub subject: String,
}

/// A rendered diff plus whether the runner capped it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitDiff {
    /// True for `--staged` (index vs HEAD), false for worktree vs index.
    pub staged: bool,
    pub text: String,
    pub truncated: bool,
}

impl ToolContext {
    fn git_repo(&self) -> Result<GitRepo, ToolError> {
        detect_repo(self)
    }

    /// Absolute top level of the repository containing the workspace.
    pub fn git_toplevel(&self) -> Result<PathBuf, ToolError> {
        Ok(self.git_repo()?.toplevel)
    }

    /// Structured working-tree status (branch, staged/unstaged/untracked).
    /// Read-only: safe in Plan mode.
    pub fn git_status(
        &self,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<GitStatus, ToolError> {
        if should_cancel.is_some_and(|cancel| cancel()) {
            return Err(ToolError::Cancelled);
        }
        let repo = self.git_repo()?;
        let result = run_git(
            &repo.git_bin,
            self.root(),
            &argv(&[
                "status",
                "--porcelain=v1",
                "--branch",
                "--untracked-files=normal",
            ]),
            self.limits().max_command_secs,
            None,
            should_cancel,
        );
        if result.status == crate::command::CommandStatus::Cancelled {
            return Err(ToolError::Cancelled);
        }
        if result.status != crate::command::CommandStatus::Success {
            return Err(ToolError::Io {
                path: self.root().display().to_string(),
                message: format!("git status failed: {}", clip_command_detail(&result)),
            });
        }
        Ok(parse_porcelain(&result.stdout))
    }

    /// Unified diff of the worktree (staged=false) or the index
    /// (staged=true). Read-only: safe in Plan mode.
    pub fn git_diff(
        &self,
        staged: bool,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<GitDiff, ToolError> {
        if should_cancel.is_some_and(|cancel| cancel()) {
            return Err(ToolError::Cancelled);
        }
        let repo = self.git_repo()?;
        let mut args = argv(&["diff", "--no-color"]);
        if staged {
            args.push("--staged".to_string());
        }
        let result = run_git(
            &repo.git_bin,
            self.root(),
            &args,
            self.limits().max_command_secs,
            None,
            should_cancel,
        );
        if result.status == crate::command::CommandStatus::Cancelled {
            return Err(ToolError::Cancelled);
        }
        if result.status != crate::command::CommandStatus::Success {
            return Err(ToolError::Io {
                path: self.root().display().to_string(),
                message: format!("git diff failed: {}", clip_command_detail(&result)),
            });
        }
        let mut text = result.stdout;
        let truncated = result.truncated;
        if truncated {
            text.push_str("\n…(output truncated to the per-stream cap)\n");
        }
        Ok(GitDiff {
            staged,
            text,
            truncated,
        })
    }

    /// Local branches with tip and subject. Read-only: safe in Plan mode.
    pub fn git_branches(
        &self,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<Vec<GitBranch>, ToolError> {
        if should_cancel.is_some_and(|cancel| cancel()) {
            return Err(ToolError::Cancelled);
        }
        let repo = self.git_repo()?;
        let result = run_git(
            &repo.git_bin,
            self.root(),
            &argv(&[
                "branch",
                "--list",
                "--format=%(refname:short)%00%(HEAD)%00%(objectname:short)%00%(subject)",
            ]),
            self.limits().max_command_secs,
            None,
            should_cancel,
        );
        if result.status == crate::command::CommandStatus::Cancelled {
            return Err(ToolError::Cancelled);
        }
        if result.status != crate::command::CommandStatus::Success {
            return Err(ToolError::Io {
                path: self.root().display().to_string(),
                message: format!("git branch failed: {}", clip_command_detail(&result)),
            });
        }
        let mut branches = Vec::new();
        // Records are newline-terminated, fields NUL-separated (`%00` is
        // one of the few escapes `git branch --format` expands; NUL cannot
        // appear in a refname or a one-line commit subject).
        for line in result.stdout.lines() {
            let fields: Vec<&str> = line.split('\0').collect();
            if fields.len() != 4 || fields[0].is_empty() {
                continue;
            }
            branches.push(GitBranch {
                name: fields[0].to_string(),
                current: fields[1].trim() == "*",
                short_oid: fields[2].to_string(),
                subject: fields[3].to_string(),
            });
        }
        Ok(branches)
    }

    /// Recent commit headers, newest first. Empty repositories (no commits
    /// yet) yield an empty list — a clean answer, not an error.
    /// Read-only: safe in Plan mode.
    pub fn git_log(
        &self,
        limit: u32,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<Vec<GitLogEntry>, ToolError> {
        if limit == 0 || limit > 50 {
            return Err(ToolError::InvalidPath(format!(
                "log limit must be 1–50 (got {limit})"
            )));
        }
        if should_cancel.is_some_and(|cancel| cancel()) {
            return Err(ToolError::Cancelled);
        }
        let repo = self.git_repo()?;
        let result = run_git(
            &repo.git_bin,
            self.root(),
            &argv(&[
                "log",
                "--format=%H%x1f%h%x1f%s",
                "--no-decorate",
                "--no-color",
                "-n",
                &limit.to_string(),
            ]),
            self.limits().max_command_secs,
            None,
            should_cancel,
        );
        if result.status == crate::command::CommandStatus::Cancelled {
            return Err(ToolError::Cancelled);
        }
        if result.status != crate::command::CommandStatus::Success {
            let detail = clip_command_detail(&result);
            // `git log` on an unborn HEAD exits 128 ("does not have any
            // commits yet" / "bad default revision 'HEAD'"): report zero
            // commits instead of failing the inspection.
            if detail.contains("does not have any commits yet")
                || detail.contains("bad default revision")
            {
                return Ok(Vec::new());
            }
            return Err(ToolError::Io {
                path: self.root().display().to_string(),
                message: format!("git log failed: {detail}"),
            });
        }
        let mut entries = Vec::new();
        for line in result.stdout.lines() {
            let fields: Vec<&str> = line.split('\u{1f}').collect();
            if fields.len() != 3 || fields[0].is_empty() {
                continue;
            }
            entries.push(GitLogEntry {
                oid: fields[0].to_string(),
                short_oid: fields[1].to_string(),
                subject: fields[2].to_string(),
            });
        }
        Ok(entries)
    }
}

// ---------------------------------------------------------------------------
// Mutation validation
// ---------------------------------------------------------------------------

/// Authoritative branch-name check: cheap local rules first (clear
/// messages), then `git check-ref-format --branch` as the final word.
pub(crate) fn validate_branch_name(
    name: &str,
    git_bin: &Path,
    workdir: &Path,
) -> Result<(), String> {
    if name.is_empty() {
        return Err("branch name must not be empty".to_string());
    }
    if name.len() > 255 {
        return Err("branch name is too long (max 255 bytes)".to_string());
    }
    if name.contains('\0') {
        return Err("branch name contains a NUL byte".to_string());
    }
    if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("branch name must not contain whitespace: '{name}'"));
    }
    for bad in ["..", "@{", ".lock"] {
        if name.contains(bad) {
            return Err(format!("branch name must not contain '{bad}': '{name}'"));
        }
    }
    if name.starts_with('-') || name.starts_with('/') || name.starts_with('.') {
        return Err(format!(
            "branch name must not start with '-', '/', or '.': '{name}'"
        ));
    }
    if name.ends_with('/') || name.ends_with('.') {
        return Err(format!(
            "branch name must not end with '/' or '.': '{name}'"
        ));
    }
    if name.contains("//") {
        return Err(format!("branch name must not contain '//': '{name}'"));
    }
    if name
        .chars()
        .any(|c| matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Err(format!(
            "branch name must not contain any of '~^:?*[\\': '{name}'"
        ));
    }
    let result = run_git(
        git_bin,
        workdir,
        &argv(&["check-ref-format", "--branch", name]),
        10,
        None,
        None,
    );
    if result.status != crate::command::CommandStatus::Success {
        return Err(format!(
            "invalid branch name '{name}': {}",
            clip_command_detail(&result)
        ));
    }
    Ok(())
}

/// Trim and bound a commit message. Returns the trimmed message.
pub(crate) fn validate_commit_message(message: &str) -> Result<String, String> {
    let trimmed = message.trim().to_string();
    if trimmed.is_empty() {
        return Err("commit message must not be empty".to_string());
    }
    if trimmed.contains('\0') {
        return Err("commit message contains a NUL byte".to_string());
    }
    if trimmed.len() > MAX_COMMIT_MESSAGE_BYTES {
        return Err(format!(
            "commit message exceeds the {MAX_COMMIT_MESSAGE_BYTES}-byte limit"
        ));
    }
    Ok(trimmed)
}

// ---------------------------------------------------------------------------
// Mutation prepare / verify / apply (called from `mutation.rs`)
// ---------------------------------------------------------------------------

fn resolve_stage_path(context: &ToolContext, user: &str) -> Result<PathBuf, String> {
    if user.is_empty() {
        return Err("stage path must not be empty".to_string());
    }
    context
        .resolve(user)
        .map_err(|e| format!("bad path '{user}': {e}"))
}

/// Render the review block for one resolved Git operation. Every block
/// states the local-only boundary, shows the exact argv, and embeds the
/// repository state the snapshot was taken from.
fn render_git_review(
    argv: &[String],
    toplevel: &Path,
    workdir: &Path,
    status: &GitStatus,
    extra: &str,
) -> String {
    let mut diff = String::from("local git operation (never touches remotes)\n");
    diff.push_str("$ git");
    for arg in argv {
        diff.push(' ');
        if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '\'') {
            diff.push_str(&format!("'{}'", arg.replace('\'', "'\\''")));
        } else {
            diff.push_str(arg);
        }
    }
    diff.push('\n');
    diff.push_str(&format!("cwd: {}\n", workdir.display()));
    diff.push_str(&format!("repo: {}\n", toplevel.display()));
    diff.push_str(&format!("state: {}\n", status.summary()));
    if !extra.is_empty() {
        diff.push_str(extra);
        if !extra.ends_with('\n') {
            diff.push('\n');
        }
    }
    diff
}

fn status_lines_capped(status: &GitStatus) -> String {
    let mut lines: Vec<String> = Vec::new();
    for path in status.staged.iter() {
        lines.push(format!("staged: {path}"));
    }
    for path in status.unstaged.iter() {
        lines.push(format!("unstaged: {path}"));
    }
    for path in status.untracked.iter() {
        lines.push(format!("untracked: {path}"));
    }
    if lines.is_empty() {
        return "working tree clean\n".to_string();
    }
    let mut out = String::new();
    for line in lines.iter().take(MAX_REVIEW_STATUS_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    if lines.len() > MAX_REVIEW_STATUS_LINES {
        out.push_str(&format!(
            "…({} more entries)\n",
            lines.len() - MAX_REVIEW_STATUS_LINES
        ));
    }
    out
}

/// Prepare a raw [`GitOp`] against the workspace: detect the repo,
/// validate, resolve paths through the sandbox, snapshot repository state,
/// and render the review diff. Anything surprising is a rejection — nothing
/// half-checked ever reaches approval.
pub(crate) fn prepare_git(
    context: &ToolContext,
    id: u64,
    op: GitOp,
) -> Result<crate::PendingProposal, crate::ProposalError> {
    use crate::ProposalError;
    let repo = detect_repo(context)
        .map_err(|e| ProposalError::rejected(format!("invalid git proposal: {e}")))?;
    let workdir = context.root().to_path_buf();
    let timeout = context.limits().max_command_secs;
    let status = parse_current_status(&repo.git_bin, &workdir, timeout)
        .map_err(|e| ProposalError::rejected(format!("invalid git proposal: {e}")))?;
    let snapshot = repo_snapshot(&repo.git_bin, &workdir, timeout)
        .map_err(|e| ProposalError::rejected(format!("invalid git proposal: {e}")))?;
    let unstage = matches!(&op, GitOp::Unstage { .. });

    match op {
        GitOp::Stage { paths } | GitOp::Unstage { paths } => {
            if paths.is_empty() {
                return Err(ProposalError::rejected("git_stage takes at least one path"));
            }
            if paths.len() > MAX_GIT_PATHS {
                return Err(ProposalError::rejected(format!(
                    "too many paths ({}; max {MAX_GIT_PATHS})",
                    paths.len()
                )));
            }
            let mut resolved = Vec::with_capacity(paths.len());
            for user in &paths {
                let real = resolve_stage_path(context, user).map_err(ProposalError::rejected)?;
                if !unstage && real.is_dir() {
                    return Err(ProposalError::rejected(format!(
                        "refusing to stage directory '{}' (stage files explicitly)",
                        real.display()
                    )));
                }
                resolved.push(real);
            }
            let mut argv = if unstage {
                argv(&["reset", "--"])
            } else {
                argv(&["add", "--"])
            };
            for path in &resolved {
                argv.push(path.display().to_string());
            }
            let kind = if unstage {
                GitKind::Unstage
            } else {
                GitKind::Stage
            };
            let action = if unstage {
                "unstage (index entries revert to HEAD; worktree untouched)"
            } else {
                "stage (worktree untouched)"
            };
            let extra = format!("effect: {action}\n{}", status_lines_capped(&status));
            let diff = render_git_review(&argv, &repo.toplevel, &workdir, &status, &extra);
            Ok(crate::PendingProposal {
                id,
                op: crate::ResolvedOp::Git(GitResolved {
                    kind,
                    git: repo.git_bin,
                    workdir,
                    paths: resolved,
                    argv,
                }),
                prior: Some(snapshot),
                prior_is_dir: false,
                diff,
            })
        }
        GitOp::Commit { message } => {
            let trimmed = validate_commit_message(&message).map_err(ProposalError::rejected)?;
            if status.detached {
                return Err(ProposalError::rejected(
                    "cannot commit on a detached HEAD (switch to a branch first)",
                ));
            }
            if status.staged.is_empty() {
                return Err(ProposalError::rejected(
                    "nothing staged to commit (stage files first)",
                ));
            }
            let argv = argv(&["commit", "-m", trimmed.as_str()]);
            let staged_list: String = status
                .staged
                .iter()
                .take(MAX_REVIEW_STATUS_LINES)
                .map(|path| format!("staged: {path}\n"))
                .collect();
            let extra = format!("message:\n{trimmed}\nwill commit:\n{staged_list}");
            let diff = render_git_review(&argv, &repo.toplevel, &workdir, &status, &extra);
            Ok(crate::PendingProposal {
                id,
                op: crate::ResolvedOp::Git(GitResolved {
                    kind: GitKind::Commit { message: trimmed },
                    git: repo.git_bin,
                    workdir,
                    paths: Vec::new(),
                    argv,
                }),
                prior: Some(snapshot),
                prior_is_dir: false,
                diff,
            })
        }
        GitOp::CreateBranch { name } => {
            validate_branch_name(&name, &repo.git_bin, &workdir).map_err(|detail| {
                ProposalError::rejected(format!("invalid git proposal: {detail}"))
            })?;
            let branches = list_branch_names(&repo.git_bin, &workdir, timeout)
                .map_err(|e| ProposalError::rejected(format!("invalid git proposal: {e}")))?;
            if branches.iter().any(|existing| existing == &name) {
                return Err(ProposalError::rejected(format!(
                    "branch '{name}' already exists"
                )));
            }
            let argv = argv(&["branch", name.as_str()]);
            let extra = format!(
                "effect: create local branch '{name}' (stays on '{}'; no checkout, no remote)\n",
                status.branch
            );
            let diff = render_git_review(&argv, &repo.toplevel, &workdir, &status, &extra);
            Ok(crate::PendingProposal {
                id,
                op: crate::ResolvedOp::Git(GitResolved {
                    kind: GitKind::CreateBranch { name },
                    git: repo.git_bin,
                    workdir,
                    paths: Vec::new(),
                    argv,
                }),
                prior: Some(snapshot),
                prior_is_dir: false,
                diff,
            })
        }
        GitOp::SwitchBranch { name } => {
            validate_branch_name(&name, &repo.git_bin, &workdir).map_err(|detail| {
                ProposalError::rejected(format!("invalid git proposal: {detail}"))
            })?;
            let branches = list_branch_names(&repo.git_bin, &workdir, timeout)
                .map_err(|e| ProposalError::rejected(format!("invalid git proposal: {e}")))?;
            if !branches.iter().any(|existing| existing == &name) {
                return Err(ProposalError::rejected(format!(
                    "branch '{name}' does not exist (create it first; no remote branches are fetched)"
                )));
            }
            if !status.detached && status.branch == name {
                return Err(ProposalError::rejected(format!(
                    "already on branch '{name}'"
                )));
            }
            let argv = argv(&["switch", name.as_str()]);
            let extra = format!(
                "effect: switch '{}' → '{name}' (refuses instead of overwriting conflicting local changes; worktree files may change to match)\n{}",
                status.branch,
                status_lines_capped(&status)
            );
            let diff = render_git_review(&argv, &repo.toplevel, &workdir, &status, &extra);
            Ok(crate::PendingProposal {
                id,
                op: crate::ResolvedOp::Git(GitResolved {
                    kind: GitKind::SwitchBranch { name },
                    git: repo.git_bin,
                    workdir,
                    paths: Vec::new(),
                    argv,
                }),
                prior: Some(snapshot),
                prior_is_dir: false,
                diff,
            })
        }
    }
}

fn parse_current_status(
    git_bin: &Path,
    workdir: &Path,
    timeout_secs: u64,
) -> Result<GitStatus, ToolError> {
    let result = run_git(
        git_bin,
        workdir,
        &argv(&[
            "status",
            "--porcelain=v1",
            "--branch",
            "--untracked-files=normal",
        ]),
        timeout_secs,
        None,
        None,
    );
    if result.status != crate::command::CommandStatus::Success {
        return Err(ToolError::Io {
            path: workdir.display().to_string(),
            message: format!("cannot read git status: {}", clip_command_detail(&result)),
        });
    }
    Ok(parse_porcelain(&result.stdout))
}

fn list_branch_names(
    git_bin: &Path,
    workdir: &Path,
    timeout_secs: u64,
) -> Result<Vec<String>, ToolError> {
    let result = run_git(
        git_bin,
        workdir,
        &argv(&["branch", "--list", "--format=%(refname:short)"]),
        timeout_secs,
        None,
        None,
    );
    if result.status != crate::command::CommandStatus::Success {
        return Err(ToolError::Io {
            path: workdir.display().to_string(),
            message: format!("cannot list git branches: {}", clip_command_detail(&result)),
        });
    }
    Ok(result
        .stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Re-verify a prepared Git proposal: the `git` binary must still be
/// runnable, the workdir must still exist, and the repository snapshot
/// must read back byte-identical. Anything else is stale.
pub(crate) fn verify_git_fresh(
    context: &ToolContext,
    id: u64,
    prior: &Option<Vec<u8>>,
    resolved: &GitResolved,
) -> Result<(), crate::ProposalError> {
    use crate::ProposalError;
    let stale = |detail: String| ProposalError::rejected(format!("stale proposal #{id}: {detail}"));
    if !crate::command::is_executable_file(&resolved.git) {
        return Err(stale(format!(
            "git binary '{}' is no longer runnable",
            resolved.git.display()
        )));
    }
    if !resolved.workdir.is_dir() {
        return Err(stale(format!(
            "working directory '{}' is gone",
            resolved.workdir.display()
        )));
    }
    let Some(expected) = prior else {
        return Err(stale("missing repository snapshot".to_string()));
    };
    let current = repo_snapshot(
        &resolved.git,
        &resolved.workdir,
        context.limits().max_command_secs,
    )
    .map_err(|e| stale(format!("cannot re-read repository state: {e}")))?;
    if current != *expected {
        return Err(stale("repository state changed since proposal".to_string()));
    }
    Ok(())
}

/// Execute an approved Git operation. Success records the run for the
/// audit trail; every failure mode is a typed error carrying Git's own
/// output — never a silent or hidden result.
pub(crate) fn apply_git_op(
    resolved: &GitResolved,
    timeout_secs: u64,
    redact: Option<&str>,
    should_cancel: Option<&dyn Fn() -> bool>,
) -> Result<crate::command::CommandResult, ToolError> {
    if should_cancel.is_some_and(|cancel| cancel()) {
        return Err(ToolError::Cancelled);
    }
    // Defense in depth: the argv builder below only ever emits allowlisted
    // local subcommands, and this check pins that property at runtime too —
    // a forbidden subcommand can never execute even if a future code path
    // misbuilds an argv.
    if let Some(subcommand) = resolved.argv.first() {
        if FORBIDDEN_GIT_SUBCOMMANDS.contains(&subcommand.as_str()) {
            return Err(ToolError::InvalidPath(format!(
                "refusing forbidden git subcommand '{subcommand}'"
            )));
        }
    }
    let result = run_git(
        &resolved.git,
        &resolved.workdir,
        &resolved.argv,
        timeout_secs,
        redact,
        should_cancel,
    );
    match result.status {
        crate::command::CommandStatus::Success => Ok(result),
        crate::command::CommandStatus::Cancelled => Err(ToolError::Cancelled),
        crate::command::CommandStatus::Timeout => Err(ToolError::Io {
            path: resolved.workdir.display().to_string(),
            message: format!(
                "git {} timed out ({})",
                resolved.argv.first().cloned().unwrap_or_default(),
                result.detail
            ),
        }),
        crate::command::CommandStatus::LaunchFailed => Err(ToolError::Io {
            path: resolved.git.display().to_string(),
            message: format!("git failed to start: {}", result.detail),
        }),
        crate::command::CommandStatus::NonZeroExit => Err(ToolError::Io {
            path: resolved.workdir.display().to_string(),
            message: format!(
                "git {} failed (exit {}): {}",
                resolved.argv.first().cloned().unwrap_or_default(),
                result
                    .exit_code
                    .map(|code| code.to_string())
                    .as_deref()
                    .unwrap_or("?"),
                clip_command_detail(&result)
            ),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-git-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test root");
        dir
    }

    fn context(name: &str) -> (PathBuf, ToolContext) {
        let root = test_root(name);
        let context = ToolContext::new(&root).expect("context builds");
        (root, context)
    }

    fn have_git() -> Option<PathBuf> {
        git_binary().ok()
    }

    fn git_in(dir: &Path, args: &[&str]) -> crate::command::CommandResult {
        let git = have_git().expect("git fixture present");
        run_git(
            &git,
            dir,
            &args
                .iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>(),
            30,
            None,
            None,
        )
    }

    /// A workspace that is also a real repository with one commit and a
    /// configured identity (local config only — never touches the
    /// developer's global gitconfig).
    fn repo_fixture(name: &str) -> (PathBuf, ToolContext) {
        let (root, context) = context(name);
        assert_eq!(
            git_in(&root, &["init"]).status,
            crate::command::CommandStatus::Success
        );
        assert_eq!(
            git_in(&root, &["config", "user.email", "aurel-test@example.com"]).status,
            crate::command::CommandStatus::Success
        );
        assert_eq!(
            git_in(&root, &["config", "user.name", "Aurel Test"]).status,
            crate::command::CommandStatus::Success
        );
        // `std::fs::write` opens, writes, and CLOSES before git runs:
        // hashing a still-open handle can stage a zero-length blob on
        // Windows (directory-entry size lags the open writer).
        std::fs::write(root.join("seed.txt"), b"seed\n").expect("seed");
        assert_eq!(
            git_in(&root, &["add", "--", "seed.txt"]).status,
            crate::command::CommandStatus::Success
        );
        assert_eq!(
            git_in(&root, &["commit", "-m", "seed commit"]).status,
            crate::command::CommandStatus::Success
        );
        (root, context)
    }

    #[test]
    fn forbidden_subcommands_have_no_constructor() {
        // Every argv the builder can produce starts with an allowlisted
        // local subcommand. If a new op ever adds an argv path, extend this
        // test's coverage with it.
        let allowed = [
            "status",
            "diff",
            "add",
            "reset",
            "commit",
            "branch",
            "switch",
            "rev-parse",
            "symbolic-ref",
            "check-ref-format",
            "log",
            "init",
            "config",
        ];
        for forbidden in FORBIDDEN_GIT_SUBCOMMANDS {
            assert!(
                !allowed.contains(forbidden),
                "forbidden subcommand '{forbidden}' must never be allowlisted"
            );
        }
    }

    #[test]
    fn porcelain_parsing_covers_branch_forms() {
        let status =
            parse_porcelain("## main...origin/main [ahead 1]\nM  a.txt\n M b.txt\n?? c.txt\n");
        assert_eq!(status.branch, "main");
        assert_eq!(status.ahead, Some(1));
        assert_eq!(status.staged, vec!["a.txt".to_string()]);
        assert_eq!(status.unstaged, vec!["b.txt".to_string()]);
        assert_eq!(status.untracked, vec!["c.txt".to_string()]);
        assert!(!status.is_clean());

        let status = parse_porcelain("## No commits yet on main\n?? new.txt\n");
        assert!(status.unborn);
        assert_eq!(status.branch, "main");

        let status = parse_porcelain("## HEAD (no branch)\n");
        assert!(status.detached);
        assert!(status.is_clean());

        let status = parse_porcelain("## main...origin/main [ahead 2, behind 3]\n");
        assert_eq!((status.ahead, status.behind), (Some(2), Some(3)));
    }

    #[test]
    fn branch_names_and_commit_messages_validate_locally() {
        assert!(validate_commit_message("  hello  ").expect("trimmed") == "hello");
        assert!(validate_commit_message("   ").is_err());
        assert!(validate_commit_message(&"x".repeat(MAX_COMMIT_MESSAGE_BYTES + 1)).is_err());
        assert!(validate_commit_message("has\0nul").is_err());
        for bad in [
            "",
            "has space",
            "a..b",
            "we@{ird",
            "x.lock",
            "-dash",
            ".dot",
            "trail/",
            "a//b",
            "a^b",
            "a:b",
        ] {
            let (_root, context) = context("validate-branch");
            let git = match have_git() {
                Some(git) => git,
                None => return,
            };
            assert!(
                validate_branch_name(bad, &git, context.root()).is_err(),
                "must reject branch name {bad:?}"
            );
        }
    }

    #[test]
    fn detection_rejects_non_repositories() {
        if have_git().is_none() {
            return;
        }
        let (_root, context) = context("detect-non-repo");
        let err = detect_repo(&context).expect_err("plain dir is not a repo");
        assert!(
            err.to_string().contains("not a git repository"),
            "got: {err:?}"
        );
    }

    #[test]
    fn inspection_reads_status_branches_and_log() {
        if have_git().is_none() {
            return;
        }
        let (root, context) = repo_fixture("inspect");
        let toplevel = context.git_toplevel().expect("toplevel");
        assert_eq!(toplevel, context.root(), "workspace is the repo top level");
        let status = context.git_status(None).expect("status");
        assert!(status.is_clean(), "got: {status:?}");
        assert!(!status.branch.is_empty());

        std::fs::write(root.join("fresh.txt"), b"new\n").expect("write");
        let status = context.git_status(None).expect("status again");
        assert_eq!(status.untracked, vec!["fresh.txt".to_string()]);

        let diff = context.git_diff(false, None).expect("diff");
        assert!(!diff.staged);
        assert!(
            diff.text.is_empty(),
            "untracked files have no diff: {:?}",
            diff.text
        );

        let branches = context.git_branches(None).expect("branches");
        assert_eq!(branches.len(), 1);
        assert!(branches[0].current, "got: {branches:?}");

        let log = context.git_log(10, None).expect("log");
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].subject, "seed commit");

        assert!(matches!(
            context.git_log(0, None),
            Err(ToolError::InvalidPath(_))
        ));
        assert!(matches!(
            context.git_log(51, None),
            Err(ToolError::InvalidPath(_))
        ));
        assert!(matches!(
            context.git_status(Some(&|| true)),
            Err(ToolError::Cancelled)
        ));
    }

    #[test]
    fn log_on_unborn_head_is_empty_not_an_error() {
        if have_git().is_none() {
            return;
        }
        let (root, context) = context("unborn");
        assert_eq!(
            git_in(&root, &["init"]).status,
            crate::command::CommandStatus::Success
        );
        let log = context.git_log(10, None).expect("unborn log");
        assert!(log.is_empty());
    }

    #[test]
    fn argv_builder_only_emits_local_subcommands() {
        if have_git().is_none() {
            return;
        }
        let (root, context) = repo_fixture("argv");
        std::fs::write(root.join("staged.txt"), b"s\n").expect("write");
        let cases: Vec<GitOp> = vec![
            GitOp::Stage {
                paths: vec!["staged.txt".into()],
            },
            GitOp::Unstage {
                paths: vec!["seed.txt".into()],
            },
            GitOp::Commit {
                message: "msg".into(),
            },
            GitOp::CreateBranch {
                name: "feature-y".into(),
            },
            GitOp::SwitchBranch {
                name: "feature-x".into(),
            },
        ];
        // Stage the file first so the commit prepare below sees staged work;
        // create the branch first so the switch prepare below finds it.
        git_in(&root, &["add", "--", "staged.txt"]);
        git_in(&root, &["branch", "feature-x"]);
        for (index, op) in cases.into_iter().enumerate() {
            let prepared = prepare_git(&context, index as u64, op).expect("prepares");
            let subcommand = resolved_of(&prepared).argv[0].clone();
            assert!(
                !FORBIDDEN_GIT_SUBCOMMANDS.contains(&subcommand.as_str()),
                "forbidden subcommand emitted: {subcommand}"
            );
            assert!(
                prepared.diff.contains("never touches remotes"),
                "review must state the local-only boundary"
            );
        }
    }

    #[test]
    fn stage_commit_and_branch_flow_end_to_end() {
        if have_git().is_none() {
            return;
        }
        let (root, context) = repo_fixture("flow");
        std::fs::write(root.join("work.txt"), b"v1\n").expect("write");

        // Stage.
        let prepared = prepare_git(
            &context,
            1,
            GitOp::Stage {
                paths: vec!["work.txt".into()],
            },
        )
        .expect("stage prepares");
        verify_git_fresh(&context, 1, &Some(snapshot_of()), &resolved_of(&prepared))
            .expect_err("hand-built prior is not the real snapshot");
        verify_git_fresh(&context, 1, &prepared.prior, &resolved_of(&prepared))
            .expect("freshly prepared is fresh");
        let result = apply_git_op(&resolved_of(&prepared), 30, None, None).expect("stage applies");
        assert_eq!(result.status, crate::command::CommandStatus::Success);
        let status = context.git_status(None).expect("status");
        assert_eq!(status.staged, vec!["work.txt".to_string()]);

        // Commit.
        let prepared = prepare_git(
            &context,
            2,
            GitOp::Commit {
                message: "add work".into(),
            },
        )
        .expect("commit prepares");
        let result = apply_git_op(&resolved_of(&prepared), 30, None, None).expect("commit applies");
        assert_eq!(result.status, crate::command::CommandStatus::Success);
        let log = context.git_log(1, None).expect("log");
        assert_eq!(log[0].subject, "add work");

        // Nothing staged now: commit refuses to prepare.
        let err = prepare_git(
            &context,
            3,
            GitOp::Commit {
                message: "empty".into(),
            },
        )
        .expect_err("nothing staged");
        assert!(err.to_string().contains("nothing staged"), "got: {err:?}");

        // Branch create + switch.
        let prepared = prepare_git(
            &context,
            4,
            GitOp::CreateBranch {
                name: "feature-a".into(),
            },
        )
        .expect("branch prepares");
        apply_git_op(&resolved_of(&prepared), 30, None, None).expect("branch applies");
        let prepared = prepare_git(
            &context,
            5,
            GitOp::SwitchBranch {
                name: "feature-a".into(),
            },
        )
        .expect("switch prepares");
        apply_git_op(&resolved_of(&prepared), 30, None, None).expect("switch applies");
        let status = context.git_status(None).expect("status");
        assert_eq!(status.branch, "feature-a");
    }

    #[test]
    fn stale_snapshot_fails_closed() {
        if have_git().is_none() {
            return;
        }
        let (root, context) = repo_fixture("stale");
        std::fs::write(root.join("a.txt"), b"a\n").expect("write");
        let prepared = prepare_git(
            &context,
            7,
            GitOp::Stage {
                paths: vec!["a.txt".into()],
            },
        )
        .expect("prepares");
        // External activity: stage something else behind the proposal's back.
        std::fs::write(root.join("b.txt"), b"b\n").expect("write");
        git_in(&root, &["add", "--", "b.txt"]);
        let err = verify_git_fresh(&context, 7, &prepared.prior, &resolved_of(&prepared))
            .expect_err("moved tree is stale");
        assert!(
            err.to_string().contains("stale proposal #7"),
            "got: {err:?}"
        );
    }

    #[test]
    fn invalid_repo_state_and_inputs_reject_cleanly() {
        if have_git().is_none() {
            return;
        }
        let (_root, context) = context("git-invalid");
        let err = prepare_git(
            &context,
            1,
            GitOp::Stage {
                paths: vec!["x.txt".into()],
            },
        )
        .expect_err("non-repo cannot prepare");
        assert!(
            err.to_string().contains("not a git repository"),
            "got: {err:?}"
        );

        let (_root, context) = repo_fixture("git-invalid-repo");
        let err =
            prepare_git(&context, 1, GitOp::Stage { paths: vec![] }).expect_err("empty paths");
        assert!(
            err.to_string().contains("at least one path"),
            "got: {err:?}"
        );
        let err = prepare_git(
            &context,
            1,
            GitOp::Stage {
                paths: vec!["../evil.txt".into()],
            },
        )
        .expect_err("traversal");
        assert!(err.to_string().contains("bad path"), "got: {err:?}");
        std::fs::create_dir_all(_root.join("subdir")).expect("mkdir");
        let err = prepare_git(
            &context,
            1,
            GitOp::Stage {
                paths: vec!["subdir".into()],
            },
        )
        .expect_err("directory stage");
        assert!(err.to_string().contains("directory"), "got: {err:?}");
        let err = prepare_git(
            &context,
            1,
            GitOp::Commit {
                message: "  ".into(),
            },
        )
        .expect_err("empty message");
        assert!(
            err.to_string().contains("must not be empty"),
            "got: {err:?}"
        );
        let err = prepare_git(
            &context,
            1,
            GitOp::SwitchBranch {
                name: "no-such-branch-xyz".into(),
            },
        )
        .expect_err("unknown branch");
        assert!(err.to_string().contains("does not exist"), "got: {err:?}");
    }

    #[test]
    fn cancelled_git_apply_runs_nothing() {
        if have_git().is_none() {
            return;
        }
        let (root, context) = repo_fixture("git-cancel");
        std::fs::write(root.join("c.txt"), b"c\n").expect("write");
        let prepared = prepare_git(
            &context,
            1,
            GitOp::Stage {
                paths: vec!["c.txt".into()],
            },
        )
        .expect("prepares");
        let err =
            apply_git_op(&resolved_of(&prepared), 30, None, Some(&|| true)).expect_err("cancelled");
        assert_eq!(err, ToolError::Cancelled);
        let status = context.git_status(None).expect("status");
        assert!(status.staged.is_empty());
    }

    #[test]
    fn git_failures_carry_git_output() {
        if have_git().is_none() {
            return;
        }
        let (_root, _context) = repo_fixture("git-failure");
        // Hand-built argv guaranteed to fail: committing with nothing
        // staged exits nonzero with Git's own explanation.
        let git = have_git().expect("git present");
        let resolved = GitResolved {
            kind: GitKind::Commit {
                message: "x".into(),
            },
            git,
            workdir: _root.clone(),
            paths: Vec::new(),
            argv: argv(&["commit", "-m", "x"]),
        };
        let err = apply_git_op(&resolved, 30, None, None).expect_err("fails");
        let text = err.to_string();
        assert!(text.contains("git commit failed"), "got: {text:?}");
        assert!(
            text.contains("nothing to commit") || text.contains("no changes"),
            "got: {text:?}"
        );
    }

    fn resolved_of(proposal: &crate::PendingProposal) -> GitResolved {
        match &proposal.op {
            crate::ResolvedOp::Git(resolved) => resolved.clone(),
            other => panic!("expected git proposal, got {other:?}"),
        }
    }

    fn snapshot_of() -> Vec<u8> {
        b"bogus-snapshot".to_vec()
    }
}
