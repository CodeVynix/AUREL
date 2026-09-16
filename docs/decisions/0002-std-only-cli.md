# ADR 0002 — std-only CLI parsing in Phase 0

## Decision

Phase 0 parses arguments with `std::env::args` and a small `match`. No
`clap`, no `lexopt`, no other CLI dependency.

## Alternatives

- `lexopt` now (tiny, grows into Phase 1).
- `clap` now (familiar, heavier).

## Reason

`--version`/`--help` plus one usage-error path need only a few string
comparisons. A framework buys nothing at this scope and costs compile time,
binary size, and API surface that Phase 1 (real subcommands/flags) should
choose deliberately.

## Resource impact

Zero third-party dependencies; smaller debug/release binaries and faster
cold start than a framework-based CLI. Verified via `cargo tree`.

## Long-term implication

Phase 1 re-evaluates CLI parsing when subcommands, config overrides, and
`--dry-run`-style flags land. That decision gets its own ADR.
