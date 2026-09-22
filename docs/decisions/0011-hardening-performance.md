# ADR 0011 — Hardening + performance without architecture change

## Decision

Phase 10 hardens in place: explicit caps on every open-ended input
(fence payloads, compaction transcripts, session listing, blurbs,
stored history), bounded work for loops that were merely
practically-bounded (session save shedding), crash-litter cleanup and
growth notices instead of silent accumulation, and real measurements
(post-exit peak RSS, warm startup, stub-driven active turns) against
the resource targets. No new crates, no new dependencies, no async,
no interaction changes.

## Alternatives

- Automatic session pruning (LRU/TTL eviction): rejected — deleting
  user conversations on a heuristic risks destroying work the user
  considers archival. Growth gets a notice with a path, never an
  automatic delete.
- Streaming session-file parse for listings: rejected as unnecessary —
  files are already capped at 2 MiB, so a metadata-only struct over
  the bounded buffer gives bounded memory without a streaming parser.
- A benchmark harness crate (criterion): rejected — it would add
  heavyweight dev-dependencies for numbers better taken from the real
  binary (Job-Object/post-exit RSS, wall timing) plus small
  structural regression tests. Criterion-scale microbenchmarks answer
  questions this codebase does not have.
- Reworking `Agent::run` to avoid per-iteration history clones:
  rejected — the clone is semantically required (each iteration
  appends), history is compaction-bounded in practice, and the
  transcript cap now bounds the pathological case instead.
- Silently dropping oversized fences mid-stream: rejected — the fence
  is consumed to its closer (later fences still parse) and named in a
  note, so a compromised endpoint's attempt stays visible.

## Reason

Hardening that changes architecture would invalidate the nine phases
of review beneath it. Every Phase 10 change is therefore either a
bound with a test, a cheaper path to the same result (metadata
parsing), or a measurement. The one piece of new UX — the disk-growth
notice — was chosen over automation precisely because storage policy
is the user's call. Performance work stopped at measurement because
measurement showed headroom (12–20× on every memory target): there
was nothing worth optimizing at the cost of complexity.

## Consequences

- New public constants (`MAX_FENCE_PAYLOAD_BYTES`,
  `MAX_COMPACT_TRANSCRIPT_MESSAGES`, `SESSION_COUNT_WARN_THRESHOLD`)
  join the existing bounds vocabulary; all are covered by tests that
  fail if the bound moves without updating them.
- The measurement harness lives outside the repo (throwaway PowerShell
  using Job Objects and post-exit counters); only its results are
  committed, in README and the architecture delta.
- REPL idle RSS is an analytic bound, not a sampled number — driving
  the interactive loop requires a PTY the test rigs do not provide.
  The bound (minimal path plus maxed session state) sits far enough
  under target that sampling would add confidence but not decisions.
