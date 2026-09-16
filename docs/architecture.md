# AUREL Architecture — Phase 1

This document describes what Phase 1 actually contains. Nothing more.

## Workspace

Single Cargo workspace (`Cargo.toml`, `resolver = "2"`) with three members.
Shared package metadata (`version`, `edition`, `rust-version`, `license`)
is defined once in `[workspace.package]` and inherited by all crates, so
there is one version definition feeding package metadata, the runtime
display (`aurel_core::version()`), and the test expectations:

- `crates/aurel-core` — library crate (`aurel_core`). Shared foundation.
  Still exposes only `aurel_core::version()`, sourced from
  `CARGO_PKG_VERSION`. No agent, tool, model, session, storage,
  security, or Git modules exist yet, and no empty placeholder crates were
  created for them.
- `crates/aurel-config` — library crate (`aurel_config`). TOML loading,
  file discovery, precedence, and config errors. Depends on `serde`
  (derive) and parse-only `toml` — the only third-party dependencies in
  the workspace (see ADR-0005).
- `crates/aurel-cli` — binary crate producing the `aurel` binary
  (`[[bin]] name = "aurel"`). Depends on both libraries via local path
  dependencies. Argument parsing itself is still dependency-free.

The root manifest is workspace-only (no root package). `Cargo.lock` is
committed because the workspace produces a binary.

## CLI

`crates/aurel-cli/src/args.rs` implements the Phase 1 grammar dependency-free
over already-normalized `String` arguments (see ADR-0004); `src/main.rs`
dispatches. The live `Runtime` (working directory, `%APPDATA%`/`$HOME`,
filtered `AUREL_*` env) is constructed lazily, only on the command path —
help/version/bare invocations never touch environment or config state, so
they stay independent of configuration/environment failures. For tests,
`run_with` injects a fake `Runtime`, keeping precedence unit-testable
without touching real user state; `tests/config.rs` covers the compiled
binary via `CARGO_BIN_EXE_aurel` with isolated temp homes and working
directories:

- empty args / `-h` / `--help` → top help to stdout, exit 0, config untouched
- `-V` / `--version` → `aurel <version>` to stdout, exit 0, config untouched
  (version comes from `aurel_core::version()`, single source of truth)
- `config show` → effective TOML to stdout, exit 0
- `config` / `config --help` → command help, exit 0
- unknown flags/commands, bad values, `--version` + command → stderr, exit 2
  (usage errors take precedence over `--help`: help prints only when the
  surrounding command line is valid)
- malformed TOML / bad file values (path included) / bad `AUREL_LOG_LEVEL` /
  missing `--config` file → stderr, exit 1

No color, no terminal requirements, safe under pipes. IO failures return
exit 1 instead of panicking. Both OS-input paths are total: `args_os` +
lossy conversion for argv, `vars_os` + key-filter/lossy-value policy for the
environment (`collect_aurel_env`), so non-Unicode argv or environment data
is handled deterministically with no panic in crate-controlled logic.
No `unwrap`/`expect` exists on the runtime paths (only in tests). No broader
claim is made about std/OS internals outside this crate's control.

## Configuration

One real setting (`log_level`, default `info`) with full precedence
machinery: defaults < global file < project file < env < CLI, plus
`--config` replacing discovery. Files are TOML parsed with unknown-field
rejection; missing files are skipped except an explicit `--config` path.
`config show` reports each layer honestly, including the three project
states (found path / `searched, none found` / `not searched`).
Details, grammar, and error tables live in `docs/configuration.md`, which
is the normative reference — this file only summarizes.

## Why Normal does not exist yet

The long-term design is two binaries (`aurel`, `aurel-normal`) sharing this
foundation (see `docs/decisions/0001-two-binaries-reserved.md`). Phase 0
ships only `aurel` so the Core footprint, build graph, and review surface
stay minimal. Normal arrives in Phase 11+ as additive crates/bins.

## Why the CLI is std-only

`--version`/`--help` need a few string matches. A framework would add
compile time, binary size, and startup cost for zero Phase 0 benefit
(see `docs/decisions/0002-std-only-cli.md`). Phase 1 re-evaluated the choice
with real subcommands on the table and kept the hand parser (see
`docs/decisions/0004-phase1-cli-parser.md`).

## Why there is no async runtime

Phase 0 does no I/O concurrency: no model streaming, no parallel tools, no
background tasks (see `docs/decisions/0003-no-async-and-workspace.md`).
Synchronous std code is sufficient; an async runtime would add build cost,
binary size, and complexity. It returns only with a measured requirement
plus an ADR.

## Lightness

- Third-party dependencies: `serde` + parse-only `toml` and their transitive
  graph, all confined to `aurel-config` (`cargo tree` shows `aurel-core`
  still dependency-free; the parser in `aurel-cli` is dependency-free).
- No background threads/workers, no network, no config file I/O except the
  files being loaded.
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

## Phase 1 delta (measured, informational)

Same host and caveats as above. The TOML stack (`serde` + parse-only
`toml`, confined to `aurel-config`) is the only footprint change; the
parser added none.

- Release binary: 492,544 bytes (≈481 KiB), i.e. +352,256 bytes vs Phase 0.
  Still far under the < 15 MB Core target. Full graph in ADR-0005.
- Startup, warm process start-to-exit: release `--version` ≈19–68 ms,
  release `--help` ≈16–76 ms — same ballpark as Phase 0, no meaningful
  regression. (First cold spawn after a rebuild ≈415 ms: Windows process
  creation + cold disk cache, as in Phase 0.)
