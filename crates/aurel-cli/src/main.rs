//! `aurel` CLI entry point.
//!
//! Phase 1: dependency-free argument handling ([`args`]) over
//! `std::env::args_os` with an explicit lossy Unicode policy (see
//! [`normalize_args`]), TOML configuration with documented precedence
//! (`aurel-config`), and a `config show` command.
//!
//! Conceptual flow:
//!
//! ```text
//! collect argv (args_os, never panics on non-Unicode)
//!     ↓
//! parse CLI
//!     ↓
//! --help → print help and exit (config/environment untouched)
//! --version → print version and exit (config/environment untouched)
//! bare / no command → print help and exit (config/environment untouched)
//! otherwise
//!     ↓
//! construct live Runtime (vars_os + lossy env policy, no panic)
//!     ↓
//! load configuration
//!     ↓
//! execute command
//! ```
//!
//! All branching lives in [`run_inner`], which takes an optional injectable
//! [`Runtime`] ([`run_with`]) so precedence is testable without touching the
//! real home directory or process environment. Integration tests in `tests/`
//! exercise the compiled binary end to end.

mod args;
mod interactive;

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use args::{Command, HelpTopic};
use aurel_config::{
    collect_aurel_env, discover_project_file, global_config_file, load, render_show, LoadRequest,
};
use aurel_model::{
    Agent, AgentConfig, AgentOutcome, AgentSession, CancelFlag, ChatRequest, ModelProvider,
    OpenAiCompatible, OpenAiConfig, StreamControl, StreamEvent,
};

/// Exit code for CLI usage errors (unknown flags, bad commands/values).
const EXIT_USAGE_ERROR: i32 = 2;
/// Exit code for configuration and other runtime errors.
const EXIT_RUNTIME_ERROR: i32 = 1;

/// Policy for non-Unicode OS arguments: convert with `to_string_lossy`
/// (unrepresentable sequences become U+FFFD) and continue through the normal
/// parser. This never panics: `args_os` does not validate Unicode and
/// `to_string_lossy` is total. A lossy argument simply fails to match a known
/// flag or command and is handled as a deterministic usage error (exit 2).
fn normalize_args(raw: &[OsString]) -> Vec<String> {
    raw.iter()
        .map(|s| s.to_string_lossy().into_owned())
        .collect()
}

/// Process state [`run`] needs but tests must fake: working directory,
/// relevant directory variables, and the `AUREL_*` environment.
struct Runtime {
    cwd: PathBuf,
    appdata: Option<String>,
    home: Option<String>,
    env: HashMap<String, String>,
}

impl Runtime {
    /// Capture the real process state without panicking on non-Unicode data.
    ///
    /// Environment collection uses `vars_os` plus [`collect_aurel_env`]:
    /// non-Unicode keys are ignored (recognized names are pure ASCII, so an
    /// undecodable key cannot name a real setting) and values are
    /// lossy-converted, flowing into normal validation. Directory lookups
    /// use the non-panicking `var(...).ok()` form.
    fn live() -> Self {
        Runtime {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            appdata: std::env::var("APPDATA").ok(),
            home: std::env::var("HOME").ok(),
            env: collect_aurel_env(std::env::vars_os()),
        }
    }
}

/// Render the top-level help text to `out`.
fn print_help(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())?;
    writeln!(out, "Autonomous Utility & Reasoning Engine for Logic")?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "    aurel [OPTIONS] [COMMAND]")?;
    writeln!(out)?;
    writeln!(out, "OPTIONS:")?;
    writeln!(out, "    -h, --help              Print this help message")?;
    writeln!(out, "    -V, --version           Print version information")?;
    writeln!(
        out,
        "        --config <path>     Use this config file instead of"
    )?;
    writeln!(
        out,
        "                            discovered global/project files"
    )?;
    writeln!(
        out,
        "        --log-level <level> error|warn|info|debug|trace"
    )?;
    writeln!(
        out,
        "        --model <name>      Model id for `chat` / `agent`"
    )?;
    writeln!(
        out,
        "        --base-url <url>    Endpoint root for `chat` / `agent`"
    )?;
    writeln!(out, "        --streaming <bool>  true|false (default true)")?;
    writeln!(
        out,
        "        --max-iterations <n> 1-100 agent loop bound for `agent`"
    )?;
    writeln!(out)?;
    writeln!(out, "COMMANDS:")?;
    writeln!(out, "    config show    Print the effective configuration")?;
    writeln!(
        out,
        "    chat [MESSAGE] Send one message to the model and print"
    )?;
    writeln!(
        out,
        "                   the reply (reads piped stdin if omitted)"
    )?;
    writeln!(
        out,
        "    agent [MESSAGE] Run one bounded agent turn sequence"
    )?;
    writeln!(
        out,
        "                   and print the reply (stdin if omitted)"
    )?;
    writeln!(out)?;
    writeln!(out, "CONFIG FILES (TOML):")?;
    writeln!(out, "    global:  %APPDATA%\\aurel\\config.toml (Windows)")?;
    writeln!(out, "             ~/.config/aurel/config.toml (Linux/WSL)")?;
    writeln!(
        out,
        "    project: nearest .aurel/config.toml at or above the"
    )?;
    writeln!(out, "             working directory")?;
    writeln!(
        out,
        "Precedence: defaults < global < project < env (AUREL_*) < CLI."
    )?;
    Ok(())
}

