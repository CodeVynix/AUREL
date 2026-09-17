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

/// Continuation cue appended (as `system`) after a truncated turn so the
/// next request deterministically asks for the rest instead of repeating
/// the same truncated prefix.
const CONTINUE_CUE: &str = "Continue.";

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

/// In-memory conversation history for one agent session. No persistence
/// (sessions are Phase 8); dropped with the process.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AgentSession {
    history: Vec<Message>,
}

impl AgentSession {
    pub fn new() -> Self {
        AgentSession {
            history: Vec::new(),
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn push(&mut self, message: Message) {
        self.history.push(message);
    }

    pub fn clear(&mut self) {
        self.history.clear();
    }
}

/// Combined result of one [`Agent::run`]: assistant text accumulated across
/// every iteration, how many provider calls that took, and trailing metadata.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentResult {
    pub content: String,
    pub iterations: u32,
    pub finish_reason: Option<FinishReason>,
    pub usage: Option<Usage>,
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
        let mut result = AgentResult::default();
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
            let request = ChatRequest {
                messages: session.history.clone(),
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
    /// outcome; running dry is a test bug (panics).
    struct ScriptedProvider {
        script: std::cell::RefCell<VecDeque<Result<ChatResponse, ProviderError>>>,
        calls: Cell<usize>,
        events: Cell<usize>,
    }

    impl ScriptedProvider {
        fn new(script: Vec<Result<ChatResponse, ProviderError>>) -> Self {
            ScriptedProvider {
                script: std::cell::RefCell::new(script.into()),
                calls: Cell::new(0),
                events: Cell::new(0),
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
}
