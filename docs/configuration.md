# AUREL Configuration — Phase 1

TOML configuration with deterministic precedence. Only settings actually
needed at this stage exist; the schema grows in later phases.

## Settings

| Key         | Type   | Default | Sources                          |
| ----------- | ------ | ------- | -------------------------------- |
| `log_level` | string | `info`  | file, `AUREL_LOG_LEVEL`, `--log-level` |

Valid values (case-insensitive everywhere): `error`, `warn`, `info`,
`debug`, `trace`.

Example file:

```toml
log_level = "debug"
```

Unknown keys are rejected (`deny_unknown_fields`) so typos fail loudly.

## Precedence

```text
built-in defaults
    ↓
global config file
    ↓
project config file
    ↓
environment variables (AUREL_*)
    ↓
CLI arguments
```

- **Global file:** `%APPDATA%\aurel\config.toml` on Windows,
  `~/.config/aurel/config.toml` on Linux/WSL. Resolved from `%APPDATA%`
  / `$HOME`; if the variable is absent the layer is skipped.
- **Project file:** nearest `.aurel/config.toml` at or above the working
  directory (bounded 64-level walk, stops at the filesystem root).
- **Environment:** `AUREL_LOG_LEVEL` overrides both files.
- **CLI:** `--log-level <level>` overrides everything.
- Missing files are skipped silently. `--config <path>` replaces file
  discovery: only that file is read (plus env and CLI above it); a missing
  `--config` file is an error.

## CLI

```text
aurel [OPTIONS] [COMMAND]

OPTIONS:
    -h, --help              Print help (never reads config files)
    -V, --version           Print version (never reads config files)
        --config <path>     Use this file instead of discovered files
        --log-level <level> error|warn|info|debug|trace

COMMANDS:
    config show    Print the effective configuration
```

Grammar notes: `--flag=value` and `--flag value` both work; only exact
`-h`/`-V` (no combined shorts, no abbreviations); `--help` wins over
everything else; `--version` combined with a command is a usage error; a
`--` token ends flag parsing. A bare `aurel` prints help without reading
config files.

## `config show`

Prints `#` comment lines describing each file layer (path, `not found`,
or `not searched`) followed by the effective settings as TOML:

```text
# aurel effective configuration (TOML)
# global: C:\Users\you\AppData\Roaming\aurel\config.toml (not found)
# project: C:\work\demo\.aurel/config.toml
log_level = "debug"
```

## Errors and exit codes

| Situation                                    | Exit | Output |
| -------------------------------------------- | ---- | ------ |
| unknown flag/command, bad CLI value, `--version` + command | 2 | stderr usage error + `aurel --help` tip |
| malformed TOML, unknown key, bad file value (with file path) | 1 | stderr, e.g. `error: invalid TOML in config file '…'` |
| bad `AUREL_LOG_LEVEL`                        | 1    | stderr names the variable |
| missing `--config` file                      | 1    | stderr names the path |
| `--help` / `--version` / `config show` ok    | 0    | stdout |

No color output. Secret-shaped values are never needed at this stage;
when credentials arrive (Phase 2), they must never be logged.
