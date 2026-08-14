use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::agent::AgentLoop;

use super::{
    identity::prepare_user_turn,
    script::summarize_turn,
    script_io::{ScriptOutputFrames, write_final_output_or_exit},
    signal::{DriverMode, SignalLatch, SignalStreams, UiSignal, self_suspend},
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("CLI_OUTPUT_FAILED")]
pub(super) struct ScriptDriverError;

pub(super) async fn run_one_turn(
    mut agent: AgentLoop,
    prompt: String,
    signals: &mut SignalStreams,
) -> Result<u8, ScriptDriverError> {
    let prepared = match prepare_user_turn(agent.session(), &prompt) {
        Ok(prepared) => prepared,
        Err(_) => return write_agent_failure(signals).await,
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
        exit_after_settlement(signal, signals);
    }

    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(_) => return write_agent_failure(signals).await,
    };
    if outcome.turn() != prepared.turn {
        return write_agent_failure(signals).await;
    }
    let summary = match summarize_turn(agent.session().events(), prepared.start_seq, prepared.turn)
    {
        Ok(summary) => summary,
        Err(_) => return write_agent_failure(signals).await,
    };
    let exit = summary.exit_code();
    let frames = ScriptOutputFrames::from_summary(&summary).map_err(|_| ScriptDriverError)?;
    write_final_output_or_exit(frames, signals)
        .await
        .map_err(|_| ScriptDriverError)?;
    Ok(exit)
}

async fn write_agent_failure(signals: &mut SignalStreams) -> Result<u8, ScriptDriverError> {
    let frames = ScriptOutputFrames::agent_failure().map_err(|_| ScriptDriverError)?;
    write_final_output_or_exit(frames, signals)
        .await
        .map_err(|_| ScriptDriverError)?;
    Ok(1)
}

fn exit_after_settlement(signal: UiSignal, signals: &mut SignalStreams) -> ! {
    if signal == UiSignal::Suspend {
        if self_suspend().is_err() {
            std::process::exit(1);
        }
        let mut latch = SignalLatch::default();
        latch.observe(DriverMode::Script, signal);
        signals.drain_ready(DriverMode::Script, &mut latch);
        std::process::exit(
            latch
                .observed()
                .and_then(UiSignal::exit_code)
                .unwrap_or(148)
                .into(),
        );
    }
    std::process::exit(signal.exit_code().unwrap_or(1).into())
}