/// Render the `config` command help text to `out`.
fn print_config_help(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "    aurel config show")?;
    writeln!(out)?;
    writeln!(
        out,
        "Prints the effective TOML configuration after applying"
    )?;
    writeln!(
        out,
        "precedence: defaults < global < project < env (AUREL_*) < CLI."
    )?;
    Ok(())
}

/// Render the version line to `out`.
fn print_version(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())
}

/// Render the `chat` command help text to `out`.
fn print_chat_help(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "    aurel [OPTIONS] chat [MESSAGE]...")?;
    writeln!(out)?;
    writeln!(
        out,
        "Send one message to the configured model and print the reply."
    )?;
    writeln!(
        out,
        "With no MESSAGE words, the message is read from piped stdin;"
    )?;
    writeln!(out, "a terminal with no message is a usage error.")?;
    writeln!(out)?;
    writeln!(
        out,
        "This is a single request/response exchange, not an agent:"
    )?;
    writeln!(
        out,
        "it never inspects repositories, edits files, or runs commands."
    )?;
    writeln!(out)?;
    writeln!(
        out,
        "Configure via [model] settings, AUREL_* env, or --model /"
    )?;
    writeln!(
        out,
        "--base-url / --streaming. There is no --api-key flag: use"
    )?;
    writeln!(out, "a config file or AUREL_API_KEY.")?;
    Ok(())
}

/// Render the `agent` command help text to `out`.
fn print_agent_help(out: &mut dyn Write) -> std::io::Result<()> {
    writeln!(out, "aurel {}", aurel_core::version())?;
    writeln!(out)?;
    writeln!(out, "USAGE:")?;
    writeln!(out, "    aurel [OPTIONS] agent [MESSAGE]...")?;
    writeln!(out)?;
    writeln!(
        out,
        "Run one bounded agent turn sequence and print the reply."
    )?;
    writeln!(
        out,
        "With no MESSAGE words, the message is read from piped stdin;"
    )?;
    writeln!(out, "a terminal with no message is a usage error.")?;
    writeln!(out)?;
    writeln!(
        out,
        "The loop requests continuations for truncated turns, up to"
    )?;
    writeln!(
        out,
        "--max-iterations (default 5, hard cap 100). There are no tools"
    )?;
    writeln!(out, "yet: the agent cannot inspect, edit, or run anything.")?;
    writeln!(out)?;
    writeln!(
        out,
        "Configure via [model]/[agent] settings, AUREL_* env, or --model /"
    )?;
    writeln!(
        out,
        "--base-url / --streaming / --max-iterations. There is no --api-key"
    )?;
    writeln!(out, "flag: use a config file or AUREL_API_KEY.")?;
    Ok(())
}

/// Build the config [`LoadRequest`] for a parsed command line. An explicit
/// `--config` path replaces global/project discovery (`project_searched` is
/// then false, rendering "not searched"); otherwise both are discovered
/// from the runtime (`project_searched` true, rendering the found path or
/// "searched, none found").
fn build_request(parsed: &args::Parsed, rt: &Runtime) -> LoadRequest {
    let (global_file, project_file, project_searched, explicit_file) = match &parsed.config_path {
        Some(path) => (None, None, false, Some(path.clone())),
        None => (
            global_config_file(rt.appdata.as_deref(), rt.home.as_deref()),
            discover_project_file(&rt.cwd),
            true,
            None,
        ),
    };
    LoadRequest {
        global_file,
        project_file,
        project_searched,
        explicit_file,
        env: rt.env.clone(),
        cli_log_level: parsed.log_level,
        cli_model_name: parsed.model_name.clone(),
        cli_base_url: parsed.base_url.clone(),
        cli_streaming: parsed.streaming,
        cli_max_iterations: parsed.max_iterations,
    }
}

/// Core CLI logic, testable without process spawning.
///
/// `--help`/`--version`/bare invocations short-circuit before any runtime
/// state (environment, config files) is touched, so they stay independent
/// of configuration/environment failures. `stdin` is only consumed on the
/// `chat` path without message words. Returns a process exit code:
/// `0` on success, `2` on usage error, `1` on configuration/runtime failure
/// or output failure.
fn run(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    stdin: &mut dyn Read,
    stdin_is_terminal: bool,
) -> i32 {
    run_inner(args, out, err, None, stdin, stdin_is_terminal)
}

/// [`run`] with an injected [`Runtime`] for tests.
#[cfg(test)]
fn run_with(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    rt: &Runtime,
    stdin: &mut dyn Read,
    stdin_is_terminal: bool,
) -> i32 {
    run_inner(args, out, err, Some(rt), stdin, stdin_is_terminal)
}

