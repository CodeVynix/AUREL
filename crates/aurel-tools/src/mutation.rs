//! Safe file mutations with explicit approval (`aurel-tools`).
//!
//! Nothing here executes on its own. The model proposes mutations as fenced
//! JSON blocks (see [`FENCE_TAG`]); [`parse_proposals`] extracts them,
//! [`prepare_proposal`] resolves paths through the workspace sandbox,
//! snapshots prior state, and renders a reviewable diff; only an explicit
//! `/approve` may then call [`ToolContext::apply_mutation`], and
//! [`ToolContext::undo_change`] reverses what was applied.
//!
//! Safety properties, all enforced and tested:
//!
//! - Every path reuses [`ToolContext::resolve`]: traversal, absolute
//!   escapes, and symlink breakouts fail closed.
//! - Writes are bounded ([`Limits::max_write_bytes`]) and atomic
//!   (temp file + backup swap inside one directory).
//! - Edits require exactly one match; overwrites/moves/deletes refuse
//!   surprising targets (missing source, existing destination).
//! - Approval re-verifies prior state byte-for-byte, so external edits
//!   between proposal and approval fail as stale instead of applying.
//! - Errors name paths only, never file contents or secrets.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;

use crate::git::{GitOp, GitResolved};
use crate::{ToolContext, ToolError};

/// Fence tag a model reply uses to propose one mutation per block:
///
/// ````text
/// ```aurel-mutation
/// {"op": "edit_file", "path": "src/main.rs", "old": "...", "new": "..."}
/// ```
/// ````
///
/// At most [`MAX_PROPOSALS_PER_RUN`] blocks are honored per reply; the rest
/// are reported and ignored.
pub const FENCE_TAG: &str = "aurel-mutation";

/// Max proposal blocks honored per model reply (runaway-queue guard).
pub const MAX_PROPOSALS_PER_RUN: usize = 8;

/// Max diff lines rendered per proposal before a truncation marker.
pub const MAX_DIFF_LINES: usize = 100;

/// A model-proposed file mutation, parsed from one fence block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MutationOp {
    CreateFile {
        path: String,
        content: String,
    },
    EditFile {
        path: String,
        old: String,
        new: String,
    },
    OverwriteFile {
        path: String,
        content: String,
    },
    MovePath {
        from: String,
        to: String,
    },
    DeleteFile {
        path: String,
    },
    /// Run one program with literal arguments in the workspace.
    /// `args` defaults to empty when omitted.
    RunCommand {
        program: String,
        args: Vec<String>,
        purpose: String,
    },
    /// Run the detected project build command.
    RunBuild,
    /// Run the detected project test command.
    RunTests,
    /// A local-only Git mutation (stage, unstage, commit, branch
    /// create/switch). Prepared, reviewed, and approved exactly like every
    /// other mutation; never touches remotes, never rewrites history.
    Git(GitOp),
}

/// The same operation with absolute, sandbox-verified paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedOp {
    CreateFile {
        path: PathBuf,
        content: String,
    },
    EditFile {
        path: PathBuf,
        old: String,
        new: String,
    },
    OverwriteFile {
        path: PathBuf,
        content: String,
    },
    MovePath {
        from: PathBuf,
        to: PathBuf,
    },
    DeleteFile {
        path: PathBuf,
    },
    RunCommand {
        program: PathBuf,
        args: Vec<String>,
        workdir: PathBuf,
        purpose: String,
        kind: CommandKind,
    },
    /// A sandbox-resolved local Git operation (see [`GitResolved`]).
    Git(GitResolved),
}

/// What a [`ResolvedOp::RunCommand`] is for: an ad-hoc request, or the
/// detected project build/test command. Only the label differs — approval,
/// timeouts, output caps, and undo semantics are identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandKind {
    AdHoc,
    Build,
    Test,
}

impl CommandKind {
    fn label(self) -> &'static str {
        match self {
            CommandKind::AdHoc => "run",
            CommandKind::Build => "run project build",
            CommandKind::Test => "run project tests",
        }
    }
}

impl ResolvedOp {
    /// One-line human summary for approvals and listings.
    pub fn summary(&self) -> String {
        match self {
            ResolvedOp::CreateFile { path, .. } => format!("create_file {}", path.display()),
            ResolvedOp::EditFile { path, .. } => format!("edit_file {}", path.display()),
            ResolvedOp::OverwriteFile { path, .. } => format!("overwrite_file {}", path.display()),
            ResolvedOp::MovePath { from, to } => {
                format!("move {} → {}", from.display(), to.display())
            }
            ResolvedOp::DeleteFile { path } => format!("delete_file {}", path.display()),
            ResolvedOp::RunCommand {
                program,
                args,
                kind,
                ..
            } => {
                let mut summary = format!("{} {}", kind.label(), program.display());
                for arg in args {
                    summary.push(' ');
                    summary.push_str(arg);
                }
                summary
            }
            ResolvedOp::Git(resolved) => resolved.summary(),
        }
    }

    /// Every absolute path this operation touches, for sandbox re-checks.
    pub fn paths(&self) -> Vec<&Path> {
        match self {
            ResolvedOp::CreateFile { path, .. }
            | ResolvedOp::EditFile { path, .. }
            | ResolvedOp::OverwriteFile { path, .. }
            | ResolvedOp::DeleteFile { path } => vec![path],
            ResolvedOp::MovePath { from, to } => vec![from, to],
            ResolvedOp::RunCommand { workdir, .. } => vec![workdir],
            ResolvedOp::Git(resolved) => {
                let mut touched: Vec<&Path> = resolved.paths.iter().map(PathBuf::as_path).collect();
                touched.push(&resolved.workdir);
                touched
            }
        }
    }
}

/// A reviewed, ready-to-apply proposal. `prior` holds exact file bytes at
/// prepare time (`None` = absent, only valid for creates); approval
/// re-reads and byte-compares before touching anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingProposal {
    pub id: u64,
    pub op: ResolvedOp,
    pub prior: Option<Vec<u8>>,
    /// True when the primary path was a directory at prepare time (only
    /// possible for a move source; content snapshots are file-only).
    pub prior_is_dir: bool,
    /// Rendered at prepare time for review (`/diff` re-shows it).
    pub diff: String,
}

/// Inverse of one applied mutation, recorded for session-scoped undo.
/// Only AUREL-applied changes ever enter an undo stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedChange {
    CreatedFile {
        path: PathBuf,
    },
    WroteFile {
        path: PathBuf,
        prior: Vec<u8>,
    },
    MovedPath {
        from: PathBuf,
        to: PathBuf,
    },
    DeletedFile {
        path: PathBuf,
        prior: Vec<u8>,
    },
    /// A shell execution completed. Effects cannot be reversed; undo
    /// reports this honestly instead of pretending.
    ShellExecuted {
        summary: String,
    },
    /// A local Git operation completed. Effects stand: undoing one would
    /// mean rewriting repository history, which AUREL never does
    /// automatically.
    GitExecuted {
        summary: String,
    },
}

/// A proposal that cannot be queued. Messages name paths and reasons only,
/// never file contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProposalError {
    message: String,
}

