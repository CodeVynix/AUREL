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

    /// Scrub the configured credential out of server-controlled text before
    /// it can reach any [`ProviderError`]. See [`scrub_opt`] for the exact
    /// policy (order matters: replace before bounding).
    fn scrub(&self, text: &str) -> String {
        scrub_opt(text, self.config.api_key.as_deref())
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

    /// Run `op` with bounded retries for transient failures only. Backoff
    /// waits are cancellation-aware: the cancel flag is observed throughout
    /// the wait, not just after it.
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
                    cancellable_wait(request, retry_delay(attempt, retry_hint(&e)))?;
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
            return parse_chat_body(&text, &self.config.model, self.config.api_key.as_deref());
        }
        Err(self.status_error(&mut response, status))
    }

    fn stream_once(
        &self,
        request: &ChatRequest,
        on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    ) -> AttemptOutcome {
        if is_cancelled(request) {
            return AttemptOutcome::FailedClean(ProviderError::Cancelled);
        }
        let response = match self.send(request, true) {
            Ok(response) => response,
            Err(error) => return AttemptOutcome::FailedClean(error),
        };
        let status = response.status().as_u16();
        if status != 200 {
            let mut response = response;
            return AttemptOutcome::FailedClean(self.status_error(&mut response, status));
        }
        drive_stream(
            response.into_body().into_reader(),
            request,
            on_event,
            &self.config.model,
            self.config.api_key.as_deref(),
        )
    }

    /// Non-200 handling: extract the server's `{"error": {"message"}}` when
    /// present, map well-known statuses to typed variants, bound everything.
    /// Server-controlled text is scrubbed of the configured credential first.
    fn status_error(
        &self,
        response: &mut ureq::http::Response<ureq::Body>,
        status: u16,
    ) -> ProviderError {
        let server_message = self.scrub(&read_error_body(response));
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
        // Retry boundary is explicit, not inferred: only FailedClean (nothing
        // user-visible delivered) may retry. FailedPartial returns at once —
        // no retry, no extra backoff, no duplicated deltas.
        let mut attempt: u32 = 0;
        loop {
            check_cancelled(request)?;
            match self.stream_once(request, on_event) {
                AttemptOutcome::Done(response) => return Ok(response),
                AttemptOutcome::FailedPartial(error) => return Err(error),
                AttemptOutcome::FailedClean(error) => {
                    if error.retryable() && attempt < self.retries() {
                        attempt += 1;
                        cancellable_wait(request, retry_delay(attempt, retry_hint(&error)))?;
                    } else {
                        return Err(error);
                    }
                }
            }
        }
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
    if is_cancelled(request) {
        return Err(ProviderError::Cancelled);
    }
    Ok(())
}

fn is_cancelled(request: &ChatRequest) -> bool {
    request
        .cancel
        .as_ref()
        .is_some_and(CancelFlag::is_cancelled)
}

/// Upper bound for any single backoff wait.
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Sleep quantum for cancellation-aware waits.
const WAIT_QUANTUM: Duration = Duration::from_millis(10);

/// Pure retry-scheduling decision (unit-tested): linear backoff
/// (`RETRY_BASE` × attempt, capped), raised to the server's numeric
/// `Retry-After` hint when larger, never above the cap.
///
/// Only numeric delta-seconds hints are honored — this provider parses just
/// that form (HTTP-date form falls back to plain backoff; documented in
/// `docs/model-providers.md`).
fn retry_delay(attempt: u32, retry_after_secs: Option<u64>) -> Duration {
    let base = RETRY_BASE.saturating_mul(attempt).min(MAX_BACKOFF);
    match retry_after_secs {
        Some(hint) => base.max(Duration::from_secs(hint).min(MAX_BACKOFF)),
        None => base,
    }
}

/// Extract the server retry hint from a failure, if it carries one.
fn retry_hint(error: &ProviderError) -> Option<u64> {
    match error {
        ProviderError::RateLimited {
            retry_after_secs, ..
        } => *retry_after_secs,
        _ => None,
    }
}

