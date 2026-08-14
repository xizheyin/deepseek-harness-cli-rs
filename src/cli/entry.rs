use std::{
    ffi::OsString,
    io::{self, IsTerminal as _, Write as _},
    path::PathBuf,
    process::ExitCode,
};

use tokio::runtime::{Builder, Runtime};

use super::{
    args::{CliOptions, ParseAction, ParseError, parse_args_os},
    assembly::{AgentAssembly, AssemblyError, assemble},
    interactive::{self, InteractiveError},
    render::VisibleRenderer,
    script_driver::{self, ScriptDriverError},
    script_io::{ScriptInputError, read_piped_prompt_or_exit},
    signal::SignalStreams,
    terminal::{OpenTerminal, TerminalError},
};

const HELP: &str = "dsh - terminal coding agent for DeepSeek\n\
\n\
Usage: dsh [OPTIONS]\n\
\n\
Options:\n\
  -p, --prompt <TEXT>      Run one prompt and exit\n\
  -m, --model <MODEL>      DeepSeek model (default: deepseek-v4-flash)\n\
  -w, --workspace <PATH>   Workspace root (default: current directory)\n\
      --no-color           Force plain output (plain is also the Phase 7 default)\n\
  -h, --help               Print help\n\
  -V, --version            Print version\n";

/// Runs the real `dsh` product entry while keeping `main.rs` assembly-free.
pub fn entry() -> ExitCode {
    match run(std::env::args_os().skip(1)) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            if error.emit_diagnostic {
                write_diagnostic(&error);
            }
            ExitCode::from(error.exit_code())
        }
    }
}

fn run(arguments: impl IntoIterator<Item = OsString>) -> Result<u8, EntryError> {
    match parse_args_os(arguments).map_err(EntryError::usage)? {
        ParseAction::Help => {
            write_stdout(HELP)?;
            Ok(0)
        }
        ParseAction::Version => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "dsh {}", env!("CARGO_PKG_VERSION"))
                .map_err(|_| EntryError::output())?;
            Ok(0)
        }
        ParseAction::Run(options) => run_options(options),
    }
}

fn run_options(options: CliOptions) -> Result<u8, EntryError> {
    let CliOptions {
        prompt,
        model,
        workspace,
        no_color: _,
    } = options;

    if let Some(prompt) = prompt {
        let workspace = resolve_workspace(workspace)?;
        let assembly = assemble(&workspace, model, false).map_err(EntryError::assembly)?;
        let AgentAssembly::Script(agent) = assembly else {
            return Err(EntryError::agent());
        };
        let runtime = build_runtime()?;
        let mut signals = runtime
            .block_on(async { SignalStreams::install() })
            .map_err(|_| EntryError::agent())?;
        return runtime
            .block_on(script_driver::run_one_turn(agent, prompt, &mut signals))
            .map_err(EntryError::script);
    }

    let stdin_is_terminal = io::stdin().is_terminal();
    if !stdin_is_terminal {
        let runtime = build_runtime()?;
        let mut signals = runtime
            .block_on(async { SignalStreams::install() })
            .map_err(|_| EntryError::agent())?;
        let prompt = runtime
            .block_on(read_piped_prompt_or_exit(&mut signals))
            .map_err(EntryError::input)?;
        let workspace = resolve_workspace(workspace)?;
        let assembly = assemble(&workspace, model, false).map_err(EntryError::assembly)?;
        let AgentAssembly::Script(agent) = assembly else {
            return Err(EntryError::agent());
        };
        return runtime
            .block_on(script_driver::run_one_turn(agent, prompt, &mut signals))
            .map_err(EntryError::script);
    }

    if !io::stdout().is_terminal() || !io::stderr().is_terminal() {
        return Err(EntryError::partial_terminal());
    }
    let workspace = resolve_workspace(workspace)?;
    let terminal = OpenTerminal::open_and_validate().map_err(EntryError::terminal)?;
    let assembly = assemble(&workspace, model, true).map_err(EntryError::assembly)?;
    let AgentAssembly::Interactive(assembly) = assembly else {
        return Err(EntryError::agent());
    };
    let runtime = build_runtime()?;
    let mut signals = runtime
        .block_on(async { SignalStreams::install() })
        .map_err(|_| EntryError::agent())?;
    runtime
        .block_on(interactive::run(assembly, terminal, &mut signals))
        .map_err(EntryError::interactive)
}

fn resolve_workspace(workspace: Option<String>) -> Result<PathBuf, EntryError> {
    match workspace {
        Some(workspace) => Ok(PathBuf::from(workspace)),
        None => std::env::current_dir().map_err(|_| EntryError::workspace()),
    }
}

fn build_runtime() -> Result<Runtime, EntryError> {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| EntryError::agent())
}

fn write_stdout(text: &str) -> Result<(), EntryError> {
    io::stdout()
        .lock()
        .write_all(text.as_bytes())
        .map_err(|_| EntryError::output())
}

