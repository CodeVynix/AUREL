# AUREL Architecture — Phase 0

This document describes what Phase 0 actually contains. Nothing more.

## Workspace

Single Cargo workspace (`Cargo.toml`, `resolver = "2"`) with two members:

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

`crates/aurel-cli/src/main.rs` parses `std::env::args` with no CLI framework.
All branching lives in `run(args, out, err) -> i32`, unit-tested in-file;
`tests/cli.rs` covers the compiled binary via `CARGO_BIN_EXE_aurel`:

- empty args / `-h` / `--help` → help to stdout, exit 0
- `-V` / `--version` → `aurel <version>` to stdout, exit 0
  (version comes from `aurel_core::version()`, single source of truth)
- anything else → error to stderr, exit 2

No color, no terminal requirements, safe under pipes. IO failures return
exit 1 instead of panicking; there are no `unwrap`/`expect` paths in the
runtime (only in tests).

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
- Release baseline is recorded at the end of Phase 0; no micro-optimization
  was done to chase it.