/// Bounded sleep that observes cooperative cancellation throughout the wait
/// instead of only after it. Synchronous: short quanta, no threads, no
/// runtime. Returns [`ProviderError::Cancelled`] as soon as the flag is set.
fn cancellable_wait(request: &ChatRequest, duration: Duration) -> Result<(), ProviderError> {
    let deadline = std::time::Instant::now() + duration;
    loop {
        check_cancelled(request)?;
        let now = std::time::Instant::now();
        if now >= deadline {
            return Ok(());
        }
        std::thread::sleep(WAIT_QUANTUM.min(deadline.saturating_duration_since(now)));
    }
}

/// Outcome of one streaming attempt, tracked as explicit state — never
/// inferred from the error variant afterwards.
#[derive(Debug)]
enum AttemptOutcome {
    /// Full response assembled (terminator seen, content non-empty).
    Done(ChatResponse),
    /// Failed before any user-visible delta was delivered: the caller may
    /// retry per the normal retry policy. Nothing was shown, so nothing
    /// can duplicate.
    FailedClean(ProviderError),
    /// Failed after delivering one or more user-visible deltas: final.
    /// Must never be retried — a retry would print content twice.
    FailedPartial(ProviderError),
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

fn wire_role(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

fn wire_message(message: &Message) -> serde_json::Value {
    serde_json::json!({ "role": wire_role(message.role), "content": message.content })
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

/// Free-function form of [`OpenAiCompatible::scrub`] for pure parsing paths
/// that only hold the key, not the whole provider.
///
/// Policy: replace both the bare key and the `Bearer <key>` form with
/// `<redacted>`. Empty/missing keys are skipped (an empty needle would match
/// everywhere). Replacement happens before bounding ([`clip`]) so a key
/// straddling the clip boundary cannot leak partially.
fn scrub_opt(text: &str, api_key: Option<&str>) -> String {
    let Some(key) = api_key.filter(|key| !key.is_empty()) else {
        return text.to_string();
    };
    let bearer = format!("Bearer {key}");
    let scrubbed = text
        .replace(&bearer, "Bearer <redacted>")
        .replace(key, "<redacted>");
    clip(&scrubbed)
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
///
/// `api_key` scopes [`scrub_opt`]: serde errors echo snippets of the server
/// payload, which must not carry the credential.
fn parse_chat_body(
    text: &str,
    configured_model: &str,
    api_key: Option<&str>,
) -> Result<ChatResponse, ProviderError> {
    if text.trim().is_empty() {
        return Err(ProviderError::MalformedResponse(
            "empty response body".into(),
        ));
    }
    let body: ChatBody = serde_json::from_str(text).map_err(|e| {
        ProviderError::MalformedResponse(scrub_opt(
            &format!("response is not valid chat JSON: {e}"),
            api_key,
        ))
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
///
/// `api_key` scopes [`scrub_opt`]: serde errors echo snippets of the server
/// payload, which must not carry the credential.
fn parse_sse_line(line: &str, api_key: Option<&str>) -> Result<SseOutcome, ProviderError> {
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
    let chunk: StreamChunk = serde_json::from_str(data).map_err(|e| {
        ProviderError::StreamError(scrub_opt(&format!("malformed stream chunk: {e}"), api_key))
    })?;
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

/// Bounded SSE line reader: the [`MAX_SSE_LINE`] limit is enforced *during*
/// reading, so an oversized line errors before it can grow without bound
/// (unlike `BufRead::lines`, which allocates first and asks later).
/// Returns lines without the trailing newline, mirroring `BufRead::lines`
/// (a trailing `\r` goes with it); a final unterminated line is still
/// returned once before `None`. Invalid UTF-8 is a stream error (SSE is UTF-8 by spec).
/// A final unterminated line is still returned once before `None`.
struct SseReader<R> {
    inner: std::io::BufReader<R>,
    buf: Vec<u8>,
}

impl<R: std::io::Read> SseReader<R> {
    fn new(reader: R) -> Self {
        SseReader {
            inner: std::io::BufReader::new(reader),
            buf: Vec::new(),
        }
    }

    fn next_line(&mut self) -> Result<Option<String>, ProviderError> {
        use std::io::BufRead;
        self.buf.clear();
        loop {
            let chunk = self
                .inner
                .fill_buf()
                .map_err(|e| ProviderError::StreamError(clip(&e.to_string())))?;
            if chunk.is_empty() {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                strip_newline(&mut self.buf);
                return decode_line(std::mem::take(&mut self.buf));
            }
            match chunk.iter().position(|&b| b == b'\n') {
                Some(i) => {
                    let take = i + 1;
                    if self.buf.len() + take > MAX_SSE_LINE {
                        return Err(ProviderError::StreamError(
                            "stream line exceeded the size limit".into(),
                        ));
                    }
                    self.buf.extend_from_slice(&chunk[..take]);
                    self.inner.consume(take);
                    strip_newline(&mut self.buf);
                    return decode_line(std::mem::take(&mut self.buf));
                }
                None => {
                    if self.buf.len() + chunk.len() > MAX_SSE_LINE {
                        return Err(ProviderError::StreamError(
                            "stream line exceeded the size limit".into(),
                        ));
                    }
                    let n = chunk.len();
                    self.buf.extend_from_slice(chunk);
                    self.inner.consume(n);
                }
            }
        }
    }
}

/// Drop one trailing `\n` and its optional preceding `\r`, like `lines()`.
fn strip_newline(buf: &mut Vec<u8>) {
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
}

fn decode_line(buf: Vec<u8>) -> Result<Option<String>, ProviderError> {
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| ProviderError::StreamError("stream chunk is not valid UTF-8".into()))
}

/// Drive a stream to completion: deliver deltas in order, honor cancel and
/// the consumer verdict, bound memory. Time is bounded by the transport's
/// overall timeout; cancellation (flag and consumer verdict) is checked
/// before every chunk — a read already blocked in the OS still waits out
/// the transport timeout (documented limitation, see
/// `docs/model-providers.md`).
///
/// Delivery state is explicit: `delivered` flips only when a non-empty
/// delta reaches `on_event`. Every failure maps through the local `fail`
/// closure, so partial output (`FailedPartial`) is never retried while
/// clean failures (`FailedClean`) may be.
fn drive_stream(
    reader: impl std::io::Read,
    request: &ChatRequest,
    on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
    configured_model: &str,
    api_key: Option<&str>,
) -> AttemptOutcome {
    let fail = |delivered: bool, error: ProviderError| {
        if delivered {
            AttemptOutcome::FailedPartial(error)
        } else {
            AttemptOutcome::FailedClean(error)
        }
    };
    let mut reader = SseReader::new(reader);
    let mut content = String::new();
    let mut delivered = false;
    let mut finish_reason = None;
    let mut usage = None;
    let mut done = false;

    loop {
        if is_cancelled(request) {
            return fail(delivered, ProviderError::Cancelled);
        }
        let line = match reader.next_line() {
            Ok(None) => break,
            Ok(Some(line)) => line,
            Err(error) => return fail(delivered, error),
        };
        match parse_sse_line(&line, api_key) {
            Err(error) => return fail(delivered, error),
            Ok(SseOutcome::Skip) => {}
            Ok(SseOutcome::Done) => {
                done = true;
                break;
            }
            Ok(SseOutcome::Chunk {
                delta,
                finish,
                usage: chunk_usage,
            }) => {
                if !delta.is_empty() {
                    if content.len() + delta.len() > MAX_STREAM_CONTENT {
                        return fail(
                            delivered,
                            ProviderError::StreamError(
                                "streamed content exceeded the size limit".into(),
                            ),
                        );
                    }
                    content.push_str(&delta);
                    delivered = true;
                    match on_event(StreamEvent { delta }) {
                        StreamControl::Continue => {}
                        StreamControl::Cancel => {
                            return fail(delivered, ProviderError::Cancelled);
                        }
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

    if !done && !delivered {
        return AttemptOutcome::FailedClean(ProviderError::StreamError(
            "stream ended without a terminator".into(),
        ));
    }
    if content.is_empty() {
        return AttemptOutcome::FailedClean(ProviderError::EmptyResponse);
    }
    AttemptOutcome::Done(ChatResponse {
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
        let res = parse_chat_body(text, "fallback", None).expect("valid body");
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
            parse_chat_body("", "m", None),
            Err(ProviderError::MalformedResponse(_))
        ));
        assert!(matches!(
            parse_chat_body("not json", "m", None),
            Err(ProviderError::MalformedResponse(_))
        ));
        assert!(matches!(
            parse_chat_body(r#"{"choices": []}"#, "m", None),
            Err(ProviderError::MalformedResponse(_))
        ));
        assert!(matches!(
            parse_chat_body(r#"{"nope": true}"#, "m", None),
            Err(ProviderError::MalformedResponse(_))
        ));
        // Present-but-empty content is EmptyResponse, not success.
        assert!(matches!(
            parse_chat_body(
                r#"{"choices": [{"message": {"content": ""}, "finish_reason": "stop"}]}"#,
                "m",
                None,
            ),
            Err(ProviderError::EmptyResponse)
        ));
        // Null content (e.g. tool-call-only replies, unsupported here) too.
        assert!(matches!(
            parse_chat_body(
                r#"{"choices": [{"message": {}, "finish_reason": "stop"}]}"#,
                "m",
                None,
            ),
            Err(ProviderError::EmptyResponse)
        ));
    }

    #[test]
    fn sse_line_parser_handles_protocol_shapes() {
        assert_eq!(parse_sse_line("", None), Ok(SseOutcome::Skip));
        assert_eq!(parse_sse_line(": comment", None), Ok(SseOutcome::Skip));
        assert_eq!(parse_sse_line("event: message", None), Ok(SseOutcome::Skip));
        assert_eq!(parse_sse_line("data: [DONE]", None), Ok(SseOutcome::Done));
        assert_eq!(
            parse_sse_line("data:  [DONE]  ", None),
            Ok(SseOutcome::Done)
        );

        let chunk = parse_sse_line(
            r#"data: {"choices": [{"delta": {"content": "Hi"}, "finish_reason": null}]}"#,
            None,
        )
        .expect("chunk");
        assert!(matches!(
            chunk,
            SseOutcome::Chunk { ref delta, .. } if delta == "Hi"
        ));

        assert!(matches!(
            parse_sse_line("data: {broken", None),
            Err(ProviderError::StreamError(_))
        ));
        // Valid JSON without usable delta: skipped, not fatal.
        assert!(matches!(
            parse_sse_line(r#"data: {"foo": 1}"#, None),
            Ok(SseOutcome::Chunk { .. })
        ));
    }

    #[test]
    fn wire_role_spellings() {
        assert_eq!(wire_role(Role::System), "system");
        assert_eq!(wire_role(Role::User), "user");
        assert_eq!(wire_role(Role::Assistant), "assistant");
    }

    #[test]
    fn retry_delay_uses_hint_within_bounds() {
        use std::time::Duration;
        // Plain linear backoff without a hint.
        assert_eq!(retry_delay(1, None), Duration::from_millis(250));
        assert_eq!(retry_delay(2, None), Duration::from_millis(500));
        assert_eq!(retry_delay(100, None), Duration::from_secs(5));
        // A larger server hint wins, but never above the cap.
        assert_eq!(retry_delay(1, Some(4)), Duration::from_secs(4));
        assert_eq!(retry_delay(1, Some(100)), Duration::from_secs(5));
        // A zero hint falls back to base backoff (never hammer).
        assert_eq!(retry_delay(1, Some(0)), Duration::from_millis(250));
        assert_eq!(retry_delay(3, Some(0)), Duration::from_millis(750));
        // A larger hint wins (bounded above).
        assert_eq!(retry_delay(3, Some(1)), Duration::from_secs(1));
    }

    #[test]
    fn cancellable_wait_observes_the_flag() {
        use std::time::{Duration, Instant};
        let req = ChatRequest::one_shot("hi", false);
        // Uncancelled short wait completes normally.
        cancellable_wait(&req, Duration::from_millis(20)).expect("short wait");
        // A pre-cancelled flag aborts a long wait immediately.
        let cancelled = ChatRequest {
            cancel: Some(CancelFlag::cancelled()),
            ..ChatRequest::one_shot("hi", false)
        };
        let start = Instant::now();
        assert!(matches!(
            cancellable_wait(&cancelled, Duration::from_secs(30)),
            Err(ProviderError::Cancelled)
        ));
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "cancel must shortcut the wait"
        );
    }

    #[test]
    fn scrub_removes_key_and_bearer_everywhere() {
        let key = "sk-test-secret-9";
        // Bare key and Bearer form both go.
        assert_eq!(
            scrub_opt("oops sk-test-secret-9 here", Some(key)),
            "oops <redacted> here"
        );
        assert_eq!(
            scrub_opt("oops Bearer sk-test-secret-9 here", Some(key)),
            "oops Bearer <redacted> here"
        );
        // A key straddling the clip boundary cannot leak partially: it is
        // replaced before bounding, so at worst the marker itself is cut.
        let padded = format!("{}tail {}", "x".repeat(600), key);
        let scrubbed = scrub_opt(&padded, Some(key));
        assert!(!scrubbed.contains(key), "partial leak: {scrubbed:?}");
        assert!(scrubbed.len() <= MAX_DIAG_CHARS + 32, "still bounded");
        // Missing/empty keys pass text through untouched.
        assert_eq!(scrub_opt("plain", None), "plain");
        assert_eq!(scrub_opt("plain", Some("")), "plain");
    }

    #[test]
    fn sse_reader_round_trips_normal_lines() {
        let input = "data: one\r\ndata: two\n\n: comment\npartial";
        let mut reader = SseReader::new(input.as_bytes());
        assert_eq!(
            reader.next_line().expect("l1"),
            Some("data: one".to_string())
        );
        assert_eq!(
            reader.next_line().expect("l2"),
            Some("data: two".to_string())
        );
        assert_eq!(reader.next_line().expect("l3"), Some(String::new()));
        assert_eq!(
            reader.next_line().expect("l4"),
            Some(": comment".to_string())
        );
        assert_eq!(reader.next_line().expect("l5"), Some("partial".to_string()));
        assert_eq!(reader.next_line().expect("eof"), None);
    }

    /// A reader that yields endless 1 KiB chunks with no newline.
    struct EndlessX {
        chunks: std::cell::Cell<usize>,
    }

    impl std::io::Read for EndlessX {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.chunks.set(self.chunks.get() + 1);
            let n = buf.len().min(1024);
            buf[..n].fill(b'x');
            Ok(n)
        }
    }

    #[test]
    fn sse_reader_rejects_oversized_lines_without_huge_allocs() {
        // The old `lines()` approach would grow forever here; the bounded
        // reader must fail after ~1 MiB / 1 KiB per chunk reads.
        let source = EndlessX {
            chunks: std::cell::Cell::new(0),
        };
        let mut reader = SseReader::new(source);
        let err = reader.next_line().expect_err("must reject the line");
        assert!(matches!(err, ProviderError::StreamError(_)), "{err:?}");
        let chunks = reader.inner.get_ref().chunks.get();
        assert!(
            chunks <= 1100,
            "allocation must stay bounded, used {chunks} KiB chunks"
        );
    }

    #[test]
    fn drive_outcome_distinguishes_clean_from_partial() {
        fn run(text: &str) -> AttemptOutcome {
            let req = ChatRequest::one_shot("hi", true);
            drive_stream(
                text.as_bytes(),
                &req,
                &mut |_| StreamControl::Continue,
                "m",
                None,
            )
        }
        // Garbage before any delta: clean failure (retryable by the caller).
        assert!(matches!(
            run("data: {broken\n\n"),
            AttemptOutcome::FailedClean(ProviderError::StreamError(_))
        ));
        // Delta first, then garbage: partial failure (never retry).
        let mut deltas = 0;
        let req = ChatRequest::one_shot("hi", true);
        let outcome = drive_stream(
            "data: {\"choices\": [{\"delta\": {\"content\": \"a\"}}]}\n\ndata: {broken\n\n"
                .as_bytes(),
            &req,
            &mut |_| {
                deltas += 1;
                StreamControl::Continue
            },
            "m",
            None,
        );
        assert!(matches!(outcome, AttemptOutcome::FailedPartial(_)));
        assert_eq!(deltas, 1, "exactly one user-visible delta");
    }
}
