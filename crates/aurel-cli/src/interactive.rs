//! Interactive layer (Phase 4): modes, slash commands, `@` scopes, the `!`
//! shell-request boundary, `/btw`, `/compact`, `/new`, and a minimal REPL.
//!
//! Slash commands are handled locally and never reach the model as prompts.
//! Backends that belong to later phases (`/model`, shell execution,
//! cross-session retrieval) report that honestly instead of pretending to
//! work. No TUI framework: plain line I/O over injected streams, so every
//! path is unit-testable.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use aurel_config::EffectiveConfig;
use aurel_model::{Agent, AgentConfig, AgentSession, CancelFlag, Mode, ModelProvider};
use aurel_session::{SessionData, SessionId, SessionStore};
use aurel_tools::{AppliedChange, InitOutcome, PendingProposal, AGENTS_MD};

use super::{
    build_provider, build_request, print_agent_outcome, Runtime, StreamSink, EXIT_RUNTIME_ERROR,
};

/// Shared empty-input hint, printed when the user submits a blank line.
/// Kept as one constant so the CLI and any future frontends (Desktop)
/// phrase empty input identically.
pub const EMPTY_INPUT_HINT: &str = "Ask anything, / for commands, @ for context, ! for shell...";

/// Context scope for one prompt (leading `@general` / `@explore` token).
/// A placeholder for future retrieval: today both scopes run against the
/// current session; `Explore` additionally says so out loud instead of
/// faking cross-session recall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextScope {
    General,
    Explore,
}

/// One parsed interactive input line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputKind {
    /// Blank line: reprompt.
    Empty,
    /// A line carrying a raw Tab: toggle Plan ↔ Build.
    TabToggle,
    /// Unparseable line with its diagnostic (unknown `@` directive).
    Invalid(String),
    Slash(SlashCommand),
    /// `!command`: an explicit user shell request. Parsed and dispatched
    /// here; execution belongs to the later shell-security phase and is
    /// refused with a clear message, never run.
    ShellRequest(String),
    /// A normal agent prompt with its scope annotation.
    Prompt {
        scope: ContextScope,
        text: String,
    },
}

/// Local slash commands. Anything whose backend lives in a later phase
/// reports that at dispatch time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    Help,
    Version,
    Plan,
    Build,
    Status,
    Compact,
    Btw(String),
    New,
    History,
    Context,
    Settings(SettingsAction),
    Model,
    Config,
    Tools,
    Init {
        /// Explicit replacement of an existing `AGENTS.md`.
        force: bool,
    },
    /// Apply the pending proposal (`/approve [#id]`).
    Approve {
        id: Option<u64>,
    },
    /// Drop the pending proposal (`/deny [#id]`).
    Deny {
        id: Option<u64>,
    },
    /// Re-show the pending proposal diff.
    Diff,
    /// Read-only local Git inspection (`/git status|diff|branches|log`).
    /// Never mutates; allowed in both Plan and Build modes.
    Git(GitAction),
    /// List saved persistent sessions (`/sessions`).
    Sessions,
    /// Resume a saved session (`/resume <id>`, unique prefixes allowed).
    /// Restores history, mode, and counters only — never proposals, undo
    /// records, or instructions, so resuming executes nothing.
    Resume {
        id: String,
    },
    /// Reverse the last AUREL-applied change.
    Undo,
    Exit,
    Unknown(String),
}

/// `/settings` sub-actions. Only `auto_compaction` is runtime-mutable (no
/// file writes exist in Phase 4, so changes are session-scoped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsAction {
    Show,
    SetAutoCompaction(bool),
    Invalid(String),
}

/// `/git` read-only inspection actions. All local, all read-only — the
/// mutating half of Git lives in the proposal queue, never here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitAction {
    Status,
    Diff { staged: bool },
    Branches,
    Log { limit: u32 },
}

/// Parse one raw input line. Total and deterministic: every shape maps to
/// exactly one variant.
pub fn parse_input_line(line: &str) -> InputKind {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // Terminals deliver a bare Tab inside the line; that toggles the
        // mode, while plain Enter just reprompts.
        if line.contains('\t') {
            return InputKind::TabToggle;
        }
        return InputKind::Empty;
    }
    if let Some(rest) = trimmed.strip_prefix('/') {
        return InputKind::Slash(parse_slash(rest));
    }
    if let Some(command) = trimmed.strip_prefix('!') {
        return InputKind::ShellRequest(command.trim().to_string());
    }
    if let Some(rest) = trimmed.strip_prefix('@') {
        return match rest.split_once(char::is_whitespace) {
            Some(("general", text)) => InputKind::Prompt {
                scope: ContextScope::General,
                text: text.trim().to_string(),
            },
            Some(("explore", text)) => InputKind::Prompt {
                scope: ContextScope::Explore,
                text: text.trim().to_string(),
            },
            _ if rest == "general" || rest == "explore" => {
                let scope = if rest == "general" {
                    ContextScope::General
                } else {
                    ContextScope::Explore
                };
                InputKind::Prompt {
                    scope,
                    text: String::new(),
                }
            }
            _ => {
                let name = rest.split(char::is_whitespace).next().unwrap_or(rest);
                InputKind::Invalid(format!(
                    "error: unknown context directive '@{name}' (expected @general or @explore)"
                ))
            }
        };
    }
    InputKind::Prompt {
        scope: ContextScope::General,
        text: trimmed.to_string(),
    }
}

fn parse_slash(rest: &str) -> SlashCommand {
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    match name {
        "help" => SlashCommand::Help,
        "version" => SlashCommand::Version,
        "plan" => SlashCommand::Plan,
        "build" => SlashCommand::Build,
        "status" => SlashCommand::Status,
        "compact" => SlashCommand::Compact,
        "btw" => SlashCommand::Btw(args.to_string()),
        "new" => SlashCommand::New,
        "history" => SlashCommand::History,
        "context" => SlashCommand::Context,
        "settings" => SlashCommand::Settings(parse_settings(args)),
        "model" => SlashCommand::Model,
        "config" => SlashCommand::Config,
        "tools" => SlashCommand::Tools,
        "init" => match args {
            "" => SlashCommand::Init { force: false },
            "--force" => SlashCommand::Init { force: true },
            _ => SlashCommand::Unknown(format!("init {args}")),
        },
        "approve" => match args {
            "" => SlashCommand::Approve { id: None },
            digits => match digits.parse::<u64>() {
                Ok(id) => SlashCommand::Approve { id: Some(id) },
                Err(_) => SlashCommand::Unknown(format!("approve {args}")),
            },
        },
        "deny" => match args {
            "" => SlashCommand::Deny { id: None },
            digits => match digits.parse::<u64>() {
                Ok(id) => SlashCommand::Deny { id: Some(id) },
                Err(_) => SlashCommand::Unknown(format!("deny {args}")),
            },
        },
        "diff" => SlashCommand::Diff,
        "git" => parse_git(args),
        "sessions" => SlashCommand::Sessions,
        "resume" => SlashCommand::Resume {
            id: args.to_string(),
        },
        "undo" => SlashCommand::Undo,
        "exit" | "quit" => SlashCommand::Exit,
        _ => SlashCommand::Unknown(name.to_string()),
    }
}

/// Parse `/git` arguments. Total: every shape maps to exactly one outcome;
/// anything unrecognized becomes `Unknown` so dispatch reports the standard
/// unknown-command error instead of guessing.
fn parse_git(args: &str) -> SlashCommand {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] | ["status"] => SlashCommand::Git(GitAction::Status),
        ["diff"] => SlashCommand::Git(GitAction::Diff { staged: false }),
        ["diff", "--staged"] => SlashCommand::Git(GitAction::Diff { staged: true }),
        ["branches"] => SlashCommand::Git(GitAction::Branches),
        ["log"] => SlashCommand::Git(GitAction::Log { limit: 10 }),
        ["log", count] => match count.parse::<u32>() {
            Ok(limit) if (1..=50).contains(&limit) => SlashCommand::Git(GitAction::Log { limit }),
            _ => SlashCommand::Unknown(format!("git {args}")),
        },
        _ => SlashCommand::Unknown(format!("git {args}")),
    }
}

fn parse_settings(args: &str) -> SettingsAction {
    let words: Vec<&str> = args.split_whitespace().collect();
    match words.as_slice() {
        [] => SettingsAction::Show,
        ["show"] => SettingsAction::Show,
        ["set", "auto_compaction", value] => match *value {
            "on" | "true" => SettingsAction::SetAutoCompaction(true),
            "off" | "false" => SettingsAction::SetAutoCompaction(false),
            _ => SettingsAction::Invalid(args.to_string()),
        },
        _ => SettingsAction::Invalid(args.to_string()),
    }
}

/// Per-message preview cap for `/history` (characters).
const HISTORY_PREVIEW_CHARS: usize = 500;

/// The interactive session: one agent, one history, locally-handled
/// commands. Generic over the provider for testability; production uses
/// `Repl<OpenAiCompatible>`.
pub struct Repl<P> {
    agent: Agent<P>,
    session: AgentSession,
    config: EffectiveConfig,
    /// Project root for `AGENTS.md` creation and instructions discovery
    /// (the working directory the loop started in). Mutation paths resolve
    /// against this same root through a fresh [`ToolContext`] per command.
    workdir: PathBuf,
    iterations_used: u32,
    compactions: u32,
    /// Queued mutation proposals awaiting explicit approval, front first.
    /// Nothing here has executed; `/approve` acts on the front only.
    pending: VecDeque<PendingProposal>,
    next_proposal_id: u64,
    /// Inverses of AUREL-applied changes, newest last. Only these records
    /// can ever be undone — nothing the user did outside AUREL is here.
    undo_stack: Vec<AppliedChange>,
    /// Stable ID of this persistent session (see `aurel-session`). A new
    /// ID is minted per loop and per `/new`; `/resume` adopts the loaded
    /// one. Shown in `/status`, used by `/sessions`.
    session_id: SessionId,
    /// Creation time of this session (0 = stamp on first save).
    session_created_ms: u64,
    /// Persistent session storage. `None` when no sessions directory is
    /// available — the loop then works purely in memory, and `/sessions`,
    /// `/resume`, and `@explore` retrieval report that honestly.
    store: Option<SessionStore>,
}

impl<P: ModelProvider> Repl<P> {
    pub fn new(
        provider: P,
        config: EffectiveConfig,
        workdir: PathBuf,
        store: Option<SessionStore>,
    ) -> Result<Self, aurel_model::ProviderError> {
        let agent = Agent::new(
            provider,
            AgentConfig {
                max_iterations: config.agent.max_iterations,
                streaming: config.model.streaming,
            },
        )?;
        let session_id = store
            .as_ref()
            .map(|store| store.create_id())
            .unwrap_or_else(SessionId::generate);
        Ok(Repl {
            agent,
            session: AgentSession::new(),
            config,
            workdir,
            iterations_used: 0,
            compactions: 0,
            pending: VecDeque::new(),
            next_proposal_id: 1,
            undo_stack: Vec::new(),
            session_id,
            session_created_ms: 0,
            store,
        })
    }

    pub fn mode(&self) -> Mode {
        self.session.mode()
    }

