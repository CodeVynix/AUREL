# ADR 0005 — TOML configuration via `toml` + `serde`

## Decision

Phase 1 parses TOML with the `toml` crate plus `serde`/`serde_derive`.
No hand-rolled TOML subset parser. Schema deserializes with
`deny_unknown_fields` so typos fail loudly.

## Alternatives

- Hand-rolled subset parser, zero deps (rejected: a partial TOML reader
  that claims TOML compatibility is a correctness liability; malformed-input
  behavior could never match the real format).
- `toml_edit` only (rejected: lower-level API for no benefit at this
  schema size).

## Reason

Configuration is a genuine architectural boundary shared by Core and Normal
(`aurel-config` crate), and TOML is the specified format. A real parser
gives correct malformed-input errors with line/column context for free.
`serde` is the standard Rust serialization framework and will serve every
future phase (model settings, sessions); paying its cost once here is
cheaper than migrating later.

## Resource impact

Measured on Windows 11 / AMD Athlon 300U / Rust 1.98.1, release profile,
against the Phase 0 baseline (140,288 bytes / 137.0 KiB binary,
≈21–31 ms startup):

- release binary size: 492,544 bytes (≈481 KiB), i.e. +352,256 bytes for
  the TOML stack. Still far under the < 15 MB Core target. A parse-only
  feature selection (`toml` without `display`, which `config show` does not
  need since it renders one fixed-domain field by hand) was measured and
  kept: 500,224 → 492,544 bytes.
- release startup: `--version` warm ≈19–68 ms, `--help` warm ≈16–76 ms —
  same ballpark as the Phase 0 means (30.8 ms / 21.19 ms); no meaningful
  startup regression. First cold spawn after rebuild ≈415 ms (Windows
  process creation + cold disk cache, as in Phase 0).
- dependency graph delta (`cargo tree -e normal`, new third-party crates):
  `serde`, `serde_core`, `serde_derive` (proc-macro) + `proc-macro2`,
  `quote`, `syn`, `unicode-ident`, and `toml` 0.8 parse-only +
  `serde_spanned`, `toml_datetime`, `toml_edit`, `indexmap`, `equivalent`,
  `hashbrown`, `winnow`. Parser itself (`args.rs`) added zero dependencies.

## Long-term implication

`serde` Deserialize/Serialize becomes the standard for AUREL config-shaped
data. If a future measurement shows the TOML stack dominating Core's
footprint, revisit (e.g. `default-features = false`, format change) with a
new ADR — never silently.
