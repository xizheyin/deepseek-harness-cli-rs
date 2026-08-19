//! Bounded projection of facts that have already committed to one Session.

use std::{
    fmt::{self, Write as _},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use aws_lc_rs::digest::{SHA256, digest};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::model::{ContentBlockKind, ContextForm, MessageSourceKind, StreamChunkKind, TokenUsage};

use super::{
    ApprovalOutcome, ApprovalRequestId, CompactionEndError, CompactionTrigger, EventKind, EventSeq,
    RetryNumber, SessionEvent, StepId, SurfaceOp, ToolFailure, TurnEndCancelCause, TurnEndReason,
    TurnId, UnixMillis,
};

const SOURCE_BITMAP_WORDS: usize = 64;
const MAX_SOURCE_BITMAP_CAPACITY: usize = 128;
const MAX_INDEXED_ASSISTANT_BLOCKS: usize = 128;
const MAX_INDEXED_ASSISTANT_BYTES: usize = 4 * 1024 * 1024;
const MAX_UI_OPAQUE_PAYLOAD_BYTES: usize = 64 * 1024;
const MAX_UI_IDENTITY_BYTES: usize = 4 * 1024;

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum UiObserverAttachError {
    #[error("the live Session observer may only attach before the first event")]
    NotFresh,
    #[error("the live Session observer was already attached")]
    AlreadyAttached,
}

pub(crate) struct CommittedUiEvent {
    pub(crate) seq: EventSeq,
    pub(crate) time: UnixMillis,
    pub(crate) kind: CommittedUiKind,
}

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
    UserMessage {
        source: UiUserSource,
        content: UiOpaquePayload,
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
    UsageSample {
        turn: TurnId,
        step: StepId,
        usage: UiTokenUsage,
    },
    AssistantMessage {
        turn: TurnId,
        step: StepId,
        content: UiAssistantContent,
        sources: SourceSeqBitmap,
        provider: UiIdentity,
        model: UiIdentity,
        usage: Option<UiTokenUsage>,
    },
    ToolRequested {
        turn: TurnId,
        step: StepId,
        call_id: UiIdentity,
        name: UiIdentity,
        arguments: UiOpaquePayload,
    },
    ToolResult {
        turn: TurnId,
        step: StepId,
        call_id: UiIdentity,
        is_error: bool,
        failure: Option<UiToolFailure>,
        content: UiOpaquePayload,
        meta: UiOpaquePayload,
        surface_replacement_target: Option<EventSeq>,
    },
    RequestContextChanged {
        provider: Option<UiIdentity>,
        model: Option<UiIdentity>,
        context_window: Option<u64>,
    },
    CompactionStarted {
        id: UiIdentity,
        turn: Option<TurnId>,
        trigger: Option<CompactionTrigger>,
        shadowed_nodes: Option<usize>,
    },
    CompactionSummarized {
        id: UiIdentity,
        shadowed_tokens: u64,
        provider: UiIdentity,
        model: UiIdentity,
        usage: Option<UiTokenUsage>,
    },
    CompactionEnded {
        id: UiIdentity,
        turn: Option<TurnId>,
        error: Option<UiCompactionError>,
    },
    CompactionPruneMarked {
        target: EventSeq,
        shadowed_tokens: u64,
    },
    ApprovalAsked {
        id: UiIdentity,
        tool_name: UiIdentity,
        call_id: Option<UiIdentity>,
        reason: Option<String>,
    },
    ApprovalDecided {
        id: UiIdentity,
        outcome: ApprovalOutcome,
    },
    RetryScheduled {
        retry_id: UiIdentity,
        retry: RetryNumber,
        provider: UiIdentity,
        delay_ms: f64,
        max_retries: Option<RetryNumber>,
        failure_code: String,
        failure_message: String,
    },
    RetryStarted {
        retry_id: UiIdentity,
        retry: RetryNumber,
    },
    TypeOnly {
        event_type: &'static str,
    },
}

impl fmt::Debug for CommittedUiEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommittedUiEvent")
            .field("seq", &self.seq)
            .field("time", &self.time)
            .field("kind", &self.kind)
            .finish()
    }
}

