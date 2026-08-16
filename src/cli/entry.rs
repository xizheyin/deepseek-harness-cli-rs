use std::{
    ffi::OsString,
    io::{self, IsTerminal as _, Write as _},
    path::PathBuf,
    process::ExitCode,
};

use tokio::runtime::{Builder, Runtime};

use crate::{session::SessionStore, workspace_authority::WorkspaceAuthority};

use super::{
    args::{CliOptions, ListSessionsOptions, ParseAction, ParseError, parse_args_os},
    assembly::{AgentAssembly, AssemblyError, assemble_session, prepare_new_session},
    interactive::{self, InteractiveError},
    render::VisibleRenderer,
    script_driver::{self, ScriptDriverError},
    script_io::{ScriptInputError, read_piped_prompt_or_exit},
    session_list::write_session_list,
    session_resume::{ResumeError, WarningTarget},
    shutdown,
    signal::{DriverMode, SignalLatch, SignalStreams, UiSignal, self_suspend},
    storage_failure,
    terminal::{AsyncTerminal, OpenTerminal, TerminalError},
};

const HELP: &str = "dsh - terminal coding agent for DeepSeek\n\
\n\
Usage: dsh [OPTIONS]\n\
       dsh --resume <SESSION_ID> [OPTIONS]\n\
       dsh --list-sessions [-w <PATH>] [--no-color]\n\
\n\
Options:\n\
  -p, --prompt <TEXT>      Run one prompt and exit\n\
  -m, --model <MODEL>      DeepSeek model (new: deepseek-v4-flash; resume: stored model)\n\
  -w, --workspace <PATH>   Workspace (new: current; resume: optional identity check)\n\
      --no-color           Force plain output (plain is also the Phase 7 default)\n\
      --list-sessions      List persisted session headers\n\
      --resume <SESSION_ID> Resume one persisted session\n\
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
        ParseAction::ListSessions(options) => run_list_sessions(options),
        ParseAction::Run(options) => run_options(options),
    }
}

fn run_list_sessions(options: ListSessionsOptions) -> Result<u8, EntryError> {
    let ListSessionsOptions {
        workspace,
        no_color: _,
    } = options;
    let store = SessionStore::open_default().map_err(EntryError::storage)?;
    let workspace = workspace
        .map(|workspace| WorkspaceAuthority::open(std::path::Path::new(&workspace)))
        .transpose()
        .map_err(|_| EntryError::workspace())?;
    let sessions = store
        .list(workspace.as_ref().map(WorkspaceAuthority::identity))
        .map_err(EntryError::storage)?;
    let mut stdout = io::stdout().lock();
    write_session_list(&mut stdout, &sessions).map_err(|_| EntryError::output())?;
    stdout.flush().map_err(|_| EntryError::output())?;
    Ok(0)
}

