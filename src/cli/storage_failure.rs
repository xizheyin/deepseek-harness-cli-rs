//! Classify durable Session failures for the process-facing CLI boundary.

use crate::{
    agent::AgentLoopError,
    session::{AppendError, BarrierError, SessionIoError, StoreError},
};

/// Recover the storage cause that an Agent turn intentionally carries without
/// exposing unrelated model, tool, or state-machine diagnostics as filesystem
/// failures.
pub(super) fn from_agent(error: &AgentLoopError) -> Option<StoreError> {
    match error {
        AgentLoopError::Store(error) | AgentLoopError::Barrier(BarrierError::Storage(error)) => {
            Some(*error)
        }
        AgentLoopError::Admission(error)
        | AgentLoopError::Session(error)
        | AgentLoopError::Barrier(BarrierError::Append(error)) => Some(from_append(error)),
        AgentLoopError::Barrier(BarrierError::ObserverUnavailable)
        | AgentLoopError::Poisoned
        | AgentLoopError::SessionNotIdle
        | AgentLoopError::InvalidTurnMessages
        | AgentLoopError::TooManyTurnMessages { .. }
        | AgentLoopError::TurnInputTooLarge { .. }
        | AgentLoopError::Model(_)
        | AgentLoopError::Event(_)
        | AgentLoopError::Runtime(_)
        | AgentLoopError::Serialization(_)
        | AgentLoopError::Invariant(_) => None,
    }
}

/// Shutdown has already narrowed failures to either the journal or an append
/// that the journal needed to settle, so both branches have a stable storage
/// classification.
pub(super) fn from_shutdown(error: &SessionIoError) -> StoreError {
    match error {
        SessionIoError::Storage(error) => *error,
        SessionIoError::Append(error) => from_append(error),
    }
}

/// Map internal substrate detail onto the deliberately small public CLI code
/// vocabulary documented for Phase 8.
pub(super) const fn stable_code(error: StoreError) -> &'static str {
    match error {
        StoreError::RootUnavailable | StoreError::UnsafeRoot => "CLI_SESSION_ROOT_UNAVAILABLE",
        StoreError::Busy => "CLI_SESSION_BUSY",
        StoreError::StoreBusy => "CLI_SESSION_STORE_BUSY",
        StoreError::NotFound => "CLI_SESSION_NOT_FOUND",
        StoreError::Changed => "CLI_SESSION_CHANGED",
        StoreError::WorkspaceMismatch => "CLI_SESSION_WORKSPACE_MISMATCH",
        StoreError::Unsupported => "CLI_SESSION_UNSUPPORTED",
        StoreError::Corrupt => "CLI_SESSION_CORRUPT",
        StoreError::Limit => "CLI_SESSION_LIMIT",
        StoreError::Io
        | StoreError::Cancelled
        | StoreError::InvalidSessionId
        | StoreError::InvalidHeader
        | StoreError::WriterStopped
        | StoreError::Poisoned => "CLI_SESSION_IO",
    }
}

fn from_append(error: &AppendError) -> StoreError {
    match error {
        AppendError::DurableRecord
        | AppendError::DurableEventLimit { .. }
        | AppendError::DurableByteLimit { .. }
        | AppendError::DurableResidentLimit { .. }
        | AppendError::EventLimit { .. }
        | AppendError::RetainedJsonLimit { .. }
        | AppendError::ReservedEventLimit { .. }
        | AppendError::ReservedRetainedJsonLimit { .. }
        | AppendError::ClaimPayloadTooLarge { .. }
        | AppendError::ClaimRowTooLarge { .. }
        | AppendError::Capacity => StoreError::Limit,
        AppendError::NeedsMaterialization
        | AppendError::DurableAsyncRequired
        | AppendError::DurablePoisoned
        | AppendError::DurableWriter
        | AppendError::NeedsAppendSettle
        | AppendError::Clock(_)
        | AppendError::Validation(_)
        | AppendError::SequenceExhausted
        | AppendError::InvalidClaim => StoreError::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::{from_agent, stable_code};
    use crate::{
        agent::AgentLoopError,
        session::{AppendError, StoreError},
    };

    #[test]
    fn internal_store_detail_folds_to_the_documented_cli_vocabulary() {
        assert_eq!(
            stable_code(StoreError::UnsafeRoot),
            "CLI_SESSION_ROOT_UNAVAILABLE"
        );
        for error in [
            StoreError::InvalidSessionId,
            StoreError::InvalidHeader,
            StoreError::WriterStopped,
            StoreError::Poisoned,
        ] {
            assert_eq!(stable_code(error), "CLI_SESSION_IO");
        }
    }

    #[test]
    fn every_durable_quota_failure_maps_to_the_single_public_limit_code() {
        for append in [
            AppendError::DurableRecord,
            AppendError::DurableEventLimit { maximum: 1 },
            AppendError::DurableByteLimit { maximum: 1 },
            AppendError::DurableResidentLimit { maximum: 1 },
        ] {
            assert_eq!(
                from_agent(&AgentLoopError::Session(append)),
                Some(StoreError::Limit)
            );
        }
        assert_eq!(stable_code(StoreError::Limit), "CLI_SESSION_LIMIT");
    }
}
