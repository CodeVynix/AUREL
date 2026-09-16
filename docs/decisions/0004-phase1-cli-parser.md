# ADR 0004 — Extend std-only CLI parsing for Phase 1

## Decision

Phase 1 extends the hand-rolled `std` argument parser (new
`crates/aurel-cli/src/args.rs`) instead of adding `lexopt`, `clap`, or any
other CLI framework.

## Alternatives

- `lexopt` now (tiny, supports flags/subcommands, would serve well).
- `clap` now (familiar, heaviest option by far).
- Defer subcommands entirely (rejected: `config show` is the only practical
  way to make precedence visible and debuggable in Phase 1).

## Reason

The Phase 1 surface is small and fixed: four global flags
(`--help`/`-h`, `--version`/`-V`, `--config <path>`, `--log-level <level>`,
both with `--flag=value` form), one command (`config`), one subcommand
(`show`). A total parser for this surface is roughly 150 lines including a
deterministic grammar documented in `docs/configuration.md`:

- global-flag zone, then at most one command word, then command args
- `--help` wins everywhere (exit 0, config files untouched)
- `--version` with anything but global flags is a usage error (exit 2)
- unknown flags, missing values, bad values, unknown commands are exit 2
- no combined short flags, no abbreviations, no shell completion

`lexopt` would only tokenize; all grammar, help text, and exit-code policy
would still be hand-written. It therefore saves almost no code while adding
an external dependency, audit surface, and build cost against the Core
lightness targets.

## Resource impact

Zero new dependencies for parsing. Parser cost is bounded source size, not
runtime: no startup or memory impact beyond a few string comparisons.

## Long-term implication

Revisit when the flag surface grows (Phase 6+ shell/build commands are the
likely trigger). A framework migration gets its own ADR with measured
before/after binary size and startup numbers.
