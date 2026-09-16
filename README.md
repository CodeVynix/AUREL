# AUREL — Autonomous Utility & Reasoning Engine for Logic

AUREL is a personal, terminal-first AI coding agent. This repository holds its
Rust implementation, built as one shared foundation with a lightweight Core
edition and (later) an extended Normal edition.

> **Status: Phase 1 — CLI + configuration.** The workspace adds TOML
> configuration with documented precedence and a `config show` command on top
> of the Phase 0 foundation. There is still no model provider, no agent loop,
> no tools, no shell/Git automation, no memory, no GUI, and no Normal edition.

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
cargo run -p aurel-cli -- config show
```

Or with the built binary (Windows example):

```sh
.\target\debug\aurel.exe --version
.\target\debug\aurel.exe --help
.\target\debug\aurel.exe config show
```

Actual `--version` output:

```text
aurel 0.2.0
```

Actual `--help` output:

```text
aurel 0.2.0
Autonomous Utility & Reasoning Engine for Logic - Phase 1 CLI + config

USAGE:
    aurel [OPTIONS] [COMMAND]

OPTIONS:
    -h, --help              Print this help message
    -V, --version           Print version information
        --config <path>     Use this config file instead of
                            discovered global/project files
        --log-level <level> error|warn|info|debug|trace

COMMANDS:
    config show    Print the effective configuration

CONFIG FILES (TOML):
    global:  %APPDATA%\aurel\config.toml (Windows)
             ~/.config/aurel/config.toml (Linux/WSL)
    project: nearest .aurel/config.toml at or above the
             working directory
Precedence: defaults < global < project < env (AUREL_*) < CLI.
```

Behavior:

| Invocation | Exit code | Output |
| ---------- | --------- | ------ |
| `aurel` | 0 | help to stdout (config files untouched) |
| `aurel --help` / `-h` | 0 | help to stdout |
| `aurel --version` / `-V` | 0 | `aurel 0.2.0` to stdout |
| `aurel config show` | 0 | effective config as TOML to stdout |
| `aurel config --help` | 0 | command help to stdout |
| `aurel --wat` / `aurel frobnicate` / `--log-level bogus` | 2 | usage error to stderr |
| `aurel config show` with malformed TOML | 1 | config error (with file path) to stderr |
| `aurel --config missing.toml config show` | 1 | `config file not found` to stderr |

No color output. Works with pipes and `NO_COLOR`. Full precedence, grammar,
and error tables: `docs/configuration.md`.

## Configuration

TOML files, one setting so far (`log_level`, default `info`):

```toml
log_level = "debug"
```

Precedence: built-in defaults < global file < project file < `AUREL_*`
environment < CLI flags. `--config <path>` replaces file discovery.
Inspect the result with `aurel config show`.

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
Cargo.toml                  # workspace (resolver 2) + shared [workspace.package]
crates/aurel-core/          # shared library foundation (version API)
crates/aurel-config/        # TOML loading, precedence, discovery, config errors
crates/aurel-cli/           # `aurel` binary: std-only arg parser + dispatch
docs/architecture.md        # what the current phase actually contains
docs/configuration.md      # config precedence, grammar, errors
docs/decisions/             # ADRs for the foundation choices
.github/workflows/ci.yml    # fmt + clippy + build + test
```

## License

Dual-licensed under `MIT OR Apache-2.0`. See `LICENSE-MIT` and `LICENSE-APACHE`.