    /// Dispatch one parsed line. Returns false only to leave the loop.
    /// Command failures print and continue; they never exit the loop.
    pub fn dispatch(
        &mut self,
        input: &InputKind,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> bool {
        match input {
            InputKind::Empty => {
                let _ = writeln!(out, "{EMPTY_INPUT_HINT}");
                true
            }
            InputKind::TabToggle => {
                let mode = self.mode().toggle();
                self.session.set_mode(mode);
                let _ = writeln!(
                    out,
                    "Switched to {mode} mode (mutations {}).",
                    mutation_word(mode)
                );
                self.persist(err);
                true
            }
            InputKind::Invalid(message) => {
                let _ = writeln!(err, "{message}");
                true
            }
            InputKind::Slash(command) => self.dispatch_slash(command, out, err),
            InputKind::ShellRequest(command) => {
                self.queue_shell_request(command, out, err);
                true
            }
            InputKind::Prompt { scope, text } => {
                self.run_prompt(*scope, text, out, err);
                true
            }
        }
    }

    fn dispatch_slash(
        &mut self,
        command: &SlashCommand,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> bool {
        match command {
            SlashCommand::Help => {
                let _ = writeln!(out, "{REPL_HELP}");
                true
            }
            SlashCommand::Version => {
                let _ = writeln!(out, "aurel {}", aurel_core::version());
                true
            }
            SlashCommand::Plan => {
                self.session.set_mode(Mode::Plan);
                let _ = writeln!(out, "Switched to plan mode (mutations blocked).");
                self.persist(err);
                true
            }
            SlashCommand::Build => {
                self.session.set_mode(Mode::Build);
                let _ = writeln!(out, "Switched to build mode (mutations allowed).");
                self.persist(err);
                true
            }
            SlashCommand::Status => {
                self.print_status(out);
                true
            }
            SlashCommand::Compact => {
                self.run_compact(out, err);
                true
            }
            SlashCommand::Btw(question) => {
                self.run_btw(question, out, err);
                true
            }
            SlashCommand::New => {
                self.session.clear();
                self.iterations_used = 0;
                self.compactions = 0;
                self.pending.clear();
                self.undo_stack.clear();
                // A new conversation gets a new stable ID; the mode travels
                // on. Nothing is written yet — the file appears on first save.
                self.session_id = self
                    .store
                    .as_ref()
                    .map(|store| store.create_id())
                    .unwrap_or_else(SessionId::generate);
                self.session_created_ms = 0;
                let _ = writeln!(
                    out,
                    "New session started (mode preserved: {}, id: {}).",
                    self.mode(),
                    self.session_id.as_str()
                );
                true
            }
            SlashCommand::History => {
                self.print_history(out);
                true
            }
            SlashCommand::Context => {
                self.print_context(out);
                true
            }
            SlashCommand::Settings(action) => {
                self.run_settings(action, out, err);
                true
            }
            SlashCommand::Model => {
                let _ = writeln!(
                    out,
                    "Model switching is not implemented yet — configure the provider via flags, environment, or config files (see /config, /settings). (No action was taken.)"
                );
                true
            }
            SlashCommand::Config => {
                let _ = writeln!(out, "{}", aurel_config::render_show(&self.config));
                true
            }
            SlashCommand::Tools => {
                self.print_tools(out);
                true
            }
            SlashCommand::Init { force } => {
                self.run_init(*force, out, err);
                true
            }
            SlashCommand::Approve { id } => {
                self.run_approve(*id, out, err);
                true
            }
            SlashCommand::Deny { id } => {
                self.run_deny(*id, out, err);
                true
            }
            SlashCommand::Diff => {
                self.run_diff(out);
                true
            }
            SlashCommand::Git(action) => {
                self.run_git_action(action, out, err);
                true
            }
            SlashCommand::Sessions => {
                self.run_sessions(out, err);
                true
            }
            SlashCommand::Resume { id } => {
                self.run_resume(id, out, err);
                true
            }
            SlashCommand::Undo => {
                self.run_undo(out, err);
                true
            }
            SlashCommand::Exit => false,
            SlashCommand::Unknown(name) => {
                let _ = writeln!(err, "error: unknown command '/{name}' (try /help)");
                true
            }
        }
    }

    /// Run one main-task prompt: optional auto-compaction, then the bounded
    /// loop. Failures print and return to the prompt; only `/exit`/EOF ends
    /// the loop.
    fn run_prompt(
        &mut self,
        scope: ContextScope,
        text: &str,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) {
        if text.trim().is_empty() {
            let _ = writeln!(err, "error: no message given (type /help for commands)");
            return;
        }
        self.refresh_instructions(err);
        match self.agent.maybe_auto_compact(
            &mut self.session,
            self.config.agent.auto_compaction,
            None,
        ) {
            Ok(Some(report)) => {
                self.compactions += 1;
                let _ = writeln!(
                    out,
                    "auto-compacted {} → {} messages (summary {} chars).",
                    report.messages_before, report.messages_after, report.summary_chars
                );
            }
            Ok(None) => {}
            Err(error) => {
                let _ = writeln!(err, "warning: auto-compaction failed: {error} (continuing)");
            }
        }
        let cancel = CancelFlag::new();
        // The explore block is built before the sink borrows `out`: it
        // prints its own retrieval notice first, then streams the answer.
        let explore = if scope == ContextScope::Explore {
            Some(self.explore_block(out))
        } else {
            None
        };
        let mut sink = StreamSink::new(out);
        let outcome = match explore {
            Some(block) => self.agent.run_explore(
                &mut self.session,
                &block,
                text,
                Some(&cancel),
                &mut |event| sink.on_event(event),
            ),
            None => self
                .agent
                .run(&mut self.session, text, Some(&cancel), &mut |event| {
                    sink.on_event(event)
                }),
        };
        self.iterations_used += outcome.result().iterations;
        let failed = sink.failed;
        let out = sink.out;
        let _ = print_agent_outcome(&outcome, self.agent.config().streaming, failed, out, err);
        // Completed turns may carry model-proposed file mutations; collect
        // them for explicit review (never auto-applied).
        if matches!(outcome, aurel_model::AgentOutcome::Completed(_)) {
            self.collect_proposals(&outcome.result().content, out, err);
        }
        self.persist(err);
    }

    /// Build the one-shot `@explore` context block: other sessions'
    /// summaries plus project metadata. The block rides the current request
    /// only and is never stored; the printed line says exactly that.
    fn explore_block(&self, out: &mut dyn Write) -> String {
        match &self.store {
            Some(store) => {
                let others = store.summaries(&self.session_id);
                if others.is_empty() {
                    let _ = writeln!(
                        out,
                        "explore: no other saved sessions — answering from the current session plus project context."
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "explore: including context from {} other session(s) (this prompt only — nothing stored).",
                        others.len()
                    );
                }
                aurel_session::explore_context(
                    &self.session_id,
                    self.mode().into(),
                    &self.workdir,
                    &others,
                )
            }
            None => {
                let _ = writeln!(
                    out,
                    "note: session persistence is unavailable — answering from the current session only."
                );
                aurel_session::explore_context(
                    &self.session_id,
                    self.mode().into(),
                    &self.workdir,
                    &[],
                )
            }
        }
    }

    /// Answer a side question on a private clone of the session: the main
    /// history, mode, and counters are untouched, and control returns to
    /// the exact main task afterwards.
    fn run_btw(&mut self, question: &str, out: &mut dyn Write, err: &mut dyn Write) {
        if question.trim().is_empty() {
            let _ = writeln!(err, "error: usage: /btw <question>");
            return;
        }
        let _ = writeln!(out, "[btw] {question}");
        self.refresh_instructions(err);
        let cancel = CancelFlag::new();
        let mut sink = StreamSink::new(out);
        let outcome = self
            .agent
            .run_btw(&self.session, question, Some(&cancel), &mut |event| {
                sink.on_event(event)
            });
        let failed = sink.failed;
        let out = sink.out;
        let _ = print_agent_outcome(&outcome, self.agent.config().streaming, failed, out, err);
        let _ = writeln!(
            out,
            "[back to {} mode — main task unchanged ({} messages)]",
            self.mode(),
            self.session.history().len()
        );
    }

    /// Manual compaction (`/compact`).
    fn run_compact(&mut self, out: &mut dyn Write, err: &mut dyn Write) {
        match self.agent.compact(&mut self.session, None) {
            Ok(report) => {
                self.compactions += 1;
                let _ = writeln!(
                    out,
                    "Compacted {} → {} messages (summary {} chars).",
                    report.messages_before, report.messages_after, report.summary_chars
                );
                self.persist(err);
            }
            Err(error) => {
                let _ = writeln!(err, "{error}");
            }
        }
    }

    /// Workspace sandbox for mutation commands, rebuilt per call so a
    /// moved or deleted working directory fails cleanly instead of acting
    /// on a stale handle.
    fn tools(&self) -> Result<aurel_tools::ToolContext, aurel_tools::ToolError> {
        aurel_tools::ToolContext::new(&self.workdir)
    }

    /// Snapshot the resumable conversation state: history, mode, workdir,
    /// project marker, and counters. Pending proposals, undo records,
    /// instructions, and configuration are deliberately absent — resuming
    /// restores a conversation, never an approval queue or credentials.
    fn snapshot(&self) -> SessionData {
        SessionData {
            id: self.session_id.clone(),
            created_ms: self.session_created_ms,
            updated_ms: 0,
            mode: self.mode().into(),
            workdir: self.workdir.display().to_string(),
            project_kind: aurel_tools::detect_build_commands(&self.workdir)
                .map(|commands| commands.kind.to_string()),
            history: self.session.history().to_vec(),
            iterations_used: self.iterations_used,
            compactions: self.compactions,
        }
    }

    /// Persist the current snapshot. Best-effort by design: storage
    /// failures warn and the loop continues in memory — a session must
    /// never fail because its save did.
    fn persist(&self, err: &mut dyn Write) {
        let Some(store) = &self.store else {
            return;
        };
        if let Err(error) = store.save(&self.snapshot()) {
            let _ = writeln!(
                err,
                "warning: could not save session {}: {error} (continuing in memory)",
                self.session_id.as_str()
            );
        }
    }

    /// Collect model-proposed mutations from a completed turn into the
    /// pending queue. Works identically in both modes — the Plan/Build
    /// gate lives at approval time, so proposals survive a mode switch
    /// (freshness is re-verified then). Nothing here executes.
    fn collect_proposals(&mut self, content: &str, out: &mut dyn Write, err: &mut dyn Write) {
        let context = match self.tools() {
            Ok(context) => context,
            Err(error) => {
                let _ = writeln!(err, "warning: cannot prepare proposals: {error}");
                return;
            }
        };
        for proposal in review_proposals(&context, &mut self.next_proposal_id, content, out, err) {
            self.enqueue_prepared(proposal, out);
        }
        if !self.pending.is_empty() {
            self.print_review_hint(out);
        }
    }

    /// Queue one prepared proposal with its review display. Shared by
    /// model-proposed collection and explicit `!` shell requests.
    fn enqueue_prepared(&mut self, proposal: aurel_tools::PendingProposal, out: &mut dyn Write) {
        let _ = writeln!(out, "Proposal #{}: {}", proposal.id, proposal.op.summary());
        let _ = write!(out, "{}", proposal.diff);
        self.pending.push_back(proposal);
    }

    /// The review hint shown after queueing, plus the Plan-mode hold note.
    fn print_review_hint(&self, out: &mut dyn Write) {
        let _ = writeln!(out, "Review with /diff, then /approve [#id] or /deny.");
        if self.session.mode() != Mode::Build {
            let _ = writeln!(
                out,
                "Note: Plan mode holds proposals without applying — switch to Build to approve."
            );
        }
    }

    /// Approve the front pending proposal after re-verifying it: session
    /// mode must still be Build, the id (if given) must match, and prior
    /// filesystem state must read back byte-identical. Success records the
    /// inverse for session-scoped undo.
    fn run_approve(&mut self, id: Option<u64>, out: &mut dyn Write, err: &mut dyn Write) {
        let front = match self.pending.front() {
            Some(proposal) => proposal,
            None => {
                let _ = writeln!(err, "error: nothing pending — no mutation to approve");
                return;
            }
        };
        if id.is_some_and(|wanted| wanted != front.id) {
            let _ = writeln!(
                err,
                "error: stale approval (proposal #{} is pending, not #{})",
                front.id,
                id.expect("checked above")
            );
            return;
        }
        if self.session.mode() != Mode::Build {
            let _ = writeln!(
                err,
                "error: mode is Plan — switch to Build with /build or Tab to approve mutations"
            );
            return;
        }
        let context = match self.tools() {
            Ok(context) => context,
            Err(error) => {
                let _ = writeln!(err, "error: cannot apply proposal: {error}");
                return;
            }
        };
        if let Err(error) = aurel_tools::verify_fresh(&context, front) {
            self.pending.pop_front();
            let _ = writeln!(err, "{error} (proposal dropped)");
            return;
        }
        let proposal = self.pending.pop_front().expect("front checked above");
        // The configured key is scrubbed from any captured command output.
        let redact = self.config.model.api_key.clone();
        match context.apply_mutation(&proposal.op, redact.as_deref(), None) {
            Ok((change, output)) => {
                let is_git = matches!(change, AppliedChange::GitExecuted { .. });
                self.undo_stack.push(change);
                if let Some(result) = output {
                    if is_git {
                        print_git_result(&result, out);
                    } else {
                        print_command_result(&result, out);
                    }
                }
                let _ = writeln!(out, "Applied proposal #{}.", proposal.id);
            }
            Err(error) => {
                let _ = writeln!(err, "error: mutation failed: {error}");
            }
        }
    }

    /// Drop the front pending proposal without executing anything.
    fn run_deny(&mut self, id: Option<u64>, out: &mut dyn Write, err: &mut dyn Write) {
        let front = match self.pending.front() {
            Some(proposal) => proposal,
            None => {
                let _ = writeln!(err, "error: nothing pending — nothing to deny");
                return;
            }
        };
        if id.is_some_and(|wanted| wanted != front.id) {
            let _ = writeln!(
                err,
                "error: stale denial (proposal #{} is pending, not #{})",
                front.id,
                id.expect("checked above")
            );
            return;
        }
        let dropped = self.pending.pop_front().expect("front checked above");
        let _ = writeln!(out, "Denied proposal #{} (nothing executed).", dropped.id);
    }

    /// Re-show the front pending proposal diff.
    fn run_diff(&self, out: &mut dyn Write) {
        match self.pending.front() {
            Some(proposal) => {
                let _ = writeln!(out, "Proposal #{}: {}", proposal.id, proposal.op.summary());
                let _ = write!(out, "{}", proposal.diff);
                let _ = writeln!(out, "(use /approve [#{}] or /deny)", proposal.id);
            }
            None => {
                let _ = writeln!(out, "No pending proposals.");
            }
        }
    }

    /// Read-only local Git inspection. Allowed in every mode — nothing
    /// here stages, commits, branches, or touches remotes; the mutating
    /// half of Git lives in the proposal queue and needs `/approve`.
    fn run_git_action(&self, action: &GitAction, out: &mut dyn Write, err: &mut dyn Write) {
        let context = match self.tools() {
            Ok(context) => context,
            Err(error) => {
                let _ = writeln!(err, "error: cannot inspect git state: {error}");
                return;
            }
        };
        match action {
            GitAction::Status => match context.git_status(None) {
                Ok(status) => {
                    let _ = writeln!(out, "branch: {}", status.branch);
                    if let (Some(ahead), Some(behind)) = (status.ahead, status.behind) {
                        let _ = writeln!(out, "upstream: ahead {ahead}, behind {behind}");
                    } else if let Some(ahead) = status.ahead {
                        let _ = writeln!(out, "upstream: ahead {ahead}");
                    } else if let Some(behind) = status.behind {
                        let _ = writeln!(out, "upstream: behind {behind}");
                    }
                    if status.unborn {
                        let _ = writeln!(out, "(no commits yet)");
                    }
                    print_git_paths(out, "staged", &status.staged);
                    print_git_paths(out, "unstaged", &status.unstaged);
                    print_git_paths(out, "untracked", &status.untracked);
                    if status.is_clean() {
                        let _ = writeln!(out, "clean.");
                    }
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                }
            },
            GitAction::Diff { staged } => match context.git_diff(*staged, None) {
                Ok(diff) => {
                    if diff.staged {
                        let _ = writeln!(out, "staged changes (index vs HEAD):");
                    } else {
                        let _ = writeln!(out, "unstaged changes (worktree vs index):");
                    }
                    if diff.text.trim().is_empty() {
                        let _ = writeln!(out, "(no changes)");
                    } else {
                        let _ = write!(out, "{}", diff.text);
                        if !diff.text.ends_with('\n') {
                            let _ = writeln!(out);
                        }
                    }
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                }
            },
            GitAction::Branches => match context.git_branches(None) {
                Ok(branches) => {
                    if branches.is_empty() {
                        let _ = writeln!(out, "No local branches.");
                    }
                    for branch in branches {
                        let marker = if branch.current { '*' } else { ' ' };
                        if branch.subject.is_empty() {
                            let _ = writeln!(out, "{marker} {} {}", branch.name, branch.short_oid);
                        } else {
                            let _ = writeln!(
                                out,
                                "{marker} {} {} {}",
                                branch.name, branch.short_oid, branch.subject
                            );
                        }
                    }
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                }
            },
            GitAction::Log { limit } => match context.git_log(*limit, None) {
                Ok(entries) => {
                    if entries.is_empty() {
                        let _ = writeln!(out, "No commits yet.");
                    }
                    for entry in entries {
                        let _ = writeln!(out, "{} {}", entry.short_oid, entry.subject);
                    }
                }
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                }
            },
        }
    }

    /// List saved persistent sessions, newest first. The current session is
    /// marked with `*`. Read-only: works in both modes.
    fn run_sessions(&self, out: &mut dyn Write, err: &mut dyn Write) {
        let Some(store) = &self.store else {
            let _ = writeln!(
                err,
                "error: session persistence is unavailable (no sessions directory)"
            );
            return;
        };
        let listed = store.list();
        if listed.is_empty() {
            let _ = writeln!(out, "No saved sessions yet.");
            return;
        }
        let _ = writeln!(out, "sessions ({}):", listed.len());
        for meta in listed {
            let marker = if meta.id == self.session_id { "*" } else { " " };
            let _ = writeln!(
                out,
                "{marker} {}  {}  {} msgs  updated {}  {}",
                meta.id.as_str(),
                meta.mode.as_str(),
                meta.message_count,
                aurel_session::describe_age(meta.updated_ms),
                meta.workdir
            );
        }
    }

    /// Resume a saved session by ID or unambiguous prefix. Restores history,
    /// mode, and counters only: pending proposals and undo records start
    /// empty (they are never persisted), and instructions reload live from
    /// the working directory. Resuming therefore cannot execute anything.
    fn run_resume(&mut self, id: &str, out: &mut dyn Write, err: &mut dyn Write) {
        if id.trim().is_empty() {
            let _ = writeln!(err, "error: usage: /resume <id> (see /sessions)");
            return;
        }
        let data = match &self.store {
            Some(store) => match store.load(id.trim()) {
                Ok(data) => data,
                Err(error) => {
                    let _ = writeln!(err, "{error}");
                    return;
                }
            },
            None => {
                let _ = writeln!(
                    err,
                    "error: session persistence is unavailable (no sessions directory)"
                );
                return;
            }
        };
        self.session = AgentSession::new();
        for message in &data.history {
            self.session.push(message.clone());
        }
        self.session.set_mode(data.mode.into());
        self.refresh_instructions(err);
        self.session_id = data.id;
        self.session_created_ms = data.created_ms;
        self.iterations_used = data.iterations_used;
        self.compactions = data.compactions;
        self.pending.clear();
        self.undo_stack.clear();
        let _ = writeln!(
            out,
            "Resumed session {} ({} messages, {} mode).",
            self.session_id.as_str(),
            self.session.history().len(),
            self.mode()
        );
        let _ = writeln!(
            out,
            "Pending proposals and undo history start empty — nothing was executed."
        );
        if data.workdir != self.workdir.display().to_string() {
            let _ = writeln!(
                out,
                "Note: this session was started in '{}'; continuing in '{}'.",
                data.workdir,
                self.workdir.display()
            );
        }
        self.persist(err);
    }

    /// Reverse the last AUREL-applied change. Only inverses recorded in the
    /// session undo stack can run here, so undo never touches anything
    /// AUREL did not itself change.
    fn run_undo(&mut self, out: &mut dyn Write, err: &mut dyn Write) {
        let change = match self.undo_stack.pop() {
            Some(change) => change,
            None => {
                let _ = writeln!(err, "error: nothing to undo");
                return;
            }
        };
        let context = match self.tools() {
            Ok(context) => context,
            Err(error) => {
                let _ = writeln!(err, "error: cannot undo: {error}");
                return;
            }
        };
        match context.undo_change(&change) {
            Ok(()) => {
                let _ = writeln!(out, "Undone.");
            }
            Err(error) => {
                let _ = writeln!(err, "error: undo failed: {error}");
            }
        }
    }

    fn print_tools(&self, out: &mut dyn Write) {
        let _ = writeln!(out, "Tools (all read-only in this phase):");
        for tool in aurel_tools::tool_catalog() {
            let _ = writeln!(
                out,
                "  {} — {} [{}]",
                tool.name,
                tool.description,
                tool.permission.as_str()
            );
        }
    }

    /// Create `AGENTS.md` in the loop's working directory. Refuses to
    /// overwrite without explicit `--force`; prints paths in every case.
    fn run_init(&self, force: bool, out: &mut dyn Write, err: &mut dyn Write) {
        match aurel_tools::init_agents_md(&self.workdir, force) {
            Ok(InitOutcome::Created(path)) => {
                let _ = writeln!(
                    out,
                    "Created {} (starter template — edit it for your project).",
                    path.display()
                );
            }
            Ok(InitOutcome::Overwritten(path)) => {
                let _ = writeln!(out, "Overwrote {} (--force).", path.display());
            }
            Ok(InitOutcome::AlreadyExists(path)) => {
                let _ = writeln!(
                    err,
                    "error: {} already exists (not overwriting; use /init --force to replace it)",
                    path.display()
                );
            }
            Err(error) => {
                let _ = writeln!(err, "{error}");
            }
        }
    }

    /// Handle an explicit `!` shell request: split into program + literal
    /// arguments (no shell involved, ever), prepare it as a normal approval
    /// proposal, and queue it for `/approve`. Plan mode holds it like any
    /// other proposal. Nothing executes on this path.
    fn queue_shell_request(&mut self, command: &str, out: &mut dyn Write, err: &mut dyn Write) {
        let mut words = match split_shell_words(command) {
            Ok(words) => words,
            Err(error) => {
                let _ = writeln!(err, "{error}");
                return;
            }
        };
        let program = words.remove(0);
        let context = match self.tools() {
            Ok(context) => context,
            Err(error) => {
                let _ = writeln!(err, "warning: cannot prepare proposals: {error}");
                return;
            }
        };
        let op = aurel_tools::MutationOp::RunCommand {
            program,
            args: words,
            purpose: "explicit user shell request".to_string(),
        };
        let id = self.next_proposal_id;
        match aurel_tools::prepare_proposal(&context, id, op) {
            Ok(proposal) => {
                self.next_proposal_id += 1;
                self.enqueue_prepared(proposal, out);
                self.print_review_hint(out);
            }
            Err(error) => {
                let _ = writeln!(err, "{error}");
            }
        }
    }

    /// Refresh project instructions from the nearest `AGENTS.md` above the
    /// working directory. Absent files clear the session value so a deleted
    /// file stops applying; load failures warn and continue bare.
    fn refresh_instructions(&mut self, err: &mut dyn Write) {
        match aurel_tools::load_instructions_for_dir(&self.workdir) {
            Ok(found) => self
                .session
                .set_instructions(found.map(|loaded| loaded.content)),
            Err(error) => {
                self.session.set_instructions(None);
                let _ = writeln!(
                    err,
                    "warning: could not load {AGENTS_MD}: {error} (continuing)"
                );
            }
        }
    }

    fn print_status(&self, out: &mut dyn Write) {
        let _ = writeln!(
            out,
            "mode: {} (mutations {})",
            self.mode(),
            mutation_word(self.mode())
        );
        let _ = writeln!(out, "model: {}", self.config.model.name);
        let _ = writeln!(out, "endpoint: {}", self.config.model.base_url);
        let _ = writeln!(out, "streaming: {}", self.config.model.streaming);
        let _ = writeln!(out, "max-iterations: {}", self.config.agent.max_iterations);
        let _ = writeln!(
            out,
            "auto-compaction: {}",
            on_off(self.config.agent.auto_compaction)
        );
        let _ = writeln!(out, "session messages: {}", self.session.history().len());
        let _ = writeln!(out, "iterations used: {}", self.iterations_used);
        let _ = writeln!(out, "compactions: {}", self.compactions);
        let _ = writeln!(out, "pending proposals: {}", self.pending.len());
        let _ = writeln!(out, "undo depth: {}", self.undo_stack.len());
        let _ = writeln!(out, "session id: {}", self.session_id.as_str());
        match &self.store {
            Some(store) => {
                let _ = writeln!(out, "sessions dir: {}", store.dir().display());
            }
            None => {
                let _ = writeln!(out, "sessions dir: (unavailable — running in memory)");
            }
        }
    }

    fn print_history(&self, out: &mut dyn Write) {
        if self.session.history().is_empty() {
            let _ = writeln!(out, "Session history is empty.");
            return;
        }
        for (i, message) in self.session.history().iter().enumerate() {
            let _ = writeln!(
                out,
                "{}. [{:?}] {}",
                i + 1,
                message.role,
                preview(&message.content)
            );
        }
    }

    fn print_context(&self, out: &mut dyn Write) {
        let chars: usize = self.session.history().iter().map(|m| m.content.len()).sum();
        let _ = writeln!(
            out,
            "scope: general (per-message @general/@explore; @explore pulls other saved sessions' summaries into that prompt only)"
        );
        let _ = writeln!(
            out,
            "messages: {} ({} chars total)",
            self.session.history().len(),
            chars
        );
        let _ = writeln!(
            out,
            "auto-compaction: {} (threshold {} messages, keeps {})",
            on_off(self.config.agent.auto_compaction),
            aurel_model::COMPACT_AT_MESSAGES,
            aurel_model::COMPACT_KEEP_MESSAGES
        );
    }

    fn run_settings(&mut self, action: &SettingsAction, out: &mut dyn Write, err: &mut dyn Write) {
        match action {
            SettingsAction::Show => {
                let _ = writeln!(out, "[agent]");
                let _ = writeln!(out, "max_iterations = {}", self.config.agent.max_iterations);
                let _ = writeln!(
                    out,
                    "auto_compaction = {}",
                    self.config.agent.auto_compaction
                );
                let _ = writeln!(out, "Change with: /settings set auto_compaction on|off");
                let _ = writeln!(out, "(session-scoped: resets when the loop exits)");
            }
            SettingsAction::SetAutoCompaction(value) => {
                self.config.agent.auto_compaction = *value;
                let _ = writeln!(out, "auto_compaction = {}.", on_off(*value));
            }
            SettingsAction::Invalid(_) => {
                let _ = writeln!(
                    err,
                    "error: usage: /settings [show|set auto_compaction on|off]"
                );
            }
        }
    }

    /// Minimal line REPL: mode-visible prompt, EOF exits 0, unreadable
    /// stdin exits 1. Command failures never end the loop.
    pub fn run_loop(
        &mut self,
        input: &mut dyn BufRead,
        out: &mut dyn Write,
        err: &mut dyn Write,
    ) -> i32 {
        let _ = writeln!(
            out,
            "aurel interactive — {} mode (Tab toggles, /help for commands, Ctrl-D to exit).",
            self.mode()
        );
        loop {
            if write!(out, "{}> ", self.mode()).is_err() || out.flush().is_err() {
                return 1;
            }
            let mut line = String::new();
            match input.read_line(&mut line) {
                Ok(0) => return 0,
                Ok(_) => {}
                Err(_) => return 1,
            }
            if !self.dispatch(&parse_input_line(&line), out, err) {
                return 0;
            }
        }
    }
}

fn mutation_word(mode: Mode) -> &'static str {
    if mode.allows_mutation() {
        "allowed"
    } else {
        "blocked"
    }
}

