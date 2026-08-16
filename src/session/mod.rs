//! Append-only session log and deterministic projections.

mod clock;
mod codec;
mod error;
mod event;
mod journal;
mod jsonl;
mod observer;
mod path_policy;
#[cfg(test)]
mod phase5_tests;
#[cfg(test)]
mod phase7_tests;
mod projection;
mod recovery;
mod resume;
mod store;

use std::sync::Arc;

pub use clock::{Clock, SystemClock};
pub use codec::{MAX_SESSION_EVENTS, MAX_SESSION_RETAINED_JSON_BYTES, MAX_SESSION_SNAPSHOT_BYTES};
pub use error::{
    AppendError, ClockError, CodecError, EventValidationError, HeaderError, NumberError,
    ReplayError, SessionError, SurfaceError, TransitionError,
};
pub use event::{
    ApprovalAskedEvent, ApprovalDecidedEvent, ApprovalOutcome, ApprovalRequestId, EpochHeader,
    EventKind, EventSeq, LlmRetryEvent, LlmRetryMode, LlmRetryStartedEvent, MAX_SAFE_INTEGER,
    MAX_SESSION_HEADER_BYTES, MAX_SOURCE_EVENT_SEQS, NewEvent, RequestContext, RequestHeaderReason,
    RetryId, RetryNumber, SESSION_FORMAT_VERSION, SessionEvent, SessionHeader, SessionId,
    SessionOrigin, StepId, SurfaceAppend, SurfaceIntent, SurfaceOp, SurfaceReplace,
    TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN, TodoItem, TodoStatus, ToolFailure, TurnEndCancelCause,
    TurnEndReason, TurnId, UnixMillis,
};
pub use projection::SessionState;
pub(crate) use store::SessionMetadata;
pub use store::{SessionStore, StoreError};

pub(crate) use observer::{
    CommittedUiEvent, CommittedUiKind, CommittedUiReceiver, SourceSeqBitmap, UiAssistantBlockKind,
    UiAssistantContent, UiObserverAttachError, UiTurnEndReason,
};
#[cfg(test)]
pub(crate) use observer::{UiAssistantBlock, UiToolFailure};
pub(crate) use recovery::{RecoveryCallReport, RecoveryReport};
#[cfg(test)]
pub(crate) use resume::PreparingResume;
pub(crate) use resume::RecoveredSession;

use crate::model::{JsonValue, Message};

use self::{
    codec::decode_snapshot,
    journal::JournalError,
    jsonl::{
        DurableTimestamp, EventLineTemplate, MAX_JOURNAL_EVENT_LINE_BYTES,
        prepared_event_line_upper_bound,
    },
    projection::{Projection, ValidationPolicy},
    store::{DeferredJournal, SessionStorage},
};

#[derive(Clone, Debug, Eq, PartialEq)]
struct PreparedEvent {
    event: NewEvent,
    original_data: JsonValue,
    retained_json_bytes: usize,
}

const MAX_DURABLE_JOURNAL_BYTES: u64 = 512 * 1024 * 1024;
const DURABLE_REPAIR_RESERVED_BYTES: u64 = 1024 * 1024;
const MAX_DURABLE_LOGICAL_EVENTS: u64 = 1_000_000;
const DURABLE_REPAIR_RESERVED_EVENTS: u64 = 68;

#[derive(Default)]
struct PendingDurableBatch {
    bytes: Vec<u8>,
    event_count: usize,
}

struct PendingDurableOperation {
    prepared: PreparedEvent,
    protected_events: u64,
    protected_row_bytes: u64,
    owner: DurableOperationOwner,
}

enum DurableOperationOwner {
    Ordinary,
    Claim {
        reservation: Arc<()>,
        token: u64,
        kind: ClaimOperationKind,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClaimOperationKind {
    Preferred,
    Fallback,
    Exact,
    PreferredOnly,
}

enum DurableLifecycle {
    Running,
    ShuttingDown { first_error: Option<SessionIoError> },
}

enum SessionMode {
    Memory {
        events: Vec<SessionEvent>,
        retained_json_bytes: usize,
    },
    Durable {
        storage: SessionStorage,
        logical_event_count: u64,
        accepted_journal_bytes: u64,
        pending_batch: PendingDurableBatch,
        pending_operation: Option<PendingDurableOperation>,
        barrier_error: Option<AppendError>,
        lifecycle: DurableLifecycle,
    },
}

enum DurableAppendAttempt {
    Committed(AppendReceipt),
    NeedsStorageSettle(PendingDurableOperation),
    Failed(AppendError),
}

/// A durability checkpoint failed before an external effect could start.
#[derive(Clone, Debug, Eq, thiserror::Error, PartialEq)]
pub enum BarrierError {
    #[error(transparent)]
    Append(#[from] AppendError),
    #[error(transparent)]
    Storage(#[from] StoreError),
    #[error("CLI_SESSION_OBSERVER_UNAVAILABLE")]
    ObserverUnavailable,
}

/// A clean Session shutdown failed while still reclaiming its owned writer.
#[derive(Clone, Debug, Eq, thiserror::Error, PartialEq)]
pub enum SessionIoError {
    #[error(transparent)]
    Append(#[from] AppendError),
    #[error(transparent)]
    Storage(#[from] StoreError),
}

/// Capacity still available before taking any active reservation into account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionBudget {
    pub remaining_events: usize,
    pub remaining_retained_json_bytes: usize,
}

/// Small owned acknowledgement for one committed event.
///
/// Durable sessions cannot return a reference into an all-history `Vec` because
/// old journal rows leave resident memory. Surface-message receipts retain only
/// the same shallow immutable message handle installed in the projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendReceipt {
    seq: EventSeq,
    time: UnixMillis,
    event_type: &'static str,
    observer_faulted: bool,
    committed_message: Option<Message>,
}

impl AppendReceipt {
    /// Continuous durable sequence assigned to the event.
    #[must_use]
    pub fn seq(&self) -> EventSeq {
        self.seq
    }

    /// Timestamp committed with the event.
    #[must_use]
    pub fn time(&self) -> UnixMillis {
        self.time
    }

    /// Stable wire tag for the committed event.
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        self.event_type
    }

    /// Whether the live observer became permanently unusable at this commit.
    #[must_use]
    pub fn observer_faulted(&self) -> bool {
        self.observer_faulted
    }

    /// Shared model-visible message for a surface event, when present.
    #[must_use]
    pub fn committed_message(&self) -> Option<&Message> {
        self.committed_message.as_ref()
    }
}

/// Result of fulfilling one exact fallback claim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimedAppend {
    Preferred(AppendReceipt),
    Fallback(AppendReceipt),
}

/// One concrete event whose exact payload cost is protected by a reservation.
#[derive(Debug)]
pub struct EventClaim {
    owner: Arc<()>,
    token: u64,
    fallback: PreparedEvent,
    reserved_retained_json_bytes: usize,
    reserved_row_bytes: u64,
    settled: bool,
}

/// Exclusive append view that prevents ordinary events from consuming claims.
pub struct SessionReservation<'a> {
    session: &'a mut Session,
    owner: Arc<()>,
    reserved_events: usize,
    reserved_retained_json_bytes: usize,
    reserved_row_bytes: u64,
    next_claim_token: u64,
}

/// Result of replaying a complete in-memory event prefix without adding events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayProjection {
    state: SessionState,
    messages: Vec<Message>,
}

impl ReplayProjection {
    /// Turn, step, pending-call, and surface state after the prefix.
    #[must_use]
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Exact messages visible to the next provider request.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Latest canonical model-request header after this prefix.
    #[must_use]
    pub fn request_header(&self) -> Option<&EpochHeader> {
        self.state.request_header()
    }

    /// Latest complete route-capacity record after this prefix.
    #[must_use]
    pub fn request_context(&self) -> Option<&RequestContext> {
        self.state.request_context()
    }
}

/// One owned in-memory session whose event log is the source of truth.
pub struct Session {
    header: SessionHeader,
    next_seq: Option<EventSeq>,
    projection: Projection,
    first_live_seq: usize,
    clock: Box<dyn Clock>,
    ui_observer: Option<observer::CommittedUiSender>,
    ui_observer_attached: bool,
    observer_attach_at: Option<EventSeq>,
    ui_observer_faulted: bool,
    mode: SessionMode,
}

impl Session {
    /// Create a fresh session with the system clock and no seed marker.
    pub fn new(id: impl Into<SessionId>) -> Result<Self, SessionError> {
        Self::with_clock(id, SystemClock)
    }

    /// Create a fresh session with an injected clock.
    pub fn with_clock(
        id: impl Into<SessionId>,
        clock: impl Clock + 'static,
    ) -> Result<Self, SessionError> {
        let id = id.into();
        let created_at = clock.now()?;
        let header = SessionHeader::new(id, created_at)?;
        let retained_json_bytes = header.raw().encoded_len();
        Ok(Self {
            header,
            next_seq: EventSeq::from_index(0),
            projection: Projection::empty(ValidationPolicy::MemoryCompatible),
            first_live_seq: 0,
            clock: Box::new(clock),
            ui_observer: None,
            ui_observer_attached: false,
            observer_attach_at: EventSeq::new(0).ok(),
            ui_observer_faulted: false,
            mode: SessionMode::Memory {
                events: Vec::new(),
                retained_json_bytes,
            },
        })
    }

    fn new_deferred(
        header: SessionHeader,
        clock: impl Clock + 'static,
        journal: DeferredJournal,
        header_bytes: u64,
    ) -> Self {
        Self {
            header,
            next_seq: EventSeq::from_index(0),
            projection: Projection::empty(ValidationPolicy::DurableStrict),
            first_live_seq: 0,
            clock: Box::new(clock),
            ui_observer: None,
            ui_observer_attached: false,
            observer_attach_at: EventSeq::new(0).ok(),
            ui_observer_faulted: false,
            mode: SessionMode::Durable {
                storage: SessionStorage::Deferred(journal),
                logical_event_count: 0,
                accepted_journal_bytes: header_bytes,
                pending_batch: PendingDurableBatch::default(),
                pending_operation: None,
                barrier_error: None,
                lifecycle: DurableLifecycle::Running,
            },
        }
    }

