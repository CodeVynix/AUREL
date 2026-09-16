//! OpenAI-compatible chat provider over blocking HTTPS.
//!
//! Talks to `{base_url}/chat/completions` with any server speaking the
//! OpenAI chat protocol (local llama.cpp/LM Studio-style servers first,
//! hosted compatibles by configuration — never hard-coded). All wire
//! details live here; callers see only [`crate::ModelProvider`].
//!
//! Security posture:
//!
//! - The API key travels in exactly one place: the `Authorization: Bearer`
//!   header. It is never a query parameter, never logged, and never
//!   embedded in any [`crate::ProviderError`].
//! - `base_url` must be `http(s)://` with a host and must not embed
//!   `userinfo` (`@`): credentials in URLs leak into logs and errors.
//! - POST bodies are never forwarded across redirects (ureq refuses); a
//!   redirect answer becomes a deterministic configuration error naming
//!   `base_url`, so keys cannot hop hosts.
//! - Response bodies are size-bounded before reading; server text inside
//!   errors is clipped to [`MAX_DIAG_CHARS`].

use std::fmt;
use std::io::BufRead;
use std::time::Duration;

use serde::Deserialize;

use crate::{
    CancelFlag, Capabilities, ChatRequest, ChatResponse, FinishReason, Message, ModelProvider,
    ProviderError, Role, StreamControl, StreamEvent, Usage,
};

/// Hard cap for any single read response body (success JSON).
const MAX_JSON_BODY: u64 = 8 * 1024 * 1024;
/// Hard cap for error bodies (only a diagnostic prefix is ever needed).
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// Hard cap for total streamed assistant content held in memory.
const MAX_STREAM_CONTENT: usize = 8 * 1024 * 1024;
/// Hard cap for a single SSE line (framing sanity, not content).
const MAX_SSE_LINE: usize = 1024 * 1024;
/// Server text is clipped to this many characters inside diagnostics.
const MAX_DIAG_CHARS: usize = 500;
/// Upper bound on configured retries (transient failures only).
const MAX_RETRIES: u32 = 5;
/// Base backoff between retries; multiplied by the attempt number.
const RETRY_BASE: Duration = Duration::from_millis(250);

/// Configuration for [`OpenAiCompatible`]. Built from `[model]` settings;
/// validated in [`OpenAiCompatible::new`].
///
/// `Debug` is manual: the API key redacts itself so provider configuration
/// can never leak through logs or diagnostics.
#[derive(Clone)]
pub struct OpenAiConfig {
    /// Endpoint root, e.g. `http://127.0.0.1:11434/v1`. A trailing slash is
    /// tolerated. Must be `http(s)://` with a host and no `userinfo`.
    pub base_url: String,
    /// Model id sent as `model` in every request body.
    pub model: String,
    /// Optional bearer credential. Sent only as an `Authorization` header.
    pub api_key: Option<String>,
    /// Total budget per attempt, streams included (matches common HTTP
    /// client semantics). Must be >= 1s so requests are always bounded.
    pub timeout: Duration,
    /// Extra attempts for transient failures only (see
    /// [`ProviderError::retryable`]). Clamped to [`MAX_RETRIES`].
    pub max_retries: u32,
}

impl fmt::Debug for OpenAiConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .finish()
    }
}

/// OpenAI-compatible chat provider (`{base_url}/chat/completions`).
pub struct OpenAiCompatible {
    agent: ureq::Agent,
    config: OpenAiConfig,
    endpoint: String,
}

