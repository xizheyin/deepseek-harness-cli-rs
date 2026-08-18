use std::time::Duration;
use std::{mem, ops::ControlFlow};

use futures_util::FutureExt as _;
use thiserror::Error;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::AgentLoop,
    session::{
        ApprovalOutcome, CommittedUiKind, CommittedUiReceiver, StoreError, TurnEndReason, TurnId,
        UiUserSource,
    },
    tui::{
        dock::{
            DockApprovalSelection, DockError, DockFrame, DockInteraction, DockModel,
            MIN_ENHANCED_COLUMNS, MIN_ENHANCED_ROWS,
        },
        inline_screen::{
            InlineScreen, InlineScreenError, POISON_REATTACH_BYTES, POISON_TEARDOWN_BYTES,
            PendingScreenWrite, ScreenSize,
        },
        input_memory::{InputMemory, InputMemoryError, LocalPromptId},
        key_decoder::{InputEvent, Key, KeyDecoder},
    },
};

use super::{
    approval::{ApprovalEnvelope, ApprovalEnvelopeReceiver},
    approval_join::{ApprovalJoin, ApprovalJoinError, ApprovalResetMode},
    approval_selector::{
        ApprovalInputProfile, ApprovalSelector, ESCAPE_SEQUENCE_WAIT, SelectorUpdate,
    },
    assembly::InteractiveAssembly,
    identity::prepare_user_turn,
    input::{
        CanonicalRecordParser, IdleInput, InputRecordEvent, MAX_APPROVAL_RECORD_BYTES,
        MAX_INTERACTIVE_PROMPT_BYTES, classify_idle_record,
    },
    live::{
        EnhancedPresenter, InteractivePresenter, LiveFrame, LiveLifecycle, LiveRenderer,
        PendingLiveFrame, PreparedPresentation,
    },
    shutdown,
    signal::{DriverMode, InteractiveSignal, SignalLatch, SignalStreams, UiSignal, self_suspend},
    storage_failure,
    terminal::{
        ApprovalTerminalMode, AsyncTerminal, ENHANCED_VISUAL_RESET_BYTES, TERMINAL_READ_BYTES,
        TerminalError, TerminalSession, TerminalSize,
    },
};

const FRAME_DEADLINE: Duration = Duration::from_secs(5);
const VISUAL_RESET_DEADLINE: Duration = Duration::from_millis(250);
const APPROVAL_INPUT_QUIET: Duration = Duration::from_millis(100);
const PASTE_INPUT_QUIET: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum InteractiveError {
    #[error("CLI_TERMINAL_UNAVAILABLE")]
    TerminalUnavailable,
    #[error("CLI_TERMINAL_UNSUPPORTED")]
    TerminalUnsupported,
    #[error("CLI_AGENT_UNAVAILABLE")]
    Agent,
    #[error(transparent)]
    Storage(StoreError),
    #[error("CLI_OUTPUT_FAILED")]
    Output,
}

impl From<TerminalError> for InteractiveError {
    fn from(value: TerminalError) -> Self {
        match value {
            TerminalError::Unavailable => Self::TerminalUnavailable,
            TerminalError::Unsupported => Self::TerminalUnsupported,
        }
    }
}

impl From<ApprovalJoinError> for InteractiveError {
    fn from(_value: ApprovalJoinError) -> Self {
        Self::Agent
    }
}