fn write_diagnostic(error: &EntryError) {
    let mut stderr = io::stderr().lock();
    if write!(stderr, "dsh: {}", error.code).is_err() {
        return;
    }
    if let Some(detail) = error.detail.as_deref() {
        if stderr.write_all(b": ").is_err() {
            return;
        }
        let mut renderer = VisibleRenderer::new();
        if renderer
            .render_fragment(detail, None, |chunk| stderr.write_all(chunk.as_bytes()))
            .is_err()
        {
            return;
        }
    }
    let _ = stderr.write_all(b"\n");
}

#[derive(Debug)]
struct EntryError {
    code: &'static str,
    exit: u8,
    detail: Option<String>,
    emit_diagnostic: bool,
}

impl EntryError {
    fn stable(code: &'static str, exit: u8) -> Self {
        Self {
            code,
            exit,
            detail: None,
            emit_diagnostic: true,
        }
    }

    fn usage(error: ParseError) -> Self {
        Self {
            code: "CLI_USAGE",
            exit: 2,
            detail: Some(error.to_string()),
            emit_diagnostic: true,
        }
    }

    fn input(error: ScriptInputError) -> Self {
        match error {
            ScriptInputError::Invalid => Self::stable("CLI_INPUT_INVALID", 2),
            ScriptInputError::TooLarge => Self::stable("CLI_INPUT_TOO_LARGE", 2),
        }
    }

    fn terminal(error: TerminalError) -> Self {
        match error {
            TerminalError::Unavailable => Self::stable("CLI_TERMINAL_UNAVAILABLE", 1),
            TerminalError::Unsupported => Self::stable("CLI_TERMINAL_UNSUPPORTED", 1),
        }
    }

    fn partial_terminal() -> Self {
        Self {
            code: "CLI_TERMINAL_UNAVAILABLE",
            exit: 1,
            detail: Some(
                "stdin, stdout, and stderr must all be terminals; use --prompt for scripted input"
                    .to_owned(),
            ),
            emit_diagnostic: true,
        }
    }

    fn assembly(error: AssemblyError) -> Self {
        match error {
            AssemblyError::Workspace => Self::workspace(),
            AssemblyError::Provider => Self::stable("CLI_PROVIDER_UNAVAILABLE", 1),
            AssemblyError::Entropy => Self::stable("CLI_ENTROPY_UNAVAILABLE", 1),
            AssemblyError::Agent => Self::agent(),
        }
    }

    fn interactive(error: InteractiveError) -> Self {
        match error {
            InteractiveError::TerminalUnavailable => Self::stable("CLI_TERMINAL_UNAVAILABLE", 1),
            InteractiveError::TerminalUnsupported => Self::stable("CLI_TERMINAL_UNSUPPORTED", 1),
            InteractiveError::Agent => Self::agent(),
            InteractiveError::Output => {
                // The terminal writer already proved that output cannot make
                // bounded progress. A second blocking stderr write could hang
                // forever on the same terminal and defeat that deadline.
                let mut failure = Self::output();
                failure.emit_diagnostic = false;
                failure
            }
        }
    }

    fn script(_error: ScriptDriverError) -> Self {
        let mut failure = Self::output();
        failure.emit_diagnostic = false;
        failure
    }

    fn workspace() -> Self {
        Self::stable("CLI_WORKSPACE_UNAVAILABLE", 1)
    }

    fn agent() -> Self {
        Self::stable("CLI_AGENT_UNAVAILABLE", 1)
    }

    fn output() -> Self {
        Self::stable("CLI_OUTPUT_FAILED", 1)
    }

    const fn exit_code(&self) -> u8 {
        self.exit
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{EntryError, HELP, run};
    use crate::cli::interactive::InteractiveError;

    #[test]
    fn help_and_version_do_not_require_product_assembly() {
        assert_eq!(run([OsString::from("--help")]).unwrap(), 0);
        assert!(HELP.contains("--prompt"));
        assert_eq!(run([OsString::from("--version")]).unwrap(), 0);
    }

    #[test]
    fn usage_failure_is_stable_and_keeps_only_the_bounded_parser_message() {
        let error = run([OsString::from("--unknown")]).unwrap_err();
        assert_eq!(error.code, "CLI_USAGE");
        assert_eq!(error.exit_code(), 2);
        assert_eq!(error.detail.as_deref(), Some("unknown command-line option"));
        let _ = EntryError::agent();
    }

    #[test]
    fn interactive_output_failure_does_not_retry_a_blocking_diagnostic() {
        let error = EntryError::interactive(InteractiveError::Output);
        assert_eq!(error.code, "CLI_OUTPUT_FAILED");
        assert_eq!(error.exit_code(), 1);
        assert!(!error.emit_diagnostic);
    }

    #[test]
    fn partial_terminal_error_recommends_the_script_entry() {
        let error = EntryError::partial_terminal();
        assert_eq!(error.code, "CLI_TERMINAL_UNAVAILABLE");
        assert_eq!(error.exit_code(), 1);
        assert_eq!(
            error.detail.as_deref(),
            Some(
                "stdin, stdout, and stderr must all be terminals; use --prompt for scripted input"
            )
        );
    }
}