impl fmt::Debug for CommittedUiKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TurnStart { turn } => formatter
                .debug_struct("TurnStart")
                .field("turn", turn)
                .finish(),
            Self::TurnEnd { turn, reason } => formatter
                .debug_struct("TurnEnd")
                .field("turn", turn)
                .field("reason", reason)
                .finish(),
            Self::StepStart { turn, step } | Self::StepEnd { turn, step } => formatter
                .debug_struct(match self {
                    Self::StepStart { .. } => "StepStart",
                    _ => "StepEnd",
                })
                .field("turn", turn)
                .field("step", step)
                .finish(),
            Self::UserMessage { source, content } => formatter
                .debug_struct("UserMessage")
                .field("source", source)
                .field("content", content)
                .finish(),
            Self::AssistantTextDelta {
                turn,
                step,
                index,
                text,
            }
            | Self::AssistantReasoningDelta {
                turn,
                step,
                index,
                text,
            } => formatter
                .debug_struct(match self {
                    Self::AssistantTextDelta { .. } => "AssistantTextDelta",
                    _ => "AssistantReasoningDelta",
                })
                .field("turn", turn)
                .field("step", step)
                .field("index", index)
                .field("text_bytes", &text.len())
                .finish(),
            Self::UsageSample { turn, step, usage } => formatter
                .debug_struct("UsageSample")
                .field("turn", turn)
                .field("step", step)
                .field("usage", usage)
                .finish(),
            Self::AssistantMessage {
                turn,
                step,
                content,
                sources,
                provider,
                model,
                usage,
            } => formatter
                .debug_struct("AssistantMessage")
                .field("turn", turn)
                .field("step", step)
                .field("content", content)
                .field("sources", sources)
                .field("provider", provider)
                .field("model", model)
                .field("usage", usage)
                .finish(),
            Self::ToolRequested {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => formatter
                .debug_struct("ToolRequested")
                .field("turn", turn)
                .field("step", step)
                .field("call_id", call_id)
                .field("name", name)
                .field("arguments", arguments)
                .finish(),
            Self::ToolResult {
                turn,
                step,
                call_id,
                is_error,
                failure,
                content,
                meta,
                surface_replacement_target,
            } => formatter
                .debug_struct("ToolResult")
                .field("turn", turn)
                .field("step", step)
                .field("call_id", call_id)
                .field("is_error", is_error)
                .field("failure", failure)
                .field("content", content)
                .field("meta", meta)
                .field("surface_replacement_target", surface_replacement_target)
                .finish(),
            Self::RequestContextChanged {
                provider,
                model,
                context_window,
            } => formatter
                .debug_struct("RequestContextChanged")
                .field("provider", provider)
                .field("model", model)
                .field("context_window", context_window)
                .finish(),
            Self::CompactionStarted {
                id,
                turn,
                trigger,
                shadowed_nodes,
            } => formatter
                .debug_struct("CompactionStarted")
                .field("id", id)
                .field("turn", turn)
                .field("trigger", trigger)
                .field("shadowed_nodes", shadowed_nodes)
                .finish(),
            Self::CompactionSummarized {
                id,
                shadowed_tokens,
                provider,
                model,
                usage,
            } => formatter
                .debug_struct("CompactionSummarized")
                .field("id", id)
                .field("shadowed_tokens", shadowed_tokens)
                .field("provider", provider)
                .field("model", model)
                .field("usage", usage)
                .finish(),
            Self::CompactionEnded { id, turn, error } => formatter
                .debug_struct("CompactionEnded")
                .field("id", id)
                .field("turn", turn)
                .field("error", error)
                .finish(),
            Self::CompactionPruneMarked {
                target,
                shadowed_tokens,
            } => formatter
                .debug_struct("CompactionPruneMarked")
                .field("target", target)
                .field("shadowed_tokens", shadowed_tokens)
                .finish(),
            Self::ApprovalAsked {
                id,
                tool_name,
                call_id,
                reason,
            } => formatter
                .debug_struct("ApprovalAsked")
                .field("id", id)
                .field("tool_name", tool_name)
                .field("call_id", call_id)
                .field("reason_bytes", &reason.as_ref().map_or(0, String::len))
                .finish(),
            Self::ApprovalDecided { id, outcome } => formatter
                .debug_struct("ApprovalDecided")
                .field("id", id)
                .field("outcome", outcome)
                .finish(),
            Self::RetryScheduled {
                retry_id,
                retry,
                provider,
                delay_ms,
                max_retries,
                failure_code,
                failure_message,
            } => formatter
                .debug_struct("RetryScheduled")
                .field("retry_id", retry_id)
                .field("retry", retry)
                .field("provider", provider)
                .field("delay_ms", delay_ms)
                .field("max_retries", max_retries)
                .field("failure_code_bytes", &failure_code.len())
                .field("failure_message_bytes", &failure_message.len())
                .finish(),
            Self::RetryStarted { retry_id, retry } => formatter
                .debug_struct("RetryStarted")
                .field("retry_id", retry_id)
                .field("retry", retry)
                .finish(),
            Self::TypeOnly { event_type } => formatter
                .debug_struct("TypeOnly")
                .field("event_type", event_type)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UiTokenUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) cache_read_tokens: Option<u64>,
    pub(crate) cache_write_tokens: Option<u64>,
    pub(crate) reasoning_tokens: Option<u64>,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct UiIdentity(Arc<UiIdentityInner>);

#[derive(Eq, PartialEq)]
struct UiIdentityInner {
    display: String,
    fingerprint: [u8; 32],
    original_bytes: usize,
    omitted: bool,
}

impl UiIdentity {
    pub(crate) fn as_str(&self) -> &str {
        &self.0.display
    }

    pub(crate) fn into_display(self) -> String {
        Arc::try_unwrap(self.0)
            .map(|inner| inner.display)
            .unwrap_or_else(|inner| inner.display.clone())
    }

    pub(crate) fn original_bytes(&self) -> usize {
        self.0.original_bytes
    }

