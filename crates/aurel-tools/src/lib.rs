//! `aurel-tools`: read-only core inspection tools plus opt-in file
//! mutations for AUREL.
//!
//! Inspection ([`ToolContext`]) only ever reads, behind one workspace
//! sandbox. Mutations ([`MutationOp`] and friends) never execute on their
//! own: they are proposed, snapshotted, diffed, and only applied after
//! explicit user approval, with session-scoped undo.
//!
//! [`ProjectInstructions`] covers `AGENTS.md`: starter template, guarded
//! creation, upward discovery, and bounded loading.
//!
//! Near-zero dependencies by design: only `serde`/`serde_json` for parsing
//! model-proposed mutation blocks (both already in the workspace graph).
//! Inspection itself stays standard-library only.

mod command;
mod context;
mod error;
mod instructions;
mod mutation;

pub use command::{
    detect_build_commands, resolve_program, run_command, scrub_secret, system_path_dirs,
    BuildCommands, CommandRequest, CommandResult, CommandStatus,
};
pub use context::{
    tool_catalog, DirEntry, DirListing, EntryKind, FileStat, Limits, Match, Permission,
    SearchOptions, SearchResults, ToolContext, ToolInfo,
};
pub use error::ToolError;
pub use instructions::{
    discover_agents_md, init_agents_md, load_agents_md, load_instructions_for_dir,
    starter_template, InitOutcome, ProjectInstructions, AGENTS_MD, MAX_INSTRUCTIONS_BYTES,
};
pub use mutation::{
    parse_proposals, prepare_proposal, render_diff, verify_fresh, AppliedChange, CommandKind,
    MutationOp, PendingProposal, ProposalError, ResolvedOp, FENCE_TAG, MAX_DIFF_LINES,
    MAX_PROPOSALS_PER_RUN,
};
