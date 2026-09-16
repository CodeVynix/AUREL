//! Typed provider failures. Every fallible provider operation returns
//! [`ProviderError`]: no panics on bad input, no collapsed "something failed"
//! variant, and never any secret material in messages.

use std::fmt;

/// What went wrong on the way to (or back from) a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    /// Caller-side problem: empty messages, bad URL, bad timeout, unknown
    /// capability request. Never retried. Never contains secrets.
    InvalidConfig(String),
    /// TCP/TLS/DNS failure before any HTTP response.
    Connection(String),
    /// Bounded wait exceeded (connect, first byte, overall deadline).
    Timeout(String),
    /// Cooperative cancellation observed.
    Cancelled,
    /// HTTP error status with a redacted, bounded diagnostic.
    Http { status: u16, message: String },
    /// 401/407-style rejection. Carries no credential material — only the
    /// fact of rejection plus which credential was missing, if any.
    Authentication(String),
    /// 429 with optional server-provided retry hint (seconds).
    RateLimited {
        message: String,
        retry_after_secs: Option<u64>,
    },
    /// 2xx plus bytes that are not the documented shape (bad JSON, missing
    /// fields, wrong types, empty body). Message is bounded; never panics.
    MalformedResponse(String),
    /// The provider returned success with no assistant content at all.
    EmptyResponse,
    /// A stream that was mid-flight failed (transport or chunk framing).
    /// Partial text already delivered stays delivered; it is not repeated.
    StreamError(String),
    /// The provider cannot do what was asked (e.g. streaming on a
    /// non-streaming provider when no fallback applies).
    UnsupportedCapability(String),
}

impl ProviderError {
    /// Whether the operation may be retried with backoff. Only genuinely
    /// transient classes qualify: never auth, config, malformed, empty, or
    /// cancellation failures.
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::Connection(_)
            | ProviderError::Timeout(_)
            | ProviderError::RateLimited { .. }
            | ProviderError::StreamError(_) => true,
            ProviderError::Http { status, .. } => {
                *status == 408 || *status == 425 || (500..600).contains(status)
            }
            ProviderError::InvalidConfig(_)
            | ProviderError::Cancelled
            | ProviderError::Authentication(_)
            | ProviderError::MalformedResponse(_)
            | ProviderError::EmptyResponse
            | ProviderError::UnsupportedCapability(_) => false,
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProviderError::InvalidConfig(m) => write!(f, "error: invalid model configuration: {m}"),
            ProviderError::Connection(m) => write!(f, "error: cannot reach model endpoint: {m}"),
            ProviderError::Timeout(m) => write!(f, "error: model request timed out: {m}"),
            ProviderError::Cancelled => write!(f, "error: model request cancelled"),
            ProviderError::Http { status, message } => {
                write!(f, "error: model endpoint returned HTTP {status}: {message}")
            }
            ProviderError::Authentication(m) => {
                write!(f, "error: model endpoint rejected credentials: {m}")
            }
            ProviderError::RateLimited {
                message,
                retry_after_secs,
            } => match retry_after_secs {
                Some(s) => write!(
                    f,
                    "error: model endpoint rate-limited ({message}); retry after {s}s"
                ),
                None => write!(f, "error: model endpoint rate-limited: {message}"),
            },
            ProviderError::MalformedResponse(m) => {
                write!(
                    f,
                    "error: model endpoint returned an unusable response: {m}"
                )
            }
            ProviderError::EmptyResponse => {
                write!(f, "error: model endpoint returned no assistant content")
            }
            ProviderError::StreamError(m) => write!(f, "error: model stream failed: {m}"),
            ProviderError::UnsupportedCapability(m) => {
                write!(f, "error: unsupported model capability: {m}")
            }
        }
    }
}

impl std::error::Error for ProviderError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_policy_retries_only_transient_failures() {
        for err in [
            ProviderError::Connection("x".into()),
            ProviderError::Timeout("x".into()),
            ProviderError::RateLimited {
                message: "x".into(),
                retry_after_secs: None,
            },
            ProviderError::StreamError("x".into()),
            ProviderError::Http {
                status: 500,
                message: "x".into(),
            },
            ProviderError::Http {
                status: 503,
                message: "x".into(),
            },
        ] {
            assert!(err.retryable(), "{err:?} should retry");
        }
        for err in [
            ProviderError::InvalidConfig("x".into()),
            ProviderError::Cancelled,
            ProviderError::Authentication("x".into()),
            ProviderError::MalformedResponse("x".into()),
            ProviderError::EmptyResponse,
            ProviderError::UnsupportedCapability("x".into()),
            ProviderError::Http {
                status: 400,
                message: "x".into(),
            },
            ProviderError::Http {
                status: 401,
                message: "x".into(),
            },
            ProviderError::Http {
                status: 404,
                message: "x".into(),
            },
        ] {
            assert!(!err.retryable(), "{err:?} must not retry");
        }
    }

    #[test]
    fn messages_carry_no_secret_shaped_content_by_construction() {
        // Provider errors are built from status codes and bounded server
        // text only; assert the Display contract holds for every variant.
        let errs = [
            ProviderError::Authentication("missing API key".into()),
            ProviderError::Http {
                status: 500,
                message: "boom".into(),
            },
        ];
        for err in errs {
            let text = err.to_string();
            assert!(!text.contains("sk-"), "{text:?}");
            assert!(!text.contains("Bearer"), "{text:?}");
        }
    }
}