    #[cfg(test)]
    fn new_active_for_test(
        id: impl Into<SessionId>,
        clock: impl Clock + 'static,
        writer: journal::JournalWriter,
    ) -> Result<Self, SessionError> {
        let id = id.into();
        let created_at = clock.now()?;
        let header = SessionHeader::new(id, created_at)?;
        Ok(Self {
            header,
            next_seq: EventSeq::from_index(0),
            projection: Projection::empty(ValidationPolicy::DurableStrict),
            first_live_seq: 0,
            clock: Box::new(clock),
            ui_observer: None,
            ui_observer_attached: false,
            observer_attach_at: EventSeq::new(0).ok(),
            ui_observer_faulted: false,
            mode: SessionMode::Durable {
                storage: SessionStorage::Active(writer),
                logical_event_count: 0,
                accepted_journal_bytes: 0,
                pending_batch: PendingDurableBatch::default(),
                pending_operation: None,
                barrier_error: None,
                lifecycle: DurableLifecycle::Running,
            },
        })
    }

    /// Validate a borrowed seed, preserve it verbatim, and mark its end once.
    pub fn from_seed(
        header: SessionHeader,
        seed: &[SessionEvent],
        clock: impl Clock + 'static,
    ) -> Result<Self, SessionError> {
        header.validate_for(header.id())?;
        let projection = replay_projection(seed)?;
        let retained_json_bytes = retained_json_bytes(&header, seed)?;
        let first_live_seq = seed.len();
        let mut session = Self {
            header,
            next_seq: EventSeq::from_index(seed.len()),
            projection,
            first_live_seq,
            clock: Box::new(clock),
            ui_observer: None,
            ui_observer_attached: false,
            observer_attach_at: None,
            ui_observer_faulted: false,
            mode: SessionMode::Memory {
                events: seed.to_vec(),
                retained_json_bytes,
            },
        };
        if !matches!(
            session.events().last().map(SessionEvent::kind),
            Some(EventKind::EndSeed)
        ) {
            session.append(NewEvent::log(EventKind::EndSeed))?;
        }
        Ok(session)
    }

    pub(crate) fn new_recovered(
        seed: recovery::RecoveredSeed,
        clock: Box<dyn Clock>,
        writer: journal::JournalWriter,
    ) -> Self {
        Self {
            header: seed.header,
            next_seq: Some(seed.next_seq),
            projection: seed.projection,
            first_live_seq: seed.first_live_seq,
            clock,
            ui_observer: None,
            ui_observer_attached: false,
            observer_attach_at: Some(seed.next_seq),
            ui_observer_faulted: false,
            mode: SessionMode::Durable {
                storage: SessionStorage::Active(writer),
                logical_event_count: seed.logical_event_count,
                accepted_journal_bytes: seed.accepted_journal_bytes,
                pending_batch: PendingDurableBatch::default(),
                pending_operation: None,
                barrier_error: None,
                lifecycle: DurableLifecycle::Running,
            },
        }
    }

    /// Decode a snapshot, validate its header and event prefix, then mark the seed boundary.
    pub fn from_json(input: &str, clock: impl Clock + 'static) -> Result<Self, SessionError> {
        let (header, events) = match decode_snapshot(input) {
            Ok(decoded) => decoded,
            Err(CodecError::Header(error)) => return Err(SessionError::Header(error)),
            Err(error) => return Err(SessionError::Codec(error)),
        };
        header.validate_for(header.id())?;
        Self::from_seed(header, &events, clock)
    }

    /// Append one event atomically after all ordinary validation succeeds.
    pub fn append(&mut self, event: NewEvent) -> Result<AppendReceipt, AppendError> {
        self.ensure_memory_append()?;
        let prepared = Self::prepare_event(event)?;
        self.append_prepared(prepared, 0, 0)
    }

    /// Append one event through either the in-memory or durable owner.
    ///
    /// A durable event is committed to the projection and the owner-held
    /// pending batch without awaiting I/O. If an older committed batch or
    /// writer flight must settle first, this future keeps the one prepared
    /// candidate across that await and retries it without generating a second
    /// timestamp or identity.
    pub async fn append_settled(&mut self, event: NewEvent) -> Result<AppendReceipt, AppendError> {
        if matches!(self.mode, SessionMode::Memory { .. }) {
            return self.append(event);
        }
        self.ensure_durable_active()?;
        let prepared = Self::prepare_event(event)?;
        self.append_prepared_settled(prepared, 0, 0).await
    }

    async fn append_prepared_settled(
        &mut self,
        prepared: PreparedEvent,
        protected_events: u64,
        protected_row_bytes: u64,
    ) -> Result<AppendReceipt, AppendError> {
        self.ensure_durable_active()?;
        if self.has_pending_durable_operation() {
            return Err(AppendError::NeedsAppendSettle);
        }
        let SessionMode::Durable {
            pending_operation, ..
        } = &mut self.mode
        else {
            return Err(AppendError::DurableAsyncRequired);
        };
        *pending_operation = Some(PendingDurableOperation {
            prepared,
            protected_events,
            protected_row_bytes,
            owner: DurableOperationOwner::Ordinary,
        });
        self.settle_pending_append()
            .await?
            .ok_or(AppendError::DurableWriter)
    }

    fn has_pending_durable_operation(&self) -> bool {
        matches!(
            self.mode,
            SessionMode::Durable {
                pending_operation: Some(_),
                ..
            }
        )
    }

    fn pending_durable_operation_is_claim(&self) -> bool {
        matches!(
            &self.mode,
            SessionMode::Durable {
                pending_operation: Some(PendingDurableOperation {
                    owner: DurableOperationOwner::Claim { .. },
                    ..
                }),
                ..
            }
        )
    }

    fn pending_claim_operation(
        &self,
        reservation: &Arc<()>,
        token: u64,
    ) -> Result<Option<ClaimOperationKind>, AppendError> {
        let SessionMode::Durable {
            pending_operation, ..
        } = &self.mode
        else {
            return Ok(None);
        };
        match pending_operation.as_ref().map(|operation| &operation.owner) {
            None => Ok(None),
            Some(DurableOperationOwner::Claim {
                reservation: pending_reservation,
                token: pending,
                kind,
            }) if Arc::ptr_eq(pending_reservation, reservation) && *pending == token => {
                Ok(Some(*kind))
            }
            Some(DurableOperationOwner::Ordinary | DurableOperationOwner::Claim { .. }) => {
                Err(AppendError::NeedsAppendSettle)
            }
        }
    }

    async fn append_claim_prepared_settled(
        &mut self,
        prepared: PreparedEvent,
        protected_events: u64,
        protected_row_bytes: u64,
        reservation: Arc<()>,
        token: u64,
        kind: ClaimOperationKind,
    ) -> Result<AppendReceipt, AppendError> {
        self.ensure_durable_active()?;
        if self.has_pending_durable_operation() {
            return Err(AppendError::NeedsAppendSettle);
        }
        let SessionMode::Durable {
            pending_operation, ..
        } = &mut self.mode
        else {
            return Err(AppendError::DurableAsyncRequired);
        };
        *pending_operation = Some(PendingDurableOperation {
            prepared,
            protected_events,
            protected_row_bytes,
            owner: DurableOperationOwner::Claim {
                reservation,
                token,
                kind,
            },
        });
        self.settle_pending_append()
            .await?
            .ok_or(AppendError::DurableWriter)
    }

    fn has_committed_durable_batch(&self) -> bool {
        matches!(
            self.mode,
            SessionMode::Durable {
                pending_batch: PendingDurableBatch { event_count, .. },
                ..
            } if event_count != 0
        )
    }

    /// Resume the exact prepared append retained after a cancelled wait.
    pub(crate) async fn settle_pending_append(
        &mut self,
    ) -> Result<Option<AppendReceipt>, AppendError> {
        self.settle_pending_append_inner(false).await
    }

    async fn settle_pending_append_for_shutdown(
        &mut self,
    ) -> Result<Option<AppendReceipt>, AppendError> {
        self.settle_pending_append_inner(true).await
    }

    async fn settle_pending_append_inner(
        &mut self,
        shutdown_cleanup: bool,
    ) -> Result<Option<AppendReceipt>, AppendError> {
        if shutdown_cleanup {
            self.ensure_durable_cleanup_active()?;
        } else {
            self.ensure_durable_active()?;
        }
        if !self.has_pending_durable_operation() {
            return Ok(None);
        }
        loop {
            let operation = match &mut self.mode {
                SessionMode::Durable {
                    pending_operation, ..
                } => pending_operation.take().ok_or(AppendError::DurableWriter)?,
                SessionMode::Memory { .. } => return Err(AppendError::DurableAsyncRequired),
            };
            match self.try_commit_durable(operation) {
                DurableAppendAttempt::Committed(receipt) => return Ok(Some(receipt)),
                DurableAppendAttempt::NeedsStorageSettle(candidate) => {
                    let SessionMode::Durable {
                        pending_operation, ..
                    } = &mut self.mode
                    else {
                        return Err(AppendError::DurableAsyncRequired);
                    };
                    *pending_operation = Some(candidate);
                    self.flush_committed_batch().await?;
                }
                DurableAppendAttempt::Failed(error) => return Err(error),
            }
        }
    }