impl OpenAiCompatible {
    /// Validate configuration and build the transport. Fails deterministically
    /// on bad URLs, empty model names, or unbounded timeouts — never panics.
    pub fn new(config: OpenAiConfig) -> Result<Self, ProviderError> {
        if config.model.trim().is_empty() {
            return Err(ProviderError::InvalidConfig(
                "model must not be empty; set [model] name, AUREL_MODEL, or --model".into(),
            ));
        }
        validate_base_url(&config.base_url)?;
        if config.timeout < Duration::from_secs(1) {
            return Err(ProviderError::InvalidConfig(
                "timeout must be at least 1s so every request stays bounded".into(),
            ));
        }
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(config.timeout))
                .http_status_as_error(false)
                .user_agent(format!("aurel/{}", env!("CARGO_PKG_VERSION")))
                .build(),
        );
        let endpoint = format!("{}/chat/completions", config.base_url.trim_end_matches('/'));
        Ok(OpenAiCompatible {
            agent,
            config,
            endpoint,
        })
    }

    /// Effective retry budget after clamping.
    fn retries(&self) -> u32 {
        self.config.max_retries.min(MAX_RETRIES)
    }

    /// The request URL (no credentials — the key travels by header only).
    #[cfg(test)]
    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn request_body(&self, request: &ChatRequest, stream: bool) -> serde_json::Value {
        serde_json::json!({
            "model": self.config.model,
            "messages": request.messages.iter().map(wire_message).collect::<Vec<_>>(),
            "stream": stream,
        })
    }

    fn send(
        &self,
        request: &ChatRequest,
        stream: bool,
    ) -> Result<ureq::http::Response<ureq::Body>, ProviderError> {
        check_cancelled(request)?;
        let body = self.request_body(request, stream);
        let mut call = self
            .agent
            .post(&self.endpoint)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json");
        if stream {
            call = call.header("Accept", "text/event-stream");
        }
        if let Some(key) = &self.config.api_key {
            // The single place the key is ever used. Pre-validate so a bad
            // key is a clean config error (never a panic, never logged).
            let value = format!("Bearer {key}");
            value.parse::<ureq::http::HeaderValue>().map_err(|_| {
                ProviderError::InvalidConfig(
                    "api_key contains characters invalid in an HTTP header".into(),
                )
            })?;
            call = call.header("Authorization", value);
        }
        call.send_json(body).map_err(map_transport_error)
    }

    /// Run `op` with bounded retries for transient failures only.
    fn with_retries<T>(
        &self,
        request: &ChatRequest,
        mut op: impl FnMut() -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let mut attempt: u32 = 0;
        loop {
            check_cancelled(request)?;
            match op() {
                Ok(value) => return Ok(value),
                Err(e) if e.retryable() && attempt < self.retries() => {
                    attempt += 1;
                    let wait = RETRY_BASE
                        .saturating_mul(attempt)
                        .min(Duration::from_secs(5));
                    std::thread::sleep(wait);
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn chat_once(&self, request: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        let mut response = self.send(request, false)?;
        let status = response.status().as_u16();
        if status == 200 {
            let text = read_limited_body(response.body_mut(), MAX_JSON_BODY)?;
            return parse_chat_body(&text, &self.config.model);
        }
        Err(status_error(&mut response, status))
    }

    fn stream_once(
        &self,
        request: &ChatRequest,
        on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    ) -> Result<ChatResponse, ProviderError> {
        check_cancelled(request)?;
        let response = self.send(request, true)?;
        let status = response.status().as_u16();
        if status != 200 {
            let mut response = response;
            return Err(status_error(&mut response, status));
        }
        let reader = response.into_body().into_reader();
        drive_stream(reader, request, on_event, &self.config.model)
    }
}

impl ModelProvider for OpenAiCompatible {
    fn name(&self) -> &'static str {
        "openai-compatible"
    }

    fn capabilities(&self) -> Capabilities {
        // Conservative: only what AUREL itself implements and reports today.
        // Tool-calling metadata arrives when tools do (Phase 4+); the context
        // window is None because this provider never guesses it.
        Capabilities {
            streaming: true,
            tool_calling: false,
            structured_output: false,
            context_window: None,
        }
    }

    fn chat(&self, request: &ChatRequest) -> Result<ChatResponse, ProviderError> {
        ensure_usable(request)?;
        self.with_retries(request, || self.chat_once(request))
    }

    fn chat_stream(
        &self,
        request: &ChatRequest,
        on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    ) -> Result<ChatResponse, ProviderError> {
        ensure_usable(request)?;
        if !request.stream {
            // Requested fallback: plain one-shot, nothing delivered early.
            return self.chat(request);
        }
        // The headers phase may retry; once bytes flow, a failure is final
        // (partial text already delivered must not repeat).
        let mut started = false;
        self.with_retries(request, || {
            if started {
                return Err(ProviderError::StreamError(
                    "not retrying a stream that already delivered content".into(),
                ));
            }
            let result = self.stream_once(request, on_event);
            if is_partial_stream_failure(&result) {
                started = true;
            }
            result
        })
    }
}

/// Shared request validation: non-empty messages with non-empty content.
fn ensure_usable(request: &ChatRequest) -> Result<(), ProviderError> {
    if request.messages.is_empty() {
        return Err(ProviderError::InvalidConfig(
            "chat requires at least one message".into(),
        ));
    }
    if request.messages.iter().any(|m| m.content.trim().is_empty()) {
        return Err(ProviderError::InvalidConfig(
            "chat messages must not be empty".into(),
        ));
    }
    Ok(())
}

fn check_cancelled(request: &ChatRequest) -> Result<(), ProviderError> {
    if request
        .cancel
        .as_ref()
        .is_some_and(CancelFlag::is_cancelled)
    {
        return Err(ProviderError::Cancelled);
    }
    Ok(())
}

/// A stream failure after delivery started must not be retried.
fn is_partial_stream_failure(result: &Result<ChatResponse, ProviderError>) -> bool {
    matches!(result, Err(ProviderError::StreamError(_)))
}

/// Syntactic base-URL check only (reachability is proven at request time):
/// `http(s)://`, non-empty host, no `userinfo` (credentials must use
/// `api_key`, never the URL).
fn validate_base_url(url: &str) -> Result<(), ProviderError> {
    let invalid = || {
        ProviderError::InvalidConfig(
            "base_url must look like http://host:port[/prefix] with no credentials in it".into(),
        )
    };
    if url.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return Err(invalid());
    }
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(invalid)?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or("");
    if authority.contains('@') || host.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

fn wire_message(message: &Message) -> serde_json::Value {
    serde_json::json!({ "role": message.role.as_str(), "content": message.content })
}

/// Map transport failures. Messages carry status/phase information only —
/// never URLs with userinfo (rejected at construction) and never headers.
fn map_transport_error(e: ureq::Error) -> ProviderError {
    match e {
        ureq::Error::Timeout(reason) => ProviderError::Timeout(format!(
            "no progress within the configured budget ({reason:?})"
        )),
        ureq::Error::HostNotFound => ProviderError::Connection("DNS lookup failed".into()),
        ureq::Error::ConnectionFailed => {
            ProviderError::Connection("connection refused or unreachable".into())
        }
        ureq::Error::Io(e) => ProviderError::Connection(clip(&e.to_string())),
        ureq::Error::Tls(msg) => ProviderError::Connection(clip(msg)),
        ureq::Error::Rustls(e) => ProviderError::Connection(clip(&format!("TLS error: {e:?}"))),
        ureq::Error::Http(_) => ProviderError::InvalidConfig(
            "failed to build the model request; check [model] settings".into(),
        ),
        ureq::Error::BadUri(_) => ProviderError::InvalidConfig(
            "base_url is not a usable HTTP URL; check [model] base_url".into(),
        ),
        ureq::Error::RedirectFailed | ureq::Error::TooManyRedirects => {
            ProviderError::InvalidConfig(
                "endpoint redirected the request; credentials are never forwarded, check base_url"
                    .into(),
            )
        }
        ureq::Error::RequireHttpsOnly(_) => {
            ProviderError::InvalidConfig("endpoint requires https; use an https base_url".into())
        }
        ureq::Error::BodyExceedsLimit(_) => {
            ProviderError::MalformedResponse("response exceeded the size limit".into())
        }
        ureq::Error::Json(_) => {
            ProviderError::InvalidConfig("failed to encode the model request".into())
        }
        // StatusCode cannot occur: http_status_as_error(false). Any future or
        // connector-specific variant degrades to a connection failure.
        _ => ProviderError::Connection(clip(&e.to_string())),
    }
}

/// Non-200 handling: extract the server's `{"error": {"message"}}` when
/// present, map well-known statuses to typed variants, bound everything.
fn status_error(response: &mut ureq::http::Response<ureq::Body>, status: u16) -> ProviderError {
    let server_message = read_error_body(response);
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok());
    match status {
        401 | 403 => ProviderError::Authentication(if server_message.is_empty() {
            "endpoint rejected the credentials; set [model] api_key or AUREL_API_KEY".into()
        } else {
            server_message
        }),
        407 => ProviderError::Authentication("proxy authentication required".into()),
        429 => ProviderError::RateLimited {
            message: if server_message.is_empty() {
                "too many requests".into()
            } else {
                server_message
            },
            retry_after_secs: retry_after,
        },
        _ => ProviderError::Http {
            status,
            message: if server_message.is_empty() {
                "unexpected status with no error detail".into()
            } else {
                server_message
            },
        },
    }
}

/// Read a bounded error body and prefer the OpenAI `error.message` shape.
fn read_error_body(response: &mut ureq::http::Response<ureq::Body>) -> String {
    let text = match read_limited_body(response.body_mut(), MAX_ERROR_BODY) {
        Ok(text) => text,
        Err(_) => return String::new(),
    };
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
        if let Some(message) = value
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
        {
            return clip(message);
        }
    }
    clip(&text)
}

