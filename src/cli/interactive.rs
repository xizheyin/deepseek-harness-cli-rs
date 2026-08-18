use std::mem;
use std::time::Duration;

use futures_util::FutureExt as _;
use thiserror::Error;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::AgentLoop,
    session::{ApprovalOutcome, CommittedUiReceiver, StoreError, TurnEndReason, TurnId},
};

use super::{
    approval::{ApprovalEnvelope, ApprovalEnvelopeReceiver},
    approval_join::{ApprovalJoin, ApprovalJoinError, ApprovalResetMode},
    approval_selector::{ApprovalSelector, ESCAPE_SEQUENCE_WAIT, SelectorUpdate},
    assembly::InteractiveAssembly,
    identity::prepare_user_turn,
    input::{
        CanonicalRecordParser, IdleInput, InputRecordEvent, MAX_APPROVAL_RECORD_BYTES,
        MAX_INTERACTIVE_PROMPT_BYTES, classify_idle_record,
    },
    live::{InteractivePresenter, LiveFrame, LiveLifecycle, LiveRenderer, PendingLiveFrame},
    shutdown,
    signal::{DriverMode, SignalLatch, SignalStreams, UiSignal, self_suspend},
    storage_failure,
    terminal::{ApprovalTerminalMode, AsyncTerminal, TERMINAL_READ_BYTES, TerminalError},
};

const FRAME_DEADLINE: Duration = Duration::from_secs(5);
const APPROVAL_INPUT_QUIET: Duration = Duration::from_millis(100);

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

pub(super) async fn run(
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
                            color,
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
        (Err(InteractiveError::Agent), Err(error)) => Err(InteractiveError::Storage(
            storage_failure::from_shutdown(&error),
        )),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(InteractiveError::Storage(storage_failure::from_shutdown(
            &error,
        ))),
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
    color: bool,
}

enum ApprovalUiState<'a> {
    Inactive,
    Arming {
        deadline: Instant,
    },
    Rendering {
        mode: ApprovalTerminalMode<'a>,
        selector: ApprovalSelector,
        compact: bool,
    },
    Accepting {
        mode: ApprovalTerminalMode<'a>,
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
    ) -> Result<String, InteractiveError> {
        if !matches!(self, Self::Arming { .. }) {
            return Err(InteractiveError::Agent);
        }
        terminal.flush_input()?;
        let compact = terminal.columns().is_none_or(|columns| columns < 48);
        let mode = terminal.enter_approval_mode()?;
        let selector = ApprovalSelector::new();
        let output = selector
            .render(color, compact, false)
            .map_err(|_| InteractiveError::Output)?;
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
                let output = selector
                    .render(color, compact, color && !compact)
                    .map_err(|_| InteractiveError::Output)?;
                *self = Self::Accepting {
                    mode,
                    selector,
                    compact,
                    escape_deadline: None,
                };
                Ok(ApprovalUiUpdate::Redraw(output))
            }
            SelectorUpdate::Decide(outcome) => {
                mode.restore()?;
                Ok(ApprovalUiUpdate::Decide(outcome))
            }
            SelectorUpdate::Eof => {
                mode.restore()?;
                Ok(ApprovalUiUpdate::Eof)
            }
            SelectorUpdate::Invalid => {
                mode.restore()?;
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
                mode.restore()?;
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
                mode.restore().map_err(Into::into)
            }
            Self::Inactive | Self::Arming { .. } => Ok(()),
        }
    }
}

