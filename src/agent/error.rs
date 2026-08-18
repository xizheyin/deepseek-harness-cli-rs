use thiserror::Error;

use crate::{
    model::ModelError,
    session::{
        AppendError, BarrierError, EventValidationError, Session, SessionIoError, StoreError,
    },
};

use super::ToolExecutorError;

/// Invalid Agent construction or configured resource ceiling.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AgentBuildError {
    #[error("agent requires an idle session")]
    SessionNotIdle,
    #[error("agent session contains an unresolved tool call and requires append-only repair")]
    UnresolvedToolCall,
    #[error("agent session contains an unresolved approval and requires append-only repair")]
    UnresolvedApproval,
    #[error("agent limit {name} must be between {minimum} and {maximum}, got {actual}")]
    InvalidLimit {
        name: &'static str,
        minimum: u64,
        maximum: u64,
        actual: u64,
    },
    #[error("agent has {actual} tool schemas; maximum is {maximum}")]
    TooManyTools { maximum: usize, actual: usize },
    #[error("agent tool schema names must be non-empty and unique")]
    InvalidToolNames,
    #[error("agent fixed request facts are {actual} bytes; maximum is {maximum}")]
    FixedRequestTooLarge { maximum: usize, actual: usize },
}

/// A deterministic ID/jitter source violated its small public contract.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum AgentRuntimeError {
    #[error("agent runtime entropy is unavailable")]
    EntropyUnavailable,
    #[error("agent runtime returned an empty {kind} id")]
    EmptyId { kind: &'static str },
    #[error("agent runtime jitter sample must be finite and between zero and one")]
    InvalidSample,
}

/// Failure observed while the Agent settles tools and then its Session.
#[derive(Debug, Error)]
pub enum AgentShutdownError {
    #[error("agent tool shutdown failed")]
    Tools(#[source] ToolExecutorError),
    #[error("agent session shutdown failed")]
    Session(#[source] SessionIoError),
    #[error("agent tool and session shutdown both failed")]
    Both {
        tools: ToolExecutorError,
        session: SessionIoError,
    },
}

impl AgentShutdownError {
    #[must_use]
    pub fn session_error(&self) -> Option<&SessionIoError> {
        match self {
            Self::Tools(_) => None,
            Self::Session(error) | Self::Both { session: error, .. } => Some(error),
        }
    }
}

/// Tool cleanup failed while returning an otherwise active Session.
pub struct AgentReleaseError {
    source: ToolExecutorError,
    session: Session,
}

impl AgentReleaseError {
    pub(super) fn new(source: ToolExecutorError, session: Session) -> Self {
        Self { source, session }
    }

    #[must_use]
    pub fn into_parts(self) -> (ToolExecutorError, Session) {
        (self.source, self.session)
    }
}

impl std::fmt::Debug for AgentReleaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentReleaseError")
            .field("source", &self.source)
            .field("session", &"<active>")
            .finish()
    }
}

impl std::fmt::Display for AgentReleaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("agent tool shutdown failed before the Session was released")
    }
}

impl std::error::Error for AgentReleaseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

/// Infrastructure failure for which a balanced durable result cannot be promised.
#[derive(Debug, Error)]
pub enum AgentLoopError {
    #[error("agent requires repair after an earlier incomplete durable operation")]
    Poisoned,
    #[error("agent session is not idle at the start of a turn")]
    SessionNotIdle,
    #[error("entered turn messages must all be user-role messages")]
    InvalidTurnMessages,
    #[error("entered turn has {actual} messages; maximum is {maximum}")]
    TooManyTurnMessages { maximum: usize, actual: usize },
    #[error("entered turn retains {actual} bytes across {messages} messages; maximum is {maximum}")]
    TurnInputTooLarge {
        maximum: usize,
        actual: usize,
        messages: usize,
    },
    #[error("session cannot admit a balanced turn: {0}")]
    Admission(AppendError),
    #[error(transparent)]
    Session(#[from] AppendError),
    #[error(transparent)]
    Barrier(#[from] BarrierError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Event(#[from] EventValidationError),
    #[error(transparent)]
    Runtime(#[from] AgentRuntimeError),
    #[error("agent internal serialization failed: {0}")]
    Serialization(String),
    #[error("agent invariant failed: {0}")]
    Invariant(&'static str),
}
