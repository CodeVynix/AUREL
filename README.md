# AUREL — Autonomous Utility & Reasoning Engine for Logic

AUREL is a personal, terminal-first AI coding agent. This repository holds its
Rust implementation, built as one shared foundation with a lightweight Core
edition and (later) an extended Normal edition.

> **Status: Phase 2 — Model provider layer.** AUREL can exchange one message
> with a configured (OpenAI-compatible, local-first) model via `aurel chat`.
> There is still no agent loop, no tools, no shell/Git automation, no memory,
> no GUI, and no Normal edition. `aurel chat` never operates the computer.

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

(`ring` compiles C code via `cc` during the build — standard on the
supported platforms, no extra setup.)

## Run

```sh
cargo run -p aurel-cli -- --version
cargo run -p aurel-cli -- --help
cargo run -p aurel-cli -- config show
cargo run -p aurel-cli -- chat "hello"
```

Or with the built binary (Windows example):

```sh
.\target\debug\aurel.exe --version
.\target\debug\aurel.exe --help
.\target\debug\aurel.exe config show
.\target\debug\aurel.exe chat "hello"
```

Actual `--version` output:

```text
aurel 0.3.0
```

Actual `--help` output:

```text
aurel 0.3.0
Autonomous Utility & Reasoning Engine for Logic

USAGE:
    aurel [OPTIONS] [COMMAND]

OPTIONS:
    -h, --help              Print this help message
    -V, --version           Print version information
        --config <path>     Use this config file instead of
                            discovered global/project files
        --log-level <level> error|warn|info|debug|trace
        --model <name>      Model id for `chat`
        --base-url <url>    Endpoint root for `chat`
        --streaming <bool>  true|false (default true)

COMMANDS:
    config show    Print the effective configuration
    chat [MESSAGE] Send one message to the model and print
                   the reply (reads piped stdin if omitted)

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
| `aurel --version` / `-V` | 0 | `aurel 0.3.0` to stdout |
| `aurel config show` | 0 | effective config as TOML to stdout (key redacted) |
| `aurel config` / `aurel config --help` | 0 | command help to stdout |
| `aurel chat "hi"` | 0 | model reply to stdout |
| `aurel chat --help` | 0 | command help to stdout |
| `aurel --wat` / `aurel frobnicate` / `--log-level bogus` | 2 | usage error to stderr |
| `aurel chat` (terminal, no message) | 2 | `no message given` to stderr |
| `aurel config show` with malformed TOML | 1 | config error (with file path) to stderr |
| `aurel --config missing.toml config show` | 1 | `config file not found` to stderr |
| `aurel chat "hi"` with no server running | 1 | `cannot reach model endpoint` to stderr |

No color output. Works with pipes and `NO_COLOR`. Full precedence, grammar,
and error tables: `docs/configuration.md`. Model setup, streaming, errors,
and secret policy: `docs/model-providers.md`.

## Configuration

TOML files: `log_level` plus a `[model]` table (`name`, `base_url`,
optional `api_key`, `timeout_secs`, `max_retries`, `streaming`):

```toml
log_level = "debug"

[model]
name = "my-local-model"
base_url = "http://127.0.0.1:8080/v1"
```

Precedence: built-in defaults < global file < project file < `AUREL_*`
environment < CLI flags. `--config <path>` replaces file discovery.
Inspect the result with `aurel config show` (the API key always prints as
`<redacted>`). There is no `--api-key` flag — use a config file or
`AUREL_API_KEY`.

## Test

```sh
cargo test --workspace
```

Provider tests run against a local stub HTTP server (no network, no API
keys). Checks run by CI (Windows + Ubuntu):

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
crates/aurel-model/         # provider abstraction + OpenAI-compatible provider
crates/aurel-cli/           # `aurel` binary: std-only arg parser + dispatch
docs/architecture.md        # what the current phase actually contains
docs/configuration.md      # config precedence, grammar, errors
docs/model-providers.md     # model setup, chat, streaming, errors, secrets
docs/decisions/             # ADRs for the foundation choices
.github/workflows/ci.yml    # fmt + clippy + build + test
```

## License

Dual-licensed under `MIT OR Apache-2.0`. See `LICENSE-MIT` and `LICENSE-APACHE`.
