# AUREL Architecture — Phase 2

This document describes what Phase 2 actually contains. Nothing more.

## Workspace

Single Cargo workspace (`Cargo.toml`, `resolver = "2"`) with four members.
Shared package metadata (`version`, `edition`, `rust-version`, `license`)
is defined once in `[workspace.package]` and inherited by all crates, so
there is one version definition feeding package metadata, the runtime
display (`aurel_core::version()`), and the test expectations:

- `crates/aurel-core` — library crate (`aurel_core`). Shared foundation.
  Still exposes only `aurel_core::version()`, sourced from
  `CARGO_PKG_VERSION`. No agent, tool, session, storage, or Git modules
  exist yet, and no empty placeholder crates were created for them.
- `crates/aurel-config` — library crate (`aurel_config`). TOML loading,
  file discovery, precedence, config errors, and the `[model]` settings
  with API-key redaction.
- `crates/aurel-model` — library crate (`aurel_model`). The
  provider-agnostic abstraction (`Message`, `ChatRequest`, `ChatResponse`,
  `Capabilities`, `ProviderError`, `ModelProvider` trait) plus the
  OpenAI-compatible provider. Owns the HTTP/TLS/JSON dependencies (see
  ADR-0006); nothing else in the workspace touches the network.
- `crates/aurel-cli` — binary crate producing the `aurel` binary
  (`[[bin]] name = "aurel"`). Depends on all three libraries via local path
  dependencies. Argument parsing itself is still dependency-free.

The root manifest is workspace-only (no root package). `Cargo.lock` is
committed because the workspace produces a binary.

## CLI

`crates/aurel-cli/src/args.rs` implements the Phase 2 grammar dependency-free
over already-normalized `String` arguments (see ADR-0004); `src/main.rs`
dispatches. The live `Runtime` (working directory, `%APPDATA%`/`$HOME`,
filtered `AUREL_*` env) is constructed lazily, only on the command path —
help/version/bare invocations never touch environment or config state, so
they stay independent of configuration/environment failures. For tests,
`run_with` injects a fake `Runtime` and fake stdin, keeping precedence and
message acquisition unit-testable without touching real user state;
`tests/*.rs` cover the compiled binary via `CARGO_BIN_EXE_aurel` with
isolated temp homes and working directories:

- empty args / `-h` / `--help` → top help to stdout, exit 0, config untouched
- `-V` / `--version` → `aurel <version>` to stdout, exit 0, config untouched
  (version comes from `aurel_core::version()`, single source of truth)
- `config show` → effective TOML to stdout, exit 0 (key redacted)
- `config` / `config --help` → command help, exit 0
- `chat [MESSAGE]...` → one model exchange, reply to stdout, exit 0
  (piped stdin when omitted; terminal without message is exit 2)
- `chat --help` → command help, exit 0
- unknown flags/commands, bad values, `--version` + command → stderr, exit 2
  (usage errors take precedence over `--help`: help prints only when the
  surrounding command line is valid)
- malformed TOML / bad file values (path included) / bad `AUREL_*` /
  missing `--config` file / provider failures → stderr, exit 1

No color, no terminal requirements, safe under pipes. IO failures return
exit 1 instead of panicking. Both OS-input paths are total: `args_os` +
lossy conversion for argv, `vars_os` + key-filter/lossy-value policy for the
environment (`collect_aurel_env`), so non-Unicode argv or environment data
is handled deterministically with no panic in crate-controlled logic.
No `unwrap`/`expect` exists on the runtime paths (only in tests). No broader
claim is made about std/OS internals outside this crate's control.

## Model layer

`aurel chat` builds one `ChatRequest` (single user turn) from resolved
`[model]` settings and runs it through `ModelProvider` — streaming deltas
print progressively, one-shot replies print whole. There is no prompt
building from repositories, no tool calls, no follow-up turns: strictly
one exchange. Details live in `docs/model-providers.md`, the normative
reference for provider behavior, errors, retries, and secrets.

## Configuration

`log_level` plus the `[model]` table (`name`, `base_url`, optional
`api_key`, `timeout_secs`, `max_retries`, `streaming`), all through the
same precedence machinery: defaults < global file < project file < env <
CLI, plus `--config` replacing discovery. Files are TOML parsed with
unknown-field rejection; missing files are skipped except an explicit
`--config` path. `config show` reports each layer honestly and always
redacts the key. Details, grammar, and error tables live in
`docs/configuration.md`, which is the normative reference — this file
only summarizes.

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

Phases 0–2 do no I/O concurrency: no parallel tools, no background tasks —
one-shot chat, sequential bounded retries, and SSE streaming all work on
blocking I/O (see `docs/decisions/0003-no-async-and-workspace.md` and
ADR-0006). Synchronous std code is sufficient; an async runtime would add
build cost, binary size, and complexity. It returns only with a measured
requirement plus an ADR.

## Lightness

- Third-party dependencies: `serde` + parse-only `toml` (config), plus the
  HTTP/TLS/JSON stack (`ureq`, `rustls` + crypto, `serde_json`) confined to
  `aurel-model` (`cargo tree` shows `aurel-core` still dependency-free; the
  parser in `aurel-cli` is dependency-free).
- No background threads/workers (test stub servers excepted), no config
  file I/O except the files being loaded, no model-server code inside
  AUREL (server memory is the server's, never attributed here).
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

## Phase 2 delta (measured, informational)

Same host and caveats as above. The HTTP/TLS/JSON stack (`ureq` with
`rustls`, `serde_json`, confined to `aurel-model`) is the only footprint
change; the parser, config machinery, and `chat` dispatch added none.

- Release binary: 2,826,752 bytes (≈2.7 MiB), i.e. +2.3 MiB vs Phase 1,
  dominated by the TLS stack (`rustls` + `ring` + `webpki-roots`).
  Still far under the < 15 MB Core target. Full graph in ADR-0006.
- Startup, warm process start-to-exit: release `--version` ≈22–36 ms —
  unchanged; the provider builds lazily per `chat` invocation and costs
  nothing at startup.
- AUREL process resources vs model-server resources are measured
  separately: this baseline covers the AUREL binary only. A local model's
  gigabytes are the server's, never attributed to AUREL.
