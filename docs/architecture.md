# AUREL Architecture — Phase 8

This document describes what Phase 8 actually contains. Nothing more.

## Workspace

Single Cargo workspace (`Cargo.toml`, `resolver = "2"`) with five members.
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
  `Capabilities`, `ProviderError`, `ModelProvider` trait), the
  OpenAI-compatible provider, and the basic agent loop (`Agent`,
  `AgentSession`, `AgentOutcome`). Owns the HTTP/TLS/JSON dependencies (see
  ADR-0006); nothing else in the workspace touches the network.
- `crates/aurel-cli` — binary crate producing the `aurel` binary
  (`[[bin]] name = "aurel"`). Depends on all four libraries via local path
  dependencies. Argument parsing itself is still dependency-free.
- `crates/aurel-tools` — library crate (`aurel_tools`). Read-only
  inspection tools, `AGENTS.md` project instructions, and the approved
  file/shell/Git mutation layers. Only `serde`/`serde_json` beyond std,
  used solely for parsing model-proposed mutation blocks.

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
- `agent [MESSAGE]...` → one bounded run, reply to stdout, exit 0
  (same message acquisition as `chat`)
- `agent --help` → command help, exit 0
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

## Agent loop

`aurel agent` runs one bounded turn sequence through `Agent<P:
ModelProvider>` with a fresh in-memory `AgentSession`: push the user
message, request, append the reply, and — only for truncated (`length`)
turns — append a `system` "Continue." cue and request again, up to
`max_iterations` (default 5, hard cap 100). Outcomes are explicit:
`Completed`, `IterationLimitReached` (partial work kept, exit 1 with a
warning), `Cancelled`, and `ProviderError` — each carrying accumulated
content, call count, and summed usage. Cancellation reuses the Phase 2
`CancelFlag`. Project instructions ride along as a leading `system`
message per request without entering history.

## Read-only tools

`crates/aurel-tools` (`ToolContext`, zero dependencies) implements the
four inspection tools — `read_file`, `list_dir`, `stat`, `search` — behind
one workspace sandbox: every path is joined to the bound root,
canonicalized (resolving symlinks to their real targets), and rejected
with `OutsideWorkspace` unless it stays inside. `..` traversal, absolute
escapes, and symlink breakouts all fail closed; failures are typed
(`ToolError`), never panics. Every tool is bounded (`Limits`: byte caps,
entry/match caps, depth cap, per-line caps) and `search` additionally
skips `.git`/`target`/binaries/oversized files, never follows symlinks,
and honors cooperative cancellation. All four carry
`Permission::ReadOnly`, valid in both Plan and Build modes — Plan forbids
*mutation*, and nothing here mutates. Listed at runtime via `/tools`.

Git repository inspection (`git_status`, `git_diff`, `git_branches`,
`git_log`, `git_toplevel` in `crates/aurel-tools/src/git.rs`) holds the
same read-only tier: it runs the real `git` binary through the shell
phase's direct-spawn sandbox (workspace root as working directory,
timeouts, output caps, cancellation, secret-filtered environment) and is
surfaced via `/git status|diff|branches|log`, allowed in both modes.

## Project instructions (`AGENTS.md`)

`/init` writes a starter `AGENTS.md` into the working directory — only
when absent, or with explicit `/init --force`; existing files are
reported, never silently replaced. Every agent run (one-shot or
interactive) discovers the nearest `AGENTS.md` above the working
directory and sends it as the leading `system` message described above;
absent files run bare, unreadable ones warn and continue. Oversized files
truncate at 64 KiB with a marker. The file itself stays user-owned:
AUREL never edits it.

## File mutations + approval

In Build mode the agent may propose file mutations as fenced
`aurel-mutation` JSON blocks (`create_file`, `edit_file` with an exact
single-anchor match, `overwrite_file`, `move`, `delete_file`). Each block
is parsed strictly (unknown ops/fields fail loudly), resolved through the
same workspace sandbox, snapshotted, and rendered as a bounded diff — then
queued, never executed. `/diff` re-shows the front proposal; `/approve`
re-checks Build mode, the requested id, and byte-identical prior state
before one atomic apply (temp file + backup swap), recording the inverse
for session-scoped `/undo`; `/deny` drops without executing. External
edits between proposal and approval fail as stale. Plan mode queues for
review but its approvals are always rejected. One-shot `aurel agent`
prints proposals and exits 1 since it cannot approve. Writes are bounded
(256 KiB), parents must exist, destinations must be absent-or-expected,
and errors name paths only — never contents.