/// Print a shell execution result: one verdict line, the exit code, then
/// captured output (already bounded and secret-scrubbed by the runner).
fn print_command_result(result: &aurel_tools::CommandResult, out: &mut dyn Write) {
    use aurel_tools::CommandStatus;
    match result.status {
        CommandStatus::Success => {
            let _ = writeln!(out, "Command exited 0.");
        }
        CommandStatus::NonZeroExit => {
            let _ = writeln!(
                out,
                "Command failed with exit code {}.",
                result
                    .exit_code
                    .map(|code| code.to_string())
                    .as_deref()
                    .unwrap_or("?")
            );
        }
        CommandStatus::Timeout => {
            let _ = writeln!(out, "Command timed out ({}).", result.detail);
        }
        CommandStatus::Cancelled => {
            let _ = writeln!(out, "Command cancelled.");
        }
        CommandStatus::LaunchFailed => {
            let _ = writeln!(out, "Command failed to start: {}.", result.detail);
        }
    }
    if !result.stdout.is_empty() {
        let _ = writeln!(out, "--- stdout ---");
        let _ = write!(out, "{}", result.stdout);
        if !result.stdout.ends_with('\n') {
            let _ = writeln!(out);
        }
    }
    if !result.stderr.is_empty() {
        let _ = writeln!(out, "--- stderr ---");
        let _ = write!(out, "{}", result.stderr);
        if !result.stderr.ends_with('\n') {
            let _ = writeln!(out);
        }
    }
    if result.truncated {
        let _ = writeln!(out, "(output truncated to the per-stream cap)");
    }
}

