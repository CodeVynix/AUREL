//! Basic bounded agent loop (Phase 3). No tools, no autonomy beyond asking
//! the model to continue: each run sends the session history, appends the
//! reply, and only iterates again when the turn was truncated
//! ([`FinishReason::Length`]), up to [`AgentConfig::max_iterations`].
//!
//! Synchronous throughout, generic over [`ModelProvider`] (no HTTP here),
//! cancellation via the Phase 2 [`CancelFlag`].

use crate::{
    CancelFlag, ChatRequest, FinishReason, Message, ModelProvider, ProviderError, StreamControl,
    StreamEvent, Usage,
};

/// Hard cap for `max_iterations`: a bounded loop must stay bounded even
/// against absurd configuration.
pub const MAX_ITERATIONS_LIMIT: u32 = 100;

/// History length above which automatic compaction may trigger.
pub const COMPACT_AT_MESSAGES: usize = 20;
/// Newest messages a compaction always keeps verbatim.
pub const COMPACT_KEEP_MESSAGES: usize = 4;
/// Per-message transcript cap (characters) when asking for a summary.
pub const COMPACT_MESSAGE_CHARS: usize = 2000;
/// Max messages folded into one summary request. With auto-compaction on
/// the prefix is tiny; with it off (or a giant manual `/compact`) the
/// request is capped to the newest messages with an explicit marker, so
/// one compaction can never build a gigabyte prompt.
pub const MAX_COMPACT_TRANSCRIPT_MESSAGES: usize = 500;

/// Continuation cue appended (as `system`) after a truncated turn so the
/// next request deterministically asks for the rest instead of repeating
/// the same truncated prefix.
const CONTINUE_CUE: &str = "Continue.";

/// Instruction framing every compaction summary request.
const SUMMARIZE_SYSTEM: &str = "You are a precise session summarizer for a coding assistant. Summarize the numbered conversation below into a short paragraph preserving: the user's goal, key decisions, and any facts needed to continue the work. Omit chit-chat.";

/// Interaction mode. One shared agent implementation serves both modes;
/// the mode travels with the session and is stamped into every result.
///
/// Plan mode must not perform mutations. No mutating capability exists yet
/// (tools arrive in Phase 4+), so the enforcement point is
/// [`Mode::allows_mutation`]: every future tool call must check it before
/// acting, and review must verify that gate rather than trusting callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Normal operation (may mutate once tools exist).
    #[default]
    Build,
    /// Planning only: reason and propose, never mutate.
    Plan,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Build => "build",
            Mode::Plan => "plan",
        }
    }

    /// Raw-Tab and `/plan`–`/build` toggle through this.
    pub fn toggle(self) -> Self {
        match self {
            Mode::Build => Mode::Plan,
            Mode::Plan => Mode::Build,
        }
    }

    /// Whether side-effecting operations are permitted in this mode.
    /// Phase 5+ tool implementations MUST consult this before mutating.
    pub fn allows_mutation(self) -> bool {
        match self {
            Mode::Build => true,
            Mode::Plan => false,
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentConfig {
    /// Maximum provider calls per [`Agent::run`]. Must be 1..=100.
    pub max_iterations: u32,
    /// Stream turns progressively when the provider supports it.
    pub streaming: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            // Generous enough for real multi-part answers, small enough
            // that a runaway run stays cheap: 5 requests worst case.
            max_iterations: 5,
            streaming: true,
        }
    }
}

/// In-memory conversation history for one agent session. Persisted across
/// runs by the session layer (Phase 9: stable IDs, bounded JSON files —
/// history, mode, and counters only, never credentials); dropped with the
/// process when persistence is unavailable.
///
/// Also carries the interaction [`Mode`] and optional project instructions
/// (`AGENTS.md` content): `/new` (via [`AgentSession::clear`]) resets
/// history but preserves the mode. Instructions are *not* history —
/// [`Agent::run`] prepends them to each request without storing them, and
/// the session layer never writes them to disk (they reload live from the
/// working directory on every run and every resume).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AgentSession {
    history: Vec<Message>,
    mode: Mode,
    instructions: Option<String>,
}

