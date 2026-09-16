# ADR 0001 — Two binaries reserved, only `aurel` in Phase 0

## Decision

Phase 0 ships a single binary, `aurel`. `aurel-normal` is reserved for
Phase 11+ as a second binary sharing the same `aurel-core` foundation.

## Alternatives

- Create both binaries now as stubs.
- Single binary forever with a `--mode` flag or feature flags.

## Reason

Stubs would fake Normal scope and widen review/build surface for no runtime
benefit. A future second binary keeps Core's dependency graph and footprint
constrainable, and makes the edition boundary explicit to users.

## Resource impact

Phase 0: no extra binary, no extra link cost. Future Normal crates/bins must
not add dependencies, background work, or init cost to Core.

## Long-term implication

Normal extends the shared foundation instead of forking it. If measurement
ever shows one binary is materially better without growing Core, that change
needs its own ADR.
