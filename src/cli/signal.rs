use std::{
    future::poll_fn,
    io,
    task::{Context, Poll},
};

use futures_util::FutureExt as _;

use tokio::signal::unix::{Signal, SignalKind, signal};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UiSignal {
    Interrupt,
    Hangup,
    Quit,
    Terminate,
    Suspend,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InteractiveSignal {
    Stop(UiSignal),
    Resize,
}

pub(super) struct SignalStreams {
    quit: Signal,
    terminate: Signal,
    hangup: Signal,
    suspend: Signal,
    interrupt: Signal,
    resize: Signal,
}

impl SignalStreams {
    /// Installs process-wide Tokio Unix handlers. The CLI calls this exactly
    /// once, inside its I/O-enabled runtime.
    pub(super) fn install() -> io::Result<Self> {
        Ok(Self {
            quit: signal(SignalKind::quit())?,
            terminate: signal(SignalKind::terminate())?,
            hangup: signal(SignalKind::hangup())?,
            suspend: signal(SignalKind::from_raw(libc::SIGTSTP))?,
            interrupt: signal(SignalKind::interrupt())?,
            resize: signal(SignalKind::window_change())?,
        })
    }

    pub(super) async fn next(&mut self) -> UiSignal {
        poll_fn(|context| self.poll_next(context)).await
    }

    pub(super) async fn next_interactive(&mut self) -> InteractiveSignal {
        poll_fn(|context| {
            if let Poll::Ready(signal) = self.poll_next(context) {
                return Poll::Ready(InteractiveSignal::Stop(signal));
            }
            if matches!(self.resize.poll_recv(context), Poll::Ready(Some(()))) {
                return Poll::Ready(InteractiveSignal::Resize);
            }
            Poll::Pending
        })
        .await
    }

    pub(super) fn poll_next(&mut self, context: &mut Context<'_>) -> Poll<UiSignal> {
        for (stream, observed) in [
            (&mut self.quit, UiSignal::Quit),
            (&mut self.terminate, UiSignal::Terminate),
            (&mut self.hangup, UiSignal::Hangup),
            (&mut self.suspend, UiSignal::Suspend),
            (&mut self.interrupt, UiSignal::Interrupt),
        ] {
            if matches!(stream.poll_recv(context), Poll::Ready(Some(()))) {
                return Poll::Ready(observed);
            }
        }
        Poll::Pending
    }

    /// Samples every distinct coalesced Unix signal class at most once.
    /// Five polls are sufficient because this driver installs five streams;
    /// a signal flood therefore cannot turn settlement into an unbounded loop.
    pub(super) fn drain_ready(&mut self, mode: DriverMode, latch: &mut SignalLatch) {
        for _ in 0..5 {
            let Some(signal) = self.next().now_or_never() else {
                break;
            };
            latch.observe(mode, signal);
        }
    }
}

pub(super) fn self_suspend() -> io::Result<()> {
    rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::STOP)
        .map_err(io::Error::from)
}

impl UiSignal {
    pub(super) const fn exit_code(self) -> Option<u8> {
        match self {
            Self::Interrupt => Some(130),
            Self::Hangup => Some(129),
            Self::Quit => Some(131),
            Self::Terminate => Some(143),
            Self::Suspend => None,
        }
    }

    const fn is_terminating(self) -> bool {
        matches!(self, Self::Hangup | Self::Quit | Self::Terminate)
    }
}

