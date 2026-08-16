//! Session-owned fold for one provider stream attempt.
//!
//! Raw chunks are durable facts, but they are not independently meaningful:
//! one final assistant message or retry must close exactly one ordered chunk
//! span. This module keeps that relationship in Session so live execution and
//! cold recovery cannot assemble the same rows in different ways.

use std::{
    collections::HashMap,
    io::{self, Write},
};

use aws_lc_rs::digest::{Context, SHA256};
use serde::Serializer as _;
use thiserror::Error;

use crate::model::{
    ContentBlock, ContentBlockKind, FinishReason, FinishReasonKind, JsonValue, LlmFailure,
    MAX_PROVIDER_STREAM_CHUNKS, Message, MessageSourceKind, PreparedStreamTransition, StreamChunk,
    StreamChunkKind, StreamProtocolError, StreamValidator, TokenUsage,
};

use super::{
    EpochHeader, EventKind, EventSeq, SessionEvent, StepId, TurnId,
    context_budget::estimate_provider_assistant,
};

pub(super) const MAX_ATTEMPT_EMITTED_BYTES: usize = 10 * 1024 * 1024;

const CONTENT_DIGEST_DOMAIN: &[u8] = b"dsh.attempt-content.v1\0";
const SOURCE_DIGEST_DOMAIN: &[u8] = b"dsh.attempt-sources.v1\0";
const REPLAY_DIGEST_DOMAIN: &[u8] = b"dsh.attempt-replay.v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttemptDisposition {
    Committed,
    Retry,
    ContextOverflow,
    Failed,
    Cancelled,
    Interrupted,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AttemptError {
    #[error("attempt bookkeeping could not reserve its bounded capacity")]
    Capacity,
    #[error("{event_type} does not match the current provider attempt: {detail}")]
    Boundary {
        event_type: &'static str,
        detail: &'static str,
    },
    #[error("provider attempt has no canonical request route")]
    MissingRoute,
    #[error(transparent)]
    Stream(#[from] StreamProtocolError),
    #[error("provider attempt emitted {actual} JSON bytes; maximum is {maximum}")]
    EmittedBytes { maximum: usize, actual: usize },
    #[error("provider attempt usage totals exceeded their bounded integer domain")]
    UsageOverflow,
    #[error("provider attempt token estimation exceeded its bounded integer domain")]
    TokenEstimateOverflow,
    #[error("assistant/message does not cite the complete ordered attempt chunk span")]
    SourceMismatch,
    #[error("assistant/message content does not match its provider attempt")]
    ContentMismatch,
    #[error("assistant/message usage does not match its provider attempt")]
    UsageMismatch,
    #[error("assistant/message route or replay state does not match its provider attempt")]
    RouteMismatch,
    #[error("llm/retry failure does not match its terminal provider attempt")]
    FailureMismatch,
    #[error("provider attempt content could not be encoded for an identity digest")]
    Digest,
    #[error("provider attempt ownership changed before its prepared transition committed")]
    OwnershipChanged,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct UsageTotals {
    uncached_input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct UsageSample {
    turn: TurnId,
    step: StepId,
    buckets: UsageTotals,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct UsageProjection {
    totals: UsageTotals,
    last: Option<UsageSample>,
}

impl UsageProjection {
    fn with_sample(
        &self,
        turn: TurnId,
        step: StepId,
        usage: &TokenUsage,
    ) -> Result<Self, AttemptError> {
        let buckets = usage_buckets(usage);
        let previous = self
            .last
            .as_ref()
            .filter(|sample| sample.turn == turn && sample.step == step)
            .map(|sample| sample.buckets)
            .unwrap_or_default();
        let totals = UsageTotals {
            uncached_input_tokens: replace_bucket(
                self.totals.uncached_input_tokens,
                previous.uncached_input_tokens,
                buckets.uncached_input_tokens,
            )?,
            output_tokens: replace_bucket(
                self.totals.output_tokens,
                previous.output_tokens,
                buckets.output_tokens,
            )?,
            cache_read_tokens: replace_bucket(
                self.totals.cache_read_tokens,
                previous.cache_read_tokens,
                buckets.cache_read_tokens,
            )?,
            cache_write_tokens: replace_bucket(
                self.totals.cache_write_tokens,
                previous.cache_write_tokens,
                buckets.cache_write_tokens,
            )?,
            reasoning_tokens: replace_bucket(
                self.totals.reasoning_tokens,
                previous.reasoning_tokens,
                buckets.reasoning_tokens,
            )?,
        };
        Ok(Self {
            totals,
            last: Some(UsageSample {
                turn,
                step,
                buckets,
            }),
        })
    }
}

fn usage_buckets(usage: &TokenUsage) -> UsageTotals {
    UsageTotals {
        uncached_input_tokens: usage.input_tokens().get(),
        output_tokens: usage.output_tokens().get(),
        cache_read_tokens: usage.cache_read_tokens().map_or(0, |value| value.get()),
        cache_write_tokens: usage.cache_write_tokens().map_or(0, |value| value.get()),
        reasoning_tokens: usage.reasoning_tokens().map_or(0, |value| value.get()),
    }
}

fn replace_bucket(total: u64, previous: u64, next: u64) -> Result<u64, AttemptError> {
    total
        .checked_sub(previous)
        .and_then(|value| value.checked_add(next))
        .ok_or(AttemptError::UsageOverflow)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttemptRoute {
    provider: String,
    model: String,
}

impl AttemptRoute {
    fn from_header(header: Option<&EpochHeader>) -> Result<Self, AttemptError> {
        let header = header.ok_or(AttemptError::MissingRoute)?;
        Ok(Self {
            provider: header.config.provider().to_owned(),
            model: header.config.model().to_owned(),
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
struct PartialBlock {
    block: Option<ContentBlock>,
}

#[derive(Debug, Eq, PartialEq)]
struct OpenAttempt {
    generation: u64,
    turn: TurnId,
    step: StepId,
    route: AttemptRoute,
    validator: StreamValidator,
    order: Vec<u64>,
    blocks: HashMap<u64, PartialBlock>,
    sources: Vec<EventSeq>,
    emitted_bytes: usize,
    usage: Option<TokenUsage>,
}

impl OpenAttempt {
    fn try_new(
        generation: u64,
        turn: TurnId,
        step: StepId,
        route: AttemptRoute,
    ) -> Result<Self, AttemptError> {
        let validator = StreamValidator::try_bounded().map_err(|_| AttemptError::Capacity)?;
        let mut order = Vec::new();
        order
            .try_reserve_exact(MAX_PROVIDER_STREAM_CHUNKS)
            .map_err(|_| AttemptError::Capacity)?;
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(MAX_PROVIDER_STREAM_CHUNKS)
            .map_err(|_| AttemptError::Capacity)?;
        let mut blocks = HashMap::new();
        blocks
            .try_reserve(MAX_PROVIDER_STREAM_CHUNKS)
            .map_err(|_| AttemptError::Capacity)?;
        Ok(Self {
            generation,
            turn,
            step,
            route,
            validator,
            order,
            blocks,
            sources,
            emitted_bytes: 0,
            usage: None,
        })
    }

    fn prepare_chunk(
        &self,
        seq: EventSeq,
        chunk: &StreamChunk,
        usage_projection: &UsageProjection,
    ) -> Result<PreparedExistingChunk, AttemptError> {
        let stream = self.validator.prepare(chunk)?;
        let emitted_bytes = self
            .emitted_bytes
            .checked_add(chunk.raw().encoded_len())
            .ok_or(AttemptError::EmittedBytes {
                maximum: MAX_ATTEMPT_EMITTED_BYTES,
                actual: usize::MAX,
            })?;
        if emitted_bytes > MAX_ATTEMPT_EMITTED_BYTES {
            return Err(AttemptError::EmittedBytes {
                maximum: MAX_ATTEMPT_EMITTED_BYTES,
                actual: emitted_bytes,
            });
        }
        let mut next_usage = None;
        let operation = match chunk.kind() {
            StreamChunkKind::BlockStart { index, .. } => {
                PreparedChunkOperation::BlockStart { index: index.get() }
            }
            StreamChunkKind::BlockEnd { index, block } => PreparedChunkOperation::BlockEnd {
                index: index.get(),
                block: block.clone(),
            },
            StreamChunkKind::Usage { usage } => {
                next_usage = Some(usage_projection.with_sample(self.turn, self.step, usage)?);
                PreparedChunkOperation::Usage {
                    usage: usage.clone(),
                }
            }
            StreamChunkKind::Finish {
                reason,
                replay_state,
            } => {
                let content = if matches!(
                    reason.kind(),
                    FinishReasonKind::Error { .. } | FinishReasonKind::Aborted { .. }
                ) {
                    None
                } else {
                    let mut content = Vec::new();
                    content
                        .try_reserve_exact(self.order.len())
                        .map_err(|_| AttemptError::Capacity)?;
                    for index in &self.order {
                        self.blocks
                            .get(index)
                            .and_then(|partial| partial.block.as_ref())
                            .ok_or(AttemptError::Boundary {
                                event_type: "assistant/chunk",
                                detail: "successful finish has an incomplete block",
                            })?;
                    }
                    Some(content)
                };
                let normalized_digest = content
                    .as_ref()
                    .map(|_| self.normalized_content_digest(reason))
                    .transpose()?;
                let provider_assistant_tokens = content
                    .as_ref()
                    .map(|_| self.provider_assistant_tokens(reason))
                    .transpose()?;
                PreparedChunkOperation::Finish {
                    reason: reason.clone(),
                    replay_state: replay_state.clone(),
                    content,
                    normalized_digest,
                    provider_assistant_tokens,
                }
            }
            StreamChunkKind::TextDelta { .. }
            | StreamChunkKind::ReasoningDelta { .. }
            | StreamChunkKind::ToolCallDelta { .. } => PreparedChunkOperation::Continue,
            StreamChunkKind::Other { .. } => {
                return Err(AttemptError::Boundary {
                    event_type: "assistant/chunk",
                    detail: "unknown live chunks are not durable attempt facts",
                });
            }
        };
        Ok(PreparedExistingChunk {
            seq,
            stream,
            expected_generation: self.generation,
            expected_source_count: self.sources.len(),
            emitted_bytes,
            operation,
            next_usage,
        })
    }

    fn commit(
        mut self,
        prepared: PreparedExistingChunk,
    ) -> Option<(AttemptState, Option<UsageProjection>)> {
        self.validator.commit(prepared.stream);
        self.sources.push(prepared.seq);
        self.emitted_bytes = prepared.emitted_bytes;
        let next_usage = prepared.next_usage;
        Some(match prepared.operation {
            PreparedChunkOperation::Continue => (AttemptState::Streaming(self), next_usage),
            PreparedChunkOperation::BlockStart { index } => {
                self.order.push(index);
                self.blocks.insert(index, PartialBlock { block: None });
                (AttemptState::Streaming(self), next_usage)
            }
            PreparedChunkOperation::BlockEnd { index, block } => {
                let partial = self.blocks.get_mut(&index)?;
                partial.block = Some(block);
                (AttemptState::Streaming(self), next_usage)
            }
            PreparedChunkOperation::Usage { usage } => {
                self.usage = Some(usage);
                (AttemptState::Streaming(self), next_usage)
            }
            PreparedChunkOperation::Finish {
                reason,
                replay_state,
                mut content,
                normalized_digest,
                provider_assistant_tokens,
            } => {
                if let Some(content) = &mut content {
                    for index in &self.order {
                        let block = self.blocks.remove(index)?.block?;
                        content.push(block);
                    }
                }
                (
                    AttemptState::Finished(FinishedAttempt {
                        generation: self.generation,
                        turn: self.turn,
                        step: self.step,
                        route: self.route,
                        sources: self.sources,
                        usage: self.usage,
                        reason,
                        replay_state,
                        content,
                        normalized_digest,
                        provider_assistant_tokens,
                    }),
                    next_usage,
                )
            }
        })
    }

    fn normalized_content_digest(&self, reason: &FinishReason) -> Result<[u8; 32], AttemptError> {
        let blocks = self.order.iter().filter_map(|index| {
            self.blocks
                .get(index)
                .and_then(|partial| partial.block.as_ref())
        });
        if matches!(reason.kind(), FinishReasonKind::MaxTokens) {
            content_digest(
                blocks.filter(|block| !matches!(block.kind(), ContentBlockKind::ToolCall { .. })),
            )
        } else {
            content_digest(blocks)
        }
    }

    fn provider_assistant_tokens(&self, reason: &FinishReason) -> Result<u64, AttemptError> {
        let blocks = self.order.iter().filter_map(|index| {
            self.blocks
                .get(index)
                .and_then(|partial| partial.block.as_ref())
        });
        if matches!(reason.kind(), FinishReasonKind::MaxTokens) {
            estimate_provider_assistant(
                blocks.filter(|block| !matches!(block.kind(), ContentBlockKind::ToolCall { .. })),
            )
        } else {
            estimate_provider_assistant(blocks)
        }
        .map_err(|_| AttemptError::TokenEstimateOverflow)
    }
}

pub(super) struct PreparedExistingChunk {
    seq: EventSeq,
    stream: PreparedStreamTransition,
    expected_generation: u64,
    expected_source_count: usize,
    emitted_bytes: usize,
    operation: PreparedChunkOperation,
    next_usage: Option<UsageProjection>,
}

enum PreparedChunkOperation {
    Continue,
    BlockStart {
        index: u64,
    },
    BlockEnd {
        index: u64,
        block: ContentBlock,
    },
    Usage {
        usage: TokenUsage,
    },
    Finish {
        reason: FinishReason,
        replay_state: Option<JsonValue>,
        content: Option<Vec<ContentBlock>>,
        normalized_digest: Option<[u8; 32]>,
        provider_assistant_tokens: Option<u64>,
    },
}

#[derive(Debug, Eq, PartialEq)]
struct FinishedAttempt {
    generation: u64,
    turn: TurnId,
    step: StepId,
    route: AttemptRoute,
    sources: Vec<EventSeq>,
    usage: Option<TokenUsage>,
    reason: FinishReason,
    replay_state: Option<JsonValue>,
    content: Option<Vec<ContentBlock>>,
    normalized_digest: Option<[u8; 32]>,
    provider_assistant_tokens: Option<u64>,
}

#[derive(Debug, Eq, PartialEq)]
struct SealedAttempt {
    generation: u64,
    turn: TurnId,
    step: StepId,
    route: AttemptRoute,
    source_count: usize,
    source_digest: [u8; 32],
    usage: Option<TokenUsage>,
    reason: FinishReason,
    replay_digest: Option<[u8; 32]>,
    normalized_digest: Option<[u8; 32]>,
    provider_assistant_tokens: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CommittedAttemptFacts {
    usage: Option<TokenUsage>,
    provider_assistant_tokens: u64,
}

impl CommittedAttemptFacts {
    pub(super) fn usage(&self) -> Option<&TokenUsage> {
        self.usage.as_ref()
    }

    pub(super) fn provider_assistant_tokens(&self) -> u64 {
        self.provider_assistant_tokens
    }
}

#[derive(Debug, Eq, PartialEq)]
enum AttemptState {
    OutsideStep,
    Ready { turn: TurnId, step: StepId },
    Streaming(OpenAttempt),
    Finished(FinishedAttempt),
    Sealed(SealedAttempt),
    RetryScheduled { turn: TurnId, step: StepId },
    Committed { turn: TurnId, step: StepId },
}

/// Compact process-local proof that recovery is closing the same incomplete
/// provider attempt that the cold scanner reconstructed.
///
/// The proof contains no model text. Its generation and ordered source digest
/// prevent a recovery action prepared for one stream from being reused for a
/// later stream in the same step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RecoveryAttemptProof {
    turn: TurnId,
    step: StepId,
    generation: u64,
    phase: RecoveryAttemptPhase,
    source_count: usize,
    source_digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryAttemptPhase {
    Streaming,
    Finished,
    Sealed,
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct AttemptProjection {
    state: AttemptState,
    usage: UsageProjection,
    next_generation: u64,
    context_overflow_used: bool,
    context_overflow_start_used: bool,
    context_overflow_replacement_generation: Option<u64>,
}

pub(super) enum PreparedAttemptChunk {
    Replace {
        turn: TurnId,
        step: StepId,
        expected_generation: u64,
        next: AttemptProjection,
    },
    Continue(PreparedExistingChunk),
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct PreparedAttempt {
    content: Vec<ContentBlock>,
    usage: Option<TokenUsage>,
    finish: FinishReason,
    replay_state: Option<JsonValue>,
    sources: Vec<EventSeq>,
}

impl PreparedAttempt {
    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<ContentBlock>,
        Option<TokenUsage>,
        FinishReason,
        Option<JsonValue>,
        Vec<EventSeq>,
    ) {
        (
            self.content,
            self.usage,
            self.finish,
            self.replay_state,
            self.sources,
        )
    }
}

impl Default for AttemptProjection {
    fn default() -> Self {
        Self {
            state: AttemptState::OutsideStep,
            usage: UsageProjection::default(),
            next_generation: 1,
            context_overflow_used: false,
            context_overflow_start_used: false,
            context_overflow_replacement_generation: None,
        }
    }
}

impl AttemptProjection {
    pub(super) fn is_outside_step(&self) -> bool {
        matches!(self.state, AttemptState::OutsideStep)
    }

    #[cfg(test)]
    pub(super) fn usage_totals_for_test(&self) -> (u64, u64, u64, u64, u64) {
        let totals = self.usage.totals;
        (
            totals.uncached_input_tokens,
            totals.output_tokens,
            totals.cache_read_tokens,
            totals.cache_write_tokens,
            totals.reasoning_tokens,
        )
    }

    pub(super) fn has_open_attempt(&self) -> bool {
        matches!(
            self.state,
            AttemptState::Streaming(_) | AttemptState::Finished(_) | AttemptState::Sealed(_)
        )
    }

    pub(super) fn recovery_proof(&self) -> Option<RecoveryAttemptProof> {
        match &self.state {
            AttemptState::Streaming(open) => Some(RecoveryAttemptProof {
                turn: open.turn,
                step: open.step,
                generation: open.generation,
                phase: RecoveryAttemptPhase::Streaming,
                source_count: open.sources.len(),
                source_digest: source_digest(&open.sources),
            }),
            AttemptState::Finished(finished) => Some(RecoveryAttemptProof {
                turn: finished.turn,
                step: finished.step,
                generation: finished.generation,
                phase: RecoveryAttemptPhase::Finished,
                source_count: finished.sources.len(),
                source_digest: source_digest(&finished.sources),
            }),
            AttemptState::Sealed(sealed) => Some(RecoveryAttemptProof {
                turn: sealed.turn,
                step: sealed.step,
                generation: sealed.generation,
                phase: RecoveryAttemptPhase::Sealed,
                source_count: sealed.source_count,
                source_digest: sealed.source_digest,
            }),
            AttemptState::OutsideStep
            | AttemptState::Ready { .. }
            | AttemptState::RetryScheduled { .. }
            | AttemptState::Committed { .. } => None,
        }
    }

    pub(super) fn interrupt_for_recovery(
        &self,
        turn: TurnId,
        step: StepId,
        proof: &RecoveryAttemptProof,
    ) -> Result<Self, AttemptError> {
        if self.recovery_proof().as_ref() != Some(proof) {
            return Err(boundary(
                "step/end",
                "the recovery attempt proof no longer matches the open stream",
            ));
        }
        self.step_end(turn, step, Some(AttemptDisposition::Interrupted))
    }

    pub(super) fn step_start(&self, turn: TurnId, step: StepId) -> Result<Self, AttemptError> {
        if !matches!(self.state, AttemptState::OutsideStep) {
            return Err(boundary(
                "step/start",
                "an earlier step still owns attempt state",
            ));
        }
        Ok(Self {
            state: AttemptState::Ready { turn, step },
            usage: self.usage.clone(),
            next_generation: self.next_generation,
            context_overflow_used: false,
            context_overflow_start_used: false,
            context_overflow_replacement_generation: None,
        })
    }

    pub(super) fn begin_live(
        &self,
        turn: TurnId,
        step: StepId,
        header: Option<&EpochHeader>,
        replacement_generation: u64,
    ) -> Result<Self, AttemptError> {
        self.require_ready("provider dispatch", turn, step)?;
        self.require_context_replay_progress("provider dispatch", replacement_generation)?;
        let next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(AttemptError::Capacity)?;
        Ok(Self {
            state: AttemptState::Streaming(OpenAttempt::try_new(
                self.next_generation,
                turn,
                step,
                AttemptRoute::from_header(header)?,
            )?),
            usage: self.usage.clone(),
            next_generation,
            context_overflow_used: self.context_overflow_used,
            context_overflow_start_used: self.context_overflow_start_used,
            context_overflow_replacement_generation: self.context_overflow_replacement_generation,
        })
    }

    pub(super) fn prepare_chunk(
        &self,
        event: &SessionEvent,
        header: Option<&EpochHeader>,
        replacement_generation: u64,
    ) -> Result<PreparedAttemptChunk, AttemptError> {
        let EventKind::AssistantChunk { turn, step, chunk } = event.kind() else {
            return Err(boundary(
                "assistant/chunk",
                "the event is not a stream chunk",
            ));
        };
        match &self.state {
            AttemptState::Ready {
                turn: ready_turn,
                step: ready_step,
            } if ready_turn == turn && ready_step == step => {
                self.require_context_replay_progress("assistant/chunk", replacement_generation)?;
                let next_generation = self
                    .next_generation
                    .checked_add(1)
                    .ok_or(AttemptError::Capacity)?;
                let open = OpenAttempt::try_new(
                    self.next_generation,
                    *turn,
                    *step,
                    AttemptRoute::from_header(header)?,
                )?;
                let prepared = open.prepare_chunk(event.seq(), chunk, &self.usage)?;
                let (state, usage) = open
                    .commit(prepared)
                    .ok_or(AttemptError::OwnershipChanged)?;
                Ok(PreparedAttemptChunk::Replace {
                    turn: *turn,
                    step: *step,
                    expected_generation: self.next_generation,
                    next: Self {
                        state,
                        usage: usage.unwrap_or_else(|| self.usage.clone()),
                        next_generation,
                        context_overflow_used: self.context_overflow_used,
                        context_overflow_start_used: self.context_overflow_start_used,
                        context_overflow_replacement_generation: self
                            .context_overflow_replacement_generation,
                    },
                })
            }
            AttemptState::Streaming(open) if open.turn == *turn && open.step == *step => {
                Ok(PreparedAttemptChunk::Continue(open.prepare_chunk(
                    event.seq(),
                    chunk,
                    &self.usage,
                )?))
            }
            _ => Err(boundary(
                "assistant/chunk",
                "no matching ready or streaming attempt exists",
            )),
        }
    }

    pub(super) fn commit_chunk(&mut self, prepared: PreparedAttemptChunk) -> bool {
        match prepared {
            PreparedAttemptChunk::Replace {
                turn,
                step,
                expected_generation,
                next,
            } => {
                if !matches!(
                    self.state,
                    AttemptState::Ready {
                        turn: ready_turn,
                        step: ready_step,
                    } if ready_turn == turn && ready_step == step
                ) {
                    return false;
                }
                if self.next_generation != expected_generation {
                    return false;
                }
                *self = next;
            }
            PreparedAttemptChunk::Continue(prepared) => {
                if !matches!(
                    &self.state,
                    AttemptState::Streaming(open)
                        if open.generation == prepared.expected_generation
                            && open.sources.len() == prepared.expected_source_count
                ) {
                    return false;
                }
                let state = std::mem::replace(&mut self.state, AttemptState::OutsideStep);
                let AttemptState::Streaming(open) = state else {
                    return false;
                };
                let Some((state, usage)) = open.commit(prepared) else {
                    return false;
                };
                self.state = state;
                if let Some(usage) = usage {
                    self.usage = usage;
                }
            }
        }
        true
    }

    pub(super) fn take_prepared(&mut self) -> Result<PreparedAttempt, AttemptError> {
        let replay_digest = match &self.state {
            AttemptState::Finished(finished) => finished
                .replay_state
                .as_ref()
                .map(json_digest)
                .transpose()?,
            _ => {
                return Err(boundary(
                    "provider attempt seal",
                    "the stream has no terminal finish",
                ));
            }
        };
        let state = std::mem::replace(&mut self.state, AttemptState::OutsideStep);
        let AttemptState::Finished(finished) = state else {
            // The immutable preflight above established this exact variant.
            self.state = state;
            return Err(AttemptError::OwnershipChanged);
        };
        let FinishedAttempt {
            generation,
            turn,
            step,
            route,
            sources,
            usage,
            reason,
            replay_state,
            content,
            normalized_digest,
            provider_assistant_tokens,
        } = finished;
        let content = content.unwrap_or_default();
        let sealed = SealedAttempt {
            generation,
            turn,
            step,
            route,
            source_count: sources.len(),
            source_digest: source_digest(&sources),
            usage: usage.clone(),
            reason: reason.clone(),
            replay_digest,
            normalized_digest,
            provider_assistant_tokens,
        };
        let prepared = PreparedAttempt {
            content,
            usage,
            finish: reason,
            replay_state,
            sources,
        };
        self.state = AttemptState::Sealed(sealed);
        Ok(prepared)
    }

    pub(super) fn assistant(
        &self,
        event: &SessionEvent,
    ) -> Result<(Self, CommittedAttemptFacts), AttemptError> {
        let EventKind::AssistantMessage {
            turn,
            step,
            message,
            usage,
        } = event.kind()
        else {
            return Err(boundary(
                "assistant/message",
                "the event is not an assistant message",
            ));
        };
        let (
            attempt_turn,
            attempt_step,
            route,
            expected_usage,
            reason,
            replay_digest,
            digest,
            provider_assistant_tokens,
        ) = match &self.state {
            AttemptState::Finished(finished) => {
                if finished.sources.as_slice() != event.source_event_seqs().unwrap_or_default() {
                    return Err(AttemptError::SourceMismatch);
                }
                (
                    finished.turn,
                    finished.step,
                    &finished.route,
                    &finished.usage,
                    &finished.reason,
                    finished
                        .replay_state
                        .as_ref()
                        .map(json_digest)
                        .transpose()?,
                    finished.normalized_digest,
                    finished.provider_assistant_tokens,
                )
            }
            AttemptState::Sealed(sealed) => {
                let sources = event.source_event_seqs().unwrap_or_default();
                if sources.len() != sealed.source_count
                    || source_digest(sources) != sealed.source_digest
                {
                    return Err(AttemptError::SourceMismatch);
                }
                (
                    sealed.turn,
                    sealed.step,
                    &sealed.route,
                    &sealed.usage,
                    &sealed.reason,
                    sealed.replay_digest,
                    sealed.normalized_digest,
                    sealed.provider_assistant_tokens,
                )
            }
            _ => {
                return Err(boundary(
                    "assistant/message",
                    "no matching finished attempt exists",
                ));
            }
        };
        if attempt_turn != *turn || attempt_step != *step {
            return Err(boundary(
                "assistant/message",
                "turn or step does not match the attempt",
            ));
        }
        if matches!(
            reason.kind(),
            FinishReasonKind::Error { .. } | FinishReasonKind::Aborted { .. }
        ) {
            return Err(boundary(
                "assistant/message",
                "a failed attempt cannot commit an assistant message",
            ));
        }
        if expected_usage != usage {
            return Err(AttemptError::UsageMismatch);
        }
        validate_message_route(message, route, replay_digest)?;
        if matches!(reason.kind(), FinishReasonKind::MaxTokens)
            && message
                .content()
                .iter()
                .any(|block| matches!(block.kind(), ContentBlockKind::ToolCall { .. }))
        {
            return Err(AttemptError::ContentMismatch);
        }
        if content_digest(message.content().iter())?
            != digest.ok_or(AttemptError::ContentMismatch)?
        {
            return Err(AttemptError::ContentMismatch);
        }
        let usage_projection = match usage {
            Some(usage) => self.usage.with_sample(*turn, *step, usage)?,
            None => self.usage.clone(),
        };
        Ok((
            Self {
                state: AttemptState::Committed {
                    turn: *turn,
                    step: *step,
                },
                usage: usage_projection,
                next_generation: self.next_generation,
                context_overflow_used: self.context_overflow_used,
                context_overflow_start_used: self.context_overflow_start_used,
                context_overflow_replacement_generation: self
                    .context_overflow_replacement_generation,
            },
            CommittedAttemptFacts {
                usage: usage.clone(),
                provider_assistant_tokens: provider_assistant_tokens
                    .ok_or(AttemptError::TokenEstimateOverflow)?,
            },
        ))
    }

    pub(super) fn retry(&self, event: &SessionEvent) -> Result<Self, AttemptError> {
        let EventKind::LlmRetry { retry } = event.kind() else {
            return Err(boundary("llm/retry", "the event is not a retry schedule"));
        };
        let (turn, step, route, reason) = match &self.state {
            AttemptState::Finished(finished) => (
                finished.turn,
                finished.step,
                &finished.route,
                &finished.reason,
            ),
            AttemptState::Sealed(sealed) => {
                (sealed.turn, sealed.step, &sealed.route, &sealed.reason)
            }
            _ => {
                return Err(boundary(
                    "llm/retry",
                    "no matching failed terminal attempt exists",
                ));
            }
        };
        if turn != retry.turn() || step != retry.step() || route.provider != retry.provider() {
            return Err(boundary(
                "llm/retry",
                "route, turn, or step does not match the attempt",
            ));
        }
        let failure = terminal_failure(reason).ok_or(AttemptError::FailureMismatch)?;
        if failure != retry.failure() {
            return Err(AttemptError::FailureMismatch);
        }
        Ok(Self {
            state: AttemptState::RetryScheduled { turn, step },
            usage: self.usage.clone(),
            next_generation: self.next_generation,
            context_overflow_used: self.context_overflow_used,
            context_overflow_start_used: self.context_overflow_start_used,
            context_overflow_replacement_generation: self.context_overflow_replacement_generation,
        })
    }

    /// Consume the one terminal context-window failure that authorizes a
    /// compaction transaction inside this step.
    pub(super) fn context_overflow(
        &self,
        turn: TurnId,
        step: StepId,
        replacement_generation: u64,
        starts_compaction: bool,
    ) -> Result<Self, AttemptError> {
        if self.context_overflow_used {
            return Err(boundary(
                "context-overflow compaction",
                "this step already consumed its context-overflow recovery",
            ));
        }
        let (attempt_turn, attempt_step, reason) = match &self.state {
            AttemptState::Finished(finished) => (finished.turn, finished.step, &finished.reason),
            AttemptState::Sealed(sealed) => (sealed.turn, sealed.step, &sealed.reason),
            _ => {
                return Err(boundary(
                    "context-overflow compaction",
                    "no matching terminal provider attempt exists",
                ));
            }
        };
        if attempt_turn != turn || attempt_step != step {
            return Err(boundary(
                "context-overflow compaction",
                "turn or step does not match the terminal attempt",
            ));
        }
        let is_context_overflow = matches!(
            reason.kind(),
            FinishReasonKind::Error { failure }
                if failure.code() == "CONTEXT_WINDOW_EXCEEDED"
        );
        if !is_context_overflow {
            return Err(AttemptError::FailureMismatch);
        }
        Ok(Self {
            state: AttemptState::Ready { turn, step },
            usage: self.usage.clone(),
            next_generation: self.next_generation,
            context_overflow_used: true,
            context_overflow_start_used: starts_compaction,
            context_overflow_replacement_generation: Some(replacement_generation),
        })
    }

    pub(super) fn context_overflow_start(
        &self,
        turn: TurnId,
        step: StepId,
    ) -> Result<Self, AttemptError> {
        self.require_ready("compaction/start", turn, step)?;
        if !self.context_overflow_used || self.context_overflow_start_used {
            return Err(boundary(
                "compaction/start",
                "the context-overflow compaction start is missing or duplicated",
            ));
        }
        Ok(Self {
            state: AttemptState::Ready { turn, step },
            usage: self.usage.clone(),
            next_generation: self.next_generation,
            context_overflow_used: true,
            context_overflow_start_used: true,
            context_overflow_replacement_generation: self.context_overflow_replacement_generation,
        })
    }

    pub(super) fn context_overflow_was_used(&self) -> bool {
        self.context_overflow_used
    }

    pub(super) fn retry_started(&self, turn: TurnId, step: StepId) -> Result<Self, AttemptError> {
        if !matches!(
            self.state,
            AttemptState::RetryScheduled {
                turn: scheduled_turn,
                step: scheduled_step,
            } if scheduled_turn == turn && scheduled_step == step
        ) {
            return Err(boundary(
                "llm/retry-started",
                "no matching retry schedule owns the attempt slot",
            ));
        }
        Ok(Self {
            state: AttemptState::Ready { turn, step },
            usage: self.usage.clone(),
            next_generation: self.next_generation,
            context_overflow_used: self.context_overflow_used,
            context_overflow_start_used: self.context_overflow_start_used,
            context_overflow_replacement_generation: self.context_overflow_replacement_generation,
        })
    }

    pub(super) fn step_end(
        &self,
        turn: TurnId,
        step: StepId,
        disposition: Option<AttemptDisposition>,
    ) -> Result<Self, AttemptError> {
        let ordinary = matches!(
            self.state,
            AttemptState::Ready {
                turn: state_turn,
                step: state_step,
            }
                | AttemptState::RetryScheduled {
                    turn: state_turn,
                    step: state_step,
                }
                | AttemptState::Committed {
                    turn: state_turn,
                    step: state_step,
                } if state_turn == turn && state_step == step
        );
        if ordinary {
            if disposition.is_some() {
                return Err(boundary(
                    "step/end",
                    "a closed or never-started attempt cannot be closed twice",
                ));
            }
        } else {
            let open_matches = match &self.state {
                AttemptState::Streaming(open) => open.turn == turn && open.step == step,
                AttemptState::Finished(finished) => finished.turn == turn && finished.step == step,
                AttemptState::Sealed(sealed) => sealed.turn == turn && sealed.step == step,
                _ => false,
            };
            if !open_matches
                || !matches!(
                    disposition,
                    Some(
                        AttemptDisposition::Failed
                            | AttemptDisposition::Cancelled
                            | AttemptDisposition::Interrupted
                    )
                )
            {
                return Err(boundary(
                    "step/end",
                    "an open attempt requires an explicit noncommitted disposition",
                ));
            }
        }
        Ok(Self {
            state: AttemptState::OutsideStep,
            usage: self.usage.clone(),
            next_generation: self.next_generation,
            context_overflow_used: false,
            context_overflow_start_used: false,
            context_overflow_replacement_generation: None,
        })
    }

    fn require_ready(
        &self,
        event_type: &'static str,
        turn: TurnId,
        step: StepId,
    ) -> Result<(), AttemptError> {
        if matches!(
            self.state,
            AttemptState::Ready {
                turn: ready_turn,
                step: ready_step,
            } if ready_turn == turn && ready_step == step
        ) {
            Ok(())
        } else {
            Err(boundary(
                event_type,
                "the step is not ready for a new attempt",
            ))
        }
    }

    fn require_context_replay_progress(
        &self,
        event_type: &'static str,
        replacement_generation: u64,
    ) -> Result<(), AttemptError> {
        if self.context_overflow_used
            && self
                .context_overflow_replacement_generation
                .is_none_or(|closed| replacement_generation <= closed)
        {
            return Err(boundary(
                event_type,
                "context-overflow replay requires a durable compaction replacement",
            ));
        }
        Ok(())
    }
}

fn boundary(event_type: &'static str, detail: &'static str) -> AttemptError {
    AttemptError::Boundary { event_type, detail }
}

fn terminal_failure(reason: &FinishReason) -> Option<&LlmFailure> {
    match reason.kind() {
        FinishReasonKind::Error { failure } | FinishReasonKind::Aborted { failure } => {
            Some(failure)
        }
        _ => None,
    }
}

fn validate_message_route(
    message: &Message,
    route: &AttemptRoute,
    replay_digest: Option<[u8; 32]>,
) -> Result<(), AttemptError> {
    match message.source().kind() {
        MessageSourceKind::Model {
            provider,
            model,
            replay_state: actual_replay,
        } if provider == &route.provider
            && model == &route.model
            && actual_replay.as_ref().map(json_digest).transpose()? == replay_digest =>
        {
            Ok(())
        }
        _ => Err(AttemptError::RouteMismatch),
    }
}

fn json_digest(value: &JsonValue) -> Result<[u8; 32], AttemptError> {
    let mut context = Context::new(&SHA256);
    context.update(REPLAY_DIGEST_DOMAIN);
    let mut writer = DigestWriter(context);
    serde_json::to_writer(&mut writer, value).map_err(|_| AttemptError::Digest)?;
    Ok(writer.finish())
}

fn content_digest<'a>(
    content: impl IntoIterator<Item = &'a ContentBlock>,
) -> Result<[u8; 32], AttemptError> {
    let mut context = Context::new(&SHA256);
    context.update(CONTENT_DIGEST_DOMAIN);
    let mut writer = DigestWriter(context);
    let mut serializer = serde_json::Serializer::new(&mut writer);
    serializer
        .collect_seq(content)
        .map_err(|_| AttemptError::Digest)?;
    Ok(writer.finish())
}

fn source_digest(sources: &[EventSeq]) -> [u8; 32] {
    let mut context = Context::new(&SHA256);
    context.update(SOURCE_DIGEST_DOMAIN);
    for source in sources {
        context.update(&source.get().to_be_bytes());
    }
    let digest = context.finish();
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(digest.as_ref());
    bytes
}

struct DigestWriter(Context);

impl DigestWriter {
    fn finish(self) -> [u8; 32] {
        let digest = self.0.finish();
        let mut bytes = [0_u8; 32];
        bytes.copy_from_slice(digest.as_ref());
        bytes
    }
}

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::model::{
        ContentBlockType, LlmCallConfig, MessageRole, MessageSource, StreamProtocolError,
    };

    use super::*;
    use crate::session::{NewEvent, SurfaceIntent, UnixMillis};

    fn turn() -> TurnId {
        TurnId::new(1).unwrap()
    }

    fn step() -> StepId {
        StepId::new(1).unwrap()
    }

    fn header() -> EpochHeader {
        EpochHeader {
            config: LlmCallConfig::new("mock", "mock-model").unwrap(),
            adapter_defaults: None,
            system: None,
            tools: None,
        }
    }

    fn event(seq: u64, new: NewEvent) -> SessionEvent {
        SessionEvent::from_new(
            EventSeq::new(seq).unwrap(),
            UnixMillis::new(1).unwrap(),
            new,
            JsonValue::new(json!({})).unwrap(),
        )
    }

    fn chunk_event(seq: u64, chunk: StreamChunk) -> SessionEvent {
        event(
            seq,
            NewEvent::log(EventKind::assistant_chunk(turn(), step(), chunk)),
        )
    }

    fn commit_chunk(
        projection: &mut AttemptProjection,
        seq: u64,
        chunk: StreamChunk,
    ) -> Result<(), AttemptError> {
        let event = chunk_event(seq, chunk);
        let prepared = projection.prepare_chunk(&event, Some(&header()), 0)?;
        assert!(projection.commit_chunk(prepared));
        Ok(())
    }

    fn streaming_atomicity_facts(
        projection: &AttemptProjection,
    ) -> (RecoveryAttemptProof, usize, usize, usize, usize) {
        let AttemptState::Streaming(open) = &projection.state else {
            panic!("test requires one streaming attempt");
        };
        (
            projection.recovery_proof().unwrap(),
            open.validator.chunk_count(),
            open.emitted_bytes,
            open.order.len(),
            open.blocks.len(),
        )
    }

    fn assistant_event(
        seq: u64,
        content: Vec<ContentBlock>,
        usage: Option<TokenUsage>,
        sources: Vec<EventSeq>,
    ) -> SessionEvent {
        assistant_event_with_replay(seq, content, usage, sources, None)
    }

    fn assistant_event_with_replay(
        seq: u64,
        content: Vec<ContentBlock>,
        usage: Option<TokenUsage>,
        sources: Vec<EventSeq>,
        replay_state: Option<JsonValue>,
    ) -> SessionEvent {
        let message = Message::new(
            "assistant-1",
            MessageRole::Assistant,
            content,
            MessageSource::model_with_replay_state("mock", "mock-model", replay_state).unwrap(),
        )
        .unwrap();
        event(
            seq,
            NewEvent::surface(
                EventKind::AssistantMessage {
                    turn: turn(),
                    step: step(),
                    message,
                    usage,
                },
                SurfaceIntent::append().with_sources(sources),
            ),
        )
    }

    #[test]
    fn finish_only_attempt_seals_and_commits_exact_sources() {
        let mut projection = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        commit_chunk(
            &mut projection,
            4,
            StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
        )
        .unwrap();

        let prepared = projection.take_prepared().unwrap();
        let (content, usage, finish, replay, sources) = prepared.into_parts();
        assert!(content.is_empty());
        assert_eq!(usage, None);
        assert!(matches!(finish.kind(), FinishReasonKind::Stop));
        assert_eq!(replay, None);
        assert_eq!(sources, vec![EventSeq::new(4).unwrap()]);

        let (committed, facts) = projection
            .assistant(&assistant_event(5, vec![], None, sources))
            .unwrap();
        assert_eq!(facts.provider_assistant_tokens(), 0);
        assert_eq!(
            committed.step_end(turn(), step(), None).unwrap().state,
            AttemptState::OutsideStep
        );
    }

    #[test]
    fn sealed_replay_digest_accepts_semantic_same_and_rejects_change() {
        let replay = JsonValue::new(json!({"number": 1.0, "nested": [true, null]})).unwrap();
        let mut projection = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        commit_chunk(
            &mut projection,
            4,
            StreamChunk::finish(FinishReason::stop().unwrap(), Some(replay.clone())).unwrap(),
        )
        .unwrap();

        let prepared = projection.take_prepared().unwrap();
        let (_, _, _, prepared_replay, sources) = prepared.into_parts();
        assert_eq!(prepared_replay, Some(replay));

        let changed = assistant_event_with_replay(
            5,
            vec![],
            None,
            sources.clone(),
            Some(JsonValue::new(json!({"number": 2, "nested": [true, null]})).unwrap()),
        );
        assert_eq!(
            projection.assistant(&changed),
            Err(AttemptError::RouteMismatch)
        );

        let equivalent = assistant_event_with_replay(
            5,
            vec![],
            None,
            sources,
            Some(JsonValue::new(json!({"number": 1, "nested": [true, null]})).unwrap()),
        );
        projection.assistant(&equivalent).unwrap();
    }

    #[test]
    fn assistant_requires_the_complete_ordered_source_span_and_exact_usage() {
        let usage = TokenUsage::new(12, 3).unwrap();
        let mut projection = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        let chunks = [
            StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
            StreamChunk::text_delta(0, "hello").unwrap(),
            StreamChunk::block_end(0, ContentBlock::text("hello").unwrap()).unwrap(),
            StreamChunk::usage(usage.clone()).unwrap(),
            StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
        ];
        for (seq, chunk) in (10..).zip(chunks) {
            commit_chunk(&mut projection, seq, chunk).unwrap();
        }
        projection.take_prepared().unwrap();
        let sources = (10..15)
            .map(|seq| EventSeq::new(seq).unwrap())
            .collect::<Vec<_>>();

        let missing = assistant_event(
            15,
            vec![ContentBlock::text("hello").unwrap()],
            Some(usage.clone()),
            sources[1..].to_vec(),
        );
        assert_eq!(
            projection.assistant(&missing),
            Err(AttemptError::SourceMismatch)
        );

        let wrong_usage = assistant_event(
            15,
            vec![ContentBlock::text("hello").unwrap()],
            Some(TokenUsage::new(12, 4).unwrap()),
            sources.clone(),
        );
        assert_eq!(
            projection.assistant(&wrong_usage),
            Err(AttemptError::UsageMismatch)
        );

        projection
            .assistant(&assistant_event(
                15,
                vec![ContentBlock::text("hello").unwrap()],
                Some(usage),
                sources,
            ))
            .unwrap();
    }

    #[test]
    fn max_tokens_may_drop_raw_tool_calls_but_cannot_forge_new_ones() {
        let raw_call = ContentBlock::tool_call("call-old", "read", "{}").unwrap();
        let forged_call = ContentBlock::tool_call("call-forged", "bash", "{}").unwrap();
        let text = ContentBlock::text("partial").unwrap();
        let mut projection = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        for (seq, chunk) in (20..).zip([
            StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
            StreamChunk::block_end(0, text.clone()).unwrap(),
            StreamChunk::block_start(1, ContentBlockType::ToolCall).unwrap(),
            StreamChunk::block_end(1, raw_call).unwrap(),
            StreamChunk::finish(FinishReason::max_tokens().unwrap(), None).unwrap(),
        ]) {
            commit_chunk(&mut projection, seq, chunk).unwrap();
        }
        let prepared = projection.take_prepared().unwrap();
        let (_, _, _, _, sources) = prepared.into_parts();

        let forged = assistant_event(25, vec![text.clone(), forged_call], None, sources.clone());
        assert_eq!(
            projection.assistant(&forged),
            Err(AttemptError::ContentMismatch)
        );
        let (_, facts) = projection
            .assistant(&assistant_event(25, vec![text], None, sources))
            .unwrap();
        assert_eq!(facts.provider_assistant_tokens(), 10);
    }

    #[test]
    fn usage_projection_replaces_a_retry_sample_inside_one_step() {
        let usage = UsageProjection::default()
            .with_sample(turn(), step(), &TokenUsage::new(100, 10).unwrap())
            .unwrap()
            .with_sample(turn(), step(), &TokenUsage::new(80, 4).unwrap())
            .unwrap();
        assert_eq!(usage.totals.uncached_input_tokens, 80);
        assert_eq!(usage.totals.output_tokens, 4);

        let next_step = StepId::new(2).unwrap();
        let usage = usage
            .with_sample(turn(), next_step, &TokenUsage::new(20, 2).unwrap())
            .unwrap();
        assert_eq!(usage.totals.uncached_input_tokens, 100);
        assert_eq!(usage.totals.output_tokens, 6);
    }

    #[test]
    fn failed_attempt_seals_and_only_its_exact_failure_can_schedule_retry() {
        let failure = LlmFailure::new("context is full", "CONTEXT_WINDOW_EXCEEDED").unwrap();
        let usage = TokenUsage::new(100, 7).unwrap();
        let mut projection = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        for (seq, chunk) in (30..).zip([
            StreamChunk::usage(usage.clone()).unwrap(),
            StreamChunk::finish(FinishReason::error(failure.clone()).unwrap(), None).unwrap(),
        ]) {
            commit_chunk(&mut projection, seq, chunk).unwrap();
        }

        let prepared = projection.take_prepared().unwrap();
        let (content, actual_usage, finish, replay, sources) = prepared.into_parts();
        assert!(content.is_empty());
        assert_eq!(actual_usage, Some(usage));
        assert!(matches!(finish.kind(), FinishReasonKind::Error { .. }));
        assert_eq!(replay, None);
        assert_eq!(
            sources,
            vec![EventSeq::new(30).unwrap(), EventSeq::new(31).unwrap()]
        );

        let wrong = LlmFailure::new("different", "OTHER").unwrap();
        let wrong_retry = event(
            32,
            NewEvent::log(EventKind::llm_retry(
                super::super::LlmRetryEvent::normal(
                    super::super::RetryId::new("retry-1"),
                    turn(),
                    step(),
                    "mock",
                    "policy",
                    super::super::RetryNumber::new(1).unwrap(),
                    super::super::RetryNumber::new(2).unwrap(),
                    crate::model::FiniteNumber::new(0.0).unwrap(),
                    wrong,
                )
                .unwrap(),
            )),
        );
        assert_eq!(
            projection.retry(&wrong_retry),
            Err(AttemptError::FailureMismatch)
        );

        let retry = event(
            32,
            NewEvent::log(EventKind::llm_retry(
                super::super::LlmRetryEvent::normal(
                    super::super::RetryId::new("retry-1"),
                    turn(),
                    step(),
                    "mock",
                    "policy",
                    super::super::RetryNumber::new(1).unwrap(),
                    super::super::RetryNumber::new(2).unwrap(),
                    crate::model::FiniteNumber::new(0.0).unwrap(),
                    failure,
                )
                .unwrap(),
            )),
        );
        let scheduled = projection.retry(&retry).unwrap();
        assert!(matches!(
            scheduled.state,
            AttemptState::RetryScheduled { .. }
        ));
    }

    #[test]
    fn context_overflow_consumes_only_one_exact_error_attempt() {
        fn terminal(reason: FinishReason) -> AttemptProjection {
            let mut projection = AttemptProjection::default()
                .step_start(turn(), step())
                .unwrap()
                .begin_live(turn(), step(), Some(&header()), 0)
                .unwrap();
            commit_chunk(
                &mut projection,
                30,
                StreamChunk::finish(reason, None).unwrap(),
            )
            .unwrap();
            projection.take_prepared().unwrap();
            projection
        }

        let exact_failure = LlmFailure::new("context is full", "CONTEXT_WINDOW_EXCEEDED").unwrap();
        let exact = terminal(FinishReason::error(exact_failure.clone()).unwrap());
        let recovered = exact.context_overflow(turn(), step(), 7, false).unwrap();
        assert!(recovered.context_overflow_was_used());
        assert!(matches!(
            recovered.begin_live(turn(), step(), Some(&header()), 7),
            Err(AttemptError::Boundary { .. })
        ));
        assert!(
            recovered
                .begin_live(turn(), step(), Some(&header()), 8)
                .is_ok()
        );
        let started = recovered.context_overflow_start(turn(), step()).unwrap();
        assert!(matches!(
            started.context_overflow_start(turn(), step()),
            Err(AttemptError::Boundary { .. })
        ));
        assert!(matches!(
            recovered.context_overflow(turn(), step(), 8, false),
            Err(AttemptError::Boundary { .. })
        ));

        let other = terminal(
            FinishReason::error(LlmFailure::new("failed", "MODEL_FAILURE").unwrap()).unwrap(),
        );
        assert_eq!(
            other.context_overflow(turn(), step(), 7, false),
            Err(AttemptError::FailureMismatch)
        );

        let aborted = terminal(FinishReason::aborted(exact_failure).unwrap());
        assert_eq!(
            aborted.context_overflow(turn(), step(), 7, false),
            Err(AttemptError::FailureMismatch)
        );
    }

    #[test]
    fn step_end_requires_exactly_one_open_attempt_disposition() {
        let ready = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap();
        assert!(ready.step_end(turn(), step(), None).is_ok());
        assert!(matches!(
            ready.step_end(turn(), step(), Some(AttemptDisposition::Failed)),
            Err(AttemptError::Boundary { .. })
        ));

        let streaming = ready
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        assert!(matches!(
            streaming.step_end(turn(), step(), None),
            Err(AttemptError::Boundary { .. })
        ));
        for disposition in [
            AttemptDisposition::Failed,
            AttemptDisposition::Cancelled,
            AttemptDisposition::Interrupted,
        ] {
            assert!(
                streaming
                    .step_end(turn(), step(), Some(disposition))
                    .is_ok()
            );
        }
        for disposition in [
            AttemptDisposition::Committed,
            AttemptDisposition::Retry,
            AttemptDisposition::ContextOverflow,
        ] {
            assert!(matches!(
                streaming.step_end(turn(), step(), Some(disposition)),
                Err(AttemptError::Boundary { .. })
            ));
        }
    }

    #[test]
    fn four_thousandth_chunk_must_be_terminal_and_state_is_atomic_on_rejection() {
        let mut projection = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        commit_chunk(
            &mut projection,
            1,
            StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        )
        .unwrap();
        for seq in 2..4_000 {
            commit_chunk(
                &mut projection,
                seq,
                StreamChunk::text_delta(0, "x").unwrap(),
            )
            .unwrap();
        }
        let before = streaming_atomicity_facts(&projection);
        let error = commit_chunk(
            &mut projection,
            4_000,
            StreamChunk::text_delta(0, "x").unwrap(),
        )
        .unwrap_err();
        assert_eq!(
            error,
            AttemptError::Stream(StreamProtocolError::TooManyChunks {
                maximum: MAX_PROVIDER_STREAM_CHUNKS,
            })
        );
        assert_eq!(streaming_atomicity_facts(&projection), before);

        let failure = LlmFailure::new("failed", "MODEL_FAILURE").unwrap();
        commit_chunk(
            &mut projection,
            4_000,
            StreamChunk::finish(FinishReason::error(failure).unwrap(), None).unwrap(),
        )
        .unwrap();
        assert!(projection.has_open_attempt());
        projection
            .step_end(turn(), step(), Some(AttemptDisposition::Failed))
            .unwrap();
    }

    #[test]
    fn emitted_chunk_bytes_accept_exactly_ten_mib_and_reject_one_more_atomically() {
        let start = StreamChunk::block_start(0, ContentBlockType::Text).unwrap();
        let empty_delta = StreamChunk::text_delta(0, "").unwrap();
        let terminal = || {
            StreamChunk::finish(
                FinishReason::error(LlmFailure::new("failed", "MODEL_FAILURE").unwrap()).unwrap(),
                None,
            )
            .unwrap()
        };
        let fixed = start
            .raw()
            .encoded_len()
            .checked_add(empty_delta.raw().encoded_len() * 2)
            .and_then(|value| value.checked_add(terminal().raw().encoded_len()))
            .unwrap();
        let payload = MAX_ATTEMPT_EMITTED_BYTES.checked_sub(fixed).unwrap();
        let first = payload / 2;
        let second = payload - first;

        let mut exact = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        commit_chunk(&mut exact, 1, start).unwrap();
        commit_chunk(
            &mut exact,
            2,
            StreamChunk::text_delta(0, "x".repeat(first)).unwrap(),
        )
        .unwrap();
        commit_chunk(
            &mut exact,
            3,
            StreamChunk::text_delta(0, "x".repeat(second)).unwrap(),
        )
        .unwrap();
        commit_chunk(&mut exact, 4, terminal()).unwrap();

        let mut one_over = AttemptProjection::default()
            .step_start(turn(), step())
            .unwrap()
            .begin_live(turn(), step(), Some(&header()), 0)
            .unwrap();
        commit_chunk(
            &mut one_over,
            1,
            StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        )
        .unwrap();
        commit_chunk(
            &mut one_over,
            2,
            StreamChunk::text_delta(0, "x".repeat(first)).unwrap(),
        )
        .unwrap();
        commit_chunk(
            &mut one_over,
            3,
            StreamChunk::text_delta(0, "x".repeat(second + 1)).unwrap(),
        )
        .unwrap();
        let before = streaming_atomicity_facts(&one_over);
        assert_eq!(
            commit_chunk(&mut one_over, 4, terminal()),
            Err(AttemptError::EmittedBytes {
                maximum: MAX_ATTEMPT_EMITTED_BYTES,
                actual: MAX_ATTEMPT_EMITTED_BYTES + 1,
            })
        );
        assert_eq!(streaming_atomicity_facts(&one_over), before);
    }
}