fn run_options(options: CliOptions) -> Result<u8, EntryError> {
    let CliOptions {
        prompt,
        model,
        workspace,
        resume,
        no_color: _,
    } = options;
    let runtime = build_runtime()?;
    let mut signals = runtime
        .block_on(async { SignalStreams::install() })
        .map_err(|_| EntryError::agent())?;

    // Piped input can intentionally process-exit on a signal, and terminal
    // registration can fail. Both therefore finish before a resume worker or
    // journal lock exists.
    let surface = if let Some(prompt) = prompt {
        LaunchSurface::Script(prompt)
    } else if !io::stdin().is_terminal() {
        let prompt = runtime
            .block_on(read_piped_prompt_or_exit(&mut signals))
            .map_err(EntryError::input)?;
        LaunchSurface::Script(prompt)
    } else {
        if !io::stdout().is_terminal() || !io::stderr().is_terminal() {
            return Err(EntryError::partial_terminal());
        }
        let open = OpenTerminal::open_and_validate().map_err(EntryError::terminal)?;
        let terminal = runtime
            .block_on(async move { open.register() })
            .map_err(EntryError::terminal)?;
        LaunchSurface::Interactive(terminal)
    };
    let mode = surface.mode();
    let interactive = matches!(surface, LaunchSurface::Interactive(_));

    let prepared = match resume {
        Some(id) => {
            let store = SessionStore::open_default().map_err(EntryError::storage)?;
            loop {
                let target = match &surface {
                    LaunchSurface::Script(_) => WarningTarget::Script,
                    LaunchSurface::Interactive(terminal) => WarningTarget::Interactive(terminal),
                };
                match runtime.block_on(super::session_resume::resume(
                    &store,
                    id.clone(),
                    workspace.as_deref().map(PathBuf::from),
                    target,
                    &mut signals,
                )) {
                    Ok(ready) => break ready.assembly,
                    Err(ResumeError::Storage(error)) => {
                        return Err(EntryError::storage(error));
                    }
                    Err(ResumeError::Terminal(error)) => {
                        return Err(EntryError::terminal(error));
                    }
                    Err(ResumeError::Output) => return Err(EntryError::failed_output()),
                    Err(ResumeError::Signal(signal)) => match resume_signal_action(mode, signal) {
                        ResumeSignalAction::RetryAfterInterrupt => {
                            let LaunchSurface::Interactive(terminal) = &surface else {
                                return Err(EntryError::agent());
                            };
                            terminal.flush_input().map_err(EntryError::terminal)?;
                            continue;
                        }
                        ResumeSignalAction::SuspendThenRetry => {
                            let LaunchSurface::Interactive(terminal) = &surface else {
                                return Err(EntryError::agent());
                            };
                            match runtime
                                .block_on(interactive::suspend_and_resume(terminal, &mut signals))
                            {
                                Ok(None) => continue,
                                Ok(Some(signal)) => {
                                    return Ok(exit_after_startup_signal(
                                        signal,
                                        DriverMode::Interactive,
                                        &mut signals,
                                    ));
                                }
                                Err(error) => return Err(EntryError::terminal(error)),
                            }
                        }
                        ResumeSignalAction::Exit => {
                            return Ok(exit_after_startup_signal(signal, mode, &mut signals));
                        }
                    },
                }
            }
        }
        None => {
            let workspace = resolve_workspace(workspace)?;
            prepare_new_session(&workspace).map_err(EntryError::assembly)?
        }
    };

    let assembly = match assemble_session(prepared, model, interactive) {
        Ok(assembly) => assembly,
        Err(failure) => {
            let (error, mut session) = failure.into_parts();
            let (cleanup, signal) = runtime.block_on(shutdown::session_with_signals(
                &mut session,
                mode,
                &mut signals,
                None,
            ));
            drop(session);
            if let Some(signal) = signal {
                return Ok(exit_after_startup_signal(signal, mode, &mut signals));
            }
            if let Err(error) = cleanup {
                return Err(EntryError::storage(storage_failure::from_shutdown(&error)));
            }
            return Err(EntryError::assembly(error));
        }
    };

    match (surface, assembly) {
        (LaunchSurface::Script(prompt), AgentAssembly::Script(agent)) => runtime
            .block_on(script_driver::run_one_turn(agent, prompt, &mut signals))
            .map_err(EntryError::script),
        (LaunchSurface::Interactive(terminal), AgentAssembly::Interactive(assembly)) => runtime
            .block_on(interactive::run(assembly, terminal, &mut signals))
            .map_err(EntryError::interactive),
        (surface, assembly) => {
            let mut agent = match assembly {
                AgentAssembly::Script(agent) => agent,
                AgentAssembly::Interactive(assembly) => assembly.agent,
            };
            let (cleanup, signal) = runtime.block_on(shutdown::agent_with_signals(
                &mut agent,
                surface.mode(),
                &mut signals,
                None,
            ));
            if let Some(signal) = signal {
                return Ok(exit_after_startup_signal(
                    signal,
                    surface.mode(),
                    &mut signals,
                ));
            }
            if let Err(error) = cleanup {
                return Err(EntryError::storage(storage_failure::from_shutdown(&error)));
            }
            Err(EntryError::agent())
        }
    }
}