    pub(crate) fn was_omitted(&self) -> bool {
        self.0.omitted
    }

    pub(crate) fn from_static(value: &'static str) -> Self {
        let fingerprint = digest(&SHA256, value.as_bytes());
        let mut fingerprint_bytes = [0_u8; 32];
        fingerprint_bytes.copy_from_slice(fingerprint.as_ref());
        Self(Arc::new(UiIdentityInner {
            display: value.to_owned(),
            fingerprint: fingerprint_bytes,
            original_bytes: value.len(),
            omitted: false,
        }))
    }

    #[cfg(test)]
    pub(crate) fn from_text_for_test(value: &str) -> Self {
        try_ui_identity(value).expect("test identity allocation should succeed")
    }
}

impl std::ops::Deref for UiIdentity {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Debug for UiIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiIdentity")
            .field("display_bytes", &self.0.display.len())
            .field("original_bytes", &self.0.original_bytes)
            .field("omitted", &self.0.omitted)
            .finish()
    }
}

impl From<&TokenUsage> for UiTokenUsage {
    fn from(usage: &TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens().get(),
            output_tokens: usage.output_tokens().get(),
            cache_read_tokens: usage.cache_read_tokens().map(|value| value.get()),
            cache_write_tokens: usage.cache_write_tokens().map(|value| value.get()),
            reasoning_tokens: usage.reasoning_tokens().map(|value| value.get()),
        }
    }
}

pub(crate) enum UiUserSource {
    Human,
    Context {
        plugin: String,
        form: Option<ContextForm>,
    },
    Other {
        kind: String,
    },
}

impl fmt::Debug for UiUserSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Human => formatter.write_str("Human"),
            Self::Context { plugin, form } => formatter
                .debug_struct("Context")
                .field("plugin_bytes", &plugin.len())
                .field("form", form)
                .finish(),
            Self::Other { kind } => formatter
                .debug_struct("Other")
                .field("kind_bytes", &kind.len())
                .finish(),
        }
    }
}

pub(crate) struct UiOpaquePayload {
    value: Option<String>,
    original_bytes: usize,
    omitted_parts: usize,
}

impl UiOpaquePayload {
    pub(crate) fn as_str(&self) -> Option<&str> {
        self.value.as_deref()
    }

    pub(crate) fn original_bytes(&self) -> usize {
        self.original_bytes
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.value.as_ref().map_or(0, String::len)
    }

    pub(crate) fn was_omitted(&self) -> bool {
        (self.value.is_none() && self.original_bytes != 0) || self.omitted_parts != 0
    }

    pub(crate) fn omitted_parts(&self) -> usize {
        self.omitted_parts
    }

    #[cfg(test)]
    pub(crate) fn from_text_for_test(value: &str) -> Self {
        opaque_payload(value).expect("test payload allocation should succeed")
    }
}

impl fmt::Debug for UiOpaquePayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiOpaquePayload")
            .field(
                "retained_bytes",
                &self.value.as_ref().map_or(0, String::len),
            )
            .field("original_bytes", &self.original_bytes)
            .field("omitted_parts", &self.omitted_parts)
            .field("omitted", &self.was_omitted())
            .finish()
    }
}

pub(crate) struct UiCompactionError {
    pub(crate) code: Option<String>,
    pub(crate) message: String,
}

impl fmt::Debug for UiCompactionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiCompactionError")
            .field("code_bytes", &self.code.as_ref().map_or(0, String::len))
            .field("message_bytes", &self.message.len())
            .finish()
    }
}

pub(crate) enum UiTurnEndReason {
    Completed,
    Aborted { cause: UiTurnEndCancelCause },
    Blocked,
    Error { code: String, message: String },
    MaxTokens,
    Interrupted,
    Other { kind: Option<String> },
}

impl fmt::Debug for UiTurnEndReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed => formatter.write_str("Completed"),
            Self::Aborted { cause } => formatter
                .debug_struct("Aborted")
                .field("cause", cause)
                .finish(),
            Self::Blocked => formatter.write_str("Blocked"),
            Self::Error { code, message } => formatter
                .debug_struct("Error")
                .field("code_bytes", &code.len())
                .field("message_bytes", &message.len())
                .finish(),
            Self::MaxTokens => formatter.write_str("MaxTokens"),
            Self::Interrupted => formatter.write_str("Interrupted"),
            Self::Other { kind } => formatter
                .debug_struct("Other")
                .field("kind_bytes", &kind.as_ref().map_or(0, String::len))
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiTurnEndCancelCause {
    User,
    Parent,
    Hook,
    Disposed,
    Legacy,
}

pub(crate) struct UiAssistantBlock {
    pub(crate) index: u16,
    pub(crate) kind: UiAssistantBlockKind,
    pub(crate) text: String,
}

pub(crate) enum UiAssistantContent {
    Indexed(Vec<UiAssistantBlock>),
    /// Complete final answer text when block-by-block deduplication is too large.
    Degraded {
        text: String,
    },
}

impl fmt::Debug for UiAssistantBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiAssistantBlock")
            .field("index", &self.index)
            .field("kind", &self.kind)
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