/// Print one path list of `/git status` (`(none)` keeps empty sections
/// explicit instead of silently absent).
fn print_git_paths(out: &mut dyn Write, label: &str, paths: &[String]) {
    if paths.is_empty() {
        let _ = writeln!(out, "{label}: (none)");
        return;
    }
    let _ = writeln!(out, "{label}:");
    for path in paths {
        let _ = writeln!(out, "  {path}");
    }
}

/// Print an approved local Git operation result: the same verdict shape as
/// shell results, labeled as Git, with the local-only boundary restated so
/// local and remote operations are never confused.
fn print_git_result(result: &aurel_tools::CommandResult, out: &mut dyn Write) {
    use aurel_tools::CommandStatus;
    match result.status {
        CommandStatus::Success => {
            let _ = writeln!(out, "Git exited 0 (local operation; remotes untouched).");
        }
        CommandStatus::NonZeroExit => {
            let _ = writeln!(
                out,
                "Git failed with exit code {} (local operation; remotes untouched).",
                result
                    .exit_code
                    .map(|code| code.to_string())
                    .as_deref()
                    .unwrap_or("?")
            );
        }
        CommandStatus::Timeout => {
            let _ = writeln!(out, "Git timed out ({}).", result.detail);
        }
        CommandStatus::Cancelled => {
            let _ = writeln!(out, "Git operation cancelled.");
        }
        CommandStatus::LaunchFailed => {
            let _ = writeln!(out, "Git failed to start: {}.", result.detail);
        }
    }
    if !result.stdout.is_empty() {
        let _ = writeln!(out, "--- git stdout ---");
        let _ = write!(out, "{}", result.stdout);
        if !result.stdout.ends_with('\n') {
            let _ = writeln!(out);
        }
    }
    if !result.stderr.is_empty() {
        let _ = writeln!(out, "--- git stderr ---");
        let _ = write!(out, "{}", result.stderr);
        if !result.stderr.ends_with('\n') {
            let _ = writeln!(out);
        }
    }
    if result.truncated {
        let _ = writeln!(out, "(output truncated to the per-stream cap)");
    }
}

/// Parse fenced mutation blocks out of model output, prepare each against
/// the workspace (resolve, snapshot, diff), print problems and diffs, and
/// return the queueable proposals with fresh ids. Pure coordination over
/// [`aurel_tools`] primitives — shared by the interactive loop and the
/// one-shot agent path so both review identically.
pub(crate) fn review_proposals(
    context: &aurel_tools::ToolContext,
    next_id: &mut u64,
    content: &str,
    _out: &mut dyn Write,
    err: &mut dyn Write,
) -> Vec<PendingProposal> {
    use aurel_tools::{parse_proposals, prepare_proposal, MAX_PROPOSALS_PER_RUN};
    let (operations, mut notes) = parse_proposals(content);
    if operations.len() > MAX_PROPOSALS_PER_RUN {
        notes.push(format!(
            "proposal cap reached ({} per reply); extras ignored",
            MAX_PROPOSALS_PER_RUN
        ));
    }
    let mut prepared = Vec::new();
    for op in operations.into_iter().take(MAX_PROPOSALS_PER_RUN) {
        let id = *next_id;
        *next_id += 1;
        match prepare_proposal(context, id, op) {
            Ok(proposal) => prepared.push(proposal),
            Err(error) => notes.push(format!("dropped proposal (#{id}): {error}")),
        }
    }
    for note in notes {
        let _ = writeln!(err, "note: {note}");
    }
    prepared
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() > max_chars {
        let clipped: String = text.chars().take(max_chars).collect();
        format!("{clipped}…")
    } else {
        text.to_string()
    }
}

fn preview(content: &str) -> String {
    truncate(content, HISTORY_PREVIEW_CHARS).replace('\n', " ")
}

/// Split a `!` shell line into program + literal arguments: whitespace
/// separates, double quotes group, backslash escapes the next character.
/// Everything else (pipes, redirects, globs, variables, single quotes) is
/// literal text — no shell ever interprets this line, so shell operators
/// are rejected downstream as unresolvable programs rather than executed.
fn split_shell_words(input: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_word = false;
    let mut chars = input.chars();
    while let Some(char) = chars.next() {
        if in_quotes {
            match char {
                '"' => in_quotes = false,
                '\\' => match chars.next() {
                    Some(escaped) => {
                        has_word = true;
                        current.push(escaped);
                    }
                    None => {
                        has_word = true;
                        current.push('\\');
                    }
                },
                _ => {
                    has_word = true;
                    current.push(char);
                }
            }
        } else {
            match char {
                '"' => {
                    in_quotes = true;
                    has_word = true;
                }
                '\\' => match chars.next() {
                    Some(escaped) => {
                        has_word = true;
                        current.push(escaped);
                    }
                    None => {
                        has_word = true;
                        current.push('\\');
                    }
                },
                _ if char.is_whitespace() => {
                    if has_word {
                        words.push(std::mem::take(&mut current));
                        has_word = false;
                    }
                }
                _ => {
                    has_word = true;
                    current.push(char);
                }
            }
        }
    }
    if in_quotes {
        return Err("error: unbalanced double quote (usage: !<program> [args])".to_string());
    }
    if has_word {
        words.push(current);
    }
    if words.is_empty() {
        return Err("error: usage: !<program> [args]".to_string());
    }
    Ok(words)
}

