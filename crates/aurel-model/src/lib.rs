//! `aurel-model`: provider-agnostic model layer for AUREL.
//!
//! The application (and the basic agent loop) depends only on the
//! abstraction here — [`Role`], [`Message`], [`ChatRequest`],
//! [`ChatResponse`], [`Capabilities`], [`ProviderError`], and the
//! [`ModelProvider`] trait — plus the [`Agent`] run driver with its
//! [`AgentSession`] history and [`AgentOutcome`] taxonomy. Wire details for
//! any specific API live in the provider modules (Phase 2: [`openai`]).
//!
//! Everything is synchronous: no async runtime is required for one-shot
//! chat, bounded agent runs, sequential bounded retries, SSE streaming, or
//! cooperative cancellation ([`CancelFlag`]).

mod agent;
mod error;
mod openai;
mod provider;
mod types;

pub use agent::{
    Agent, AgentConfig, AgentOutcome, AgentResult, AgentSession, MAX_ITERATIONS_LIMIT,
};
pub use error::ProviderError;
pub use openai::{OpenAiCompatible, OpenAiConfig};
pub use provider::ModelProvider;
pub use types::{
    CancelFlag, Capabilities, ChatRequest, ChatResponse, FinishReason, Message, Role,
    StreamControl, StreamEvent, Usage,
};