impl ProposalError {
    pub(crate) fn rejected(message: impl Into<String>) -> Self {
        ProposalError {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProposalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error: invalid proposal: {}", self.message)
    }
}

impl std::error::Error for ProposalError {}

// ---------------------------------------------------------------------------
// Parsing: fences → MutationOp
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateFileOp {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFileOp {
    path: String,
    old: String,
    new: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OverwriteFileOp {
    path: String,
    content: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveOp {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteFileOp {
    path: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunCommandOp {
    program: String,
    #[serde(default)]
    args: Vec<String>,
    purpose: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitPathsOp {
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitCommitOp {
    message: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitBranchOp {
    name: String,
}

/// Decode one fence payload into a typed operation. Strict shapes
/// (`deny_unknown_fields`, all fields required) so model confusion fails
/// loudly instead of executing something unintended.
fn decode_op(payload: &str) -> Result<MutationOp, ProposalError> {
    let mut value: serde_json::Value = serde_json::from_str(payload).map_err(|e| {
        ProposalError::rejected(format!("malformed JSON: {}", clip(&e.to_string())))
    })?;
    let op = value
        .get("op")
        .and_then(|op| op.as_str())
        .ok_or_else(|| ProposalError::rejected("missing string field 'op'"))?
        .to_string();
    // The tag field has done its job; variant structs deny unknown fields,
    // so it must not travel into them.
    if let Some(map) = value.as_object_mut() {
        map.remove("op");
    }
    match op.as_str() {
        "create_file" => {
            let parsed: CreateFileOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad create_file: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::CreateFile {
                path: parsed.path,
                content: parsed.content,
            })
        }
        "edit_file" => {
            let parsed: EditFileOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad edit_file: {}", clip(&e.to_string())))
            })?;
            if parsed.old.is_empty() {
                return Err(ProposalError::rejected("edit_file 'old' must not be empty"));
            }
            Ok(MutationOp::EditFile {
                path: parsed.path,
                old: parsed.old,
                new: parsed.new,
            })
        }
        "overwrite_file" => {
            let parsed: OverwriteFileOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad overwrite_file: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::OverwriteFile {
                path: parsed.path,
                content: parsed.content,
            })
        }
        "move" => {
            let parsed: MoveOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad move: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::MovePath {
                from: parsed.from,
                to: parsed.to,
            })
        }
        "delete_file" => {
            let parsed: DeleteFileOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad delete_file: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::DeleteFile { path: parsed.path })
        }
        "run_command" => {
            let parsed: RunCommandOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad run_command: {}", clip(&e.to_string())))
            })?;
            if parsed.program.trim().is_empty() {
                return Err(ProposalError::rejected(
                    "run_command 'program' must not be empty",
                ));
            }
            if parsed.purpose.trim().is_empty() {
                return Err(ProposalError::rejected(
                    "run_command 'purpose' must not be empty (say why it should run)",
                ));
            }
            Ok(MutationOp::RunCommand {
                program: parsed.program,
                args: parsed.args,
                purpose: parsed.purpose,
            })
        }
        "run_build" => {
            if value.as_object().is_some_and(|map| !map.is_empty()) {
                return Err(ProposalError::rejected("run_build takes no fields"));
            }
            Ok(MutationOp::RunBuild)
        }
        "run_tests" => {
            if value.as_object().is_some_and(|map| !map.is_empty()) {
                return Err(ProposalError::rejected("run_tests takes no fields"));
            }
            Ok(MutationOp::RunTests)
        }
        "git_stage" => {
            let parsed: GitPathsOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad git_stage: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::Git(GitOp::Stage {
                paths: parsed.paths,
            }))
        }
        "git_unstage" => {
            let parsed: GitPathsOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad git_unstage: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::Git(GitOp::Unstage {
                paths: parsed.paths,
            }))
        }
        "git_commit" => {
            let parsed: GitCommitOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad git_commit: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::Git(GitOp::Commit {
                message: parsed.message,
            }))
        }
        "git_create_branch" => {
            let parsed: GitBranchOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad git_create_branch: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::Git(GitOp::CreateBranch { name: parsed.name }))
        }
        "git_switch_branch" => {
            let parsed: GitBranchOp = serde_json::from_value(value).map_err(|e| {
                ProposalError::rejected(format!("bad git_switch_branch: {}", clip(&e.to_string())))
            })?;
            Ok(MutationOp::Git(GitOp::SwitchBranch { name: parsed.name }))
        }
        _ => Err(ProposalError::rejected(format!("unknown op '{op}'"))),
    }
}

/// Extract fenced proposal blocks from model output text.
///
/// Returns `(operations, notes)`: valid operations in reply order alongside
/// human-readable notes for everything skipped (unclosed fences, malformed
/// JSON, unknown shapes). Never panics; oversized payloads are checked
/// later against [`Limits::max_write_bytes`] at prepare time.
pub fn parse_proposals(text: &str) -> (Vec<MutationOp>, Vec<String>) {
    let mut operations = Vec::new();
    let mut notes = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let after_ticks = match line.trim_start().strip_prefix("```") {
            Some(rest) => rest.trim_start(),
            None => continue,
        };
        let trailing = match after_ticks.strip_prefix(FENCE_TAG) {
            Some(rest) => rest.trim(),
            None => continue,
        };
        if !trailing.is_empty() {
            notes.push("ignored aurel-mutation fence with trailing text".to_string());
            continue;
        }
        let mut payload = String::new();
        let mut closed = false;
        for fence_line in lines.by_ref() {
            if fence_line.trim_start().starts_with("```") {
                closed = true;
                break;
            }
            payload.push_str(fence_line);
            payload.push('\n');
        }
        if !closed {
            notes.push("ignored unclosed aurel-mutation fence".to_string());
            break;
        }
        match decode_op(&payload) {
            Ok(op) => operations.push(op),
            Err(error) => notes.push(format!("ignored malformed proposal ({error})")),
        }
    }
    (operations, notes)
}

// ---------------------------------------------------------------------------
// Prepare: MutationOp → PendingProposal (resolve, snapshot, diff)
// ---------------------------------------------------------------------------

