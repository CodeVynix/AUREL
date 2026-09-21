# ADR 0010 — Persistent sessions + cross-session `@explore`

## Decision

Phase 9 adds a new `aurel-session` crate (stable IDs, bounded JSON
store) and wires it into the interactive loop: every loop owns a session
ID, prompts/compactions/mode switches autosave, `/sessions` lists,
`/resume <id>` restores history/mode/counters with an empty approval
queue, and `@explore` prepends other sessions' summaries to one request
via `Agent::run_explore` without storing them.

## Alternatives

- SQLite (rusqlite) for session storage: rejected — a native C
  dependency and query machinery for what amounts to whole-file JSON
  documents (one per session, listed newest-first, loaded whole).
  Bounded flat files need no migrations, stay human-inspectable, and
  keep the dependency graph unchanged (`serde`/`serde_json` only, both
  already in-graph).
- Persisting pending proposals and undo records: rejected — proposals
  embed absolute-path snapshots and freshness state that cannot survive
  a process boundary meaningfully, and restoring an approval queue
  would let a resumed session execute with one stray `/approve`.
  Resume restores conversation, never executability.
- Storing `AGENTS.md` content in the session file: rejected — the file
  is user-owned and live; snapshotting it would silently freeze stale
  instructions. Every run and every resume reloads from the working
  directory.
- libgit2-style native Git or embeddings for retrieval: rejected —
  retrieval here is small-scale human context (≤5 blurbs × 500 chars),
  not semantic search. Summaries already exist via compaction; ranking
  is recency. A vector index would add dependencies and daemons for no
  measured need.
- Auto-migrating unknown format versions: rejected — forward migration
  guesses at semantics. Unknown versions fail as `Incompatible`, naming
  the file and both versions, leaving the live loop untouched.

## Reason

Sessions are the first AUREL state that outlives the process, so the
design isolates exactly what crosses the boundary: conversation text,
mode, and counters — each bounded, each validated on the way back in.
Everything executable (proposals, undo inverses) and everything secret
(keys, config) structurally cannot cross it: the file format has no
fields for them, which a dedicated test pins by scanning raw bytes.
`@explore` reuses the same isolation in miniature (private clone, copy
back only the exchange, debug-asserted), so cross-session context is a
per-request loan, never a second history.

## Consequences

- `Message`/`Role` gain serde derives with explicit lowercase spellings;
  the stored form is versioned separately (`format: 1`), so type
  evolution stays decoupled from file evolution.
- One-shot `aurel agent` stays non-persistent (no loop, no ID to resume
  under); persistence is a property of the interactive loop.
- Tests needing a store use isolated temp directories; tests needing
  `git`-style fixtures are unaffected. Session tests skip nothing —
  the store is pure std I/O plus JSON.
- Windows fixture lesson, repeated: test files must be closed before
  assertions that read them back (the session tests use
  `std::fs::write`, which closes, throughout).