fn run_inner(
    args: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
    rt: Option<&Runtime>,
    stdin: &mut dyn Read,
    stdin_is_terminal: bool,
) -> i32 {
    let parsed = match args::parse(args) {
        Ok(parsed) => parsed,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            let _ = writeln!(err, "tip: run 'aurel --help' for usage.");
            return EXIT_USAGE_ERROR;
        }
    };

    if let Some(topic) = parsed.help {
        let ok = match topic {
            HelpTopic::Top => print_help(out),
            HelpTopic::Config => print_config_help(out),
            HelpTopic::Chat => print_chat_help(out),
            HelpTopic::Agent => print_agent_help(out),
        };
        return if ok.is_ok() { 0 } else { 1 };
    }
    if parsed.version {
        return if print_version(out).is_ok() { 0 } else { 1 };
    }

    match parsed.command.as_ref() {
        // Bare invocation: an interactive terminal enters the REPL; piped
        // input keeps the Phase 0 help behavior (the injected-runtime test
        // path also prints help deterministically).
        None => match rt {
            Some(_) => {
                if print_help(out).is_ok() {
                    0
                } else {
                    1
                }
            }
            None if !stdin_is_terminal => {
                if print_help(out).is_ok() {
                    0
                } else {
                    1
                }
            }
            None => {
                let live = Runtime::live();
                let mut input = std::io::BufReader::new(stdin);
                interactive::start_interactive(&live, &mut input, out, err)
            }
        },
        // Command paths: only here is the live runtime constructed.
        Some(Command::ConfigShow) => match rt {
            Some(injected) => execute_config_show(&parsed, out, err, injected),
            None => {
                let live = Runtime::live();
                execute_config_show(&parsed, out, err, &live)
            }
        },
        Some(Command::Chat { message }) => {
            let text = match resolve_message(message, stdin, stdin_is_terminal) {
                Ok(text) => text,
                Err(e) => {
                    let _ = writeln!(err, "{e}");
                    let _ = writeln!(err, "tip: run 'aurel chat --help' for usage.");
                    return EXIT_USAGE_ERROR;
                }
            };
            match rt {
                Some(injected) => execute_chat(&parsed, &text, out, err, injected),
                None => {
                    let live = Runtime::live();
                    execute_chat(&parsed, &text, out, err, &live)
                }
            }
        }
        Some(Command::Agent { message }) => {
            let text = match resolve_message(message, stdin, stdin_is_terminal) {
                Ok(text) => text,
                Err(e) => {
                    let _ = writeln!(err, "{e}");
                    let _ = writeln!(err, "tip: run 'aurel agent --help' for usage.");
                    return EXIT_USAGE_ERROR;
                }
            };
            match rt {
                Some(injected) => execute_agent(&parsed, &text, out, err, injected),
                None => {
                    let live = Runtime::live();
                    execute_agent(&parsed, &text, out, err, &live)
                }
            }
        }
    }
}

fn execute_config_show(
    parsed: &args::Parsed,
    out: &mut dyn Write,
    err: &mut dyn Write,
    rt: &Runtime,
) -> i32 {
    match load(&build_request(parsed, rt)) {
        Ok(cfg) => {
            if writeln!(out, "{}", render_show(&cfg)).is_ok() {
                0
            } else {
                1
            }
        }
        Err(e) => {
            let _ = writeln!(err, "{e}");
            EXIT_RUNTIME_ERROR
        }
    }
}

/// Why no chat message is available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageError {
    /// Nothing on argv and (terminal with no pipe, or empty pipe).
    NoMessage,
    /// Piped stdin could not be read at all.
    UnreadableStdin,
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageError::NoMessage => write!(
                f,
                "error: no message given (pass MESSAGE arguments or pipe one via stdin)"
            ),
            MessageError::UnreadableStdin => write!(f, "error: could not read message from stdin"),
        }
    }
}

/// Resolve the chat message: argv words joined with spaces, else piped
/// stdin (trimmed). A terminal with no words is a usage error rather than
/// a blocking read. Pure over injected stdin, so unit-testable.
fn resolve_message(
    words: &[String],
    stdin: &mut dyn Read,
    stdin_is_terminal: bool,
) -> Result<String, MessageError> {
    if !words.is_empty() {
        let text = words.join(" ");
        return if text.trim().is_empty() {
            Err(MessageError::NoMessage)
        } else {
            Ok(text)
        };
    }
    if stdin_is_terminal {
        return Err(MessageError::NoMessage);
    }
    let mut buf = String::new();
    stdin
        .read_to_string(&mut buf)
        .map_err(|_| MessageError::UnreadableStdin)?;
    let text = buf.trim().to_string();
    if text.is_empty() {
        Err(MessageError::NoMessage)
    } else {
        Ok(text)
    }
}

/// Build the provider from resolved `[model]` settings. Validation failures
/// (empty model, bad URL, unbounded timeout) surface as clean exit-1 errors.
fn build_provider(
    cfg: &aurel_config::EffectiveConfig,
) -> Result<OpenAiCompatible, aurel_model::ProviderError> {
    OpenAiCompatible::new(OpenAiConfig {
        base_url: cfg.model.base_url.clone(),
        model: cfg.model.name.clone(),
        api_key: cfg.model.api_key.clone(),
        timeout: Duration::from_secs(cfg.model.timeout_secs),
        max_retries: cfg.model.max_retries,
    })
}