/// Resolve, validate, snapshot prior state, and render the review diff.
/// Anything surprising becomes a rejection note instead of a queued
/// proposal — nothing half-checked ever reaches approval.
pub fn prepare_proposal(
    context: &ToolContext,
    id: u64,
    op: MutationOp,
) -> Result<PendingProposal, ProposalError> {
    let limit = context.limits().max_write_bytes as usize;
    let check_size = |what: &str, content: &str| {
        if content.len() > limit {
            Err(ProposalError::rejected(format!(
                "{what} exceeds the {limit}-byte write limit"
            )))
        } else {
            Ok(())
        }
    };
    match op {
        MutationOp::CreateFile { path, content } => {
            check_size("create_file content", &content)?;
            let real = resolve_named(context, &path)?;
            if real.exists() {
                return Err(ProposalError::rejected(format!(
                    "create_file target already exists: '{}' (use overwrite_file or edit_file)",
                    real.display()
                )));
            }
            require_parent_dir(&real)?;
            let diff = render_diff(None, Some(&content));
            Ok(PendingProposal {
                id,
                op: ResolvedOp::CreateFile {
                    path: real,
                    content,
                },
                prior: None,
                prior_is_dir: false,
                diff,
            })
        }
        MutationOp::EditFile { path, old, new } => {
            check_size("edit_file replacement", &new)?;
            let real = resolve_named(context, &path)?;
            let prior = read_prior_file(&real, context.limits().max_write_bytes)?;
            let prior_text = String::from_utf8_lossy(&prior);
            let occurrences = prior_text.matches(old.as_str()).count();
            if occurrences == 0 {
                return Err(ProposalError::rejected(format!(
                    "'old' text not found in '{}'",
                    real.display()
                )));
            }
            if occurrences > 1 {
                return Err(ProposalError::rejected(format!(
                    "'old' text matches {occurrences} times in '{}' (must match exactly once)",
                    real.display()
                )));
            }
            let after = prior_text.replacen(old.as_str(), &new, 1);
            let diff = render_diff(Some(&prior_text), Some(&after));
            Ok(PendingProposal {
                id,
                op: ResolvedOp::EditFile {
                    path: real,
                    old,
                    new,
                },
                prior: Some(prior),
                prior_is_dir: false,
                diff,
            })
        }
        MutationOp::OverwriteFile { path, content } => {
            check_size("overwrite_file content", &content)?;
            let real = resolve_named(context, &path)?;
            let prior = read_prior_file(&real, context.limits().max_write_bytes)?;
            let diff = render_diff(
                Some(&String::from_utf8_lossy(&prior)),
                Some(content.as_str()),
            );
            Ok(PendingProposal {
                id,
                op: ResolvedOp::OverwriteFile {
                    path: real,
                    content,
                },
                prior: Some(prior),
                prior_is_dir: false,
                diff,
            })
        }
        MutationOp::MovePath { from, to } => {
            let real_from = resolve_named(context, &from)?;
            let real_to = resolve_named(context, &to)?;
            if !real_from.exists() {
                return Err(ProposalError::rejected(format!(
                    "move source does not exist: '{}'",
                    real_from.display()
                )));
            }
            if real_to.exists() {
                return Err(ProposalError::rejected(format!(
                    "move destination already exists: '{}' (refusing to overwrite)",
                    real_to.display()
                )));
            }
            require_parent_dir(&real_to)?;
            let from_is_dir = real_from.is_dir();
            let diff = format!("rename {} → {}\n", real_from.display(), real_to.display());
            Ok(PendingProposal {
                id,
                op: ResolvedOp::MovePath {
                    from: real_from,
                    to: real_to,
                },
                prior: None,
                prior_is_dir: from_is_dir,
                diff,
            })
        }
        MutationOp::DeleteFile { path } => {
            let real = resolve_named(context, &path)?;
            let prior = read_prior_file(&real, context.limits().max_write_bytes)?;
            if prior.len() as u64 > context.limits().max_write_bytes {
                return Err(ProposalError::rejected(format!(
                    "'{}' is too large to delete safely (undo would need the full content)",
                    real.display()
                )));
            }
            let diff = render_diff(Some(&String::from_utf8_lossy(&prior)), None);
            Ok(PendingProposal {
                id,
                op: ResolvedOp::DeleteFile { path: real },
                prior: Some(prior),
                prior_is_dir: false,
                diff,
            })
        }
        MutationOp::RunCommand {
            program,
            args,
            purpose,
        } => prepare_command(context, id, &program, args, purpose, CommandKind::AdHoc),
        MutationOp::RunBuild => {
            let detected =
                crate::detect_build_commands(context.root()).ok_or_else(|| {
                    ProposalError::rejected(
                        "no supported project marker found (Cargo.toml, package.json, go.mod, Makefile)",
                    )
                })?;
            prepare_command(
                context,
                id,
                &detected.build_program,
                detected.build_args,
                format!("project build via {}", detected.kind),
                CommandKind::Build,
            )
        }
        MutationOp::RunTests => {
            let detected =
                crate::detect_build_commands(context.root()).ok_or_else(|| {
                    ProposalError::rejected(
                        "no supported project marker found (Cargo.toml, package.json, go.mod, Makefile)",
                    )
                })?;
            prepare_command(
                context,
                id,
                &detected.test_program,
                detected.test_args,
                format!("project tests via {}", detected.kind),
                CommandKind::Test,
            )
        }
        MutationOp::Git(op) => crate::git::prepare_git(context, id, op),
    }
}

/// Shared preparation for all command executions: resolve the program
/// (PATH search, never the workspace file sandbox — program identity is
/// controlled by approval visibility, not by file containment), pin the
/// working directory to the workspace root, and render the review block.
fn prepare_command(
    context: &ToolContext,
    id: u64,
    program: &str,
    args: Vec<String>,
    purpose: String,
    kind: CommandKind,
) -> Result<PendingProposal, ProposalError> {
    let program_path = crate::resolve_program(program, &crate::system_path_dirs())
        .map_err(|e| ProposalError::rejected(format!("bad program '{program}': {e}")))?;
    let workdir = context.root().to_path_buf();
    let timeout = context.limits().max_command_secs;
    let cap = context.limits().max_command_output_bytes;
    let mut diff = format!("$ {} {}\n", program_path.display(), shell_quote_args(&args));
    diff.push_str(&format!("cwd: {}\n", workdir.display()));
    diff.push_str(&format!("purpose: {purpose}\n"));
    diff.push_str(&format!(
        "timeout: {timeout}s, output capped at {cap} bytes per stream\n"
    ));
    Ok(PendingProposal {
        id,
        op: ResolvedOp::RunCommand {
            program: program_path,
            args,
            workdir,
            purpose,
            kind,
        },
        prior: None,
        prior_is_dir: false,
        diff,
    })
}