impl From<InputMemoryError> for InteractiveError {
    fn from(_: InputMemoryError) -> Self {
        Self::Agent
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StopIntent {
    Interrupt,
    Eof,
    Suspend,
    Exit(UiSignal),
    Failure(InteractiveError),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AfterFrame {
    #[default]
    None,
    ApprovalFence,
    ApprovalAccepting,
    TurnEnd,
}

enum PendingOutput {
    Unprepared(LiveFrame),
    Prepared(PreparedPresentation),
    Linear(PendingLiveFrame),
    Dock(DockInteraction),
    Inline(PendingInlineOutput),
}

enum InlineIntent {
    Transcript(PreparedPresentation),
    Dock(DockInteraction),
}

struct PendingInlineOutput {
    write: PendingScreenWrite,
    intent: InlineIntent,
}

impl PendingOutput {
    fn bytes(&self) -> &[u8] {
        match self {
            Self::Unprepared(_) | Self::Prepared(_) | Self::Dock(_) => &[],
            Self::Linear(frame) => frame.bytes(),
            Self::Inline(output) => output.write.bytes(),
        }
    }

    fn advance(&mut self, count: usize) -> Result<(), InteractiveError> {
        match self {
            Self::Unprepared(_) | Self::Prepared(_) | Self::Dock(_) => Err(InteractiveError::Agent),
            Self::Linear(frame) => frame.advance(count).map_err(|_| InteractiveError::Output),
            Self::Inline(output) => output.write.advance(count).map_err(map_inline_screen_error),
        }
    }

    fn has_started(&self) -> bool {
        match self {
            Self::Unprepared(_) | Self::Prepared(_) | Self::Dock(_) => false,
            Self::Linear(_) => false,
            Self::Inline(output) => output.write.has_started(),
        }
    }
}

impl InlineIntent {
    fn into_pending(self) -> PendingOutput {
        match self {
            Self::Transcript(presentation) => PendingOutput::Prepared(presentation),
            Self::Dock(interaction) => PendingOutput::Dock(interaction),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnDisposition {
    Continue,
    Exit(u8),
    Signal(UiSignal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InteractiveExit {
    Ordinary(u8),
    Signal(UiSignal),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InteractivePresentation {
    Auto,
    Enhanced,
    Linear,
}

pub(super) async fn run(
    assembly: InteractiveAssembly,
    terminal: AsyncTerminal,
    signals: &mut SignalStreams,
    presentation: InteractivePresentation,
) -> Result<u8, InteractiveError> {
    let enhanced = presentation_uses_enhanced(presentation, terminal.size());
    if enhanced {
        run_enhanced(assembly, terminal, signals).await
    } else {
        run_linear(assembly, terminal, signals, false).await
    }
}

fn presentation_uses_enhanced(
    presentation: InteractivePresentation,
    size: Option<TerminalSize>,
) -> bool {
    !matches!(presentation, InteractivePresentation::Linear)
        && size.is_some_and(|size| {
            size.columns >= MIN_ENHANCED_COLUMNS && size.rows >= MIN_ENHANCED_ROWS
        })
}

async fn run_enhanced(
    assembly: InteractiveAssembly,
    terminal: AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<u8, InteractiveError> {
    let InteractiveAssembly {
        mut agent,
        mut events,
        mut approvals,
        mut joins,
        session_id,
        resumed,
    } = assembly;
    let mut live = LiveRenderer::new();
    let mut presenter = InteractivePresenter::with_color(true);
    let mut enhanced_presenter = EnhancedPresenter::new();
    let mut parser = CanonicalRecordParser::new(MAX_INTERACTIVE_PROMPT_BYTES);
    let mut scratch = [0_u8; TERMINAL_READ_BYTES];

    let banner = match LiveFrame::startup_banner(&session_id, resumed) {
        Ok(banner) => banner,
        Err(_) => {
            return shutdown_after_enhanced_error(&mut agent, signals, InteractiveError::Output)
                .await;
        }
    };
    let banner_signal = match write_frame(banner, &mut presenter, &terminal, signals).await {
        Ok(signal) => signal,
        Err(error) => return shutdown_after_enhanced_error(&mut agent, signals, error).await,
    };
    let banner_exit = match banner_signal {
        Some(signal) => match handle_idle_signal(signal, &terminal, signals).await {
            Ok(exit) => exit,
            Err(error) => {
                return shutdown_after_enhanced_error(&mut agent, signals, error).await;
            }
        },
        None => None,
    };
    if let Some(signal) = banner_exit {
        let mut agent_result = Ok(());
        let (shutdown, observed) = shutdown::agent_with_signals(
            &mut agent,
            DriverMode::Interactive,
            signals,
            Some(signal),
        )
        .await;
        if let Err(error) = shutdown {
            agent_result = Err(error);
        }
        let signal = observed.unwrap_or(signal);
        return match agent_result {
            Err(error) => match error.session_error() {
                Some(error) => Err(InteractiveError::Storage(storage_failure::from_shutdown(
                    error,
                ))),
                None => Err(InteractiveError::Agent),
            },
            Ok(()) => signal.exit_code().ok_or(InteractiveError::Agent),
        };
    }

    let mut last_size = match terminal.size() {
        Some(size) => size,
        None => {
            return shutdown_after_enhanced_error(
                &mut agent,
                signals,
                InteractiveError::TerminalUnsupported,
            )
            .await;
        }
    };
    let mut terminal = match terminal.into_application_session() {
        Ok(terminal) => terminal,
        Err(error) => {
            return shutdown_after_enhanced_error(&mut agent, signals, error.into()).await;
        }
    };
    let mut decoder = KeyDecoder::default();
    if decoder.reset_epoch().is_err() {
        let _ = terminal.finish();
        return shutdown_after_enhanced_error(&mut agent, signals, InteractiveError::Agent).await;
    }
    let mut input = InputMemory::default();
    let mut notice = None;
    let mut screen = InlineScreen::default();
    let initial_dock = render_enhanced_dock(
        &input,
        notice.as_deref(),
        DockInteraction::Idle,
        &terminal,
        &mut last_size,
        signals,
        &mut screen,
    )
    .await;
    let mut pending_signal = match initial_dock {
        Ok(signal) => signal,
        Err(error) => {
            terminal.best_effort_visual_reset();
            let _ = terminal.finish();
            return shutdown_after_enhanced_error(&mut agent, signals, error).await;
        }
    };
    let mut auto_queue_paused = false;

    let result: Result<InteractiveExit, InteractiveError> = async {
        loop {
            let event = if let Some(signal) = pending_signal.take() {
                EnhancedIdleEvent::Signal(signal)
            } else if input.queue().len() != 0 && !auto_queue_paused {
                EnhancedIdleEvent::AutoSubmit
            } else {
                terminal.revalidate_application()?;
                tokio::select! {
                    biased;
                    signal = signals.next_interactive() => match signal {
                        InteractiveSignal::Stop(signal) => EnhancedIdleEvent::Signal(signal),
                        InteractiveSignal::Resize => EnhancedIdleEvent::Resize,
                    },
                    read = terminal.read_once(&mut scratch) => {
                        let count = read.map_err(|_| InteractiveError::TerminalUnavailable)?;
                        if count == 0 {
                            EnhancedIdleEvent::Eof
                        } else {
                            EnhancedIdleEvent::Bytes(count)
                        }
                    }
                }
            };
            let action = match event {
                EnhancedIdleEvent::Signal(UiSignal::Interrupt) => {
                    decoder.reset_epoch().map_err(|_| InteractiveError::Agent)?;
                    if input.composer().is_empty() {
                        break Ok(InteractiveExit::Ordinary(0));
                    }
                    let _ = input.take_draft_for_turn()?;
                    notice = Some("Draft cleared · Ctrl+C again to exit".to_owned());
                    EnhancedInputAction::Redraw
                }
                EnhancedIdleEvent::Signal(UiSignal::Suspend) => {
                    pending_signal = suspend_enhanced(
                        &mut terminal,
                        signals,
                        &mut decoder,
                        &input,
                        notice.as_deref(),
                        &mut screen,
                        &mut last_size,
                    )
                    .await?;
                    continue;
                }
                EnhancedIdleEvent::Signal(
                    signal @ (UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate),
                ) => break Ok(InteractiveExit::Signal(signal)),
                EnhancedIdleEvent::Eof => break Ok(InteractiveExit::Ordinary(0)),
                EnhancedIdleEvent::Resize => EnhancedInputAction::Redraw,
                EnhancedIdleEvent::AutoSubmit => EnhancedInputAction::Submit,
                EnhancedIdleEvent::Bytes(count) => {
                    auto_queue_paused = false;
                    apply_enhanced_input(
                        &mut decoder,
                        &scratch[..count],
                        &mut input,
                        last_size,
                        &mut notice,
                    )?
                }
            };

            match action {
                EnhancedInputAction::None => continue,
                EnhancedInputAction::Redraw | EnhancedInputAction::PasteFence => {}
                EnhancedInputAction::Exit => break Ok(InteractiveExit::Ordinary(0)),
                EnhancedInputAction::Submit => {
                    let mut queued_id: Option<LocalPromptId> = None;
                    let (draft, cursor) = if input.queue().len() != 0 {
                        let reserved = input.reserve_front()?;
                        let id = reserved.id();
                        let text = copy_enhanced_prompt(reserved.text())?;
                        queued_id = Some(id);
                        let cursor = text.len();
                        (text, cursor)
                    } else {
                        let cursor = input.composer().cursor();
                        (input.take_draft_for_turn()?, cursor)
                    };
                    let submission = if queued_id.is_some() {
                        EnhancedSubmission::Prompt
                    } else {
                        classify_enhanced_submission(&draft)
                    };
                    match submission {
                        EnhancedSubmission::Empty => notice = None,
                        EnhancedSubmission::Help => {
                            notice = Some(
                                "/help | /exit | /quit | Enter send | Ctrl+J newline".to_owned(),
                            );
                        }
                        EnhancedSubmission::Exit => break Ok(InteractiveExit::Ordinary(0)),
                        EnhancedSubmission::Prompt => {
                            let prompt = copy_enhanced_prompt(&draft)?;
                            presenter.observe_external_line_start();
                            parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
                            let mut prompt_committed = false;
                            let active_terminal = terminal.application_terminal()?;
                            let mut active_dock = ActiveDock {
                                screen: &mut screen,
                                last_size: &mut last_size,
                            };
                            if let Some(signal) = render_active_dock(
                                &input,
                                notice.as_deref(),
                                DockInteraction::Running,
                                active_terminal,
                                signals,
                                &mut active_dock,
                            )
                            .await?
                            {
                                if let Some(id) = queued_id {
                                    input.release_reserved(id)?;
                                } else {
                                    input
                                        .restore_uncommitted_draft(draft, cursor)
                                        .map_err(|_| InteractiveError::Agent)?;
                                }
                                pending_signal = Some(signal);
                                continue;
                            }
                            let disposition = run_turn(ActiveTurn {
                                agent: &mut agent,
                                events: &mut events,
                                approvals: &mut approvals,
                                joins: &mut joins,
                                live: &mut live,
                                presenter: &mut presenter,
                                terminal: active_terminal,
                                signals,
                                parser: &mut parser,
                                scratch: &mut scratch,
                                prompt,
                                prompt_committed: &mut prompt_committed,
                                queued_input: Some(&mut input),
                                queue_notice: Some(&mut notice),
                                enhanced_decoder: Some(&mut decoder),
                                active_dock: Some(active_dock),
                                enhanced_presenter: Some(&mut enhanced_presenter),
                                color: true,
                                enhanced: true,
                            })
                            .await?;
                            if matches!(
                                disposition,
                                TurnDisposition::Continue
                                    | TurnDisposition::Signal(UiSignal::Suspend)
                            ) {
                                settle_enhanced_prompt(
                                    &mut input,
                                    queued_id,
                                    draft,
                                    cursor,
                                    prompt_committed,
                                    &mut notice,
                                    &mut auto_queue_paused,
                                )?;
                                decoder.reset_epoch().map_err(|_| InteractiveError::Agent)?;
                            }
                            match disposition {
                                TurnDisposition::Continue => {}
                                TurnDisposition::Exit(code) => {
                                    break Ok(InteractiveExit::Ordinary(code));
                                }
                                TurnDisposition::Signal(UiSignal::Suspend) => {
                                    pending_signal = suspend_enhanced(
                                        &mut terminal,
                                        signals,
                                        &mut decoder,
                                        &input,
                                        notice.as_deref(),
                                        &mut screen,
                                        &mut last_size,
                                    )
                                    .await?;
                                    continue;
                                }
                                TurnDisposition::Signal(signal) => {
                                    break Ok(InteractiveExit::Signal(signal));
                                }
                            }
                        }
                    }
                }
            }

            pending_signal = render_enhanced_dock(
                &input,
                notice.as_deref(),
                DockInteraction::Idle,
                &terminal,
                &mut last_size,
                signals,
                &mut screen,
            )
            .await?;
            if action == EnhancedInputAction::PasteFence && pending_signal.is_none() {
                match complete_paste_input_fence(
                    terminal.application_terminal()?,
                    signals,
                    &mut scratch,
                )
                .await?
                {
                    PasteFenceOutcome::Ready => {
                        decoder.reset_epoch().map_err(|_| InteractiveError::Agent)?;
                        notice = Some("Paste ready · Enter sends".to_owned());
                        pending_signal = render_enhanced_dock(
                            &input,
                            notice.as_deref(),
                            DockInteraction::Idle,
                            &terminal,
                            &mut last_size,
                            signals,
                            &mut screen,
                        )
                        .await?;
                    }
                    PasteFenceOutcome::Signal(signal) => pending_signal = Some(signal),
                    PasteFenceOutcome::Eof => break Ok(InteractiveExit::Ordinary(0)),
                }
            }
        }
    }
    .await;

    let mut result = result;
    let mut cleanup_signals = SignalLatch::default();
    let mut visual_reset_complete = false;
    let cleanup_geometry_changed = terminal.size().is_none_or(|size| size != last_size);
    let mut visual_reset_requires_clear = screen.is_poisoned() || cleanup_geometry_changed;
    if !screen.is_detached() && !screen.is_poisoned() && !cleanup_geometry_changed {
        match screen.stage_detach().map_err(map_inline_screen_error) {
            Ok(write) => match write_screen_transaction(
                terminal.output_terminal(),
                signals,
                &mut screen,
                write,
            )
            .await
            {
                Ok(ScreenWriteOutcome::Complete) => visual_reset_complete = true,
                Ok(ScreenWriteOutcome::Signal(signal)) => {
                    visual_reset_requires_clear = true;
                    observe_enhanced_cleanup_signal(&mut result, &mut cleanup_signals, signal);
                }
                Ok(ScreenWriteOutcome::Resize | ScreenWriteOutcome::PoisonedResize) => {
                    // The emulator changes geometry before SIGWINCH is
                    // delivered, so the old dock coordinates are no longer
                    // safe to clear selectively.
                    visual_reset_requires_clear = true;
                }
                Err(error) => {
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            },
            Err(error) => {
                if result.is_ok() {
                    result = Err(error);
                }
            }
        }
    }
    if !visual_reset_complete {
        visual_reset_requires_clear |= screen.is_poisoned();
        let reset = if visual_reset_requires_clear {
            POISON_TEARDOWN_BYTES
        } else {
            ENHANCED_VISUAL_RESET_BYTES
        };
        match write_enhanced_bytes(terminal.output_terminal(), reset, signals).await {
            Ok(Some(signal)) => {
                observe_enhanced_cleanup_signal(&mut result, &mut cleanup_signals, signal);
            }
            Ok(None) => visual_reset_complete = true,
            Err(error) => {
                if matches!(result, Ok(InteractiveExit::Ordinary(_))) {
                    result = Err(error);
                }
            }
        }
    }
    if !visual_reset_complete {
        terminal.best_effort_visual_reset();
    }
    if let Err(error) = terminal.finish() {
        if result.is_ok() {
            result = Err(error.into());
        }
    }

    if let Some(signal) = result.as_ref().ok().and_then(|exit| match exit {
        InteractiveExit::Signal(signal) => Some(*signal),
        InteractiveExit::Ordinary(_) => None,
    }) {
        cleanup_signals.observe(DriverMode::Interactive, signal);
    }
    let initial_signal = cleanup_signals.observed();
    let (shutdown, signal) =
        shutdown::agent_with_signals(&mut agent, DriverMode::Interactive, signals, initial_signal)
            .await;
    if let Some(signal) = signal {
        if let Some(code) =
            finish_signal_after_shutdown(signal, terminal.restored_terminal()?, signals).await?
        {
            return Ok(code);
        }
    }
    match (result, shutdown) {
        (Err(InteractiveError::Agent), Err(error)) => match error.session_error() {
            Some(error) => Err(InteractiveError::Storage(storage_failure::from_shutdown(
                error,
            ))),
            None => Err(InteractiveError::Agent),
        },
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => match error.session_error() {
            Some(error) => Err(InteractiveError::Storage(storage_failure::from_shutdown(
                error,
            ))),
            None => Err(InteractiveError::Agent),
        },
        (Ok(InteractiveExit::Ordinary(exit)), Ok(())) => Ok(exit),
        (Ok(InteractiveExit::Signal(_)), Ok(())) => Err(InteractiveError::Agent),
    }
}

fn observe_enhanced_cleanup_signal(
    result: &mut Result<InteractiveExit, InteractiveError>,
    signals: &mut SignalLatch,
    signal: UiSignal,
) {
    signals.observe(DriverMode::Interactive, signal);
    let terminating = matches!(
        signal,
        UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate
    );
    let terminating_already_latched = matches!(
        result,
        Ok(InteractiveExit::Signal(
            UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate
        ))
    );
    if terminating && !terminating_already_latched {
        *result = Ok(InteractiveExit::Signal(signal));
    }
}

#[allow(clippy::too_many_arguments)]
fn settle_enhanced_prompt(
    input: &mut InputMemory,
    queued_id: Option<LocalPromptId>,
    draft: String,
    cursor: usize,
    prompt_committed: bool,
    notice: &mut Option<String>,
    auto_queue_paused: &mut bool,
) -> Result<(), InteractiveError> {
    if prompt_committed {
        let history_prompt = if let Some(id) = queued_id {
            let admitted = input.commit_reserved(id)?;
            if admitted.id() != id {
                return Err(InteractiveError::Agent);
            }
            admitted.into_text()
        } else {
            draft
        };
        if input.record_committed_human(&history_prompt).is_err() {
            *notice = Some("History is full; the conversation is safe".to_owned());
        } else {
            *notice = None;
        }
        *auto_queue_paused = false;
    } else {
        if let Some(id) = queued_id {
            input.release_reserved(id)?;
            *auto_queue_paused = true;
        } else {
            input
                .restore_uncommitted_draft(draft, cursor)
                .map_err(|_| InteractiveError::Agent)?;
        }
        *notice = Some("Prompt was not admitted; draft or queue entry kept".to_owned());
    }
    Ok(())
}

async fn complete_paste_input_fence(
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    scratch: &mut [u8; TERMINAL_READ_BYTES],
) -> Result<PasteFenceOutcome, InteractiveError> {
    let mut quiet_deadline = Instant::now() + PASTE_INPUT_QUIET;
    loop {
        tokio::select! {
            biased;
            signal = signals.next_interactive() => match signal {
                InteractiveSignal::Stop(signal) => {
                    return Ok(PasteFenceOutcome::Signal(signal));
                }
                InteractiveSignal::Resize => {}
            },
            read = terminal.read_once(scratch) => {
                let count = read.map_err(|_| InteractiveError::TerminalUnavailable)?;
                if count == 0 {
                    return Ok(PasteFenceOutcome::Eof);
                }
                quiet_deadline = Instant::now() + PASTE_INPUT_QUIET;
            }
            () = tokio::time::sleep_until(quiet_deadline) => {
                terminal.flush_input()?;
                return Ok(PasteFenceOutcome::Ready);
            }
        }
    }
}

async fn shutdown_after_enhanced_error(
    agent: &mut AgentLoop,
    signals: &mut SignalStreams,
    error: InteractiveError,
) -> Result<u8, InteractiveError> {
    let (shutdown, signal) =
        shutdown::agent_with_signals(agent, DriverMode::Interactive, signals, None).await;
    if let Some(signal @ (UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate)) = signal {
        return signal.exit_code().ok_or(InteractiveError::Agent);
    }
    if let Err(shutdown) = shutdown {
        if let Some(storage) = shutdown.session_error() {
            return Err(InteractiveError::Storage(storage_failure::from_shutdown(
                storage,
            )));
        }
        if error == InteractiveError::Agent {
            return Err(InteractiveError::Agent);
        }
    }
    Err(error)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnhancedIdleEvent {
    Signal(UiSignal),
    Resize,
    Eof,
    AutoSubmit,
    Bytes(usize),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScreenWriteOutcome {
    Complete,
    Signal(UiSignal),
    Resize,
    PoisonedResize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnhancedInputAction {
    None,
    Redraw,
    PasteFence,
    Submit,
    Exit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PasteFenceOutcome {
    Ready,
    Signal(UiSignal),
    Eof,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnhancedSubmission {
    Empty,
    Help,
    Exit,
    Prompt,
}

fn classify_enhanced_submission(prompt: &str) -> EnhancedSubmission {
    match prompt.trim_matches(|character: char| character.is_ascii_whitespace()) {
        "" => EnhancedSubmission::Empty,
        "/help" => EnhancedSubmission::Help,
        "/exit" | "/quit" => EnhancedSubmission::Exit,
        _ => EnhancedSubmission::Prompt,
    }
}

fn apply_enhanced_input(
    decoder: &mut KeyDecoder,
    bytes: &[u8],
    input: &mut InputMemory,
    size: super::terminal::TerminalSize,
    notice: &mut Option<String>,
) -> Result<EnhancedInputAction, InteractiveError> {
    let mut action = EnhancedInputAction::None;
    let width = usize::from(size.columns.saturating_sub(3)).max(1);
    let _ = decoder.feed(bytes, |decoded| {
        let rejected = matches!(
            &decoded.event,
            InputEvent::Rejected(_) | InputEvent::PasteRejected(_)
        );
        let completed_paste = matches!(
            &decoded.event,
            InputEvent::Paste(_) | InputEvent::PasteRejected(_)
        );
        let update = match decoded.event {
            InputEvent::PasteStarted => Ok(EnhancedInputAction::None),
            InputEvent::Paste(text) => {
                *notice = Some(match input.insert_paste(&text) {
                    Ok(()) => "Paste inserted · Enter sends after the input fence".to_owned(),
                    Err(error) => format!("{error} · draft kept behind the input fence"),
                });
                Ok(EnhancedInputAction::PasteFence)
            }
            InputEvent::PasteRejected(error) => {
                *notice = Some(error.to_string());
                Ok(EnhancedInputAction::PasteFence)
            }
            InputEvent::Rejected(error) => {
                *notice = Some(error.to_string());
                Ok(EnhancedInputAction::Redraw)
            }
            InputEvent::Key(key) => apply_enhanced_key(key, input, width, notice),
        };
        match update {
            Ok(EnhancedInputAction::None) => ControlFlow::Continue(()),
            Ok(next) => {
                action = next;
                if rejected
                    || completed_paste
                    || matches!(
                        next,
                        EnhancedInputAction::Submit | EnhancedInputAction::Exit
                    )
                {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
            Err(error) => {
                *notice = Some(error.to_string());
                action = EnhancedInputAction::Redraw;
                ControlFlow::Break(())
            }
        }
    });
    Ok(action)
}

fn apply_enhanced_key(
    key: Key,
    input: &mut InputMemory,
    width: usize,
    notice: &mut Option<String>,
) -> Result<EnhancedInputAction, InputMemoryError> {
    let mut changed = false;
    match key {
        Key::Enter => return Ok(EnhancedInputAction::Submit),
        Key::Newline => input.insert_newline()?,
        Key::Char('?') if input.composer().is_empty() => {
            *notice = Some("/help · /exit · Enter send · Ctrl+J newline".to_owned());
            return Ok(EnhancedInputAction::Redraw);
        }
        Key::Char(character) => input.insert_char(character)?,
        Key::Tab => input.insert_text("\t")?,
        Key::BackTab => changed = input.move_left(),
        Key::Left => changed = input.move_left(),
        Key::Right => changed = input.move_right(),
        Key::Up => changed = input.move_up_or_history(width)?,
        Key::Down => changed = input.move_down_or_history(width)?,
        Key::Home => changed = input.move_line_start(),
        Key::End => changed = input.move_line_end(),
        Key::Backspace => changed = input.backspace()?,
        Key::Delete => changed = input.delete()?,
        Key::WordErase => changed = input.erase_word()?,
        Key::ClearBefore => changed = input.clear_before_cursor()?,
        Key::ClearAfter => changed = input.clear_after_cursor()?,
        Key::Yank => changed = input.yank()?,
        Key::Undo => changed = input.undo()?,
        Key::ReverseSearch => {
            let found = input.reverse_search_previous()?;
            *notice = Some(if found {
                "Reverse search · Ctrl+R finds the next older match".to_owned()
            } else {
                "No older history match".to_owned()
            });
            return Ok(EnhancedInputAction::Redraw);
        }
        Key::Escape => {
            *notice = None;
            return Ok(EnhancedInputAction::Redraw);
        }
        Key::Eof => {
            if input.composer().is_empty() {
                return Ok(EnhancedInputAction::Exit);
            }
            changed = input.delete()?;
        }
    }
    *notice = None;
    Ok(
        if changed
            || !matches!(
                key,
                Key::BackTab | Key::Left | Key::Right | Key::Up | Key::Down | Key::Home | Key::End
            )
        {
            EnhancedInputAction::Redraw
        } else {
            EnhancedInputAction::None
        },
    )
}

async fn render_enhanced_dock(
    input: &InputMemory,
    notice: Option<&str>,
    interaction: DockInteraction,
    terminal: &TerminalSession,
    last_size: &mut TerminalSize,
    signals: &mut SignalStreams,
    screen: &mut InlineScreen,
) -> Result<Option<UiSignal>, InteractiveError> {
    loop {
        if screen.is_poisoned() {
            if let Some(signal) =
                recover_poisoned_screen(terminal.output_terminal(), signals, screen).await?
            {
                return Ok(Some(signal));
            }
        }
        let size = terminal.size().unwrap_or(*last_size);
        let resized = size != *last_size;
        let frame = enhanced_dock_frame(input, notice, interaction, size)?;
        let write = if screen.is_detached() {
            screen.stage_attach(screen_size(size), &frame, true)
        } else if resized {
            screen.stage_resize(screen_size(size), &frame, true)
        } else {
            screen.stage_dock(&frame, true)
        }
        .map_err(map_inline_screen_error)?;
        match write_screen_transaction(terminal.output_terminal(), signals, screen, write).await? {
            ScreenWriteOutcome::Complete => {
                *last_size = size;
                return Ok(None);
            }
            ScreenWriteOutcome::Signal(signal) => return Ok(Some(signal)),
            ScreenWriteOutcome::Resize => continue,
            ScreenWriteOutcome::PoisonedResize => {
                if let Some(signal) =
                    recover_poisoned_screen(terminal.output_terminal(), signals, screen).await?
                {
                    return Ok(Some(signal));
                }
            }
        }
    }
}

async fn render_active_dock(
    input: &InputMemory,
    notice: Option<&str>,
    interaction: DockInteraction,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    dock: &mut ActiveDock<'_>,
) -> Result<Option<UiSignal>, InteractiveError> {
    loop {
        if dock.screen.is_poisoned() {
            if let Some(signal) = recover_poisoned_screen(terminal, signals, dock.screen).await? {
                return Ok(Some(signal));
            }
        }
        let size = terminal.size().unwrap_or(*dock.last_size);
        let resized = size != *dock.last_size;
        let frame = enhanced_dock_frame(input, notice, interaction, size)?;
        let write = if dock.screen.is_detached() {
            dock.screen.stage_attach(screen_size(size), &frame, true)
        } else if resized {
            dock.screen.stage_resize(screen_size(size), &frame, true)
        } else {
            dock.screen.stage_dock(&frame, true)
        }
        .map_err(map_inline_screen_error)?;
        match write_screen_transaction(terminal, signals, dock.screen, write).await? {
            ScreenWriteOutcome::Complete => {
                *dock.last_size = size;
                return Ok(None);
            }
            ScreenWriteOutcome::Signal(signal) => return Ok(Some(signal)),
            ScreenWriteOutcome::Resize => continue,
            ScreenWriteOutcome::PoisonedResize => {
                if let Some(signal) =
                    recover_poisoned_screen(terminal, signals, dock.screen).await?
                {
                    return Ok(Some(signal));
                }
            }
        }
    }
}

fn map_dock_error(error: DockError) -> InteractiveError {
    match error {
        DockError::TooSmall => InteractiveError::TerminalUnsupported,
        DockError::Capacity | DockError::Limit | DockError::InvalidState => {
            InteractiveError::Output
        }
    }
}

fn enhanced_dock_frame(
    input: &InputMemory,
    notice: Option<&str>,
    interaction: DockInteraction,
    size: TerminalSize,
) -> Result<DockFrame, InteractiveError> {
    DockFrame::layout(
        DockModel {
            interaction,
            composer: input.composer(),
            queue: input.queue(),
            notice,
        },
        size.rows,
        size.columns,
    )
    .map_err(map_dock_error)
}

fn map_inline_screen_error(error: InlineScreenError) -> InteractiveError {
    match error {
        InlineScreenError::TooSmall => InteractiveError::TerminalUnsupported,
        InlineScreenError::Capacity
        | InlineScreenError::Limit
        | InlineScreenError::InvalidState
        | InlineScreenError::Poisoned => InteractiveError::Output,
    }
}

const fn screen_size(size: TerminalSize) -> ScreenSize {
    ScreenSize {
        rows: size.rows,
        columns: size.columns,
    }
}

async fn write_enhanced_bytes(
    terminal: &AsyncTerminal,
    bytes: &[u8],
    signals: &mut SignalStreams,
) -> Result<Option<UiSignal>, InteractiveError> {
    let mut written = 0_usize;
    // This helper is used only after the normal coordinate transaction has
    // already failed or been poisoned. A blocked terminal must not cost a
    // second full frame deadline before termios is restored.
    let deadline = Instant::now() + VISUAL_RESET_DEADLINE;
    while written < bytes.len() {
        let work = tokio::select! {
            biased;
            signal = signals.next() => IdleWriteWork::Signal(signal),
            () = tokio::time::sleep_until(deadline) => IdleWriteWork::Expired,
            write = terminal.write_once(&bytes[written..]) => IdleWriteWork::Write(write),
        };
        match work {
            IdleWriteWork::Signal(signal) => {
                return Ok(Some(signal));
            }
            IdleWriteWork::Expired | IdleWriteWork::Write(Err(_)) => {
                return Err(InteractiveError::Output);
            }
            IdleWriteWork::Write(Ok(count)) => {
                written = written
                    .checked_add(count)
                    .filter(|written| *written <= bytes.len())
                    .ok_or(InteractiveError::Output)?;
            }
        }
    }
    Ok(None)
}

async fn recover_poisoned_screen(
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    screen: &mut InlineScreen,
) -> Result<Option<UiSignal>, InteractiveError> {
    if !screen.is_poisoned() {
        return Err(InteractiveError::Output);
    }
    if let Some(signal) = write_enhanced_bytes(terminal, POISON_REATTACH_BYTES, signals).await? {
        return Ok(Some(signal));
    }
    screen.recover_after_visual_reset();
    Ok(None)
}

async fn write_screen_transaction(
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    screen: &mut InlineScreen,
    mut write: PendingScreenWrite,
) -> Result<ScreenWriteOutcome, InteractiveError> {
    let deadline = Instant::now() + FRAME_DEADLINE;
    while !write.is_complete() {
        let work = tokio::select! {
            biased;
            signal = signals.next_interactive() => signal,
            () = tokio::time::sleep_until(deadline) => {
                screen.abort(write);
                return Err(InteractiveError::Output);
            }
            result = terminal.write_once(write.bytes()) => {
                match result {
                    Ok(count) => {
                        write.advance(count).map_err(map_inline_screen_error)?;
                        continue;
                    }
                    Err(_) => {
                        screen.abort(write);
                        return Err(InteractiveError::Output);
                    }
                }
            }
        };
        screen.abort(write);
        return match work {
            // A partially written coordinate batch poisons the screen ledger,
            // but it must not erase the operating-system signal that caused us
            // to stop. The caller will use a coordinate-free visual reset and
            // restore termios before honoring that signal.
            InteractiveSignal::Stop(signal) => Ok(ScreenWriteOutcome::Signal(signal)),
            InteractiveSignal::Resize if screen.is_poisoned() => {
                Ok(ScreenWriteOutcome::PoisonedResize)
            }
            InteractiveSignal::Resize => Ok(ScreenWriteOutcome::Resize),
        };
    }
    screen.commit(write).map_err(map_inline_screen_error)?;
    Ok(ScreenWriteOutcome::Complete)
}

fn copy_enhanced_prompt(prompt: &str) -> Result<String, InteractiveError> {
    let mut copy = String::new();
    copy.try_reserve_exact(prompt.len())
        .map_err(|_| InteractiveError::Agent)?;
    copy.push_str(prompt);
    Ok(copy)
}

async fn suspend_enhanced(
    terminal: &mut TerminalSession,
    signals: &mut SignalStreams,
    decoder: &mut KeyDecoder,
    input: &InputMemory,
    notice: Option<&str>,
    screen: &mut InlineScreen,
    last_size: &mut TerminalSize,
) -> Result<Option<UiSignal>, InteractiveError> {
    if !screen.is_detached() && !screen.is_poisoned() {
        let write = screen.stage_detach().map_err(map_inline_screen_error)?;
        match write_screen_transaction(terminal.output_terminal(), signals, screen, write).await? {
            ScreenWriteOutcome::Complete => {}
            ScreenWriteOutcome::Signal(signal) => return Ok(Some(signal)),
            ScreenWriteOutcome::Resize | ScreenWriteOutcome::PoisonedResize => {
                // Even a zero-byte resize invalidates the old absolute Dock
                // coordinates. Clear the uncertain viewport before giving
                // the terminal back to the shell.
                match write_enhanced_bytes(
                    terminal.output_terminal(),
                    POISON_TEARDOWN_BYTES,
                    signals,
                )
                .await
                {
                    Ok(Some(signal)) => return Ok(Some(signal)),
                    Ok(None) => screen.recover_after_visual_reset(),
                    Err(_) => terminal.best_effort_visual_reset(),
                }
            }
        }
    }
    if screen.is_poisoned() {
        match write_enhanced_bytes(terminal.output_terminal(), POISON_TEARDOWN_BYTES, signals).await
        {
            Ok(Some(signal)) => return Ok(Some(signal)),
            Ok(None) => screen.recover_after_visual_reset(),
            Err(_) => terminal.best_effort_visual_reset(),
        }
    }
    terminal.restore_for_suspend()?;
    loop {
        self_suspend().map_err(|_| InteractiveError::TerminalUnsupported)?;
        let mut latch = SignalLatch::default();
        signals.drain_ready(DriverMode::Interactive, &mut latch);
        if let Some(signal @ (UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate)) =
            latch.observed()
        {
            return Ok(Some(signal));
        }
        if terminal.is_foreground()? {
            terminal.reenter_after_resume()?;
            decoder.reset_epoch().map_err(|_| InteractiveError::Agent)?;
            if screen.is_poisoned() {
                if let Some(signal) =
                    recover_poisoned_screen(terminal.output_terminal(), signals, screen).await?
                {
                    return Ok(Some(signal));
                }
            }
            return render_enhanced_dock(
                input,
                notice,
                DockInteraction::Idle,
                terminal,
                last_size,
                signals,
                screen,
            )
            .await;
        }
    }
}

async fn run_linear(
    assembly: InteractiveAssembly,
    terminal: AsyncTerminal,
    signals: &mut SignalStreams,
    color: bool,
) -> Result<u8, InteractiveError> {
    let InteractiveAssembly {
        mut agent,
        mut events,
        mut approvals,
        mut joins,
        session_id,
        resumed,
    } = assembly;
    let mut live = LiveRenderer::new();
    let mut presenter = InteractivePresenter::with_color(color);
    let mut parser = CanonicalRecordParser::new(MAX_INTERACTIVE_PROMPT_BYTES);
    let mut scratch = [0_u8; TERMINAL_READ_BYTES];

    let result: Result<InteractiveExit, InteractiveError> = async {
        let banner = LiveFrame::startup_banner(&session_id, resumed)
            .map_err(|_| InteractiveError::Output)?;
        if let Some(signal) = write_frame(banner, &mut presenter, &terminal, signals).await? {
            if let Some(signal) = handle_idle_signal(signal, &terminal, signals).await? {
                return Ok(InteractiveExit::Signal(signal));
            }
        }

        loop {
            terminal.revalidate()?;
            terminal.flush_input()?;
            parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
            let prompt = LiveFrame::idle_prompt().map_err(|_| InteractiveError::Output)?;
            if let Some(signal) = write_frame(prompt, &mut presenter, &terminal, signals).await? {
                match handle_idle_signal(signal, &terminal, signals).await? {
                    Some(signal) => return Ok(InteractiveExit::Signal(signal)),
                    None => continue,
                }
            }
            terminal.revalidate()?;

            let input = loop {
                tokio::select! {
                    biased;
                    signal = signals.next() => break IdleEvent::Signal(signal),
                    read = terminal.read_once(&mut scratch) => {
                        let count = read.map_err(|_| InteractiveError::TerminalUnavailable)?;
                        if count == 0 {
                            break IdleEvent::Eof;
                        }
                        let mut first = None;
                        parser.feed(&scratch[..count], count < TERMINAL_READ_BYTES, |event| {
                            if first.is_none() {
                                first = Some(event);
                            }
                        });
                        if let Some(event) = first {
                            break IdleEvent::Record(event);
                        }
                    }
                }
            };

            // A signal may become ready after `select!` polled its stream but
            // before the terminal read completed. Sample again before treating
            // EOF or a record as success, and coalesce all ready signal classes.
            let mut latch = SignalLatch::default();
            if let IdleEvent::Signal(signal) = input {
                latch.observe(DriverMode::Interactive, signal);
            }
            tokio::task::yield_now().await;
            signals.drain_ready(DriverMode::Interactive, &mut latch);
            let input = match latch.observed() {
                Some(signal) => IdleEvent::Signal(signal),
                None => input,
            };

            match input {
                IdleEvent::Signal(signal) => {
                    presenter.discard_partly_written_frame();
                    match handle_idle_signal(signal, &terminal, signals).await? {
                        Some(signal) => return Ok(InteractiveExit::Signal(signal)),
                        None => continue,
                    }
                }
                IdleEvent::Eof => return Ok(InteractiveExit::Ordinary(0)),
                IdleEvent::Record(InputRecordEvent::TooLarge) => {
                    if let Some(signal) = write_notice(
                        "[input exceeds 1000 bytes]\n",
                        &mut presenter,
                        &terminal,
                        signals,
                    )
                    .await?
                    {
                        return Ok(InteractiveExit::Signal(signal));
                    }
                }
                IdleEvent::Record(InputRecordEvent::InvalidUtf8) => {
                    if let Some(signal) = write_notice(
                        "[input is not valid UTF-8]\n",
                        &mut presenter,
                        &terminal,
                        signals,
                    )
                    .await?
                    {
                        return Ok(InteractiveExit::Signal(signal));
                    }
                }
                IdleEvent::Record(InputRecordEvent::Record {
                    text,
                    terminated_by_lf,
                }) => match classify_idle_record(&text, terminated_by_lf) {
                    IdleInput::Redraw => {}
                    IdleInput::Help => {
                        let help = LiveFrame::help().map_err(|_| InteractiveError::Output)?;
                        if let Some(signal) =
                            write_frame(help, &mut presenter, &terminal, signals).await?
                        {
                            if let Some(signal) =
                                handle_idle_signal(signal, &terminal, signals).await?
                            {
                                return Ok(InteractiveExit::Signal(signal));
                            }
                        }
                    }
                    IdleInput::Exit => return Ok(InteractiveExit::Ordinary(0)),
                    IdleInput::Submit(prompt) => {
                        parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
                        let mut prompt_committed = false;
                        match run_turn(ActiveTurn {
                            agent: &mut agent,
                            events: &mut events,
                            approvals: &mut approvals,
                            joins: &mut joins,
                            live: &mut live,
                            presenter: &mut presenter,
                            terminal: &terminal,
                            signals,
                            parser: &mut parser,
                            scratch: &mut scratch,
                            prompt,
                            prompt_committed: &mut prompt_committed,
                            queued_input: None,
                            queue_notice: None,
                            enhanced_decoder: None,
                            active_dock: None,
                            enhanced_presenter: None,
                            color,
                            enhanced: false,
                        })
                        .await?
                        {
                            TurnDisposition::Continue => {}
                            TurnDisposition::Exit(code) => {
                                return Ok(InteractiveExit::Ordinary(code));
                            }
                            TurnDisposition::Signal(signal) => {
                                return Ok(InteractiveExit::Signal(signal));
                            }
                        }
                    }
                },
            }
        }
    }
    .await;
    let initial_signal = result.as_ref().ok().and_then(|exit| match exit {
        InteractiveExit::Signal(signal) => Some(*signal),
        InteractiveExit::Ordinary(_) => None,
    });
    let (shutdown, signal) =
        shutdown::agent_with_signals(&mut agent, DriverMode::Interactive, signals, initial_signal)
            .await;
    if let Some(signal) = signal {
        if let Some(code) = finish_signal_after_shutdown(signal, &terminal, signals).await? {
            return Ok(code);
        }
    }
    match (result, shutdown) {
        (Err(InteractiveError::Agent), Err(error)) => match error.session_error() {
            Some(error) => Err(InteractiveError::Storage(storage_failure::from_shutdown(
                error,
            ))),
            None => Err(InteractiveError::Agent),
        },
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => match error.session_error() {
            Some(error) => Err(InteractiveError::Storage(storage_failure::from_shutdown(
                error,
            ))),
            None => Err(InteractiveError::Agent),
        },
        (Ok(InteractiveExit::Ordinary(exit)), Ok(())) => Ok(exit),
        (Ok(InteractiveExit::Signal(_)), Ok(())) => Err(InteractiveError::Agent),
    }
}

enum IdleEvent {
    Signal(UiSignal),
    Eof,
    Record(InputRecordEvent),
}

struct ActiveTurn<'a> {
    agent: &'a mut AgentLoop,
    events: &'a mut CommittedUiReceiver,
    approvals: &'a mut ApprovalEnvelopeReceiver,
    joins: &'a mut ApprovalJoin,
    live: &'a mut LiveRenderer,
    presenter: &'a mut InteractivePresenter,
    terminal: &'a AsyncTerminal,
    signals: &'a mut SignalStreams,
    parser: &'a mut CanonicalRecordParser,
    scratch: &'a mut [u8; TERMINAL_READ_BYTES],
    prompt: String,
    prompt_committed: &'a mut bool,
    queued_input: Option<&'a mut InputMemory>,
    queue_notice: Option<&'a mut Option<String>>,
    enhanced_decoder: Option<&'a mut KeyDecoder>,
    active_dock: Option<ActiveDock<'a>>,
    enhanced_presenter: Option<&'a mut EnhancedPresenter>,
    color: bool,
    enhanced: bool,
}

struct ActiveDock<'a> {
    screen: &'a mut InlineScreen,
    last_size: &'a mut TerminalSize,
}

enum ApprovalUiState<'a> {
    Inactive,
    Arming {
        deadline: Instant,
    },
    Rendering {
        mode: Option<ApprovalTerminalMode<'a>>,
        selector: ApprovalSelector,
        compact: bool,
    },
    Accepting {
        mode: Option<ApprovalTerminalMode<'a>>,
        selector: ApprovalSelector,
        compact: bool,
        escape_deadline: Option<Instant>,
    },
}

enum ApprovalUiUpdate {
    None,
    Redraw(String),
    Decide(ApprovalOutcome),
    Eof,
    Invalid,
}

impl<'a> ApprovalUiState<'a> {
    const fn new() -> Self {
        Self::Inactive
    }

    const fn is_inactive(&self) -> bool {
        matches!(self, Self::Inactive)
    }

    const fn is_accepting(&self) -> bool {
        matches!(self, Self::Accepting { .. })
    }

    const fn suppresses_read_while_pending(&self) -> bool {
        matches!(self, Self::Rendering { .. } | Self::Accepting { .. })
    }

    const fn arm_deadline(&self) -> Option<Instant> {
        match self {
            Self::Arming { deadline } => Some(*deadline),
            _ => None,
        }
    }

    const fn escape_deadline(&self) -> Option<Instant> {
        match self {
            Self::Accepting {
                escape_deadline, ..
            } => *escape_deadline,
            _ => None,
        }
    }

    fn begin_arming(&mut self) -> Result<(), InteractiveError> {
        if !self.is_inactive() {
            return Err(InteractiveError::Agent);
        }
        *self = Self::Arming {
            deadline: Instant::now() + APPROVAL_INPUT_QUIET,
        };
        Ok(())
    }

    fn observe_unaccepted_input(&mut self) {
        if let Self::Arming { deadline } = self {
            *deadline = Instant::now() + APPROVAL_INPUT_QUIET;
        }
    }

    fn begin_rendering(
        &mut self,
        terminal: &'a AsyncTerminal,
        color: bool,
        enhanced: bool,
    ) -> Result<String, InteractiveError> {
        if !matches!(self, Self::Arming { .. }) {
            return Err(InteractiveError::Agent);
        }
        terminal.flush_input()?;
        let compact = terminal.columns().is_none_or(|columns| columns < 48);
        let mode = if enhanced {
            terminal.revalidate_identity()?;
            None
        } else {
            Some(terminal.enter_approval_mode()?)
        };
        let profile = if enhanced {
            ApprovalInputProfile::EnhancedDirectional
        } else {
            ApprovalInputProfile::LinearRecord
        };
        let selector = ApprovalSelector::new(profile).map_err(|_| InteractiveError::Agent)?;
        let output = if enhanced {
            String::new()
        } else {
            selector
                .render(color, compact, false)
                .map_err(|_| InteractiveError::Output)?
        };
        *self = Self::Rendering {
            mode,
            selector,
            compact,
        };
        Ok(output)
    }

    fn accept_rendered(&mut self, terminal: &AsyncTerminal) -> Result<(), InteractiveError> {
        let state = std::mem::replace(self, Self::Inactive);
        let Self::Rendering {
            mode,
            selector,
            compact,
        } = state
        else {
            *self = state;
            return Err(InteractiveError::Agent);
        };
        terminal.flush_input()?;
        *self = Self::Accepting {
            mode,
            selector,
            compact,
            escape_deadline: None,
        };
        Ok(())
    }

    fn feed(
        &mut self,
        bytes: &[u8],
        challenge: uuid::Uuid,
        color: bool,
        enhanced: bool,
    ) -> Result<ApprovalUiUpdate, InteractiveError> {
        let state = std::mem::replace(self, Self::Inactive);
        let Self::Accepting {
            mode,
            mut selector,
            compact,
            ..
        } = state
        else {
            *self = state;
            return Ok(ApprovalUiUpdate::None);
        };
        let update = selector.feed(bytes, challenge);
        match update {
            SelectorUpdate::None => {
                let escape_deadline = selector
                    .escape_is_pending()
                    .then(|| Instant::now() + ESCAPE_SEQUENCE_WAIT);
                *self = Self::Accepting {
                    mode,
                    selector,
                    compact,
                    escape_deadline,
                };
                Ok(ApprovalUiUpdate::None)
            }
            SelectorUpdate::Redraw => {
                let output = if enhanced {
                    String::new()
                } else {
                    selector
                        .render(color, compact, color && !compact)
                        .map_err(|_| InteractiveError::Output)?
                };
                *self = Self::Accepting {
                    mode,
                    selector,
                    compact,
                    escape_deadline: None,
                };
                Ok(ApprovalUiUpdate::Redraw(output))
            }
            SelectorUpdate::Decide(outcome) => {
                if let Some(mode) = mode {
                    mode.restore()?;
                }
                Ok(ApprovalUiUpdate::Decide(outcome))
            }
            SelectorUpdate::Eof => {
                if let Some(mode) = mode {
                    mode.restore()?;
                }
                Ok(ApprovalUiUpdate::Eof)
            }
            SelectorUpdate::Invalid => {
                if let Some(mode) = mode {
                    mode.restore()?;
                }
                Ok(ApprovalUiUpdate::Invalid)
            }
        }
    }

    fn expire_escape(&mut self) -> Result<ApprovalUiUpdate, InteractiveError> {
        let state = std::mem::replace(self, Self::Inactive);
        let Self::Accepting {
            mode,
            mut selector,
            compact,
            ..
        } = state
        else {
            *self = state;
            return Ok(ApprovalUiUpdate::None);
        };
        match selector.expire_escape() {
            SelectorUpdate::Decide(outcome) => {
                if let Some(mode) = mode {
                    mode.restore()?;
                }
                Ok(ApprovalUiUpdate::Decide(outcome))
            }
            SelectorUpdate::None => {
                *self = Self::Accepting {
                    mode,
                    selector,
                    compact,
                    escape_deadline: None,
                };
                Ok(ApprovalUiUpdate::None)
            }
            SelectorUpdate::Redraw | SelectorUpdate::Eof | SelectorUpdate::Invalid => {
                Err(InteractiveError::Agent)
            }
        }
    }

    fn restore(&mut self) -> Result<(), InteractiveError> {
        let state = std::mem::replace(self, Self::Inactive);
        match state {
            Self::Rendering { mode, .. } | Self::Accepting { mode, .. } => {
                mode.map_or(Ok(()), |mode| mode.restore().map_err(Into::into))
            }
            Self::Inactive | Self::Arming { .. } => Ok(()),
        }
    }

    fn dock_selection(&self) -> Result<DockApprovalSelection, InteractiveError> {
        let selector = match self {
            Self::Rendering { selector, .. } | Self::Accepting { selector, .. } => selector,
            Self::Inactive | Self::Arming { .. } => return Err(InteractiveError::Agent),
        };
        Ok(match selector.selected() {
            super::approval_selector::ApprovalSelection::AllowOnce => {
                DockApprovalSelection::AllowOnce
            }
            super::approval_selector::ApprovalSelection::Reject => DockApprovalSelection::Reject,
            super::approval_selector::ApprovalSelection::Cancel => DockApprovalSelection::Cancel,
        })
    }

    fn dock_interaction(&self) -> DockInteraction {
        match self {
            Self::Rendering { selector, .. } | Self::Accepting { selector, .. } => {
                DockInteraction::Approval(match selector.selected() {
                    super::approval_selector::ApprovalSelection::AllowOnce => {
                        DockApprovalSelection::AllowOnce
                    }
                    super::approval_selector::ApprovalSelection::Reject => {
                        DockApprovalSelection::Reject
                    }
                    super::approval_selector::ApprovalSelection::Cancel => {
                        DockApprovalSelection::Cancel
                    }
                })
            }
            Self::Inactive | Self::Arming { .. } => DockInteraction::Running,
        }
    }
}

async fn next_turn_signal(signals: &mut SignalStreams, enhanced: bool) -> InteractiveSignal {
    if enhanced {
        signals.next_interactive().await
    } else {
        InteractiveSignal::Stop(signals.next().await)
    }
}

async fn redraw_active_after_resize(
    enhanced: bool,
    input: Option<&InputMemory>,
    notice: Option<&str>,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    dock: Option<&mut ActiveDock<'_>>,
) -> Result<Option<UiSignal>, InteractiveError> {
    if !enhanced {
        return Ok(None);
    }
    render_active_dock(
        input.ok_or(InteractiveError::Agent)?,
        notice,
        DockInteraction::Running,
        terminal,
        signals,
        dock.ok_or(InteractiveError::Agent)?,
    )
    .await
}

fn prepare_pending_for_resize(
    pending: &mut Option<PendingOutput>,
    screen: &mut InlineScreen,
) -> Result<bool, InteractiveError> {
    if pending.as_ref().is_some_and(PendingOutput::has_started)
        && !matches!(
            pending.as_ref(),
            Some(PendingOutput::Inline(PendingInlineOutput {
                intent: InlineIntent::Dock(_),
                ..
            }))
        )
    {
        return Err(InteractiveError::Output);
    }
    let Some(output) = pending.take() else {
        return Ok(false);
    };
    let mut recover_visual_state = false;
    *pending = Some(match output {
        PendingOutput::Inline(output) => {
            recover_visual_state = output.write.has_started();
            screen.abort(output.write);
            if screen.is_poisoned() != recover_visual_state {
                return Err(InteractiveError::Output);
            }
            output.intent.into_pending()
        }
        output => output,
    });
    Ok(recover_visual_state)
}

#[allow(clippy::too_many_arguments)]
async fn reconcile_active_geometry(
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    input: Option<&InputMemory>,
    notice: Option<&str>,
    interaction: DockInteraction,
    dock: Option<&mut ActiveDock<'_>>,
    pending: &mut Option<PendingOutput>,
    presenter: Option<&mut EnhancedPresenter>,
) -> Result<Option<UiSignal>, InteractiveError> {
    let Some(dock) = dock else {
        return Ok(None);
    };
    let size = terminal.size().unwrap_or(*dock.last_size);
    if size == *dock.last_size {
        return Ok(None);
    }
    if prepare_pending_for_resize(pending, dock.screen)? {
        if let Some(signal) = recover_poisoned_screen(terminal, signals, dock.screen).await? {
            return Ok(Some(signal));
        }
    }
    let signal = render_active_dock(
        input.ok_or(InteractiveError::Agent)?,
        notice,
        interaction,
        terminal,
        signals,
        dock,
    )
    .await?;
    if signal.is_none() {
        if let Some(PendingOutput::Prepared(presentation)) = pending.as_mut() {
            presentation.force_next_line_boundary();
        }
        if let Some(presenter) = presenter {
            presenter.force_line_boundary();
        }
    }
    Ok(signal)
}

async fn run_turn(mut active: ActiveTurn<'_>) -> Result<TurnDisposition, InteractiveError> {
    let prepared = prepare_user_turn(active.agent.session(), &active.prompt)
        .map_err(|_| InteractiveError::Agent)?;
    let start_seq = prepared.start_seq;
    let turn = prepared.turn;
    active.joins.begin_turn()?;
    active.parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
    let cancellation = CancellationToken::new();
    let mut pending = None;
    let mut frame_deadline = None;
    let mut after_frame = AfterFrame::None;
    let mut approval_ui = ApprovalUiState::new();
    let mut turn_end_seen = false;
    let mut turn_end_rendered = false;
    let mut stop = None;
    let mut prefer_input = true;
    let mut dock_redraw_requested = false;

    let result = {
        let future = active
            .agent
            .run_turn(prepared.proposal, cancellation.clone());
        tokio::pin!(future);
        loop {
            if latch_observer_fault(active.events, &mut stop, &cancellation) {
                discard_pending(&mut pending, active.presenter);
            }
            if stop.is_some() {
                if let Err(error) = approval_ui.restore() {
                    observe_failure(&mut stop, error);
                    cancellation.cancel();
                }
                tokio::select! {
                    biased;
                    result = &mut future => break result,
                    signal = active.signals.next() => {
                        observe_signal(&mut stop, signal);
                        cancellation.cancel();
                    }
                }
                continue;
            }

            match reconcile_active_geometry(
                active.terminal,
                active.signals,
                active.queued_input.as_deref(),
                active.queue_notice.as_deref().and_then(Option::as_deref),
                approval_ui.dock_interaction(),
                active.active_dock.as_mut(),
                &mut pending,
                active.enhanced_presenter.as_deref_mut(),
            )
            .await
            {
                Ok(Some(signal)) => {
                    observe_signal(&mut stop, signal);
                    cancellation.cancel();
                    discard_pending(&mut pending, active.presenter);
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    latch_active_failure(
                        &mut stop,
                        &cancellation,
                        &mut pending,
                        active.presenter,
                        error,
                    );
                    continue;
                }
            }

            if let Err(error) = complete_ready_frame(
                &mut pending,
                &mut frame_deadline,
                &mut after_frame,
                &mut approval_ui,
                &mut turn_end_rendered,
                active.presenter,
                active.terminal,
                active.parser,
                active.enhanced,
                active.enhanced_presenter.as_deref_mut(),
                active.queued_input.as_deref(),
                active.queue_notice.as_deref().and_then(Option::as_deref),
                active.active_dock.as_mut(),
            ) {
                latch_active_failure(
                    &mut stop,
                    &cancellation,
                    &mut pending,
                    active.presenter,
                    error,
                );
                continue;
            }
            if pending.is_none() && mem::take(&mut dock_redraw_requested) {
                match redraw_active_after_resize(
                    active.enhanced,
                    active.queued_input.as_deref(),
                    active.queue_notice.as_deref().and_then(Option::as_deref),
                    active.terminal,
                    active.signals,
                    active.active_dock.as_mut(),
                )
                .await
                {
                    Ok(Some(signal)) => {
                        observe_signal(&mut stop, signal);
                        cancellation.cancel();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        observe_failure(&mut stop, error);
                        cancellation.cancel();
                    }
                }
                if stop.is_some() {
                    continue;
                }
            }
            if pending.is_none() && active.joins.question().is_some() && approval_ui.is_inactive() {
                let enqueue = approval_frame(active.joins, false).and_then(|frame| {
                    enqueue_frame(
                        frame,
                        AfterFrame::ApprovalFence,
                        &mut pending,
                        &mut frame_deadline,
                        &mut after_frame,
                    )
                });
                if let Err(error) = enqueue {
                    latch_active_failure(
                        &mut stop,
                        &cancellation,
                        &mut pending,
                        active.presenter,
                        error,
                    );
                    continue;
                }
            }
            // A freshly enqueued frame has no rendered bytes yet. Return to
            // `complete_ready_frame` before polling terminal writability;
            // writing an empty slice would look like a fatal WriteZero.
            if pending
                .as_ref()
                .is_some_and(|frame| frame.bytes().is_empty())
            {
                continue;
            }

            let work = next_ui_work(
                active.terminal,
                active.approvals,
                active.events,
                active.scratch,
                pending.as_ref(),
                frame_deadline,
                approval_ui.arm_deadline(),
                approval_ui.escape_deadline(),
                !(pending.is_some() && approval_ui.suppresses_read_while_pending()),
                prefer_input,
            );
            tokio::select! {
                biased;
                signal = next_turn_signal(active.signals, active.enhanced) => {
                    match signal {
                        InteractiveSignal::Stop(signal) => {
                            observe_signal(&mut stop, signal);
                            cancellation.cancel();
                            discard_pending(&mut pending, active.presenter);
                        }
                        InteractiveSignal::Resize => {
                            match reconcile_active_geometry(
                                active.terminal,
                                active.signals,
                                active.queued_input.as_deref(),
                                active
                                    .queue_notice
                                    .as_deref()
                                    .and_then(Option::as_deref),
                                approval_ui.dock_interaction(),
                                active.active_dock.as_mut(),
                                &mut pending,
                                active.enhanced_presenter.as_deref_mut(),
                            )
                            .await
                            {
                                Ok(Some(signal)) => {
                                    observe_signal(&mut stop, signal);
                                    cancellation.cancel();
                                    discard_pending(&mut pending, active.presenter);
                                }
                                Ok(None) => {}
                                Err(error) => latch_active_failure(
                                    &mut stop,
                                    &cancellation,
                                    &mut pending,
                                    active.presenter,
                                    error,
                                ),
                            }
                        }
                    }
                }
                result = &mut future => break result,
                work = work => {
                    prefer_input = !prefer_input;
                    match work {
                        UiWork::FrameExpired => latch_active_failure(
                            &mut stop,
                            &cancellation,
                            &mut pending,
                            active.presenter,
                            InteractiveError::Output,
                        ),
                        UiWork::ApprovalArmed => {
                            let prepared = approval_ui
                                .begin_rendering(active.terminal, active.color, active.enhanced)
                                .and_then(|output| {
                                    enqueue_approval_selector_surface(
                                        output,
                                        AfterFrame::ApprovalAccepting,
                                        &approval_ui,
                                        active.enhanced,
                                        &mut pending,
                                        &mut frame_deadline,
                                        &mut after_frame,
                                    )
                                });
                            if let Err(error) = prepared {
                                latch_active_failure(
                                    &mut stop,
                                    &cancellation,
                                    &mut pending,
                                    active.presenter,
                                    error,
                                );
                            }
                        }
                        UiWork::EscapeExpired => {
                            let handled = approval_ui.expire_escape().and_then(|update| {
                                dispatch_approval_update(
                                    update,
                                    active.joins,
                                    active.parser,
                                    &approval_ui,
                                    active.enhanced,
                                    &mut pending,
                                    &mut frame_deadline,
                                    &mut after_frame,
                                )
                            });
                            match handled {
                                Ok(false) => {}
                                Ok(true) => {
                                    stop = Some(StopIntent::Eof);
                                    cancellation.cancel();
                                    discard_pending(&mut pending, active.presenter);
                                }
                                Err(error) => latch_active_failure(
                                    &mut stop,
                                    &cancellation,
                                    &mut pending,
                                    active.presenter,
                                    error,
                                ),
                            }
                        }
                        UiWork::Write(write) => match write {
                        Ok(count) => {
                            let advanced = pending
                                .as_mut()
                                .ok_or(InteractiveError::Agent)
                                .and_then(|frame| frame.advance(count));
                            if let Err(error) = advanced {
                                latch_active_failure(
                                    &mut stop,
                                    &cancellation,
                                    &mut pending,
                                    active.presenter,
                                    error,
                                );
                            }
                        }
                        Err(_) => {
                            latch_active_failure(
                                &mut stop,
                                &cancellation,
                                &mut pending,
                                active.presenter,
                                InteractiveError::Output,
                            );
                        }
                        },
                        UiWork::Envelope(envelope) => {
                            let received = envelope
                        .ok_or(InteractiveError::Agent)
                        .and_then(|envelope| active.joins.receive_envelope(envelope).map_err(Into::into));
                            if let Err(error) = received {
                                latch_active_failure(
                                    &mut stop,
                                    &cancellation,
                                    &mut pending,
                                    active.presenter,
                                    error,
                                );
                            }
                        }
                        UiWork::Event(event) => {
                            let processed = event.ok_or(InteractiveError::Agent).and_then(|event| {
                                process_event(
                                    event,
                                    start_seq,
                                    turn,
                                    active.live,
                                    active.joins,
                                    EventTargets {
                                        pending: &mut pending,
                                        frame_deadline: &mut frame_deadline,
                                        after_frame: &mut after_frame,
                                        approval_ui: &mut approval_ui,
                                        turn_end_seen: &mut turn_end_seen,
                                        prompt_committed: active.prompt_committed,
                                        expected_prompt: &active.prompt,
                                        render_committed_prompt: active.enhanced,
                                    },
                                )
                            });
                            if let Err(error) = processed {
                                latch_active_failure(
                                    &mut stop,
                                    &cancellation,
                                    &mut pending,
                                    active.presenter,
                                    error,
                                );
                            }
                        }
                        UiWork::Read(read) => match read {
                        Ok(0) => {
                            stop = Some(StopIntent::Eof);
                            cancellation.cancel();
                            discard_pending(&mut pending, active.presenter);
                        }
                        Ok(count) => {
                            tokio::task::yield_now().await;
                            drain_active_signals(active.signals, &mut stop);
                            latch_observer_fault(active.events, &mut stop, &cancellation);
                            if stop.is_some() {
                                cancellation.cancel();
                                discard_pending(&mut pending, active.presenter);
                            } else if approval_ui.is_accepting() {
                                let handled = active
                                    .joins
                                    .question()
                                    .ok_or(InteractiveError::Agent)
                                    .and_then(|question| {
                                        approval_ui.feed(
                                            &active.scratch[..count],
                                            question.challenge(),
                                            active.color,
                                            active.enhanced,
                                        )
                                    })
                                    .and_then(|update| {
                                        dispatch_approval_update(
                                            update,
                                            active.joins,
                                            active.parser,
                                            &approval_ui,
                                            active.enhanced,
                                            &mut pending,
                                            &mut frame_deadline,
                                            &mut after_frame,
                                        )
                                    });
                                match handled {
                                    Ok(false) => {}
                                    Ok(true) => {
                                        stop = Some(StopIntent::Eof);
                                        cancellation.cancel();
                                        discard_pending(&mut pending, active.presenter);
                                    }
                                    Err(error) => latch_active_failure(
                                        &mut stop,
                                        &cancellation,
                                        &mut pending,
                                        active.presenter,
                                        error,
                                    ),
                                }
                            } else if approval_ui.is_inactive() {
                                let input_outcome = handle_active_input(
                                    active.terminal,
                                    active.parser,
                                    active.enhanced_decoder.as_deref_mut(),
                                    active.queued_input.as_deref_mut(),
                                    active.queue_notice.as_deref_mut(),
                                    &active.scratch[..count],
                                    count < TERMINAL_READ_BYTES,
                                )?;
                                match input_outcome {
                                    ActiveInputOutcome::Continue => {}
                                    ActiveInputOutcome::Eof => {
                                        stop = Some(StopIntent::Eof);
                                        cancellation.cancel();
                                        discard_pending(&mut pending, active.presenter);
                                    }
                                    ActiveInputOutcome::Redraw
                                    | ActiveInputOutcome::PasteFence => {
                                        let paste_fence =
                                            input_outcome == ActiveInputOutcome::PasteFence;
                                        if pending.is_some() {
                                            dock_redraw_requested = true;
                                        } else if let (Some(input), Some(dock)) = (
                                            active.queued_input.as_deref(),
                                            active.active_dock.as_mut(),
                                        ) {
                                            match render_active_dock(
                                                input,
                                                active
                                                    .queue_notice
                                                    .as_deref()
                                                    .and_then(Option::as_deref),
                                                DockInteraction::Running,
                                                active.terminal,
                                                active.signals,
                                                dock,
                                            )
                                            .await
                                            {
                                                Ok(Some(signal)) => {
                                                    observe_signal(&mut stop, signal);
                                                    cancellation.cancel();
                                                    discard_pending(
                                                        &mut pending,
                                                        active.presenter,
                                                    );
                                                }
                                                Ok(None) => {}
                                                Err(error) => latch_active_failure(
                                                    &mut stop,
                                                    &cancellation,
                                                    &mut pending,
                                                    active.presenter,
                                                    error,
                                                ),
                                            }
                                        }
                                        if paste_fence && stop.is_none() {
                                            match complete_paste_input_fence(
                                                active.terminal,
                                                active.signals,
                                                active.scratch,
                                            )
                                            .await?
                                            {
                                                PasteFenceOutcome::Ready => {
                                                    active
                                                        .enhanced_decoder
                                                        .as_deref_mut()
                                                        .ok_or(InteractiveError::Agent)?
                                                        .reset_epoch()
                                                        .map_err(|_| InteractiveError::Agent)?;
                                                    *active
                                                        .queue_notice
                                                        .as_deref_mut()
                                                        .ok_or(InteractiveError::Agent)? =
                                                        Some("Paste ready · Enter sends".to_owned());
                                                    dock_redraw_requested = true;
                                                }
                                                PasteFenceOutcome::Signal(signal) => {
                                                    observe_signal(&mut stop, signal);
                                                    cancellation.cancel();
                                                    discard_pending(
                                                        &mut pending,
                                                        active.presenter,
                                                    );
                                                }
                                                PasteFenceOutcome::Eof => {
                                                    stop = Some(StopIntent::Eof);
                                                    cancellation.cancel();
                                                    discard_pending(
                                                        &mut pending,
                                                        active.presenter,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                approval_ui.observe_unaccepted_input();
                            }
                        }
                        Err(_) => {
                            observe_failure(&mut stop, InteractiveError::TerminalUnavailable);
                            cancellation.cancel();
                            discard_pending(&mut pending, active.presenter);
                        }
                        },
                    }
                }
            }
        }
    };

    tokio::task::yield_now().await;
    drain_active_signals(active.signals, &mut stop);
    if latch_observer_fault(active.events, &mut stop, &cancellation) {
        discard_pending(&mut pending, active.presenter);
    }
    if let Err(error) = approval_ui.restore() {
        observe_failure(&mut stop, error);
    }
    let session_capacity_exhausted = match &result {
        Ok(outcome) => turn_exhausted_session_capacity(outcome.reason()),
        Err(_) => false,
    };
    match &result {
        Ok(outcome) if outcome.turn() == turn => {
            if outcome.turn_end_seq().get() < start_seq.get() {
                observe_failure(&mut stop, InteractiveError::Agent);
            }
        }
        Ok(_) => observe_failure(&mut stop, InteractiveError::Agent),
        Err(error) => observe_failure(
            &mut stop,
            storage_failure::from_agent(error)
                .map_or(InteractiveError::Agent, InteractiveError::Storage),
        ),
    }

    if stop.is_none() {
        let final_deadline = Instant::now() + FRAME_DEADLINE;
        loop {
            drain_active_signals(active.signals, &mut stop);
            if latch_observer_fault(active.events, &mut stop, &cancellation) {
                discard_pending(&mut pending, active.presenter);
            }
            if stop.is_some() {
                discard_pending(&mut pending, active.presenter);
                break;
            }
            match reconcile_active_geometry(
                active.terminal,
                active.signals,
                active.queued_input.as_deref(),
                active.queue_notice.as_deref().and_then(Option::as_deref),
                approval_ui.dock_interaction(),
                active.active_dock.as_mut(),
                &mut pending,
                active.enhanced_presenter.as_deref_mut(),
            )
            .await
            {
                Ok(Some(signal)) => {
                    observe_signal(&mut stop, signal);
                    discard_pending(&mut pending, active.presenter);
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    observe_failure(&mut stop, error);
                    discard_pending(&mut pending, active.presenter);
                    continue;
                }
            }
            if let Err(error) = complete_ready_frame(
                &mut pending,
                &mut frame_deadline,
                &mut after_frame,
                &mut approval_ui,
                &mut turn_end_rendered,
                active.presenter,
                active.terminal,
                active.parser,
                active.enhanced,
                active.enhanced_presenter.as_deref_mut(),
                active.queued_input.as_deref(),
                active.queue_notice.as_deref().and_then(Option::as_deref),
                active.active_dock.as_mut(),
            ) {
                observe_failure(&mut stop, error);
                discard_pending(&mut pending, active.presenter);
                continue;
            }
            if pending.is_none() && mem::take(&mut dock_redraw_requested) {
                match redraw_active_after_resize(
                    active.enhanced,
                    active.queued_input.as_deref(),
                    active.queue_notice.as_deref().and_then(Option::as_deref),
                    active.terminal,
                    active.signals,
                    active.active_dock.as_mut(),
                )
                .await
                {
                    Ok(Some(signal)) => observe_signal(&mut stop, signal),
                    Ok(None) => {}
                    Err(error) => observe_failure(&mut stop, error),
                }
                if stop.is_some() {
                    continue;
                }
            }
            if turn_end_seen && pending.is_none() {
                break;
            }
            if pending
                .as_ref()
                .is_some_and(|frame| frame.bytes().is_empty())
            {
                continue;
            }
            let work = next_ui_work(
                active.terminal,
                active.approvals,
                active.events,
                active.scratch,
                pending.as_ref(),
                frame_deadline,
                approval_ui.arm_deadline(),
                approval_ui.escape_deadline(),
                !(pending.is_some() && approval_ui.suppresses_read_while_pending()),
                prefer_input,
            );
            tokio::select! {
                biased;
                signal = next_turn_signal(active.signals, active.enhanced) => {
                    match signal {
                        InteractiveSignal::Stop(signal) => {
                            observe_signal(&mut stop, signal);
                            discard_pending(&mut pending, active.presenter);
                        }
                        InteractiveSignal::Resize => {
                            match reconcile_active_geometry(
                                active.terminal,
                                active.signals,
                                active.queued_input.as_deref(),
                                active
                                    .queue_notice
                                    .as_deref()
                                    .and_then(Option::as_deref),
                                approval_ui.dock_interaction(),
                                active.active_dock.as_mut(),
                                &mut pending,
                                active.enhanced_presenter.as_deref_mut(),
                            )
                            .await
                            {
                                Ok(Some(signal)) => {
                                    observe_signal(&mut stop, signal);
                                    discard_pending(&mut pending, active.presenter);
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    observe_failure(&mut stop, error);
                                    discard_pending(&mut pending, active.presenter);
                                }
                            }
                        }
                    }
                }
                () = tokio::time::sleep_until(final_deadline) => {
                    observe_failure(&mut stop, InteractiveError::Output);
                    discard_pending(&mut pending, active.presenter);
                }
                work = work => {
                    prefer_input = !prefer_input;
                    match work {
                        UiWork::FrameExpired => {
                            observe_failure(&mut stop, InteractiveError::Output);
                            discard_pending(&mut pending, active.presenter);
                        }
                        UiWork::ApprovalArmed => {
                            let prepared = approval_ui
                                .begin_rendering(active.terminal, active.color, active.enhanced)
                                .and_then(|output| {
                                    enqueue_approval_selector_surface(
                                        output,
                                        AfterFrame::ApprovalAccepting,
                                        &approval_ui,
                                        active.enhanced,
                                        &mut pending,
                                        &mut frame_deadline,
                                        &mut after_frame,
                                    )
                                });
                            if let Err(error) = prepared {
                                observe_failure(&mut stop, error);
                                discard_pending(&mut pending, active.presenter);
                            }
                        }
                        UiWork::EscapeExpired => {
                            let handled = approval_ui.expire_escape().and_then(|update| {
                                dispatch_approval_update(
                                    update,
                                    active.joins,
                                    active.parser,
                                    &approval_ui,
                                    active.enhanced,
                                    &mut pending,
                                    &mut frame_deadline,
                                    &mut after_frame,
                                )
                            });
                            match handled {
                                Ok(false) => {}
                                Ok(true) => {
                                    stop = Some(StopIntent::Eof);
                                    discard_pending(&mut pending, active.presenter);
                                }
                                Err(error) => {
                                    observe_failure(&mut stop, error);
                                    discard_pending(&mut pending, active.presenter);
                                }
                            }
                        }
                        UiWork::Write(write) => match write {
                            Ok(count) => {
                                let advanced = pending
                                    .as_mut()
                                    .ok_or(InteractiveError::Agent)
                                    .and_then(|frame| frame.advance(count));
                                if let Err(error) = advanced {
                                    observe_failure(&mut stop, error);
                                    discard_pending(&mut pending, active.presenter);
                                }
                            }
                            Err(_) => {
                                observe_failure(&mut stop, InteractiveError::Output);
                                discard_pending(&mut pending, active.presenter);
                            }
                        },
                        UiWork::Envelope(envelope) => {
                            let received = envelope
                                .ok_or(InteractiveError::Agent)
                                .and_then(|envelope| {
                                    active.joins.receive_envelope(envelope).map_err(Into::into)
                                });
                            if let Err(error) = received {
                                observe_failure(&mut stop, error);
                                discard_pending(&mut pending, active.presenter);
                            }
                        }
                        UiWork::Event(event) => {
                            let processed = event.ok_or(InteractiveError::Agent).and_then(|event| {
                                process_event(
                                    event,
                                    start_seq,
                                    turn,
                                    active.live,
                                    active.joins,
                                    EventTargets {
                                        pending: &mut pending,
                                        frame_deadline: &mut frame_deadline,
                                        after_frame: &mut after_frame,
                                        approval_ui: &mut approval_ui,
                                        turn_end_seen: &mut turn_end_seen,
                                        prompt_committed: active.prompt_committed,
                                        expected_prompt: &active.prompt,
                                        render_committed_prompt: active.enhanced,
                                    },
                                )
                            });
                            if let Err(error) = processed {
                                observe_failure(&mut stop, error);
                                discard_pending(&mut pending, active.presenter);
                            }
                        }
                        UiWork::Read(Ok(0)) => {
                            stop = Some(StopIntent::Eof);
                            discard_pending(&mut pending, active.presenter);
                        }
                        UiWork::Read(Ok(count)) => {
                            if approval_ui.is_accepting() {
                                let handled = active
                                    .joins
                                    .question()
                                    .ok_or(InteractiveError::Agent)
                                    .and_then(|question| {
                                        approval_ui.feed(
                                            &active.scratch[..count],
                                            question.challenge(),
                                            active.color,
                                            active.enhanced,
                                        )
                                    })
                                    .and_then(|update| {
                                        dispatch_approval_update(
                                            update,
                                            active.joins,
                                            active.parser,
                                            &approval_ui,
                                            active.enhanced,
                                            &mut pending,
                                            &mut frame_deadline,
                                            &mut after_frame,
                                        )
                                    });
                                match handled {
                                    Ok(false) => {}
                                    Ok(true) => {
                                        stop = Some(StopIntent::Eof);
                                        discard_pending(&mut pending, active.presenter);
                                    }
                                    Err(error) => {
                                        observe_failure(&mut stop, error);
                                        discard_pending(&mut pending, active.presenter);
                                    }
                                }
                            } else if approval_ui.is_inactive() {
                                let input_outcome = handle_active_input(
                                    active.terminal,
                                    active.parser,
                                    active.enhanced_decoder.as_deref_mut(),
                                    active.queued_input.as_deref_mut(),
                                    active.queue_notice.as_deref_mut(),
                                    &active.scratch[..count],
                                    count < TERMINAL_READ_BYTES,
                                )?;
                                match input_outcome {
                                    ActiveInputOutcome::Continue => {}
                                    ActiveInputOutcome::Eof => {
                                        stop = Some(StopIntent::Eof);
                                        discard_pending(&mut pending, active.presenter);
                                    }
                                    ActiveInputOutcome::Redraw
                                    | ActiveInputOutcome::PasteFence => {
                                        let paste_fence =
                                            input_outcome == ActiveInputOutcome::PasteFence;
                                        if pending.is_some() {
                                            dock_redraw_requested = true;
                                        } else if let (Some(input), Some(dock)) = (
                                            active.queued_input.as_deref(),
                                            active.active_dock.as_mut(),
                                        ) {
                                            match render_active_dock(
                                                input,
                                                active
                                                    .queue_notice
                                                    .as_deref()
                                                    .and_then(Option::as_deref),
                                                DockInteraction::Running,
                                                active.terminal,
                                                active.signals,
                                                dock,
                                            )
                                            .await
                                            {
                                                Ok(Some(signal)) => {
                                                    observe_signal(&mut stop, signal);
                                                    discard_pending(
                                                        &mut pending,
                                                        active.presenter,
                                                    );
                                                }
                                                Ok(None) => {}
                                                Err(error) => {
                                                    observe_failure(&mut stop, error);
                                                    discard_pending(
                                                        &mut pending,
                                                        active.presenter,
                                                    );
                                                }
                                            }
                                        }
                                        if paste_fence && stop.is_none() {
                                            match complete_paste_input_fence(
                                                active.terminal,
                                                active.signals,
                                                active.scratch,
                                            )
                                            .await?
                                            {
                                                PasteFenceOutcome::Ready => {
                                                    active
                                                        .enhanced_decoder
                                                        .as_deref_mut()
                                                        .ok_or(InteractiveError::Agent)?
                                                        .reset_epoch()
                                                        .map_err(|_| InteractiveError::Agent)?;
                                                    *active
                                                        .queue_notice
                                                        .as_deref_mut()
                                                        .ok_or(InteractiveError::Agent)? =
                                                        Some("Paste ready · Enter sends".to_owned());
                                                    dock_redraw_requested = true;
                                                }
                                                PasteFenceOutcome::Signal(signal) => {
                                                    observe_signal(&mut stop, signal);
                                                    discard_pending(
                                                        &mut pending,
                                                        active.presenter,
                                                    );
                                                }
                                                PasteFenceOutcome::Eof => {
                                                    stop = Some(StopIntent::Eof);
                                                    discard_pending(
                                                        &mut pending,
                                                        active.presenter,
                                                    );
                                                }
                                            }
                                        }
                                    }
                                }
                            } else {
                                approval_ui.observe_unaccepted_input();
                            }
                        }
                        UiWork::Read(Err(_)) => {
                            observe_failure(&mut stop, InteractiveError::TerminalUnavailable);
                            discard_pending(&mut pending, active.presenter);
                        }
                    }
                }
            }
        }
    }

    drain_active_signals(active.signals, &mut stop);
    if latch_observer_fault(active.events, &mut stop, &cancellation) {
        discard_pending(&mut pending, active.presenter);
    }
    if let Err(error) = approval_ui.restore() {
        observe_failure(&mut stop, error);
    }

    let mut skipped = 0_usize;
    if stop.is_some() {
        skipped = discard_ready_updates_after_stop(
            active.events,
            start_seq,
            &active.prompt,
            active.prompt_committed,
        )?;
        active
            .joins
            .finish_turn(active.approvals, ApprovalResetMode::Discard)?;
    } else {
        active
            .joins
            .finish_turn(active.approvals, ApprovalResetMode::Normal)?;
    }

    let disposition = finish_turn_disposition(
        stop,
        skipped,
        turn_end_rendered,
        active.presenter,
        active.terminal,
        active.signals,
        active.enhanced,
        active.queued_input.as_deref(),
        active
            .queue_notice
            .as_deref()
            .and_then(|notice| notice.as_deref()),
        active.enhanced_presenter.as_deref_mut(),
        active.active_dock.as_mut(),
    )
    .await?;
    if session_capacity_exhausted && disposition == TurnDisposition::Continue {
        Ok(TurnDisposition::Exit(1))
    } else {
        Ok(disposition)
    }
}

fn turn_exhausted_session_capacity(reason: &TurnEndReason) -> bool {
    matches!(reason, TurnEndReason::Error { error } if error.code() == "AGENT_EVENT_BUDGET")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActiveInputOutcome {
    Continue,
    Redraw,
    PasteFence,
    Eof,
}

fn handle_active_input(
    terminal: &AsyncTerminal,
    _parser: &mut CanonicalRecordParser,
    decoder: Option<&mut KeyDecoder>,
    input: Option<&mut InputMemory>,
    notice: Option<&mut Option<String>>,
    bytes: &[u8],
    _boundary: bool,
) -> Result<ActiveInputOutcome, InteractiveError> {
    let (Some(decoder), Some(input), Some(notice)) = (decoder, input, notice) else {
        return Ok(ActiveInputOutcome::Continue);
    };
    let size = terminal.size().unwrap_or(TerminalSize {
        rows: MIN_ENHANCED_ROWS,
        columns: MIN_ENHANCED_COLUMNS,
    });
    match apply_enhanced_input(decoder, bytes, input, size, notice)? {
        EnhancedInputAction::None => Ok(ActiveInputOutcome::Continue),
        EnhancedInputAction::Redraw => Ok(ActiveInputOutcome::Redraw),
        EnhancedInputAction::PasteFence => Ok(ActiveInputOutcome::PasteFence),
        EnhancedInputAction::Exit => Ok(ActiveInputOutcome::Eof),
        EnhancedInputAction::Submit => {
            match classify_enhanced_submission(input.composer().text()) {
                EnhancedSubmission::Exit => return Ok(ActiveInputOutcome::Eof),
                EnhancedSubmission::Help => {
                    *notice =
                        Some("/help | /exit | /quit | Enter queue | Ctrl+J newline".to_owned());
                    return Ok(ActiveInputOutcome::Redraw);
                }
                EnhancedSubmission::Empty => {
                    *notice = Some("Type a prompt before queueing the next turn".to_owned());
                    return Ok(ActiveInputOutcome::Redraw);
                }
                EnhancedSubmission::Prompt => {}
            }
            match input.enqueue_draft() {
                Ok(_) => {
                    *notice = Some(format!(
                        "{} next-turn prompt(s) queued",
                        input.queue().len()
                    ));
                }
                Err(error) => {
                    *notice = Some(format!("{error} · draft kept"));
                }
            }
            Ok(ActiveInputOutcome::Redraw)
        }
    }
}

fn process_event(
    event: crate::session::CommittedUiEvent,
    expected_start: crate::session::EventSeq,
    expected_turn: TurnId,
    live: &mut LiveRenderer,
    joins: &mut ApprovalJoin,
    targets: EventTargets<'_, '_>,
) -> Result<(), InteractiveError> {
    if event.seq.get() < expected_start.get() {
        return Err(InteractiveError::Agent);
    }
    let committed_prompt = match &event.kind {
        CommittedUiKind::UserMessage {
            source: UiUserSource::Human,
            content,
        } => {
            let content = content.as_str().ok_or(InteractiveError::Agent)?;
            if content != targets.expected_prompt {
                return Err(InteractiveError::Agent);
            }
            targets
                .render_committed_prompt
                .then(|| copy_enhanced_prompt(content))
                .transpose()?
        }
        _ => None,
    };
    if committed_prompt.is_some()
        || matches!(
            &event.kind,
            CommittedUiKind::UserMessage {
                source: UiUserSource::Human,
                ..
            }
        )
    {
        *targets.prompt_committed = true;
    }
    let update = live.consume(event).map_err(|_| InteractiveError::Output)?;
    let mut frame_after = AfterFrame::None;
    match update.lifecycle {
        LiveLifecycle::None => {}
        LiveLifecycle::ApprovalAsked {
            id,
            tool_name,
            call_id,
            reason,
        } => joins.observe_asked(id, tool_name, call_id, reason)?,
        LiveLifecycle::ApprovalDecided { id, outcome } => {
            targets.approval_ui.restore()?;
            joins.observe_decided(id, outcome)?;
        }
        LiveLifecycle::TurnEnded { turn } => {
            if turn != expected_turn {
                return Err(InteractiveError::Agent);
            }
            joins.observe_turn_end()?;
            *targets.turn_end_seen = true;
            frame_after = AfterFrame::TurnEnd;
        }
    }
    let frame = match (committed_prompt, update.frame) {
        (Some(prompt), None) => {
            Some(LiveFrame::human_message(prompt).map_err(|_| InteractiveError::Output)?)
        }
        (None, frame) => frame,
        (Some(_), Some(_)) => return Err(InteractiveError::Agent),
    };
    if let Some(frame) = frame {
        enqueue_frame(
            frame,
            frame_after,
            targets.pending,
            targets.frame_deadline,
            targets.after_frame,
        )?;
    }
    Ok(())
}

struct EventTargets<'a, 'terminal> {
    pending: &'a mut Option<PendingOutput>,
    frame_deadline: &'a mut Option<Instant>,
    after_frame: &'a mut AfterFrame,
    approval_ui: &'a mut ApprovalUiState<'terminal>,
    turn_end_seen: &'a mut bool,
    prompt_committed: &'a mut bool,
    expected_prompt: &'a str,
    render_committed_prompt: bool,
}

fn apply_approval_update(
    update: ApprovalUiUpdate,
    joins: &mut ApprovalJoin,
    parser: &mut CanonicalRecordParser,
    pending: &mut Option<PendingOutput>,
    frame_deadline: &mut Option<Instant>,
    after_frame: &mut AfterFrame,
) -> Result<bool, InteractiveError> {
    match update {
        ApprovalUiUpdate::None => {}
        ApprovalUiUpdate::Redraw(output) => {
            let frame =
                LiveFrame::approval_selector(output).map_err(|_| InteractiveError::Output)?;
            enqueue_frame(
                frame,
                AfterFrame::None,
                pending,
                frame_deadline,
                after_frame,
            )?;
        }
        ApprovalUiUpdate::Decide(outcome) => {
            joins.answer(outcome)?;
            parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
        }
        ApprovalUiUpdate::Eof => return Ok(true),
        ApprovalUiUpdate::Invalid => {
            let frame = approval_frame(joins, true)?;
            enqueue_frame(
                frame,
                AfterFrame::ApprovalFence,
                pending,
                frame_deadline,
                after_frame,
            )?;
            parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
        }
    }
    Ok(false)
}

fn approval_frame(joins: &ApprovalJoin, retry: bool) -> Result<LiveFrame, InteractiveError> {
    let question = joins.question().ok_or(InteractiveError::Agent)?;
    let frame = LiveFrame::approval(
        question.tool_name(),
        question.call_id(),
        question.reason(),
        question.preview(),
        retry,
    )
    .map_err(|_| InteractiveError::Output)?;
    Ok(frame)
}

fn enqueue_frame(
    frame: LiveFrame,
    after: AfterFrame,
    pending: &mut Option<PendingOutput>,
    deadline: &mut Option<Instant>,
    pending_after: &mut AfterFrame,
) -> Result<(), InteractiveError> {
    if pending.is_some() {
        return Err(InteractiveError::Agent);
    }
    *pending = Some(PendingOutput::Unprepared(frame));
    *deadline = Some(Instant::now() + FRAME_DEADLINE);
    *pending_after = after;
    Ok(())
}

fn enqueue_enhanced_dock(
    interaction: DockInteraction,
    after: AfterFrame,
    pending: &mut Option<PendingOutput>,
    deadline: &mut Option<Instant>,
    pending_after: &mut AfterFrame,
) -> Result<(), InteractiveError> {
    if pending.is_some() {
        return Err(InteractiveError::Agent);
    }
    *pending = Some(PendingOutput::Dock(interaction));
    *deadline = Some(Instant::now() + FRAME_DEADLINE);
    *pending_after = after;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn enqueue_approval_selector_surface(
    output: String,
    after: AfterFrame,
    approval_ui: &ApprovalUiState<'_>,
    enhanced: bool,
    pending: &mut Option<PendingOutput>,
    deadline: &mut Option<Instant>,
    pending_after: &mut AfterFrame,
) -> Result<(), InteractiveError> {
    if enhanced {
        enqueue_enhanced_dock(
            DockInteraction::Approval(approval_ui.dock_selection()?),
            after,
            pending,
            deadline,
            pending_after,
        )
    } else {
        let frame = LiveFrame::approval_selector(output).map_err(|_| InteractiveError::Output)?;
        enqueue_frame(frame, after, pending, deadline, pending_after)
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_approval_update(
    update: ApprovalUiUpdate,
    joins: &mut ApprovalJoin,
    parser: &mut CanonicalRecordParser,
    approval_ui: &ApprovalUiState<'_>,
    enhanced: bool,
    pending: &mut Option<PendingOutput>,
    frame_deadline: &mut Option<Instant>,
    after_frame: &mut AfterFrame,
) -> Result<bool, InteractiveError> {
    if !enhanced {
        return apply_approval_update(update, joins, parser, pending, frame_deadline, after_frame);
    }
    match update {
        ApprovalUiUpdate::None => {}
        ApprovalUiUpdate::Redraw(output) => enqueue_approval_selector_surface(
            output,
            AfterFrame::None,
            approval_ui,
            true,
            pending,
            frame_deadline,
            after_frame,
        )?,
        ApprovalUiUpdate::Decide(outcome) => {
            joins.answer(outcome)?;
            parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
        }
        ApprovalUiUpdate::Eof => return Ok(true),
        ApprovalUiUpdate::Invalid => {
            enqueue_enhanced_dock(
                DockInteraction::Running,
                AfterFrame::ApprovalFence,
                pending,
                frame_deadline,
                after_frame,
            )?;
            parser.reset(MAX_INTERACTIVE_PROMPT_BYTES);
        }
    }
    Ok(false)
}

#[allow(clippy::too_many_arguments)]
fn complete_ready_frame(
    pending: &mut Option<PendingOutput>,
    deadline: &mut Option<Instant>,
    after: &mut AfterFrame,
    approval_ui: &mut ApprovalUiState<'_>,
    turn_end_rendered: &mut bool,
    presenter: &mut InteractivePresenter,
    terminal: &AsyncTerminal,
    parser: &mut CanonicalRecordParser,
    enhanced: bool,
    mut enhanced_presenter: Option<&mut EnhancedPresenter>,
    input: Option<&InputMemory>,
    notice: Option<&str>,
    mut active_dock: Option<&mut ActiveDock<'_>>,
) -> Result<(), InteractiveError> {
    if matches!(pending, Some(PendingOutput::Unprepared(_))) {
        let frame = match pending.take() {
            Some(PendingOutput::Unprepared(frame)) => frame,
            _ => return Err(InteractiveError::Agent),
        };
        *pending = Some(if enhanced {
            let presenter = enhanced_presenter
                .as_deref_mut()
                .ok_or(InteractiveError::Agent)?;
            PendingOutput::Prepared(
                presenter
                    .prepare(frame)
                    .map_err(|_| InteractiveError::Output)?,
            )
        } else {
            PendingOutput::Linear(frame.into_pending().map_err(|_| InteractiveError::Output)?)
        });
    }
    if matches!(pending, Some(PendingOutput::Prepared(_))) {
        let presentation = match pending.take() {
            Some(PendingOutput::Prepared(presentation)) => presentation,
            _ => return Err(InteractiveError::Agent),
        };
        let staged = (|| {
            let input = input.ok_or(InteractiveError::Agent)?;
            let dock = active_dock.as_deref_mut().ok_or(InteractiveError::Agent)?;
            let size = terminal.size().unwrap_or(*dock.last_size);
            if size != *dock.last_size {
                return Err(InteractiveError::TerminalUnsupported);
            }
            let dock_frame = enhanced_dock_frame(input, notice, DockInteraction::Running, size)?;
            let write = dock
                .screen
                .stage_transcript(presentation.chunk(), &dock_frame, true)
                .map_err(map_inline_screen_error)?;
            Ok(PendingOutput::Inline(PendingInlineOutput {
                write,
                intent: InlineIntent::Transcript(presentation),
            }))
        })();
        match staged {
            Ok(staged) => *pending = Some(staged),
            Err(error) => return Err(error),
        }
    }
    if matches!(pending, Some(PendingOutput::Dock(_))) {
        let interaction = match pending.take() {
            Some(PendingOutput::Dock(interaction)) => interaction,
            _ => return Err(InteractiveError::Agent),
        };
        let input = input.ok_or(InteractiveError::Agent)?;
        let dock = active_dock.as_deref_mut().ok_or(InteractiveError::Agent)?;
        let size = terminal.size().unwrap_or(*dock.last_size);
        if size != *dock.last_size {
            return Err(InteractiveError::TerminalUnsupported);
        }
        let dock_frame = enhanced_dock_frame(input, notice, interaction, size)?;
        let write = dock
            .screen
            .stage_dock(&dock_frame, true)
            .map_err(map_inline_screen_error)?;
        *pending = Some(PendingOutput::Inline(PendingInlineOutput {
            write,
            intent: InlineIntent::Dock(interaction),
        }));
    }
    let Some(frame) = pending.as_mut() else {
        return Ok(());
    };
    match frame {
        PendingOutput::Unprepared(_) | PendingOutput::Prepared(_) | PendingOutput::Dock(_) => {
            return Err(InteractiveError::Agent);
        }
        PendingOutput::Linear(frame) => {
            if frame
                .prepare_next(presenter)
                .map_err(|_| InteractiveError::Output)?
            {
                return Ok(());
            }
        }
        PendingOutput::Inline(output) => {
            if !output.write.is_complete() {
                return Ok(());
            }
            let output = match pending.take() {
                Some(PendingOutput::Inline(output)) => output,
                _ => return Err(InteractiveError::Agent),
            };
            active_dock
                .ok_or(InteractiveError::Agent)?
                .screen
                .commit(output.write)
                .map_err(map_inline_screen_error)?;
            if let InlineIntent::Transcript(presentation) = output.intent {
                enhanced_presenter
                    .ok_or(InteractiveError::Agent)?
                    .commit(presentation);
            }
        }
    }
    *pending = None;
    *deadline = None;
    match mem::take(after) {
        AfterFrame::None => {}
        AfterFrame::ApprovalFence => {
            if enhanced {
                terminal.revalidate_identity()?;
            } else {
                terminal.revalidate()?;
            }
            terminal.flush_input()?;
            parser.reset(MAX_APPROVAL_RECORD_BYTES);
            approval_ui.begin_arming()?;
        }
        AfterFrame::ApprovalAccepting => {
            parser.reset(MAX_APPROVAL_RECORD_BYTES);
            approval_ui.accept_rendered(terminal)?;
        }
        AfterFrame::TurnEnd => *turn_end_rendered = true,
    }
    Ok(())
}

fn discard_pending(pending: &mut Option<PendingOutput>, presenter: &mut InteractivePresenter) {
    if pending.take().is_some() {
        presenter.discard_partly_written_frame();
    }
}

fn latch_active_failure(
    stop: &mut Option<StopIntent>,
    cancellation: &CancellationToken,
    pending: &mut Option<PendingOutput>,
    presenter: &mut InteractivePresenter,
    error: InteractiveError,
) {
    observe_failure(stop, error);
    cancellation.cancel();
    discard_pending(pending, presenter);
}

enum UiWork {
    FrameExpired,
    ApprovalArmed,
    EscapeExpired,
    Write(std::io::Result<usize>),
    Envelope(Option<ApprovalEnvelope>),
    Event(Option<crate::session::CommittedUiEvent>),
    Read(std::io::Result<usize>),
}

#[allow(clippy::too_many_arguments)]
async fn next_ui_work(
    terminal: &AsyncTerminal,
    approvals: &mut ApprovalEnvelopeReceiver,
    events: &mut CommittedUiReceiver,
    scratch: &mut [u8; TERMINAL_READ_BYTES],
    pending: Option<&PendingOutput>,
    frame_deadline: Option<Instant>,
    approval_arm_deadline: Option<Instant>,
    escape_deadline: Option<Instant>,
    read_enabled: bool,
    prefer_input: bool,
) -> UiWork {
    let deadline = frame_deadline.unwrap_or_else(Instant::now);
    let arm_deadline = approval_arm_deadline.unwrap_or_else(Instant::now);
    let escape_pending = escape_deadline.is_some();
    let escape_deadline_at = escape_deadline.unwrap_or_else(Instant::now);
    if prefer_input {
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline), if pending.is_some() => UiWork::FrameExpired,
            () = tokio::time::sleep_until(arm_deadline), if approval_arm_deadline.is_some() => UiWork::ApprovalArmed,
            () = tokio::time::sleep_until(escape_deadline_at), if escape_pending => UiWork::EscapeExpired,
            read = terminal.read_once(scratch), if read_enabled => UiWork::Read(read),
            write = write_pending(terminal, pending), if pending.is_some() => UiWork::Write(write),
            envelope = approvals.recv() => UiWork::Envelope(envelope),
            event = events.recv(), if pending.is_none() => UiWork::Event(event),
        }
    } else {
        tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline), if pending.is_some() => UiWork::FrameExpired,
            () = tokio::time::sleep_until(arm_deadline), if approval_arm_deadline.is_some() => UiWork::ApprovalArmed,
            () = tokio::time::sleep_until(escape_deadline_at), if escape_pending => UiWork::EscapeExpired,
            write = write_pending(terminal, pending), if pending.is_some() => UiWork::Write(write),
            envelope = approvals.recv() => UiWork::Envelope(envelope),
            event = events.recv(), if pending.is_none() => UiWork::Event(event),
            read = terminal.read_once(scratch), if read_enabled => UiWork::Read(read),
        }
    }
}

async fn write_pending(
    terminal: &AsyncTerminal,
    pending: Option<&PendingOutput>,
) -> std::io::Result<usize> {
    let bytes = pending.map(PendingOutput::bytes).unwrap_or_default();
    terminal.write_once(bytes).await
}

async fn write_frame(
    frame: LiveFrame,
    presenter: &mut InteractivePresenter,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<Option<UiSignal>, InteractiveError> {
    let mut pending = frame.into_pending().map_err(|_| InteractiveError::Output)?;
    let deadline = Instant::now() + FRAME_DEADLINE;
    loop {
        if !pending
            .prepare_next(presenter)
            .map_err(|_| InteractiveError::Output)?
        {
            return Ok(None);
        }
        let work = tokio::select! {
            biased;
            signal = signals.next() => IdleWriteWork::Signal(signal),
            () = tokio::time::sleep_until(deadline) => IdleWriteWork::Expired,
            write = terminal.write_once(pending.bytes()) => IdleWriteWork::Write(write),
        };
        let mut latch = SignalLatch::default();
        if let IdleWriteWork::Signal(signal) = &work {
            latch.observe(DriverMode::Interactive, *signal);
        }
        tokio::task::yield_now().await;
        signals.drain_ready(DriverMode::Interactive, &mut latch);
        match work {
            IdleWriteWork::Signal(_) => {
                presenter.discard_partly_written_frame();
                return Ok(latch.observed());
            }
            IdleWriteWork::Expired | IdleWriteWork::Write(Err(_)) => {
                if let Some(signal @ (UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate)) =
                    latch.observed()
                {
                    presenter.discard_partly_written_frame();
                    return Ok(Some(signal));
                }
                return Err(InteractiveError::Output);
            }
            IdleWriteWork::Write(Ok(count)) => {
                if let Some(signal) = latch.observed() {
                    presenter.discard_partly_written_frame();
                    return Ok(Some(signal));
                }
                pending
                    .advance(count)
                    .map_err(|_| InteractiveError::Output)?;
            }
        }
    }
}

enum IdleWriteWork {
    Signal(UiSignal),
    Expired,
    Write(std::io::Result<usize>),
}

async fn write_notice(
    notice: &'static str,
    presenter: &mut InteractivePresenter,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<Option<UiSignal>, InteractiveError> {
    let frame = LiveFrame::notice(notice).map_err(|_| InteractiveError::Output)?;
    if let Some(signal) = write_frame(frame, presenter, terminal, signals).await? {
        if let Some(signal) = handle_idle_signal(signal, terminal, signals).await? {
            return Ok(Some(signal));
        }
    }
    Ok(None)
}

async fn handle_idle_signal(
    signal: UiSignal,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<Option<UiSignal>, InteractiveError> {
    match signal {
        UiSignal::Interrupt => {
            terminal.flush_input()?;
            Ok(None)
        }
        UiSignal::Suspend => Ok(suspend_and_resume(terminal, signals).await?),
        UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate => Ok(Some(signal)),
    }
}

async fn finish_signal_after_shutdown(
    signal: UiSignal,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<Option<u8>, InteractiveError> {
    match signal {
        UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate => {
            signal.exit_code().map(Some).ok_or(InteractiveError::Agent)
        }
        UiSignal::Suspend => match suspend_and_resume(terminal, signals).await? {
            Some(terminating) => terminating
                .exit_code()
                .map(Some)
                .ok_or(InteractiveError::Agent),
            None => Ok(None),
        },
        UiSignal::Interrupt => Ok(None),
    }
}

pub(super) async fn suspend_and_resume(
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<Option<UiSignal>, TerminalError> {
    loop {
        self_suspend().map_err(|_| TerminalError::Unsupported)?;
        let mut latch = SignalLatch::default();
        signals.drain_ready(DriverMode::Interactive, &mut latch);
        if let Some(signal @ (UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate)) =
            latch.observed()
        {
            return Ok(Some(signal));
        }
        if terminal.is_foreground()? {
            terminal.revalidate()?;
            terminal.flush_input()?;
            return Ok(None);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_turn_disposition(
    stop: Option<StopIntent>,
    skipped: usize,
    turn_end_rendered: bool,
    presenter: &mut InteractivePresenter,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    defer_suspend: bool,
    input: Option<&InputMemory>,
    notice: Option<&str>,
    enhanced_presenter: Option<&mut EnhancedPresenter>,
    active_dock: Option<&mut ActiveDock<'_>>,
) -> Result<TurnDisposition, InteractiveError> {
    match stop {
        None => Ok(TurnDisposition::Continue),
        Some(StopIntent::Interrupt) => {
            if !turn_end_rendered {
                let frame = LiveFrame::stopped(skipped).map_err(|_| InteractiveError::Output)?;
                let signal = if defer_suspend {
                    write_enhanced_terminal_frame(
                        frame,
                        input.ok_or(InteractiveError::Agent)?,
                        notice,
                        enhanced_presenter.ok_or(InteractiveError::Agent)?,
                        terminal,
                        signals,
                        active_dock.ok_or(InteractiveError::Agent)?,
                    )
                    .await?
                } else {
                    write_frame(frame, presenter, terminal, signals).await?
                };
                if let Some(signal) = signal {
                    return finish_signal_after_cleanup(signal, terminal, signals, defer_suspend)
                        .await;
                }
            }
            Ok(TurnDisposition::Continue)
        }
        Some(StopIntent::Eof) => Ok(TurnDisposition::Exit(0)),
        Some(StopIntent::Suspend) if defer_suspend => {
            Ok(TurnDisposition::Signal(UiSignal::Suspend))
        }
        Some(StopIntent::Suspend) => match suspend_and_resume(terminal, signals).await? {
            Some(signal) => Ok(TurnDisposition::Signal(signal)),
            None => Ok(TurnDisposition::Continue),
        },
        Some(StopIntent::Exit(signal)) => Ok(TurnDisposition::Signal(signal)),
        Some(StopIntent::Failure(error)) => Err(error),
    }
}

async fn write_enhanced_terminal_frame(
    frame: LiveFrame,
    input: &InputMemory,
    notice: Option<&str>,
    presenter: &mut EnhancedPresenter,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    dock: &mut ActiveDock<'_>,
) -> Result<Option<UiSignal>, InteractiveError> {
    let mut presentation = presenter
        .prepare(frame)
        .map_err(|_| InteractiveError::Output)?;
    loop {
        let mut boundary_changed = false;
        if dock.screen.is_poisoned() {
            if let Some(signal) = recover_poisoned_screen(terminal, signals, dock.screen).await? {
                return Ok(Some(signal));
            }
            boundary_changed = true;
        }
        let size = terminal.size().unwrap_or(*dock.last_size);
        if dock.screen.is_detached() || size != *dock.last_size {
            if let Some(signal) = render_active_dock(
                input,
                notice,
                DockInteraction::Running,
                terminal,
                signals,
                dock,
            )
            .await?
            {
                return Ok(Some(signal));
            }
            boundary_changed = true;
        }
        if boundary_changed {
            presentation.force_next_line_boundary();
        }

        let size = terminal.size().unwrap_or(*dock.last_size);
        if size != *dock.last_size {
            continue;
        }
        let dock_frame = enhanced_dock_frame(input, notice, DockInteraction::Running, size)?;
        let write = dock
            .screen
            .stage_transcript(presentation.chunk(), &dock_frame, true)
            .map_err(map_inline_screen_error)?;
        match write_screen_transaction(terminal, signals, dock.screen, write).await? {
            ScreenWriteOutcome::Complete => {
                presenter.commit(presentation);
                return Ok(None);
            }
            ScreenWriteOutcome::Signal(signal) => return Ok(Some(signal)),
            ScreenWriteOutcome::Resize | ScreenWriteOutcome::PoisonedResize => continue,
        }
    }
}

async fn finish_signal_after_cleanup(
    signal: UiSignal,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
    defer_suspend: bool,
) -> Result<TurnDisposition, InteractiveError> {
    match signal {
        UiSignal::Interrupt => Ok(TurnDisposition::Continue),
        UiSignal::Suspend if defer_suspend => Ok(TurnDisposition::Signal(UiSignal::Suspend)),
        UiSignal::Suspend => match suspend_and_resume(terminal, signals).await? {
            Some(signal) => Ok(TurnDisposition::Signal(signal)),
            None => Ok(TurnDisposition::Continue),
        },
        UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate => {
            Ok(TurnDisposition::Signal(signal))
        }
    }
}

fn observe_signal(stop: &mut Option<StopIntent>, signal: UiSignal) {
    match signal {
        UiSignal::Hangup | UiSignal::Quit | UiSignal::Terminate => {
            if !matches!(stop, Some(StopIntent::Exit(_))) {
                *stop = Some(StopIntent::Exit(signal));
            }
        }
        UiSignal::Suspend => {
            if stop.is_none() || matches!(stop, Some(StopIntent::Interrupt)) {
                *stop = Some(StopIntent::Suspend);
            }
        }
        UiSignal::Interrupt => {
            if stop.is_none() {
                *stop = Some(StopIntent::Interrupt);
            }
        }
    }
}

fn drain_active_signals(signals: &mut SignalStreams, stop: &mut Option<StopIntent>) {
    // Tokio coalesces each of the five installed signal classes. Bounding the
    // drain prevents a signal flood from starving the owned Agent cleanup.
    for _ in 0..5 {
        let Some(signal) = signals.next().now_or_never() else {
            break;
        };
        observe_signal(stop, signal);
    }
}

fn observe_failure(stop: &mut Option<StopIntent>, error: InteractiveError) {
    if !matches!(stop, Some(StopIntent::Exit(_))) {
        *stop = Some(StopIntent::Failure(error));
    }
}

fn latch_observer_fault(
    events: &CommittedUiReceiver,
    stop: &mut Option<StopIntent>,
    cancellation: &CancellationToken,
) -> bool {
    if !events.is_producer_faulted() {
        return false;
    }
    observe_failure(stop, InteractiveError::Agent);
    cancellation.cancel();
    true
}

fn discard_ready_updates_after_stop(
    events: &mut CommittedUiReceiver,
    expected_start: crate::session::EventSeq,
    expected_prompt: &str,
    prompt_committed: &mut bool,
) -> Result<usize, InteractiveError> {
    let mut skipped = 0_usize;
    while let Ok(event) = events.try_recv() {
        if event.seq.get() < expected_start.get() {
            return Err(InteractiveError::Agent);
        }
        if let CommittedUiKind::UserMessage {
            source: UiUserSource::Human,
            content,
        } = &event.kind
        {
            if content.as_str() != Some(expected_prompt) {
                return Err(InteractiveError::Agent);
            }
            *prompt_committed = true;
        }
        skipped = skipped.saturating_add(1);
    }
    Ok(skipped)
}

#[cfg(test)]
mod tests {
    use super::{
        AfterFrame, ApprovalUiUpdate, InlineIntent, InteractiveError, InteractiveExit,
        InteractivePresentation, PendingInlineOutput, PendingOutput, StopIntent,
        apply_approval_update, discard_ready_updates_after_stop, latch_observer_fault,
        observe_enhanced_cleanup_signal, observe_failure, observe_signal,
        prepare_pending_for_resize, presentation_uses_enhanced, turn_exhausted_session_capacity,
    };
    use crate::{
        agent::{ApprovalPrompt, ApprovalRequest},
        cli::{
            approval::{ApprovalChallengePool, ApprovalEnvelope},
            approval_join::ApprovalJoin,
            input::CanonicalRecordParser,
            signal::{SignalLatch, UiSignal},
            terminal::TerminalSize,
        },
        entropy::{EntropyError, EntropySource},
        model::{CallId, ContentBlock, LlmFailure, Message, MessageSource},
        session::{
            ApprovalOutcome, ApprovalRequestId, EventKind, MAX_SESSION_EVENTS, NewEvent, Session,
            SurfaceIntent, TurnEndReason,
        },
        tui::{
            dock::{DockApprovalSelection, DockInteraction},
            inline_screen::InlineScreen,
            input_memory::InputMemory,
        },
    };
    use tokio::sync::oneshot;

    fn fill(bytes: &mut [u8]) -> Result<(), EntropyError> {
        bytes.fill(0);
        Ok(())
    }

    #[test]
    fn terminating_signals_override_local_stops_but_not_each_other() {
        let mut stop = Some(StopIntent::Interrupt);
        observe_signal(&mut stop, UiSignal::Terminate);
        assert_eq!(stop, Some(StopIntent::Exit(UiSignal::Terminate)));
        observe_signal(&mut stop, UiSignal::Hangup);
        assert_eq!(stop, Some(StopIntent::Exit(UiSignal::Terminate)));
    }

    #[test]
    fn enhanced_startup_has_an_exact_polished_geometry_threshold() {
        for presentation in [
            InteractivePresentation::Auto,
            InteractivePresentation::Enhanced,
        ] {
            assert!(presentation_uses_enhanced(
                presentation,
                Some(TerminalSize {
                    rows: 12,
                    columns: 44,
                })
            ));
            for size in [
                TerminalSize {
                    rows: 12,
                    columns: 43,
                },
                TerminalSize {
                    rows: 11,
                    columns: 44,
                },
            ] {
                assert!(!presentation_uses_enhanced(presentation, Some(size)));
            }
        }
        assert!(!presentation_uses_enhanced(
            InteractivePresentation::Linear,
            Some(TerminalSize {
                rows: 24,
                columns: 120,
            })
        ));
        assert!(!presentation_uses_enhanced(
            InteractivePresentation::Auto,
            None
        ));
    }

    #[test]
    fn partially_written_approval_resize_keeps_selection_after_visual_recovery() {
        let input = InputMemory::default();
        let interaction = DockInteraction::Approval(DockApprovalSelection::AllowOnce);
        let size = TerminalSize {
            rows: 24,
            columns: 80,
        };
        let frame = super::enhanced_dock_frame(&input, None, interaction, size).unwrap();
        let mut screen = InlineScreen::default();
        let mut attach = screen
            .stage_attach(super::screen_size(size), &frame, true)
            .unwrap();
        let attach_bytes = attach.bytes().len();
        attach.advance(attach_bytes).unwrap();
        screen.commit(attach).unwrap();

        let mut write = screen.stage_dock(&frame, true).unwrap();
        write.advance(1).unwrap();
        let mut pending = Some(PendingOutput::Inline(PendingInlineOutput {
            write,
            intent: InlineIntent::Dock(interaction),
        }));
        assert!(prepare_pending_for_resize(&mut pending, &mut screen).unwrap());
        assert!(screen.is_poisoned());
        assert!(matches!(
            pending,
            Some(PendingOutput::Dock(DockInteraction::Approval(
                DockApprovalSelection::AllowOnce
            )))
        ));

        screen.recover_after_visual_reset();
        let compact_size = TerminalSize {
            rows: 6,
            columns: 15,
        };
        let compact = super::enhanced_dock_frame(&input, None, interaction, compact_size).unwrap();
        let mut second_resize = screen
            .stage_attach(super::screen_size(compact_size), &compact, true)
            .unwrap();
        second_resize.advance(1).unwrap();
        screen.abort(second_resize);
        assert!(screen.is_poisoned());
        screen.recover_after_visual_reset();
        assert!(
            screen
                .stage_attach(super::screen_size(compact_size), &compact, true)
                .is_ok()
        );
    }

    #[test]
    fn output_failure_is_preserved_unless_a_terminating_signal_wins() {
        let mut stop = None;
        observe_failure(&mut stop, InteractiveError::Output);
        observe_signal(&mut stop, UiSignal::Interrupt);
        assert_eq!(stop, Some(StopIntent::Failure(InteractiveError::Output)));
        observe_signal(&mut stop, UiSignal::Quit);
        assert_eq!(stop, Some(StopIntent::Exit(UiSignal::Quit)));
    }

    #[test]
    fn terminating_signal_observed_during_visual_reset_overrides_output_failure() {
        let mut result = Err(InteractiveError::Output);
        let mut signals = SignalLatch::default();
        observe_enhanced_cleanup_signal(&mut result, &mut signals, UiSignal::Interrupt);
        assert_eq!(result, Err(InteractiveError::Output));
        observe_enhanced_cleanup_signal(&mut result, &mut signals, UiSignal::Terminate);
        assert_eq!(result, Ok(InteractiveExit::Signal(UiSignal::Terminate)));
        observe_enhanced_cleanup_signal(&mut result, &mut signals, UiSignal::Hangup);
        assert_eq!(result, Ok(InteractiveExit::Signal(UiSignal::Terminate)));
        assert_eq!(signals.observed(), Some(UiSignal::Terminate));

        let mut ordinary = Ok(InteractiveExit::Ordinary(7));
        let mut local_signals = SignalLatch::default();
        observe_enhanced_cleanup_signal(&mut ordinary, &mut local_signals, UiSignal::Interrupt);
        observe_enhanced_cleanup_signal(&mut ordinary, &mut local_signals, UiSignal::Suspend);
        assert_eq!(ordinary, Ok(InteractiveExit::Ordinary(7)));
        assert_eq!(local_signals.observed(), Some(UiSignal::Suspend));
    }

    #[tokio::test]
    async fn invalid_selector_input_rearms_before_a_later_decision() {
        let challenges =
            ApprovalChallengePool::from_entropy(EntropySource::injected(fill)).unwrap();
        let mut joins = ApprovalJoin::new(challenges).unwrap();
        joins.begin_turn().unwrap();
        let request = ApprovalRequest::new(
            ApprovalRequestId::new("approval-hostile"),
            "apply_patch".to_owned(),
            CallId::new("call-hostile"),
            &ApprovalPrompt::new(Some("change one file".to_owned()), "bounded preview").unwrap(),
        );
        let (response, mut receive) = oneshot::channel();
        joins
            .receive_envelope(ApprovalEnvelope { request, response })
            .unwrap();
        joins
            .observe_asked(
                "approval-hostile".to_owned(),
                "apply_patch".to_owned(),
                Some("call-hostile".to_owned()),
                Some("change one file".to_owned()),
            )
            .unwrap();
        let mut parser = CanonicalRecordParser::new(64);
        let mut pending = None;
        let mut deadline = None;
        let mut after = AfterFrame::None;

        apply_approval_update(
            ApprovalUiUpdate::Invalid,
            &mut joins,
            &mut parser,
            &mut pending,
            &mut deadline,
            &mut after,
        )
        .unwrap();
        assert!(pending.is_some());
        assert_eq!(after, AfterFrame::ApprovalFence);
        assert!(matches!(
            receive.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        pending = None;
        deadline = None;
        after = AfterFrame::None;
        apply_approval_update(
            ApprovalUiUpdate::Decide(ApprovalOutcome::AllowedOnce),
            &mut joins,
            &mut parser,
            &mut pending,
            &mut deadline,
            &mut after,
        )
        .unwrap();
        assert!(pending.is_none());
        assert_eq!(after, AfterFrame::None);
        assert_eq!(receive.await.unwrap().outcome, ApprovalOutcome::AllowedOnce);
    }

    #[test]
    fn observer_fault_cancels_once_and_discards_the_existing_fifo_without_deadlines() {
        let mut session = Session::new("interactive-observer-fault").unwrap();
        let mut events = session.attach_ui_observer().unwrap();
        for _ in 0..MAX_SESSION_EVENTS - 1 {
            session.append(NewEvent::log(EventKind::EndSeed)).unwrap();
        }
        events.fail_next_projection_for_test();
        session.append(NewEvent::log(EventKind::EndSeed)).unwrap();
        assert_eq!(session.events().len(), MAX_SESSION_EVENTS);

        let cancellation = tokio_util::sync::CancellationToken::new();
        let mut stop = None;
        assert!(latch_observer_fault(&events, &mut stop, &cancellation));
        assert!(cancellation.is_cancelled());
        assert_eq!(stop, Some(StopIntent::Failure(InteractiveError::Agent)));
        let mut prompt_committed = false;
        assert_eq!(
            discard_ready_updates_after_stop(
                &mut events,
                crate::session::EventSeq::new(0).unwrap(),
                "not present",
                &mut prompt_committed,
            )
            .unwrap(),
            MAX_SESSION_EVENTS - 1
        );
        assert_eq!(
            discard_ready_updates_after_stop(
                &mut events,
                crate::session::EventSeq::new(0).unwrap(),
                "not present",
                &mut prompt_committed,
            )
            .unwrap(),
            0
        );
        assert!(!prompt_committed);
    }

    #[test]
    fn stopped_turn_still_recognizes_a_committed_prompt_waiting_in_the_ui_fifo() {
        let mut session = Session::new("interactive-stop-admission-race").unwrap();
        let mut events = session.attach_ui_observer().unwrap();
        session
            .append(NewEvent::log(EventKind::turn_start(
                crate::session::TurnId::new(1).unwrap(),
            )))
            .unwrap();
        let message = Message::user(
            "user-stop-race",
            vec![ContentBlock::text("already committed").unwrap()],
            MessageSource::user().unwrap(),
        )
        .unwrap();
        session
            .append(NewEvent::surface(
                EventKind::user_message(message),
                SurfaceIntent::append(),
            ))
            .unwrap();

        let mut prompt_committed = false;
        let skipped = discard_ready_updates_after_stop(
            &mut events,
            crate::session::EventSeq::new(0).unwrap(),
            "already committed",
            &mut prompt_committed,
        )
        .unwrap();
        assert_eq!(skipped, 2);
        assert!(prompt_committed);
    }

    #[test]
    fn only_the_terminal_session_capacity_failure_forces_exit_after_rendering() {
        let exhausted = TurnEndReason::Error {
            error: LlmFailure::new(
                "the session has no safe room for another agent event",
                "AGENT_EVENT_BUDGET",
            )
            .unwrap(),
        };
        let ordinary = TurnEndReason::Error {
            error: LlmFailure::new("provider failed", "SERVER").unwrap(),
        };
        assert!(turn_exhausted_session_capacity(&exhausted));
        assert!(!turn_exhausted_session_capacity(&ordinary));
        assert!(!turn_exhausted_session_capacity(&TurnEndReason::Completed));
    }
}
