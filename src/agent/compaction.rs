//! One bounded pre-step summary pass for long conversations.

use std::{collections::HashMap, panic::AssertUnwindSafe};

use futures_util::{FutureExt, StreamExt};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    model::{
        ContentBlock, ContentBlockKind, FinishReason, FinishReasonKind, MAX_PROVIDER_STREAM_CHUNKS,
        Message, MessageSource, NonNegativeSafeInteger, RequestPurpose, StreamChunkKind,
        StreamValidator, TokenUsage,
    },
    provider::{ProviderPreflightError, ProviderRequestDraft},
    session::{
        COMPACTION_CHECKPOINT_PREFIX, COMPACTION_CHECKPOINT_SOURCE, COMPACTION_CHECKPOINT_SUFFIX,
        COMPACTION_INSTRUCTION_SOURCE, CompactionEndEvent, CompactionId, CompactionStartEvent,
        CompactionSummaryEvent, CompactionSummaryInput, CompactionTrigger, EventClaim, EventKind,
        ModelVisibleDispatchInput, ModelVisibleDispatchSnapshot, NewEvent,
        PreparedCompactionCallSnapshot, PreparedRetryBackoffSnapshot, PreparedRetryPolicySnapshot,
        SessionReservation, SurfaceIntent, TurnId,
    },
};

use super::{
    AgentIdKind, AgentLoopError, DispatchBarrier, Driver, dispatch_barrier, failure_reason,
    is_budget_error, is_fatal_loop_error, next_id, proposed_config,
};

const PRESSURE_NUMERATOR: u64 = 4;
const PRESSURE_DENOMINATOR: u64 = 5;
const RETAIN_NUMERATOR: u64 = 4;
const RETAIN_DENOMINATOR: u64 = 25;
const MAX_COMPACTION_OUTPUT_TOKENS: u64 = 8_192;
const MAX_COMPACTION_STREAM_BYTES: usize = 10 * 1024 * 1024;
const COMPACTION_INSTRUCTION: &str = "Summarize the selected older conversation prefix for another coding agent. Preserve concrete user requirements, decisions, file paths, commands, code changes, test results, unresolved work, and tool outcomes. Do not call tools. Return only a concise factual summary; do not add commentary about summarizing.";

pub(super) enum CompactionOutcome {
    Progress,
    NoProgress,
    AdvisoryFailure(crate::model::LlmFailure),
    Cancelled,
    TurnError(crate::model::LlmFailure),
}

struct CompactionClose {
    claim: EventClaim,
    compaction_id: CompactionId,
    turn: TurnId,
}

pub(super) fn pressure_reached(total_tokens: u64, context_window: u64) -> bool {
    total_tokens
        .checked_mul(PRESSURE_DENOMINATOR)
        .zip(context_window.checked_mul(PRESSURE_NUMERATOR))
        .is_some_and(|(total, threshold)| total >= threshold)
}

pub(super) fn retained_token_target(context_window: Option<u64>) -> u64 {
    context_window
        .and_then(|window| window.checked_mul(RETAIN_NUMERATOR))
        .map_or(0, |tokens| tokens / RETAIN_DENOMINATOR)
}

