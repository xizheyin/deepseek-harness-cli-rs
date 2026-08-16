use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{agent::AgentLoop, session::StoreError};

use super::{
    identity::prepare_user_turn,
    script::summarize_outcome,
    script_io::{ScriptOutputFrames, write_final_output_or_exit},
    shutdown,
    signal::{DriverMode, SignalLatch, SignalStreams, UiSignal, self_suspend},
    storage_failure,
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ScriptDriverError {
    #[error("CLI_OUTPUT_FAILED")]
    Output,
    #[error(transparent)]
    Storage(StoreError),
}

pub(super) async fn run_one_turn(
    mut agent: AgentLoop,
    prompt: String,
    signals: &mut SignalStreams,
) -> Result<u8, ScriptDriverError> {
    let result = run_one_turn_inner(&mut agent, prompt, signals).await;
    let initial_signal = result.as_ref().ok().and_then(|output| match output.exit {
        ScriptExit::Signal(signal) => Some(signal),
        ScriptExit::Ordinary(_) => None,
    });
    let (shutdown, signal) =
        shutdown::agent_with_signals(&mut agent, DriverMode::Script, signals, initial_signal).await;
    if let Some(signal) = signal {
        return Ok(exit_after_settlement(signal, signals));
    }
    let output = match (result, shutdown) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(ScriptDriverError::Storage(storage_failure::from_shutdown(
            &error,
        ))),
        (Ok(output), Ok(())) => Ok(output),
    }?;
    if let Some(frames) = output.frames {
        write_final_output_or_exit(frames, signals)
            .await
            .map_err(|_| ScriptDriverError::Output)?;
    }
    Ok(output.exit.code())
}

async fn run_one_turn_inner(
    agent: &mut AgentLoop,
    prompt: String,
    signals: &mut SignalStreams,
) -> Result<ScriptTurnOutput, ScriptDriverError> {
    let prepared = match prepare_user_turn(agent.session(), &prompt) {
        Ok(prepared) => prepared,
        Err(_) => return agent_failure_output(),
    };
    let cancellation = CancellationToken::new();
    let outcome = {
        let future = agent.run_turn(prepared.proposal, cancellation.clone());
        tokio::pin!(future);
        let mut latch = SignalLatch::default();
        loop {
            if latch.observed().is_some() {
                tokio::select! {
                    biased;
                    result = &mut future => break (result, latch),
                    signal = signals.next() => {
                        latch.observe(DriverMode::Script, signal);
                        cancellation.cancel();
                    }
                }
            } else {
                tokio::select! {
                    biased;
                    signal = signals.next() => {
                        latch.observe(DriverMode::Script, signal);
                        cancellation.cancel();
                    }
                    result = &mut future => break (result, latch),
                }
            }
        }
    };

    let (outcome, mut latch) = outcome;
    tokio::task::yield_now().await;
    signals.drain_ready(DriverMode::Script, &mut latch);
    if let Some(signal) = latch.observed() {
        return Ok(ScriptTurnOutput {
            exit: ScriptExit::Signal(signal),
            frames: None,
        });
    }

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Some(error) = storage_failure::from_agent(&error) {
                return Err(ScriptDriverError::Storage(error));
            }
            return agent_failure_output();
        }
    };
    if outcome.turn() != prepared.turn {
        return agent_failure_output();
    }
    let summary = summarize_outcome(&outcome);
    let exit = summary.exit_code();
    let frames =
        ScriptOutputFrames::from_summary(&summary).map_err(|_| ScriptDriverError::Output)?;
    Ok(ScriptTurnOutput {
        exit: ScriptExit::Ordinary(exit),
        frames: Some(frames),
    })
}

struct ScriptTurnOutput {
    exit: ScriptExit,
    frames: Option<ScriptOutputFrames>,
}

enum ScriptExit {
    Ordinary(u8),
    Signal(UiSignal),
}

impl ScriptExit {
    fn code(&self) -> u8 {
        match self {
            Self::Ordinary(code) => *code,
            Self::Signal(signal) => signal.exit_code().unwrap_or(1),
        }
    }
}

fn agent_failure_output() -> Result<ScriptTurnOutput, ScriptDriverError> {
    let frames = ScriptOutputFrames::agent_failure().map_err(|_| ScriptDriverError::Output)?;
    Ok(ScriptTurnOutput {
        exit: ScriptExit::Ordinary(1),
        frames: Some(frames),
    })
}

fn exit_after_settlement(signal: UiSignal, signals: &mut SignalStreams) -> u8 {
    if signal == UiSignal::Suspend {
        if self_suspend().is_err() {
            return 1;
        }
        let mut latch = SignalLatch::default();
        latch.observe(DriverMode::Script, signal);
        signals.drain_ready(DriverMode::Script, &mut latch);
        return latch
            .observed()
            .and_then(UiSignal::exit_code)
            .unwrap_or(148);
    }
    signal.exit_code().unwrap_or(1)
}