fn read_limited_body(body: &mut ureq::Body, limit: u64) -> Result<String, ProviderError> {
    match body.with_config().limit(limit).read_to_string() {
        Ok(text) => Ok(text),
        Err(ureq::Error::BodyExceedsLimit(_)) => Err(ProviderError::MalformedResponse(
            "response exceeded the size limit".into(),
        )),
        Err(e) => Err(map_transport_error(e)),
    }
}

/// Clip server-controlled text for diagnostics.
fn clip(text: &str) -> String {
    if text.chars().count() > MAX_DIAG_CHARS {
        let clipped: String = text.chars().take(MAX_DIAG_CHARS).collect();
        format!("{clipped}…")
    } else {
        text.to_string()
    }
}

// ---------------------------------------------------------------------------
// Response parsing (pure, unit-tested against fixtures)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ChatBody {
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Option<WireUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    #[serde(default)]
    message: Option<WireMessage>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

/// Parse a 200 response body. Never panics; every shape violation is a
/// typed error. Empty assistant content is [`ProviderError::EmptyResponse`].
fn parse_chat_body(text: &str, configured_model: &str) -> Result<ChatResponse, ProviderError> {
    if text.trim().is_empty() {
        return Err(ProviderError::MalformedResponse(
            "empty response body".into(),
        ));
    }
    let body: ChatBody = serde_json::from_str(text).map_err(|e| {
        ProviderError::MalformedResponse(format!("response is not valid chat JSON: {e}"))
    })?;
    let choice = body.choices.into_iter().next().ok_or_else(|| {
        ProviderError::MalformedResponse("response has no 'choices' entries".into())
    })?;
    let content = choice.message.and_then(|m| m.content).unwrap_or_default();
    if content.is_empty() {
        return Err(ProviderError::EmptyResponse);
    }
    Ok(ChatResponse {
        content,
        role: Role::Assistant,
        model: body.model.unwrap_or_else(|| configured_model.to_string()),
        finish_reason: choice.finish_reason.map(map_finish_reason),
        usage: body.usage.map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }),
    })
}