## Shell execution

`!command` and the `run_command` / `run_build` / `run_tests` proposal ops
share the file-mutation approval queue above: Build proposes, Plan holds,
`/approve` executes, `/deny` drops, `/undo` honestly refuses (shell
effects cannot be reversed). Execution itself is direct process spawn —
never a shell — with the program plus literal arguments in the workspace
directory, so pipes, redirects, globs, and expansions do not exist.
Security boundaries, stated plainly:

- Exact resolved program shown before approval; bare names search `PATH`
  (never the current directory), absolute paths must exist, no allowlist
  to maintain — approval visibility is the control.
- 60 s timeout and 1 MiB per-stream output caps (drained, never deadlock);
  stdin is always null; cooperative cancellation kills promptly.
- The child environment is the parent's *minus* secret variables (exact
  names, currently `AUREL_API_KEY`, ASCII case-insensitive): ordinary
  variables pass through so builds behave, credentials never do. Output
  redaction stays a second, independent defense.
- Success, nonzero exit, timeout, cancellation, and launch failure are
  distinct typed states, never one generic error.

Project builds/tests resolve `Cargo.toml`, then `package.json`, `go.mod`,
`Makefile`. Local Git work has its own typed layer (next section); an
explicit `!git ...` line still queues as an ordinary shell proposal, but
AUREL never runs remote or history-rewriting Git on its own.

## Git mutations

`crates/aurel-tools/src/git.rs` adds local-only Git operations on top of
the same proposal → diff/review → explicit approval flow as files and
shell. The model proposes five fenced ops (`git_stage`, `git_unstage`,
`git_commit`, `git_create_branch`, `git_switch_branch`); each prepares by
detecting the repository containing the workspace (`git rev-parse
--show-toplevel`, canonicalized), validating (non-empty bounded paths
resolved through the sandbox, no directory staging, non-empty bounded
commit message with staged work present, never on a detached HEAD,
branch names checked locally and via `git check-ref-format`, create
refuses existing names, switch refuses unknown or current names),
snapshotting HEAD + branch + porcelain status, and rendering a review
block that states the local-only boundary and shows the exact `git` argv.
`/diff` re-shows it; `/approve` re-checks Build mode, the requested id,
and byte-identical repository state before running the exact argv through
the shell phase's runner (secret redaction included), printing Git's own
output labeled as a local operation; `/deny` drops; stale trees fail as
stale. `/undo` refuses applied Git operations explicitly, because
reversing them would mean rewriting history. Security boundaries, stated
plainly:

- Argument vectors are built from typed operations, never caller text, so
  only `add`, `reset`, `commit`, `branch`, `switch` (plus read-only
  `status`, `diff`, `rev-parse`, `symbolic-ref`, `check-ref-format`,
  `log`) can execute. Push, pull, fetch, merge, rebase, remote, and clone
  have no constructors, a runtime refusal guard, and a regression test
  pinning the property — local and remote GitHub operations cannot be
  confused because the remote half has no code path.
- Stage/unstage paths obey the workspace sandbox; the repository itself
  only needs to contain the workspace root.
- Approval freshness covers the whole tree snapshot, not just touched
  paths: any commit or status change between proposal and approval fails
  closed.
- Failures carry Git's own stdout/stderr (clipped, secret-scrubbed) as
  typed `ToolError`s — output is never hidden.

## Interaction layer

Bare `aurel` on a terminal enters a minimal line REPL
(`crates/aurel-cli/src/interactive.rs` — no TUI framework, injected
streams throughout); piped input keeps the Phase 0 help behavior. One
`Agent`/`AgentSession` pair lives for the whole loop:

- Modes: `Mode::{Plan, Build}` travels with the session and is stamped
  into every result. `/plan`, `/build`, and a bare Tab toggle it; the
  prompt and `/status` always show it. `allows_mutation()` is the gate
  future tools must consult — Plan performs no mutations.
- Slash commands are parsed and dispatched locally, never sent to the
  model: help/version/plan/build/status/compact/btw/new/history/context/
  settings/config/tools/init/approve/deny/diff/undo/exit/quit. Backends
  owned by later phases (`/model`, shell execution, cross-session
  retrieval) say so instead of pretending.
- `@general` (default) and `@explore` annotate one prompt's scope;
  `@explore` answers from the current session with an explicit notice.