    fn ensure_durable_active(&self) -> Result<(), AppendError> {
        match &self.mode {
            SessionMode::Memory { .. } => Err(AppendError::DurableAsyncRequired),
            SessionMode::Durable {
                storage,
                lifecycle: DurableLifecycle::Running,
                barrier_error: None,
                ..
            } => match storage {
                SessionStorage::Deferred(_) => Err(AppendError::NeedsMaterialization),
                SessionStorage::Active(_) => Ok(()),
                SessionStorage::Finishing(_)
                | SessionStorage::Failed(_)
                | SessionStorage::Closed => Err(AppendError::DurablePoisoned),
            },
            SessionMode::Durable {
                lifecycle: DurableLifecycle::ShuttingDown { .. },
                ..
            }
            | SessionMode::Durable {
                barrier_error: Some(_),
                ..
            } => Err(AppendError::DurablePoisoned),
        }
    }

    fn ensure_durable_cleanup_active(&self) -> Result<(), AppendError> {
        match &self.mode {
            SessionMode::Durable {
                storage: SessionStorage::Active(_),
                ..
            } => Ok(()),
            SessionMode::Durable {
                storage: SessionStorage::Deferred(_),
                ..
            } => Err(AppendError::NeedsMaterialization),
            SessionMode::Memory { .. } => Err(AppendError::DurableAsyncRequired),
            SessionMode::Durable { .. } => Err(AppendError::DurablePoisoned),
        }
    }

    fn try_commit_durable(&mut self, operation: PendingDurableOperation) -> DurableAppendAttempt {
        let PendingDurableOperation {
            prepared,
            protected_events,
            protected_row_bytes,
            owner,
        } = operation;
        let Some(seq) = self.next_seq else {
            return DurableAppendAttempt::Failed(AppendError::SequenceExhausted);
        };
        let next_seq = seq
            .get()
            .checked_add(1)
            .and_then(|next| EventSeq::new(next).ok());
        let next_logical_event_count = match &self.mode {
            SessionMode::Memory { .. } => {
                return DurableAppendAttempt::Failed(AppendError::DurableAsyncRequired);
            }
            SessionMode::Durable {
                storage,
                logical_event_count,
                pending_batch,
                ..
            } => {
                let SessionStorage::Active(writer) = storage else {
                    return DurableAppendAttempt::Failed(match storage {
                        SessionStorage::Deferred(_) => AppendError::NeedsMaterialization,
                        SessionStorage::Finishing(_)
                        | SessionStorage::Failed(_)
                        | SessionStorage::Closed => AppendError::DurablePoisoned,
                        SessionStorage::Active(_) => AppendError::DurableWriter,
                    });
                };
                if writer.ensure_stageable().is_err() || pending_batch.event_count != 0 {
                    return DurableAppendAttempt::NeedsStorageSettle(PendingDurableOperation {
                        prepared,
                        protected_events,
                        protected_row_bytes,
                        owner,
                    });
                }
                let Some(logical) = logical_event_count
                    .checked_add(1)
                    .and_then(|value| value.checked_add(protected_events))
                else {
                    return DurableAppendAttempt::Failed(AppendError::SequenceExhausted);
                };
                let ordinary_max = MAX_DURABLE_LOGICAL_EVENTS - DURABLE_REPAIR_RESERVED_EVENTS;
                if logical > ordinary_max {
                    return DurableAppendAttempt::Failed(AppendError::DurableEventLimit {
                        maximum: ordinary_max,
                    });
                }
                logical - protected_events
            }
        };
        let PreparedEvent {
            event: new_event,
            original_data,
            retained_json_bytes: _,
        } = prepared;
        let placeholder_time = match i64::try_from(MAX_SAFE_INTEGER)
            .ok()
            .and_then(|value| UnixMillis::new(value).ok())
        {
            Some(time) => time,
            None => return DurableAppendAttempt::Failed(AppendError::SequenceExhausted),
        };
        let mut event = SessionEvent::from_new(seq, placeholder_time, new_event, original_data);
        let Some(event_type) = event.kind().live_event_type() else {
            return DurableAppendAttempt::Failed(EventValidationError::UnknownLiveEvent.into());
        };
        let projection = match self.projection.with_event(&event) {
            Ok(projection) => projection,
            Err(error) => return DurableAppendAttempt::Failed(error.into()),
        };
        let committed_message = match event.kind() {
            EventKind::UserMessage { message }
            | EventKind::AssistantMessage { message, .. }
            | EventKind::ToolResult { message, .. } => Some(message.clone()),
            _ => None,
        };
        let row_template = match EventLineTemplate::new(&event) {
            Ok(template) => template,
            Err(_) => return DurableAppendAttempt::Failed(AppendError::DurableRecord),
        };
        let row_template_len = row_template.encoded_len();
        let row_bytes = match u64::try_from(row_template_len) {
            Ok(bytes) => bytes,
            Err(_) => return DurableAppendAttempt::Failed(AppendError::DurableRecord),
        };
        let ordinary_byte_max = MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES;
        let next_accepted_upper_bound = match &mut self.mode {
            SessionMode::Durable {
                accepted_journal_bytes,
                pending_batch,
                ..
            } => {
                let Some(next) = accepted_journal_bytes.checked_add(row_bytes) else {
                    return DurableAppendAttempt::Failed(AppendError::DurableByteLimit {
                        maximum: ordinary_byte_max,
                    });
                };
                if next
                    .checked_add(protected_row_bytes)
                    .is_none_or(|with_claims| with_claims > ordinary_byte_max)
                {
                    return DurableAppendAttempt::Failed(AppendError::DurableByteLimit {
                        maximum: ordinary_byte_max,
                    });
                }
                if pending_batch
                    .bytes
                    .try_reserve_exact(row_template_len)
                    .is_err()
                {
                    return DurableAppendAttempt::Failed(AppendError::Capacity);
                }
                next
            }
            SessionMode::Memory { .. } => {
                return DurableAppendAttempt::Failed(AppendError::DurableAsyncRequired);
            }
        };
        let time = match self.clock.now() {
            Ok(time) => time,
            Err(error) => return DurableAppendAttempt::Failed(error.into()),
        };
        let time_value = match u64::try_from(time.get()) {
            Ok(value) => value,
            Err(_) => {
                return DurableAppendAttempt::Failed(
                    ClockError::new("live event clock returned a negative timestamp").into(),
                );
            }
        };
        let Some(timestamp) = DurableTimestamp::new(time_value) else {
            return DurableAppendAttempt::Failed(
                ClockError::new("live event clock returned an out-of-range timestamp").into(),
            );
        };
        event.set_time_for_commit(time);
        let row = row_template.finish(timestamp);
        let Some(unused_timestamp_bytes) = row_template_len.checked_sub(row.len()) else {
            if let SessionMode::Durable { storage, .. } = &mut self.mode {
                *storage = SessionStorage::Failed(StoreError::Poisoned);
            }
            return DurableAppendAttempt::Failed(AppendError::DurablePoisoned);
        };
        let unused_timestamp_bytes = match u64::try_from(unused_timestamp_bytes) {
            Ok(bytes) => bytes,
            Err(_) => {
                if let SessionMode::Durable { storage, .. } = &mut self.mode {
                    *storage = SessionStorage::Failed(StoreError::Poisoned);
                }
                return DurableAppendAttempt::Failed(AppendError::DurablePoisoned);
            }
        };
        let Some(next_accepted_journal_bytes) =
            next_accepted_upper_bound.checked_sub(unused_timestamp_bytes)
        else {
            if let SessionMode::Durable { storage, .. } = &mut self.mode {
                *storage = SessionStorage::Failed(StoreError::Poisoned);
            }
            return DurableAppendAttempt::Failed(AppendError::DurablePoisoned);
        };
        let SessionMode::Durable {
            logical_event_count,
            accepted_journal_bytes,
            pending_batch,
            storage: SessionStorage::Active(_),
            ..
        } = &mut self.mode
        else {
            return DurableAppendAttempt::Failed(AppendError::DurablePoisoned);
        };
        pending_batch.bytes.extend_from_slice(&row);
        pending_batch.event_count += 1;
        *logical_event_count = next_logical_event_count;
        *accepted_journal_bytes = next_accepted_journal_bytes;
        self.next_seq = next_seq;
        self.projection = projection;
        let observer_faulted = observer::publish_committed(&mut self.ui_observer, &event);
        self.ui_observer_faulted |= observer_faulted;
        DurableAppendAttempt::Committed(AppendReceipt {
            seq: event.seq(),
            time: event.time(),
            event_type,
            observer_faulted: self.ui_observer_faulted,
            committed_message,
        })
    }

    async fn flush_committed_batch(&mut self) -> Result<(), AppendError> {
        let settle_result = match &mut self.mode {
            SessionMode::Memory { .. } => return Ok(()),
            SessionMode::Durable { storage, .. } => match storage {
                SessionStorage::Active(writer) => writer.settle_before_stage().await,
                SessionStorage::Deferred(_) => return Err(AppendError::NeedsMaterialization),
                SessionStorage::Finishing(_)
                | SessionStorage::Failed(_)
                | SessionStorage::Closed => {
                    return Err(AppendError::DurablePoisoned);
                }
            },
        };
        if let Err(error) = settle_result {
            self.fail_durable_writer(error);
            return Err(map_journal_append_error(error));
        }

        let bytes = match &mut self.mode {
            SessionMode::Durable { pending_batch, .. } if pending_batch.event_count != 0 => {
                pending_batch.event_count = 0;
                Some(std::mem::take(&mut pending_batch.bytes))
            }
            SessionMode::Durable { .. } | SessionMode::Memory { .. } => None,
        };
        let Some(bytes) = bytes else {
            return Ok(());
        };
        let result = match &mut self.mode {
            SessionMode::Durable {
                storage: SessionStorage::Active(writer),
                ..
            } => match writer.stage(bytes) {
                Ok(()) => writer.flush_staged().await,
                Err(error) => Err(error),
            },
            _ => return Err(AppendError::DurablePoisoned),
        };
        if let Err(error) = result {
            self.fail_durable_writer(error);
            return Err(map_journal_append_error(error));
        }
        Ok(())
    }