enum LaunchSurface {
    Script(String),
    Interactive(AsyncTerminal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResumeSignalAction {
    RetryAfterInterrupt,
    SuspendThenRetry,
    Exit,
}

const fn resume_signal_action(mode: DriverMode, signal: UiSignal) -> ResumeSignalAction {
    match (mode, signal) {
        (DriverMode::Interactive, UiSignal::Interrupt) => ResumeSignalAction::RetryAfterInterrupt,
        (DriverMode::Interactive, UiSignal::Suspend) => ResumeSignalAction::SuspendThenRetry,
        _ => ResumeSignalAction::Exit,
    }
}

impl LaunchSurface {
    const fn mode(&self) -> DriverMode {
        match self {
            Self::Script(_) => DriverMode::Script,
            Self::Interactive(_) => DriverMode::Interactive,
        }
    }
}

fn exit_after_startup_signal(
    signal: UiSignal,
    mode: DriverMode,
    signals: &mut SignalStreams,
) -> u8 {
    if signal == UiSignal::Suspend {
        if self_suspend().is_err() {
            return 1;
        }
        let mut latch = SignalLatch::default();
        latch.observe(mode, signal);
        signals.drain_ready(mode, &mut latch);
        return latch
            .observed()
            .and_then(UiSignal::exit_code)
            .unwrap_or(148);
    }
    signal.exit_code().unwrap_or(1)
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
            AssemblyError::Store(error) => Self::storage(error),
        }
    }

    fn interactive(error: InteractiveError) -> Self {
        match error {
            InteractiveError::TerminalUnavailable => Self::stable("CLI_TERMINAL_UNAVAILABLE", 1),
            InteractiveError::TerminalUnsupported => Self::stable("CLI_TERMINAL_UNSUPPORTED", 1),
            InteractiveError::Agent => Self::agent(),
            InteractiveError::Storage(error) => Self::storage(error),
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

    fn script(error: ScriptDriverError) -> Self {
        match error {
            ScriptDriverError::Storage(error) => Self::storage(error),
            ScriptDriverError::Output => {
                let mut failure = Self::output();
                failure.emit_diagnostic = false;
                failure
            }
        }
    }

    fn storage(error: crate::session::StoreError) -> Self {
        Self::stable(storage_failure::stable_code(error), 1)
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

    fn failed_output() -> Self {
        let mut failure = Self::output();
        failure.emit_diagnostic = false;
        failure
    }

    const fn exit_code(&self) -> u8 {
        self.exit
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::{EntryError, HELP, ResumeSignalAction, resume_signal_action, run};
    use crate::cli::{
        interactive::InteractiveError,
        signal::{DriverMode, UiSignal},
    };

    #[test]
    fn help_and_version_do_not_require_product_assembly() {
        assert_eq!(run([OsString::from("--help")]).unwrap(), 0);
        assert!(HELP.contains("--prompt"));
        assert!(HELP.contains("--list-sessions"));
        assert!(HELP.contains("--resume <SESSION_ID>"));
        assert!(HELP.contains("resume: stored model"));
        assert!(HELP.contains("resume: optional identity check"));
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

    #[test]
    fn interactive_resume_retries_local_stops_but_script_resume_exits() {
        assert_eq!(
            resume_signal_action(DriverMode::Interactive, UiSignal::Interrupt),
            ResumeSignalAction::RetryAfterInterrupt
        );
        assert_eq!(
            resume_signal_action(DriverMode::Interactive, UiSignal::Suspend),
            ResumeSignalAction::SuspendThenRetry
        );
        for signal in [UiSignal::Hangup, UiSignal::Quit, UiSignal::Terminate] {
            assert_eq!(
                resume_signal_action(DriverMode::Interactive, signal),
                ResumeSignalAction::Exit
            );
        }
        for signal in [
            UiSignal::Interrupt,
            UiSignal::Hangup,
            UiSignal::Quit,
            UiSignal::Terminate,
            UiSignal::Suspend,
        ] {
            assert_eq!(
                resume_signal_action(DriverMode::Script, signal),
                ResumeSignalAction::Exit
            );
        }
    }
}