const REPL_HELP: &str = "\
Commands (local — never sent to the model):
  /help                 Show this help
  /version              Print the version
  /plan | /build        Switch modes (Tab toggles)
  /status               Session and configuration summary
   /compact              Summarize history into a compacted session
   /btw <question>       Side question (main task untouched)
   /new                  Start a fresh session (new id, mode preserved)
   /sessions             List saved persistent sessions (newest first)
   /resume <id>          Resume a saved session (history/mode/counters only)
  /history              Show session messages
  /context              Show context scope and usage
  /settings [show|set]  View or change session settings
  /model                Not implemented yet
  /config               Show effective configuration (key redacted)
  /tools                List registered tools (all read-only in this phase)
  /init [--force]       Create AGENTS.md starter (never overwrites silently)
   /approve [#id]        Apply the pending proposal (Build mode only)
   /deny [#id]           Drop the pending proposal without executing
   /diff                 Re-show the pending proposal diff
   /git <status|diff|branches|log>
                         Inspect the workspace Git repo (read-only, local only)
  /undo                 Reverse the last AUREL-applied change
  /exit | /quit         Leave the loop
@general / @explore prefix one prompt with a context scope.
@explore also pulls other saved sessions' summaries into that prompt only.
!command proposes a shell command for approval (direct execution, no shell).
A bare Tab toggles Plan ↔ Build. Ctrl-D exits.";

/// Build the provider/config snapshot from the live runtime and enter the
/// REPL. Used for bare `aurel` on a terminal; piped invocations keep the
/// Phase 0 help behavior.
pub fn start_interactive(
    rt: &Runtime,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    use super::args::Parsed;
    let cfg = match aurel_config::load(&build_request(&Parsed::default(), rt)) {
        Ok(cfg) => cfg,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    let provider = match build_provider(&cfg) {
        Ok(provider) => provider,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    // Persistent sessions live beside the global config. A missing or
    // unusable directory degrades to in-memory sessions with one warning —
    // the loop itself must never fail because storage did.
    let store = match aurel_config::global_sessions_dir(rt.appdata.as_deref(), rt.home.as_deref()) {
        Some(dir) => match aurel_session::SessionStore::open(&dir) {
            Ok(store) => Some(store),
            Err(error) => {
                let _ = writeln!(
                    err,
                    "warning: {error} (continuing without session persistence)"
                );
                None
            }
        },
        None => {
            let _ = writeln!(
                err,
                "warning: no home directory found (continuing without session persistence)"
            );
            None
        }
    };
    let mut repl = match Repl::new(provider, cfg, rt.cwd.clone(), store) {
        Ok(repl) => repl,
        Err(error) => {
            let _ = writeln!(err, "{error}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    repl.run_loop(input, out, err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aurel_config::LoadRequest;
    use aurel_model::{StreamControl, StreamEvent};
    use std::cell::Cell;
    use std::collections::VecDeque;

    /// Scripted provider double with canned chat outcomes. Records every
    /// received message list so tests can assert on request context.
    struct ScriptedProvider {
        script: std::cell::RefCell<
            VecDeque<Result<aurel_model::ChatResponse, aurel_model::ProviderError>>,
        >,
        calls: Cell<usize>,
        seen: std::cell::RefCell<Vec<Vec<aurel_model::Message>>>,
    }

    impl ScriptedProvider {
        fn reply(content: &str) -> Result<aurel_model::ChatResponse, aurel_model::ProviderError> {
            Ok(aurel_model::ChatResponse {
                content: content.to_string(),
                role: aurel_model::Role::Assistant,
                model: "scripted".to_string(),
                finish_reason: Some(aurel_model::FinishReason::Stop),
                usage: None,
            })
        }
    }

    impl ModelProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn capabilities(&self) -> aurel_model::Capabilities {
            aurel_model::Capabilities {
                streaming: false,
                tool_calling: false,
                structured_output: false,
                context_window: None,
            }
        }

        fn chat(
            &self,
            request: &aurel_model::ChatRequest,
        ) -> Result<aurel_model::ChatResponse, aurel_model::ProviderError> {
            self.calls.set(self.calls.get() + 1);
            self.seen.borrow_mut().push(request.messages.clone());
            self.script
                .borrow_mut()
                .pop_front()
                .expect("script exhausted")
        }

        fn chat_stream(
            &self,
            request: &aurel_model::ChatRequest,
            on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
        ) -> Result<aurel_model::ChatResponse, aurel_model::ProviderError> {
            // Behave like a real streaming provider: one delta carrying the
            // whole reply, honoring the consumer verdict.
            let response = self.chat(request)?;
            if !response.content.is_empty() {
                match on_event(StreamEvent {
                    delta: response.content.clone(),
                }) {
                    StreamControl::Continue => {}
                    StreamControl::Cancel => {
                        return Err(aurel_model::ProviderError::Cancelled);
                    }
                }
            }
            Ok(response)
        }
    }

    fn test_repl(
        script: Vec<Result<aurel_model::ChatResponse, aurel_model::ProviderError>>,
    ) -> Repl<ScriptedProvider> {
        test_repl_in(
            script,
            std::env::temp_dir().join(format!("aurel-repl-test-{}", std::process::id())),
        )
    }

    fn test_repl_in(
        script: Vec<Result<aurel_model::ChatResponse, aurel_model::ProviderError>>,
        workdir: std::path::PathBuf,
    ) -> Repl<ScriptedProvider> {
        test_repl_in_store(script, workdir, None)
    }

    fn test_repl_in_store(
        script: Vec<Result<aurel_model::ChatResponse, aurel_model::ProviderError>>,
        workdir: std::path::PathBuf,
        store: Option<SessionStore>,
    ) -> Repl<ScriptedProvider> {
        std::fs::create_dir_all(&workdir).expect("test workdir");
        let cfg = aurel_config::load(&LoadRequest::default()).expect("defaults load");
        Repl::new(
            ScriptedProvider {
                script: std::cell::RefCell::new(script.into()),
                calls: Cell::new(0),
                seen: std::cell::RefCell::new(Vec::new()),
            },
            cfg,
            workdir,
            store,
        )
        .expect("repl builds")
    }

    /// A real on-disk session store in an isolated temp directory.
    fn test_store(name: &str) -> SessionStore {
        let dir =
            std::env::temp_dir().join(format!("aurel-repl-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        SessionStore::open(&dir).expect("test store opens")
    }

    fn dispatch_to_string(
        repl: &mut Repl<ScriptedProvider>,
        input: &InputKind,
    ) -> (bool, String, String) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let cont = repl.dispatch(input, &mut out, &mut err);
        (
            cont,
            String::from_utf8(out).expect("utf8"),
            String::from_utf8(err).expect("utf8"),
        )
    }

    #[test]
    fn parses_every_command_shape() {
        use InputKind::*;
        use SlashCommand::*;
        assert_eq!(parse_input_line(""), Empty);
        assert_eq!(parse_input_line("   \n"), Empty);
        assert_eq!(parse_input_line("\t\n"), TabToggle);
        assert_eq!(parse_input_line("  \t  "), TabToggle);
        assert_eq!(parse_input_line("/help"), Slash(Help));
        assert_eq!(parse_input_line("/quit"), Slash(Exit));
        assert_eq!(parse_input_line("/exit"), Slash(Exit));
        assert_eq!(parse_input_line("/plan extra"), Slash(Plan));
        assert_eq!(
            parse_input_line("/btw what is this?"),
            Slash(Btw("what is this?".into()))
        );
        assert_eq!(
            parse_input_line("/settings"),
            Slash(Settings(SettingsAction::Show))
        );
        assert_eq!(
            parse_input_line("/settings set auto_compaction off"),
            Slash(Settings(SettingsAction::SetAutoCompaction(false)))
        );
        assert!(matches!(parse_input_line("/nope"), Slash(Unknown(_))));
        assert!(matches!(
            parse_input_line("/settings set bogus x"),
            Slash(Settings(SettingsAction::Invalid(_)))
        ));
        assert_eq!(parse_input_line("!ls -la"), ShellRequest("ls -la".into()));
        assert_eq!(
            parse_input_line("@general hi"),
            Prompt {
                scope: ContextScope::General,
                text: "hi".into()
            }
        );
        assert_eq!(
            parse_input_line("@explore hi"),
            Prompt {
                scope: ContextScope::Explore,
                text: "hi".into()
            }
        );
        assert!(matches!(parse_input_line("@bogus hi"), Invalid(_)));
        assert_eq!(
            parse_input_line("just a prompt"),
            Prompt {
                scope: ContextScope::General,
                text: "just a prompt".into()
            }
        );
    }

    #[test]
    fn mode_switching_and_tab_toggle() {
        let mut repl = test_repl(vec![]);
        assert_eq!(repl.mode(), Mode::Build);
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Plan));
        assert!(cont);
        assert_eq!(repl.mode(), Mode::Plan);
        assert!(out.contains("plan mode") && out.contains("blocked"));
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::TabToggle);
        assert!(cont);
        assert_eq!(repl.mode(), Mode::Build);
        assert!(out.contains("build mode") && out.contains("allowed"));
        let (cont, _, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Build));
        assert!(cont && repl.mode() == Mode::Build);
    }

    #[test]
    fn unknown_and_unimplemented_report_honestly() {
        let mut repl = test_repl(vec![]);
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Unknown("frobnicate".into())),
        );
        assert!(cont);
        assert!(err.contains("unknown command '/frobnicate'"));
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Model));
        assert!(cont);
        assert!(out.contains("not implemented yet"));
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Tools));
        assert!(cont);
        for name in ["read_file", "list_dir", "stat", "search"] {
            assert!(out.contains(name), "catalog must list {name}: {out:?}");
        }
        assert!(out.contains("read-only"), "got: {out:?}");
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::ShellRequest("cargo --version".into()),
        );
        assert!(cont);
        // Explicit requests queue for approval — never execute inline.
        assert!(out.contains("Proposal #1"), "got: {out:?}");
        assert!(out.contains("cargo"), "got: {out:?}");
        assert_eq!(repl.pending.len(), 1);
    }

    #[test]
    fn exit_stops_and_other_commands_continue() {
        let mut repl = test_repl(vec![]);
        let (cont, _, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Exit));
        assert!(!cont);
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Help));
        assert!(cont);
        assert!(out.contains("/btw") && out.contains("/compact"));
        let (cont, out, _) =
            dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Version));
        assert!(cont);
        assert!(out.contains("aurel "));
    }

    #[test]
    fn btw_isolation_and_return() {
        let mut repl = test_repl(vec![
            ScriptedProvider::reply("main-answer"),
            ScriptedProvider::reply("side-answer"),
        ]);
        // Main prompt first.
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Prompt {
                scope: ContextScope::General,
                text: "main task".into(),
            },
        );
        assert!(cont);
        assert!(out.contains("main-answer"));
        assert_eq!(repl.session.history().len(), 2);
        repl.session.set_mode(Mode::Plan);
        // Side question: answered, main history byte-identical afterwards.
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Btw("side question".into())),
        );
        assert!(cont);
        assert!(out.contains("[btw] side question"));
        assert!(out.contains("side-answer"));
        assert!(out.contains("main task unchanged (2 messages)"));
        assert!(out.contains("plan mode"));
        assert_eq!(repl.session.history().len(), 2);
        assert_eq!(repl.session.history()[1].content, "main-answer");
        assert_eq!(repl.mode(), Mode::Plan);
    }

    #[test]
    fn btw_empty_question_is_usage_error() {
        let mut repl = test_repl(vec![]);
        let (cont, _, err) =
            dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Btw("  ".into())));
        assert!(cont);
        assert!(err.contains("usage: /btw"));
    }

    #[test]
    fn compact_and_new_behaviors() {
        let mut repl = test_repl(vec![ScriptedProvider::reply("SUMMARY")]);
        for i in 0..6 {
            repl.session
                .push(aurel_model::Message::user(format!("q{i}")));
            repl.session
                .push(aurel_model::Message::assistant(format!("a{i}")));
        }
        let (cont, out, _) =
            dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Compact));
        assert!(cont);
        assert!(out.contains("Compacted 12 → 5 messages"), "got: {out:?}");
        assert_eq!(repl.session.history().len(), 5);
        assert_eq!(repl.compactions, 1);
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::New));
        assert!(cont);
        assert!(out.contains("New session started"));
        assert!(repl.session.history().is_empty());
        assert_eq!(repl.iterations_used, 0);
        assert_eq!(repl.compactions, 0);
    }

    #[test]
    fn status_history_context_settings() {
        let mut repl = test_repl(vec![ScriptedProvider::reply("hi")]);
        repl.session.set_mode(Mode::Plan);
        let (_, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Status));
        assert!(
            out.contains("mode: plan (mutations blocked)"),
            "got: {out:?}"
        );
        assert!(out.contains("session messages: 0"), "got: {out:?}");
        let (_, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Prompt {
                scope: ContextScope::General,
                text: "hello".into(),
            },
        );
        assert!(out.contains("hi"));
        let (_, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::History));
        assert!(
            out.contains("1. [User] hello") && out.contains("2. [Assistant] hi"),
            "got: {out:?}"
        );
        let (_, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Context));
        assert!(
            out.contains("messages: 2") && out.contains("auto-compaction: on"),
            "got: {out:?}"
        );
        // Settings default on, toggle off and back.
        let (_, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Settings(SettingsAction::Show)),
        );
        assert!(out.contains("auto_compaction = true"), "got: {out:?}");
        let (_, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Settings(SettingsAction::SetAutoCompaction(
                false,
            ))),
        );
        assert!(out.contains("auto_compaction = off"), "got: {out:?}");
        assert!(!repl.config.agent.auto_compaction);
        let (_, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Settings(SettingsAction::Invalid("x".into()))),
        );
        assert!(err.contains("usage: /settings"), "got: {err:?}");
    }

    #[test]
    fn run_loop_drives_lines_and_exits() {
        let mut repl = test_repl(vec![ScriptedProvider::reply("yo")]);
        let input = b"/plan\nhello\n/status\n/exit\n";
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = repl.run_loop(&mut &input[..], &mut out, &mut err);
        assert_eq!(code, 0);
        let out = String::from_utf8(out).expect("utf8");
        assert!(out.contains("plan> "), "prompt must show the mode: {out:?}");
        assert!(out.contains("Switched to plan mode"), "got: {out:?}");
        assert!(out.contains("yo"), "got: {out:?}");
        assert!(out.contains("mode: plan"), "got: {out:?}");
    }

    #[test]
    fn run_loop_eof_exits_cleanly() {
        let mut repl = test_repl(vec![]);
        let mut out = Vec::new();
        let mut err = Vec::new();
        assert_eq!(repl.run_loop(&mut &b""[..], &mut out, &mut err), 0);
    }

    #[test]
    fn config_command_redacts_key() {
        let mut repl = test_repl(vec![]);
        repl.config.model.api_key = Some("sk-test-00".into());
        let (_, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Config));
        assert!(out.contains("api_key = \"<redacted>\""), "got: {out:?}");
        assert!(!out.contains("sk-test-00"), "leak: {out:?}");
    }

    #[test]
    fn parses_init_forms() {
        assert_eq!(
            parse_input_line("/init"),
            InputKind::Slash(SlashCommand::Init { force: false })
        );
        assert_eq!(
            parse_input_line("/init --force"),
            InputKind::Slash(SlashCommand::Init { force: true })
        );
        assert!(matches!(
            parse_input_line("/init --bogus"),
            InputKind::Slash(SlashCommand::Unknown(_))
        ));
        let (_, out, _) = dispatch_to_string(
            &mut test_repl(vec![]),
            &InputKind::Slash(SlashCommand::Help),
        );
        assert!(
            out.contains("/init [--force]"),
            "help must document /init: {out:?}"
        );
    }

    #[test]
    fn init_creates_refuses_and_forces() {
        let workdir = std::env::temp_dir().join(format!("aurel-init-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workdir);
        let mut repl = test_repl_in(vec![], workdir.clone());

        // Creates with the starter template.
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Init { force: false }),
        );
        assert!(cont);
        assert!(out.contains("Created"), "got: {out:?}");
        assert!(out.contains("AGENTS.md"), "got: {out:?}");
        let text = std::fs::read_to_string(workdir.join("AGENTS.md")).expect("file created");
        assert!(text.contains("# AGENTS.md"), "got: {text:?}");

        // Refuses without force, naming the file and the flag.
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Init { force: false }),
        );
        assert!(cont);
        assert!(err.contains("already exists"), "got: {err:?}");
        assert!(err.contains("--force"), "got: {err:?}");

        // Explicit force replaces.
        std::fs::write(workdir.join("AGENTS.md"), "custom").expect("customize");
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Init { force: true }),
        );
        assert!(cont);
        assert!(out.contains("Overwrote"), "got: {out:?}");
        let text = std::fs::read_to_string(workdir.join("AGENTS.md")).expect("read back");
        assert!(!text.contains("custom"), "force must replace: {text:?}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn empty_input_prints_the_shared_hint() {
        let mut repl = test_repl(vec![]);
        let (cont, out, err) = dispatch_to_string(&mut repl, &InputKind::Empty);
        assert!(cont);
        assert_eq!(out, format!("{EMPTY_INPUT_HINT}\n"));
        assert!(err.is_empty());
    }

    #[test]
    fn run_prompt_loads_instructions_into_requests_not_history() {
        let workdir = std::env::temp_dir().join(format!("aurel-instr-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&workdir);
        std::fs::create_dir_all(&workdir).expect("mkdir");
        std::fs::write(workdir.join("AGENTS.md"), "Prefer tabs.\n").expect("instructions");
        let mut repl = test_repl_in(vec![ScriptedProvider::reply("ok")], workdir.clone());

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Prompt {
                scope: ContextScope::General,
                text: "do it".into(),
            },
        );
        assert!(cont);
        assert!(out.contains("ok"), "got: {out:?}");
        // The provider saw instructions first, then the user message.
        let seen = repl.agent.provider().seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].len(), 2);
        assert_eq!(seen[0][0].content, "Prefer tabs.\n");
        assert_eq!(seen[0][1].content, "do it");
        // History holds only the exchange itself.
        assert_eq!(repl.session.history().len(), 2);
        assert!(repl
            .session
            .history()
            .iter()
            .all(|message| message.content != "Prefer tabs.\n"));
        let _ = std::fs::remove_dir_all(&workdir);
    }

    fn p6_workdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aurel-p6-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test workdir");
        dir
    }

    fn fence(op_json: &str) -> String {
        format!("Sure, here it is:\n```aurel-mutation\n{op_json}\n```\nDone.\n")
    }

    fn prompt(text: String) -> InputKind {
        InputKind::Prompt {
            scope: ContextScope::General,
            text,
        }
    }

    #[test]
    fn parses_approval_forms() {
        assert_eq!(
            parse_input_line("/approve"),
            InputKind::Slash(SlashCommand::Approve { id: None })
        );
        assert_eq!(
            parse_input_line("/approve 3"),
            InputKind::Slash(SlashCommand::Approve { id: Some(3) })
        );
        assert!(matches!(
            parse_input_line("/approve x"),
            InputKind::Slash(SlashCommand::Unknown(_))
        ));
        assert_eq!(
            parse_input_line("/deny 2"),
            InputKind::Slash(SlashCommand::Deny { id: Some(2) })
        );
        assert_eq!(
            parse_input_line("/diff"),
            InputKind::Slash(SlashCommand::Diff)
        );
        assert_eq!(
            parse_input_line("/undo"),
            InputKind::Slash(SlashCommand::Undo)
        );
    }

    #[test]
    fn plan_holds_proposals_and_blocks_approval() {
        let workdir = p6_workdir("plan-hold");
        let reply = fence(r#"{"op": "create_file", "path": "p.txt", "content": "hi"}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], workdir.clone());
        repl.session.set_mode(Mode::Plan);

        let (cont, out, _) = dispatch_to_string(&mut repl, &prompt("do it".into()));
        assert!(cont);
        assert!(out.contains("Proposal #1"), "got: {out:?}");
        assert!(out.contains("Plan mode"), "got: {out:?}");
        assert_eq!(repl.pending.len(), 1);
        // Nothing executed in Plan mode.
        assert!(!workdir.join("p.txt").exists());

        // Approval stays rejected until the mode flips back.
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("Plan"), "got: {err:?}");
        assert_eq!(repl.pending.len(), 1);
        assert!(!workdir.join("p.txt").exists());

        let (cont, _, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Build));
        assert!(cont);
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(out.contains("Applied proposal #1"), "got: {out:?}");
        assert_eq!(
            std::fs::read_to_string(workdir.join("p.txt")).expect("created"),
            "hi"
        );
        assert!(repl.pending.is_empty());
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn approve_after_flip_to_plan_is_rejected() {
        // Propose in Build, flip to Plan, approve: rejected, queue intact.
        let workdir = p6_workdir("flip-to-plan");
        let reply = fence(r#"{"op": "create_file", "path": "f.txt", "content": "x"}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], workdir.clone());
        dispatch_to_string(&mut repl, &prompt("do it".into()));
        assert_eq!(repl.pending.len(), 1);
        dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Plan));
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("Plan"), "got: {err:?}");
        assert_eq!(repl.pending.len(), 1);
        assert!(!workdir.join("f.txt").exists());
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn build_collects_without_executing() {
        let workdir = p6_workdir("collect");
        let reply = fence(r#"{"op": "create_file", "path": "q.txt", "content": "hello"}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], workdir.clone());

        let (cont, out, _) = dispatch_to_string(&mut repl, &prompt("do it".into()));
        assert!(cont);
        assert!(out.contains("Proposal #1: create_file"), "got: {out:?}");
        assert!(out.contains("+hello"), "got: {out:?}");
        assert!(out.contains("/approve"), "got: {out:?}");
        assert_eq!(repl.pending.len(), 1);
        // Collection never executes: approval is the only path.
        assert!(!workdir.join("q.txt").exists());
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn approve_diff_deny_undo_flow() {
        let workdir = p6_workdir("flow");
        let reply = fence(r#"{"op": "create_file", "path": "f.txt", "content": "v1"}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], workdir.clone());
        dispatch_to_string(&mut repl, &prompt("do it".into()));

        // /diff re-shows the pending proposal.
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Diff));
        assert!(cont);
        assert!(
            out.contains("Proposal #1") && out.contains("+v1"),
            "got: {out:?}"
        );

        // /deny drops without executing.
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Deny { id: None }),
        );
        assert!(cont);
        assert!(out.contains("Denied proposal #1"), "got: {out:?}");
        assert!(repl.pending.is_empty());
        assert!(!workdir.join("f.txt").exists());

        // Re-propose (id 2), approve by explicit id, then undo restores.
        let reply = fence(r#"{"op": "create_file", "path": "f.txt", "content": "v2"}"#);
        repl.agent = Agent::new(
            ScriptedProvider {
                script: std::cell::RefCell::new(vec![ScriptedProvider::reply(&reply)].into()),
                calls: Cell::new(0),
                seen: std::cell::RefCell::new(Vec::new()),
            },
            AgentConfig {
                max_iterations: 5,
                streaming: false,
            },
        )
        .expect("rebuild agent");
        dispatch_to_string(&mut repl, &prompt("again".into()));
        assert_eq!(repl.pending.len(), 1);
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: Some(2) }),
        );
        assert!(cont);
        assert!(out.contains("Applied proposal #2"), "got: {out:?}");
        assert_eq!(
            std::fs::read_to_string(workdir.join("f.txt")).expect("created"),
            "v2"
        );
        assert_eq!(repl.undo_stack.len(), 1);

        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Undo));
        assert!(cont);
        assert!(out.contains("Undone"), "got: {out:?}");
        assert!(!workdir.join("f.txt").exists());
        assert!(repl.undo_stack.is_empty());

        // Empty states report honestly instead of acting.
        let (cont, _, err) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Undo));
        assert!(cont);
        assert!(err.contains("nothing to undo"), "got: {err:?}");
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("nothing pending"), "got: {err:?}");
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Diff));
        assert!(cont);
        assert!(out.contains("No pending proposals"), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn stale_ids_and_stale_files_fail_closed() {
        let workdir = p6_workdir("stale");
        std::fs::write(workdir.join("a.txt"), "one\n").expect("seed");
        let reply = fence(r#"{"op": "edit_file", "path": "a.txt", "old": "one", "new": "two"}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], workdir.clone());
        dispatch_to_string(&mut repl, &prompt("do it".into()));
        assert_eq!(repl.pending.len(), 1);

        // Wrong id: rejected, queue untouched.
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: Some(99) }),
        );
        assert!(cont);
        assert!(err.contains("stale approval"), "got: {err:?}");
        assert_eq!(repl.pending.len(), 1);

        // External edit: approve drops the proposal, file keeps its bytes.
        std::fs::write(workdir.join("a.txt"), "one changed\n").expect("external edit");
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("stale proposal #1"), "got: {err:?}");
        assert!(repl.pending.is_empty());
        assert_eq!(
            std::fs::read_to_string(workdir.join("a.txt")).expect("read"),
            "one changed\n"
        );
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn malformed_fences_become_notes() {
        let workdir = p6_workdir("malformed");
        let reply = "text\n```aurel-mutation\n{\"op\": \"nope\"}\n```\n```aurel-mutation\n{broken\n```\n```aurel-mutation\n{\"op\": \"delete_file\", \"path\": \"x\"}\n";
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(reply)], workdir.clone());
        let (cont, _, err) = dispatch_to_string(&mut repl, &prompt("do it".into()));
        assert!(cont);
        assert!(err.contains("unknown op"), "got: {err:?}");
        assert!(err.contains("malformed JSON"), "got: {err:?}");
        assert!(err.contains("unclosed"), "got: {err:?}");
        assert!(repl.pending.is_empty());
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn new_clears_approval_state() {
        let workdir = p6_workdir("new-clears");
        let reply = fence(r#"{"op": "create_file", "path": "n.txt", "content": "x"}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], workdir.clone());
        dispatch_to_string(&mut repl, &prompt("do it".into()));
        assert_eq!(repl.pending.len(), 1);
        let (_, _, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::New));
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("nothing pending"), "got: {err:?}");
        let (cont, _, err) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Undo));
        assert!(cont);
        assert!(err.contains("nothing to undo"), "got: {err:?}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn status_and_help_show_approval_state() {
        let workdir = p6_workdir("status-approval");
        let reply = fence(r#"{"op": "create_file", "path": "s.txt", "content": "x"}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], workdir.clone());
        dispatch_to_string(&mut repl, &prompt("do it".into()));
        let (_, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Status));
        assert!(out.contains("pending proposals: 1"), "got: {out:?}");
        assert!(out.contains("undo depth: 0"), "got: {out:?}");
        let (_, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Help));
        for command in ["/approve", "/deny", "/diff", "/undo", "/init"] {
            assert!(out.contains(command), "help must list {command}: {out:?}");
        }
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn review_cap_bounds_runaway_replies() {
        use aurel_tools::ToolContext;
        let workdir = p6_workdir("cap");
        let context = ToolContext::new(&workdir).expect("context");
        let mut text = String::new();
        for i in 0..10 {
            text.push_str(&format!(
                "```aurel-mutation\n{{\"op\": \"create_file\", \"path\": \"f{i}.txt\", \"content\": \"x\"}}\n```\n"
            ));
        }
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut next_id = 1u64;
        let queued = review_proposals(&context, &mut next_id, &text, &mut out, &mut err);
        assert_eq!(queued.len(), 8);
        assert_eq!(next_id, 9);
        let err = String::from_utf8(err).expect("utf8");
        assert!(err.contains("proposal cap reached"), "got: {err:?}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn split_shell_words_handles_quoting() {
        assert_eq!(
            split_shell_words("cargo test --lib").expect("simple"),
            vec!["cargo", "test", "--lib"]
        );
        assert_eq!(
            split_shell_words("run \"my file.txt\" plain").expect("quoted"),
            vec!["run", "my file.txt", "plain"]
        );
        assert_eq!(
            split_shell_words("echo a\\ b c").expect("escaped"),
            vec!["echo", "a b", "c"]
        );
        // Single quotes are literal (no shell to interpret them).
        assert_eq!(
            split_shell_words("echo 'hi'").expect("squotes"),
            vec!["echo", "'hi'"]
        );
        assert!(split_shell_words("").is_err());
        assert!(split_shell_words("   ").is_err());
        assert!(split_shell_words("run \"oops").is_err());
    }

    #[test]
    fn bang_queues_without_executing() {
        let workdir = p6_workdir("bang-queue");
        let mut repl = test_repl_in(vec![], workdir.clone());
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::ShellRequest("cargo --version".into()),
        );
        assert!(cont);
        assert!(out.contains("Proposal #1"), "got: {out:?}");
        assert!(out.contains("explicit user shell request"), "got: {out:?}");
        assert_eq!(repl.pending.len(), 1);
        assert!(repl.undo_stack.is_empty(), "queueing must not record undo");
        // Malformed shell lines are usage errors, not proposals.
        let (cont, _, err) =
            dispatch_to_string(&mut repl, &InputKind::ShellRequest("run \"oops".into()));
        assert!(cont);
        assert!(err.contains("unbalanced"), "got: {err:?}");
        assert_eq!(repl.pending.len(), 1);
        // Unknown programs are rejected with the program named.
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::ShellRequest("aurel-no-such-program-xyz".into()),
        );
        assert!(cont);
        assert!(err.contains("aurel-no-such-program-xyz"), "got: {err:?}");
        assert_eq!(repl.pending.len(), 1);
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn bang_approve_runs_and_undo_reports_honestly() {
        let workdir = p6_workdir("bang-run");
        let mut repl = test_repl_in(vec![], workdir.clone());
        dispatch_to_string(
            &mut repl,
            &InputKind::ShellRequest("cargo --version".into()),
        );
        assert_eq!(repl.pending.len(), 1);
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(out.contains("Command exited 0."), "got: {out:?}");
        assert!(out.contains("cargo"), "got: {out:?}");
        assert!(out.contains("Applied proposal #1"), "got: {out:?}");
        assert!(repl.pending.is_empty());
        assert_eq!(repl.undo_stack.len(), 1);
        // Shell effects cannot be reversed: undo says so explicitly.
        let (cont, _, err) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Undo));
        assert!(cont);
        assert!(err.contains("cannot undo"), "got: {err:?}");
        assert!(repl.undo_stack.is_empty());
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn bang_plan_blocks_approval() {
        let workdir = p6_workdir("bang-plan");
        let mut repl = test_repl_in(vec![], workdir.clone());
        repl.session.set_mode(Mode::Plan);
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::ShellRequest("cargo --version".into()),
        );
        assert!(cont);
        assert!(out.contains("Proposal #1"), "got: {out:?}");
        assert!(out.contains("Plan mode"), "got: {out:?}");
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("Plan"), "got: {err:?}");
        assert_eq!(repl.pending.len(), 1);
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn print_command_result_shapes() {
        use aurel_tools::{CommandResult, CommandStatus};
        fn rendered(result: &CommandResult) -> String {
            let mut out = Vec::new();
            print_command_result(result, &mut out);
            String::from_utf8(out).expect("utf8")
        }
        let base = CommandResult {
            program: "p".into(),
            status: CommandStatus::Success,
            exit_code: Some(0),
            stdout: "hi\n".into(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 3,
            detail: String::new(),
        };
        assert!(rendered(&base).contains("Command exited 0."));
        let failed = CommandResult {
            status: CommandStatus::NonZeroExit,
            exit_code: Some(2),
            ..base.clone()
        };
        assert!(rendered(&failed).contains("exit code 2"));
        let timed_out = CommandResult {
            status: CommandStatus::Timeout,
            exit_code: None,
            detail: "exceeded 1s timeout".into(),
            ..base.clone()
        };
        assert!(rendered(&timed_out).contains("timed out"));
        let cancelled = CommandResult {
            status: CommandStatus::Cancelled,
            exit_code: None,
            ..base.clone()
        };
        assert!(rendered(&cancelled).contains("cancelled"));
        let launch = CommandResult {
            status: CommandStatus::LaunchFailed,
            exit_code: None,
            detail: "spawn failed".into(),
            ..base.clone()
        };
        assert!(rendered(&launch).contains("failed to start"));
        let clipped = CommandResult {
            stdout: "x".into(),
            stderr: "y".into(),
            truncated: true,
            ..base.clone()
        };
        assert!(rendered(&clipped).contains("truncated"));
    }

    // -- Phase 8: Git ------------------------------------------------------

    fn git_workdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-git-repl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test workdir");
        dir
    }

    fn git_bin_or_skip() -> Option<PathBuf> {
        aurel_tools::git_binary().ok()
    }

    fn run_git(dir: &std::path::Path, args: &[&str]) {
        let git = git_bin_or_skip().expect("git fixture present");
        let status = std::process::Command::new(&git)
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    /// A repo workdir with repo-local identity and one commit. `None` when
    /// `git` is missing: tests skip instead of failing on thin machines.
    fn git_repo_workdir(name: &str) -> Option<PathBuf> {
        git_bin_or_skip()?;
        let dir = git_workdir(name);
        run_git(&dir, &["init"]);
        run_git(&dir, &["config", "user.email", "aurel-test@example.com"]);
        run_git(&dir, &["config", "user.name", "Aurel Test"]);
        // Closed before git runs (see the `aurel-tools` fixture note about
        // open handles staging empty blobs on Windows).
        std::fs::write(dir.join("seed.txt"), b"seed\n").expect("seed");
        run_git(&dir, &["add", "--", "seed.txt"]);
        run_git(&dir, &["commit", "-m", "seed commit"]);
        Some(dir)
    }

    fn tool_status(dir: &std::path::Path) -> aurel_tools::GitStatus {
        aurel_tools::ToolContext::new(dir)
            .expect("context builds")
            .git_status(None)
            .expect("status reads")
    }

    #[test]
    fn parses_git_command_shapes() {
        use SlashCommand::*;
        assert_eq!(
            parse_input_line("/git"),
            InputKind::Slash(Git(GitAction::Status))
        );
        assert_eq!(
            parse_input_line("/git status"),
            InputKind::Slash(Git(GitAction::Status))
        );
        assert_eq!(
            parse_input_line("/git diff"),
            InputKind::Slash(Git(GitAction::Diff { staged: false }))
        );
        assert_eq!(
            parse_input_line("/git diff --staged"),
            InputKind::Slash(Git(GitAction::Diff { staged: true }))
        );
        assert_eq!(
            parse_input_line("/git branches"),
            InputKind::Slash(Git(GitAction::Branches))
        );
        assert_eq!(
            parse_input_line("/git log"),
            InputKind::Slash(Git(GitAction::Log { limit: 10 }))
        );
        assert_eq!(
            parse_input_line("/git log 5"),
            InputKind::Slash(Git(GitAction::Log { limit: 5 }))
        );
        // Out-of-range limits and remote-flavored verbs never parse: there
        // is no remote surface to reach through `/git`.
        for bad in [
            "/git log 0",
            "/git log 51",
            "/git log many",
            "/git push",
            "/git pull",
            "/git fetch",
            "/git merge",
            "/git frobnicate",
            "/git diff --stat",
        ] {
            assert!(
                matches!(parse_input_line(bad), InputKind::Slash(Unknown(_))),
                "{bad} must be an unknown command"
            );
        }
    }

    #[test]
    fn git_inspection_is_read_only_and_plan_safe() {
        let Some(dir) = git_repo_workdir("inspect") else {
            return;
        };
        let mut repl = test_repl_in(vec![], dir.clone());
        repl.session.set_mode(Mode::Plan);

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Git(GitAction::Status)),
        );
        assert!(cont);
        assert!(out.contains("branch:"), "got: {out:?}");
        assert!(out.contains("staged: (none)"), "got: {out:?}");
        assert!(out.contains("clean."), "got: {out:?}");

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Git(GitAction::Branches)),
        );
        assert!(cont);
        assert!(out.contains('*'), "current branch must be marked: {out:?}");

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Git(GitAction::Log { limit: 5 })),
        );
        assert!(cont);
        assert!(out.contains("seed commit"), "got: {out:?}");

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Git(GitAction::Diff { staged: false })),
        );
        assert!(cont);
        assert!(out.contains("(no changes)"), "got: {out:?}");

        // Inspection queued nothing and recorded nothing.
        assert!(repl.pending.is_empty());
        assert!(repl.undo_stack.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_stage_propose_review_approve_flow() {
        let Some(dir) = git_repo_workdir("stage-flow") else {
            return;
        };
        std::fs::write(dir.join("work.txt"), b"v1\n").expect("write");
        let reply = fence(r#"{"op": "git_stage", "paths": ["work.txt"]}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], dir.clone());

        let (cont, out, _) = dispatch_to_string(&mut repl, &prompt("stage it".into()));
        assert!(cont);
        assert!(out.contains("Proposal #1: git stage"), "got: {out:?}");
        assert!(out.contains("never touches remotes"), "got: {out:?}");
        assert!(out.contains("git add"), "got: {out:?}");
        assert_eq!(repl.pending.len(), 1);
        // Proposal never self-executes.
        assert!(tool_status(&dir).staged.is_empty());

        // /diff re-shows the exact argv under review.
        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Diff));
        assert!(cont);
        assert!(
            out.contains("git add") && out.contains("work.txt"),
            "got: {out:?}"
        );

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(out.contains("Applied proposal #1"), "got: {out:?}");
        assert!(out.contains("Git exited 0"), "got: {out:?}");
        assert!(out.contains("remotes untouched"), "got: {out:?}");
        assert_eq!(tool_status(&dir).staged, vec!["work.txt".to_string()]);
        assert!(repl.pending.is_empty());
        assert_eq!(repl.undo_stack.len(), 1);

        // Git effects stand: undo is honestly refused, never a rewrite.
        let (cont, _, err) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Undo));
        assert!(cont);
        assert!(err.contains("cannot undo"), "got: {err:?}");
        assert!(err.contains("never rewrites history"), "got: {err:?}");
        assert_eq!(tool_status(&dir).staged, vec!["work.txt".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_plan_mode_blocks_approval_but_allows_inspection() {
        let Some(dir) = git_repo_workdir("plan-block") else {
            return;
        };
        std::fs::write(dir.join("held.txt"), b"h\n").expect("write");
        let reply = fence(r#"{"op": "git_stage", "paths": ["held.txt"]}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], dir.clone());
        repl.session.set_mode(Mode::Plan);

        let (cont, out, _) = dispatch_to_string(&mut repl, &prompt("stage it".into()));
        assert!(cont);
        assert!(out.contains("Proposal #1"), "got: {out:?}");
        assert!(out.contains("Plan mode"), "got: {out:?}");
        assert_eq!(repl.pending.len(), 1);
        assert!(tool_status(&dir).staged.is_empty());

        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("Plan"), "got: {err:?}");
        assert_eq!(repl.pending.len(), 1);
        assert!(tool_status(&dir).staged.is_empty());

        // Read-only inspection still answers in Plan mode.
        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Git(GitAction::Status)),
        );
        assert!(cont);
        assert!(out.contains("untracked:"), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_deny_drops_without_executing() {
        let Some(dir) = git_repo_workdir("deny") else {
            return;
        };
        std::fs::write(dir.join("drop.txt"), b"d\n").expect("write");
        let reply = fence(r#"{"op": "git_stage", "paths": ["drop.txt"]}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], dir.clone());
        dispatch_to_string(&mut repl, &prompt("stage it".into()));
        assert_eq!(repl.pending.len(), 1);

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Deny { id: None }),
        );
        assert!(cont);
        assert!(out.contains("Denied proposal #1"), "got: {out:?}");
        assert!(repl.pending.is_empty());
        assert!(tool_status(&dir).staged.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_stale_proposal_dropped_at_approve() {
        let Some(dir) = git_repo_workdir("stale") else {
            return;
        };
        std::fs::write(dir.join("a.txt"), b"a\n").expect("write");
        let reply = fence(r#"{"op": "git_stage", "paths": ["a.txt"]}"#);
        let mut repl = test_repl_in(vec![ScriptedProvider::reply(&reply)], dir.clone());
        dispatch_to_string(&mut repl, &prompt("stage it".into()));
        assert_eq!(repl.pending.len(), 1);

        // External Git activity behind the proposal's back.
        std::fs::write(dir.join("b.txt"), b"b\n").expect("write");
        run_git(&dir, &["add", "--", "b.txt"]);

        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Approve { id: None }),
        );
        assert!(cont);
        assert!(err.contains("stale proposal #1"), "got: {err:?}");
        assert!(repl.pending.is_empty());
        assert!(repl.undo_stack.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_commit_and_branch_flows() {
        let Some(dir) = git_repo_workdir("commit-branch") else {
            return;
        };
        std::fs::write(dir.join("work.txt"), b"v1\n").expect("write");
        let stage = fence(r#"{"op": "git_stage", "paths": ["work.txt"]}"#);
        let commit = fence(r#"{"op": "git_commit", "message": "add work"}"#);
        let create = fence(r#"{"op": "git_create_branch", "name": "feature-a"}"#);
        let switch = fence(r#"{"op": "git_switch_branch", "name": "feature-a"}"#);
        let mut repl = test_repl_in(
            vec![
                ScriptedProvider::reply(&stage),
                ScriptedProvider::reply(&commit),
                ScriptedProvider::reply(&create),
                ScriptedProvider::reply(&switch),
            ],
            dir.clone(),
        );

        for (step, text) in ["stage it", "commit it", "branch it", "switch it"]
            .iter()
            .enumerate()
        {
            let (cont, _, _) = dispatch_to_string(&mut repl, &prompt((*text).into()));
            assert!(cont, "step {step} prompts");
            let (cont, out, _) = dispatch_to_string(
                &mut repl,
                &InputKind::Slash(SlashCommand::Approve { id: None }),
            );
            assert!(cont, "step {step} approves");
            assert!(out.contains("Applied proposal"), "step {step}: {out:?}");
        }

        let context = aurel_tools::ToolContext::new(&dir).expect("context builds");
        let log = context.git_log(1, None).expect("log reads");
        assert_eq!(log[0].subject, "add work");
        let branches = context.git_branches(None).expect("branches read");
        assert!(branches
            .iter()
            .any(|branch| branch.name == "feature-a" && branch.current));
        let status = context.git_status(None).expect("status reads");
        assert_eq!(status.branch, "feature-a");
        assert!(status.is_clean(), "got: {status:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_outside_a_repo_reports_cleanly() {
        let workdir = git_workdir("non-repo");
        let mut repl = test_repl_in(vec![], workdir.clone());

        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Git(GitAction::Status)),
        );
        assert!(cont);
        assert!(err.contains("not a git repository"), "got: {err:?}");

        // Model-proposed Git ops in a non-repo become notes, never queue.
        let reply = fence(r#"{"op": "git_stage", "paths": ["x.txt"]}"#);
        repl.agent = Agent::new(
            ScriptedProvider {
                script: std::cell::RefCell::new(vec![ScriptedProvider::reply(&reply)].into()),
                calls: Cell::new(0),
                seen: std::cell::RefCell::new(Vec::new()),
            },
            AgentConfig {
                max_iterations: 5,
                streaming: false,
            },
        )
        .expect("rebuild agent");
        let (cont, _, err) = dispatch_to_string(&mut repl, &prompt("stage it".into()));
        assert!(cont);
        assert!(err.contains("not a git repository"), "got: {err:?}");
        assert!(repl.pending.is_empty());
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn print_git_result_labels_locality() {
        use aurel_tools::{CommandResult, CommandStatus};
        fn rendered(result: &CommandResult) -> String {
            let mut out = Vec::new();
            print_git_result(result, &mut out);
            String::from_utf8(out).expect("utf8")
        }
        let base = CommandResult {
            program: "git".into(),
            status: CommandStatus::Success,
            exit_code: Some(0),
            stdout: "Switched to branch 'x'\n".into(),
            stderr: String::new(),
            truncated: false,
            duration_ms: 3,
            detail: String::new(),
        };
        let text = rendered(&base);
        assert!(text.contains("Git exited 0"), "got: {text:?}");
        assert!(text.contains("remotes untouched"), "got: {text:?}");
        assert!(
            text.contains("Switched to branch"),
            "git output must be shown: {text:?}"
        );
        let failed = CommandResult {
            status: CommandStatus::NonZeroExit,
            exit_code: Some(1),
            stdout: String::new(),
            stderr: "nothing to commit\n".into(),
            ..base.clone()
        };
        let text = rendered(&failed);
        assert!(text.contains("exit code 1"), "got: {text:?}");
        assert!(
            text.contains("nothing to commit"),
            "git errors must be shown: {text:?}"
        );
    }

    // -- Phase 9: sessions -------------------------------------------------

    fn session_workdir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aurel-sess-repl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test workdir");
        dir
    }

    fn stored_file(store: &SessionStore, id: &str) -> PathBuf {
        store.dir().join(format!("{id}.json"))
    }

    #[test]
    fn parses_session_command_shapes() {
        assert_eq!(
            parse_input_line("/sessions"),
            InputKind::Slash(SlashCommand::Sessions)
        );
        assert_eq!(
            parse_input_line("/resume abc123"),
            InputKind::Slash(SlashCommand::Resume {
                id: "abc123".into()
            })
        );
        // Missing IDs stay parseable so dispatch can print usage (like /btw).
        assert_eq!(
            parse_input_line("/resume"),
            InputKind::Slash(SlashCommand::Resume { id: "".into() })
        );
        assert!(matches!(
            parse_input_line("/resumex"),
            InputKind::Slash(SlashCommand::Unknown(_))
        ));
    }

    #[test]
    fn prompts_persist_and_sessions_lists_with_current_marked() {
        let store = test_store("list");
        let workdir = session_workdir("list");
        let mut repl = test_repl_in_store(
            vec![ScriptedProvider::reply("hi there")],
            workdir.clone(),
            Some(store.clone()),
        );

        // Nothing saved before the first prompt: no litter.
        assert!(std::fs::read_dir(store.dir())
            .expect("read")
            .next()
            .is_none());
        let (cont, _, _) = dispatch_to_string(&mut repl, &prompt("hello".into()));
        assert!(cont);
        let id = repl.session_id.as_str().to_string();

        // The file holds history, mode, and counters — and no secrets.
        let raw = std::fs::read_to_string(stored_file(&store, &id)).expect("session file");
        assert!(
            raw.contains("hello") && raw.contains("hi there"),
            "got: {raw:?}"
        );
        assert!(raw.contains("\"build\""), "got: {raw:?}");
        assert!(!raw.to_lowercase().contains("api_key"), "got: {raw:?}");

        let (cont, out, _) =
            dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Sessions));
        assert!(cont);
        assert!(out.contains("sessions (1):"), "got: {out:?}");
        assert!(out.contains(&id), "got: {out:?}");
        assert!(out.contains("*"), "current session must be marked: {out:?}");
        assert!(out.contains("2 msgs"), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn resume_restores_history_mode_and_counters_without_executing() {
        let store = test_store("resume");
        let workdir = session_workdir("resume");
        let reply = fence(r#"{"op": "create_file", "path": "made.txt", "content": "x"}"#);
        let mut repl = test_repl_in_store(
            vec![ScriptedProvider::reply(&reply)],
            workdir.clone(),
            Some(store.clone()),
        );
        dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Plan));
        dispatch_to_string(&mut repl, &prompt("plan something".into()));
        let id = repl.session_id.as_str().to_string();
        assert_eq!(repl.pending.len(), 1, "proposal queued, not executed");

        // A fresh loop resumes by ID: history, mode, and counters return;
        // the approval queue starts empty and nothing runs.
        let mut revived = test_repl_in_store(vec![], workdir.clone(), Some(store.clone()));
        let (cont, out, _) = dispatch_to_string(
            &mut revived,
            &InputKind::Slash(SlashCommand::Resume { id: id.clone() }),
        );
        assert!(cont);
        assert!(out.contains(&id), "got: {out:?}");
        assert!(out.contains("2 messages"), "got: {out:?}");
        assert!(out.contains("plan mode"), "got: {out:?}");
        assert!(out.contains("nothing was executed"), "got: {out:?}");
        assert_eq!(revived.mode(), Mode::Plan);
        assert_eq!(revived.session.history().len(), 2);
        assert!(revived.pending.is_empty());
        assert!(revived.undo_stack.is_empty());
        assert!(!workdir.join("made.txt").exists());

        // Unique prefixes resolve too.
        let mut prefixed = test_repl_in_store(vec![], workdir.clone(), Some(store.clone()));
        let (cont, out, _) = dispatch_to_string(
            &mut prefixed,
            &InputKind::Slash(SlashCommand::Resume {
                id: id[..8].to_string(),
            }),
        );
        assert!(cont);
        assert!(out.contains("Resumed session"), "got: {out:?}");

        // Counters survive the round trip (one prompt ran above).
        let (cont, out, _) =
            dispatch_to_string(&mut revived, &InputKind::Slash(SlashCommand::Status));
        assert!(cont);
        assert!(out.contains("iterations used: 1"), "got: {out:?}");
        assert!(out.contains(&format!("session id: {id}")), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn resume_rejects_unknown_bad_and_corrupt_ids() {
        let store = test_store("resume-bad");
        let workdir = session_workdir("resume-bad");
        let mut repl = test_repl_in_store(vec![], workdir.clone(), Some(store.clone()));

        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Resume { id: "".into() }),
        );
        assert!(cont);
        assert!(err.contains("usage: /resume"), "got: {err:?}");

        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Resume {
                id: "20260918-120301-deadbeef".into(),
            }),
        );
        assert!(cont);
        assert!(err.contains("no such session"), "got: {err:?}");

        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Resume {
                id: "../evil".into(),
            }),
        );
        assert!(cont);
        assert!(err.contains("invalid session id"), "got: {err:?}");

        // A corrupt file fails loudly and leaves the live session alone.
        let bad = SessionId::generate();
        std::fs::write(stored_file(&store, bad.as_str()), b"{not json").expect("write");
        let before = repl.session.history().len();
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Resume {
                id: bad.as_str().to_string(),
            }),
        );
        assert!(cont);
        assert!(err.contains("corrupt session file"), "got: {err:?}");
        assert_eq!(repl.session.history().len(), before);
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn new_starts_a_fresh_conversation_but_keeps_saved_history() {
        let store = test_store("new");
        let workdir = session_workdir("new");
        let mut repl = test_repl_in_store(
            vec![ScriptedProvider::reply("first answer")],
            workdir.clone(),
            Some(store.clone()),
        );
        dispatch_to_string(&mut repl, &prompt("first".into()));
        let old_id = repl.session_id.as_str().to_string();

        let (cont, out, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::New));
        assert!(cont);
        assert!(out.contains("New session started"), "got: {out:?}");
        assert!(repl.session.history().is_empty());
        let new_id = repl.session_id.as_str().to_string();
        assert_ne!(old_id, new_id, "a fresh conversation gets a fresh id");

        // The old conversation is still resumable from disk.
        let mut revived = test_repl_in_store(vec![], workdir.clone(), Some(store.clone()));
        let (cont, out, _) = dispatch_to_string(
            &mut revived,
            &InputKind::Slash(SlashCommand::Resume { id: old_id }),
        );
        assert!(cont);
        assert!(out.contains("2 messages"), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn btw_leaves_the_persisted_session_untouched() {
        let store = test_store("btw");
        let workdir = session_workdir("btw");
        let mut repl = test_repl_in_store(
            vec![
                ScriptedProvider::reply("main answer"),
                ScriptedProvider::reply("side answer"),
            ],
            workdir.clone(),
            Some(store.clone()),
        );
        dispatch_to_string(&mut repl, &prompt("main task".into()));
        let id = repl.session_id.as_str().to_string();
        let saved = std::fs::read(stored_file(&store, &id)).expect("session file");

        let (cont, out, _) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Btw("side question".into())),
        );
        assert!(cont);
        assert!(out.contains("side answer"), "got: {out:?}");
        // The side question touched neither memory nor disk.
        assert_eq!(repl.session.history().len(), 2);
        assert_eq!(
            std::fs::read(stored_file(&store, &id)).expect("reread"),
            saved,
            "a /btw must not rewrite the session file"
        );
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn compact_then_resume_keeps_the_summary() {
        let store = test_store("compact");
        let workdir = session_workdir("compact");
        let mut repl = test_repl_in_store(
            vec![
                ScriptedProvider::reply("one"),
                ScriptedProvider::reply("two"),
                ScriptedProvider::reply("three"),
                ScriptedProvider::reply("SUMMARY"),
            ],
            workdir.clone(),
            Some(store.clone()),
        );
        for word in ["one", "two", "three"] {
            dispatch_to_string(&mut repl, &prompt(word.into()));
        }
        assert_eq!(repl.session.history().len(), 6);
        let (cont, out, _) =
            dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Compact));
        assert!(cont);
        assert!(out.contains("Compacted 6 → 5 messages"), "got: {out:?}");
        assert!(repl.session.history()[0]
            .content
            .starts_with("Session summary:"));

        let mut revived = test_repl_in_store(vec![], workdir.clone(), Some(store.clone()));
        let id = repl.session_id.as_str().to_string();
        let (cont, out, _) =
            dispatch_to_string(&mut revived, &InputKind::Slash(SlashCommand::Resume { id }));
        assert!(cont);
        assert!(out.contains("5 messages"), "got: {out:?}");
        assert!(
            revived.session.history()[0]
                .content
                .starts_with("Session summary:"),
            "compaction summary must survive the round trip"
        );
        let _ = std::fs::remove_dir_all(&workdir);
    }

    #[test]
    fn explore_pulls_other_session_context_without_storing_it() {
        let store = test_store("explore");
        let dir_a = session_workdir("explore-a");
        let dir_b = session_workdir("explore-b");
        let mut repl_a = test_repl_in_store(
            vec![ScriptedProvider::reply("alpha answer")],
            dir_a.clone(),
            Some(store.clone()),
        );
        dispatch_to_string(&mut repl_a, &prompt("alpha work".into()));

        let mut repl_b = test_repl_in_store(
            vec![ScriptedProvider::reply("beta answer")],
            dir_b.clone(),
            Some(store.clone()),
        );
        let (cont, out, _) = dispatch_to_string(
            &mut repl_b,
            &InputKind::Prompt {
                scope: ContextScope::Explore,
                text: "what relates?".into(),
            },
        );
        assert!(cont);
        assert!(out.contains("other session(s)"), "got: {out:?}");
        assert!(out.contains("beta answer"), "got: {out:?}");

        // The provider saw A's summary; B's stored history holds only its
        // own exchange.
        let seen = repl_b.agent.provider().seen.borrow();
        assert_eq!(seen.len(), 1);
        let context_first = seen[0]
            .iter()
            .find(|message| message.content.contains("alpha work"));
        assert!(
            context_first.is_some(),
            "provider must see cross-session context: {:?}",
            seen[0]
                .iter()
                .map(|message| &message.content)
                .collect::<Vec<_>>()
        );
        let stored: Vec<&str> = repl_b
            .session
            .history()
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(stored, ["what relates?", "beta answer"]);
        assert!(
            !stored.iter().any(|content| content.contains("alpha work")),
            "explore context must not persist: {stored:?}"
        );
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn sessions_degrade_honestly_without_a_store() {
        let mut repl = test_repl(vec![]);
        let (cont, _, err) =
            dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Sessions));
        assert!(cont);
        assert!(err.contains("unavailable"), "got: {err:?}");
        let (cont, _, err) = dispatch_to_string(
            &mut repl,
            &InputKind::Slash(SlashCommand::Resume { id: "abc".into() }),
        );
        assert!(cont);
        assert!(err.contains("unavailable"), "got: {err:?}");
        // Plain prompts still work in memory.
        let (cont, _, _) = dispatch_to_string(&mut repl, &InputKind::Slash(SlashCommand::Status));
        assert!(cont);
    }
}
