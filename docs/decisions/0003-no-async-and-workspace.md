# ADR 0003 — Synchronous workspace, no async runtime

## Decision

Phase 0 is synchronous standard-library Rust. No Tokio, async-std, or other
async runtime. Workspace uses `resolver = "2"`, `edition = "2021"`,
`rust-version = "1.98.1"`, pinned via `rust-toolchain.toml`
(`channel = "1.98.1"` + `rustfmt`/`clippy`).

## Alternatives

- Add Tokio now "for later" (streaming, background tasks).
- Newer edition (2024) immediately.

## Reason

Phase 0 performs no concurrent I/O: no model streaming, no parallel tool
execution, no watchers. Threads/`std::process` cover everything. An async
runtime would add build time, binary size, and cognitive overhead with no
measured requirement. Edition 2021 is conservative and fully supported by
the pinned toolchain.

## Resource impact

Minimal build graph (two workspace crates, zero external deps), fast
incremental builds, small binaries, no runtime thread-pool/idle cost.

## Long-term implication

Async returns only when a concrete requirement (e.g. concurrent streaming
+ cancellation) proves sync primitives insufficient. That change needs
profiling evidence and its own ADR per the master spec.
