# ADR 0008 — Shell execution (direct spawn, approval-gated)

## Decision

Phase 7 executes shell commands by direct process spawn (`std::process`,
no shell, no async runtime) inside `aurel-tools/src/command.rs`, reusing
the Phase 6 proposal/approval/undo machinery with three new proposal ops
(`run_command`, `run_build`, `run_tests`). `!command` parses (whitespace
splitting with double-quote grouping, no shell features) and queues like
any model proposal.

## Alternatives

- Invoke `sh -c` / `cmd /C`: rejected — shell operators, expansions, and
  quoting rules differ per platform and would make review meaningless
  (the user approves text the shell then reinterprets).
- `reqwest`-style heavy process crates / Tokio `Command`: rejected —
  `std::process` plus two scoped reader threads and a 10 ms poll loop
  cover timeout, cancellation, and bounded output with no new
  dependencies.
- Program allowlist: rejected as unmaintainable and hostile to real
  builds; approval visibility plus exact resolved paths is the control.
- Separate approval track for shell: rejected — one state machine
  (queue → review → approve/deny → undo) for files and commands alike
  means one code path to audit.

## Reason

Direct execution makes the approved artifact (`program` + literal `args`
+ `cwd`) identical to what runs — nothing reinterprets it afterwards.
PATH lookup is explicit (never the current directory; `PATHEXT` honored
on Windows; execute-bit checked on Unix). Timeouts kill, cancellation is
observed every 10 ms, output is capped per stream with drain-discard (no
deadlock, bounded memory). The configured API key is scrubbed from
captured output before display or storage.

## Resource impact

Measured deltas recorded before the Phase 7 commit (Windows 11 /
AMD Athlon 300U / Rust 1.98.1, release), vs Phase 6 (3,085,312 bytes /
≈18–26 ms startup):

- release binary size: 3,232,768 bytes (≈3.08 MiB), i.e. +147,456 bytes
  (+4.8%) — new code paths, no new dependency weight.
- release `--version` startup: warm ≈16–26 ms — unchanged.
- new crates.io graph entries: none (`std::process`/`std::thread` only).
  Two short-lived reader threads exist per running command and join
  before it returns — no background workers, no detached threads.

## Long-term implication

`run_build`/`run_tests` resolve markers once (`Cargo.toml`, then
`package.json`, `go.mod`, `Makefile`); richer project intelligence can
extend the detector table without touching execution. A future `quiet`
flag or per-command timeout override would be a config addition, not an
architecture change. Undo of shell effects stays honestly impossible
(`NotUndoable`); file-undo semantics are untouched.
