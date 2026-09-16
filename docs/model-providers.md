# AUREL Model Providers — Phase 2

`aurel chat` is a single request/response exchange with a configured model.
It is a connectivity and behavior probe, **not** an agent: it never inspects
repositories, edits files, runs commands, or plans tasks. Autonomy starts in
Phase 3.

## Configuring a model

`[model]` settings (files, `AUREL_*` env, or CLI flags — same precedence as
everything else; see `docs/configuration.md`):

```toml
[model]
name = "my-local-model"          # model id sent to the server
base_url = "http://127.0.0.1:11434/v1"
api_key = "..."                  # optional; local servers often need none
timeout_secs = 60                # total budget per attempt, streams included
max_retries = 1                  # extra attempts for transient failures only
streaming = true                 # progressive display in `aurel chat`
```

| Key | Env | CLI flag |
| --- | --- | -------- |
| `name` | `AUREL_MODEL` | `--model` |
| `base_url` | `AUREL_BASE_URL` | `--base-url` |
| `api_key` | `AUREL_API_KEY` | *(none — deliberate, see below)* |
| `timeout_secs` | `AUREL_TIMEOUT_SECS` | — |
| `max_retries` | `AUREL_MAX_RETRIES` | — |
| `streaming` | `AUREL_STREAMING` (`true`/`false`) | `--streaming true\|false` |

Defaults point at a common local-server root (`http://127.0.0.1:11434/v1`,
model `default`) so a local setup works with minimal configuration; nothing
names or requires a commercial provider.

### Local endpoints

Any server speaking the OpenAI chat protocol works: point `base_url` at its
`/v1`-style root (AUREL appends `/chat/completions`) and set `name` to a
model the server serves. Example for a local server on port 8080:

```toml
[model]
name = "local-model"
base_url = "http://127.0.0.1:8080/v1"
```

No account, key, or payment is involved. `aurel chat` without a running
server fails fast with `error: cannot reach model endpoint: ...` (exit 1).

## `aurel chat`

```text
aurel [OPTIONS] chat [MESSAGE]...

Send one message, print the reply. With no MESSAGE words, the message is
read from piped stdin; a terminal with no message is a usage error (exit 2).
```

With `streaming = true` (the default) reply chunks print as they arrive;
with `false` the full reply prints at once. Exit codes: `0` on success,
`2` on usage errors, `1` on configuration/provider failures.

## Abstraction

Application code depends on the `ModelProvider` trait (`aurel-model`
crate): typed `Message`/`Role`, `ChatRequest` (messages, stream flag,
cooperative `CancelFlag`), `ChatResponse` (content, role, model,
`finish_reason`, `usage`), honest `Capabilities`, and streaming
`StreamEvent`/`StreamControl`. HTTP/JSON wire shapes never leave the
provider module. Everything is blocking — no async runtime (ADR-0006).

## The OpenAI-compatible provider

`OpenAiCompatible` POSTs `{base_url}/chat/completions` with
`{"model", "messages", "stream"}` and parses chat JSON or SSE streams.
The API key travels in exactly one place — the `Authorization: Bearer`
header — and `base_url` must be `http(s)://` with a host and no embedded
credentials. POSTs are never forwarded across redirects, so keys cannot
hop hosts. System proxy environment is honored per the HTTP client's
defaults. A custom `aurel/<version>` User-Agent is sent.

Capabilities are reported conservatively — only what AUREL implements and
verifies today (`streaming: true`; tool-calling, structured output, and
context window are *not* claimed). If streaming is disabled in
configuration, `chat_stream` falls back to one-shot and still returns the
full response.

## Timeouts, retries, cancellation

- **Timeout:** `timeout_secs` (minimum 1s) is the total budget per attempt,
  streams included — the same semantic as common HTTP clients. Slow but
  healthy long generations may need a raise; that is explicit, not a bug.
- **Retries:** bounded (`max_retries`, clamped to 5) with linear backoff,
  and only for transient classes — connection/timeout/rate-limit/5xx (plus
  408/425). Authentication, configuration, malformed, empty, and most 4xx
  failures never retry. A stream that already delivered content never
  retries (partial text must not repeat).
- **Cancellation:** cooperative via `CancelFlag`, checked between attempts
  and before every streamed chunk, plus a per-chunk consumer verdict.
  `aurel chat` itself relies on the timeout and normal process termination;
  the flag exists for the future agent loop.

## Errors

Typed `ProviderError`s with clean messages: invalid config/endpoint,
connection failure, timeout, cancellation, HTTP status (with the server's
`error.message` extracted when present, bounded to 500 chars), auth
rejection (401/403/407 — never the credential), rate limits (with the
`Retry-After` hint when the server sends one), malformed/empty responses,
stream failures, unsupported capabilities. Malformed input never panics and
never loops: one parse attempt, one typed error. Response bodies are
size-bounded before reading (8 MiB success, 64 KiB error diagnostics,
8 MiB streamed accumulation).

## Security and redaction

- `aurel config show` prints `api_key = "<redacted>"` (or `"<unset>"`);
  the real value is never printed, including in `Debug` formatting of
  configuration and provider structs (regression-tested).
- API keys never appear in logs, diagnostics, or error messages. There is
  deliberately **no `--api-key` flag**: argv leaks into shell history and
  process listings. Use a config file (permissions `0600`-style care apply
  as with any secret file) or `AUREL_API_KEY`.
- A malformed config file can echo the offending line in its parse error;
  since a real key is always a TOML string (which parses cleanly), key
  material cannot surface that way — wrong-type errors only echo
  non-string values.
- Do not commit files containing keys. `.gitignore` already excludes
  `.env`; never paste keys into tracked files.

## Known limitations (Phase 2)

- One-shot exchanges only: no conversation history, no system prompt
  setting, no sampling parameters (temperature etc.).
- No capability probing: `context_window` is unknown, tool-calling metadata
  arrives with tools (Phase 4+).
- SSE assumes the OpenAI-compatible convention of one JSON event per
  `data:` line ending in `data: [DONE]`; a stream cut without terminator
  keeps delivered content but claims no finish reason.
- `chat` has no `--api-key`, no inline history, no piping of replies back
  in (each invocation is independent).