pub(super) async fn compact_once(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    turn: TurnId,
    trigger: CompactionTrigger,
    retain_tokens: u64,
    cancellation: &CancellationToken,
    budget_failure: &crate::model::LlmFailure,
) -> Result<CompactionOutcome, AgentLoopError> {
    if cancellation.is_cancelled() {
        return Ok(CompactionOutcome::Cancelled);
    }
    if tokio::time::Instant::now() >= driver.deadline {
        return Ok(CompactionOutcome::TurnError(failure_reason(
            "AGENT_TURN_TIMEOUT",
            "the agent turn timed out",
        )?));
    }
    if driver
        .counters
        .attempts
        .checked_add(1)
        .is_none_or(|after_summary| after_summary >= driver.config.limits.max_attempts_per_turn)
    {
        return Ok(CompactionOutcome::NoProgress);
    }

    let candidate = match reservation.session().compaction_candidate(retain_tokens) {
        Ok(Some(candidate))
            if candidate.shadowed_token_count > 0 && !candidate.messages.is_empty() =>
        {
            candidate
        }
        Ok(_) | Err(_) => return Ok(CompactionOutcome::NoProgress),
    };

    let maximum = driver
        .config
        .limits
        .max_output_tokens_per_request
        .min(MAX_COMPACTION_OUTPUT_TOKENS);
    let Ok(maximum) = NonNegativeSafeInteger::new(maximum) else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let Ok(proposed) = proposed_config(
        driver.config,
        reservation.session().request_header(),
        *driver.request_header_logged,
    )
    .and_then(|config| {
        config
            .with_max_tokens_preserving_extensions(maximum)
            .map_err(AgentLoopError::from)
    }) else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let instruction = (|| -> Result<Message, AgentLoopError> {
        Ok(Message::user(
            next_id(driver.runtime, AgentIdKind::Message)?,
            vec![ContentBlock::text(COMPACTION_INSTRUCTION)?],
            MessageSource::plugin(COMPACTION_INSTRUCTION_SOURCE)?,
        )?)
    })();
    let Ok(instruction) = instruction else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let mut request_messages = candidate.messages.clone();
    if request_messages.try_reserve(1).is_err() {
        return Ok(CompactionOutcome::NoProgress);
    }
    request_messages.push(instruction.clone());

    let current_header = reservation
        .session()
        .request_header()
        .map(crate::session::EpochHeader::canonicalized);
    let (system, tools) = match &current_header {
        Some(header) => (
            header.system.clone(),
            header.tools.clone().unwrap_or_default(),
        ),
        None => (driver.config.system.clone(), driver.config.tools.clone()),
    };
    let session_id = reservation.session().id().clone();
    let draft = (|| {
        let mut draft = ProviderRequestDraft::new(&proposed, &request_messages)?;
        if let Some(system) = system.as_deref() {
            draft = draft.with_system(system)?;
        }
        if !tools.is_empty() {
            draft = draft.with_tools(&tools)?;
        }
        draft
            .with_purpose(RequestPurpose::Compaction)
            .with_session_id(&session_id)
    })();
    let Ok(draft) = draft else {
        return Ok(CompactionOutcome::NoProgress);
    };
    driver.counters.attempts += 1;
    let preflight = match std::panic::catch_unwind(AssertUnwindSafe(|| {
        driver.provider.preflight_request(draft)
    })) {
        Ok(Ok(preflight)) => preflight,
        Ok(Err(
            ProviderPreflightError::Preparation(_)
            | ProviderPreflightError::WireTooLarge { .. }
            | ProviderPreflightError::RequestLimit { .. }
            | ProviderPreflightError::InvalidRequest { .. },
        ))
        | Err(_) => return Ok(CompactionOutcome::NoProgress),
    };
    if !preflight
        .prepared_call()
        .config()
        .max_tokens()
        .is_some_and(|prepared| prepared.get() > 0 && prepared <= maximum)
    {
        return Ok(CompactionOutcome::NoProgress);
    }
    let Ok(prepared_call) = prepared_snapshot(preflight.prepared_call()) else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let effective_provider = preflight.prepared_call().config().provider().to_owned();
    let effective_model = preflight.prepared_call().config().model().to_owned();
    let effective_max_tokens = preflight.prepared_call().config().max_tokens();
    let Ok(source_surface_generation) =
        NonNegativeSafeInteger::new(candidate.source_surface_generation)
    else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let Ok(dispatch) = ModelVisibleDispatchSnapshot::new(ModelVisibleDispatchInput {
        trigger,
        source_surface_generation,
        shadowed_range: candidate.range,
        shadowed_seqs: candidate.shadowed_seqs.clone(),
        prepared_call,
        request_header_seq: candidate.request_header_seq,
        request_context_seq: candidate.request_context_seq,
        system: system.clone(),
        tools: tools.clone(),
        session_id: session_id.clone(),
        instruction,
    }) else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let Ok(compaction_id) = next_id(driver.runtime, AgentIdKind::Message) else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let compaction_id = CompactionId::new(compaction_id);
    let Ok(start) = CompactionStartEvent::new(compaction_id.clone(), None, turn, dispatch) else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let Ok(failure) = failure_reason(
        "AGENT_COMPACTION_FAILED",
        "the automatic context summary did not complete",
    ) else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let Ok(failure_end) = CompactionEndEvent::new(compaction_id.clone(), None, turn, Some(failure))
    else {
        return Ok(CompactionOutcome::NoProgress);
    };
    let close =
        match reservation.claim_batch([NewEvent::log(EventKind::compaction_end(failure_end))]) {
            Ok(mut claims) => claims.pop().ok_or(AgentLoopError::Invariant(
                "compaction closure claim disappeared",
            ))?,
            Err(error) if is_budget_error(&error) => {
                return Ok(CompactionOutcome::TurnError(
                    driver.failure_for_budget(&error, budget_failure),
                ));
            }
            Err(error) if is_fatal_loop_error(&AgentLoopError::Session(error.clone())) => {
                return Err(error.into());
            }
            Err(_) => return Ok(CompactionOutcome::NoProgress),
        };
    let mut close = CompactionClose {
        claim: close,
        compaction_id: compaction_id.clone(),
        turn,
    };
    let start_receipt = match reservation
        .append_settled(NewEvent::log(EventKind::compaction_start(start)))
        .await
    {
        Ok(receipt) => receipt,
        Err(error) => {
            if is_fatal_loop_error(&AgentLoopError::Session(error.clone())) {
                return Err(error.into());
            }
            reservation.release(&mut close.claim)?;
            if is_budget_error(&error) {
                return Ok(CompactionOutcome::TurnError(
                    driver.failure_for_budget(&error, budget_failure),
                ));
            }
            return Ok(CompactionOutcome::NoProgress);
        }
    };
    match dispatch_barrier(reservation).await? {
        DispatchBarrier::Ready => {}
        DispatchBarrier::ObserverUnavailable => {
            driver.observer_unavailable = true;
            let outcome = CompactionOutcome::TurnError(failure_reason(
                "AGENT_OBSERVER_UNAVAILABLE",
                "the live session observer became unavailable",
            )?);
            return close_failed_bracket(reservation, &mut close, driver, outcome).await;
        }
    }
    if cancellation.is_cancelled() {
        return close_failed_bracket(
            reservation,
            &mut close,
            driver,
            CompactionOutcome::Cancelled,
        )
        .await;
    }
    if tokio::time::Instant::now() >= driver.deadline {
        let outcome = CompactionOutcome::TurnError(failure_reason(
            "AGENT_TURN_TIMEOUT",
            "the agent turn timed out",
        )?);
        return close_failed_bracket(reservation, &mut close, driver, outcome).await;
    }

    let request = match draft.into_request(preflight) {
        Ok(request) => request,
        Err(_) => {
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::NoProgress,
            )
            .await;
        }
    };
    let child = cancellation.child_token();
    let stream = match std::panic::catch_unwind(AssertUnwindSafe(|| {
        driver.provider.stream(request, child.clone())
    })) {
        Ok(stream) => stream,
        Err(_) => {
            child.cancel();
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::NoProgress,
            )
            .await;
        }
    };
    let collected = collect_summary(stream, cancellation, driver.deadline).await;
    if !matches!(&collected.outcome, CollectedSummaryOutcome::Success { .. }) {
        child.cancel();
    }
    if !matches!(&collected.outcome, CollectedSummaryOutcome::Success { .. }) {
        let output_tokens = collected
            .usage
            .as_ref()
            .map(|usage| usage.output_tokens().get());
        if let Some(failure) = record_summary_usage(driver, output_tokens)? {
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::TurnError(failure),
            )
            .await;
        }
    }
    let (summary, raw_output, usage) = match collected.outcome {
        CollectedSummaryOutcome::Success {
            summary,
            raw_output,
        } => (summary, raw_output, collected.usage),
        CollectedSummaryOutcome::Failed(failure) => {
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::AdvisoryFailure(failure),
            )
            .await;
        }
        CollectedSummaryOutcome::Cancelled => {
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::Cancelled,
            )
            .await;
        }
        CollectedSummaryOutcome::TimedOut => {
            let outcome = CompactionOutcome::TurnError(failure_reason(
                "AGENT_TURN_TIMEOUT",
                "the agent turn timed out",
            )?);
            return close_failed_bracket(reservation, &mut close, driver, outcome).await;
        }
        CollectedSummaryOutcome::Invalid => {
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::NoProgress,
            )
            .await;
        }
    };
    if cancellation.is_cancelled() {
        return close_failed_bracket(
            reservation,
            &mut close,
            driver,
            CompactionOutcome::Cancelled,
        )
        .await;
    }
    if tokio::time::Instant::now() >= driver.deadline {
        let outcome = CompactionOutcome::TurnError(failure_reason(
            "AGENT_TURN_TIMEOUT",
            "the agent turn timed out",
        )?);
        return close_failed_bracket(reservation, &mut close, driver, outcome).await;
    }
    let summary_output_tokens = usage.as_ref().map(|usage| usage.output_tokens().get());

    let summary_event = CompactionSummaryEvent::new(CompactionSummaryInput {
        compaction_id: compaction_id.clone(),
        source_command_id: None,
        summary: summary.clone(),
        raw_output,
        shadowed_range: candidate.range,
        shadowed_seqs: candidate.shadowed_seqs.clone(),
        shadowed_token_count: match NonNegativeSafeInteger::new(candidate.shadowed_token_count) {
            Ok(tokens) => tokens,
            Err(_) => {
                return close_failed_bracket(
                    reservation,
                    &mut close,
                    driver,
                    CompactionOutcome::NoProgress,
                )
                .await;
            }
        },
        provider: effective_provider,
        model: effective_model,
        max_tokens: effective_max_tokens,
        usage,
    });
    let Ok(summary_event) = summary_event else {
        return close_failed_bracket(
            reservation,
            &mut close,
            driver,
            CompactionOutcome::NoProgress,
        )
        .await;
    };
    let summary_receipt = match reservation
        .append_settled(NewEvent::log(EventKind::compaction_summary(summary_event)))
        .await
    {
        Ok(receipt) => receipt,
        Err(error) if is_budget_error(&error) => {
            let outcome =
                CompactionOutcome::TurnError(driver.failure_for_budget(&error, budget_failure));
            return close_failed_bracket(reservation, &mut close, driver, outcome).await;
        }
        Err(error) => {
            if is_fatal_loop_error(&AgentLoopError::Session(error.clone())) {
                return Err(error.into());
            }
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::NoProgress,
            )
            .await;
        }
    };
    if let Some(failure) = record_summary_usage(driver, summary_output_tokens)? {
        let outcome = CompactionOutcome::TurnError(failure);
        return close_failed_bracket(reservation, &mut close, driver, outcome).await;
    }
    if cancellation.is_cancelled() {
        return close_failed_bracket(
            reservation,
            &mut close,
            driver,
            CompactionOutcome::Cancelled,
        )
        .await;
    }
    if tokio::time::Instant::now() >= driver.deadline {
        let outcome = CompactionOutcome::TurnError(failure_reason(
            "AGENT_TURN_TIMEOUT",
            "the agent turn timed out",
        )?);
        return close_failed_bracket(reservation, &mut close, driver, outcome).await;
    }

    let body = (|| -> Result<_, AgentLoopError> {
        let mut checkpoint_content = Vec::new();
        checkpoint_content
            .try_reserve_exact(summary.len() + 2)
            .map_err(|_| AgentLoopError::Invariant("compaction checkpoint capacity failed"))?;
        checkpoint_content.push(ContentBlock::text(COMPACTION_CHECKPOINT_PREFIX)?);
        checkpoint_content.extend(summary);
        checkpoint_content.push(ContentBlock::text(COMPACTION_CHECKPOINT_SUFFIX)?);
        let checkpoint = Message::user(
            next_id(driver.runtime, AgentIdKind::Message)?,
            checkpoint_content,
            MessageSource::from_value(json!({
                "kind": "plugin",
                "plugin": COMPACTION_CHECKPOINT_SOURCE,
                "compactionId": compaction_id.as_str(),
            }))?,
        )?;
        let checkpoint_tokens = reservation
            .session()
            .estimated_message_tokens(&checkpoint)
            .map_err(|_| AgentLoopError::Invariant("compaction checkpoint pricing failed"))?;
        let success_end = CompactionEndEvent::new(compaction_id.clone(), None, turn, None)?;
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(candidate.shadowed_seqs.len() + 2)
            .map_err(|_| AgentLoopError::Invariant("compaction source capacity failed"))?;
        sources.push(start_receipt.seq());
        Ok((checkpoint, checkpoint_tokens, success_end, sources))
    })();
    let Ok((checkpoint, checkpoint_tokens, success_end, mut sources)) = body else {
        return close_failed_bracket(
            reservation,
            &mut close,
            driver,
            CompactionOutcome::NoProgress,
        )
        .await;
    };
    if checkpoint_tokens >= candidate.shadowed_token_count {
        return close_failed_bracket(
            reservation,
            &mut close,
            driver,
            CompactionOutcome::NoProgress,
        )
        .await;
    }
    sources.push(summary_receipt.seq());
    sources.extend(candidate.shadowed_seqs);
    let checkpoint_event = NewEvent::surface(
        EventKind::user_message(checkpoint),
        SurfaceIntent::replace(candidate.range.start(), candidate.range.end(), sources),
    );
    match reservation.append_settled(checkpoint_event).await {
        Ok(_) => {}
        Err(error) if is_budget_error(&error) => {
            let outcome =
                CompactionOutcome::TurnError(driver.failure_for_budget(&error, budget_failure));
            return close_failed_bracket(reservation, &mut close, driver, outcome).await;
        }
        Err(error) => {
            if is_fatal_loop_error(&AgentLoopError::Session(error.clone())) {
                return Err(error.into());
            }
            return close_failed_bracket(
                reservation,
                &mut close,
                driver,
                CompactionOutcome::NoProgress,
            )
            .await;
        }
    }
    match reservation
        .settle_preferred_only_settled(
            &mut close.claim,
            NewEvent::log(EventKind::compaction_end(success_end)),
        )
        .await
    {
        Ok(_) => {}
        Err(first @ crate::session::AppendError::Clock(_))
            if reservation.session().is_durable() =>
        {
            match reservation
                .resume_preferred_only_settled(&mut close.claim)
                .await
            {
                Ok(_) => {}
                Err(crate::session::AppendError::Clock(_)) => return Err(first.into()),
                Err(error) => return Err(error.into()),
            }
        }
        Err(error) if is_fatal_loop_error(&AgentLoopError::Session(error.clone())) => {
            return Err(error.into());
        }
        Err(_) => {
            reservation.settle_exact_settled(&mut close.claim).await?;
        }
    }
    // A pre-commit resource rejection may force the protected error close
    // after the checkpoint has already replaced the surface. The replacement
    // is still real progress, so the ordinary request is rebuilt either way.
    match dispatch_barrier(reservation).await? {
        DispatchBarrier::Ready => Ok(CompactionOutcome::Progress),
        DispatchBarrier::ObserverUnavailable => {
            driver.observer_unavailable = true;
            Ok(CompactionOutcome::TurnError(failure_reason(
                "AGENT_OBSERVER_UNAVAILABLE",
                "the live session observer became unavailable",
            )?))
        }
    }
}

