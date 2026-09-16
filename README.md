# AUREL — Autonomous Utility & Reasoning Engine for Logic

AUREL is a personal, terminal-first AI coding agent. This repository holds its
Rust implementation, built as one shared foundation with a lightweight Core
edition and (later) an extended Normal edition.

> **Status: Phase 0 — Foundation.** The workspace compiles, formats, lints,
> builds, and tests cleanly. The only runtime behavior is `aurel --version`
> and `aurel --help`. There is no agent loop, no model provider, no tools,
> no shell/Git automation, no memory, no GUI, and no Normal edition yet.

## Toolchain

Pinned: Rust **1.98.1** (see `rust-toolchain.toml`), edition 2021.
`rustfmt` and `clippy` components are required.

```sh
rustc --version
cargo --version
```

## Build

```sh
cargo build --workspace
```

Release:

```sh
cargo build --release
```

## Run

```sh
cargo run -p aurel-cli -- --version
cargo run -p aurel-cli -- --help
```

Or with the built binary (Windows example):

```sh
.\target\debug\aurel.exe --version
.\target\debug\aurel.exe --help
```

Actual `--version` output:

```text
aurel 0.1.0
```

Actual `--help` output:

```text
aurel 0.1.0
Autonomous Utility & Reasoning Engine for Logic - Phase 0 foundation

USAGE:
    aurel [OPTIONS]

OPTIONS:
    -h, --help       Print this help message
    -V, --version    Print version information
```

Behavior:

| Invocation      | Exit code | Output |
| --------------- | --------- | ------ |
| `aurel`         | 0         | help to stdout |
| `aurel --help`  | 0         | help to stdout |
| `aurel -h`      | 0         | help to stdout |
| `aurel --version` | 0       | `aurel 0.1.0` to stdout |
| `aurel -V`      | 0         | `aurel 0.1.0` to stdout |
| `aurel --wat`   | 2         | error to stderr |

No color output. Works with pipes and `NO_COLOR`.

## Test

```sh
cargo test --workspace
```

Checks run by CI (Windows + Ubuntu):

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo build --workspace
cargo test --workspace
```

## Layout

```text
Cargo.toml                  # workspace (resolver 2): aurel-core + aurel-cli
crates/aurel-core/          # shared library foundation (Phase 0: version API only)
crates/aurel-cli/           # `aurel` binary, std-only arg handling
docs/architecture.md        # what Phase 0 actually contains
docs/decisions/             # ADRs for the foundation choices
.github/workflows/ci.yml    # fmt + clippy + build + test
```

## License

Dual-licensed under `MIT OR Apache-2.0`. See `LICENSE-MIT` and `LICENSE-APACHE`.