    fn fail_durable_writer(&mut self, error: JournalError) {
        if let SessionMode::Durable { storage, .. } = &mut self.mode {
            *storage = SessionStorage::Failed(StoreError::from(error));
        }
    }

    /// Lazily create and fully synchronize this durable session's header.
    ///
    /// Cancelling the wait does not lose ownership of an in-flight creation;
    /// calling this method again settles that same operation.
    pub async fn materialize_if_needed(&mut self) -> Result<(), StoreError> {
        if matches!(
            &self.mode,
            SessionMode::Durable {
                lifecycle: DurableLifecycle::ShuttingDown { .. },
                ..
            }
        ) {
            return Err(StoreError::WriterStopped);
        }
        let result = match &mut self.mode {
            SessionMode::Memory { .. } => return Ok(()),
            SessionMode::Durable { storage, .. } => match storage {
                SessionStorage::Active(_) => return Ok(()),
                SessionStorage::Finishing(_) => return Err(StoreError::WriterStopped),
                SessionStorage::Failed(error) => return Err(*error),
                SessionStorage::Closed => return Err(StoreError::WriterStopped),
                SessionStorage::Deferred(journal) => journal.wait_ready().await,
            },
        };
        let SessionMode::Durable { storage, .. } = &mut self.mode else {
            return Err(StoreError::WriterStopped);
        };
        match result {
            Ok(writer) => {
                *storage = SessionStorage::Active(writer);
                Ok(())
            }
            Err(error) => {
                *storage = SessionStorage::Failed(error);
                Err(error)
            }
        }
    }

    /// Synchronize all durable facts staged before this point.
    pub async fn flush_barrier(&mut self) -> Result<(), BarrierError> {
        if matches!(
            &self.mode,
            SessionMode::Durable {
                lifecycle: DurableLifecycle::ShuttingDown { .. },
                ..
            }
        ) {
            return match self.shutdown().await {
                Ok(()) => Err(BarrierError::Storage(StoreError::WriterStopped)),
                Err(SessionIoError::Append(error)) => Err(BarrierError::Append(error)),
                Err(SessionIoError::Storage(error)) => Err(BarrierError::Storage(error)),
            };
        }
        let mut append_error = match &self.mode {
            SessionMode::Durable { barrier_error, .. } => barrier_error.clone(),
            SessionMode::Memory { .. } => None,
        };
        if self.pending_durable_operation_is_claim() {
            append_error.get_or_insert(AppendError::NeedsAppendSettle);
        } else if self.has_pending_durable_operation() {
            if let Err(error) = self.settle_pending_append().await {
                self.remember_barrier_error(error.clone());
                self.discard_pending_durable_operation();
                append_error.get_or_insert(error);
            }
        }
        if self.has_committed_durable_batch() {
            if let Err(error) = self.flush_committed_batch().await {
                self.remember_barrier_error(error.clone());
                append_error.get_or_insert(error);
            }
        }
        let result = match &mut self.mode {
            SessionMode::Memory { .. } => Ok(()),
            SessionMode::Durable { storage, .. } => match storage {
                SessionStorage::Deferred(_) => Err(StoreError::WriterStopped),
                SessionStorage::Active(writer) => {
                    writer.barrier().await.map(|_| ()).map_err(StoreError::from)
                }
                SessionStorage::Finishing(_) => Err(StoreError::WriterStopped),
                SessionStorage::Failed(error) => Err(*error),
                SessionStorage::Closed => Err(StoreError::WriterStopped),
            },
        };
        if let Err(error) = result {
            if let SessionMode::Durable { storage, .. } = &mut self.mode {
                if matches!(storage, SessionStorage::Active(_)) {
                    *storage = SessionStorage::Failed(error);
                }
            }
        }
        if let Err(error) = result {
            self.take_barrier_error();
            return append_error.map_or_else(
                || Err(BarrierError::Storage(error)),
                |error| Err(BarrierError::Append(error)),
            );
        }
        if let Some(error) = append_error {
            self.take_barrier_error();
            return Err(BarrierError::Append(error));
        }
        if self.ui_observer_faulted {
            return Err(BarrierError::ObserverUnavailable);
        }
        Ok(())
    }

    /// Finish the owned journal thread and release its file lock.
    ///
    /// The first semantic or storage error remains owned by the Session while
    /// cleanup continues. Cancelling the wait after `finish` is sent leaves a
    /// `Finishing` writer here, so a later shutdown resumes that exact command.
    pub async fn shutdown(&mut self) -> Result<(), SessionIoError> {
        if matches!(self.mode, SessionMode::Memory { .. }) {
            return Ok(());
        }
        self.begin_durable_shutdown();
        if let Some(error) = self.take_barrier_error() {
            self.remember_shutdown_error(SessionIoError::Append(error));
        }

        let closed_or_failed = match &self.mode {
            SessionMode::Durable { storage, .. } => match storage {
                SessionStorage::Closed => Some(None),
                SessionStorage::Failed(error) => Some(Some(*error)),
                SessionStorage::Deferred(_)
                | SessionStorage::Active(_)
                | SessionStorage::Finishing(_) => None,
            },
            SessionMode::Memory { .. } => Some(None),
        };
        if let Some(storage_error) = closed_or_failed {
            if let Some(error) = storage_error {
                self.remember_shutdown_error(SessionIoError::Storage(error));
            }
            return self.shutdown_result();
        }

        let deferred_result = match &mut self.mode {
            SessionMode::Durable {
                storage: SessionStorage::Deferred(journal),
                ..
            } if journal.has_started() => Some(journal.wait_ready().await),
            SessionMode::Durable {
                storage: SessionStorage::Deferred(_),
                ..
            } => {
                self.set_durable_storage(SessionStorage::Closed);
                return self.shutdown_result();
            }
            SessionMode::Durable { .. } | SessionMode::Memory { .. } => None,
        };
        if let Some(result) = deferred_result {
            match result {
                Ok(writer) => self.set_durable_storage(SessionStorage::Active(writer)),
                Err(error) => {
                    self.set_durable_storage(SessionStorage::Failed(error));
                    self.remember_shutdown_error(SessionIoError::Storage(error));
                    return self.shutdown_result();
                }
            }
        }

        if self.has_pending_durable_operation() {
            if let Err(error) = self.settle_pending_append_for_shutdown().await {
                self.remember_shutdown_error(SessionIoError::Append(error));
                self.discard_pending_durable_operation();
            }
        }
        if self.has_committed_durable_batch() {
            if let Err(error) = self.flush_committed_batch().await {
                self.remember_shutdown_error(SessionIoError::Append(error));
            }
        }

        self.begin_durable_finishing();
        let finish_result = match &mut self.mode {
            SessionMode::Durable {
                storage: SessionStorage::Finishing(writer),
                ..
            } => Some(writer.finish().await.map(|_| ()).map_err(StoreError::from)),
            SessionMode::Durable {
                storage: SessionStorage::Failed(error),
                ..
            } => {
                let error = *error;
                self.remember_shutdown_error(SessionIoError::Storage(error));
                None
            }
            SessionMode::Durable {
                storage: SessionStorage::Closed,
                ..
            } => None,
            SessionMode::Durable {
                storage: SessionStorage::Deferred(_) | SessionStorage::Active(_),
                ..
            }
            | SessionMode::Memory { .. } => {
                self.remember_shutdown_error(SessionIoError::Storage(StoreError::WriterStopped));
                None
            }
        };
        if let Some(result) = finish_result {
            match result {
                Ok(()) => self.set_durable_storage(SessionStorage::Closed),
                Err(error) => {
                    self.set_durable_storage(SessionStorage::Failed(error));
                    self.remember_shutdown_error(SessionIoError::Storage(error));
                }
            }
        }
        self.shutdown_result()
    }

    fn set_durable_storage(&mut self, next: SessionStorage) {
        if let SessionMode::Durable { storage, .. } = &mut self.mode {
            *storage = next;
        }
    }

    fn begin_durable_shutdown(&mut self) {
        if let SessionMode::Durable { lifecycle, .. } = &mut self.mode {
            if matches!(lifecycle, DurableLifecycle::Running) {
                *lifecycle = DurableLifecycle::ShuttingDown { first_error: None };
            }
        }
    }

    fn begin_durable_finishing(&mut self) {
        let SessionMode::Durable { storage, .. } = &mut self.mode else {
            return;
        };
        let current = std::mem::replace(storage, SessionStorage::Closed);
        *storage = match current {
            SessionStorage::Active(writer) => SessionStorage::Finishing(writer),
            other => other,
        };
    }

    fn discard_pending_durable_operation(&mut self) {
        if let SessionMode::Durable {
            pending_operation, ..
        } = &mut self.mode
        {
            pending_operation.take();
        }
    }

    fn remember_shutdown_error(&mut self, error: SessionIoError) {
        if let SessionMode::Durable {
            lifecycle: DurableLifecycle::ShuttingDown { first_error },
            ..
        } = &mut self.mode
        {
            first_error.get_or_insert(error);
        }
    }

    fn remember_barrier_error(&mut self, error: AppendError) {
        if let SessionMode::Durable { barrier_error, .. } = &mut self.mode {
            barrier_error.get_or_insert(error);
        }
    }

    fn take_barrier_error(&mut self) -> Option<AppendError> {
        match &mut self.mode {
            SessionMode::Durable { barrier_error, .. } => barrier_error.take(),
            SessionMode::Memory { .. } => None,
        }
    }

    fn shutdown_result(&self) -> Result<(), SessionIoError> {
        match &self.mode {
            SessionMode::Memory { .. } => Ok(()),
            SessionMode::Durable {
                lifecycle: DurableLifecycle::Running,
                ..
            } => Ok(()),
            SessionMode::Durable {
                lifecycle: DurableLifecycle::ShuttingDown { first_error },
                ..
            } => first_error.clone().map_or(Ok(()), Err),
        }
    }