impl AgentSession {
    pub fn new() -> Self {
        AgentSession {
            history: Vec::new(),
            mode: Mode::default(),
            instructions: None,
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn push(&mut self, message: Message) {
        self.history.push(message);
    }

    /// Drop history but keep the interaction mode (and instructions, which
    /// describe the project rather than the conversation).
    pub fn clear(&mut self) {
        self.history.clear();
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// Project instructions (`AGENTS.md` content) for every run on this
    /// session. Sent as a leading `system` message per request, never
    /// stored in [`AgentSession::history`].
    pub fn set_instructions(&mut self, instructions: Option<String>) {
        self.instructions = instructions;
    }

    pub fn instructions(&self) -> Option<&str> {
        self.instructions.as_deref()
    }
}

/// Combined result of one [`Agent::run`]: assistant text accumulated across
/// every iteration, how many provider calls that took, trailing metadata,
/// and the [`Mode`] that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentResult {
    pub content: String,
    pub iterations: u32,
    pub finish_reason: Option<FinishReason>,
    pub usage: Option<Usage>,
    pub mode: Mode,
}

/// How one [`Agent::run`] ended. Every variant carries what was produced so
/// far, so callers never lose partial work silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOutcome {
    /// A turn finished for a reason other than truncation.
    Completed(AgentResult),
    /// Still truncated after `max_iterations` calls. Content so far is kept.
    IterationLimitReached(AgentResult),
    /// Cooperative cancellation observed. Content so far is kept.
    Cancelled(AgentResult),
    /// The provider failed. Content so far is kept alongside the error.
    ProviderError {
        error: ProviderError,
        partial: AgentResult,
    },
}

impl AgentOutcome {
    /// The accumulated result, whatever the outcome.
    pub fn result(&self) -> &AgentResult {
        match self {
            AgentOutcome::Completed(result)
            | AgentOutcome::IterationLimitReached(result)
            | AgentOutcome::Cancelled(result) => result,
            AgentOutcome::ProviderError { partial, .. } => partial,
        }
    }
}

/// What one [`Agent::compact`] did, for honest user-facing reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactReport {
    pub messages_before: usize,
    pub messages_after: usize,
    pub summary_chars: usize,
}

/// A bounded run driver over any [`ModelProvider`].
pub struct Agent<P> {
    provider: P,
    config: AgentConfig,
}

impl<P: ModelProvider> Agent<P> {
    pub fn new(provider: P, config: AgentConfig) -> Result<Self, ProviderError> {
        if config.max_iterations == 0 || config.max_iterations > MAX_ITERATIONS_LIMIT {
            return Err(ProviderError::InvalidConfig(format!(
                "max_iterations must be between 1 and {MAX_ITERATIONS_LIMIT}"
            )));
        }
        Ok(Agent { provider, config })
    }