/// Quote arguments for review display (single quotes with embedded-quote
/// escaping). Display-only: execution always uses the raw argument vector.
fn shell_quote_args(args: &[String]) -> String {
    args.iter()
        .map(|arg| {
            if arg.is_empty() || arg.chars().any(|c| c.is_whitespace() || c == '\'') {
                format!("'{}'", arg.replace('\'', "'\\''"))
            } else {
                arg.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn resolve_named(context: &ToolContext, user: &str) -> Result<PathBuf, ProposalError> {
    context
        .resolve(user)
        .map_err(|e| ProposalError::rejected(format!("bad path '{user}': {e}")))
}

/// Read an existing regular file for snapshot/diff purposes, bounded by
/// `cap` bytes. Missing, non-file, binary, and oversized targets are
/// rejections, not reads.
fn read_prior_file(path: &Path, cap: u64) -> Result<Vec<u8>, ProposalError> {
    let label = path.display().to_string();
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            ProposalError::rejected(format!("file does not exist: '{label}'"))
        } else {
            ProposalError::rejected(format!("cannot read '{label}': {e}"))
        }
    })?;
    if !meta.file_type().is_file() {
        return Err(ProposalError::rejected(format!("not a file: '{label}'")));
    }
    // Bounded read: one byte past the cap tells truncation apart.
    let bytes = read_capped(path, cap + 1)
        .map_err(|e| ProposalError::rejected(format!("cannot read '{label}': {e}")))?;
    if bytes.len() as u64 > cap {
        return Err(ProposalError::rejected(format!(
            "'{label}' exceeds the readable size limit"
        )));
    }
    if bytes.contains(&0) {
        return Err(ProposalError::rejected(format!(
            "not readable text: '{label}'"
        )));
    }
    Ok(bytes)
}

/// Re-verify a prepared proposal against current filesystem state: every
/// touched path must read back exactly what was snapshotted (or stay
/// absent for creates, and keep kind/absence for moves). Anything else is
/// stale — an external edit landed between proposal and approval — and the
/// caller must drop the proposal instead of applying it.
pub fn verify_fresh(
    context: &ToolContext,
    proposal: &PendingProposal,
) -> Result<(), ProposalError> {
    let stale = |detail: String| {
        ProposalError::rejected(format!("stale proposal #{}: {detail}", proposal.id))
    };
    let current_bytes = |path: &Path| -> Option<Vec<u8>> {
        let meta = fs::symlink_metadata(path).ok()?;
        if !meta.file_type().is_file() {
            return None;
        }
        fs::read(path).ok()
    };
    match &proposal.op {
        ResolvedOp::CreateFile { path, .. } => {
            if path.exists() {
                return Err(stale(format!(
                    "'{}' appeared since proposal",
                    path.display()
                )));
            }
        }
        ResolvedOp::EditFile { path, .. }
        | ResolvedOp::OverwriteFile { path, .. }
        | ResolvedOp::DeleteFile { path } => {
            if current_bytes(path).as_deref() != proposal.prior.as_deref() {
                return Err(stale(format!(
                    "'{}' changed since proposal",
                    path.display()
                )));
            }
        }
        ResolvedOp::MovePath { from, to } => {
            let from_ok = match fs::symlink_metadata(from) {
                Ok(meta) => {
                    let is_dir = meta.file_type().is_dir();
                    // Symlinked sources resolve at prepare; a swapped-in
                    // symlink at approve time is a change.
                    !meta.file_type().is_symlink() && is_dir == proposal.prior_is_dir
                }
                Err(_) => false,
            };
            if !from_ok || to.exists() {
                return Err(stale(format!(
                    "move '{}' → '{}' no longer applies cleanly",
                    from.display(),
                    to.display()
                )));
            }
        }
        ResolvedOp::RunCommand {
            program, workdir, ..
        } => {
            if !crate::command::is_executable_file(program) {
                return Err(stale(format!(
                    "program '{}' is no longer runnable",
                    program.display()
                )));
            }
            if !workdir.is_dir() {
                return Err(stale(format!(
                    "working directory '{}' is gone",
                    workdir.display()
                )));
            }
        }
        ResolvedOp::Git(resolved) => {
            // Repository state (HEAD + branch + porcelain status) must read
            // back byte-identical; errors already carry the stale prefix,
            // and the generic sandbox loop below re-checks every path.
            crate::git::verify_git_fresh(context, proposal.id, &proposal.prior, resolved)?;
        }
    }
    // Sandbox once more: roots do not move, but cheap certainty beats trust.
    for path in proposal.op.paths() {
        context
            .resolve(&path.display().to_string())
            .map_err(|e| stale(format!("path check failed: {e}")))?;
    }
    Ok(())
}

fn read_capped(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let file = fs::File::open(path)?;
    let mut buffer = Vec::new();
    file.take(cap).read_to_end(&mut buffer)?;
    Ok(buffer)
}

/// The parent directory must already exist: AUREL never conjures directory
/// trees as a side effect of a file mutation.
fn require_parent_dir(path: &Path) -> Result<(), ProposalError> {
    match path.parent() {
        Some(parent) if parent.is_dir() => Ok(()),
        _ => Err(ProposalError::rejected(format!(
            "parent directory does not exist for '{}'",
            path.display()
        ))),
    }
}

// ---------------------------------------------------------------------------
// Diff rendering (dependency-free, bounded)
// ---------------------------------------------------------------------------

/// Render a reviewable diff between optional before/after texts:
/// `None` means absent (create/delete). Common prefix/suffix lines fold
/// away; the middle renders as `-`/`+` blocks capped at
/// [`MAX_DIFF_LINES`] with a truncation marker.
pub fn render_diff(before: Option<&str>, after: Option<&str>) -> String {
    match (before, after) {
        (None, None) => String::new(),
        (None, Some(text)) => prefix_lines('+', text),
        (Some(text), None) => prefix_lines('-', text),
        (Some(old), Some(new)) => {
            if old == new {
                return "(no changes)\n".to_string();
            }
            let old_lines: Vec<&str> = old.lines().collect();
            let new_lines: Vec<&str> = new.lines().collect();
            let mut prefix = 0;
            while prefix < old_lines.len()
                && prefix < new_lines.len()
                && old_lines[prefix] == new_lines[prefix]
            {
                prefix += 1;
            }
            let mut suffix = 0;
            while suffix < old_lines.len() - prefix
                && suffix < new_lines.len() - prefix
                && old_lines[old_lines.len() - 1 - suffix]
                    == new_lines[new_lines.len() - 1 - suffix]
            {
                suffix += 1;
            }
            let mut out = String::new();
            let mut shown = 0usize;
            let mut truncated = 0usize;
            let mut emit = |marker: char, line: &str| {
                if shown < MAX_DIFF_LINES {
                    out.push(marker);
                    out.push_str(line);
                    out.push('\n');
                    shown += 1;
                } else {
                    truncated += 1;
                }
            };
            for line in &old_lines[prefix..old_lines.len() - suffix] {
                emit('-', line);
            }
            for line in &new_lines[prefix..new_lines.len() - suffix] {
                emit('+', line);
            }
            if truncated > 0 {
                out.push_str(&format!("…({truncated} more lines)\n"));
            }
            out.push_str(&format!(
                "({} common line{} hidden)\n",
                prefix + suffix,
                if prefix + suffix == 1 { "" } else { "s" }
            ));
            out
        }
    }
}

fn prefix_lines(marker: char, text: &str) -> String {
    let mut out = String::new();
    let mut shown = 0usize;
    let mut truncated = 0usize;
    for line in text.lines() {
        if shown < MAX_DIFF_LINES {
            out.push(marker);
            out.push_str(line);
            out.push('\n');
            shown += 1;
        } else {
            truncated += 1;
        }
    }
    if truncated > 0 {
        out.push_str(&format!("…({truncated} more lines)\n"));
    }
    out
}

// ---------------------------------------------------------------------------
// Apply + undo (ToolContext methods: same sandbox, atomic writes)
// ---------------------------------------------------------------------------

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Write `bytes` to `dest` atomically: temp file in the same directory,
/// existing content swung aside to a backup, temp renamed over, backup
/// removed. A failed final rename restores the backup best-effort.
fn atomic_write(dest: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let io_error = |message: String| ToolError::Io {
        path: dest.display().to_string(),
        message,
    };
    let parent = dest.parent().ok_or_else(|| {
        ToolError::InvalidPath(format!("no parent directory: '{}'", dest.display()))
    })?;
    let id = TEMP_COUNTER.fetch_add(1, Ordering::SeqCst);
    let temp = parent.join(format!(".aurel-tmp-{}-{id}", std::process::id()));
    let backup = parent.join(format!(".aurel-bak-{}-{id}", std::process::id()));
    fs::write(&temp, bytes).map_err(|e| {
        let _ = fs::remove_file(&temp);
        io_error(e.to_string())
    })?;
    let had_dest = dest.exists();
    if had_dest {
        fs::rename(dest, &backup).map_err(|e| {
            let _ = fs::remove_file(&temp);
            io_error(e.to_string())
        })?;
    }
    if let Err(e) = fs::rename(&temp, dest) {
        if had_dest {
            let _ = fs::rename(&backup, dest);
        }
        let _ = fs::remove_file(&temp);
        return Err(io_error(e.to_string()));
    }
    if had_dest {
        let _ = fs::remove_file(&backup);
    }
    Ok(())
}

impl ToolContext {
    /// Execute a prepared, approved mutation. Callers must have shown the
    /// proposal diff and received explicit approval first — this function
    /// performs no prompting itself.
    ///
    /// Returns the inverse change for the session undo stack, plus the
    /// command result for shell executions (file operations print nothing
    /// themselves, so theirs is `None`).
    pub fn apply_mutation(
        &self,
        op: &ResolvedOp,
        redact: Option<&str>,
        should_cancel: Option<&dyn Fn() -> bool>,
    ) -> Result<(AppliedChange, Option<crate::command::CommandResult>), ToolError> {
        if should_cancel.is_some_and(|cancel| cancel()) {
            return Err(ToolError::Cancelled);
        }
        // Defense in depth: every path is re-checked against the sandbox
        // even though `prepare` already resolved them.
        match op {
            ResolvedOp::CreateFile { path, content } => {
                self.recheck(path)?;
                if path.exists() {
                    return Err(ToolError::Io {
                        path: display_of(path),
                        message: "target appeared before apply".to_string(),
                    });
                }
                atomic_write(path, content.as_bytes())?;
                Ok((AppliedChange::CreatedFile { path: path.clone() }, None))
            }
            ResolvedOp::EditFile { path, old, new } => {
                let prior = self.recheck_file(path)?;
                let text = String::from_utf8_lossy(&prior);
                // Single-occurrence was verified at prepare; re-verify so a
                // concurrently changed file fails instead of mis-applying.
                if text.matches(old.as_str()).count() != 1 {
                    return Err(ToolError::Io {
                        path: display_of(path),
                        message: "file changed since proposal (stale match)".to_string(),
                    });
                }
                let after = text.replacen(old.as_str(), new, 1);
                atomic_write(path, after.as_bytes())?;
                Ok((
                    AppliedChange::WroteFile {
                        path: path.clone(),
                        prior,
                    },
                    None,
                ))
            }
            ResolvedOp::OverwriteFile { path, content } => {
                let prior = self.recheck_file(path)?;
                atomic_write(path, content.as_bytes())?;
                Ok((
                    AppliedChange::WroteFile {
                        path: path.clone(),
                        prior,
                    },
                    None,
                ))
            }
            ResolvedOp::MovePath { from, to } => {
                self.recheck(from)?;
                self.recheck(to)?;
                if !from.exists() {
                    return Err(ToolError::NotFound {
                        path: display_of(from),
                    });
                }
                if to.exists() {
                    return Err(ToolError::Io {
                        path: display_of(to),
                        message: "destination appeared before apply".to_string(),
                    });
                }
                fs::rename(from, to).map_err(|e| ToolError::Io {
                    path: format!("{} → {}", from.display(), to.display()),
                    message: e.to_string(),
                })?;
                Ok((
                    AppliedChange::MovedPath {
                        from: from.clone(),
                        to: to.clone(),
                    },
                    None,
                ))
            }
            ResolvedOp::DeleteFile { path } => {
                let prior = self.recheck_file(path)?;
                fs::remove_file(path).map_err(|e| ToolError::Io {
                    path: display_of(path),
                    message: e.to_string(),
                })?;
                Ok((
                    AppliedChange::DeletedFile {
                        path: path.clone(),
                        prior,
                    },
                    None,
                ))
            }
            ResolvedOp::RunCommand {
                program,
                args,
                workdir,
                purpose,
                kind,
            } => {
                use crate::command::{run_command, CommandRequest};
                self.recheck(workdir)?;
                if !workdir.is_dir() {
                    return Err(ToolError::NotDirectory {
                        path: display_of(workdir),
                    });
                }
                let summary = ResolvedOp::RunCommand {
                    program: program.clone(),
                    args: args.clone(),
                    workdir: workdir.clone(),
                    purpose: purpose.clone(),
                    kind: *kind,
                }
                .summary();
                let result = run_command(
                    &CommandRequest {
                        program: display_of(program),
                        args: args.clone(),
                        workdir: workdir.clone(),
                        timeout: Duration::from_secs(self.limits().max_command_secs),
                        redact: redact.map(str::to_string),
                    },
                    should_cancel,
                );
                Ok((AppliedChange::ShellExecuted { summary }, Some(result)))
            }
            ResolvedOp::Git(resolved) => {
                self.recheck(&resolved.workdir)?;
                for path in &resolved.paths {
                    self.recheck(path)?;
                }
                let summary = resolved.summary();
                let result = crate::git::apply_git_op(
                    resolved,
                    self.limits().max_command_secs,
                    redact,
                    should_cancel,
                )?;
                Ok((AppliedChange::GitExecuted { summary }, Some(result)))
            }
        }
    }

    /// Re-check one absolute path against the sandbox (defense in depth for
    /// paths resolved earlier, e.g. at proposal time).
    fn recheck(&self, path: &Path) -> Result<(), ToolError> {
        let canonical = if path.exists() {
            path.canonicalize().map_err(|e| ToolError::Io {
                path: display_of(path),
                message: e.to_string(),
            })?
        } else {
            match path.parent() {
                Some(parent) if parent.exists() => {
                    let mut real = parent.canonicalize().map_err(|e| ToolError::Io {
                        path: display_of(path),
                        message: e.to_string(),
                    })?;
                    if let Some(name) = path.file_name() {
                        real.push(name);
                    }
                    real
                }
                _ => {
                    return Err(ToolError::NotFound {
                        path: display_of(path),
                    });
                }
            }
        };
        if !canonical.starts_with(self.root()) {
            return Err(ToolError::OutsideWorkspace {
                path: display_of(path),
            });
        }
        Ok(())
    }

    /// Re-check plus read-back of an existing regular file.
    fn recheck_file(&self, path: &Path) -> Result<Vec<u8>, ToolError> {
        self.recheck(path)?;
        let meta = fs::symlink_metadata(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ToolError::NotFound {
                    path: display_of(path),
                }
            } else {
                ToolError::Io {
                    path: display_of(path),
                    message: e.to_string(),
                }
            }
        })?;
        if !meta.file_type().is_file() {
            return Err(ToolError::NotFile {
                path: display_of(path),
            });
        }
        fs::read(path).map_err(|e| ToolError::Io {
            path: display_of(path),
            message: e.to_string(),
        })
    }

    /// Reverse one AUREL-applied change. Only inverses recorded in
    /// [`AppliedChange`] can run here, so undo never touches anything
    /// AUREL did not itself change. A failed inverse is reported and the
    /// record is consumed (there is no redo).
    pub fn undo_change(&self, change: &AppliedChange) -> Result<(), ToolError> {
        match change {
            AppliedChange::CreatedFile { path } => {
                self.recheck(path)?;
                if !path.is_file() {
                    return Err(ToolError::NotFound {
                        path: display_of(path),
                    });
                }
                fs::remove_file(path).map_err(|e| ToolError::Io {
                    path: display_of(path),
                    message: e.to_string(),
                })
            }
            AppliedChange::WroteFile { path, prior } => {
                self.recheck(path)?;
                if !path.is_file() {
                    return Err(ToolError::Io {
                        path: display_of(path),
                        message: "undo target is missing".to_string(),
                    });
                }
                atomic_write(path, prior)
            }
            AppliedChange::MovedPath { from, to } => {
                self.recheck(from)?;
                self.recheck(to)?;
                if !to.exists() {
                    return Err(ToolError::NotFound {
                        path: display_of(to),
                    });
                }
                if from.exists() {
                    return Err(ToolError::Io {
                        path: display_of(from),
                        message: "undo destination is occupied".to_string(),
                    });
                }
                fs::rename(to, from).map_err(|e| ToolError::Io {
                    path: format!("{} → {}", to.display(), from.display()),
                    message: e.to_string(),
                })
            }
            AppliedChange::DeletedFile { path, prior } => {
                self.recheck(path)?;
                if path.exists() {
                    return Err(ToolError::Io {
                        path: display_of(path),
                        message: "undo target already exists".to_string(),
                    });
                }
                atomic_write(path, prior)
            }
            AppliedChange::ShellExecuted { summary } => Err(ToolError::NotUndoable(format!(
                "shell execution '{summary}' already ran; its effects stand"
            ))),
            AppliedChange::GitExecuted { summary } => Err(ToolError::NotUndoable(format!(
                "git operation '{summary}' already ran; local effects stand (undo never rewrites history)"
            ))),
        }
    }
}

