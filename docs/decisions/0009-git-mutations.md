# ADR 0009 — Git mutations (typed, local-only, approval-gated)

## Decision

Phase 8 adds Git operations as a dedicated layer in
`aurel-tools/src/git.rs`, reusing the Phase 6 proposal/approval state
machine and the Phase 7 direct-spawn runner with no new dependencies:

- Read-only inspection (`git_status`, `git_diff`, `git_branches`,
  `git_log`, `git_toplevel`) holds the read-only permission tier —
  allowed in Plan and Build, surfaced via `/git`, executed through the
  command sandbox.
- Five mutation ops (`git_stage`, `git_unstage`, `git_commit`,
  `git_create_branch`, `git_switch_branch`) prepare (detect repo,
  validate, sandbox-resolve paths, snapshot HEAD + branch + porcelain
  status, render review), verify fresh at approval, execute the exact
  shown argv, and record a non-undoable audit entry.
- Only local subcommands have constructors (`add`, `reset`, `commit`,
  `branch`, `switch`, plus read-only inspection verbs). Push, pull,
  fetch, merge, rebase, remote, and clone have no constructor, a runtime
  refusal guard in the apply path, and a regression test pinning the
  argv builder.

## Alternatives

- Generic `!git <anything>` as the Git interface: rejected — raw text
  cannot be validated or snapshotted, and review would depend on the
  user spotting a smuggled `push`. Typed ops make the forbidden set
  unrepresentable instead of merely visible. Explicit `!git ...` lines
  still queue as ordinary shell proposals (the user's own text, fully
  shown), but AUREL never proposes remote/history-rewriting Git itself.
- Undo via reverse operations (unstage a stage, reset a commit,
  delete a branch): rejected — a commit reset rewrites history and a
  branch delete destroys work; both contradict the approval-flow promise
  that undo only reverses what is safe. Applied Git ops record
  `GitExecuted` and `/undo` refuses honestly, exactly like shell effects.
- libgit2 bindings (git2 crate): rejected — a C dependency (libgit2)
  would break the dependency-minimal, MSVC-friendly build story for
  behavior the `git` binary already implements authoritatively
  (ref-format rules, status semantics, switch safety checks). Shelling
  out keeps one Git implementation: the user's own.
- Merge/rebase/remote automation: rejected out of scope — Phase 8 is
  local version control only. Combining trees and touching remotes need
  their own design (and their own phase), not a flag on this one.

## Reason

Git operations are the first mutations whose blast radius extends beyond
the workspace files (branch pointers, the index, commit graph), so they
get the strongest form of the existing controls: typed argv
construction (forbidden verbs have no code path), whole-tree freshness
snapshots (any external commit or status change fails closed as stale),
precondition checks at prepare time (staged work exists, HEAD is
attached, branch names valid and in the expected existence state), and
Git's own output surfaced verbatim on every path. Plan mode blocks
approvals through the unchanged generic gate; Build proposes but never
auto-executes through the unchanged queue.

## Consequences

- `aurel-tools` gains its first subprocess-using inspection methods;
  they share the command layer's bounds, cancellation, and secret
  filtering, so no new security surface is introduced.
- Tests that exercise mutations require a `git` binary and skip cleanly
  without one; repo-local config keeps fixtures hermetic (no global
  gitconfig writes).
- Windows fixture lesson (pinned in test comments): files must be
  closed before `git add` runs — an open writer can stage a
  zero-length blob because the directory-entry size lags the handle.
