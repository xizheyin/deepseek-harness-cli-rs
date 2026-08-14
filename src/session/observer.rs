//! Bounded projection of facts that have already committed to one Session.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use thiserror::Error;
use tokio::sync::mpsc;

use crate::model::{ContentBlockKind, MessageSourceKind, StreamChunkKind};

use super::{
    ApprovalOutcome, ApprovalRequestId, EventKind, EventSeq, RetryNumber, SessionEvent, StepId,
    ToolFailure, TurnEndReason, TurnId, UnixMillis,
};

const SOURCE_BITMAP_WORDS: usize = 64;
const MAX_SOURCE_BITMAP_CAPACITY: usize = 128;
const MAX_INDEXED_ASSISTANT_BLOCKS: usize = 128;
const MAX_INDEXED_ASSISTANT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum UiObserverAttachError {
    #[error("the live Session observer may only attach before the first event")]
    NotFresh,
    #[error("the live Session observer was already attached")]
    AlreadyAttached,
}

#[derive(Debug)]
pub(crate) struct CommittedUiEvent {
    pub(crate) seq: EventSeq,
    pub(crate) time: UnixMillis,
    pub(crate) kind: CommittedUiKind,
}

#[derive(Debug)]
pub(crate) enum CommittedUiKind {
    TurnStart {
        turn: TurnId,
    },
    TurnEnd {
        turn: TurnId,
        reason: UiTurnEndReason,
    },
    StepStart {
        turn: TurnId,
        step: StepId,
    },
    StepEnd {
        turn: TurnId,
        step: StepId,
    },
    AssistantTextDelta {
        turn: TurnId,
        step: StepId,
        index: u64,
        text: String,
    },
    AssistantReasoningDelta {
        turn: TurnId,
        step: StepId,
        index: u64,
        text: String,
    },
    AssistantMessage {
        turn: TurnId,
        step: StepId,
        content: UiAssistantContent,
        sources: SourceSeqBitmap,
    },
    ToolRequested {
        turn: TurnId,
        step: StepId,
        call_id: String,
        name: String,
        arguments_preview: String,
        arguments_truncated: bool,
    },
    ToolResult {
        turn: TurnId,
        step: StepId,
        call_id: String,
        is_error: bool,
        failure: Option<UiToolFailure>,
    },
    ApprovalAsked {
        id: String,
        tool_name: String,
        call_id: Option<String>,
        reason: Option<String>,
    },
    ApprovalDecided {
        id: String,
        outcome: ApprovalOutcome,
    },
    RetryScheduled {
        retry_id: String,
        retry: RetryNumber,
    },
    RetryStarted {
        retry_id: String,
        retry: RetryNumber,
    },
    TypeOnly {
        event_type: &'static str,
    },
}

#[derive(Debug)]
pub(crate) enum UiTurnEndReason {
    Completed,
    Aborted,
    Blocked,
    Error { code: String, message: String },
    MaxTokens,
    Interrupted,
    Other { kind: Option<String> },
}

#[derive(Debug)]
pub(crate) struct UiAssistantBlock {
    pub(crate) index: u16,
    pub(crate) kind: UiAssistantBlockKind,
    pub(crate) text: String,
}

