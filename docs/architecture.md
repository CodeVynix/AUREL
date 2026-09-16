# AUREL Architecture — Phase 0

This document describes what Phase 0 actually contains. Nothing more.

## Workspace

Single Cargo workspace (`Cargo.toml`, `resolver = "2"`) with two members.
Shared package metadata (`version`, `edition`, `rust-version`, `license`)
is defined once in `[workspace.package]` and inherited by both crates, so
there is one version definition feeding package metadata, the runtime
display (`aurel_core::version()`), and the test expectations:

- `crates/aurel-core` — library crate (`aurel_core`). Shared foundation.
  Phase 0 exposes one real API: `aurel_core::version()`, sourced from
  `CARGO_PKG_VERSION`. No agent, tool, model, config, session, storage,
  security, or Git modules exist yet, and no empty placeholder crates were
  created for them.
- `crates/aurel-cli` — binary crate producing the `aurel` binary
  (`[[bin]] name = "aurel"`). Depends on `aurel-core` via a local path
  dependency. Zero third-party dependencies.

The root manifest is workspace-only (no root package). `Cargo.lock` is
committed because the workspace produces a binary.

## CLI

`crates/aurel-cli/src/main.rs` parses `std::env::args_os` with no CLI
framework. Raw OS arguments pass through `normalize_args`, which applies an
explicit lossy Unicode policy (`to_string_lossy`, unrepresentable sequences
become U+FFFD) and continues through the normal parser. All branching lives
in `run(args, out, err) -> i32`, unit-tested in-file; `tests/cli.rs` covers
the compiled binary via `CARGO_BIN_EXE_aurel`:

- empty args / `-h` / `--help` → help to stdout, exit 0
- `-V` / `--version` → `aurel <version>` to stdout, exit 0
  (version comes from `aurel_core::version()`, single source of truth)
- anything else (including lossy-converted non-Unicode input, which matches
  no known flag) → error to stderr, exit 2

No color, no terminal requirements, safe under pipes. IO failures return
exit 1 instead of panicking, and the argument-conversion path itself is
total (`args_os` performs no Unicode validation, `to_string_lossy` cannot
fail), so non-Unicode input is handled deterministically with no panic in
crate-controlled logic. No `unwrap`/`expect` exists on the runtime argument
path (only in tests). No broader claim is made about std/OS internals
outside this crate's control.

## Why Normal does not exist yet

The long-term design is two binaries (`aurel`, `aurel-normal`) sharing this
foundation (see `docs/decisions/0001-two-binaries-reserved.md`). Phase 0
ships only `aurel` so the Core footprint, build graph, and review surface
stay minimal. Normal arrives in Phase 11+ as additive crates/bins.

## Why the CLI is std-only

`--version`/`--help` need a few string matches. A framework would add
compile time, binary size, and startup cost for zero Phase 0 benefit
(see `docs/decisions/0002-std-only-cli.md`). The parser choice is revisited
in Phase 1 when real subcommands/flags appear.

## Why there is no async runtime

Phase 0 does no I/O concurrency: no model streaming, no parallel tools, no
background tasks (see `docs/decisions/0003-no-async-and-workspace.md`).
Synchronous std code is sufficient; an async runtime would add build cost,
binary size, and complexity. It returns only with a measured requirement
plus an ADR.

## Lightness

- Third-party dependencies: 0 (`cargo tree` shows only `aurel-cli →
  aurel-core`).
- No background threads/workers, no network, no config file I/O.
- No micro-optimization was done to chase the baseline below.

## Phase 0 baseline (measured, informational)

Phase 0 baseline, measured on the stated host. Informational regression
reference — not a hard gate, not a universal guarantee, and not claimed at
sub-millisecond precision. Model memory is excluded because Phase 0 has no
model.

- Host: Windows 11 Home, AMD Athlon 300U, Rust 1.98.1, release profile
  (unless noted)
- Release binary: 140,288 bytes (137.0 KiB)
- Debug binary: 175,616 bytes (171.5 KiB)
- Observed peak working set: approximately 3.7 MB
- Startup, process start-to-exit on Windows (heavily influenced by Windows
  process creation/cache behavior; approximate means):
  - release `--version` ≈ 30.8 ms mean
  - release `--help` ≈ 21.19 ms mean
  - debug `--version` ≈ 17.49 ms mean