fn display_of(path: &Path) -> String {
    path.display().to_string()
}

/// Clip server/model-controlled text for diagnostics (never file content,
///
/// just error snippets).
fn clip(text: &str) -> String {
    const MAX_DIAG_CHARS: usize = 500;
    if text.chars().count() > MAX_DIAG_CHARS {
        let clipped: String = text.chars().take(MAX_DIAG_CHARS).collect();
        format!("{clipped}…")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn test_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-mutate-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test root");
        dir
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parents");
        }
        let mut file = std::fs::File::create(path).expect("create file");
        file.write_all(content).expect("write file");
    }

    fn context(name: &str) -> (PathBuf, ToolContext) {
        let root = test_root(name);
        let context = ToolContext::new(&root).expect("context builds");
        (root, context)
    }

    fn prepare(context: &ToolContext, id: u64, op: MutationOp) -> PendingProposal {
        prepare_proposal(context, id, op).expect("proposal prepares")
    }

    fn apply(context: &ToolContext, proposal: &PendingProposal) -> AppliedChange {
        let (change, output) = context
            .apply_mutation(&proposal.op, None, None)
            .expect("apply succeeds");
        assert!(output.is_none(), "file ops produce no command output");
        change
    }

    #[test]
    fn fence_extraction_round_trips_all_ops() {
        let text = "thinking\n```aurel-mutation\n{\"op\": \"create_file\", \"path\": \"a.txt\", \"content\": \"hi\"}\n```\ntail\n```aurel-mutation\n{\"op\": \"delete_file\", \"path\": \"b.txt\"}\n```\n";
        let (operations, notes) = parse_proposals(text);
        assert!(notes.is_empty(), "got notes: {notes:?}");
        assert_eq!(
            operations,
            vec![
                MutationOp::CreateFile {
                    path: "a.txt".into(),
                    content: "hi".into(),
                },
                MutationOp::DeleteFile {
                    path: "b.txt".into()
                },
            ]
        );
    }

    #[test]
    fn fence_problems_become_notes_not_panics() {
        let (operations, notes) = parse_proposals("```aurel-mutation\n{\"op\": \"nope\"}\n```\n");
        assert!(operations.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("unknown op"));

        let (operations, notes) =
            parse_proposals("```aurel-mutation\n{\"op\": \"delete_file\"}\n```\n");
        assert!(operations.is_empty());
        assert!(notes[0].contains("delete_file"));

        let (operations, notes) = parse_proposals("```aurel-mutation\n{not json\n```\n");
        assert!(operations.is_empty());
        assert!(notes[0].contains("malformed JSON"));

        let (operations, notes) =
            parse_proposals("```aurel-mutation\n{\"op\": \"delete_file\", \"path\": \"x\"}\n");
        assert!(operations.is_empty());
        assert!(notes.iter().any(|note| note.contains("unclosed")));

        // Unknown fields and empty edit anchors are strict rejections.
        let (operations, notes) = parse_proposals(
            "```aurel-mutation\n{\"op\": \"delete_file\", \"path\": \"x\", \"extra\": 1}\n```\n",
        );
        assert!(operations.is_empty());
        assert!(!notes.is_empty());

        let (operations, notes) = parse_proposals(
            "```aurel-mutation\n{\"op\": \"edit_file\", \"path\": \"x\", \"old\": \"\", \"new\": \"y\"}\n```\n",
        );
        assert!(operations.is_empty());
        assert!(!notes.is_empty());

        // Non-fence code blocks are ignored entirely.
        let (operations, notes) = parse_proposals("```rust\nlet x = 1;\n```\n");
        assert!(operations.is_empty());
        assert!(notes.is_empty());
    }

    #[test]
    fn create_edit_overwrite_round_trip_with_undo() {
        let (_root, context) = context("round-trip");
        let created = prepare(
            &context,
            1,
            MutationOp::CreateFile {
                path: "new.txt".into(),
                content: "hello\n".into(),
            },
        );
        assert!(created.diff.contains("+hello"));
        let change = apply(&context, &created);
        assert!(matches!(change, AppliedChange::CreatedFile { .. }));
        assert_eq!(
            std::fs::read_to_string(_root.join("new.txt")).expect("read back"),
            "hello\n"
        );

        let edited = prepare(
            &context,
            2,
            MutationOp::EditFile {
                path: "new.txt".into(),
                old: "hello".into(),
                new: "goodbye".into(),
            },
        );
        assert!(edited.diff.contains("-hello") && edited.diff.contains("+goodbye"));
        let change = apply(&context, &edited);
        assert!(matches!(change, AppliedChange::WroteFile { .. }));
        context.undo_change(&change).expect("undo edit");
        assert_eq!(
            std::fs::read_to_string(_root.join("new.txt")).expect("read back"),
            "hello\n"
        );

        let overwritten = prepare(
            &context,
            3,
            MutationOp::OverwriteFile {
                path: "new.txt".into(),
                content: "fresh\n".into(),
            },
        );
        let change = apply(&context, &overwritten);
        context.undo_change(&change).expect("undo overwrite");
        assert_eq!(
            std::fs::read_to_string(_root.join("new.txt")).expect("read back"),
            "hello\n"
        );
    }

    #[test]
    fn edit_requires_exactly_one_match() {
        let (root, context) = context("edit-once");
        write(&root.join("a.txt"), b"same same same\n");
        let err = prepare_proposal(
            &context,
            1,
            MutationOp::EditFile {
                path: "a.txt".into(),
                old: "same".into(),
                new: "x".into(),
            },
        )
        .expect_err("ambiguous edit");
        assert!(err.to_string().contains("exactly once"));
        write(&root.join("a.txt"), b"nothing alike\n");
        let err = prepare_proposal(
            &context,
            2,
            MutationOp::EditFile {
                path: "a.txt".into(),
                old: "missing".into(),
                new: "x".into(),
            },
        )
        .expect_err("missing anchor");
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn create_refuses_existing_and_missing_parents() {
        let (root, context) = context("create-guards");
        write(&root.join("taken.txt"), b"taken\n");
        let err = prepare_proposal(
            &context,
            1,
            MutationOp::CreateFile {
                path: "taken.txt".into(),
                content: "x".into(),
            },
        )
        .expect_err("existing target");
        assert!(err.to_string().contains("already exists"));
        let err = prepare_proposal(
            &context,
            2,
            MutationOp::CreateFile {
                path: "no/such/dir/f.txt".into(),
                content: "x".into(),
            },
        )
        .expect_err("missing parent");
        assert!(err.to_string().contains("parent directory"));
    }

    #[test]
    fn move_and_delete_round_trip_with_undo() {
        let (root, context) = context("move-delete");
        write(&root.join("a.txt"), b"data\n");
        write(&root.join("taken.txt"), b"taken\n");
        // Destination occupied: rejected.
        assert!(prepare_proposal(
            &context,
            1,
            MutationOp::MovePath {
                from: "a.txt".into(),
                to: "taken.txt".into(),
            }
        )
        .is_err());
        let moved = prepare(
            &context,
            2,
            MutationOp::MovePath {
                from: "a.txt".into(),
                to: "b.txt".into(),
            },
        );
        assert!(moved.diff.contains("a.txt") && moved.diff.contains("b.txt"));
        let change = apply(&context, &moved);
        assert!(!root.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).expect("moved"),
            "data\n"
        );
        context.undo_change(&change).expect("undo move");
        assert!(root.join("a.txt").exists());
        assert!(!root.join("b.txt").exists());

        let deleted = prepare(
            &context,
            3,
            MutationOp::DeleteFile {
                path: "taken.txt".into(),
            },
        );
        assert!(deleted.diff.contains("-taken"));
        let change = apply(&context, &deleted);
        assert!(!root.join("taken.txt").exists());
        context.undo_change(&change).expect("undo delete");
        assert_eq!(
            std::fs::read_to_string(root.join("taken.txt")).expect("restored"),
            "taken\n"
        );
        // Undo stack discipline: undoing twice fails, touching nothing.
        assert!(context
            .undo_change(&AppliedChange::CreatedFile {
                path: root.join("never-created.txt"),
            })
            .is_err());
    }

    #[test]
    fn mutations_respect_the_sandbox() {
        let (root, context) = context("mutate-jail");
        let outside = test_root("mutate-outside");
        write(&outside.join("secret.txt"), b"secret\n");
        let attempts = [
            MutationOp::CreateFile {
                path: "../evil.txt".into(),
                content: "x".into(),
            },
            MutationOp::EditFile {
                path: "../evil.txt".into(),
                old: "a".into(),
                new: "b".into(),
            },
            MutationOp::OverwriteFile {
                path: outside.join("secret.txt").display().to_string(),
                content: "x".into(),
            },
            MutationOp::MovePath {
                from: "a.txt".into(),
                to: "../evil.txt".into(),
            },
            MutationOp::DeleteFile {
                path: "../evil.txt".into(),
            },
        ];
        for (i, op) in attempts.into_iter().enumerate() {
            let err = prepare_proposal(&context, i as u64, op).expect_err("must be jailed");
            assert!(
                err.to_string().contains("escapes the workspace")
                    || err.to_string().contains("does not exist")
                    || err.to_string().contains("bad path"),
                "unexpected rejection: {err}"
            );
        }
        // The symlink escape below proves outside bytes are never touched.
        // Symlink escape: a link inside pointing out cannot be written through.
        let link = root.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("secret.txt"), &link).expect("symlink");
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_file(outside.join("secret.txt"), &link).is_err() {
                return;
            }
        }
        #[cfg(not(any(unix, windows)))]
        return;
        let err = prepare_proposal(
            &context,
            99,
            MutationOp::OverwriteFile {
                path: "link.txt".into(),
                content: "pwned".into(),
            },
        )
        .expect_err("symlink escape");
        assert!(err.to_string().contains("escapes the workspace"));
        assert_eq!(
            std::fs::read_to_string(outside.join("secret.txt")).expect("untouched"),
            "secret\n"
        );
    }

    #[test]
    fn stale_proposals_fail_closed() {
        let (root, context) = context("stale");
        write(&root.join("a.txt"), b"one\n");
        let prepared = prepare(
            &context,
            7,
            MutationOp::EditFile {
                path: "a.txt".into(),
                old: "one".into(),
                new: "two".into(),
            },
        );
        // Untouched since prepare: fresh.
        verify_fresh(&context, &prepared).expect("fresh proposal verifies");
        // External edit between proposal and approval changes the bytes.
        write(&root.join("a.txt"), b"one changed\n");
        let err = verify_fresh(&context, &prepared).expect_err("must go stale");
        assert!(err.to_string().contains("stale proposal #7"), "got: {err}");
        // And a created target appearing out of band is stale too.
        let created = prepare(
            &context,
            8,
            MutationOp::CreateFile {
                path: "fresh.txt".into(),
                content: "x".into(),
            },
        );
        write(&root.join("fresh.txt"), b"someone else\n");
        assert!(verify_fresh(&context, &created).is_err());
    }

    #[test]
    fn errors_carry_paths_not_contents() {
        let (root, context) = context("no-leak");
        write(&root.join("keys.txt"), b"sk-test-secret-abc123\n");
        let err = prepare_proposal(
            &context,
            1,
            MutationOp::EditFile {
                path: "keys.txt".into(),
                old: "missing anchor".into(),
                new: "x".into(),
            },
        )
        .expect_err("anchor missing");
        let text = err.to_string();
        assert!(!text.contains("sk-test-secret"), "leak: {text:?}");
        assert!(text.contains("keys.txt"));
    }

    #[test]
    fn diff_shapes_are_bounded_and_readable() {
        let created = render_diff(None, Some("a\nb\n"));
        assert!(created.contains("+a") && created.contains("+b"));
        let deleted = render_diff(Some("a\nb\n"), None);
        assert!(deleted.contains("-a"));
        let same = render_diff(Some("x\n"), Some("x\n"));
        assert!(same.contains("no changes"));
        let middle = render_diff(Some("1\n2\n3\n4\n5\n"), Some("1\n2\nX\n4\n5\n"));
        assert!(middle.contains("-3") && middle.contains("+X"));
        assert!(middle.contains("common"));
        // Long diffs truncate with a marker instead of growing unbounded.
        let big: String = (0..500).map(|i| format!("line{i}\n")).collect();
        let changed: String = (0..500).map(|i| format!("row{i}\n")).collect();
        let diff = render_diff(Some(&big), Some(&changed));
        assert!(diff.contains("more lines"));
        assert!(diff.lines().count() < 200);
    }

    #[test]
    fn oversized_payloads_rejected_at_prepare() {
        let (root, context) = context("oversize");
        write(&root.join("ok.txt"), b"fine\n");
        let big = "z".repeat(300 * 1024);
        let err = prepare_proposal(
            &context,
            1,
            MutationOp::CreateFile {
                path: "big.txt".into(),
                content: big,
            },
        )
        .expect_err("oversized");
        assert!(err.to_string().contains("write limit"));
    }

    #[test]
    fn cancelled_apply_does_nothing() {
        let (_root, context) = context("cancel-apply");
        let prepared = prepare(
            &context,
            1,
            MutationOp::CreateFile {
                path: "c.txt".into(),
                content: "x".into(),
            },
        );
        let err = context
            .apply_mutation(&prepared.op, None, Some(&|| true))
            .expect_err("cancelled");
        assert_eq!(err, ToolError::Cancelled);
        assert!(!_root.join("c.txt").exists());
    }

    #[test]
    fn run_command_ops_parse_prepare_and_execute() {
        use crate::detect_build_commands;
        let (root, context) = context("run-ops");
        // Ad-hoc command proposal parses with purpose required.
        let (operations, notes) = parse_proposals(
            "```aurel-mutation\n{\"op\": \"run_command\", \"program\": \"cargo\", \"args\": [\"--version\"], \"purpose\": \"check toolchain\"}\n```\n",
        );
        assert!(notes.is_empty(), "got notes: {notes:?}");
        assert!(matches!(
            operations.as_slice(),
            [MutationOp::RunCommand { .. }]
        ));
        let (operations, notes) = parse_proposals(
            "```aurel-mutation\n{\"op\": \"run_command\", \"program\": \"cargo\"}\n```\n",
        );
        assert!(operations.is_empty());
        assert!(
            notes.iter().any(|note| note.contains("purpose")),
            "got: {notes:?}"
        );

        // run_build / run_tests resolve against workspace markers.
        let (operations, _) = parse_proposals("```aurel-mutation\n{\"op\": \"run_build\"}\n```\n");
        assert!(matches!(operations.as_slice(), [MutationOp::RunBuild]));
        let (operations, _) =
            parse_proposals("```aurel-mutation\n{\"op\": \"run_tests\", \"extra\": true}\n```\n");
        assert!(operations.is_empty(), "extra fields must be strict");

        // No marker here: clean rejection, nothing queued.
        let err = prepare_proposal(&context, 1, MutationOp::RunBuild).expect_err("no marker");
        assert!(err.to_string().contains("no supported project marker"));

        // With a marker: prepares with a reviewable diff, executes for real.
        std::fs::write(root.join("Cargo.toml"), "[package]\n").expect("marker");
        assert_eq!(
            detect_build_commands(&root).expect("detected").kind,
            "cargo"
        );
        let prepared = prepare(&context, 2, MutationOp::RunTests);
        // The diff shows the resolved program plus purpose (paths vary by machine).
        assert!(prepared.diff.contains(" test"), "got: {:?}", prepared.diff);
        assert!(prepared.diff.contains("purpose: project tests via cargo"));
        let (change, output) = context
            .apply_mutation(&prepared.op, None, None)
            .expect("cargo test --help-shaped run");
        let output = output.expect("shell ops return their result");
        assert!(matches!(change, AppliedChange::ShellExecuted { .. }));
        // `cargo test` on an empty marker dir fails (no real crate), but it
        // must fail as a typed NonZeroExit — proving execution happened.
        assert!(matches!(output.status, crate::CommandStatus::NonZeroExit));
        // Undoing a shell execution is honestly refused.
        let err = context.undo_change(&change).expect_err("not undoable");
        assert!(matches!(err, ToolError::NotUndoable(_)), "got: {err:?}");
        assert!(err.to_string().contains("cannot undo"));
    }

    #[test]
    fn run_command_prepares_with_displayed_review() {
        let (_root, context) = context("run-review");
        let prepared = prepare(
            &context,
            5,
            MutationOp::RunCommand {
                program: "cargo".into(),
                args: vec!["--version".into()],
                purpose: "check toolchain".into(),
            },
        );
        assert!(prepared.diff.contains("check toolchain"));
        assert!(prepared.diff.contains("cwd:"));
        assert!(prepared.diff.contains("timeout:"));
        let (change, output) = context
            .apply_mutation(&prepared.op, None, None)
            .expect("cargo --version runs");
        let output = output.expect("shell result");
        assert_eq!(output.status, crate::CommandStatus::Success);
        assert_eq!(output.exit_code, Some(0));
        assert!(output.stdout.contains("cargo"), "got: {:?}", output.stdout);
        assert!(matches!(change, AppliedChange::ShellExecuted { .. }));
    }

    #[test]
    fn git_ops_parse_strictly_and_need_a_repo() {
        // All five Git ops decode from strict shapes.
        let cases = [
            (r#"{"op": "git_stage", "paths": ["a.txt"]}"#, "git_stage"),
            (
                r#"{"op": "git_unstage", "paths": ["a.txt"]}"#,
                "git_unstage",
            ),
            (r#"{"op": "git_commit", "message": "msg"}"#, "git_commit"),
            (
                r#"{"op": "git_create_branch", "name": "feat"}"#,
                "git_create_branch",
            ),
            (
                r#"{"op": "git_switch_branch", "name": "feat"}"#,
                "git_switch_branch",
            ),
        ];
        for (json, _) in &cases {
            let text = format!("```aurel-mutation\n{json}\n```\n");
            let (operations, notes) = parse_proposals(&text);
            assert!(notes.is_empty(), "got notes: {notes:?}");
            assert!(
                matches!(operations.as_slice(), [MutationOp::Git(_)]),
                "got: {operations:?}"
            );
        }
        // Strict shapes: unknown fields, missing fields, and remote-shaped
        // ops are notes, never proposals.
        for bad in [
            r#"{"op": "git_stage", "paths": ["a.txt"], "force": true}"#,
            r#"{"op": "git_stage"}"#,
            r#"{"op": "git_commit"}"#,
            r#"{"op": "git_push", "remote": "origin"}"#,
            r#"{"op": "git_pull"}"#,
            r#"{"op": "git_merge", "branch": "x"}"#,
        ] {
            let text = format!("```aurel-mutation\n{bad}\n```\n");
            let (operations, notes) = parse_proposals(&text);
            assert!(operations.is_empty(), "got: {operations:?}");
            assert_eq!(notes.len(), 1, "got: {notes:?}");
        }
        // Preparing any Git op outside a repository is a clean rejection.
        let (_root, context) = context("git-needs-repo");
        if crate::git_binary().is_err() {
            return;
        }
        let err = prepare_proposal(
            &context,
            1,
            MutationOp::Git(crate::GitOp::Stage {
                paths: vec!["x.txt".into()],
            }),
        )
        .expect_err("non-repo cannot prepare git ops");
        assert!(
            err.to_string().contains("not a git repository"),
            "got: {err:?}"
        );
    }

    #[test]
    fn git_apply_flows_through_apply_mutation_without_undo() {
        if crate::git_binary().is_err() {
            return;
        }
        let (root, context) = context("git-apply-path");
        let git = crate::git_binary().expect("git present");
        let sh = |args: &[&str]| {
            let status = std::process::Command::new(&git)
                .args(args)
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .status()
                .expect("git runs");
            assert!(status.success(), "git {args:?} failed");
        };
        sh(&["init"]);
        sh(&["config", "user.email", "aurel-test@example.com"]);
        sh(&["config", "user.name", "Aurel Test"]);
        std::fs::write(root.join("seed.txt"), b"seed\n").expect("seed");
        sh(&["add", "--", "seed.txt"]);
        sh(&["commit", "-m", "seed"]);
        std::fs::write(root.join("via.txt"), b"v\n").expect("write");

        // Prepare through the shared entry point, apply through the shared
        // mutation path: Git ops ride the exact approval machinery files do.
        let prepared = prepare_proposal(
            &context,
            1,
            MutationOp::Git(crate::GitOp::Stage {
                paths: vec!["via.txt".into()],
            }),
        )
        .expect("stage prepares");
        assert!(prepared.diff.contains("never touches remotes"));
        crate::verify_fresh(&context, &prepared).expect("fresh");
        let (change, output) = context
            .apply_mutation(&prepared.op, None, None)
            .expect("stage applies");
        let output = output.expect("git ops return their result");
        assert_eq!(output.status, crate::CommandStatus::Success);
        assert!(matches!(change, AppliedChange::GitExecuted { .. }));
        // Undo is honestly refused: no history rewriting, ever.
        let err = context.undo_change(&change).expect_err("not undoable");
        assert!(matches!(err, ToolError::NotUndoable(_)), "got: {err:?}");
    }
}