fn record_summary_usage(
    driver: &mut Driver<'_>,
    output_tokens: Option<u64>,
) -> Result<Option<crate::model::LlmFailure>, AgentLoopError> {
    let Some(output_tokens) = output_tokens else {
        return Ok(None);
    };
    driver.counters.reported_output_tokens = driver
        .counters
        .reported_output_tokens
        .checked_add(output_tokens)
        .unwrap_or(u64::MAX);
    if driver.counters.reported_output_tokens
        > driver.config.limits.max_reported_output_tokens_per_turn
    {
        return Ok(Some(failure_reason(
            "AGENT_TOKEN_BUDGET",
            "the agent reached its reported output-token limit",
        )?));
    }
    Ok(None)
}

async fn close_failed_bracket(
    reservation: &mut SessionReservation<'_>,
    close: &mut CompactionClose,
    driver: &mut Driver<'_>,
    outcome: CompactionOutcome,
) -> Result<CompactionOutcome, AgentLoopError> {
    let preferred_failure = match &outcome {
        CompactionOutcome::Cancelled => Some(failure_reason(
            "AGENT_COMPACTION_CANCELLED",
            "the automatic context summary was cancelled",
        )?),
        CompactionOutcome::AdvisoryFailure(failure) | CompactionOutcome::TurnError(failure) => {
            Some(failure.clone())
        }
        CompactionOutcome::Progress | CompactionOutcome::NoProgress => None,
    };
    if let Some(failure) = preferred_failure {
        let preferred =
            CompactionEndEvent::new(close.compaction_id.clone(), None, close.turn, Some(failure))
                .ok()
                .map(|end| NewEvent::log(EventKind::compaction_end(end)));
        if let Some(preferred) = preferred {
            match reservation
                .settle_settled(&mut close.claim, preferred)
                .await
            {
                Ok(_) => {}
                Err(error) if is_fatal_loop_error(&AgentLoopError::Session(error.clone())) => {
                    return Err(error.into());
                }
                Err(_) => {
                    reservation.settle_exact_settled(&mut close.claim).await?;
                }
            }
        } else {
            reservation.settle_exact_settled(&mut close.claim).await?;
        }
    } else {
        reservation.settle_exact_settled(&mut close.claim).await?;
    }
    match dispatch_barrier(reservation).await? {
        DispatchBarrier::Ready => Ok(outcome),
        DispatchBarrier::ObserverUnavailable => {
            driver.observer_unavailable = true;
            Ok(CompactionOutcome::TurnError(failure_reason(
                "AGENT_OBSERVER_UNAVAILABLE",
                "the live session observer became unavailable",
            )?))
        }
    }
}