fn map_finish_reason(reason: String) -> FinishReason {
    match reason.as_str() {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        other => FinishReason::Other(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// SSE streaming (pure line parser + bounded driver)
// ---------------------------------------------------------------------------

/// Outcome of one SSE line.
#[derive(Debug, PartialEq)]
enum SseOutcome {
    /// Progress worth delivering (possibly empty delta carrying metadata).
    Chunk {
        delta: String,
        finish: Option<FinishReason>,
        usage: Option<Usage>,
    },
    /// `data: [DONE]`: the stream is complete.
    Done,
    /// Blank lines, comments, `event:`/`id:` fields: nothing to do.
    Skip,
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    #[serde(default)]
    delta: Option<StreamDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
}

/// Parse one SSE line. Total: blank/comment/non-data lines skip, `[DONE]`
/// terminates, unparseable `data:` payloads are stream errors, parseable
/// payloads without usable delta are skipped.
///
/// Note: multi-line `data:` event accumulation (legal per the SSE spec) is
/// deliberately unsupported — OpenAI-compatible chat servers emit one JSON
/// event per line, and a split payload is treated as the framing violation
/// it would be on those servers.
fn parse_sse_line(line: &str) -> Result<SseOutcome, ProviderError> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return Ok(SseOutcome::Skip);
    }
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(SseOutcome::Skip);
    };
    let data = data.trim();
    if data == "[DONE]" {
        return Ok(SseOutcome::Done);
    }
    let chunk: StreamChunk = serde_json::from_str(data)
        .map_err(|e| ProviderError::StreamError(format!("malformed stream chunk: {e}")))?;
    let mut delta = String::new();
    let mut finish = None;
    for choice in &chunk.choices {
        if let Some(content) = choice.delta.as_ref().and_then(|d| d.content.as_ref()) {
            delta.push_str(content);
        }
        if finish.is_none() {
            finish = choice.finish_reason.clone().map(map_finish_reason);
        }
    }
    Ok(SseOutcome::Chunk {
        delta,
        finish,
        usage: chunk.usage.map(|u| Usage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
        }),
    })
}