/// Returns the signal selected by the product's fixed same-poll priority.
#[cfg(test)]
pub(super) const fn strongest_ready(
    quit: bool,
    terminate: bool,
    hangup: bool,
    suspend: bool,
    interrupt: bool,
) -> Option<UiSignal> {
    if quit {
        Some(UiSignal::Quit)
    } else if terminate {
        Some(UiSignal::Terminate)
    } else if hangup {
        Some(UiSignal::Hangup)
    } else if suspend {
        Some(UiSignal::Suspend)
    } else if interrupt {
        Some(UiSignal::Interrupt)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum DriverMode {
    Interactive,
    Script,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(super) enum DriverPhase {
    Idle,
    Active,
    Approval,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(test)]
pub(super) enum SignalAction {
    Redraw,
    CancelTurn,
    Exit(u8),
    CancelThenExit(u8),
    Suspend,
    CancelThenSuspend,
    SuspendThenExit(u8),
    CancelThenSuspendAndExit(u8),
}

#[cfg(test)]
pub(super) const fn action_for(
    mode: DriverMode,
    phase: DriverPhase,
    signal: UiSignal,
) -> SignalAction {
    let owns_turn = matches!(
        phase,
        DriverPhase::Active | DriverPhase::Approval | DriverPhase::Cleanup
    );
    match (mode, signal, owns_turn) {
        (DriverMode::Interactive, UiSignal::Interrupt, false) => SignalAction::Redraw,
        (DriverMode::Interactive, UiSignal::Interrupt, true) => SignalAction::CancelTurn,
        (DriverMode::Script, UiSignal::Interrupt, false) => SignalAction::Exit(130),
        (DriverMode::Script, UiSignal::Interrupt, true) => SignalAction::CancelThenExit(130),
        (DriverMode::Interactive, UiSignal::Suspend, false) => SignalAction::Suspend,
        (DriverMode::Interactive, UiSignal::Suspend, true) => SignalAction::CancelThenSuspend,
        (DriverMode::Script, UiSignal::Suspend, false) => SignalAction::SuspendThenExit(148),
        (DriverMode::Script, UiSignal::Suspend, true) => {
            SignalAction::CancelThenSuspendAndExit(148)
        }
        (_, signal, false) => SignalAction::Exit(
            signal
                .exit_code()
                .expect("only terminating signals reach this branch"),
        ),
        (_, signal, true) => SignalAction::CancelThenExit(
            signal
                .exit_code()
                .expect("only terminating signals reach this branch"),
        ),
    }
}

/// Remembers the first observed stop. A later terminating signal may replace a
/// local interrupt or suspension request, but one terminating signal never
/// rewrites another terminating signal's already-observed exit fact.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SignalLatch {
    observed: Option<UiSignal>,
}

impl SignalLatch {
    pub(super) const fn observed(self) -> Option<UiSignal> {
        self.observed
    }

    pub(super) fn observe(&mut self, mode: DriverMode, signal: UiSignal) {
        match self.observed {
            None => self.observed = Some(signal),
            Some(current) if !is_exit_signal(mode, current) && is_exit_signal(mode, signal) => {
                self.observed = Some(signal);
            }
            Some(UiSignal::Interrupt)
                if mode == DriverMode::Interactive && signal == UiSignal::Suspend =>
            {
                self.observed = Some(signal);
            }
            Some(_) => {}
        }
    }
}

const fn is_exit_signal(mode: DriverMode, signal: UiSignal) -> bool {
    signal.is_terminating() || matches!((mode, signal), (DriverMode::Script, UiSignal::Interrupt))
}

#[cfg(test)]
mod tests {
    use super::{
        DriverMode, DriverPhase, SignalAction, SignalLatch, UiSignal, action_for, strongest_ready,
    };

    #[test]
    fn same_poll_priority_is_quit_term_hup_tstp_then_int() {
        assert_eq!(
            strongest_ready(true, true, true, true, true),
            Some(UiSignal::Quit)
        );
        assert_eq!(
            strongest_ready(false, true, true, true, true),
            Some(UiSignal::Terminate)
        );
        assert_eq!(
            strongest_ready(false, false, true, true, true),
            Some(UiSignal::Hangup)
        );
        assert_eq!(
            strongest_ready(false, false, false, true, true),
            Some(UiSignal::Suspend)
        );
        assert_eq!(
            strongest_ready(false, false, false, false, true),
            Some(UiSignal::Interrupt)
        );
        assert_eq!(strongest_ready(false, false, false, false, false), None);
    }

    #[test]
    fn interactive_interrupt_redraws_idle_and_cancels_owned_turns() {
        assert_eq!(
            action_for(
                DriverMode::Interactive,
                DriverPhase::Idle,
                UiSignal::Interrupt,
            ),
            SignalAction::Redraw
        );
        for phase in [
            DriverPhase::Active,
            DriverPhase::Approval,
            DriverPhase::Cleanup,
        ] {
            assert_eq!(
                action_for(DriverMode::Interactive, phase, UiSignal::Interrupt),
                SignalAction::CancelTurn
            );
        }
    }

    #[test]
    fn script_interrupt_and_termination_use_stable_shell_exit_codes() {
        assert_eq!(
            action_for(DriverMode::Script, DriverPhase::Active, UiSignal::Interrupt,),
            SignalAction::CancelThenExit(130)
        );
        for (signal, code) in [
            (UiSignal::Hangup, 129),
            (UiSignal::Quit, 131),
            (UiSignal::Terminate, 143),
        ] {
            assert_eq!(
                action_for(DriverMode::Interactive, DriverPhase::Idle, signal),
                SignalAction::Exit(code)
            );
            assert_eq!(
                action_for(DriverMode::Script, DriverPhase::Active, signal),
                SignalAction::CancelThenExit(code)
            );
        }
    }

    #[test]
    fn suspension_waits_for_cleanup_and_script_exits_after_resume() {
        assert_eq!(
            action_for(
                DriverMode::Interactive,
                DriverPhase::Idle,
                UiSignal::Suspend,
            ),
            SignalAction::Suspend
        );
        assert_eq!(
            action_for(
                DriverMode::Interactive,
                DriverPhase::Approval,
                UiSignal::Suspend,
            ),
            SignalAction::CancelThenSuspend
        );
        assert_eq!(
            action_for(DriverMode::Script, DriverPhase::Idle, UiSignal::Suspend,),
            SignalAction::SuspendThenExit(148)
        );
        assert_eq!(
            action_for(DriverMode::Script, DriverPhase::Cleanup, UiSignal::Suspend,),
            SignalAction::CancelThenSuspendAndExit(148)
        );
    }

    #[test]
    fn a_terminating_signal_upgrades_local_cleanup_but_first_termination_wins() {
        let mut latch = SignalLatch::default();
        latch.observe(DriverMode::Interactive, UiSignal::Interrupt);
        latch.observe(DriverMode::Interactive, UiSignal::Suspend);
        assert_eq!(latch.observed(), Some(UiSignal::Suspend));
        latch.observe(DriverMode::Interactive, UiSignal::Hangup);
        assert_eq!(latch.observed(), Some(UiSignal::Hangup));
        latch.observe(DriverMode::Interactive, UiSignal::Quit);
        latch.observe(DriverMode::Interactive, UiSignal::Terminate);
        assert_eq!(latch.observed(), Some(UiSignal::Hangup));

        let mut script = SignalLatch::default();
        script.observe(DriverMode::Script, UiSignal::Interrupt);
        script.observe(DriverMode::Script, UiSignal::Suspend);
        script.observe(DriverMode::Script, UiSignal::Terminate);
        assert_eq!(script.observed(), Some(UiSignal::Interrupt));
    }
}
