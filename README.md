# AUREL — Autonomous Utility & Reasoning Engine for Logic

AUREL is a personal, terminal-first AI coding agent. This repository holds its
Rust implementation, built as one shared foundation with a lightweight Core
edition and (later) an extended Normal edition.

> **Status: Phase 8 — Git mutations.** `/git status|diff|branches|log`
> inspects the workspace repo (read-only, local only), and the
> `git_stage` / `git_unstage` / `git_commit` / `git_create_branch` /
> `git_switch_branch` proposal ops mutate it behind the same explicit
> approval workflow as file mutations: Build proposes, Plan blocks,
> `/approve` runs the exact shown `git` argv, `/undo` honestly refuses
> (undo never rewrites history). Local operations only — no push, pull,
> fetch, merge, remotes, or PRs. There is still no memory, no GUI, and no
> Normal edition.

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
cargo run -p aurel-cli -- agent "hello"
```

Or with the built binary (Windows example):

```sh
.\target\debug\aurel.exe --version
.\target\debug\aurel.exe --help
.\target\debug\aurel.exe config show
.\target\debug\aurel.exe chat "hello"
.\target\debug\aurel.exe agent "hello"
```

Bare `aurel` on an interactive terminal opens the loop (`build>` prompt);
with piped stdin it prints help as before:

```text
aurel interactive — build mode (Tab toggles, /help for commands, Ctrl-D to exit).
build> /plan
Switched to plan mode (mutations blocked).
plan> hello
...
```

Actual `--version` output:

```text
aurel 0.9.0
```

Actual `--help` output:

```text
aurel 0.9.0
Autonomous Utility & Reasoning Engine for Logic

USAGE:
    aurel [OPTIONS] [COMMAND]

OPTIONS:
    -h, --help              Print this help message
    -V, --version           Print version information
        --config <path>     Use this config file instead of
                            discovered global/project files
        --log-level <level> error|warn|info|debug|trace
        --model <name>      Model id for `chat` / `agent`
        --base-url <url>    Endpoint root for `chat` / `agent`
        --streaming <bool>  true|false (default true)
        --max-iterations <n> 1-100 agent loop bound for `agent`

COMMANDS:
    config show    Print the effective configuration
    chat [MESSAGE] Send one message to the model and print
                   the reply (reads piped stdin if omitted)
    agent [MESSAGE] Run one bounded agent turn sequence
                   and print the reply (stdin if omitted)

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
| `aurel --version` / `-V` | 0 | `aurel 0.9.0` to stdout |
| `aurel config show` | 0 | effective config as TOML to stdout (key redacted) |
| `aurel config` / `aurel config --help` | 0 | command help to stdout |
| `aurel chat "hi"` | 0 | model reply to stdout |
| `aurel chat --help` | 0 | command help to stdout |
| `aurel agent "hi"` | 0 | bounded run reply to stdout |
| `aurel agent --help` | 0 | command help to stdout |
| `aurel --wat` / `aurel frobnicate` / `--log-level bogus` | 2 | usage error to stderr |
| `aurel chat` (terminal, no message) | 2 | `no message given` to stderr |
| `aurel config show` with malformed TOML | 1 | config error (with file path) to stderr |
| `aurel --config missing.toml config show` | 1 | `config file not found` to stderr |
| `aurel chat "hi"` with no server running | 1 | `cannot reach model endpoint` to stderr |
| `aurel agent "hi"` hitting the iteration bound | 1 | partial reply + `iteration limit reached` warning |

No color output. Works with pipes and `NO_COLOR`. Full precedence, grammar,
and error tables: `docs/configuration.md`. Model setup, streaming, errors,
and secret policy: `docs/model-providers.md`.

## Configuration

TOML files: `log_level`, a `[model]` table (`name`, `base_url`, optional
`api_key`, `timeout_secs`, `max_retries`, `streaming`), and an `[agent]`
table (`max_iterations`, default 5):

```toml
log_level = "debug"

[model]
name = "my-local-model"
base_url = "http://127.0.0.1:8080/v1"

[agent]
max_iterations = 5
auto_compaction = true
```

Precedence: built-in defaults < global file < project file < `AUREL_*`
environment < CLI flags. `--config <path>` replaces file discovery.
Inspect the result with `aurel config show` (the API key always prints as
`<redacted>`). There is no `--api-key` flag — use a config file or
`AUREL_API_KEY`.

## Interactive loop

Bare `aurel` on a terminal starts the loop; everything else also works
one-shot (`aurel agent "hi"`, `aurel chat "hi"`, `aurel config show`).

| Input | Effect |
| ----- | ------ |
| `Tab` (bare) | Toggle Plan ↔ Build |
| `/plan`, `/build` | Switch mode (shown in prompt and `/status`) |
| `/help`, `/version`, `/status`, `/history`, `/context` | Local reports |
| `/compact` | Summarize history via the model, keep going |
| `/btw <q>` | Side answer without touching the main task |
| `/new` | Fresh session (mode preserved) |
| `/settings [show\|set auto_compaction on\|off]` | Session-scoped settings |
| `/config` | Effective config (key redacted) |
| `/model` | Honestly reports its backend is unimplemented |
| `/tools` | List registered tools (all read-only in this phase) |
| `/init [--force]` | Create `AGENTS.md` starter (never overwrites silently) |
| `/approve [#id]`, `/deny [#id]` | Apply / drop the pending proposal (Build only) |
| `/diff` | Re-show the pending proposal diff |
| `/git <status|diff|branches|log>` | Inspect the workspace Git repo (read-only, local only) |
| `/undo` | Reverse the last AUREL-applied change |
| `@general`, `@explore` | Prompt scope (`@explore` notes cross-session is future) |
| `!command` | Queues a shell command for approval (direct execution, no shell) |
| blank line | Prints the input hint, then reprompts |
| `/exit`, `/quit`, Ctrl-D | Leave the loop |

