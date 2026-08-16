//! Structured failures for session construction, append, replay, and codecs.

use thiserror::Error;

use crate::model::{CallId, JsonValueError, ModelError};

use super::{EventSeq, StepId, TurnId};

/// A durable integer falls outside the exact JavaScript number domain.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum NumberError {
    #[error("{field} must be a non-negative JavaScript-safe integer")]
    NonNegativeSafeInteger { field: &'static str },
    #[error("{field} must be a positive JavaScript-safe integer")]
    PositiveSafeInteger { field: &'static str },
    #[error("{field} must be a signed JavaScript-safe integer")]
    SignedSafeInteger { field: &'static str },
}

/// Clock failure before an event receives a timestamp.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("{message}")]
pub struct ClockError {
    message: String,
}

impl ClockError {
    /// Construct a clock failure with user-readable context.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Invalid durable header fields.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum HeaderError {
    #[error("session format version must be {expected}, got {actual}")]
    UnsupportedVersion { expected: u64, actual: u64 },
    #[error("session header id {actual:?} does not match requested id {expected:?}")]
    MismatchedId { expected: String, actual: String },
    #[error("session header createdAt must be non-negative")]
    NegativeCreatedAt,
    #[error("session header cwd must be an absolute path: {0:?}")]
    RelativeWorkingDirectory(String),
    #[error("session header {field} exceeds the JavaScript safe-integer range")]
    UnsafeInteger { field: &'static str },
    #[error("session header field {field} is invalid: {detail}")]
    InvalidField { field: &'static str, detail: String },
    #[error("session header is {actual} bytes; maximum is {maximum}")]
    TooLarge { maximum: usize, actual: usize },
    #[error(transparent)]
    Json(#[from] JsonValueError),
}

/// A candidate event violates turn, step, or tool-call relations.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum TransitionError {
    #[error("turn {open} is already open; cannot start turn {attempted}")]
    TurnAlreadyOpen { open: TurnId, attempted: TurnId },
    #[error("turn/start expected turn {expected}, got {actual}")]
    WrongNextTurn { expected: TurnId, actual: TurnId },
    #[error("turn/end names turn {actual}, but the open turn is {open:?}")]
    WrongTurnEnd {
        open: Option<TurnId>,
        actual: TurnId,
    },
    #[error("turn {turn} cannot end while step {step} is open")]
    TurnEndWhileStepOpen { turn: TurnId, step: StepId },
    #[error("step/start names turn {actual}, but the open turn is {open:?}")]
    StepOutsideTurn {
        open: Option<TurnId>,
        actual: TurnId,
    },
    #[error("step {open} is already open; cannot start step {attempted}")]
    StepAlreadyOpen { open: StepId, attempted: StepId },
    #[error("step/start expected step {expected} in turn {turn}, got {actual}")]
    WrongNextStep {
        turn: TurnId,
        expected: StepId,
        actual: StepId,
    },
    #[error(
        "{event_type} names turn {actual_turn}/step {actual_step}, but open is turn {open_turn:?}/step {open_step:?}"
    )]
    WrongOpenStep {
        event_type: &'static str,
        open_turn: Option<TurnId>,
        open_step: Option<StepId>,
        actual_turn: TurnId,
        actual_step: StepId,
    },
    #[error("{event_type} requires an open turn")]
    EventOutsideTurn { event_type: &'static str },
    #[error("tool/result for {call_id} has no prior tool/call in this step")]
    MissingToolCall { call_id: CallId },
    #[error("llm/retry names provider {actual:?}, but the open request uses {expected:?}")]
    RetryProviderMismatch { expected: String, actual: String },
    #[error("llm/retry expected retry {expected}, got {actual}")]
    WrongRetryNumber {
        expected: super::RetryNumber,
        actual: super::RetryNumber,
    },
    #[error("llm/retry must preserve retryId {expected}, got {actual}")]
    RetryChainIdMismatch {
        expected: super::RetryId,
        actual: super::RetryId,
    },
    #[error("llm/retry retryId {retry_id} is already owned by another chain")]
    RetryIdAlreadyOwned { retry_id: super::RetryId },
    #[error("llm/retry-started has no matching scheduled retry {retry} in chain {retry_id}")]
    RetryStartedWithoutSchedule {
        retry_id: super::RetryId,
        retry: super::RetryNumber,
    },
    #[error("llm/retry-started repeats retry {retry} in chain {retry_id}")]
    RetryStartedTwice {
        retry_id: super::RetryId,
        retry: super::RetryNumber,
    },
    #[error("approval request id {approval_id} is already pending")]
    ApprovalIdAlreadyPending {
        approval_id: super::ApprovalRequestId,
    },
    #[error("approval request id {approval_id} was already used")]
    ApprovalIdAlreadyOwned {
        approval_id: super::ApprovalRequestId,
    },
    #[error("approval decision id {approval_id} has no pending request")]
    ApprovalDecisionWithoutRequest {
        approval_id: super::ApprovalRequestId,
    },
    #[error("{event_type} cannot close while approval request {approval_id} is pending")]
    ApprovalStillPending {
        event_type: &'static str,
        approval_id: super::ApprovalRequestId,
    },
    #[error("durable tool call {call_id} has an invalid id or tool name")]
    InvalidDurableToolCallIdentity { call_id: CallId },
    #[error("a durable step may declare at most {maximum} tool calls")]
    TooManyDurableToolCalls { maximum: usize },
    #[error("durable assistant messages cannot declare call id {call_id} twice in one step")]
    DuplicateDurableToolCall { call_id: CallId },
    #[error("durable tool/call {call_id} has no next assistant declaration")]
    DurableToolCallWithoutDeclaration { call_id: CallId },
    #[error("durable tool/call {actual} does not match the next declaration {expected}")]
    DurableToolCallMismatch { expected: CallId, actual: CallId },
    #[error("durable approval/asked must name its tool call")]
    DurableApprovalWithoutCall,
    #[error("durable approval references unavailable call {call_id}")]
    DurableApprovalCallMismatch { call_id: CallId },
    #[error("durable approval tool {actual:?} does not match declared tool {expected:?}")]
    DurableApprovalToolMismatch { expected: String, actual: String },
    #[error("durable call {call_id} cannot ask for approval more than once")]
    DurableApprovalRepeated { call_id: CallId },
    #[error("durable state permits only one pending approval; {pending} is still pending")]
    MultipleDurableApprovals { pending: super::ApprovalRequestId },
    #[error("durable approval decision {approval_id} is not owned by an unresolved call")]
    DurableApprovalDecisionMismatch {
        approval_id: super::ApprovalRequestId,
    },
    #[error("durable tool/result does not match declared call {call_id}")]
    DurableToolResultMismatch { call_id: CallId },
    #[error("durable tool/result repeats the result for call {call_id}")]
    DuplicateDurableToolResult { call_id: CallId },
    #[error("durable tool/result for call {call_id} arrived before its approval decision")]
    DurableToolResultBeforeDecision { call_id: CallId },
    #[error("durable tool/result for call {call_id} cites the wrong intent source")]
    DurableToolResultWrongSource { call_id: CallId },
    #[error("durable tool/result for call {call_id} has no intent and is not a canonical repair")]
    DurableToolResultWithoutIntent { call_id: CallId },
    #[error("{event_type} can be appended only by the owned recovery lifecycle")]
    DurableRecoveryEventNotAllowed { event_type: &'static str },
    #[error("{event_type} can be appended only by the owned tool-result pruner")]
    DurablePruneEventNotAllowed { event_type: &'static str },
    #[error("durable result for call {call_id} does not match approval decision {approval_id}")]
    DurableApprovalResultMismatch {
        approval_id: super::ApprovalRequestId,
        call_id: CallId,
    },
    #[error("{event_type} cannot close while durable call {call_id} has no result")]
    DurableCallStillPending {
        event_type: &'static str,
        call_id: CallId,
    },
    #[error("compaction/start cannot nest inside compaction {open}")]
    CompactionAlreadyOpen { open: super::CompactionId },
    #[error("{event_type} has no matching compaction/start")]
    CompactionWithoutStart { event_type: &'static str },
    #[error("{event_type} compaction id {actual} does not match open id {expected}")]
    CompactionIdMismatch {
        event_type: &'static str,
        expected: super::CompactionId,
        actual: super::CompactionId,
    },
    #[error("{event_type} sourceCommandId does not match compaction/start")]
    CompactionSourceCommandMismatch { event_type: &'static str },
    #[error("{event_type} compaction owner does not match the open turn or bracket")]
    CompactionOwnerMismatch { event_type: &'static str },
    #[error("{event_type} cannot cross an open compaction bracket")]
    CompactionBoundaryCrossed { event_type: &'static str },
    #[error("{event_type} is out of order in the open compaction bracket")]
    CompactionBodyOutOfOrder { event_type: &'static str },
    #[error("successful compaction/end requires its adjacent replacement checkpoint")]
    CompactionSuccessWithoutReplacement,
    #[error("durable compaction/start requires a complete dispatch snapshot")]
    DurableCompactionDispatchRequired,
    #[error("durable compaction/end cannot use the legacy string error shape")]
    DurableLegacyCompactionError,
    #[error("compaction dispatch does not match current Session facts: {0}")]
    CompactionDispatchMismatch(&'static str),
    #[error("turn or step number has no representable successor")]
    IdentifierExhausted,
}

/// A candidate event violates model-visible surface rules.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SurfaceError {
    #[error("surface event {event_type:?} requires a surfaceOp marker")]
    MissingOperation { event_type: String },
    #[error("non-surface event {event_type:?} cannot carry surface metadata")]
    MetadataOnIneligibleEvent { event_type: String },
    #[error("sourceEventSeqs can be empty only on assistant/message")]
    EmptySources,
    #[error("sourceEventSeqs contains {actual} entries; maximum is {maximum}")]
    TooManySources { maximum: usize, actual: usize },
    #[error("sourceEventSeqs contains duplicate seq {0}")]
    DuplicateSource(EventSeq),
    #[error("sourceEventSeqs must refer to earlier events; {source_seq} is not before {current}")]
    SourceNotEarlier {
        source_seq: EventSeq,
        current: EventSeq,
    },
    #[error("surface replace start seq {0} is not a current surface node")]
    StartNotFound(EventSeq),
    #[error("surface replace end seq {0} is not a current surface node")]
    EndNotFound(EventSeq),
    #[error("surface replace start seq {start} is after end seq {end}")]
    ReversedRange { start: EventSeq, end: EventSeq },
    #[error("surface replacement does not cite shadowed seq {0}")]
    MissingShadowedSource(EventSeq),
    #[error("tool/result surface replacement must rewrite exactly one current node")]
    ToolResultMultipleTargets,
    #[error("tool/result surface replacement must target a current tool/result")]
    ToolResultWrongTarget,
    #[error("tool/result surface replacement may change only model-facing result content")]
    ToolResultChangedIdentity,
    #[error("surface token accounting exceeded its bounded integer domain")]
    TokenAccountingOverflow,
    #[error("surface tool calls and results do not form balanced positional groups")]
    UnbalancedToolSurface,
    #[error("shadowed token count is {actual}, but the selected surface costs {expected}")]
    ShadowedTokenCountMismatch { expected: u64, actual: u64 },
    #[error(
        "compaction replacement costs {replacement} tokens and does not shrink the {shadowed}-token range"
    )]
    CompactionDoesNotShrink { shadowed: u64, replacement: u64 },
    #[error("compaction checkpoint replacement does not exactly match its summary claim")]
    CompactionReplacementMismatch,
    #[error("compaction/prune must target one current tool/result node")]
    PruneTargetNotToolResult,
    #[error("durable tool/result replacement requires an adjacent compaction/prune marker")]
    PruneReplacementWithoutMarker,
    #[error("tool/result replacement does not match the adjacent compaction/prune marker")]
    PruneReplacementMismatch,
}

/// Semantic validation shared by live append and replay.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum EventValidationError {
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Transition(#[from] TransitionError),
    #[error(transparent)]
    Surface(#[from] SurfaceError),
    #[error("unknown events can enter only through validated replay")]
    UnknownLiveEvent,
    #[error("event seq must equal its zero-based log position: expected {expected}, got {actual}")]
    NonContiguousSequence {
        expected: EventSeq,
        actual: EventSeq,
    },
    #[error("an unknown event is missing ignorable: true")]
    UnknownRequiredEvent,
    #[error("legacy request/header reason \"fallback\" is unsupported")]
    LegacyRequestHeaderReason,
    #[error("request/header reason {reason:?} must use its canonical typed variant")]
    NonCanonicalRequestHeaderReason { reason: String },
    #[error("turn/end reason's typed kind disagrees with its retained JSON")]
    InconsistentTurnEndReason,
    #[error("invalid llm retry event: {0}")]
    InvalidRetryEvent(&'static str),
    #[error("invalid approval event: {0}")]
    InvalidApprovalEvent(&'static str),
    #[error("invalid compaction event: {0}")]
    InvalidCompactionEvent(&'static str),
    #[error("session contains {actual} events; maximum is {maximum}")]
    TooManyEvents { maximum: usize, actual: usize },
}

/// A live append failed before changing committed session state.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AppendError {
    #[error("a deferred durable session must be materialized before appending")]
    NeedsMaterialization,
    #[error("an active durable session requires the asynchronous append path")]
    DurableAsyncRequired,
    #[error("the durable session is poisoned")]
    DurablePoisoned,
    #[error("the durable session writer could not settle the event")]
    DurableWriter,
    #[error("the event cannot fit one durable journal record")]
    DurableRecord,
    #[error("the previous durable append must be settled before another event starts")]
    NeedsAppendSettle,
    #[error("the durable session reached its ordinary limit of {maximum} logical events")]
    DurableEventLimit { maximum: u64 },
    #[error("the durable session reached its ordinary limit of {maximum} journal bytes")]
    DurableByteLimit { maximum: u64 },
    #[error(transparent)]
    Clock(#[from] ClockError),
    #[error(transparent)]
    Validation(#[from] EventValidationError),
    #[error("session event sequence is exhausted")]
    SequenceExhausted,
    #[error("session already contains the maximum of {maximum} events")]
    EventLimit { maximum: usize },
    #[error("session would retain more than {maximum} compact JSON bytes")]
    RetainedJsonLimit { maximum: usize },
    #[error(
        "session reservation protects {reserved} event slot(s); this append would exceed the maximum of {maximum}"
    )]
    ReservedEventLimit { maximum: usize, reserved: usize },
    #[error(
        "session reservation protects {reserved} compact JSON bytes; this append would exceed the maximum of {maximum}"
    )]
    ReservedRetainedJsonLimit { maximum: usize, reserved: usize },
    #[error("event claim does not belong to this active session reservation")]
    InvalidClaim,
    #[error(
        "event claim protects {reserved} compact JSON bytes, but the rebound payload needs {actual}"
    )]
    ClaimPayloadTooLarge { reserved: usize, actual: usize },
    #[error("event claim protects {reserved} durable row bytes, but the event needs {actual}")]
    ClaimRowTooLarge { reserved: u64, actual: u64 },
    #[error("could not reserve memory for the next session event")]
    Capacity,
}

/// Replaying one imported event prefix failed at a specific position.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid session event at index {index}: {source}")]
pub struct ReplayError {
    pub index: usize,
    #[source]
    pub source: EventValidationError,
}

/// JSON syntax, wire-shape, or unknown-event failure.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("durable session history must be read through the journal reader")]
    DurableSnapshotUnavailable,
    #[error("invalid session JSON: {0}")]
    Syntax(#[from] serde_json::Error),
    #[error("session snapshot is {actual} bytes; maximum is {maximum} bytes")]
    SnapshotTooLarge { maximum: usize, actual: usize },
    #[error("session contains {actual} events; maximum is {maximum}")]
    TooManyEvents { maximum: usize, actual: usize },
    #[error("session snapshot must contain exactly header and events")]
    SnapshotEnvelope,
    #[error("session event at index {index} has an invalid envelope: {detail}")]
    EventEnvelope { index: usize, detail: String },
    #[error("session event at index {index} has invalid JSON data: {detail}")]
    EventData { index: usize, detail: String },
    #[error("session event at index {index} has invalid {event_type} data: {source}")]
    EventPayload {
        index: usize,
        event_type: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("session event at index {index} uses unknown required type {event_type:?}")]
    UnknownRequiredEvent { index: usize, event_type: String },
    #[error("session snapshot cannot be encoded: {0}")]
    Encode(serde_json::Error),
    #[error(transparent)]
    Header(#[from] HeaderError),
    #[error(transparent)]
    Model(#[from] ModelError),
}

/// Constructing a session from a clock, header, or seed failed atomically.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error(transparent)]
    Clock(#[from] ClockError),
    #[error(transparent)]
    Header(#[from] HeaderError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error(transparent)]
    Replay(#[from] ReplayError),
    #[error(transparent)]
    Append(#[from] AppendError),
    #[error("session would retain more than {maximum} compact JSON bytes")]
    RetainedJsonLimit { maximum: usize },
}
