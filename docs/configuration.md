# AUREL Configuration — Phase 2

TOML configuration with deterministic precedence. Only settings actually
needed at this stage exist; the schema grows in later phases.

## Settings

| Key         | Type   | Default | Sources                          |
| ----------- | ------ | ------- | -------------------------------- |
| `log_level` | string | `info`  | file, `AUREL_LOG_LEVEL`, `--log-level` |

Valid values (case-insensitive everywhere): `error`, `warn`, `info`,
`debug`, `trace`.

## Model settings (`[model]`)

| Key | Type | Default | Sources |
| --- | ---- | ------- | ------- |
| `name` | string | `default` | file, `AUREL_MODEL`, `--model` |
| `base_url` | string | `http://127.0.0.1:11434/v1` | file, `AUREL_BASE_URL`, `--base-url` |
| `api_key` | string (optional) | unset | file, `AUREL_API_KEY` (**no CLI flag**) |
| `timeout_secs` | integer | `60` | file, `AUREL_TIMEOUT_SECS` |
| `max_retries` | integer | `1` | file, `AUREL_MAX_RETRIES` |
| `streaming` | boolean | `true` | file, `AUREL_STREAMING`, `--streaming` |

Example file:

```toml
log_level = "debug"

[model]
name = "my-local-model"
base_url = "http://127.0.0.1:8080/v1"
timeout_secs = 120
```

There is deliberately no `--api-key` flag: argv leaks into shell history
and process listings. `config show` prints `api_key = "<redacted>"` (or
`"<unset>"`) and `Debug` formatting redacts it too — the real value is
never printed, logged, or embedded in errors.

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
- **Environment:** `AUREL_LOG_LEVEL` overrides both files. Collection uses
  `vars_os`, so non-Unicode environment data cannot panic the process:
  non-Unicode keys are ignored (recognized names are pure ASCII), values
  are lossy-converted and flow into normal validation (a bad value is exit
  1, never a panic).
- **CLI:** `--log-level <level>`, `--model`, `--base-url`, `--streaming`
  override everything in their area.
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
        --model <name>      Model id for `chat`
        --base-url <url>    Endpoint root for `chat`
        --streaming <bool>  true|false

COMMANDS:
    config show    Print the effective configuration
    config         Print the `config` command help (same as `config --help`)
    chat [MESSAGE] Send one message to the model (stdin if omitted)
```

Grammar notes: `--flag=value` and `--flag value` both work; only exact
`-h`/`-V` (no combined shorts, no abbreviations); `--help` is printed only
when the surrounding command line is valid — a usage error anywhere in the
line takes precedence and exits 2; `--version` combined with a command is a
usage error; a `--` token ends flag parsing (inside `chat`, `--` forces the
rest to be message words). A bare `aurel` prints help without reading
config files. `--help`, `--version`, bare `aurel`, and bare `config` never
construct runtime state, so they stay independent of broken config files or
environment data.

## `config show`

Prints `#` comment lines describing each file layer followed by the
effective settings as TOML. The project layer reports three honest states:
the discovered path, `searched, none found` (discovery ran empty), or
`not searched` (discovery replaced by `--config`):

```text
# aurel effective configuration (TOML)
# global: C:\Users\you\AppData\Roaming\aurel\config.toml (not found)
# project: C:\work\demo\.aurel/config.toml
log_level = "debug"

[model]
name = "my-local-model"
base_url = "http://127.0.0.1:8080/v1"
api_key = "<redacted>"
timeout_secs = 120
max_retries = 1
streaming = true
```

## Errors and exit codes

| Situation                                    | Exit | Output |
| -------------------------------------------- | ---- | ------ |
| unknown flag/command, bad CLI value, `--version` + command | 2 | stderr usage error + `aurel --help` tip |
| `chat` with no message and no pipe | 2 | stderr `no message given` |
| malformed TOML, unknown key, bad file value (with file path) | 1 | stderr, e.g. `error: invalid TOML in config file '…'` |
| bad `AUREL_*` value | 1 | stderr names the variable |
| missing `--config` file                      | 1 | stderr names the path |
| model/provider failure (unreachable, timeout, auth, malformed…) | 1 | stderr clean provider error, never secrets |
| `--help` / `--version` / `config show` / `chat` ok | 0 | stdout |

No color output. API keys never appear in logs, diagnostics, errors, or
`config show` — see `docs/model-providers.md` for the full secret policy.
