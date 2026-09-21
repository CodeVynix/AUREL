//! Provider-agnostic chat types. Plain data only — no HTTP, no JSON wire
//! shapes, no wire spellings. Providers translate between these and their
//! API (role spellings included).

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

/// Conversation role. Only `user` is produced by `aurel chat` today;
/// `system` exists so future phases need no type change.
///
/// Deliberately no string spelling here: each provider maps roles to its
/// own wire format at its boundary.
///
/// The `serde` spellings (`"system"`/`"user"`/`"assistant"`) are the
/// stable session-file format (Phase 9) — never the provider wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// One conversation message. Serializable for session persistence; the
/// stored form is plain data (role + content), never credentials.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: content.into(),
        }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Message {
            role: Role::System,
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: content.into(),
        }
    }
}

/// A chat request. Cooperative cancellation is observed between attempts
/// and, for streams, before each delivered chunk.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Ordered conversation. Must be non-empty with non-empty content
    /// (validated by providers; empty input is a usage error, not a request).
    pub messages: Vec<Message>,
    /// When true and the provider supports it, [`ModelProvider::chat_stream`]
    /// delivers progress; otherwise the provider falls back to one-shot.
    pub stream: bool,
    /// Optional cooperative cancellation shared with the caller.
    pub cancel: Option<CancelFlag>,
}

impl ChatRequest {
    /// Single user turn, the only shape `aurel chat` builds today.
    pub fn one_shot(content: impl Into<String>, stream: bool) -> Self {
        ChatRequest {
            messages: vec![Message::user(content)],
            stream,
            cancel: None,
        }
    }
}

/// Cooperative cancellation flag. Cheap to clone; safe to share across
/// threads. Providers check it between attempts and per stream chunk and
/// return [`crate::ProviderError::Cancelled`] promptly — no threads, no
/// runtime, no signal handling inside the model layer.
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn new() -> Self {
        CancelFlag(Arc::new(AtomicBool::new(false)))
    }

    /// A flag that is already cancelled (useful for tests).
    pub fn cancelled() -> Self {
        let flag = Self::new();
        flag.cancel();
        flag
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Token usage metadata, when the provider reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

/// Why the model stopped generating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinishReason {
    /// Natural end of turn.
    Stop,
    /// Truncated by a length limit.
    Length,
    /// Provider-specific reason, preserved verbatim for diagnostics.
    Other(String),
}

impl FinishReason {
    pub fn as_str(&self) -> &str {
        match self {
            FinishReason::Stop => "stop",
            FinishReason::Length => "length",
            FinishReason::Other(s) => s,
        }
    }
}

/// A completed assistant turn.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: String,
    pub role: Role,
    pub model: String,
    pub finish_reason: Option<FinishReason>,
    pub usage: Option<Usage>,
}

/// Honest capability metadata. A provider reports only what its API actually
/// supports; the application must not assume more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Progressive delivery is implemented (SSE for the OpenAI-compatible
    /// provider). When false, `chat_stream` falls back to one-shot.
    pub streaming: bool,
    /// The API describes tool-calling support. Tool *execution* does not
    /// exist yet (Phase 4+); this is metadata only.
    pub tool_calling: bool,
    /// The API supports constrained/structured output.
    pub structured_output: bool,
    /// Advertised context window in tokens, when known. `None` means the
    /// provider did not report one — never a guess.
    pub context_window: Option<u64>,
}

/// One streamed progress event: a slice of not-yet-delivered content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub delta: String,
}

/// Consumer verdict per streamed chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamControl {
    Continue,
    /// Stop reading; the provider returns `Cancelled`.
    Cancel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_flag_round_trips() {
        let flag = CancelFlag::new();
        assert!(!flag.is_cancelled());
        let shared = flag.clone();
        flag.cancel();
        assert!(shared.is_cancelled());
        assert!(CancelFlag::cancelled().is_cancelled());
    }

    #[test]
    fn one_shot_builds_single_user_turn() {
        let req = ChatRequest::one_shot("hi", true);
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].role, Role::User);
        assert!(req.stream);
        assert!(req.cancel.is_none());
    }
}