/// Drive a stream to completion: deliver deltas in order, honor cancel and
/// the consumer verdict, bound memory. Time is bounded by the transport's
/// overall timeout; cancellation is checked before every chunk.
fn drive_stream(
    reader: impl std::io::Read,
    request: &ChatRequest,
    on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    configured_model: &str,
) -> Result<ChatResponse, ProviderError> {
    let mut lines = std::io::BufReader::new(reader).lines();
    let mut content = String::new();
    let mut finish_reason = None;
    let mut usage = None;
    let mut done = false;

    while let Some(line) = lines.next() {
        check_cancelled(request)?;
        let line = line.map_err(|e| ProviderError::StreamError(clip(&e.to_string())))?;
        if line.len() > MAX_SSE_LINE {
            return Err(ProviderError::StreamError("stream chunk too large".into()));
        }
        match parse_sse_line(&line)? {
            SseOutcome::Skip => {}
            SseOutcome::Done => {
                done = true;
                break;
            }
            SseOutcome::Chunk {
                delta,
                finish,
                usage: chunk_usage,
            } => {
                if !delta.is_empty() {
                    if content.len() + delta.len() > MAX_STREAM_CONTENT {
                        return Err(ProviderError::StreamError(
                            "streamed content exceeded the size limit".into(),
                        ));
                    }
                    content.push_str(&delta);
                    match on_event(StreamEvent { delta }) {
                        StreamControl::Continue => {}
                        StreamControl::Cancel => return Err(ProviderError::Cancelled),
                    }
                }
                if finish.is_some() {
                    finish_reason = finish;
                }
                if chunk_usage.is_some() {
                    usage = chunk_usage;
                }
            }
        }
    }

    if !done && content.is_empty() {
        return Err(ProviderError::StreamError(
            "stream ended without a terminator".into(),
        ));
    }
    if content.is_empty() {
        return Err(ProviderError::EmptyResponse);
    }
    Ok(ChatResponse {
        content,
        role: Role::Assistant,
        model: configured_model.to_string(),
        finish_reason,
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> OpenAiConfig {
        OpenAiConfig {
            base_url: "http://127.0.0.1:9/v1".into(),
            model: "test-model".into(),
            api_key: Some("test-key".into()),
            timeout: Duration::from_secs(5),
            max_retries: 0,
        }
    }

    #[test]
    fn constructor_validates_and_normalizes() {
        let ok = OpenAiCompatible::new(test_config()).expect("valid config");
        assert_eq!(ok.endpoint(), "http://127.0.0.1:9/v1/chat/completions");

        let slash = OpenAiConfig {
            base_url: "http://127.0.0.1:9/v1/".into(),
            ..test_config()
        };
        assert_eq!(
            OpenAiCompatible::new(slash)
                .expect("trailing slash")
                .endpoint(),
            "http://127.0.0.1:9/v1/chat/completions"
        );

        for bad_url in [
            "",
            "notaurl",
            "ftp://host/v1",
            "http://",
            "http:///v1",
            "http://user:pass@host/v1",
            "http://ho st/v1",
        ] {
            let cfg = OpenAiConfig {
                base_url: bad_url.into(),
                ..test_config()
            };
            assert!(
                OpenAiCompatible::new(cfg).is_err(),
                "base_url {bad_url:?} must be rejected"
            );
        }

        let empty_model = OpenAiConfig {
            model: "  ".into(),
            ..test_config()
        };
        assert!(OpenAiCompatible::new(empty_model).is_err());

        let unbounded = OpenAiConfig {
            timeout: Duration::from_secs(0),
            ..test_config()
        };
        assert!(OpenAiCompatible::new(unbounded).is_err());
    }

    #[test]
    fn parses_success_body() {
        let text = r#"{
            "id": "chatcmpl-1", "object": "chat.completion", "model": "m",
            "choices": [{"index": 0,
                "message": {"role": "assistant", "content": "Hello!"},
                "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}
        }"#;
        let res = parse_chat_body(text, "fallback").expect("valid body");
        assert_eq!(res.content, "Hello!");
        assert_eq!(res.model, "m");
        assert_eq!(res.finish_reason, Some(FinishReason::Stop));
        assert_eq!(
            res.usage,
            Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 3,
                total_tokens: 8
            })
        );
    }

    #[test]
    fn rejects_malformed_and_empty_bodies() {
        assert!(matches!(
            parse_chat_body("", "m"),
            Err(ProviderError::MalformedResponse(_))
        ));
        assert!(matches!(
            parse_chat_body("not json", "m"),
            Err(ProviderError::MalformedResponse(_))
        ));
        assert!(matches!(
            parse_chat_body(r#"{"choices": []}"#, "m"),
            Err(ProviderError::MalformedResponse(_))
        ));
        assert!(matches!(
            parse_chat_body(r#"{"nope": true}"#, "m"),
            Err(ProviderError::MalformedResponse(_))
        ));
        // Present-but-empty content is EmptyResponse, not success.
        assert!(matches!(
            parse_chat_body(
                r#"{"choices": [{"message": {"content": ""}, "finish_reason": "stop"}]}"#,
                "m"
            ),
            Err(ProviderError::EmptyResponse)
        ));
        // Null content (e.g. tool-call-only replies, unsupported here) too.
        assert!(matches!(
            parse_chat_body(
                r#"{"choices": [{"message": {}, "finish_reason": "stop"}]}"#,
                "m"
            ),
            Err(ProviderError::EmptyResponse)
        ));
    }

    #[test]
    fn sse_line_parser_handles_protocol_shapes() {
        assert_eq!(parse_sse_line(""), Ok(SseOutcome::Skip));
        assert_eq!(parse_sse_line(": comment"), Ok(SseOutcome::Skip));
        assert_eq!(parse_sse_line("event: message"), Ok(SseOutcome::Skip));
        assert_eq!(parse_sse_line("data: [DONE]"), Ok(SseOutcome::Done));
        assert_eq!(parse_sse_line("data:  [DONE]  "), Ok(SseOutcome::Done));

        let chunk = parse_sse_line(
            r#"data: {"choices": [{"delta": {"content": "Hi"}, "finish_reason": null}]}"#,
        )
        .expect("chunk");
        assert!(matches!(
            chunk,
            SseOutcome::Chunk { ref delta, .. } if delta == "Hi"
        ));

        assert!(matches!(
            parse_sse_line("data: {broken"),
            Err(ProviderError::StreamError(_))
        ));
        // Valid JSON without usable delta: skipped, not fatal.
        assert!(matches!(
            parse_sse_line(r#"data: {"foo": 1}"#),
            Ok(SseOutcome::Chunk { .. })
        ));
    }
}
