//! End-to-end provider tests against a local `std`-only HTTP server.
//!
//! No external network, no API keys, no live services: a tiny single-file
//! HTTP/1.1 stub serves canned responses (and records what it received) on
//! 127.0.0.1 with an ephemeral port. Proxy environment variables are
//! stripped so loopback can never be diverted.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aurel_model::{
    CancelFlag, Capabilities, ChatRequest, Message, ModelProvider, OpenAiCompatible, OpenAiConfig,
    ProviderError, Role,
};

// ---------------------------------------------------------------------------
// Minimal stub server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Canned {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
    extra_headers: Vec<(String, String)>,
    /// Sleep after reading the request, before responding.
    response_delay: Duration,
    /// If set, split the body into chunks of this size with a pause between.
    chunk: Option<(usize, Duration)>,
}

impl Canned {
    fn json(status: u16, body: &str) -> Self {
        Canned {
            status,
            body: body.as_bytes().to_vec(),
            content_type: "application/json",
            extra_headers: Vec::new(),
            response_delay: Duration::ZERO,
            chunk: None,
        }
    }

    fn sse(body: &str) -> Self {
        Canned {
            status: 200,
            body: body.as_bytes().to_vec(),
            content_type: "text/event-stream",
            extra_headers: Vec::new(),
            response_delay: Duration::ZERO,
            chunk: None,
        }
    }
}

#[derive(Debug, Default)]
struct Captured {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

fn read_request(stream: &TcpStream) -> Captured {
    let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
    let mut request_line = String::new();
    reader.read_line(&mut request_line).expect("request line");
    let mut parts = request_line.split_whitespace();
    let mut captured = Captured {
        method: parts.next().unwrap_or("").to_string(),
        path: parts.next().unwrap_or("").to_string(),
        ..Captured::default()
    };
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).expect("header line");
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            captured.headers.insert(name, value);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).expect("request body");
    captured.body = body;
    captured
}

fn serve(mut stream: TcpStream, canned: &Canned, captured: &Arc<Mutex<Vec<Captured>>>) {
    captured.lock().expect("lock").push(read_request(&stream));
    if !canned.response_delay.is_zero() {
        std::thread::sleep(canned.response_delay);
    }
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n{}\r\n",
        canned.status,
        reason(canned.status),
        canned.content_type,
        canned.body.len(),
        canned
            .extra_headers
            .iter()
            .map(|(k, v)| format!("{k}: {v}\r\n"))
            .collect::<String>(),
    );
    // A failed write just means the client went away (timeout tests).
    let _ = stream.write_all(head.as_bytes());
    match canned.chunk {
        None => {
            let _ = stream.write_all(&canned.body);
        }
        Some((size, pause)) => {
            for piece in canned.body.chunks(size) {
                if stream.write_all(piece).is_err() {
                    break;
                }
                let _ = stream.flush();
                std::thread::sleep(pause);
            }
        }
    }
    let _ = stream.flush();
}

/// Serve each canned response to successive connections, then stop.
/// Returns the base URL (without trailing slash) and the server thread.
/// Panics inside the thread if fewer connections arrive within 15s, so a
/// hanging test fails loudly instead of blocking CI forever.
fn serve_all(
    canned: Vec<Canned>,
) -> (
    String,
    Arc<Mutex<Vec<Captured>>>,
    std::thread::JoinHandle<()>,
) {
    strip_proxy_env();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr: SocketAddr = listener.local_addr().expect("local addr");
    let captured: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));
    let thread_captured = Arc::clone(&captured);
    let handle = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let mut served = 0usize;
        while served < canned.len() {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    serve(stream, &canned[served], &thread_captured);
                    served += 1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() > deadline {
                        panic!("stub server timed out waiting for connections");
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => panic!("stub accept failed: {e}"),
            }
        }
    });
    (
        format!("http://127.0.0.1:{}/v1", addr.port()),
        captured,
        handle,
    )
}