fn prepared_snapshot(
    prepared: &crate::provider::PreparedProviderCall,
) -> Result<PreparedCompactionCallSnapshot, AgentLoopError> {
    let backoff = prepared.retry_policy().backoff();
    let backoff = PreparedRetryBackoffSnapshot::new(
        backoff.initial_delay_ms(),
        backoff.max_delay_ms(),
        backoff.jitter_ratio(),
    )?;
    let retry = match prepared.retry_policy().mode() {
        crate::provider::RetryMode::Normal => PreparedRetryPolicySnapshot::normal(
            prepared
                .retry_policy()
                .max_retries()
                .ok_or(AgentLoopError::Invariant(
                    "normal retry policy omitted maxRetries",
                ))?,
            prepared.retry_policy().retryable_codes().to_vec(),
            backoff,
        )?,
        crate::provider::RetryMode::Always => PreparedRetryPolicySnapshot::always(backoff)?,
    };
    Ok(PreparedCompactionCallSnapshot::new(
        prepared.config().clone(),
        prepared.adapter_defaults().clone(),
        prepared.context_window(),
        retry,
    )?)
}

struct CollectedSummary {
    outcome: CollectedSummaryOutcome,
    usage: Option<TokenUsage>,
}

enum CollectedSummaryOutcome {
    Success {
        summary: Vec<ContentBlock>,
        raw_output: Vec<ContentBlock>,
    },
    Failed(crate::model::LlmFailure),
    Cancelled,
    TimedOut,
    Invalid,
}