impl fmt::Debug for UiAssistantContent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Indexed(blocks) => formatter
                .debug_struct("Indexed")
                .field("blocks", blocks)
                .finish(),
            Self::Degraded { text } => formatter
                .debug_struct("Degraded")
                .field("text_bytes", &text.len())
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiAssistantBlockKind {
    Text,
    Reasoning,
}

pub(crate) struct UiToolFailure {
    pub(crate) name: String,
    pub(crate) code: String,
}

impl fmt::Debug for UiToolFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiToolFailure")
            .field("name_bytes", &self.name.len())
            .field("code_bytes", &self.code.len())
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct SourceSeqBitmap {
    base: EventSeq,
    words: Vec<u64>,
}

impl SourceSeqBitmap {
    pub(crate) fn from_sources(sources: &[EventSeq]) -> Result<Self, UiProjectionError> {
        let base = sources
            .iter()
            .map(|source| source.get())
            .min()
            .map(EventSeq::new)
            .transpose()
            .map_err(|_| UiProjectionError)?
            .unwrap_or_else(|| EventSeq::new(0).expect("zero is a valid event sequence"));
        let mut words = Vec::new();
        words
            .try_reserve_exact(SOURCE_BITMAP_WORDS)
            .map_err(|_| UiProjectionError)?;
        words.resize(SOURCE_BITMAP_WORDS, 0);
        let mut bitmap = Self::finish_words(base, words)?;
        for source in sources {
            let relative = source
                .get()
                .checked_sub(base.get())
                .ok_or(UiProjectionError)?;
            let index = usize::try_from(relative).map_err(|_| UiProjectionError)?;
            if index >= super::MAX_SESSION_EVENTS {
                return Err(UiProjectionError);
            }
            bitmap.words[index / u64::BITS as usize] |= 1_u64 << (index % u64::BITS as usize);
        }
        Ok(bitmap)
    }

    fn finish_words(base: EventSeq, words: Vec<u64>) -> Result<Self, UiProjectionError> {
        if words.len() != SOURCE_BITMAP_WORDS || !Self::capacity_is_acceptable(words.capacity()) {
            return Err(UiProjectionError);
        }
        Ok(Self { base, words })
    }

    fn capacity_is_acceptable(capacity: usize) -> bool {
        (SOURCE_BITMAP_WORDS..=MAX_SOURCE_BITMAP_CAPACITY).contains(&capacity)
    }

