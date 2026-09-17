//! `aurel-tools`: read-only core inspection tools for AUREL.
//!
//! Everything here inspects; nothing mutates. The shared entry point is
//! [`ToolContext`], bound to one workspace root: every path a tool touches
//! is resolved through [`ToolContext::resolve`], which canonicalizes (so
//! symlinks cannot escape) and rejects anything outside the root.
//!
//! [`ProjectInstructions`] covers `AGENTS.md`: starter template, guarded
//! creation, upward discovery, and bounded loading.
//!
//! Standard library only, by design: inspection must stay cheap enough
//! that linking it changes Core's footprint negligibly.

mod context;
mod error;
mod instructions;

pub use context::{
    tool_catalog, DirEntry, DirListing, EntryKind, FileStat, Limits, Match, Permission,
    SearchOptions, SearchResults, ToolContext, ToolInfo,
};
pub use error::ToolError;
pub use instructions::{
    discover_agents_md, init_agents_md, load_agents_md, load_instructions_for_dir,
    starter_template, InitOutcome, ProjectInstructions, AGENTS_MD, MAX_INSTRUCTIONS_BYTES,
};