async fn run_turn(active: ActiveTurn<'_>) -> Result<TurnDisposition, InteractiveError> {
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

            if let Err(error) = complete_ready_frame(
                &mut pending,
                &mut frame_deadline,
                &mut after_frame,
                &mut approval_ui,
                &mut turn_end_rendered,
                active.presenter,
                active.terminal,
                active.parser,
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
                signal = active.signals.next() => {
                    observe_signal(&mut stop, signal);
                    cancellation.cancel();
                    discard_pending(&mut pending, active.presenter);
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
                                .begin_rendering(active.terminal, active.color)
                                .and_then(|output| {
                                    LiveFrame::approval_selector(output)
                                        .map_err(|_| InteractiveError::Output)
                                })
                                .and_then(|frame| {
                                    enqueue_frame(
                                        frame,
                                        AfterFrame::ApprovalAccepting,
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
                                apply_approval_update(
                                    update,
                                    active.joins,
                                    active.parser,
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
                                .and_then(|frame| {
                                    frame.advance(count).map_err(|_| InteractiveError::Output)
                                });
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
                                        )
                                    })
                                    .and_then(|update| {
                                        apply_approval_update(
                                            update,
                                            active.joins,
                                            active.parser,
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
            if let Err(error) = complete_ready_frame(
                &mut pending,
                &mut frame_deadline,
                &mut after_frame,
                &mut approval_ui,
                &mut turn_end_rendered,
                active.presenter,
                active.terminal,
                active.parser,
            ) {
                observe_failure(&mut stop, error);
                discard_pending(&mut pending, active.presenter);
                continue;
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
                signal = active.signals.next() => {
                    observe_signal(&mut stop, signal);
                    discard_pending(&mut pending, active.presenter);
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
                                .begin_rendering(active.terminal, active.color)
                                .and_then(|output| {
                                    LiveFrame::approval_selector(output)
                                        .map_err(|_| InteractiveError::Output)
                                })
                                .and_then(|frame| {
                                    enqueue_frame(
                                        frame,
                                        AfterFrame::ApprovalAccepting,
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
                                apply_approval_update(
                                    update,
                                    active.joins,
                                    active.parser,
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
                                    .and_then(|frame| {
                                        frame.advance(count).map_err(|_| InteractiveError::Output)
                                    });
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
                                        )
                                    })
                                    .and_then(|update| {
                                        apply_approval_update(
                                            update,
                                            active.joins,
                                            active.parser,
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
        skipped = discard_ready_updates(active.events);
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
    if let Some(frame) = update.frame {
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
    pending: &'a mut Option<PendingLiveFrame>,
    frame_deadline: &'a mut Option<Instant>,
    after_frame: &'a mut AfterFrame,
    approval_ui: &'a mut ApprovalUiState<'terminal>,
    turn_end_seen: &'a mut bool,
}

fn apply_approval_update(
    update: ApprovalUiUpdate,
    joins: &mut ApprovalJoin,
    parser: &mut CanonicalRecordParser,
    pending: &mut Option<PendingLiveFrame>,
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
    pending: &mut Option<PendingLiveFrame>,
    deadline: &mut Option<Instant>,
    pending_after: &mut AfterFrame,
) -> Result<(), InteractiveError> {
    if pending.is_some() {
        return Err(InteractiveError::Agent);
    }
    *pending = Some(frame.into_pending().map_err(|_| InteractiveError::Output)?);
    *deadline = Some(Instant::now() + FRAME_DEADLINE);
    *pending_after = after;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn complete_ready_frame(
    pending: &mut Option<PendingLiveFrame>,
    deadline: &mut Option<Instant>,
    after: &mut AfterFrame,
    approval_ui: &mut ApprovalUiState<'_>,
    turn_end_rendered: &mut bool,
    presenter: &mut InteractivePresenter,
    terminal: &AsyncTerminal,
    parser: &mut CanonicalRecordParser,
) -> Result<(), InteractiveError> {
    let Some(frame) = pending.as_mut() else {
        return Ok(());
    };
    if frame
        .prepare_next(presenter)
        .map_err(|_| InteractiveError::Output)?
    {
        return Ok(());
    }
    *pending = None;
    *deadline = None;
    match mem::take(after) {
        AfterFrame::None => {}
        AfterFrame::ApprovalFence => {
            terminal.revalidate()?;
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

fn discard_pending(pending: &mut Option<PendingLiveFrame>, presenter: &mut InteractivePresenter) {
    if pending.take().is_some() {
        presenter.discard_partly_written_frame();
    }
}

fn latch_active_failure(
    stop: &mut Option<StopIntent>,
    cancellation: &CancellationToken,
    pending: &mut Option<PendingLiveFrame>,
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
    pending: Option<&PendingLiveFrame>,
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
    pending: Option<&PendingLiveFrame>,
) -> std::io::Result<usize> {
    let bytes = pending.map(PendingLiveFrame::bytes).unwrap_or_default();
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

async fn finish_turn_disposition(
    stop: Option<StopIntent>,
    skipped: usize,
    turn_end_rendered: bool,
    presenter: &mut InteractivePresenter,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<TurnDisposition, InteractiveError> {
    match stop {
        None => Ok(TurnDisposition::Continue),
        Some(StopIntent::Interrupt) => {
            if !turn_end_rendered {
                let frame = LiveFrame::stopped(skipped).map_err(|_| InteractiveError::Output)?;
                if let Some(signal) = write_frame(frame, presenter, terminal, signals).await? {
                    return finish_signal_after_cleanup(signal, terminal, signals).await;
                }
            }
            Ok(TurnDisposition::Continue)
        }
        Some(StopIntent::Eof) => Ok(TurnDisposition::Exit(0)),
        Some(StopIntent::Suspend) => match suspend_and_resume(terminal, signals).await? {
            Some(signal) => Ok(TurnDisposition::Signal(signal)),
            None => Ok(TurnDisposition::Continue),
        },
        Some(StopIntent::Exit(signal)) => Ok(TurnDisposition::Signal(signal)),
        Some(StopIntent::Failure(error)) => Err(error),
    }
}

async fn finish_signal_after_cleanup(
    signal: UiSignal,
    terminal: &AsyncTerminal,
    signals: &mut SignalStreams,
) -> Result<TurnDisposition, InteractiveError> {
    match signal {
        UiSignal::Interrupt => Ok(TurnDisposition::Continue),
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

fn discard_ready_updates(events: &mut CommittedUiReceiver) -> usize {
    let mut skipped = 0_usize;
    while events.try_recv().is_ok() {
        skipped = skipped.saturating_add(1);
    }
    skipped
}

#[cfg(test)]
mod tests {
    use super::{
        AfterFrame, ApprovalUiUpdate, InteractiveError, StopIntent, apply_approval_update,
        discard_ready_updates, latch_observer_fault, observe_failure, observe_signal,
        turn_exhausted_session_capacity,
    };
    use crate::{
        agent::{ApprovalPrompt, ApprovalRequest},
        cli::{
            approval::{ApprovalChallengePool, ApprovalEnvelope},
            approval_join::ApprovalJoin,
            input::CanonicalRecordParser,
            signal::UiSignal,
        },
        entropy::{EntropyError, EntropySource},
        model::{CallId, LlmFailure},
        session::{
            ApprovalOutcome, ApprovalRequestId, EventKind, MAX_SESSION_EVENTS, NewEvent, Session,
            TurnEndReason,
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
    fn output_failure_is_preserved_unless_a_terminating_signal_wins() {
        let mut stop = None;
        observe_failure(&mut stop, InteractiveError::Output);
        observe_signal(&mut stop, UiSignal::Interrupt);
        assert_eq!(stop, Some(StopIntent::Failure(InteractiveError::Output)));
        observe_signal(&mut stop, UiSignal::Quit);
        assert_eq!(stop, Some(StopIntent::Exit(UiSignal::Quit)));
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
        assert_eq!(discard_ready_updates(&mut events), MAX_SESSION_EVENTS - 1);
        assert_eq!(discard_ready_updates(&mut events), 0);
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