/// Progressive stream sink shared by `chat` and `agent`: prints deltas as
/// they arrive, and cancels the turn on the first output failure (without a
/// working sink the rest of the stream is worthless, and continuing would
/// only waste time and risk duplicate output on any retry).
struct StreamSink<'a> {
    out: &'a mut dyn Write,
    failed: bool,
}

impl<'a> StreamSink<'a> {
    fn new(out: &'a mut dyn Write) -> Self {
        StreamSink { out, failed: false }
    }

    fn on_event(&mut self, event: StreamEvent) -> StreamControl {
        if !self.failed
            && (write!(self.out, "{}", event.delta).is_err() || self.out.flush().is_err())
        {
            self.failed = true;
            return StreamControl::Cancel;
        }
        StreamControl::Continue
    }
}

/// One request/response exchange against any provider. Streaming deltas
/// print progressively; the full reply (or the error) determines the exit
/// code. Provider errors print their own clean messages — never secrets.
fn chat_with_provider(
    message: &str,
    streaming: bool,
    provider: &dyn ModelProvider,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let request = ChatRequest::one_shot(message, streaming);
    if streaming {
        let mut sink = StreamSink::new(out);
        let result = provider.chat_stream(&request, &mut |event| sink.on_event(event));
        match result {
            Ok(_) => {
                let _ = writeln!(sink.out);
                if sink.failed {
                    1
                } else {
                    0
                }
            }
            Err(e) => {
                // A write failure cancels the stream on purpose (see above):
                // report the real cause, not a misleading cancellation.
                if sink.failed {
                    let _ = writeln!(err, "error: failed to write model output");
                } else {
                    let _ = writeln!(err, "{e}");
                }
                EXIT_RUNTIME_ERROR
            }
        }
    } else {
        match provider.chat(&request) {
            Ok(response) => {
                if writeln!(out, "{}", response.content).is_ok() {
                    0
                } else {
                    1
                }
            }
            Err(e) => {
                let _ = writeln!(err, "{e}");
                EXIT_RUNTIME_ERROR
            }
        }
    }
}