    fn ensure_memory_append(&self) -> Result<(), AppendError> {
        match &self.mode {
            SessionMode::Memory { .. } => Ok(()),
            SessionMode::Durable { storage, .. } => match storage {
                SessionStorage::Deferred(_) => Err(AppendError::NeedsMaterialization),
                SessionStorage::Active(_) => Err(AppendError::DurableAsyncRequired),
                SessionStorage::Finishing(_)
                | SessionStorage::Failed(_)
                | SessionStorage::Closed => Err(AppendError::DurablePoisoned),
            },
        }
    }

    fn prepare_event(event: NewEvent) -> Result<PreparedEvent, AppendError> {
        if matches!(event.kind, EventKind::Unknown { .. }) {
            return Err(EventValidationError::UnknownLiveEvent.into());
        }
        event.kind.validate()?;
        let original_data =
            JsonValue::new(codec::kind_data_value(&event.kind).map_err(|error| {
                EventValidationError::from(crate::model::ModelError::InvalidShape {
                    subject: "session event",
                    detail: error.to_string(),
                })
            })?)
            .map_err(crate::model::ModelError::from)
            .map_err(EventValidationError::from)?;
        let retained_json_bytes = original_data.encoded_len();
        Ok(PreparedEvent {
            event,
            original_data,
            retained_json_bytes,
        })
    }

    /// Measure the exact compact payload bytes charged by one candidate event.
    pub(crate) fn event_retained_json_bytes(event: &NewEvent) -> Result<usize, AppendError> {
        Self::prepare_event(event.clone()).map(|prepared| prepared.retained_json_bytes)
    }

    fn durable_row_upper_bound(prepared: &PreparedEvent) -> Result<u64, AppendError> {
        prepared_event_line_upper_bound(prepared).map_err(|_| AppendError::DurableRecord)
    }

    fn durable_candidate_fits(
        &self,
        row_bytes: u64,
        protected_events: u64,
        protected_row_bytes: u64,
    ) -> bool {
        let SessionMode::Durable {
            logical_event_count,
            accepted_journal_bytes,
            ..
        } = &self.mode
        else {
            return false;
        };
        let ordinary_event_max = MAX_DURABLE_LOGICAL_EVENTS - DURABLE_REPAIR_RESERVED_EVENTS;
        let ordinary_byte_max = MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES;
        logical_event_count
            .checked_add(1)
            .and_then(|count| count.checked_add(protected_events))
            .is_some_and(|count| count <= ordinary_event_max)
            && accepted_journal_bytes
                .checked_add(row_bytes)
                .and_then(|bytes| bytes.checked_add(protected_row_bytes))
                .is_some_and(|bytes| bytes <= ordinary_byte_max)
    }

    fn append_prepared(
        &mut self,
        prepared: PreparedEvent,
        reserved_events: usize,
        reserved_retained_json_bytes: usize,
    ) -> Result<AppendReceipt, AppendError> {
        let (committed_events, retained_json_bytes) = match &self.mode {
            SessionMode::Memory {
                events,
                retained_json_bytes,
            } => (events.len(), *retained_json_bytes),
            SessionMode::Durable { .. } => return Err(AppendError::DurableAsyncRequired),
        };
        let event_count = committed_events
            .checked_add(1)
            .and_then(|value| value.checked_add(reserved_events))
            .ok_or(AppendError::EventLimit {
                maximum: MAX_SESSION_EVENTS,
            })?;
        if event_count > MAX_SESSION_EVENTS {
            return Err(if reserved_events == 0 {
                AppendError::EventLimit {
                    maximum: MAX_SESSION_EVENTS,
                }
            } else {
                AppendError::ReservedEventLimit {
                    maximum: MAX_SESSION_EVENTS,
                    reserved: reserved_events,
                }
            });
        }
        let next_retained_json_bytes = retained_json_bytes
            .checked_add(prepared.retained_json_bytes)
            .ok_or(AppendError::RetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
            })?;
        let retained_with_reservations = next_retained_json_bytes
            .checked_add(reserved_retained_json_bytes)
            .ok_or(AppendError::RetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
            })?;
        if retained_with_reservations > MAX_SESSION_RETAINED_JSON_BYTES {
            return Err(if reserved_retained_json_bytes == 0 {
                AppendError::RetainedJsonLimit {
                    maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                }
            } else {
                AppendError::ReservedRetainedJsonLimit {
                    maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                    reserved: reserved_retained_json_bytes,
                }
            });
        }
        let seq = self.next_seq.ok_or(AppendError::SequenceExhausted)?;
        let placeholder_time = UnixMillis::new(0).map_err(|_| AppendError::SequenceExhausted)?;
        let mut candidate = SessionEvent::from_new(
            seq,
            placeholder_time,
            prepared.event,
            prepared.original_data,
        );
        let event_type = candidate
            .kind()
            .live_event_type()
            .ok_or(EventValidationError::UnknownLiveEvent)?;
        let next_projection = self.projection.with_event(&candidate)?;
        let committed_message = match candidate.kind() {
            EventKind::UserMessage { message }
            | EventKind::AssistantMessage { message, .. }
            | EventKind::ToolResult { message, .. } => Some(message.clone()),
            _ => None,
        };
        match &mut self.mode {
            SessionMode::Memory {
                events,
                retained_json_bytes: _,
            } => {
                events
                    .try_reserve(1_usize.saturating_add(reserved_events))
                    .map_err(|_| AppendError::Capacity)?;
            }
            SessionMode::Durable { .. } => return Err(AppendError::DurableAsyncRequired),
        }
        let time = self.clock.now()?;
        if time.get() < 0 {
            return Err(ClockError::new("live event clock returned a negative timestamp").into());
        }
        candidate.set_time_for_commit(time);
        match &mut self.mode {
            SessionMode::Memory {
                events,
                retained_json_bytes,
            } => {
                events.push(candidate);
                *retained_json_bytes = next_retained_json_bytes;
            }
            SessionMode::Durable { .. } => return Err(AppendError::DurableAsyncRequired),
        }
        self.next_seq = seq
            .get()
            .checked_add(1)
            .and_then(|next| EventSeq::new(next).ok());
        self.projection = next_projection;
        let committed = match &self.mode {
            SessionMode::Memory { events, .. } => events.last().ok_or(AppendError::Capacity)?,
            SessionMode::Durable { .. } => return Err(AppendError::DurableAsyncRequired),
        };
        let observer_faulted = observer::publish_committed(&mut self.ui_observer, committed);
        self.ui_observer_faulted |= observer_faulted;
        Ok(AppendReceipt {
            seq,
            time,
            event_type,
            observer_faulted: self.ui_observer_faulted,
            committed_message,
        })
    }

    /// Attach the CLI's single live view before any event is committed.
    pub(crate) fn attach_ui_observer(
        &mut self,
    ) -> Result<CommittedUiReceiver, UiObserverAttachError> {
        self.attach_ui_observer_with_capacity(MAX_SESSION_EVENTS)
    }

    fn attach_ui_observer_with_capacity(
        &mut self,
        capacity: usize,
    ) -> Result<CommittedUiReceiver, UiObserverAttachError> {
        if self.ui_observer_attached {
            return Err(UiObserverAttachError::AlreadyAttached);
        }
        if self.observer_attach_at != self.next_seq {
            return Err(UiObserverAttachError::NotFresh);
        }
        let (sender, receiver) = observer::channel(capacity);
        self.ui_observer = Some(sender);
        self.ui_observer_attached = true;
        self.observer_attach_at = None;
        Ok(receiver)
    }

    #[cfg(test)]
    pub(crate) fn attach_ui_observer_for_test(
        &mut self,
        capacity: usize,
    ) -> Result<CommittedUiReceiver, UiObserverAttachError> {
        self.attach_ui_observer_with_capacity(capacity)
    }

    fn validate_prepared(&self, prepared: &PreparedEvent) -> Result<(), AppendError> {
        let seq = self.next_seq.ok_or(AppendError::SequenceExhausted)?;
        let time = UnixMillis::new(0).map_err(|_| AppendError::SequenceExhausted)?;
        let candidate = SessionEvent::from_new(
            seq,
            time,
            prepared.event.clone(),
            prepared.original_data.clone(),
        );
        self.projection.with_event(&candidate)?;
        Ok(())
    }

    /// Remaining raw in-memory limits before a reservation protects closures.
    #[must_use]
    pub fn remaining_budget(&self) -> SessionBudget {
        let (events, retained_json_bytes) = match &self.mode {
            SessionMode::Memory {
                events,
                retained_json_bytes,
            } => (events.len(), *retained_json_bytes),
            SessionMode::Durable { .. } => (0, 0),
        };
        SessionBudget {
            remaining_events: MAX_SESSION_EVENTS.saturating_sub(events),
            remaining_retained_json_bytes: MAX_SESSION_RETAINED_JSON_BYTES
                .saturating_sub(retained_json_bytes),
        }
    }

    /// Start one exclusive scope whose concrete fallback events cannot be displaced.
    pub fn reservation(&mut self) -> SessionReservation<'_> {
        SessionReservation {
            session: self,
            owner: Arc::new(()),
            reserved_events: 0,
            reserved_retained_json_bytes: 0,
            reserved_row_bytes: 0,
            next_claim_token: 0,
        }
    }

    /// Rebuild state and model messages from an event prefix without adding a seed marker.
    pub fn replay(events: &[SessionEvent]) -> Result<ReplayProjection, ReplayError> {
        let projection = replay_projection(events)?;
        Ok(ReplayProjection {
            state: projection.state(),
            messages: projection.messages(),
        })
    }

    /// Encode the current in-memory header and event array deterministically.
    pub fn to_json(&self) -> Result<String, CodecError> {
        match &self.mode {
            SessionMode::Memory { events, .. } => codec::encode_snapshot(&self.header, events),
            SessionMode::Durable { .. } => Err(CodecError::DurableSnapshotUnavailable),
        }
    }

    /// Immutable durable metadata.
    #[must_use]
    pub fn header(&self) -> &SessionHeader {
        &self.header
    }

    /// Stable session identity derived from the header.
    #[must_use]
    pub fn id(&self) -> &SessionId {
        self.header.id()
    }

    /// Immutable committed event prefix.
    #[must_use]
    pub fn events(&self) -> &[SessionEvent] {
        match &self.mode {
            SessionMode::Memory { events, .. } => events,
            SessionMode::Durable { .. } => &[],
        }
    }

    /// The next globally continuous durable sequence number.
    #[must_use]
    pub fn next_seq(&self) -> Option<EventSeq> {
        self.next_seq
    }

    /// Number of logical events committed in this session lifecycle.
    #[must_use]
    pub fn logical_event_count(&self) -> u64 {
        match &self.mode {
            SessionMode::Memory { events, .. } => u64::try_from(events.len()).unwrap_or(u64::MAX),
            SessionMode::Durable {
                logical_event_count,
                ..
            } => *logical_event_count,
        }
    }

    /// Whether this Session is backed by the append-only durable journal.
    pub(crate) fn is_durable(&self) -> bool {
        matches!(self.mode, SessionMode::Durable { .. })
    }

    #[cfg(test)]
    pub(crate) fn set_durable_event_room_for_test(&mut self, remaining_events: u64) {
        let ordinary_max = MAX_DURABLE_LOGICAL_EVENTS - DURABLE_REPAIR_RESERVED_EVENTS;
        assert!(remaining_events <= ordinary_max);
        let SessionMode::Durable {
            storage: SessionStorage::Active(_),
            logical_event_count,
            pending_batch,
            pending_operation,
            ..
        } = &mut self.mode
        else {
            panic!("the quota test seam requires an idle active durable session");
        };
        assert!(pending_batch.bytes.is_empty());
        assert_eq!(pending_batch.event_count, 0);
        assert!(pending_operation.is_none());
        *logical_event_count = ordinary_max - remaining_events;
    }

    #[cfg(test)]
    pub(crate) fn set_durable_byte_room_for_test(&mut self, remaining_bytes: u64) {
        let ordinary_max = MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES;
        assert!(remaining_bytes <= ordinary_max);
        let SessionMode::Durable {
            storage: SessionStorage::Active(_),
            accepted_journal_bytes,
            pending_batch,
            pending_operation,
            ..
        } = &mut self.mode
        else {
            panic!("the quota test seam requires an idle active durable session");
        };
        assert!(pending_batch.bytes.is_empty());
        assert_eq!(pending_batch.event_count, 0);
        assert!(pending_operation.is_none());
        *accepted_journal_bytes = ordinary_max - remaining_bytes;
    }

    /// Number of events supplied through construction before this lifecycle began.
    #[must_use]
    pub fn first_live_seq(&self) -> usize {
        self.first_live_seq
    }

    /// Detached read-only projection of the current boundary and surface state.
    #[must_use]
    pub fn state(&self) -> SessionState {
        self.projection.state()
    }

    /// Fresh snapshot of the exact messages visible to the next model request.
    #[must_use]
    pub fn messages(&self) -> Vec<Message> {
        self.projection.messages()
    }

    /// Latest canonical model-request header, or `None` before one is logged.
    #[must_use]
    pub fn request_header(&self) -> Option<&EpochHeader> {
        self.projection.request_header()
    }

    /// Latest complete route-capacity record, or `None` before one is logged.
    #[must_use]
    pub fn request_context(&self) -> Option<&RequestContext> {
        self.projection.request_context()
    }

    /// Whether the current model-visible surface has an assistant tool call
    /// without a matching tool result.
    #[must_use]
    pub(crate) fn has_unresolved_surface_tool_calls(&self) -> bool {
        self.projection.has_unresolved_surface_tool_calls()
    }
}

