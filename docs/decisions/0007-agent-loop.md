# ADR 0007 — Basic agent loop (Phase 3, no tools)

## Decision

Phase 3 adds a small bounded agent loop as `aurel-model/src/agent.rs`
(no new crate): `AgentSession` (in-memory history), `Agent<P:
ModelProvider>` with `AgentConfig { max_iterations, streaming }`, and an
`AgentOutcome` enum (`Completed` / `IterationLimitReached` / `Cancelled` /
`ProviderError`).

Without tools, the loop's deterministic iteration driver is
length-continuation: a turn ending in `FinishReason::Length` appends the
partial assistant message plus a `system` "Continue." cue and requests
again, up to `max_iterations` (default 5, hard cap 100). Any other finish
becomes `Completed`. History, combined content, summed usage, and the
per-call count travel in the outcome. Cancellation reuses the Phase 2
`CancelFlag`, threaded into every request and checked by the provider
between attempts and per stream chunk.

The CLI gains one-shot `aurel agent [MESSAGE]...` (argv or piped stdin,
same acquisition as `chat`): one bounded run against the configured
provider, streaming progressively. No REPL, no persistence (sessions are
Phase 8), no autonomy beyond requesting continuations.

## Alternatives

- New `aurel-agent` crate now: a cleaner long-term home, but Phase 3 needs
  no tool types and the CLI already depends on `aurel-model` — a new crate
  would add graph edges for zero current benefit. Revisit in Phase 4 when
  tools arrive (moving the loop then is mechanical).
- Single-turn loop (no continuation): would make "iteration limit" dead
  code with no honest multi-iteration behavior to test.
- Async loop: no concurrency requirement; rejected per standing policy.

## Reason

Smallest loop with genuine bounded-iteration semantics: every outcome is
reachable and testable with a scripted `ModelProvider` double, no network.
`max_iterations` default 5 bounds cost (5 requests worst case) while
allowing real multi-part answers; the 100 cap stops absurd configuration.

## Resource impact

No new dependencies; one small module. Release size/startup deltas
measured before commit (expected: negligible — no new crates.io graph).

## Long-term implication

Phase 4 adds tools by extending the loop's per-iteration step (act on
assistant output) and likely promotes the loop to its own crate. The
outcome taxonomy and cancel/config plumbing are designed to survive that
move unchanged.