/// One bounded agent run: resolve config, build the provider, run the loop
/// with a fresh in-memory session, and print the outcome.
///
/// Exit codes: 0 for a completed run; 1 for iteration-limit, cancellation,
/// provider/config failures, and output failures. The limit is informative
/// but nonzero so scripts do not mistake partial output for completion.
fn execute_agent(
    parsed: &args::Parsed,
    message: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
    rt: &Runtime,
) -> i32 {
    let cfg = match load(&build_request(parsed, rt)) {
        Ok(cfg) => cfg,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    let provider = match build_provider(&cfg) {
        Ok(provider) => provider,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    let agent = match Agent::new(
        provider,
        AgentConfig {
            max_iterations: cfg.agent.max_iterations,
            streaming: cfg.model.streaming,
        },
    ) {
        Ok(agent) => agent,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    let mut session = AgentSession::new();
    load_session_instructions(&mut session, &rt.cwd, err);
    let (code, outcome) = run_agent(&agent, &mut session, message, out, err);
    // One-shot runs cannot approve anything: surface completed-turn
    // proposals as a reviewable diff and exit nonzero so scripts do not
    // mistake the situation for a clean completion.
    if matches!(outcome, AgentOutcome::Completed(_)) {
        let context = match aurel_tools::ToolContext::new(&rt.cwd) {
            Ok(context) => context,
            Err(error) => {
                let _ = writeln!(err, "warning: cannot prepare proposals: {error}");
                return code;
            }
        };
        let mut next_id = 1u64;
        let mut shown = 0usize;
        for proposal in interactive::review_proposals(
            &context,
            &mut next_id,
            &outcome.result().content,
            out,
            err,
        ) {
            shown += 1;
            let _ = writeln!(out, "Proposal #{}: {}", proposal.id, proposal.op.summary());
            let _ = write!(out, "{}", proposal.diff);
        }
        if shown > 0 {
            let _ = writeln!(
                out,
                "Re-run interactively to review (/diff), then /approve or /deny."
            );
            return EXIT_RUNTIME_ERROR;
        }
    }
    code
}

/// Load the nearest `AGENTS.md` above `cwd` into the session as project
/// instructions (kept out of conversation history). Absent files clear any
/// value; load failures warn and continue bare — instructions must never
/// fail a run on their own.
fn load_session_instructions(session: &mut AgentSession, cwd: &Path, err: &mut dyn Write) {
    match aurel_tools::load_instructions_for_dir(cwd) {
        Ok(found) => session.set_instructions(found.map(|loaded| loaded.content)),
        Err(error) => {
            session.set_instructions(None);
            let _ = writeln!(
                err,
                "warning: could not load {}: {error} (continuing)",
                aurel_tools::AGENTS_MD
            );
        }
    }
}

/// Drive one agent run against any provider and print the outcome.
/// Split from [`execute_agent`] so streaming output failures are testable
/// without network access.
///
/// A failed stdout write cancels the turn on purpose (see [`StreamSink`]):
/// such outcomes report the write failure, matching the Phase 2 `chat`
/// behavior, instead of a misleading cancellation or provider error.
fn run_agent<P: ModelProvider>(
    agent: &Agent<P>,
    session: &mut AgentSession,
    message: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> (i32, AgentOutcome) {
    let streaming = agent.config().streaming;
    let cancel = CancelFlag::new();
    let mut sink = StreamSink::new(out);
    let outcome = agent.run(session, message, Some(&cancel), &mut |event| {
        sink.on_event(event)
    });
    let failed = sink.failed;
    let out = sink.out;
    let code = print_agent_outcome(&outcome, streaming, failed, out, err);
    (code, outcome)
}

/// Print an [`AgentOutcome`] produced by [`run_agent`] or the interactive
/// loop: progressive content is already on stdout in streaming mode, whole
/// content prints here otherwise. Returns the exit code (0 completed, 1
/// otherwise); the interactive loop prints but ignores it and continues.
fn print_agent_outcome(
    outcome: &AgentOutcome,
    streaming: bool,
    write_failed: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match outcome {
        AgentOutcome::Completed(result) => {
            if streaming {
                let _ = writeln!(out);
            } else if writeln!(out, "{}", result.content).is_err() {
                return 1;
            }
            if write_failed {
                1
            } else {
                0
            }
        }
        AgentOutcome::IterationLimitReached(result) => {
            if !streaming {
                let _ = writeln!(out, "{}", result.content);
            } else {
                let _ = writeln!(out);
            }
            let _ = writeln!(
                err,
                "warning: iteration limit reached ({}) — output may be incomplete",
                result.iterations
            );
            EXIT_RUNTIME_ERROR
        }
        AgentOutcome::Cancelled(_) => {
            if write_failed {
                let _ = writeln!(err, "error: failed to write model output");
            } else {
                let _ = writeln!(err, "error: agent run cancelled");
            }
            EXIT_RUNTIME_ERROR
        }
        AgentOutcome::ProviderError { error, .. } => {
            if write_failed {
                let _ = writeln!(err, "error: failed to write model output");
            } else {
                let _ = writeln!(err, "{error}");
            }
            EXIT_RUNTIME_ERROR
        }
    }
}

fn execute_chat(
    parsed: &args::Parsed,
    message: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
    rt: &Runtime,
) -> i32 {
    let cfg = match load(&build_request(parsed, rt)) {
        Ok(cfg) => cfg,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    let provider = match build_provider(&cfg) {
        Ok(provider) => provider,
        Err(e) => {
            let _ = writeln!(err, "{e}");
            return EXIT_RUNTIME_ERROR;
        }
    };
    chat_with_provider(message, cfg.model.streaming, &provider, out, err)
}

fn main() {
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let args = normalize_args(&raw);
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let stdin = std::io::stdin();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let mut input = stdin.lock();
    // Note: the live Runtime is constructed lazily inside `run`, only on the
    // command path — help/version/bare never touch environment or config.
    let code = run(&args, &mut out, &mut err, &mut input, stdin.is_terminal());
    // Ensure buffered output is flushed before exiting with a code.
    let _ = out.flush();
    let _ = err.flush();
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_runtime() -> Runtime {
        Runtime {
            cwd: std::env::temp_dir().join(format!("aurel-cli-test-{}-noroot", std::process::id())),
            appdata: None,
            home: None,
            env: HashMap::new(),
        }
    }

    fn run_to_string(args: &[&str], rt: &Runtime) -> (i32, String, String) {
        run_to_string_with_stdin(args, rt, &mut &b""[..], false)
    }

    fn run_to_string_with_stdin(
        args: &[&str],
        rt: &Runtime,
        stdin: &mut dyn Read,
        stdin_is_terminal: bool,
    ) -> (i32, String, String) {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with(&owned, &mut out, &mut err, rt, stdin, stdin_is_terminal);
        (
            code,
            String::from_utf8(out).expect("stdout must be UTF-8"),
            String::from_utf8(err).expect("stderr must be UTF-8"),
        )
    }

    #[test]
    fn no_args_prints_help_to_stdout() {
        let rt = test_runtime();
        let (code, out, err) = run_to_string(&[], &rt);
        assert_eq!(code, 0);
        assert!(out.contains("USAGE:"), "help must contain USAGE");
        assert!(err.is_empty());
    }

    #[test]
    fn help_flag_prints_help() {
        let rt = test_runtime();
        for flag in ["--help", "-h"] {
            let (code, out, err) = run_to_string(&[flag], &rt);
            assert_eq!(code, 0, "flag {flag}");
            assert!(out.contains("USAGE:"), "flag {flag}");
            assert!(err.is_empty());
        }
    }

    #[test]
    fn version_flag_prints_version() {
        let rt = test_runtime();
        for flag in ["--version", "-V"] {
            let (code, out, err) = run_to_string(&[flag], &rt);
            assert_eq!(code, 0, "flag {flag}");
            assert!(
                out.contains(&format!("aurel {}", aurel_core::version())),
                "flag {flag}"
            );
            assert!(err.is_empty());
        }
    }

    #[test]
    fn unknown_argument_returns_exit_code_2() {
        let rt = test_runtime();
        let (code, _out, err) = run_to_string(&["--wat"], &rt);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("--wat"));
    }

    #[test]
    fn unknown_command_returns_exit_code_2() {
        let rt = test_runtime();
        let (code, _out, err) = run_to_string(&["frobnicate"], &rt);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("frobnicate"));
    }

    #[test]
    fn version_with_command_returns_exit_code_2() {
        let rt = test_runtime();
        let (code, _out, _err) = run_to_string(&["--version", "config", "show"], &rt);
        assert_eq!(code, EXIT_USAGE_ERROR);
    }

    #[test]
    fn config_show_prints_effective_config() {
        let dir = std::env::temp_dir().join(format!("aurel-cli-test-{}-show", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = \"debug\"\n").expect("test config");
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env: HashMap::new(),
        };
        let (code, out, err) = run_to_string(&["config", "show"], &rt);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("log_level = \"debug\""), "got: {out:?}");
        assert!(out.contains(&dir.display().to_string()), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cli_log_level_beats_file_in_config_show() {
        let dir =
            std::env::temp_dir().join(format!("aurel-cli-test-{}-override", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = \"error\"\n").expect("test config");
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env: HashMap::new(),
        };
        let (code, out, err) = run_to_string(&["--log-level", "trace", "config", "show"], &rt);
        assert_eq!(code, 0, "stderr: {err}");
        assert!(out.contains("log_level = \"trace\""), "got: {out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_config_reports_exit_code_1() {
        let dir = std::env::temp_dir().join(format!("aurel-cli-test-{}-bad", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = [oops\n").expect("test config");
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env: HashMap::new(),
        };
        let (code, _out, err) = run_to_string(&["config", "show"], &rt);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        assert!(err.contains("invalid TOML"), "got: {err:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn help_version_and_bare_ignore_broken_runtime_state() {
        // Help/version/bare paths never touch environment or config files,
        // so they succeed even when the runtime state would fail loading.
        let dir =
            std::env::temp_dir().join(format!("aurel-cli-test-{}-broken-rt", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let proj = dir.join(".aurel");
        std::fs::create_dir_all(&proj).expect("test dirs");
        std::fs::write(proj.join("config.toml"), "log_level = [oops\n").expect("test config");
        let mut env = HashMap::new();
        env.insert("AUREL_LOG_LEVEL".to_string(), "chatty".to_string());
        let rt = Runtime {
            cwd: dir.clone(),
            appdata: None,
            home: None,
            env,
        };
        for argv in [
            vec![],
            vec!["--help"],
            vec!["-h"],
            vec!["--version"],
            vec!["-V"],
            vec!["config"],
            vec!["config", "--help"],
        ] {
            let (code, _out, _err) = run_to_string(&argv, &rt);
            assert_eq!(code, 0, "argv {argv:?} must not touch broken runtime state");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_config_prints_config_help() {
        let rt = test_runtime();
        let (code, out, err) = run_to_string(&["config"], &rt);
        assert_eq!(code, 0);
        assert!(out.contains("aurel config show"), "got: {out:?}");
        assert!(err.is_empty());
    }

    #[test]
    fn non_unicode_argument_is_handled_without_panic() {
        // Regression test: the args_os -> lossy conversion path must be total.
        let raw = vec![non_unicode_arg()];
        let args = normalize_args(&raw);
        assert_eq!(args.len(), 1);
        let rt = test_runtime();
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = run_with(&args, &mut out, &mut err, &rt, &mut &b""[..], false);
        // Lossy output matches no known flag or command, so it is a usage
        // error — reaching this assertion proves no panic occurred.
        assert_eq!(code, EXIT_USAGE_ERROR);
    }

    #[test]
    fn resolve_message_prefers_argv_then_pipe() {
        let words = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(
            resolve_message(&words, &mut &b"ignored"[..], false),
            Ok("hello world".to_string())
        );
        // Piped stdin, trimmed.
        assert_eq!(
            resolve_message(&[], &mut &b"  piped\n"[..], false),
            Ok("piped".to_string())
        );
        // Terminal with no words: usage error, never a blocking read.
        assert_eq!(
            resolve_message(&[], &mut &b"would-block"[..], true),
            Err(MessageError::NoMessage)
        );
        // Empty pipe: usage error.
        assert_eq!(
            resolve_message(&[], &mut &b"  \n"[..], false),
            Err(MessageError::NoMessage)
        );
        // Empty argv word: usage error.
        assert_eq!(
            resolve_message(&["  ".to_string()], &mut &b""[..], false),
            Err(MessageError::NoMessage)
        );
    }

    /// Test double for [`chat_with_provider`]: canned outcome, optional
    /// progressive delivery, event counting.
    struct FakeProvider {
        response: Result<aurel_model::ChatResponse, aurel_model::ProviderError>,
        streaming: bool,
        events_seen: std::cell::Cell<usize>,
    }

    impl ModelProvider for FakeProvider {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn capabilities(&self) -> aurel_model::Capabilities {
            aurel_model::Capabilities {
                streaming: self.streaming,
                tool_calling: false,
                structured_output: false,
                context_window: None,
            }
        }

        fn chat(
            &self,
            _request: &aurel_model::ChatRequest,
        ) -> Result<aurel_model::ChatResponse, aurel_model::ProviderError> {
            self.response.clone()
        }

        fn chat_stream(
            &self,
            request: &aurel_model::ChatRequest,
            on_event: &mut dyn FnMut(aurel_model::StreamEvent) -> aurel_model::StreamControl,
        ) -> Result<aurel_model::ChatResponse, aurel_model::ProviderError> {
            if request.stream && self.streaming {
                for delta in ["Hel", "lo"] {
                    self.events_seen.set(self.events_seen.get() + 1);
                    on_event(aurel_model::StreamEvent {
                        delta: delta.to_string(),
                    });
                }
            }
            self.response.clone()
        }
    }

    fn fake_response() -> aurel_model::ChatResponse {
        aurel_model::ChatResponse {
            content: "fake reply".to_string(),
            role: aurel_model::Role::Assistant,
            model: "fake-model".to_string(),
            finish_reason: Some(aurel_model::FinishReason::Stop),
            usage: None,
        }
    }

    #[test]
    fn chat_prints_full_reply_without_streaming() {
        let provider = FakeProvider {
            response: Ok(fake_response()),
            streaming: false,
            events_seen: std::cell::Cell::new(0),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = chat_with_provider("hi", false, &provider, &mut out, &mut err);
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).expect("utf8"), "fake reply\n");
        assert_eq!(provider.events_seen.get(), 0);
    }

    #[test]
    fn chat_streams_progressively() {
        let provider = FakeProvider {
            response: Ok(fake_response()),
            streaming: true,
            events_seen: std::cell::Cell::new(0),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = chat_with_provider("hi", true, &provider, &mut out, &mut err);
        assert_eq!(code, 0);
        assert_eq!(String::from_utf8(out).expect("utf8"), "Hello\n");
        assert_eq!(provider.events_seen.get(), 2);
    }

    #[test]
    fn chat_provider_failure_reports_exit_1_without_secrets() {
        let provider = FakeProvider {
            response: Err(aurel_model::ProviderError::Authentication(
                "endpoint rejected the credentials; set [model] api_key or AUREL_API_KEY".into(),
            )),
            streaming: true,
            events_seen: std::cell::Cell::new(0),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let code = chat_with_provider("hi", true, &provider, &mut out, &mut err);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        let err = String::from_utf8(err).expect("utf8");
        assert!(err.contains("rejected the credentials"), "got: {err:?}");
    }

    /// A writer that accepts exactly one write, then fails every write.
    struct FailAfterFirst {
        writes: std::cell::Cell<usize>,
    }

    impl Write for FailAfterFirst {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.set(self.writes.get() + 1);
            if self.writes.get() > 1 {
                Err(std::io::Error::other("sink is broken"))
            } else {
                Ok(buf.len())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A provider offering far more deltas than the test will consume.
    struct ManyDeltas {
        offered: std::cell::Cell<usize>,
    }

    impl ModelProvider for ManyDeltas {
        fn name(&self) -> &'static str {
            "many"
        }

        fn capabilities(&self) -> aurel_model::Capabilities {
            aurel_model::Capabilities {
                streaming: true,
                tool_calling: false,
                structured_output: false,
                context_window: None,
            }
        }

        fn chat(
            &self,
            _request: &aurel_model::ChatRequest,
        ) -> Result<aurel_model::ChatResponse, aurel_model::ProviderError> {
            Ok(fake_response())
        }

        fn chat_stream(
            &self,
            _request: &aurel_model::ChatRequest,
            on_event: &mut dyn FnMut(StreamEvent) -> StreamControl,
        ) -> Result<aurel_model::ChatResponse, aurel_model::ProviderError> {
            for i in 0..10 {
                self.offered.set(self.offered.get() + 1);
                if on_event(StreamEvent {
                    delta: format!("d{i}"),
                }) == StreamControl::Cancel
                {
                    return Err(aurel_model::ProviderError::Cancelled);
                }
            }
            Ok(fake_response())
        }
    }

    #[test]
    fn chat_stops_consuming_when_stdout_fails() {
        let provider = ManyDeltas {
            offered: std::cell::Cell::new(0),
        };
        let mut out = FailAfterFirst {
            writes: std::cell::Cell::new(0),
        };
        let mut err = Vec::new();
        let code = chat_with_provider("hi", true, &provider, &mut out, &mut err);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        assert!(
            provider.offered.get() < 10,
            "must stop early instead of draining the stream (offered {})",
            provider.offered.get()
        );
        let err = String::from_utf8(err).expect("utf8");
        assert!(err.contains("failed to write model output"), "got: {err:?}");
    }

    #[test]
    fn agent_reports_write_failure_not_cancellation() {
        // Same broken-sink setup through the agent path: the provider sees
        // our Cancel verdict and reports Cancelled, but the user must see
        // the write failure — matching `chat` behavior.
        let provider = ManyDeltas {
            offered: std::cell::Cell::new(0),
        };
        let agent = Agent::new(
            provider,
            AgentConfig {
                max_iterations: 5,
                streaming: true,
            },
        )
        .expect("valid config");
        let mut out = FailAfterFirst {
            writes: std::cell::Cell::new(0),
        };
        let mut err = Vec::new();
        let mut session = AgentSession::new();
        let (code, _) = run_agent(&agent, &mut session, "hi", &mut out, &mut err);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        assert!(
            agent.provider().offered.get() < 10,
            "must stop early instead of draining the stream (offered {})",
            agent.provider().offered.get()
        );
        let err = String::from_utf8(err).expect("utf8");
        assert!(err.contains("failed to write model output"), "got: {err:?}");
        assert!(!err.contains("cancelled"), "must not misreport: {err:?}");
    }

    #[test]
    fn chat_without_message_or_pipe_is_exit_2() {
        let rt = test_runtime();
        // Empty pipe: usage error, no network touched.
        let (code, _out, err) = run_to_string_with_stdin(&["chat"], &rt, &mut &b""[..], false);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("no message"), "got: {err:?}");
    }

    #[test]
    fn chat_build_failure_is_exit_1_without_network() {
        // timeout_secs 0 fails provider construction before any I/O.
        let rt = Runtime {
            cwd: test_runtime().cwd,
            appdata: None,
            home: None,
            env: [("AUREL_TIMEOUT_SECS".to_string(), "0".to_string())]
                .into_iter()
                .collect(),
        };
        let (code, _out, err) =
            run_to_string_with_stdin(&["chat", "hi"], &rt, &mut &b""[..], false);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        assert!(err.contains("timeout"), "got: {err:?}");
    }

    #[test]
    fn agent_help_names_the_loop_bound() {
        let rt = test_runtime();
        let (code, out, err) = run_to_string(&["agent", "--help"], &rt);
        assert_eq!(code, 0);
        assert!(out.contains("USAGE:"), "got: {out:?}");
        assert!(out.contains("max-iterations"), "got: {out:?}");
        assert!(err.is_empty());
    }

    #[test]
    fn agent_without_message_or_pipe_is_exit_2() {
        let rt = test_runtime();
        let (code, _out, err) = run_to_string_with_stdin(&["agent"], &rt, &mut &b""[..], false);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("no message"), "got: {err:?}");
    }

    #[test]
    fn agent_invalid_max_iterations_flag_is_exit_2() {
        let rt = test_runtime();
        let (code, _out, err) = run_to_string(&["--max-iterations", "0", "agent", "hi"], &rt);
        assert_eq!(code, EXIT_USAGE_ERROR);
        assert!(err.contains("--max-iterations"), "got: {err:?}");
    }

    #[test]
    fn agent_build_failure_is_exit_1_without_network() {
        // timeout_secs 0 fails provider construction before any I/O, even on
        // the agent path.
        let rt = Runtime {
            cwd: test_runtime().cwd,
            appdata: None,
            home: None,
            env: [("AUREL_TIMEOUT_SECS".to_string(), "0".to_string())]
                .into_iter()
                .collect(),
        };
        let (code, _out, err) =
            run_to_string_with_stdin(&["agent", "hi"], &rt, &mut &b""[..], false);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        assert!(err.contains("timeout"), "got: {err:?}");
    }

    #[test]
    fn agent_rejects_over_cap_iterations_without_network() {
        // max_iterations above the hard cap fails agent construction: no I/O.
        let rt = Runtime {
            cwd: test_runtime().cwd,
            appdata: None,
            home: None,
            env: [("AUREL_MAX_ITERATIONS".to_string(), "101".to_string())]
                .into_iter()
                .collect(),
        };
        let (code, _out, err) =
            run_to_string_with_stdin(&["agent", "hi"], &rt, &mut &b""[..], false);
        assert_eq!(code, EXIT_RUNTIME_ERROR);
        assert!(err.contains("max_iterations"), "got: {err:?}");
    }

    /// Non-Unicode OS input for the current platform (bytes that are invalid
    /// UTF-8 and would make `std::env::args()` panic).
    #[cfg(unix)]
    fn non_unicode_arg() -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(vec![0xFF, b'x'])
    }

    /// Non-Unicode OS input for the current platform (an unpaired surrogate,
    /// which is not valid Unicode).
    #[cfg(windows)]
    fn non_unicode_arg() -> OsString {
        use std::os::windows::ffi::OsStringExt;
        OsString::from_wide(&[0xD800, b'x' as u16])
    }

    #[cfg(not(any(unix, windows)))]
    fn non_unicode_arg() -> OsString {
        // Fallback: no portable invalid-Unicode constructor here, so exercise
        // the conversion path with a plain argument instead.
        OsString::from("--wat")
    }
}
