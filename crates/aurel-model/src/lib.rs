//! `aurel-model`: provider-agnostic model layer for AUREL.
//!
//! The application (and later the agent loop) depends only on the
//! abstraction here — [`Role`], [`Message`], [`ChatRequest`],
//! [`ChatResponse`], [`Capabilities`], [`ProviderError`], and the
//! [`ModelProvider`] trait. Wire details for any specific API live in the
//! provider modules (Phase 2: [`openai`]).
//!
//! Everything is synchronous: no async runtime is required for one-shot
//! chat, sequential bounded retries, SSE streaming, or cooperative
//! cancellation ([`CancelFlag`]).

mod error;
mod openai;
mod provider;
mod types;

pub use error::ProviderError;
pub use openai::{OpenAiCompatible, OpenAiConfig};
pub use provider::ModelProvider;
pub use types::{
    CancelFlag, Capabilities, ChatRequest, ChatResponse, FinishReason, Message, Role,
    StreamControl, StreamEvent, Usage,
};
