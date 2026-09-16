# ADR 0006 — Model provider architecture (sync, ureq + serde_json)

## Decision

Phase 2 introduces an `aurel-model` crate with three layers:

1. **Abstraction** (`types.rs`, `error.rs`, `provider.rs`): `Role`, `Message`,
   `ChatRequest`, `ChatResponse`, `Usage`, `FinishReason`, `Capabilities`,
   `StreamEvent`/`StreamControl`, cooperative `CancelFlag`, typed
   `ProviderError`, and a blocking `ModelProvider` trait (`chat`,
   `chat_stream`). The application depends only on this.
2. **First provider** (`openai.rs`): `OpenAiCompatible` against
   `{base_url}/chat/completions` (chat JSON + SSE streams). All HTTP/JSON
   wire details stay inside this module.
3. **Transport**: `ureq` 3 (blocking) with `rustls`, plus `serde_json` for
   bodies. `default-features = false`, `features = ["rustls", "json"]`
   (no gzip: local endpoints rarely need it).

## Alternatives

- `reqwest` (blocking): pulls Tokio even in blocking mode — rejected, it
  would smuggle an async runtime into Core against the standing policy.
- Hand-rolled HTTP/TLS over `TcpStream`: a correctness and security
  liability (TLS, chunked encoding, proxies) — rejected.
- `native-tls`: smaller on Windows (schannel) but needs OpenSSL on Linux
  and diverges per platform — rejected for consistency; revisit only with
  measurements showing rustls dominating the footprint.
- Async provider trait now: no concurrency requirement exists (one-shot
  `chat`, sequential retries, cooperative cancel flag) — rejected per the
  no-async-without-measurement rule.

## Reason

`ureq` is sync (fits the ADRs 0003/0004 philosophy), MIT/Apache licensed,
MSRV 1.85 (under our 1.98.1 pin), and small relative to alternatives.
`serde_json` reuses the `serde` already in the tree. SSE streaming is plain
line parsing over the response reader — no framework needed.

## Resource impact

Measured on Windows 11 / AMD Athlon 300U / Rust 1.98.1, release profile,
against Phase 1 (492,544 bytes / ≈481 KiB binary, ≈16–76 ms startup):

- release binary size: 2,826,752 bytes (≈2.7 MiB), i.e. +2.3 MiB. The growth
  is the TLS stack, not application code: `rustls` + `ring` (C/assembly
  crypto) + `webpki-roots` dominate; HTTP (`ureq`, `http`, `httparse`) and
  JSON (`serde_json`) are small by comparison. Still far under the < 15 MB
  Core target, and the price of real HTTPS without an async runtime.
- release `--version` startup: warm ≈22–36 ms — unchanged from prior
  baselines; the provider adds no startup work (the agent builds lazily
  per request, and `chat` is the only path that constructs one).
- new third-party crates (`cargo tree -e normal`): `ureq`, `ureq-proto`,
  `http`, `httparse`, `bytes`, `base64`, `percent-encoding`, `utf8-zero`,
  `log`, `once_cell`, `serde_json` (+ `itoa`, `memchr`, `zmij`), `rustls`,
  `rustls-pki-types`, `rustls-webpki`, `webpki-roots`, `ring`, `untrusted`,
  `zeroize`, `subtle`, `getrandom`, `cfg-if`. No Tokio, no async anything.
- build note: `ring` compiles C code via `cc` — standard on the CI images
  (MSVC / gcc), no extra setup needed.

Timeouts bound every request (unary: global timeout; streams: first-byte
timeout plus a cooperative overall deadline and cancel flag checked per
chunk). Retries are bounded and only for transient classes; auth, config,
malformed, and most 4xx errors never retry (policy in
`docs/model-providers.md`).

## Long-term implication

Future providers implement `ModelProvider` in new modules/crates without
touching the application layer. If rustls ever dominates Core's footprint,
revisit transport (e.g. `native-tls`) with a new ADR and measurements —
never silently.
