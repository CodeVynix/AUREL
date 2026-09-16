//! The provider contract. Application and agent code program against
//! [`ModelProvider`]; wire formats never leak past the provider modules.

use crate::{Capabilities, ChatRequest, ChatResponse, ProviderError, StreamControl, StreamEvent};

/// Synchronous model provider. Blocking by design (see ADR-0006): one-shot
/// chat, sequential bounded retries, SSE streaming, and cooperative
/// cancellation all work without an async runtime.
pub trait ModelProvider {
    /// Short human name for diagnostics (e.g. `"openai-compatible"`).
    /// Never includes endpoint URLs or credentials.
    fn name(&self) -> &'static str;

    /// Honestly reported capabilities (see [`Capabilities`]).
    fn capabilities(&self) -> Capabilities;

    /// One full assistant turn. Honors `request.cancel` between attempts.
    fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, ProviderError>;

    /// Streaming turn. Each `delta` is delivered to `on_event` in order;
    /// returning [`StreamControl::Cancel`] (or a set cancel flag) stops the
    /// stream and yields [`ProviderError::Cancelled`]. The returned response
    /// carries the full concatenated content. Providers without streaming
    /// fall back to [`ModelProvider::chat`] and deliver nothing progressively
    /// (still returning the full response).
    fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    ) -> Result<ChatResponse, ProviderError>;
}
