//! Interactive layer (Phase 4): modes, slash commands, `@` scopes, the `!`
//! shell-request boundary, `/btw`, `/compact`, `/new`, and a minimal REPL.
//!
//! Slash commands are handled locally and never reach the model as prompts.
//! Backends that belong to later phases (`/model`, shell execution,
//! cross-session retrieval) report that honestly instead of pretending to
//! work. No TUI framework: plain line I/O over injected streams, so every
//! path is unit-testable.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use aurel_config::EffectiveConfig;
use aurel_model::{Agent, AgentConfig, AgentSession, CancelFlag, Mode, ModelProvider};
use aurel_tools::{InitOutcome, AGENTS_MD};

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
        "exit" | "quit" => SlashCommand::Exit,
        _ => SlashCommand::Unknown(name.to_string()),
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
    /// (the working directory the loop started in).
    workdir: PathBuf,
    iterations_used: u32,
    compactions: u32,
}

impl<P: ModelProvider> Repl<P> {
    pub fn new(
        provider: P,
        config: EffectiveConfig,
        workdir: PathBuf,
    ) -> Result<Self, aurel_model::ProviderError> {
        let agent = Agent::new(
            provider,
            AgentConfig {
                max_iterations: config.agent.max_iterations,
                streaming: config.model.streaming,
            },
        )?;
        Ok(Repl {
            agent,
            session: AgentSession::new(),
            config,
            workdir,
            iterations_used: 0,
            compactions: 0,
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
                true
            }
            InputKind::Invalid(message) => {
                let _ = writeln!(err, "{message}");
                true
            }
            InputKind::Slash(command) => self.dispatch_slash(command, out, err),
            InputKind::ShellRequest(command) => {
                if command.is_empty() {
                    let _ = writeln!(err, "error: usage: !<command>");
                } else {
                    let shown = truncate(command, 200);
                    let _ = writeln!(
                        out,
                        "Shell execution is not implemented yet — the shell-security phase will gate it. Received (not run): {shown}"
                    );
                }
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
                true
            }
            SlashCommand::Build => {
                self.session.set_mode(Mode::Build);
                let _ = writeln!(out, "Switched to build mode (mutations allowed).");
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
                let _ = writeln!(
                    out,
                    "New session started (mode preserved: {}).",
                    self.mode()
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
        if scope == ContextScope::Explore {
            let _ = writeln!(
                out,
                "note: cross-session retrieval is not implemented yet; answering from the current session only."
            );
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
        let mut sink = StreamSink::new(out);
        let outcome = self
            .agent
            .run(&mut self.session, text, Some(&cancel), &mut |event| {
                sink.on_event(event)
            });
        self.iterations_used += outcome.result().iterations;
        let failed = sink.failed;
        let out = sink.out;
        let _ = print_agent_outcome(&outcome, self.agent.config().streaming, failed, out, err);
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
            }
            Err(error) => {
                let _ = writeln!(err, "{error}");
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
            "scope: general (per-message @general/@explore; @explore answers from this session — cross-session retrieval is not implemented yet)"
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

const REPL_HELP: &str = "\
Commands (local — never sent to the model):
  /help                 Show this help
  /version              Print the version
  /plan | /build        Switch modes (Tab toggles)
  /status               Session and configuration summary
  /compact              Summarize history into a compacted session
  /btw <question>       Side question (main task untouched)
  /new                  Start a fresh session
  /history              Show session messages
  /context              Show context scope and usage
  /settings [show|set]  View or change session settings
  /model                Not implemented yet
  /config               Show effective configuration (key redacted)
  /tools                List registered tools (all read-only in this phase)
  /init [--force]       Create AGENTS.md starter (never overwrites silently)
  /exit | /quit         Leave the loop
@general / @explore prefix one prompt with a context scope.
!command names an explicit shell request (not run yet).
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
    let mut repl = match Repl::new(provider, cfg, rt.cwd.clone()) {
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
        )
        .expect("repl builds")
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
        let (cont, out, _) =
            dispatch_to_string(&mut repl, &InputKind::ShellRequest("rm -rf /".into()));
        assert!(cont);
        assert!(out.contains("not implemented yet") && out.contains("not run"));
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
}
