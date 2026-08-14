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
    #[error("session contains {actual} events; maximum is {maximum}")]
    TooManyEvents { maximum: usize, actual: usize },
}

/// A live append failed before changing committed session state.
#[derive(Debug, Error)]
pub enum AppendError {
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