impl SessionReservation<'_> {
    /// Atomically protect the exact payload cost of every supplied fallback event.
    pub fn claim_batch(
        &mut self,
        fallbacks: impl IntoIterator<Item = NewEvent>,
    ) -> Result<Vec<EventClaim>, AppendError> {
        if matches!(&self.session.mode, SessionMode::Durable { .. }) {
            self.session.ensure_durable_active()?;
            if self.session.has_pending_durable_operation() {
                return Err(AppendError::NeedsAppendSettle);
            }
        }
        let prepared = fallbacks
            .into_iter()
            .map(Session::prepare_event)
            .collect::<Result<Vec<_>, _>>()?;
        let added_events = prepared.len();
        let added_claim_tokens =
            u64::try_from(added_events).map_err(|_| AppendError::SequenceExhausted)?;
        let first_claim_token = self.next_claim_token;
        let next_claim_token = first_claim_token
            .checked_add(added_claim_tokens)
            .ok_or(AppendError::SequenceExhausted)?;
        let added_bytes = prepared
            .iter()
            .try_fold(0_usize, |total, event| {
                total.checked_add(event.retained_json_bytes)
            })
            .ok_or(AppendError::RetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
            })?;
        let next_reserved_events =
            self.reserved_events
                .checked_add(added_events)
                .ok_or(AppendError::EventLimit {
                    maximum: MAX_SESSION_EVENTS,
                })?;
        let next_reserved_bytes = self
            .reserved_retained_json_bytes
            .checked_add(added_bytes)
            .ok_or(AppendError::RetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
            })?;
        let mut row_bytes = Vec::new();
        row_bytes
            .try_reserve(added_events)
            .map_err(|_| AppendError::Capacity)?;
        let mut added_row_bytes = 0_u64;
        if matches!(&self.session.mode, SessionMode::Durable { .. }) {
            self.session.ensure_durable_active()?;
            if self.session.has_pending_durable_operation() {
                return Err(AppendError::NeedsAppendSettle);
            }
            for fallback in &prepared {
                let row = Session::durable_row_upper_bound(fallback)?;
                added_row_bytes =
                    added_row_bytes
                        .checked_add(row)
                        .ok_or(AppendError::DurableByteLimit {
                            maximum: MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES,
                        })?;
                row_bytes.push(row);
            }
        } else {
            row_bytes.resize(added_events, 0);
        }
        let next_reserved_row_bytes = self.reserved_row_bytes.checked_add(added_row_bytes).ok_or(
            AppendError::DurableByteLimit {
                maximum: MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES,
            },
        )?;

        match &mut self.session.mode {
            SessionMode::Memory {
                events,
                retained_json_bytes,
                ..
            } => {
                if events
                    .len()
                    .checked_add(next_reserved_events)
                    .is_none_or(|value| value > MAX_SESSION_EVENTS)
                {
                    return Err(AppendError::ReservedEventLimit {
                        maximum: MAX_SESSION_EVENTS,
                        reserved: next_reserved_events,
                    });
                }
                if retained_json_bytes
                    .checked_add(next_reserved_bytes)
                    .is_none_or(|value| value > MAX_SESSION_RETAINED_JSON_BYTES)
                {
                    return Err(AppendError::ReservedRetainedJsonLimit {
                        maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                        reserved: next_reserved_bytes,
                    });
                }
                events
                    .try_reserve(next_reserved_events)
                    .map_err(|_| AppendError::Capacity)?;
            }
            SessionMode::Durable {
                logical_event_count,
                accepted_journal_bytes,
                ..
            } => {
                let reserved_events = u64::try_from(next_reserved_events)
                    .map_err(|_| AppendError::SequenceExhausted)?;
                let ordinary_event_max =
                    MAX_DURABLE_LOGICAL_EVENTS - DURABLE_REPAIR_RESERVED_EVENTS;
                if logical_event_count
                    .checked_add(reserved_events)
                    .is_none_or(|value| value > ordinary_event_max)
                {
                    return Err(AppendError::DurableEventLimit {
                        maximum: ordinary_event_max,
                    });
                }
                let ordinary_byte_max = MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES;
                if accepted_journal_bytes
                    .checked_add(next_reserved_row_bytes)
                    .is_none_or(|value| value > ordinary_byte_max)
                {
                    return Err(AppendError::DurableByteLimit {
                        maximum: ordinary_byte_max,
                    });
                }
            }
        }

        let mut claims = Vec::new();
        claims
            .try_reserve(added_events)
            .map_err(|_| AppendError::Capacity)?;
        for (index, (fallback, reserved_row_bytes)) in
            prepared.into_iter().zip(row_bytes).enumerate()
        {
            let token = first_claim_token
                .checked_add(u64::try_from(index).map_err(|_| AppendError::SequenceExhausted)?)
                .ok_or(AppendError::SequenceExhausted)?;
            claims.push(EventClaim {
                owner: self.owner.clone(),
                token,
                reserved_retained_json_bytes: fallback.retained_json_bytes,
                reserved_row_bytes,
                fallback,
                settled: false,
            });
        }
        self.reserved_events = next_reserved_events;
        self.reserved_retained_json_bytes = next_reserved_bytes;
        self.reserved_row_bytes = next_reserved_row_bytes;
        self.next_claim_token = next_claim_token;
        Ok(claims)
    }

    /// Append an ordinary event without invading any active fallback claim.
    pub fn append(&mut self, event: NewEvent) -> Result<AppendReceipt, AppendError> {
        self.session.ensure_memory_append()?;
        let prepared = Session::prepare_event(event)?;
        self.session.append_prepared(
            prepared,
            self.reserved_events,
            self.reserved_retained_json_bytes,
        )
    }

    /// Append through the durable owner without consuming any protected claim.
    pub(crate) async fn append_settled(
        &mut self,
        event: NewEvent,
    ) -> Result<AppendReceipt, AppendError> {
        if matches!(&self.session.mode, SessionMode::Memory { .. }) {
            return self.append(event);
        }
        self.session.ensure_durable_active()?;
        let prepared = Session::prepare_event(event)?;
        let protected_events =
            u64::try_from(self.reserved_events).map_err(|_| AppendError::SequenceExhausted)?;
        self.session
            .append_prepared_settled(prepared, protected_events, self.reserved_row_bytes)
            .await
    }

    /// Commit a preferred event when it fits, otherwise commit the protected fallback.
    pub fn settle(
        &mut self,
        claim: &mut EventClaim,
        preferred: NewEvent,
    ) -> Result<ClaimedAppend, AppendError> {
        self.session.ensure_memory_append()?;
        self.validate_claim(claim)?;
        let preferred = Session::prepare_event(preferred)?;
        self.session.validate_prepared(&preferred)?;
        let other_events = self.reserved_events - 1;
        let other_bytes = self.reserved_retained_json_bytes - claim.reserved_retained_json_bytes;
        let preferred_fits = self
            .session
            .events()
            .len()
            .checked_add(1)
            .and_then(|value| value.checked_add(other_events))
            .is_some_and(|value| value <= MAX_SESSION_EVENTS)
            && match &self.session.mode {
                SessionMode::Memory {
                    retained_json_bytes,
                    ..
                } => *retained_json_bytes,
                SessionMode::Durable { .. } => return Err(AppendError::DurableAsyncRequired),
            }
            .checked_add(preferred.retained_json_bytes)
            .and_then(|value| value.checked_add(other_bytes))
            .is_some_and(|value| value <= MAX_SESSION_RETAINED_JSON_BYTES);
        let (selected, fallback) = if preferred_fits {
            (preferred, false)
        } else {
            (claim.fallback.clone(), true)
        };
        let receipt = self
            .session
            .append_prepared(selected, other_events, other_bytes)?;
        claim.settled = true;
        self.reserved_events = other_events;
        self.reserved_retained_json_bytes = other_bytes;
        self.reserved_row_bytes = self
            .reserved_row_bytes
            .checked_sub(claim.reserved_row_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        Ok(if fallback {
            ClaimedAppend::Fallback(receipt)
        } else {
            ClaimedAppend::Preferred(receipt)
        })
    }

    /// Durable counterpart of `settle`; the claim stays protected across I/O waits.
    pub(crate) async fn settle_settled(
        &mut self,
        claim: &mut EventClaim,
        preferred: NewEvent,
    ) -> Result<ClaimedAppend, AppendError> {
        if matches!(&self.session.mode, SessionMode::Memory { .. }) {
            return self.settle(claim, preferred);
        }
        self.session.ensure_durable_active()?;
        self.validate_claim(claim)?;
        if let Some(kind) = self
            .session
            .pending_claim_operation(&self.owner, claim.token)?
        {
            if !matches!(
                kind,
                ClaimOperationKind::Preferred | ClaimOperationKind::Fallback
            ) {
                return Err(AppendError::InvalidClaim);
            }
            let receipt = self
                .session
                .settle_pending_append()
                .await?
                .ok_or(AppendError::DurableWriter)?;
            self.finish_claim_bookkeeping(claim)?;
            return Ok(if kind == ClaimOperationKind::Fallback {
                ClaimedAppend::Fallback(receipt)
            } else {
                ClaimedAppend::Preferred(receipt)
            });
        }
        let preferred = Session::prepare_event(preferred)?;
        self.session.validate_prepared(&preferred)?;
        let other_events = self
            .reserved_events
            .checked_sub(1)
            .ok_or(AppendError::InvalidClaim)?;
        let other_row_bytes = self
            .reserved_row_bytes
            .checked_sub(claim.reserved_row_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        // A fallback claim protects enough space to close the operation even
        // when the rest of the journal becomes full. Like memory mode, a
        // read-only preferred result may still use otherwise unclaimed global
        // space; only preferred-only settlement after an irreversible side
        // effect must fit inside the claim's own pre-reserved ceiling.
        let protected_events =
            u64::try_from(other_events).map_err(|_| AppendError::SequenceExhausted)?;
        let preferred_fits = Session::durable_row_upper_bound(&preferred).is_ok_and(|row_bytes| {
            self.session
                .durable_candidate_fits(row_bytes, protected_events, other_row_bytes)
        });
        let (selected, kind) = if preferred_fits {
            (preferred, ClaimOperationKind::Preferred)
        } else {
            (claim.fallback.clone(), ClaimOperationKind::Fallback)
        };
        let receipt = self
            .session
            .append_claim_prepared_settled(
                selected,
                protected_events,
                other_row_bytes,
                self.owner.clone(),
                claim.token,
                kind,
            )
            .await?;
        self.finish_claim_bookkeeping(claim)?;
        Ok(if kind == ClaimOperationKind::Fallback {
            ClaimedAppend::Fallback(receipt)
        } else {
            ClaimedAppend::Preferred(receipt)
        })
    }

    /// Commit the exact fallback template protected by a claim.
    pub fn settle_exact(&mut self, claim: &mut EventClaim) -> Result<AppendReceipt, AppendError> {
        self.session.ensure_memory_append()?;
        self.validate_claim(claim)?;
        let other_events = self.reserved_events - 1;
        let other_bytes = self.reserved_retained_json_bytes - claim.reserved_retained_json_bytes;
        let receipt =
            self.session
                .append_prepared(claim.fallback.clone(), other_events, other_bytes)?;
        claim.settled = true;
        self.reserved_events = other_events;
        self.reserved_retained_json_bytes = other_bytes;
        self.reserved_row_bytes = self
            .reserved_row_bytes
            .checked_sub(claim.reserved_row_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        Ok(receipt)
    }

    /// Durable counterpart of `settle_exact`.
    pub(crate) async fn settle_exact_settled(
        &mut self,
        claim: &mut EventClaim,
    ) -> Result<AppendReceipt, AppendError> {
        if matches!(&self.session.mode, SessionMode::Memory { .. }) {
            return self.settle_exact(claim);
        }
        self.session.ensure_durable_active()?;
        self.validate_claim(claim)?;
        if let Some(kind) = self
            .session
            .pending_claim_operation(&self.owner, claim.token)?
        {
            if kind != ClaimOperationKind::Exact {
                return Err(AppendError::InvalidClaim);
            }
            let receipt = self
                .session
                .settle_pending_append()
                .await?
                .ok_or(AppendError::DurableWriter)?;
            self.finish_claim_bookkeeping(claim)?;
            return Ok(receipt);
        }
        let other_events = self
            .reserved_events
            .checked_sub(1)
            .ok_or(AppendError::InvalidClaim)?;
        let other_row_bytes = self
            .reserved_row_bytes
            .checked_sub(claim.reserved_row_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        let protected_events =
            u64::try_from(other_events).map_err(|_| AppendError::SequenceExhausted)?;
        let receipt = self
            .session
            .append_claim_prepared_settled(
                claim.fallback.clone(),
                protected_events,
                other_row_bytes,
                self.owner.clone(),
                claim.token,
                ClaimOperationKind::Exact,
            )
            .await?;
        self.finish_claim_bookkeeping(claim)?;
        Ok(receipt)
    }

    /// Read the exact committed session while retaining exclusive append ownership.
    #[must_use]
    pub fn session(&self) -> &Session {
        self.session
    }

    /// Synchronize all committed facts while retaining the reservation and
    /// its protected closure capacity.
    pub(crate) async fn flush_barrier(&mut self) -> Result<(), BarrierError> {
        self.session.flush_barrier().await
    }

    /// Explicitly stop protecting a claim that the caller will never publish.
    pub fn release(&mut self, claim: &mut EventClaim) -> Result<(), AppendError> {
        self.validate_claim(claim)?;
        if matches!(&self.session.mode, SessionMode::Durable { .. }) {
            self.session.ensure_durable_active()?;
            if self.session.has_pending_durable_operation() {
                return Err(AppendError::NeedsAppendSettle);
            }
        }
        self.reserved_events -= 1;
        self.reserved_retained_json_bytes -= claim.reserved_retained_json_bytes;
        self.reserved_row_bytes -= claim.reserved_row_bytes;
        claim.settled = true;
        Ok(())
    }

    /// Increase one active claim's protected byte ceiling without changing its
    /// fallback. This is used when a read-only preparation stage discovers the
    /// exact maximum size of a possible committed result.
    pub(crate) fn reserve_claim_retained_json_bytes(
        &mut self,
        claim: &mut EventClaim,
        requested: usize,
    ) -> Result<(), AppendError> {
        self.validate_claim(claim)?;
        if matches!(&self.session.mode, SessionMode::Durable { .. }) {
            self.session.ensure_durable_active()?;
            if self.session.has_pending_durable_operation() {
                return Err(AppendError::NeedsAppendSettle);
            }
        }
        if requested <= claim.reserved_retained_json_bytes {
            return Ok(());
        }
        let other_bytes = self
            .reserved_retained_json_bytes
            .checked_sub(claim.reserved_retained_json_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        let next_reserved_bytes =
            other_bytes
                .checked_add(requested)
                .ok_or(AppendError::ReservedRetainedJsonLimit {
                    maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                    reserved: usize::MAX,
                })?;
        match &self.session.mode {
            SessionMode::Memory {
                retained_json_bytes,
                ..
            } => {
                if retained_json_bytes
                    .checked_add(next_reserved_bytes)
                    .is_none_or(|total| total > MAX_SESSION_RETAINED_JSON_BYTES)
                {
                    return Err(AppendError::ReservedRetainedJsonLimit {
                        maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                        reserved: next_reserved_bytes,
                    });
                }
            }
            SessionMode::Durable {
                accepted_journal_bytes,
                ..
            } => {
                self.session.ensure_durable_active()?;
                if self.session.has_pending_durable_operation() {
                    return Err(AppendError::NeedsAppendSettle);
                }
                let added_payload = requested
                    .checked_sub(claim.reserved_retained_json_bytes)
                    .ok_or(AppendError::InvalidClaim)?;
                let added_payload =
                    u64::try_from(added_payload).map_err(|_| AppendError::DurableRecord)?;
                let next_claim_row = claim.reserved_row_bytes.checked_add(added_payload).ok_or(
                    AppendError::DurableByteLimit {
                        maximum: MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES,
                    },
                )?;
                let record_limit = u64::try_from(MAX_JOURNAL_EVENT_LINE_BYTES)
                    .map_err(|_| AppendError::DurableRecord)?;
                if next_claim_row > record_limit {
                    return Err(AppendError::DurableRecord);
                }
                let other_rows = self
                    .reserved_row_bytes
                    .checked_sub(claim.reserved_row_bytes)
                    .ok_or(AppendError::InvalidClaim)?;
                let next_reserved_rows = other_rows.checked_add(next_claim_row).ok_or(
                    AppendError::DurableByteLimit {
                        maximum: MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES,
                    },
                )?;
                let ordinary_max = MAX_DURABLE_JOURNAL_BYTES - DURABLE_REPAIR_RESERVED_BYTES;
                if accepted_journal_bytes
                    .checked_add(next_reserved_rows)
                    .is_none_or(|total| total > ordinary_max)
                {
                    return Err(AppendError::DurableByteLimit {
                        maximum: ordinary_max,
                    });
                }
                claim.reserved_row_bytes = next_claim_row;
                self.reserved_row_bytes = next_reserved_rows;
            }
        }
        claim.reserved_retained_json_bytes = requested;
        self.reserved_retained_json_bytes = next_reserved_bytes;
        Ok(())
    }

    /// Replace an active claim's exact fallback while staying within its
    /// already-protected byte ceiling. No event, projection, sequence, or clock
    /// state changes until the claim is later settled.
    pub(crate) fn rebind_claim_fallback(
        &mut self,
        claim: &mut EventClaim,
        fallback: NewEvent,
    ) -> Result<(), AppendError> {
        self.validate_claim(claim)?;
        if matches!(&self.session.mode, SessionMode::Durable { .. }) {
            self.session.ensure_durable_active()?;
            if self.session.has_pending_durable_operation() {
                return Err(AppendError::NeedsAppendSettle);
            }
        }
        let fallback = Session::prepare_event(fallback)?;
        if fallback.retained_json_bytes > claim.reserved_retained_json_bytes {
            return Err(AppendError::ClaimPayloadTooLarge {
                reserved: claim.reserved_retained_json_bytes,
                actual: fallback.retained_json_bytes,
            });
        }
        if matches!(&self.session.mode, SessionMode::Durable { .. }) {
            self.session.ensure_durable_active()?;
            let actual = Session::durable_row_upper_bound(&fallback)?;
            if actual > claim.reserved_row_bytes {
                return Err(AppendError::ClaimRowTooLarge {
                    reserved: claim.reserved_row_bytes,
                    actual,
                });
            }
        }
        claim.fallback = fallback;
        Ok(())
    }

    /// Commit exactly the supplied event from a claim's protected capacity.
    /// Unlike `settle`, this never substitutes the fallback, which is required
    /// after an irreversible external side effect has already committed.
    pub(crate) fn settle_preferred_only(
        &mut self,
        claim: &mut EventClaim,
        preferred: NewEvent,
    ) -> Result<AppendReceipt, AppendError> {
        self.session.ensure_memory_append()?;
        self.validate_claim(claim)?;
        let preferred = Session::prepare_event(preferred)?;
        if preferred.retained_json_bytes > claim.reserved_retained_json_bytes {
            return Err(AppendError::ClaimPayloadTooLarge {
                reserved: claim.reserved_retained_json_bytes,
                actual: preferred.retained_json_bytes,
            });
        }
        self.session.validate_prepared(&preferred)?;
        let other_events = self.reserved_events - 1;
        let other_bytes = self.reserved_retained_json_bytes - claim.reserved_retained_json_bytes;
        let receipt = self
            .session
            .append_prepared(preferred, other_events, other_bytes)?;
        claim.settled = true;
        self.reserved_events = other_events;
        self.reserved_retained_json_bytes = other_bytes;
        self.reserved_row_bytes = self
            .reserved_row_bytes
            .checked_sub(claim.reserved_row_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        Ok(receipt)
    }

    /// Durable counterpart of `settle_preferred_only`; it never substitutes
    /// the fallback after an irreversible external side effect.
    pub(crate) async fn settle_preferred_only_settled(
        &mut self,
        claim: &mut EventClaim,
        preferred: NewEvent,
    ) -> Result<AppendReceipt, AppendError> {
        if matches!(&self.session.mode, SessionMode::Memory { .. }) {
            return self.settle_preferred_only(claim, preferred);
        }
        self.session.ensure_durable_active()?;
        self.validate_claim(claim)?;
        if let Some(kind) = self
            .session
            .pending_claim_operation(&self.owner, claim.token)?
        {
            if kind != ClaimOperationKind::PreferredOnly {
                return Err(AppendError::InvalidClaim);
            }
            let receipt = self
                .session
                .settle_pending_append()
                .await?
                .ok_or(AppendError::DurableWriter)?;
            self.finish_claim_bookkeeping(claim)?;
            return Ok(receipt);
        }
        let preferred = Session::prepare_event(preferred)?;
        if preferred.retained_json_bytes > claim.reserved_retained_json_bytes {
            return Err(AppendError::ClaimPayloadTooLarge {
                reserved: claim.reserved_retained_json_bytes,
                actual: preferred.retained_json_bytes,
            });
        }
        self.session.validate_prepared(&preferred)?;
        let actual_row = Session::durable_row_upper_bound(&preferred)?;
        if actual_row > claim.reserved_row_bytes {
            return Err(AppendError::ClaimRowTooLarge {
                reserved: claim.reserved_row_bytes,
                actual: actual_row,
            });
        }
        let other_events = self
            .reserved_events
            .checked_sub(1)
            .ok_or(AppendError::InvalidClaim)?;
        let other_row_bytes = self
            .reserved_row_bytes
            .checked_sub(claim.reserved_row_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        let protected_events =
            u64::try_from(other_events).map_err(|_| AppendError::SequenceExhausted)?;
        let receipt = self
            .session
            .append_claim_prepared_settled(
                preferred,
                protected_events,
                other_row_bytes,
                self.owner.clone(),
                claim.token,
                ClaimOperationKind::PreferredOnly,
            )
            .await?;
        self.finish_claim_bookkeeping(claim)?;
        Ok(receipt)
    }

    fn finish_claim_bookkeeping(&mut self, claim: &mut EventClaim) -> Result<(), AppendError> {
        let next_events = self
            .reserved_events
            .checked_sub(1)
            .ok_or(AppendError::InvalidClaim)?;
        let next_retained_json_bytes = self
            .reserved_retained_json_bytes
            .checked_sub(claim.reserved_retained_json_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        let next_row_bytes = self
            .reserved_row_bytes
            .checked_sub(claim.reserved_row_bytes)
            .ok_or(AppendError::InvalidClaim)?;
        self.reserved_events = next_events;
        self.reserved_retained_json_bytes = next_retained_json_bytes;
        self.reserved_row_bytes = next_row_bytes;
        claim.settled = true;
        Ok(())
    }

    fn validate_claim(&self, claim: &EventClaim) -> Result<(), AppendError> {
        if claim.settled || !Arc::ptr_eq(&self.owner, &claim.owner) {
            return Err(AppendError::InvalidClaim);
        }
        Ok(())
    }
}

fn retained_json_bytes(
    header: &SessionHeader,
    events: &[SessionEvent],
) -> Result<usize, SessionError> {
    events
        .iter()
        .try_fold(header.raw().encoded_len(), |total, event| {
            total
                .checked_add(event.data().encoded_len())
                .filter(|next| *next <= MAX_SESSION_RETAINED_JSON_BYTES)
                .ok_or(SessionError::RetainedJsonLimit {
                    maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                })
        })
}

fn map_journal_append_error(error: JournalError) -> AppendError {
    match error {
        JournalError::Poisoned => AppendError::DurablePoisoned,
        JournalError::WriterStopped
        | JournalError::NothingStaged
        | JournalError::AlreadyStaged
        | JournalError::FlightInProgress => AppendError::DurableWriter,
    }
}

fn replay_projection(events: &[SessionEvent]) -> Result<Projection, ReplayError> {
    if events.len() > MAX_SESSION_EVENTS {
        return Err(ReplayError {
            index: MAX_SESSION_EVENTS,
            source: EventValidationError::TooManyEvents {
                maximum: MAX_SESSION_EVENTS,
                actual: events.len(),
            },
        });
    }
    let mut projection = Projection::empty(ValidationPolicy::MemoryCompatible);
    for (index, event) in events.iter().enumerate() {
        let Some(expected) = EventSeq::from_index(index) else {
            return Err(ReplayError {
                index,
                source: EventValidationError::NonContiguousSequence {
                    expected: event.seq,
                    actual: event.seq,
                },
            });
        };
        if event.seq != expected {
            return Err(ReplayError {
                index,
                source: EventValidationError::NonContiguousSequence {
                    expected,
                    actual: event.seq,
                },
            });
        }
        if matches!(event.kind, EventKind::Unknown { .. }) && event.ignorable.is_none() {
            return Err(ReplayError {
                index,
                source: EventValidationError::UnknownRequiredEvent,
            });
        }
        projection = projection
            .with_event(event)
            .map_err(|source| ReplayError { index, source })?;
    }
    Ok(projection)
}