    pub(crate) fn contains(&self, source: EventSeq) -> bool {
        let Some(relative) = source.get().checked_sub(self.base.get()) else {
            return false;
        };
        let Ok(index) = usize::try_from(relative) else {
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
    pub(crate) fn base_for_test(&self) -> EventSeq {
        self.base
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
        Self::finish_words(
            EventSeq::new(0).expect("zero is a valid event sequence"),
            words,
        )
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

pub(super) fn publish_committed(
    observer: &mut Option<CommittedUiSender>,
    event: &SessionEvent,
) -> bool {
    let Some(active) = observer.as_ref() else {
        return false;
    };
    if active.state.should_fail_projection() {
        active.state.fault();
        *observer = None;
        return true;
    }
    let projection = match CommittedUiEvent::from_event(event) {
        Ok(projection) => projection,
        Err(_) => {
            active.state.fault();
            *observer = None;
            return true;
        }
    };
    match active.sender.try_send(projection) {
        Ok(()) => false,
        Err(mpsc::error::TrySendError::Full(_)) => {
            active.state.fault();
            *observer = None;
            true
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            *observer = None;
            false
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
                StreamChunkKind::Usage { usage } => CommittedUiKind::UsageSample {
                    turn: *turn,
                    step: *step,
                    usage: UiTokenUsage::from(usage),
                },
                _ => CommittedUiKind::TypeOnly {
                    event_type: "assistant/chunk",
                },
            },
            EventKind::AssistantMessage {
                turn,
                step,
                message,
                usage,
            } => {
                let (provider, model) = match message.source().kind() {
                    MessageSourceKind::Model {
                        provider, model, ..
                    } => (provider.as_str(), model.as_str()),
                    // Memory-compatible/imported sessions can retain unusual
                    // but valid assistant facts. The UI must degrade instead
                    // of imposing a stronger invariant than Session.
                    _ => ("unknown", "unknown"),
                };
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
                    provider: try_ui_identity(provider)?,
                    model: try_ui_identity(model)?,
                    usage: usage.as_ref().map(UiTokenUsage::from),
                }
            }
            EventKind::ToolCall {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => CommittedUiKind::ToolRequested {
                turn: *turn,
                step: *step,
                call_id: try_ui_identity(call_id.as_str())?,
                name: try_ui_identity(name)?,
                arguments: opaque_payload(arguments)?,
            },
            EventKind::ToolResult {
                turn,
                step,
                message,
                error,
                meta,
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
                    call_id: try_ui_identity(call_id.as_str())?,
                    is_error: (*is_error).unwrap_or(error.is_some()),
                    failure: error.as_ref().map(project_tool_failure).transpose()?,
                    content: opaque_json(block.tool_result_content().ok_or(UiProjectionError)?)?,
                    meta: match meta {
                        Some(meta) => opaque_json(meta.as_value())?,
                        None => opaque_payload("")?,
                    },
                    surface_replacement_target: match event.surface_op() {
                        Some(SurfaceOp::Replace(replacement)) => Some(replacement.start),
                        _ => None,
                    },
                }
            }
            EventKind::ApprovalAsked { asked } => CommittedUiKind::ApprovalAsked {
                id: approval_id(asked.id())?,
                tool_name: try_ui_identity(asked.tool_name())?,
                call_id: asked
                    .call_id()
                    .map(|call_id| try_ui_identity(call_id.as_str()))
                    .transpose()?,
                reason: asked.reason().map(try_ui_text).transpose()?,
            },
            EventKind::ApprovalDecided { decided } => CommittedUiKind::ApprovalDecided {
                id: approval_id(decided.id())?,
                outcome: decided.outcome(),
            },
            EventKind::LlmRetry { retry } => CommittedUiKind::RetryScheduled {
                retry_id: try_ui_identity(retry.retry_id().as_str())?,
                retry: retry.retry(),
                provider: try_ui_identity(retry.provider())?,
                delay_ms: retry.delay_ms().get(),
                max_retries: retry.max_retries(),
                failure_code: try_ui_text(retry.failure().code())?,
                failure_message: try_ui_text(retry.failure().message())?,
            },
            EventKind::LlmRetryStarted { started } => CommittedUiKind::RetryStarted {
                retry_id: try_ui_identity(started.retry_id().as_str())?,
                retry: started.retry(),
            },
            EventKind::UserMessage { message } => CommittedUiKind::UserMessage {
                source: project_user_source(message.source().kind())?,
                content: message_text_payload(message.content())?,
            },
            EventKind::TodoWrite { .. } => CommittedUiKind::TypeOnly {
                event_type: "todo/write",
            },
            EventKind::RequestHeader { .. } => CommittedUiKind::TypeOnly {
                event_type: "request/header",
            },
            EventKind::RequestContext { context } => CommittedUiKind::RequestContextChanged {
                provider: context.provider().map(try_ui_identity).transpose()?,
                model: context.model().map(try_ui_identity).transpose()?,
                context_window: context.context_window().map(|value| value.get()),
            },
            EventKind::CompactionStart { start } => {
                let dispatch = start.dispatch();
                CommittedUiKind::CompactionStarted {
                    id: try_ui_identity(start.compaction_id().as_str())?,
                    turn: start.turn(),
                    trigger: dispatch.map(|value| value.trigger()),
                    shadowed_nodes: dispatch.map(|value| value.shadowed_seqs().len()),
                }
            }
            EventKind::CompactionSummary { summary } => CommittedUiKind::CompactionSummarized {
                id: try_ui_identity(summary.compaction_id().as_str())?,
                shadowed_tokens: summary.shadowed_token_count().get(),
                provider: try_ui_identity(summary.provider())?,
                model: try_ui_identity(summary.model())?,
                usage: summary.usage().map(UiTokenUsage::from),
            },
            EventKind::CompactionEnd { end } => CommittedUiKind::CompactionEnded {
                id: try_ui_identity(end.compaction_id().as_str())?,
                turn: end.turn(),
                error: end.error().map(project_compaction_error).transpose()?,
            },
            EventKind::CompactionPrune { prune } => CommittedUiKind::CompactionPruneMarked {
                target: prune.shadowed_range().start(),
                shadowed_tokens: prune.shadowed_token_count().get(),
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
        TurnEndReason::Aborted { reason } => UiTurnEndReason::Aborted {
            cause: match reason {
                TurnEndCancelCause::User => UiTurnEndCancelCause::User,
                TurnEndCancelCause::Parent => UiTurnEndCancelCause::Parent,
                TurnEndCancelCause::Hook { .. } => UiTurnEndCancelCause::Hook,
                TurnEndCancelCause::Disposed => UiTurnEndCancelCause::Disposed,
                TurnEndCancelCause::Legacy => UiTurnEndCancelCause::Legacy,
            },
        },
        TurnEndReason::Blocked => UiTurnEndReason::Blocked,
        TurnEndReason::Error { error } => UiTurnEndReason::Error {
            code: try_ui_text(error.code())?,
            message: try_ui_text(error.message())?,
        },
        TurnEndReason::MaxTokens => UiTurnEndReason::MaxTokens,
        TurnEndReason::Interrupted => UiTurnEndReason::Interrupted,
        TurnEndReason::Other { kind, .. } => UiTurnEndReason::Other {
            kind: kind.as_deref().map(try_ui_text).transpose()?,
        },
    })
}

fn project_tool_failure(failure: &ToolFailure) -> Result<UiToolFailure, UiProjectionError> {
    Ok(UiToolFailure {
        name: try_ui_text(&failure.name)?,
        code: try_ui_text(&failure.code)?,
    })
}

fn project_user_source(source: &MessageSourceKind) -> Result<UiUserSource, UiProjectionError> {
    Ok(match source {
        MessageSourceKind::User => UiUserSource::Human,
        MessageSourceKind::Plugin { plugin, form, .. } => UiUserSource::Context {
            plugin: try_ui_text(plugin)?,
            form: *form,
        },
        MessageSourceKind::Other { kind } => UiUserSource::Other {
            kind: try_ui_text(kind)?,
        },
        MessageSourceKind::Model { .. } => UiUserSource::Other {
            kind: try_copy("model")?,
        },
        MessageSourceKind::Tool { .. } => UiUserSource::Other {
            kind: try_copy("tool")?,
        },
    })
}

fn project_compaction_error(
    error: &CompactionEndError,
) -> Result<UiCompactionError, UiProjectionError> {
    Ok(match error {
        CompactionEndError::Failure(failure) => UiCompactionError {
            code: Some(try_ui_text(failure.code())?),
            message: try_ui_text(failure.message())?,
        },
        CompactionEndError::LegacyString(message) => UiCompactionError {
            code: None,
            message: try_ui_text(message)?,
        },
    })
}

fn message_text_payload(
    blocks: &[crate::model::ContentBlock],
) -> Result<UiOpaquePayload, UiProjectionError> {
    let omitted_parts = blocks
        .iter()
        .filter(|block| !matches!(block.kind(), ContentBlockKind::Text { .. }))
        .count();
    let total = blocks.iter().try_fold(0_usize, |total, block| {
        if let ContentBlockKind::Text { text } = block.kind() {
            total.checked_add(text.len()).ok_or(UiProjectionError)
        } else {
            Ok(total)
        }
    })?;
    if total > MAX_UI_OPAQUE_PAYLOAD_BYTES {
        return Ok(UiOpaquePayload {
            value: None,
            original_bytes: total,
            omitted_parts,
        });
    }
    let mut text = String::new();
    text.try_reserve_exact(total)
        .map_err(|_| UiProjectionError)?;
    for block in blocks {
        if let ContentBlockKind::Text { text: block_text } = block.kind() {
            text.push_str(block_text);
        }
    }
    Ok(UiOpaquePayload {
        value: Some(text),
        original_bytes: total,
        omitted_parts,
    })
}

fn opaque_json<T: serde::Serialize + ?Sized>(
    value: &T,
) -> Result<UiOpaquePayload, UiProjectionError> {
    let encoded = serde_json::to_string(value).map_err(|_| UiProjectionError)?;
    opaque_payload(&encoded)
}

fn opaque_payload(value: &str) -> Result<UiOpaquePayload, UiProjectionError> {
    if value.len() > MAX_UI_OPAQUE_PAYLOAD_BYTES {
        return Ok(UiOpaquePayload {
            value: None,
            original_bytes: value.len(),
            omitted_parts: 0,
        });
    }
    Ok(UiOpaquePayload {
        value: Some(try_copy(value)?),
        original_bytes: value.len(),
        omitted_parts: 0,
    })
}

fn approval_id(id: &ApprovalRequestId) -> Result<UiIdentity, UiProjectionError> {
    try_ui_identity(id.as_str())
}

fn try_copy(value: &str) -> Result<String, UiProjectionError> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| UiProjectionError)?;
    copy.push_str(value);
    Ok(copy)
}

fn try_ui_identity(value: &str) -> Result<UiIdentity, UiProjectionError> {
    let fingerprint = digest(&SHA256, value.as_bytes());
    let mut fingerprint_bytes = [0_u8; 32];
    fingerprint_bytes.copy_from_slice(fingerprint.as_ref());
    let omitted = value.len() > MAX_UI_IDENTITY_BYTES;
    let display = if omitted {
        let mut marker = String::new();
        marker
            .try_reserve_exact(128)
            .map_err(|_| UiProjectionError)?;
        write!(&mut marker, "[omitted {}-byte value sha256:", value.len())
            .map_err(|_| UiProjectionError)?;
        for byte in fingerprint.as_ref() {
            write!(&mut marker, "{byte:02x}").map_err(|_| UiProjectionError)?;
        }
        marker.push(']');
        marker
    } else {
        try_copy(value)?
    };
    Ok(UiIdentity(Arc::new(UiIdentityInner {
        display,
        fingerprint: fingerprint_bytes,
        original_bytes: value.len(),
        omitted,
    })))
}

fn try_ui_text(value: &str) -> Result<String, UiProjectionError> {
    if value.len() <= MAX_UI_IDENTITY_BYTES {
        return try_copy(value);
    }
    let mut marker = String::new();
    marker
        .try_reserve_exact(64)
        .map_err(|_| UiProjectionError)?;
    write!(&mut marker, "[omitted {}-byte text]", value.len()).map_err(|_| UiProjectionError)?;
    Ok(marker)
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CommittedUiEvent, CommittedUiKind, MAX_UI_IDENTITY_BYTES, MAX_UI_OPAQUE_PAYLOAD_BYTES,
        SourceSeqBitmap, UiAssistantBlock, UiAssistantBlockKind, UiAssistantContent,
        UiCompactionError, UiIdentity, UiTurnEndReason, UiUserSource, opaque_payload,
        try_ui_identity, try_ui_text,
    };
    use crate::session::{EventSeq, RetryNumber, StepId, TurnId, UnixMillis, codec::decode_event};