- `/btw` runs a side question on a private session clone and discards it:
  history, mode, and counters are byte-identical afterwards.
- `/compact` (manual) and auto-compaction (on by default, threshold 20,
  keeps 4) summarize through the model layer into one `system` message.
- `/new` clears history, pending proposals, and undo state (mode
  preserved). `/settings` toggles `auto_compaction` session-scoped — no
  file writes except through the approval workflow below.
- `!command` queues an explicit shell request into the same approval
  flow as file mutations (direct execution, no shell).

## Configuration

`log_level`, the `[model]` table (`name`, `base_url`, optional
`api_key`, `timeout_secs`, `max_retries`, `streaming`), and the `[agent]`
table (`max_iterations`, `auto_compaction`), all through the
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
  `aurel-model`; `aurel-tools` adds only `serde`/`serde_json` edges for
  mutation-block parsing, no new crates (`cargo tree` shows `aurel-core`
  still dependency-free; the parser in `aurel-cli` is dependency-free).
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

## Phase 3 delta (measured, informational)

Same host and caveats as above. No new dependencies and no new crates —
one module (`aurel-model/src/agent.rs`), config keys, and CLI wiring.

- Release binary: 2,871,296 bytes (≈2.74 MiB), i.e. +37,376 bytes
  (+1.3%) vs Phase 2. Still far under the < 15 MB Core target.
- Startup, warm process start-to-exit: release `--version` ≈20–25 ms —
  unchanged; the loop builds per `agent` invocation and costs nothing
  at startup.

## Phase 4 delta (measured, informational)

Same host and caveats as above. No new dependencies and no new crates —
one CLI module (`interactive.rs`), agent-session mode/compaction methods,
and two config keys.

- Release binary: 2,920,960 bytes (≈2.79 MiB), i.e. +49,664 bytes
  (+1.7%) vs Phase 3. Still far under the < 15 MB Core target.
- Startup, warm process start-to-exit: release `--version` ≈24–32 ms —
  unchanged; the REPL builds per bare invocation and costs nothing
  at startup.

## Phase 5 delta (measured, informational)

Same host and caveats as above. One new crate (`aurel-tools`) with zero
third-party dependencies, plus session instructions plumbing and CLI
wiring — no new crates.io graph entries at all.

- Release binary: 2,933,248 bytes (≈2.80 MiB), i.e. +12,288 bytes
  (+0.4%) vs Phase 4. Still far under the < 15 MB Core target.
- Startup, warm process start-to-exit: release `--version` ≈26–31 ms —
  unchanged; tools and instructions load lazily per invocation.

## Phase 6 delta (measured, informational)

Same host and caveats as above. Mutation machinery inside `aurel-tools`
plus approval wiring in the CLI — two crates.io edges (`serde`,
`serde_json`, both already in the graph), no new crates, no async.

- Release binary: 3,085,312 bytes (≈2.94 MiB), i.e. +152,064 bytes
  (+5.2%) vs Phase 5, mostly new code paths rather than dependencies.
  Still far under the < 15 MB Core target.
- Startup, warm process start-to-exit: release `--version` ≈18–26 ms —
  unchanged; approval state builds per session and costs nothing
  at startup.

## Phase 7 delta (measured, informational)

Same host and caveats as above. Direct-spawn execution plus approval
wiring for shell commands — `std::process`/`std::thread` only, no new
crates.io graph entries, no async.

- Release binary: 3,243,008 bytes (≈3.09 MiB), i.e. +157,696 bytes
  (+5.1%) vs Phase 6, mostly new code paths rather than dependencies.
  Still far under the < 15 MB Core target.
- Startup, warm process start-to-exit: release `--version` ≈16–26 ms —
  unchanged; command state builds per approval and costs nothing
  at startup.

## Phase 8 delta (measured, informational)

Same host and caveats as above. One new module (`aurel-tools/src/git.rs`)
plus five proposal ops, five inspection methods, one `/git` slash family,
and CLI approval labeling — no new crates.io graph entries, no async.

- Release binary: 3,352,064 bytes (≈3.20 MiB), i.e. +109,056 bytes
  (+3.4%) vs Phase 7, mostly new code paths rather than dependencies.
  Still far under the < 15 MB Core target.
- Startup, warm process start-to-exit: release `--version` — unchanged;
  Git state builds per inspection/approval and costs nothing at startup.
