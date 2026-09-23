# AUREL Core changelog

Notable changes per release. Version identity is single-sourced in the
workspace `Cargo.toml`; `aurel --version`, package metadata, and the
docs below all report the same number.

## 0.11.0 — Core stabilization release (Phase 11)

Release-hardening only: no new features, no architecture changes.

- Version identity audited: workspace metadata, `--version` output, and
  documentation agree on `0.11.0`.
- CLI and first-run experience reviewed: help/version/bare/error paths
  produce the documented output and exit codes (0/1/2) without panics,
  including from a clean environment with no config or session
  directories.
- Documentation drift fixed where found (proxy handling, cancellation,
  session-scoped settings, stale phase references); every behavior
  statement re-checked against the implementation.
- Focused regression coverage added for release-relevant seams
  (session directory resolution, session mode spellings, clock-skew age
  display).
- Full gate green: fmt, clippy, 300+ workspace tests (online and
  offline), release build, diff check.

What 0.11.0 is: a lightweight terminal-first coding agent — CLI plus
interactive REPL, Plan/Build modes, OpenAI-compatible provider, bounded
agent loop, read-only tools, approval-gated file/shell/local-Git
mutations, persistent sessions with cross-session `@explore`, `AGENTS.md`
instructions, workspace sandboxing, secret filtering, cancellation, and
bounded resource behavior throughout.

Resource profile (Windows 11 / Athlon 300U; see README for methodology):
startup ~20–40 ms warm, minimal-path peak ~5 MB, active-turn peak
~6.3 MB, 3.34 MiB release binary as the whole installed footprint.

Supported build: Rust 1.98.1 (pinned in `rust-toolchain.toml`), edition
2021; CI exercises Windows and Ubuntu. See “Known limitations” below
and the freeze statement in `docs/architecture.md`.

## 0.10.0 — Sessions (Phase 9) + hardening (Phase 10)

- Phase 9: `aurel-session` crate (stable IDs, bounded JSON store beside
  the global config), `/sessions`, `/resume`, real `@explore` retrieval,
  fresh IDs on `/new`, `/btw` isolation, compact persistence.
- Phase 10: bounds with regression tests (fence payloads, compaction
  transcripts, metadata-only listing, blurb clipping, stored history),
  disk-growth notice without auto-pruning, crash-litter cleanup, and
  measured resource results against the targets.

## 0.9.0 — Local Git mutations (Phase 8)

- `git_stage` / `git_unstage` / `git_commit` / `git_create_branch` /
  `git_switch_branch` behind the same approval flow; `/git`
  read-only inspection; remote verbs structurally unrepresentable.

## 0.8.0 — Shell execution (Phase 7)

- Direct-spawn `run_command` / `run_build` / `run_tests` plus `!`
  requests in the approval queue; secret-filtered child environments.

## 0.7.0 and earlier — incremental Core construction (Phases 0–6)

- Workspace foundation, CLI grammar, TOML configuration, provider
  abstraction and OpenAI-compatible implementation, agent loop,
  interactive REPL with modes, read-only tools, `AGENTS.md`, and the
  file-mutation approval workflow. Details per phase live in
  `docs/architecture.md` and `docs/decisions/`.

## Known limitations (0.11.0)

- A reachable OpenAI-compatible endpoint is required for `chat`,
  `agent`, and interactive prompts; everything else works offline.
- REPL idle RSS is an analytic bound (no PTY in test rigs to sample
  it); all sampled numbers are committed with methodology in README.
- Session files accumulate until the user deletes them (`/sessions`
  warns past 200 files; nothing is ever auto-pruned).
- `git log` caps at 50 entries; `/sessions` lists 100 rows;
  `@explore` loans at most 5 × 500 chars for one prompt.
- First-run cold start (~1 s) reflects OS process/disk-cache effects,
  not AUREL logic; steady state is milliseconds.
- No installer: the release artifact is the single binary.
- Out of scope by design: Normal edition, extra providers/routing,
  remote Git automation, Desktop/Voice, telemetry, paid services.