Plan mode reasons and proposes but must not mutate (`Mode::allows_mutation`
is the gate future tools must check). Auto-compaction is on by default
(threshold 20 messages, keeps 4); toggle per session via `/settings`.

## Read-only tools

Four inspection tools share one workspace sandbox (traversal, absolute
escapes, and symlink breakouts are rejected; everything is bounded):

| Tool | Does |
| ---- | ---- |
| `read_file` | Read a text file with line offset/limit |
| `list_dir` | List entries, optionally recursive to a bounded depth |
| `stat` | Kind, size, and modification time of one path |
| `search` | Substring search over text files (skips `.git`/`target`/binaries) |

List them at runtime with `/tools`. They read only: no writes, no shell,
no hidden access. Git repository state reads through `/git status|diff|
branches|log` (same sandbox, same read-only tier); anything that changes
the repo needs the approval workflow below.

## File mutations + approval (Build mode)

In Build mode the agent may propose file mutations as fenced
`aurel-mutation` blocks (`create_file`, `edit_file`, `overwrite_file`,
`move`, `delete_file`). Proposals are parsed, sandboxed, snapshotted, and
shown as diffs — never auto-applied. Review with `/diff`, apply with
`/approve`, drop with `/deny`, revert with `/undo` (session-scoped, only
AUREL-applied changes). Plan mode holds proposals without applying;
approval re-verifies prior bytes, so external edits fail as stale instead
of applying. One-shot `aurel agent` prints proposals and exits 1 since it
cannot approve.

## Shell + build/test execution (Build mode, approved)

`!command` queues a shell command exactly like a model proposal — same
queue, same diff review, same `/approve`. Execution is direct process
spawn, never a shell: the program plus literal arguments runs in the
workspace directory, so pipes, redirects, globs, and expansions do not
exist (a `!` line needing them is rejected at parse or resolve time).
Every run is bounded (60 s timeout, 1 MiB per output stream), reports
success / nonzero exit / timeout / cancellation / launch failure
separately, and scrubs the configured API key from captured output.
Child processes inherit the environment minus secret variables
(`AUREL_API_KEY` never passes through); ordinary variables are preserved
so builds behave. Plan mode holds shell proposals without applying, like
file mutations.
Shell effects cannot be undone — `/undo` says so explicitly instead of
pretending. Project builds and tests run through the same system:
`run_build` / `run_tests` proposals resolve `Cargo.toml`, `package.json`,
`go.mod`, or `Makefile` in that order. Local Git work has its own typed
layer (see below); an explicit `!git ...` line still queues as an ordinary
shell proposal showing its exact argv, but AUREL never runs remote or
history-rewriting Git on its own.

## Git mutations (Build mode, approved)

`/git status` (branch, staged/unstaged/untracked, ahead/behind),
`/git diff [--staged]`, `/git branches`, and `/git log [N]` inspect the
repository containing the workspace — read-only, allowed in Plan and
Build alike, over the same direct-spawn command sandbox as shell runs.

In Build mode the agent may propose local Git mutations as fenced
`aurel-mutation` blocks: `git_stage` / `git_unstage` (sandbox-resolved
paths, directories refused for staging), `git_commit` (non-empty message,
something staged, never on a detached HEAD), `git_create_branch`, and
`git_switch_branch`. Every proposal shows its exact `git` argv, the repo
it runs in, and the repository state it was taken from; `/approve`
re-verifies HEAD + branch + status byte-identical and drops stale
proposals instead of applying against a moved tree. Approval prints Git's
own output and labels the run local-only. Only these five subcommands can
ever execute (`add`, `reset`, `commit`, `branch`, `switch`, plus read-only
inspection): push, pull, fetch, merge, rebase, remote, and clone have no
constructors, a runtime guard, and a regression test pinning that. Applied
Git operations are recorded for audit but never undone — `/undo` refuses,
because undoing them would mean rewriting history.

## Project instructions (`AGENTS.md`)

`/init` writes a starter `AGENTS.md` into the working directory — only
when absent, or with explicit `/init --force`. Every agent run
(one-shot or interactive) loads the nearest `AGENTS.md` above the working
directory and sends it as a leading `system` message; conversation
history never stores it. Oversized files truncate with a marker.

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
crates/aurel-model/         # provider abstraction + OpenAI-compatible provider + agent loop
crates/aurel-tools/         # inspection + AGENTS.md + approved file/shell/Git mutations
crates/aurel-cli/           # `aurel` binary: std-only arg parser + dispatch + interactive loop
docs/architecture.md        # what the current phase actually contains
docs/configuration.md      # config precedence, grammar, errors
docs/model-providers.md     # model setup, chat, streaming, errors, secrets
docs/decisions/             # ADRs for the foundation choices
.github/workflows/ci.yml    # fmt + clippy + build + test
```

## License

Dual-licensed under `MIT OR Apache-2.0`. See `LICENSE-MIT` and `LICENSE-APACHE`.