fn strip_proxy_env() {
    for var in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        std::env::remove_var(var);
    }
}

fn provider(
    base_url: &str,
    timeout: Duration,
    max_retries: u32,
    api_key: Option<&str>,
) -> OpenAiCompatible {
    OpenAiCompatible::new(OpenAiConfig {
        base_url: base_url.to_string(),
        model: "test-model".to_string(),
        api_key: api_key.map(str::to_string),
        timeout,
        max_retries,
    })
    .expect("test provider config")
}

fn chat_json(content: &str) -> String {
    format!(
        r#"{{"id": "chatcmpl-1", "object": "chat.completion", "model": "test-model",
            "choices": [{{"index": 0, "message": {{"role": "assistant", "content": {content:?}}},
            "finish_reason": "stop"}}],
            "usage": {{"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 8}}}}"#
    )
}

fn request(stream: bool) -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user("hello")],
        stream,
        cancel: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn chat_success_sends_auth_and_parses_response() {
    let (base, captured, server) = serve_all(vec![Canned::json(200, &chat_json("Hi there"))]);
    let provider = provider(&base, Duration::from_secs(5), 0, Some("test-key-abc"));

    let res = provider.chat(&request(false)).expect("chat succeeds");

    assert_eq!(res.content, "Hi there");
    assert_eq!(res.model, "test-model");
    assert_eq!(res.role, Role::Assistant);
    let usage = res.usage.expect("usage reported");
    assert_eq!((usage.prompt_tokens, usage.total_tokens), (5, 8));

    let seen = captured.lock().expect("lock");
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/chat/completions");
    assert_eq!(
        seen[0].headers.get("authorization").map(String::as_str),
        Some("Bearer test-key-abc")
    );
    let body: serde_json::Value = serde_json::from_slice(&seen[0].body).expect("request is JSON");
    assert_eq!(body["model"], "test-model");
    assert_eq!(body["stream"], false);
    assert_eq!(body["messages"][0]["content"], "hello");
    server.join().expect("server finishes");
}

#[test]
fn chat_without_key_sends_no_authorization_header() {
    let (base, captured, server) = serve_all(vec![Canned::json(200, &chat_json("ok"))]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    provider.chat(&request(false)).expect("chat succeeds");

    let seen = captured.lock().expect("lock");
    assert!(!seen[0].headers.contains_key("authorization"));
    server.join().expect("server finishes");
}

#[test]
fn chat_unauthorized_maps_to_authentication() {
    let body =
        r#"{"error": {"message": "Incorrect API key provided", "type": "invalid_request_error"}}"#;
    let (base, _, server) = serve_all(vec![Canned::json(401, body)]);
    let provider = provider(&base, Duration::from_secs(5), 0, Some("bad-key"));

    let err = provider.chat(&request(false)).expect_err("must fail");
    assert!(matches!(err, ProviderError::Authentication(_)), "{err:?}");
    assert!(err.to_string().contains("Incorrect API key"), "{err}");
    assert!(
        !err.to_string().contains("bad-key"),
        "key must not leak: {err}"
    );
    server.join().expect("server finishes");
}

#[test]
fn chat_rate_limited_reports_retry_hint() {
    let mut canned = Canned::json(429, r#"{"error": {"message": "slow down"}}"#);
    canned
        .extra_headers
        .push(("Retry-After".into(), "7".into()));
    let (base, _, server) = serve_all(vec![canned]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let err = provider.chat(&request(false)).expect_err("must fail");
    assert!(
        matches!(
            err,
            ProviderError::RateLimited {
                retry_after_secs: Some(7),
                ..
            }
        ),
        "{err:?}"
    );
    server.join().expect("server finishes");
}

#[test]
fn chat_retries_transient_then_succeeds() {
    let (base, captured, server) = serve_all(vec![
        Canned::json(500, "boom"),
        Canned::json(200, &chat_json("recovered")),
    ]);
    let provider = provider(&base, Duration::from_secs(5), 1, None);

    let res = provider
        .chat(&request(false))
        .expect("recovers after retry");
    assert_eq!(res.content, "recovered");
    assert_eq!(captured.lock().expect("lock").len(), 2);
    server.join().expect("server finishes");
}

#[test]
fn chat_does_not_retry_client_errors() {
    let (base, captured, server) = serve_all(vec![Canned::json(400, "bad request")]);
    let provider = provider(&base, Duration::from_secs(5), 3, None);

    let err = provider.chat(&request(false)).expect_err("must fail");
    assert!(
        matches!(err, ProviderError::Http { status: 400, .. }),
        "{err:?}"
    );
    assert_eq!(captured.lock().expect("lock").len(), 1, "no retry on 400");
    server.join().expect("server finishes");
}

#[test]
fn chat_malformed_json_is_typed() {
    let (base, _, server) = serve_all(vec![Canned::json(200, "this is not json")]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let err = provider.chat(&request(false)).expect_err("must fail");
    assert!(
        matches!(err, ProviderError::MalformedResponse(_)),
        "{err:?}"
    );
    server.join().expect("server finishes");
}

#[test]
fn chat_empty_content_is_empty_response() {
    for body in [
        chat_json(""),
        r#"{"choices": [{"message": {}, "finish_reason": "stop"}]}"#.to_string(),
    ] {
        let (base, _, server) = serve_all(vec![Canned::json(200, &body)]);
        let provider = provider(&base, Duration::from_secs(5), 0, None);
        let err = provider.chat(&request(false)).expect_err("must fail");
        assert!(matches!(err, ProviderError::EmptyResponse), "{err:?}");
        server.join().expect("server finishes");
    }
}

#[test]
fn chat_timeout_is_typed() {
    let mut canned = Canned::json(200, &chat_json("too late"));
    canned.response_delay = Duration::from_secs(10);
    let (base, _, _) = serve_all(vec![canned]);
    let provider = provider(&base, Duration::from_secs(1), 0, None);

    let err = provider.chat(&request(false)).expect_err("must time out");
    assert!(matches!(err, ProviderError::Timeout(_)), "{err:?}");
}

#[test]
fn chat_connection_refused_is_typed() {
    // Bind-then-drop yields a port nothing listens on.
    let port = TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port();
    strip_proxy_env();
    let provider = provider(
        &format!("http://127.0.0.1:{port}/v1"),
        Duration::from_secs(5),
        0,
        None,
    );

    let err = provider.chat(&request(false)).expect_err("must fail");
    assert!(matches!(err, ProviderError::Connection(_)), "{err:?}");
}

#[test]
fn chat_cancelled_before_sending() {
    let provider = provider("http://127.0.0.1:9/v1", Duration::from_secs(5), 0, None);
    let req = ChatRequest {
        messages: vec![Message::user("hello")],
        stream: false,
        cancel: Some(CancelFlag::cancelled()),
    };
    assert!(matches!(provider.chat(&req), Err(ProviderError::Cancelled)));
}

#[test]
fn stream_success_delivers_deltas_in_order() {
    let sse = concat!(
        "data: {\"choices\": [{\"delta\": {\"content\": \"Hel\"}}]}\n\n",
        "data: {\"choices\": [{\"delta\": {\"content\": \"lo\"}}]}\n\n",
        ": heartbeat\n\n",
        "data: {\"choices\": [{\"delta\": {}, \"finish_reason\": \"stop\"}], \"usage\": {\"prompt_tokens\": 1, \"completion_tokens\": 2, \"total_tokens\": 3}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _, server) = serve_all(vec![Canned::sse(sse)]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let mut deltas = Vec::new();
    let res = provider
        .chat_stream(&request(true), &mut |event| {
            deltas.push(event.delta.clone());
            aurel_model::StreamControl::Continue
        })
        .expect("stream succeeds");

    assert_eq!(deltas, vec!["Hel".to_string(), "lo".to_string()]);
    assert_eq!(res.content, "Hello");
    assert_eq!(res.finish_reason, Some(aurel_model::FinishReason::Stop));
    assert_eq!(res.usage.map(|u| u.total_tokens), Some(3));
    server.join().expect("server finishes");
}

#[test]
fn stream_malformed_chunk_is_typed() {
    let sse = "data: {\"choices\": [{\"delta\": {\"content\": \"Hi\"}}]}\n\ndata: {broken\n\n";
    let (base, _, server) = serve_all(vec![Canned::sse(sse)]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let err = provider
        .chat_stream(&request(true), &mut |_| {
            aurel_model::StreamControl::Continue
        })
        .expect_err("must fail");
    assert!(matches!(err, ProviderError::StreamError(_)), "{err:?}");
    server.join().expect("server finishes");
}

#[test]
fn stream_missing_terminator_with_content_is_accepted_without_finish() {
    // Tolerant EOF: delivered content stands, but no finish is claimed.
    let sse = "data: {\"choices\": [{\"delta\": {\"content\": \"partial\"}}]}\n\n";
    let (base, _, server) = serve_all(vec![Canned::sse(sse)]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let res = provider
        .chat_stream(&request(true), &mut |_| {
            aurel_model::StreamControl::Continue
        })
        .expect("tolerant EOF");
    assert_eq!(res.content, "partial");
    assert_eq!(res.finish_reason, None);
    server.join().expect("server finishes");
}

#[test]
fn stream_missing_terminator_without_content_fails() {
    let (base, _, server) = serve_all(vec![Canned::sse("")]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let err = provider
        .chat_stream(&request(true), &mut |_| {
            aurel_model::StreamControl::Continue
        })
        .expect_err("must fail");
    assert!(
        matches!(
            err,
            ProviderError::StreamError(_) | ProviderError::EmptyResponse
        ),
        "{err:?}"
    );
    server.join().expect("server finishes");
}

#[test]
fn stream_consumer_cancel_stops_with_cancelled() {
    let sse = concat!(
        "data: {\"choices\": [{\"delta\": {\"content\": \"one\"}}]}\n\n",
        "data: {\"choices\": [{\"delta\": {\"content\": \"two\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _, server) = serve_all(vec![Canned::sse(sse)]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let mut seen = 0;
    let err = provider
        .chat_stream(&request(true), &mut |_| {
            seen += 1;
            aurel_model::StreamControl::Cancel
        })
        .expect_err("must cancel");
    assert!(matches!(err, ProviderError::Cancelled), "{err:?}");
    assert_eq!(seen, 1);
    server.join().expect("server finishes");
}

#[test]
fn stream_fallback_returns_full_response_without_events() {
    let (base, _, server) = serve_all(vec![Canned::json(200, &chat_json("whole"))]);
    let provider = provider(&base, Duration::from_secs(5), 0, None);

    let mut events = 0;
    let res = provider
        .chat_stream(&request(false), &mut |_| {
            events += 1;
            aurel_model::StreamControl::Continue
        })
        .expect("fallback succeeds");
    assert_eq!(res.content, "whole");
    assert_eq!(events, 0, "fallback delivers nothing progressively");
    server.join().expect("server finishes");
}

#[test]
fn provider_capabilities_are_conservative() {
    let provider = provider("http://127.0.0.1:9/v1", Duration::from_secs(5), 0, None);
    assert_eq!(provider.name(), "openai-compatible");
    let caps: Capabilities = provider.capabilities();
    assert!(caps.streaming);
    // Nothing is claimed that AUREL does not implement or verify.
    assert!(!caps.tool_calling);
    assert!(!caps.structured_output);
    assert_eq!(caps.context_window, None);
}