async fn collect_summary(
    mut stream: crate::provider::ProviderStream,
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
) -> CollectedSummary {
    let mut validator = StreamValidator::default();
    let mut usage = None;
    let mut blocks = Vec::<(u64, Option<ContentBlock>)>::new();
    if blocks.try_reserve(MAX_PROVIDER_STREAM_CHUNKS).is_err() {
        return CollectedSummary {
            outcome: CollectedSummaryOutcome::Invalid,
            usage,
        };
    }
    let mut positions = HashMap::<u64, usize>::new();
    if positions.try_reserve(MAX_PROVIDER_STREAM_CHUNKS).is_err() {
        return CollectedSummary {
            outcome: CollectedSummaryOutcome::Invalid,
            usage,
        };
    }
    let mut finish: Option<FinishReason> = None;
    let mut emitted_bytes = 0_usize;
    loop {
        let next = AssertUnwindSafe(stream.next()).catch_unwind();
        let item = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return CollectedSummary {
                outcome: CollectedSummaryOutcome::Cancelled,
                usage,
            },
            _ = tokio::time::sleep_until(deadline) => return CollectedSummary {
                outcome: CollectedSummaryOutcome::TimedOut,
                usage,
            },
            item = next => match item {
                Ok(item) => item,
                Err(_) => return CollectedSummary {
                    outcome: CollectedSummaryOutcome::Invalid,
                    usage,
                },
            },
        };
        let Some(item) = item else {
            break;
        };
        let Ok(chunk) = item else {
            return CollectedSummary {
                outcome: CollectedSummaryOutcome::Invalid,
                usage,
            };
        };
        emitted_bytes = match emitted_bytes.checked_add(chunk.raw().encoded_len()) {
            Some(bytes) if bytes <= MAX_COMPACTION_STREAM_BYTES => bytes,
            _ => {
                return CollectedSummary {
                    outcome: CollectedSummaryOutcome::Invalid,
                    usage,
                };
            }
        };
        if validator.accept(&chunk).is_err() {
            return CollectedSummary {
                outcome: CollectedSummaryOutcome::Invalid,
                usage,
            };
        }
        match chunk.kind() {
            StreamChunkKind::BlockStart { index, .. } => {
                let slot = blocks.len();
                blocks.push((index.get(), None));
                positions.insert(index.get(), slot);
            }
            StreamChunkKind::BlockEnd { index, block } => {
                let Some(slot) = positions.get(&index.get()).copied() else {
                    return CollectedSummary {
                        outcome: CollectedSummaryOutcome::Invalid,
                        usage,
                    };
                };
                blocks[slot].1 = Some(block.clone());
            }
            StreamChunkKind::Usage { usage: found } => usage = Some(found.clone()),
            StreamChunkKind::Finish { reason, .. } => finish = Some(reason.clone()),
            StreamChunkKind::TextDelta { .. }
            | StreamChunkKind::ReasoningDelta { .. }
            | StreamChunkKind::ToolCallDelta { .. } => {}
            StreamChunkKind::Other { .. } => {
                return CollectedSummary {
                    outcome: CollectedSummaryOutcome::Invalid,
                    usage,
                };
            }
        }
    }
    if validator.complete().is_err() {
        return CollectedSummary {
            outcome: CollectedSummaryOutcome::Invalid,
            usage,
        };
    }
    let Some(finish) = finish else {
        return CollectedSummary {
            outcome: CollectedSummaryOutcome::Invalid,
            usage,
        };
    };
    match finish.kind() {
        FinishReasonKind::Stop => {}
        FinishReasonKind::Error { failure } | FinishReasonKind::Aborted { failure } => {
            return CollectedSummary {
                outcome: CollectedSummaryOutcome::Failed(failure.clone()),
                usage,
            };
        }
        FinishReasonKind::ToolCalls
        | FinishReasonKind::MaxTokens
        | FinishReasonKind::Other { .. } => {
            return CollectedSummary {
                outcome: CollectedSummaryOutcome::Invalid,
                usage,
            };
        }
    }
    let Some(raw_output) = blocks
        .into_iter()
        .map(|(_, block)| block)
        .collect::<Option<Vec<_>>>()
    else {
        return CollectedSummary {
            outcome: CollectedSummaryOutcome::Invalid,
            usage,
        };
    };
    if raw_output.iter().any(|block| {
        !matches!(
            block.kind(),
            ContentBlockKind::Text { .. } | ContentBlockKind::Reasoning { .. }
        )
    }) {
        return CollectedSummary {
            outcome: CollectedSummaryOutcome::Invalid,
            usage,
        };
    }
    let summary = raw_output
        .iter()
        .filter(|block| matches!(block.kind(), ContentBlockKind::Text { .. }))
        .cloned()
        .collect::<Vec<_>>();
    if summary.is_empty()
        || !summary.iter().any(
            |block| matches!(block.kind(), ContentBlockKind::Text { text } if !text.trim().is_empty()),
        )
    {
        return CollectedSummary {
            outcome: CollectedSummaryOutcome::Invalid,
            usage,
        };
    }
    CollectedSummary {
        outcome: CollectedSummaryOutcome::Success {
            summary,
            raw_output,
        },
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::{pressure_reached, retained_token_target};

    #[test]
    fn pressure_and_retained_tail_use_the_fixed_integer_boundaries() {
        assert!(!pressure_reached(799, 1_000));
        assert!(pressure_reached(800, 1_000));
        assert!(pressure_reached(801, 1_000));
        assert_eq!(retained_token_target(Some(1_000)), 160);
        assert_eq!(retained_token_target(None), 0);
    }
}