    pub fn config(&self) -> AgentConfig {
        self.config
    }

    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Run one bounded turn sequence: push `user_input`, then request,
    /// append, and continue-on-truncation until done, limited, cancelled,
    /// or failed. Appends every exchanged message to `session`.
    ///
    /// `cancel` is checked explicitly before every provider call and
    /// threaded into each request (Phase 2 mechanism); `on_event` receives
    /// streamed deltas in order and may cancel the turn.
    ///
    /// Partial output is preserved: streamed deltas are intercepted as they
    /// pass through, so when a turn delivers content and then fails
    /// (cancellation or provider error), the outcome still carries that
    /// content, the call counts as an iteration, and the partial assistant
    /// message joins the session. Failures before any delivery leave the
    /// accumulated result untouched.
    pub fn run(
        &self,
        session: &mut AgentSession,
        user_input: &str,
        cancel: Option<&CancelFlag>,
        on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    ) -> AgentOutcome {
        let mut result = AgentResult {
            mode: session.mode(),
            ..AgentResult::default()
        };
        if user_input.trim().is_empty() {
            return AgentOutcome::ProviderError {
                error: ProviderError::InvalidConfig(
                    "agent run requires a non-empty message".into(),
                ),
                partial: result,
            };
        }
        session.push(Message::user(user_input));

        loop {
            if result.iterations >= self.config.max_iterations {
                return AgentOutcome::IterationLimitReached(result);
            }
            // Explicit agent-side cancellation, independent of whether the
            // provider itself honors the flag.
            if cancel.as_ref().is_some_and(|flag| flag.is_cancelled()) {
                return AgentOutcome::Cancelled(result);
            }
            // Project instructions ride along as a leading `system` message
            // on every request, including continuations — without ever
            // entering `session.history`.
            let mut messages = Vec::with_capacity(session.history.len() + 1);
            if let Some(instructions) = session.instructions() {
                messages.push(Message::system(instructions));
            }
            messages.extend(session.history.iter().cloned());
            let request = ChatRequest {
                messages,
                stream: self.config.streaming,
                cancel: cancel.cloned(),
            };
            // Capture what the user already saw: on a failed turn the
            // buffer below is what gets preserved.
            let mut turn_text = String::new();
            let response = if self.config.streaming {
                let mut capture = |event: StreamEvent| {
                    turn_text.push_str(&event.delta);
                    on_event(event)
                };
                self.provider.chat_stream(&request, &mut capture)
            } else {
                self.provider.chat(&request)
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if !turn_text.is_empty() {
                        result.iterations += 1;
                        result.content.push_str(&turn_text);
                        session.push(Message::assistant(turn_text));
                    }
                    return match error {
                        ProviderError::Cancelled => AgentOutcome::Cancelled(result),
                        error => AgentOutcome::ProviderError {
                            error,
                            partial: result,
                        },
                    };
                }
            };
            result.iterations += 1;
            result.content.push_str(&response.content);
            add_usage(&mut result.usage, response.usage);
            result.finish_reason = response.finish_reason.clone();
            session.push(Message::assistant(response.content));
            match result.finish_reason {
                Some(FinishReason::Length) => {
                    session.push(Message::system(CONTINUE_CUE));
                }
                _ => return AgentOutcome::Completed(result),
            }
        }
    }

    /// Answer a side question without touching `session`: the run executes
    /// against a private clone (current history as read-only context) which
    /// is discarded afterwards. Mode, history, and totals of the main
    /// session are unchanged; the caller prints the returned outcome.
    pub fn run_btw(
        &self,
        session: &AgentSession,
        question: &str,
        cancel: Option<&CancelFlag>,
        on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    ) -> AgentOutcome {
        let mut scratch = session.clone();
        self.run(&mut scratch, question, cancel, on_event)
    }

    /// Answer with cross-session context without storing that context: the
    /// run executes against a private clone whose front carries
    /// `context_block` (other sessions' summaries, project metadata) as a
    /// leading `system` message, then the exchanged messages (user turn
    /// plus replies, never the context block itself) are appended to the
    /// real `session`. The caller persists afterwards exactly like a normal
    /// prompt. The context block rides this request only.
    pub fn run_explore(
        &self,
        session: &mut AgentSession,
        context_block: &str,
        user_input: &str,
        cancel: Option<&CancelFlag>,
        on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    ) -> AgentOutcome {
        let before = session.history.len() + 1;
        let mut scratch = session.clone();
        scratch
            .history
            .insert(0, Message::system(context_block.to_string()));
        let outcome = self.run(&mut scratch, user_input, cancel, on_event);
        // Copy back exactly what the turn exchanged: everything past the
        // context block. The block itself never enters stored history.
        session
            .history
            .extend(scratch.history[before..].iter().cloned());
        debug_assert!(
            !session
                .history
                .iter()
                .any(|message| message.content == context_block),
            "explore context must not persist into history"
        );
        outcome
    }

    /// Whether `session` has grown past the auto-compaction threshold.
    pub fn needs_compaction(session: &AgentSession) -> bool {
        session.history.len() > COMPACT_AT_MESSAGES
    }

    /// Compact `session` when `auto` is enabled and the threshold is passed;
    /// otherwise do nothing. Returns the report when compaction ran.
    pub fn maybe_auto_compact(
        &self,
        session: &mut AgentSession,
        auto: bool,
        cancel: Option<&CancelFlag>,
    ) -> Result<Option<CompactReport>, ProviderError> {
        if auto && Self::needs_compaction(session) {
            self.compact(session, cancel).map(Some)
        } else {
            Ok(None)
        }
    }

    /// Replace all but the newest [`COMPACT_KEEP_MESSAGES`] messages with a
    /// single `system` summary produced through the model layer (one
    /// non-streaming call — no summarization framework, just a provider
    /// request). Histories at or below the keep window are left alone and
    /// reported unchanged, without spending a request.
    pub fn compact(
        &self,
        session: &mut AgentSession,
        cancel: Option<&CancelFlag>,
    ) -> Result<CompactReport, ProviderError> {
        let before = session.history.len();
        if before <= COMPACT_KEEP_MESSAGES {
            return Ok(CompactReport {
                messages_before: before,
                messages_after: before,
                summary_chars: 0,
            });
        }
        let split = before - COMPACT_KEEP_MESSAGES;
        // Newest-first window: the summary covers what the request
        // carries; anything older is named by the marker, never silently
        // absorbed. Numbering keeps original 1-based positions.
        let omitted = split.saturating_sub(MAX_COMPACT_TRANSCRIPT_MESSAGES);
        let window_start = split - split.min(MAX_COMPACT_TRANSCRIPT_MESSAGES);
        let mut transcript = String::new();
        if omitted > 0 {
            transcript.push_str(&format!(
                "…[{omitted} earlier message(s) omitted from the summary request]\n"
            ));
        }
        // Role labels use Debug introspection, not wire spellings (those
        // belong to providers): the transcript is prompt text, not protocol.
        for (i, message) in session.history[window_start..split].iter().enumerate() {
            transcript.push_str(&format!(
                "{}. [{:?}] {}\n",
                window_start + i + 1,
                message.role,
                message.content
            ));
        }
        // Bound the summary request: transcript already shrinks with every
        // compaction, and each message is capped for the request.
        let transcript: String = transcript
            .lines()
            .map(|line| {
                if line.chars().count() > COMPACT_MESSAGE_CHARS {
                    let clipped: String = line.chars().take(COMPACT_MESSAGE_CHARS).collect();
                    format!("{clipped}…[truncated]")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        let request = ChatRequest {
            messages: vec![Message::system(SUMMARIZE_SYSTEM), Message::user(transcript)],
            stream: false,
            cancel: cancel.cloned(),
        };
        let response = self.provider.chat(&request)?;
        let summary = format!("Session summary: {}", response.content.trim());
        let summary_chars = summary.chars().count();
        let mut kept = session.history[split..].to_vec();
        let mut history = vec![Message::system(summary)];
        history.append(&mut kept);
        session.history = history;
        Ok(CompactReport {
            messages_before: before,
            messages_after: session.history.len(),
            summary_chars,
        })
    }
}

/// Fold one turn's usage into the running total (missing stays missing only
/// when no turn ever reported usage).
fn add_usage(total: &mut Option<Usage>, turn: Option<Usage>) {
    if let Some(turn) = turn {
        let entry = total.get_or_insert(Usage::default());
        entry.prompt_tokens += turn.prompt_tokens;
        entry.completion_tokens += turn.completion_tokens;
        entry.total_tokens += turn.total_tokens;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatResponse;
    use std::cell::Cell;
    use std::collections::VecDeque;

    /// Scripted provider double: each call consumes the next scripted
    /// outcome; running dry is a test bug (panics). Records every received
    /// message list so tests can assert on request context.
    struct ScriptedProvider {
        script: std::cell::RefCell<VecDeque<Result<ChatResponse, ProviderError>>>,
        calls: Cell<usize>,
        events: Cell<usize>,
        seen: std::cell::RefCell<Vec<Vec<Message>>>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Result<ChatResponse, ProviderError>>) -> Self {
            ScriptedProvider {
                script: std::cell::RefCell::new(script.into()),
                calls: Cell::new(0),
                events: Cell::new(0),
                seen: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn reply(
            content: &str,
            finish: Option<FinishReason>,
        ) -> Result<ChatResponse, ProviderError> {
            Ok(ChatResponse {
                content: content.to_string(),
                role: crate::Role::Assistant,
                model: "scripted".to_string(),
                finish_reason: finish,
                usage: Some(Usage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                    total_tokens: 3,
                }),
            })
        }
    }

    impl ModelProvider for ScriptedProvider {
        fn name(&self) -> &'static str {
            "scripted"
        }

        fn capabilities(&self) -> crate::Capabilities {
            crate::Capabilities {
                streaming: true,
                tool_calling: false,
                structured_output: false,
                context_window: None,
            }
        }

        fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.calls.set(self.calls.get() + 1);
            if request
                .cancel
                .as_ref()
                .is_some_and(CancelFlag::is_cancelled)
            {
                return Err(ProviderError::Cancelled);
            }
            self.seen.borrow_mut().push(request.messages.clone());
            self.script
                .borrow_mut()
                .pop_front()
                .expect("script exhausted")
        }

        fn chat_stream(
            &self,
            request: &ChatRequest,
            on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
        ) -> Result<ChatResponse, ProviderError> {
            self.calls.set(self.calls.get() + 1);
            if request
                .cancel
                .as_ref()
                .is_some_and(CancelFlag::is_cancelled)
            {
                return Err(ProviderError::Cancelled);
            }
            self.seen.borrow_mut().push(request.messages.clone());
            let response = self
                .script
                .borrow_mut()
                .pop_front()
                .expect("script exhausted")?;
            if !response.content.is_empty() {
                self.events.set(self.events.get() + 1);
                match on_event(StreamEvent {
                    delta: response.content.clone(),
                }) {
                    StreamControl::Continue => {}
                    StreamControl::Cancel => return Err(ProviderError::Cancelled),
                }
            }
            Ok(response)
        }
    }

    fn agent(
        script: Vec<Result<ChatResponse, ProviderError>>,
    ) -> (Agent<ScriptedProvider>, AgentSession) {
        let provider = ScriptedProvider::new(script);
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid config");
        (agent, AgentSession::new())
    }

    fn sink() -> impl FnMut(StreamEvent) -> StreamControl {
        |_| StreamControl::Continue
    }

    #[test]
    fn invalid_config_rejected() {
        let provider = ScriptedProvider::new(vec![]);
        assert!(Agent::new(
            provider,
            AgentConfig {
                max_iterations: 0,
                streaming: true
            }
        )
        .is_err());
        let provider = ScriptedProvider::new(vec![]);
        assert!(Agent::new(
            provider,
            AgentConfig {
                max_iterations: MAX_ITERATIONS_LIMIT + 1,
                streaming: true
            }
        )
        .is_err());
        let provider = ScriptedProvider::new(vec![]);
        assert!(Agent::new(
            provider,
            AgentConfig {
                max_iterations: MAX_ITERATIONS_LIMIT,
                streaming: true
            }
        )
        .is_ok());
    }

    #[test]
    fn empty_input_is_usage_error_without_calls() {
        let (agent, mut session) = agent(vec![]);
        let outcome = agent.run(&mut session, "   ", None, &mut sink());
        assert!(matches!(
            outcome,
            AgentOutcome::ProviderError {
                error: ProviderError::InvalidConfig(_),
                ..
            }
        ));
        assert!(session.history().is_empty());
    }

    #[test]
    fn completion_first_try() {
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "done",
            Some(FinishReason::Stop),
        )]);
        let outcome = agent.run(&mut session, "hi", None, &mut sink());
        let AgentOutcome::Completed(result) = outcome else {
            panic!("expected completion, got {outcome:?}");
        };
        assert_eq!(result.content, "done");
        assert_eq!(result.iterations, 1);
        assert_eq!(result.finish_reason, Some(FinishReason::Stop));
        assert_eq!(result.usage.map(|u| u.total_tokens), Some(3));
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[0].content, "hi");
        assert_eq!(session.history()[1].content, "done");
    }

    #[test]
    fn truncation_continues_then_completes() {
        let (agent, mut session) = agent(vec![
            ScriptedProvider::reply("part-", Some(FinishReason::Length)),
            ScriptedProvider::reply("one", Some(FinishReason::Stop)),
        ]);
        let outcome = agent.run(&mut session, "go", None, &mut sink());
        let AgentOutcome::Completed(result) = outcome else {
            panic!("expected completion, got {outcome:?}");
        };
        assert_eq!(result.content, "part-one");
        assert_eq!(result.iterations, 2);
        assert_eq!(result.usage.map(|u| u.total_tokens), Some(6));
        // user, partial assistant, continue cue, final assistant.
        assert_eq!(session.history().len(), 4);
        assert_eq!(session.history()[1].content, "part-");
        assert_eq!(session.history()[2].role, crate::Role::System);
        assert_eq!(session.history()[3].content, "one");
    }

    #[test]
    fn iteration_limit_keeps_partial_work() {
        let agent = Agent::new(
            ScriptedProvider::new(vec![
                ScriptedProvider::reply("a", Some(FinishReason::Length)),
                ScriptedProvider::reply("b", Some(FinishReason::Length)),
            ]),
            AgentConfig {
                max_iterations: 2,
                streaming: false,
            },
        )
        .expect("valid config");
        let mut session = AgentSession::new();
        let outcome = agent.run(&mut session, "go", None, &mut sink());
        let AgentOutcome::IterationLimitReached(result) = outcome else {
            panic!("expected limit, got {outcome:?}");
        };
        assert_eq!(result.content, "ab");
        assert_eq!(result.iterations, 2);
        // History holds both exchanges plus the dangling cue.
        assert_eq!(session.history().len(), 5);
    }

    #[test]
    fn provider_error_carries_iterations() {
        let (agent, mut session) = agent(vec![
            ScriptedProvider::reply("part-", Some(FinishReason::Length)),
            Err(ProviderError::Timeout("slow".into())),
        ]);
        let outcome = agent.run(&mut session, "go", None, &mut sink());
        let AgentOutcome::ProviderError { error, partial } = outcome else {
            panic!("expected provider error, got {outcome:?}");
        };
        assert!(matches!(error, ProviderError::Timeout(_)));
        assert_eq!(partial.content, "part-");
        assert_eq!(partial.iterations, 1);
    }

    #[test]
    fn pre_cancelled_flag_cancels_without_calls() {
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "done",
            Some(FinishReason::Stop),
        )]);
        let outcome = agent.run(
            &mut session,
            "hi",
            Some(&CancelFlag::cancelled()),
            &mut sink(),
        );
        assert!(matches!(outcome, AgentOutcome::Cancelled(_)));
        assert_eq!(outcome.result().iterations, 0);
        // The user message was recorded, but no turn ran.
        assert_eq!(session.history().len(), 1);
    }

    #[test]
    fn consumer_cancel_verdict_cancels_with_partial_preserved() {
        // The consumer saw "done" before cancelling, so the outcome keeps
        // that content and counts the call that produced it.
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "done",
            Some(FinishReason::Stop),
        )]);
        let outcome = agent.run(&mut session, "hi", None, &mut |_| StreamControl::Cancel);
        let AgentOutcome::Cancelled(result) = outcome else {
            panic!("expected cancellation, got {outcome:?}");
        };
        assert_eq!(result.content, "done");
        assert_eq!(result.iterations, 1);
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[1].content, "done");
    }

    /// Provider double that delivers one delta and then fails, exercising
    /// the partial-preservation path for any error kind.
    struct DeliverThenFail {
        calls: Cell<usize>,
        error: ProviderError,
    }

    impl ModelProvider for DeliverThenFail {
        fn name(&self) -> &'static str {
            "deliver-then-fail"
        }

        fn capabilities(&self) -> crate::Capabilities {
            crate::Capabilities {
                streaming: true,
                tool_calling: false,
                structured_output: false,
                context_window: None,
            }
        }

        fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.calls.set(self.calls.get() + 1);
            Err(self.error.clone())
        }

        fn chat_stream(
            &self,
            _request: &ChatRequest,
            on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
        ) -> Result<ChatResponse, ProviderError> {
            self.calls.set(self.calls.get() + 1);
            on_event(StreamEvent {
                delta: "part-".to_string(),
            });
            Err(self.error.clone())
        }
    }

    #[test]
    fn cancelled_after_delivery_preserves_partial() {
        let provider = DeliverThenFail {
            calls: Cell::new(0),
            error: ProviderError::Cancelled,
        };
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid config");
        let mut session = AgentSession::new();
        let outcome = agent.run(&mut session, "hi", None, &mut sink());
        let AgentOutcome::Cancelled(result) = outcome else {
            panic!("expected cancellation, got {outcome:?}");
        };
        assert_eq!(result.content, "part-");
        assert_eq!(result.iterations, 1);
        assert_eq!(agent.provider().calls.get(), 1);
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[1].content, "part-");
        assert_eq!(session.history()[1].role, crate::Role::Assistant);
    }

    #[test]
    fn provider_error_after_delivery_preserves_partial() {
        let provider = DeliverThenFail {
            calls: Cell::new(0),
            error: ProviderError::Timeout("slow".into()),
        };
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid config");
        let mut session = AgentSession::new();
        let outcome = agent.run(&mut session, "hi", None, &mut sink());
        let AgentOutcome::ProviderError { error, partial } = outcome else {
            panic!("expected provider error, got {outcome:?}");
        };
        assert!(matches!(error, ProviderError::Timeout(_)));
        assert_eq!(partial.content, "part-");
        assert_eq!(partial.iterations, 1);
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[1].content, "part-");
    }

    /// Provider double that ignores cancellation entirely: always succeeds.
    /// Proves the agent-side pre-iteration check works on its own.
    struct DeafProvider {
        calls: Cell<usize>,
    }

    impl ModelProvider for DeafProvider {
        fn name(&self) -> &'static str {
            "deaf"
        }

        fn capabilities(&self) -> crate::Capabilities {
            crate::Capabilities {
                streaming: true,
                tool_calling: false,
                structured_output: false,
                context_window: None,
            }
        }

        fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse, ProviderError> {
            self.calls.set(self.calls.get() + 1);
            Ok(ChatResponse {
                content: "done".to_string(),
                role: crate::Role::Assistant,
                model: "deaf".to_string(),
                finish_reason: Some(FinishReason::Stop),
                usage: None,
            })
        }

        fn chat_stream(
            &self,
            request: &ChatRequest,
            _on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
        ) -> Result<ChatResponse, ProviderError> {
            self.chat(request)
        }
    }

    #[test]
    fn agent_side_check_cancels_despite_deaf_provider() {
        let provider = DeafProvider {
            calls: Cell::new(0),
        };
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid config");
        let mut session = AgentSession::new();
        let outcome = agent.run(
            &mut session,
            "hi",
            Some(&CancelFlag::cancelled()),
            &mut sink(),
        );
        assert!(matches!(outcome, AgentOutcome::Cancelled(_)));
        assert_eq!(outcome.result().iterations, 0);
        // No provider call happened; only the user message was recorded.
        assert_eq!(agent.provider().calls.get(), 0);
        assert_eq!(session.history().len(), 1);
    }

    #[test]
    fn non_streaming_path_delivers_nothing_early() {
        let agent = Agent::new(
            ScriptedProvider::new(vec![ScriptedProvider::reply(
                "done",
                Some(FinishReason::Stop),
            )]),
            AgentConfig {
                max_iterations: 5,
                streaming: false,
            },
        )
        .expect("valid");
        let mut session = AgentSession::new();
        let mut events = 0;
        let outcome = agent.run(&mut session, "hi", None, &mut |_| {
            events += 1;
            StreamControl::Continue
        });
        assert!(matches!(outcome, AgentOutcome::Completed(_)));
        assert_eq!(events, 0);
    }

    #[test]
    fn session_reuse_accumulates_history() {
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "one",
            Some(FinishReason::Stop),
        )]);
        agent.run(&mut session, "first", None, &mut sink());
        assert_eq!(agent.provider().calls.get(), 1);
        assert_eq!(session.history().len(), 2);
        session.clear();
        assert!(session.history().is_empty());
    }

    #[test]
    fn mode_defaults_toggle_and_gates() {
        assert_eq!(Mode::default(), Mode::Build);
        assert_eq!(Mode::Build.toggle(), Mode::Plan);
        assert_eq!(Mode::Plan.toggle(), Mode::Build);
        assert!(Mode::Build.allows_mutation());
        assert!(!Mode::Plan.allows_mutation());
        assert_eq!(Mode::Plan.as_str(), "plan");
        let mut session = AgentSession::new();
        assert_eq!(session.mode(), Mode::Build);
        session.set_mode(Mode::Plan);
        assert_eq!(session.mode(), Mode::Plan);
        session.clear();
        assert_eq!(session.mode(), Mode::Plan, "clear keeps the mode");
    }

    #[test]
    fn result_stamps_session_mode() {
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "done",
            Some(FinishReason::Stop),
        )]);
        session.set_mode(Mode::Plan);
        let outcome = agent.run(&mut session, "hi", None, &mut sink());
        assert_eq!(outcome.result().mode, Mode::Plan);
    }

    #[test]
    fn instructions_ride_alongside_history() {
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "done",
            Some(FinishReason::Stop),
        )]);
        session.set_instructions(Some("Be terse.".to_string()));
        let outcome = agent.run(&mut session, "hi", None, &mut sink());
        assert!(matches!(outcome, AgentOutcome::Completed(_)));
        // The provider saw a leading system message ahead of the history.
        let seen = agent.provider().seen.borrow();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].len(), 2);
        assert_eq!(seen[0][0].role, crate::Role::System);
        assert_eq!(seen[0][0].content, "Be terse.");
        assert_eq!(seen[0][1].content, "hi");
        // History itself holds no trace of the instructions.
        assert_eq!(session.history().len(), 2);
        assert!(session
            .history()
            .iter()
            .all(|message| message.content != "Be terse."));
    }

    #[test]
    fn btw_carries_instructions_without_touching_history() {
        let (agent, mut session) = agent(vec![
            ScriptedProvider::reply("main", Some(FinishReason::Stop)),
            ScriptedProvider::reply("side", Some(FinishReason::Stop)),
        ]);
        session.set_instructions(Some("Be terse.".to_string()));
        agent.run(&mut session, "main task", None, &mut sink());
        agent
            .run_btw(&session, "side question", None, &mut sink())
            .result();
        let seen = agent.provider().seen.borrow();
        assert_eq!(seen.len(), 2);
        for request in seen.iter() {
            assert_eq!(request[0].role, crate::Role::System);
            assert_eq!(request[0].content, "Be terse.");
        }
        assert_eq!(session.history().len(), 2);
    }

    fn history_with_pairs(n: usize) -> AgentSession {
        let mut session = AgentSession::new();
        for i in 0..n {
            session.push(Message::user(format!("q{i}")));
            session.push(Message::assistant(format!("a{i}")));
        }
        session
    }

    #[test]
    fn compact_replaces_prefix_with_summary() {
        // 12 messages: 8 summarized, last 4 kept.
        let provider = ScriptedProvider::new(vec![ScriptedProvider::reply(
            "SUMMARY",
            Some(FinishReason::Stop),
        )]);
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid");
        let mut session = history_with_pairs(6);
        let report = agent.compact(&mut session, None).expect("compact");
        assert_eq!(report.messages_before, 12);
        assert_eq!(report.messages_after, 5);
        assert!(report.summary_chars > 0);
        assert_eq!(session.history().len(), 5);
        assert_eq!(session.history()[0].role, crate::Role::System);
        assert!(session.history()[0]
            .content
            .starts_with("Session summary: SUMMARY"));
        // Last 4 verbatim (q4/a4, q5/a5).
        assert_eq!(session.history()[1].content, "q4");
        assert_eq!(session.history()[4].content, "a5");
        assert_eq!(agent.provider().calls.get(), 1);
    }

    #[test]
    fn compact_small_history_is_noop_without_provider_call() {
        // Empty script: any provider call would panic the test.
        let provider = ScriptedProvider::new(vec![]);
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid");
        let mut session = history_with_pairs(2);
        let report = agent.compact(&mut session, None).expect("compact");
        assert_eq!(report.messages_before, 4);
        assert_eq!(report.messages_after, 4);
        assert_eq!(report.summary_chars, 0);
        assert_eq!(agent.provider().calls.get(), 0);
    }

    #[test]
    fn compact_caps_giant_transcripts_with_a_marker() {
        // 600 pairs (1200 messages): the summary request must stay bounded
        // instead of folding the whole history into one prompt.
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "SUMMARY",
            Some(FinishReason::Stop),
        )]);
        for i in 0..600 {
            session.push(Message::user(format!("question {i}")));
            session.push(Message::assistant(format!("answer {i}")));
        }
        let report = agent.compact(&mut session, None).expect("compact");
        assert_eq!(report.messages_before, 1200);
        assert_eq!(report.messages_after, COMPACT_KEEP_MESSAGES + 1);
        // The provider saw the marker plus at most the capped window.
        let seen = agent.provider().seen.borrow();
        assert_eq!(seen.len(), 1);
        let transcript = &seen[0][1].content;
        assert!(
            transcript.contains("omitted from the summary request"),
            "giant histories must be marked: {transcript:?}"
        );
        assert!(
            transcript.lines().count() <= MAX_COMPACT_TRANSCRIPT_MESSAGES + 2,
            "transcript unbounded: {} lines",
            transcript.lines().count()
        );
        assert!(
            transcript.contains("question 597"),
            "the window keeps the newest context"
        );
        // History itself compacted normally around the summary.
        assert!(session.history()[0].content.starts_with("Session summary:"));
    }

    #[test]
    fn auto_compaction_threshold_and_bypass() {
        assert!(!Agent::<ScriptedProvider>::needs_compaction(
            &history_with_pairs(10)
        ));
        assert!(Agent::<ScriptedProvider>::needs_compaction(
            &history_with_pairs(11)
        ));

        // Disabled: no call even above threshold (empty script proves it).
        let provider = ScriptedProvider::new(vec![]);
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid");
        let mut session = history_with_pairs(11);
        assert_eq!(
            agent
                .maybe_auto_compact(&mut session, false, None)
                .expect("skip"),
            None
        );
        assert_eq!(agent.provider().calls.get(), 0);

        // Enabled: compacts (22 messages -> 1 summary + 4 kept).
        let provider = ScriptedProvider::new(vec![ScriptedProvider::reply(
            "AUTO",
            Some(FinishReason::Stop),
        )]);
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid");
        let report = agent
            .maybe_auto_compact(&mut session, true, None)
            .expect("compact")
            .expect("report");
        assert_eq!(report.messages_before, 22);
        assert_eq!(report.messages_after, 5);
        assert_eq!(session.history().len(), 5);
    }

    #[test]
    fn compact_cancel_leaves_history_untouched() {
        let provider = ScriptedProvider::new(vec![ScriptedProvider::reply(
            "SUMMARY",
            Some(FinishReason::Stop),
        )]);
        let agent = Agent::new(provider, AgentConfig::default()).expect("valid");
        let mut session = history_with_pairs(11);
        let err = agent
            .compact(&mut session, Some(&CancelFlag::cancelled()))
            .expect_err("cancelled compaction must fail");
        assert!(matches!(err, ProviderError::Cancelled));
        assert_eq!(session.history().len(), 22, "history untouched");
    }

    #[test]
    fn btw_leaves_main_session_untouched() {
        let (agent, mut session) = agent(vec![
            ScriptedProvider::reply("main-answer", Some(FinishReason::Stop)),
            ScriptedProvider::reply("side-answer", Some(FinishReason::Stop)),
        ]);
        agent.run(&mut session, "main task", None, &mut sink());
        assert_eq!(session.history().len(), 2);
        session.set_mode(Mode::Plan);

        let outcome = agent.run_btw(&session, "side question", None, &mut sink());
        let AgentOutcome::Completed(result) = outcome else {
            panic!("expected side answer, got {outcome:?}");
        };
        assert_eq!(result.content, "side-answer");
        // Main session byte-identical: 2 messages, mode preserved.
        assert_eq!(session.history().len(), 2);
        assert_eq!(session.history()[1].content, "main-answer");
        assert_eq!(session.mode(), Mode::Plan);
        assert_eq!(agent.provider().calls.get(), 2);
    }

    #[test]
    fn run_explore_records_exchange_without_storing_context() {
        let (agent, mut session) = agent(vec![ScriptedProvider::reply(
            "explore-answer",
            Some(FinishReason::Stop),
        )]);
        session.push(Message::user("earlier work"));
        let outcome = agent.run_explore(
            &mut session,
            "[context] other-session summary",
            "deep question",
            None,
            &mut sink(),
        );
        let AgentOutcome::Completed(result) = outcome else {
            panic!("expected explore answer, got {outcome:?}");
        };
        assert_eq!(result.content, "explore-answer");
        // Stored history holds the exchange (earlier + this turn) but never
        // the context block itself.
        let contents: Vec<&str> = session
            .history()
            .iter()
            .map(|message| message.content.as_str())
            .collect();
        assert_eq!(
            contents,
            ["earlier work", "deep question", "explore-answer"]
        );
        assert!(
            !contents.iter().any(|content| content.contains("[context]")),
            "explore context must not persist: {contents:?}"
        );
        // The provider did see the context first: leading system message,
        // then the real history.
        let seen = agent.provider().seen.borrow();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].len() >= 3, "got: {:?}", seen[0]);
        assert_eq!(seen[0][0].role, crate::Role::System);
        assert!(seen[0][0].content.contains("[context]"));
        assert_eq!(seen[0][1].content, "earlier work");
    }
}