#[derive(Debug)]
pub(crate) enum UiAssistantContent {
    Indexed(Vec<UiAssistantBlock>),
    /// Complete final answer text when block-by-block deduplication is too large.
    Degraded {
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiAssistantBlockKind {
    Text,
    Reasoning,
}

#[derive(Debug)]
pub(crate) struct UiToolFailure {
    pub(crate) name: String,
    pub(crate) code: String,
}

#[derive(Debug)]
pub(crate) struct SourceSeqBitmap {
    words: Vec<u64>,
}

impl SourceSeqBitmap {
    pub(crate) fn from_sources(sources: &[EventSeq]) -> Result<Self, UiProjectionError> {
        let mut words = Vec::new();
        words
            .try_reserve_exact(SOURCE_BITMAP_WORDS)
            .map_err(|_| UiProjectionError)?;
        words.resize(SOURCE_BITMAP_WORDS, 0);
        let mut bitmap = Self::finish_words(words)?;
        for source in sources {
            let index = usize::try_from(source.get()).map_err(|_| UiProjectionError)?;
            if index >= super::MAX_SESSION_EVENTS {
                return Err(UiProjectionError);
            }
            bitmap.words[index / u64::BITS as usize] |= 1_u64 << (index % u64::BITS as usize);
        }
        Ok(bitmap)
    }

    fn finish_words(words: Vec<u64>) -> Result<Self, UiProjectionError> {
        if words.len() != SOURCE_BITMAP_WORDS || !Self::capacity_is_acceptable(words.capacity()) {
            return Err(UiProjectionError);
        }
        Ok(Self { words })
    }

    fn capacity_is_acceptable(capacity: usize) -> bool {
        (SOURCE_BITMAP_WORDS..=MAX_SOURCE_BITMAP_CAPACITY).contains(&capacity)
    }

    pub(crate) fn contains(&self, source: EventSeq) -> bool {
        let Ok(index) = usize::try_from(source.get()) else {
            return false;
        };
        self.words
            .get(index / u64::BITS as usize)
            .is_some_and(|word| word & (1_u64 << (index % u64::BITS as usize)) != 0)
    }

    #[cfg(test)]
    pub(crate) fn word_len_for_test(&self) -> usize {
        self.words.len()
    }

    #[cfg(test)]
    pub(crate) fn word_capacity_for_test(&self) -> usize {
        self.words.capacity()
    }

    #[cfg(test)]
    pub(crate) fn allocated_bytes_for_test(&self) -> usize {
        self.words.capacity() * size_of::<u64>()
    }

    #[cfg(test)]
    pub(crate) fn capacity_is_acceptable_for_test(capacity: usize) -> bool {
        Self::capacity_is_acceptable(capacity)
    }

    #[cfg(test)]
    pub(crate) fn from_words_for_test(words: Vec<u64>) -> Result<Self, UiProjectionError> {
        Self::finish_words(words)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UiProjectionError;

#[derive(Debug)]
struct ObserverState {
    faulted: Arc<AtomicBool>,
    #[cfg(test)]
    fail_next_projection: AtomicBool,
}

impl ObserverState {
    fn new() -> Self {
        Self {
            faulted: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            fail_next_projection: AtomicBool::new(false),
        }
    }

    fn fault(&self) {
        self.faulted.store(true, Ordering::SeqCst);
    }

    fn should_fail_projection(&self) -> bool {
        #[cfg(test)]
        {
            self.fail_next_projection.swap(false, Ordering::SeqCst)
        }
        #[cfg(not(test))]
        {
            false
        }
    }
}

pub(super) struct CommittedUiSender {
    sender: mpsc::Sender<CommittedUiEvent>,
    state: Arc<ObserverState>,
}

pub(crate) struct CommittedUiReceiver {
    receiver: mpsc::Receiver<CommittedUiEvent>,
    state: Arc<ObserverState>,
}

impl CommittedUiReceiver {
    pub(crate) async fn recv(&mut self) -> Option<CommittedUiEvent> {
        self.receiver.recv().await
    }

    pub(crate) fn try_recv(&mut self) -> Result<CommittedUiEvent, mpsc::error::TryRecvError> {
        self.receiver.try_recv()
    }

    pub(crate) fn is_producer_faulted(&self) -> bool {
        self.state.faulted.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    pub(crate) fn fail_next_projection_for_test(&self) {
        self.state
            .fail_next_projection
            .store(true, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub(crate) fn fault_handle_for_test(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.state.faulted)
    }
}

pub(super) fn channel(capacity: usize) -> (CommittedUiSender, CommittedUiReceiver) {
    let state = Arc::new(ObserverState::new());
    let (sender, receiver) = mpsc::channel(capacity);
    (
        CommittedUiSender {
            sender,
            state: Arc::clone(&state),
        },
        CommittedUiReceiver { receiver, state },
    )
}

pub(super) fn publish_committed(observer: &mut Option<CommittedUiSender>, event: &SessionEvent) {
    let Some(active) = observer.as_ref() else {
        return;
    };
    if active.state.should_fail_projection() {
        active.state.fault();
        *observer = None;
        return;
    }
    let projection = match CommittedUiEvent::from_event(event) {
        Ok(projection) => projection,
        Err(_) => {
            active.state.fault();
            *observer = None;
            return;
        }
    };
    match active.sender.try_send(projection) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            active.state.fault();
            *observer = None;
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            *observer = None;
        }
    }
}

impl CommittedUiEvent {
    fn from_event(event: &SessionEvent) -> Result<Self, UiProjectionError> {
        let kind = match event.kind() {
            EventKind::TurnStart { turn } => CommittedUiKind::TurnStart { turn: *turn },
            EventKind::TurnEnd { turn, reason } => CommittedUiKind::TurnEnd {
                turn: *turn,
                reason: project_turn_end_reason(reason)?,
            },
            EventKind::StepStart { turn, step } => CommittedUiKind::StepStart {
                turn: *turn,
                step: *step,
            },
            EventKind::StepEnd { turn, step } => CommittedUiKind::StepEnd {
                turn: *turn,
                step: *step,
            },
            EventKind::AssistantChunk { turn, step, chunk } => match chunk.kind() {
                StreamChunkKind::TextDelta { index, text } => CommittedUiKind::AssistantTextDelta {
                    turn: *turn,
                    step: *step,
                    index: index.get(),
                    text: try_copy(text)?,
                },
                StreamChunkKind::ReasoningDelta { index, text } => {
                    CommittedUiKind::AssistantReasoningDelta {
                        turn: *turn,
                        step: *step,
                        index: index.get(),
                        text: try_copy(text)?,
                    }
                }
                _ => CommittedUiKind::TypeOnly {
                    event_type: "assistant/chunk",
                },
            },
            EventKind::AssistantMessage {
                turn,
                step,
                message,
                ..
            } => {
                let visible_blocks = message
                    .content()
                    .iter()
                    .filter(|block| {
                        matches!(
                            block.kind(),
                            ContentBlockKind::Text { .. } | ContentBlockKind::Reasoning { .. }
                        )
                    })
                    .count();
                let visible_bytes =
                    message.content().iter().try_fold(0_usize, |total, block| {
                        match block.kind() {
                            ContentBlockKind::Text { text }
                            | ContentBlockKind::Reasoning { text } => {
                                total.checked_add(text.len()).ok_or(UiProjectionError)
                            }
                            _ => Ok(total),
                        }
                    })?;
                let content = if visible_blocks <= MAX_INDEXED_ASSISTANT_BLOCKS
                    && visible_bytes <= MAX_INDEXED_ASSISTANT_BYTES
                {
                    let mut blocks = Vec::new();
                    blocks
                        .try_reserve_exact(visible_blocks)
                        .map_err(|_| UiProjectionError)?;
                    for (index, block) in message.content().iter().enumerate() {
                        let (kind, text) = match block.kind() {
                            ContentBlockKind::Text { text } => (UiAssistantBlockKind::Text, text),
                            ContentBlockKind::Reasoning { text } => {
                                (UiAssistantBlockKind::Reasoning, text)
                            }
                            _ => continue,
                        };
                        blocks.push(UiAssistantBlock {
                            index: u16::try_from(index).map_err(|_| UiProjectionError)?,
                            kind,
                            text: try_copy(text)?,
                        });
                    }
                    UiAssistantContent::Indexed(blocks)
                } else {
                    UiAssistantContent::Degraded {
                        text: concat_final_text(message.content())?,
                    }
                };
                CommittedUiKind::AssistantMessage {
                    turn: *turn,
                    step: *step,
                    content,
                    sources: SourceSeqBitmap::from_sources(
                        event.source_event_seqs().unwrap_or_default(),
                    )?,
                }
            }
            EventKind::ToolCall {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => {
                CommittedUiKind::ToolRequested {
                    turn: *turn,
                    step: *step,
                    call_id: try_copy(call_id.as_str())?,
                    name: try_copy(name)?,
                    // Tool arguments can contain patch/file bodies. The live
                    // status channel therefore reports omission, not a prefix.
                    arguments_preview: try_copy("arguments omitted")?,
                    arguments_truncated: !arguments.is_empty(),
                }
            }
            EventKind::ToolResult {
                turn,
                step,
                message,
                error,
                ..
            } => {
                let Some(block) = message.content().first() else {
                    return Err(UiProjectionError);
                };
                let ContentBlockKind::ToolResult {
                    tool_call_id,
                    is_error,
                } = block.kind()
                else {
                    return Err(UiProjectionError);
                };
                let MessageSourceKind::Tool { call_id } = message.source().kind() else {
                    return Err(UiProjectionError);
                };
                if call_id != tool_call_id {
                    return Err(UiProjectionError);
                }
                CommittedUiKind::ToolResult {
                    turn: *turn,
                    step: *step,
                    call_id: try_copy(call_id.as_str())?,
                    is_error: (*is_error).unwrap_or(error.is_some()),
                    failure: error.as_ref().map(project_tool_failure).transpose()?,
                }
            }
            EventKind::ApprovalAsked { asked } => CommittedUiKind::ApprovalAsked {
                id: approval_id(asked.id())?,
                tool_name: try_copy(asked.tool_name())?,
                call_id: asked
                    .call_id()
                    .map(|call_id| try_copy(call_id.as_str()))
                    .transpose()?,
                reason: asked.reason().map(try_copy).transpose()?,
            },
            EventKind::ApprovalDecided { decided } => CommittedUiKind::ApprovalDecided {
                id: approval_id(decided.id())?,
                outcome: decided.outcome(),
            },
            EventKind::LlmRetry { retry } => CommittedUiKind::RetryScheduled {
                retry_id: try_copy(retry.retry_id().as_str())?,
                retry: retry.retry(),
            },
            EventKind::LlmRetryStarted { started } => CommittedUiKind::RetryStarted {
                retry_id: try_copy(started.retry_id().as_str())?,
                retry: started.retry(),
            },
            EventKind::UserMessage { .. } => CommittedUiKind::TypeOnly {
                event_type: "user/message",
            },
            EventKind::TodoWrite { .. } => CommittedUiKind::TypeOnly {
                event_type: "todo/write",
            },
            EventKind::RequestHeader { .. } => CommittedUiKind::TypeOnly {
                event_type: "request/header",
            },
            EventKind::RequestContext { .. } => CommittedUiKind::TypeOnly {
                event_type: "request/context",
            },
            EventKind::EndSeed => CommittedUiKind::TypeOnly {
                event_type: "session/end-seed",
            },
            EventKind::Unknown { .. } => return Err(UiProjectionError),
        };
        Ok(Self {
            seq: event.seq(),
            time: event.time(),
            kind,
        })
    }
}

fn project_turn_end_reason(reason: &TurnEndReason) -> Result<UiTurnEndReason, UiProjectionError> {
    Ok(match reason {
        TurnEndReason::Completed => UiTurnEndReason::Completed,
        TurnEndReason::Aborted { .. } => UiTurnEndReason::Aborted,
        TurnEndReason::Blocked => UiTurnEndReason::Blocked,
        TurnEndReason::Error { error } => UiTurnEndReason::Error {
            code: try_copy(error.code())?,
            message: try_copy(error.message())?,
        },
        TurnEndReason::MaxTokens => UiTurnEndReason::MaxTokens,
        TurnEndReason::Interrupted => UiTurnEndReason::Interrupted,
        TurnEndReason::Other { kind, .. } => UiTurnEndReason::Other {
            kind: kind.as_deref().map(try_copy).transpose()?,
        },
    })
}

fn project_tool_failure(failure: &ToolFailure) -> Result<UiToolFailure, UiProjectionError> {
    Ok(UiToolFailure {
        name: try_copy(&failure.name)?,
        code: try_copy(&failure.code)?,
    })
}

fn approval_id(id: &ApprovalRequestId) -> Result<String, UiProjectionError> {
    try_copy(id.as_str())
}

fn try_copy(value: &str) -> Result<String, UiProjectionError> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| UiProjectionError)?;
    copy.push_str(value);
    Ok(copy)
}

fn concat_final_text(blocks: &[crate::model::ContentBlock]) -> Result<String, UiProjectionError> {
    let total = blocks.iter().try_fold(0_usize, |total, block| {
        if let ContentBlockKind::Text { text } = block.kind() {
            total.checked_add(text.len()).ok_or(UiProjectionError)
        } else {
            Ok(total)
        }
    })?;
    let mut text = String::new();
    text.try_reserve_exact(total)
        .map_err(|_| UiProjectionError)?;
    for block in blocks {
        if let ContentBlockKind::Text { text: block_text } = block.kind() {
            text.push_str(block_text);
        }
    }
    Ok(text)
}