    #[test]
    fn compaction_projection_exposes_safe_counts_without_summary_payloads() {
        const SECRET: &str = "SECRET_COMPACTION_PAYLOAD_MUST_NOT_REACH_UI";
        let event = decode_event(
            json!({
                "type": "compaction/summary",
                "seq": 0,
                "time": 1,
                "data": {
                    "compactionId": "compact-ui",
                    "summary": [{ "type": "text", "text": SECRET }],
                    "rawOutput": [{ "type": "reasoning", "text": SECRET }],
                    "shadowedRange": { "start": 1, "end": 1 },
                    "shadowedSeqs": [1],
                    "shadowedTokenCount": 1,
                    "provider": "deepseek",
                    "model": "deepseek-chat"
                }
            }),
            0,
        )
        .unwrap();

        let projection = CommittedUiEvent::from_event(&event).unwrap();
        assert!(matches!(
            projection.kind,
            CommittedUiKind::CompactionSummarized {
                shadowed_tokens: 1,
                ref provider,
                ref model,
                ..
            } if provider.as_str() == "deepseek" && model.as_str() == "deepseek-chat"
        ));
        assert!(!format!("{projection:?}").contains(SECRET));
    }

    #[test]
    fn committed_ui_debug_never_exposes_assistant_or_failure_text() {
        const SECRET: &str = "SECRET_COMMITTED_UI_DEBUG";
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let id = || UiIdentity::from_text_for_test("safe-id");
        let kinds = vec![
            CommittedUiKind::AssistantTextDelta {
                turn,
                step,
                index: 0,
                text: SECRET.to_owned(),
            },
            CommittedUiKind::AssistantReasoningDelta {
                turn,
                step,
                index: 0,
                text: SECRET.to_owned(),
            },
            CommittedUiKind::AssistantMessage {
                turn,
                step,
                content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                    index: 0,
                    kind: UiAssistantBlockKind::Text,
                    text: SECRET.to_owned(),
                }]),
                sources: SourceSeqBitmap::from_sources(&[]).unwrap(),
                provider: id(),
                model: id(),
                usage: None,
            },
            CommittedUiKind::AssistantMessage {
                turn,
                step,
                content: UiAssistantContent::Degraded {
                    text: SECRET.to_owned(),
                },
                sources: SourceSeqBitmap::from_sources(&[]).unwrap(),
                provider: id(),
                model: id(),
                usage: None,
            },
            CommittedUiKind::ApprovalAsked {
                id: id(),
                tool_name: id(),
                call_id: Some(id()),
                reason: Some(SECRET.to_owned()),
            },
            CommittedUiKind::RetryScheduled {
                retry_id: id(),
                retry: RetryNumber::new(1).unwrap(),
                provider: id(),
                delay_ms: 1.0,
                max_retries: None,
                failure_code: SECRET.to_owned(),
                failure_message: SECRET.to_owned(),
            },
            CommittedUiKind::TurnEnd {
                turn,
                reason: UiTurnEndReason::Error {
                    code: SECRET.to_owned(),
                    message: SECRET.to_owned(),
                },
            },
            CommittedUiKind::CompactionEnded {
                id: id(),
                turn: Some(turn),
                error: Some(UiCompactionError {
                    code: Some(SECRET.to_owned()),
                    message: SECRET.to_owned(),
                }),
            },
        ];
        for (seq, kind) in kinds.into_iter().enumerate() {
            let event = CommittedUiEvent {
                seq: EventSeq::new(u64::try_from(seq).unwrap()).unwrap(),
                time: UnixMillis::new(i64::try_from(seq).unwrap()).unwrap(),
                kind,
            };
            let debug = format!("{event:?}");
            assert!(!debug.contains(SECRET));
            assert!(debug.contains("bytes") || debug.contains("UiCompactionError"));
        }
    }

    #[test]
    fn direct_user_content_is_distinct_from_plugin_context() {
        let human = decode_event(
            json!({
                "type": "user/message",
                "seq": 0,
                "time": 1,
                "data": {
                    "id": "human",
                    "role": "user",
                    "content": [
                        { "type": "text", "text": "hello" },
                        { "type": "future-block", "secret": "must-not-be-silently-rendered" }
                    ],
                    "source": { "kind": "user" }
                },
                "surfaceOp": "append"
            }),
            0,
        )
        .unwrap();
        let context = decode_event(
            json!({
                "type": "user/message",
                "seq": 1,
                "time": 2,
                "data": {
                    "id": "checkpoint",
                    "role": "user",
                    "content": [{ "type": "text", "text": "internal context" }],
                    "source": {
                        "kind": "plugin",
                        "plugin": "compact",
                        "form": "notice",
                        "summary": "checkpoint"
                    }
                },
                "surfaceOp": "append"
            }),
            1,
        )
        .unwrap();
        let unusual_but_valid = decode_event(
            json!({
                "type": "user/message",
                "seq": 2,
                "time": 3,
                "data": {
                    "id": "model-sourced-user-role",
                    "role": "user",
                    "content": [{ "type": "text", "text": "retained" }],
                    "source": {
                        "kind": "model",
                        "provider": "provider",
                        "model": "model"
                    }
                },
                "surfaceOp": "append"
            }),
            2,
        )
        .unwrap();

        assert!(matches!(
            CommittedUiEvent::from_event(&human).unwrap().kind,
            CommittedUiKind::UserMessage {
                source: UiUserSource::Human,
                ref content,
            } if content.as_str() == Some("hello") && content.omitted_parts() == 1
        ));
        assert!(matches!(
            CommittedUiEvent::from_event(&context).unwrap().kind,
            CommittedUiKind::UserMessage {
                source: UiUserSource::Context { ref plugin, .. },
                ..
            } if plugin == "compact"
        ));
        assert!(matches!(
            CommittedUiEvent::from_event(&unusual_but_valid)
                .unwrap()
                .kind,
            CommittedUiKind::UserMessage {
                source: UiUserSource::Other { ref kind },
                ..
            } if kind == "model"
        ));
    }

    #[test]
    fn opaque_ui_payload_is_exact_bounded_and_debug_redacted() {
        const SECRET: &str = "UI_PAYLOAD_SECRET";
        let exact_text = format!(
            "{SECRET}{}",
            "x".repeat(MAX_UI_OPAQUE_PAYLOAD_BYTES - SECRET.len())
        );
        let exact = opaque_payload(&exact_text).unwrap();
        assert_eq!(
            exact.as_str().map(str::len),
            Some(MAX_UI_OPAQUE_PAYLOAD_BYTES)
        );
        assert!(!format!("{exact:?}").contains(SECRET));

        let over = opaque_payload(&format!("{exact_text}x")).unwrap();
        assert!(over.as_str().is_none());
        assert!(over.was_omitted());
        assert_eq!(over.original_bytes(), MAX_UI_OPAQUE_PAYLOAD_BYTES + 1);
    }

    #[test]
    fn ui_identities_are_exact_at_the_limit_and_distinctly_fingerprinted_above_it() {
        let exact = "x".repeat(MAX_UI_IDENTITY_BYTES);
        assert_eq!(try_ui_identity(&exact).unwrap().as_str(), exact);

        let first = format!("{}a", "x".repeat(MAX_UI_IDENTITY_BYTES));
        let second = format!("{}b", "x".repeat(MAX_UI_IDENTITY_BYTES));
        let projected_first = try_ui_identity(&first).unwrap();
        let projected_second = try_ui_identity(&second).unwrap();
        assert_ne!(projected_first, projected_second);
        assert!(projected_first.as_str().len() < 128);
        assert!(projected_first.as_str().contains("4097-byte value sha256:"));
        assert!(!projected_first.as_str().contains(&first));

        let literal_marker = try_ui_identity(projected_first.as_str()).unwrap();
        assert_ne!(projected_first, literal_marker);
    }

    #[test]
    fn oversized_human_readable_text_is_omitted_without_a_secret_fingerprint() {
        let secret = format!("LOW_ENTROPY_SECRET{}", "x".repeat(MAX_UI_IDENTITY_BYTES));
        let projected = try_ui_text(&secret).unwrap();
        assert_eq!(projected, format!("[omitted {}-byte text]", secret.len()));
        assert!(!projected.contains("sha256"));
        assert!(!projected.contains("LOW_ENTROPY_SECRET"));
    }

    #[test]
    fn prune_marker_and_surface_replacement_project_the_same_historical_target() {
        let marker = decode_event(
            json!({
                "type": "compaction/prune",
                "seq": 10,
                "time": 1,
                "data": {
                    "shadowedRange": { "start": 9, "end": 9 },
                    "shadowedSeqs": [9],
                    "shadowedTokenCount": 24
                }
            }),
            10,
        )
        .unwrap();
        let replacement = decode_event(
            json!({
                "type": "tool/result",
                "seq": 11,
                "time": 2,
                "data": {
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "id": "result-1",
                        "role": "user",
                        "content": [{
                            "type": "tool-result",
                            "toolCallId": "call-1",
                            "content": [{ "type": "text", "text": "pruned" }],
                            "isError": false
                        }],
                        "source": { "kind": "tool", "callId": "call-1" }
                    }
                },
                "surfaceOp": { "op": "replace", "start": 9, "end": 9 },
                "sourceEventSeqs": [9]
            }),
            11,
        )
        .unwrap();

        assert!(matches!(
            CommittedUiEvent::from_event(&marker).unwrap().kind,
            CommittedUiKind::CompactionPruneMarked {
                target,
                shadowed_tokens: 24,
            } if target.get() == 9
        ));
        assert!(matches!(
            CommittedUiEvent::from_event(&replacement).unwrap().kind,
            CommittedUiKind::ToolResult {
                surface_replacement_target: Some(target),
                ..
            } if target.get() == 9
        ));
    }
}
