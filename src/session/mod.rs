//! Append-only session log and deterministic projections.

mod clock;
mod codec;
mod error;
mod event;
#[cfg(test)]
mod phase5_tests;
mod projection;

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
    TOOL_NOT_STARTED, TodoItem, TodoStatus, ToolFailure, TurnEndCancelCause, TurnEndReason, TurnId,
    UnixMillis,
};
pub use projection::SessionState;

use crate::model::{JsonValue, Message};

use self::{codec::decode_snapshot, projection::Projection};

#[derive(Clone, Debug)]
struct PreparedEvent {
    event: NewEvent,
    original_data: JsonValue,
    retained_json_bytes: usize,
}

/// Capacity still available before taking any active reservation into account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionBudget {
    pub remaining_events: usize,
    pub remaining_retained_json_bytes: usize,
}

/// Result of fulfilling one exact fallback claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaimedAppend {
    Preferred(EventSeq),
    Fallback(EventSeq),
}

/// One concrete event whose exact payload cost is protected by a reservation.
#[derive(Debug)]
pub struct EventClaim {
    owner: Arc<()>,
    fallback: PreparedEvent,
    reserved_retained_json_bytes: usize,
    settled: bool,
}

/// Exclusive append view that prevents ordinary events from consuming claims.
pub struct SessionReservation<'a> {
    session: &'a mut Session,
    owner: Arc<()>,
    reserved_events: usize,
    reserved_retained_json_bytes: usize,
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
    events: Vec<SessionEvent>,
    projection: Projection,
    first_live_seq: usize,
    retained_json_bytes: usize,
    clock: Box<dyn Clock>,
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
        Ok(Self {
            retained_json_bytes: header.raw().encoded_len(),
            header,
            events: Vec::new(),
            projection: Projection::empty(),
            first_live_seq: 0,
            clock: Box::new(clock),
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
            events: seed.to_vec(),
            projection,
            first_live_seq,
            retained_json_bytes,
            clock: Box::new(clock),
        };
        if !matches!(
            session.events.last().map(SessionEvent::kind),
            Some(EventKind::EndSeed)
        ) {
            session.append(NewEvent::log(EventKind::EndSeed))?;
        }
        Ok(session)
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
    pub fn append(&mut self, event: NewEvent) -> Result<&SessionEvent, AppendError> {
        let prepared = Self::prepare_event(event)?;
        self.append_prepared(prepared, 0, 0)
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

    fn append_prepared(
        &mut self,
        prepared: PreparedEvent,
        reserved_events: usize,
        reserved_retained_json_bytes: usize,
    ) -> Result<&SessionEvent, AppendError> {
        let event_count = self
            .events
            .len()
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
        let next_retained_json_bytes = self
            .retained_json_bytes
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
        let seq = EventSeq::from_index(self.events.len()).ok_or(AppendError::SequenceExhausted)?;
        let time = self.clock.now()?;
        if time.get() < 0 {
            return Err(ClockError::new("live event clock returned a negative timestamp").into());
        }
        let candidate = SessionEvent::from_new(seq, time, prepared.event, prepared.original_data);
        let next_projection = self.projection.with_event(&candidate, &self.events)?;
        self.events
            .try_reserve(1_usize.saturating_add(reserved_events))
            .map_err(|_| AppendError::Capacity)?;
        self.events.push(candidate);
        self.projection = next_projection;
        self.retained_json_bytes = next_retained_json_bytes;
        // The push above makes this index valid, and no fallible work occurs after it.
        let committed_index = self.events.len() - 1;
        Ok(&self.events[committed_index])
    }

    fn validate_prepared(&self, prepared: &PreparedEvent) -> Result<(), AppendError> {
        let seq = EventSeq::from_index(self.events.len()).ok_or(AppendError::SequenceExhausted)?;
        let time = UnixMillis::new(0).map_err(|_| AppendError::SequenceExhausted)?;
        let candidate = SessionEvent::from_new(
            seq,
            time,
            prepared.event.clone(),
            prepared.original_data.clone(),
        );
        self.projection.with_event(&candidate, &self.events)?;
        Ok(())
    }

    /// Remaining raw in-memory limits before a reservation protects closures.
    #[must_use]
    pub fn remaining_budget(&self) -> SessionBudget {
        SessionBudget {
            remaining_events: MAX_SESSION_EVENTS.saturating_sub(self.events.len()),
            remaining_retained_json_bytes: MAX_SESSION_RETAINED_JSON_BYTES
                .saturating_sub(self.retained_json_bytes),
        }
    }

    /// Start one exclusive scope whose concrete fallback events cannot be displaced.
    pub fn reservation(&mut self) -> SessionReservation<'_> {
        SessionReservation {
            session: self,
            owner: Arc::new(()),
            reserved_events: 0,
            reserved_retained_json_bytes: 0,
        }
    }

    /// Rebuild state and model messages from an event prefix without adding a seed marker.
    pub fn replay(events: &[SessionEvent]) -> Result<ReplayProjection, ReplayError> {
        let projection = replay_projection(events)?;
        Ok(ReplayProjection {
            state: projection.state(),
            messages: projection.messages(events),
        })
    }

    /// Encode the current in-memory header and event array deterministically.
    pub fn to_json(&self) -> Result<String, CodecError> {
        codec::encode_snapshot(&self.header, &self.events)
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
        &self.events
    }

    /// The next event sequence, equal to the current log length.
    #[must_use]
    pub fn next_seq(&self) -> Option<EventSeq> {
        EventSeq::from_index(self.events.len())
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
        self.projection.messages(&self.events)
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
}

impl SessionReservation<'_> {
    /// Atomically protect the exact payload cost of every supplied fallback event.
    pub fn claim_batch(
        &mut self,
        fallbacks: impl IntoIterator<Item = NewEvent>,
    ) -> Result<Vec<EventClaim>, AppendError> {
        let prepared = fallbacks
            .into_iter()
            .map(Session::prepare_event)
            .collect::<Result<Vec<_>, _>>()?;
        let added_events = prepared.len();
        let added_bytes = prepared.iter().try_fold(0_usize, |total, event| {
            total.checked_add(event.retained_json_bytes)
        });
        let next_reserved_events =
            self.reserved_events
                .checked_add(added_events)
                .ok_or(AppendError::EventLimit {
                    maximum: MAX_SESSION_EVENTS,
                })?;
        let next_reserved_bytes = self
            .reserved_retained_json_bytes
            .checked_add(added_bytes.ok_or(AppendError::RetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
            })?)
            .ok_or(AppendError::RetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
            })?;
        if self
            .session
            .events
            .len()
            .checked_add(next_reserved_events)
            .is_none_or(|value| value > MAX_SESSION_EVENTS)
        {
            return Err(AppendError::ReservedEventLimit {
                maximum: MAX_SESSION_EVENTS,
                reserved: next_reserved_events,
            });
        }
        if self
            .session
            .retained_json_bytes
            .checked_add(next_reserved_bytes)
            .is_none_or(|value| value > MAX_SESSION_RETAINED_JSON_BYTES)
        {
            return Err(AppendError::ReservedRetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                reserved: next_reserved_bytes,
            });
        }
        self.session
            .events
            .try_reserve(next_reserved_events)
            .map_err(|_| AppendError::Capacity)?;

        let mut claims = Vec::new();
        claims
            .try_reserve(added_events)
            .map_err(|_| AppendError::Capacity)?;
        for fallback in prepared {
            claims.push(EventClaim {
                owner: self.owner.clone(),
                reserved_retained_json_bytes: fallback.retained_json_bytes,
                fallback,
                settled: false,
            });
        }
        self.reserved_events = next_reserved_events;
        self.reserved_retained_json_bytes = next_reserved_bytes;
        Ok(claims)
    }

    /// Append an ordinary event without invading any active fallback claim.
    pub fn append(&mut self, event: NewEvent) -> Result<&SessionEvent, AppendError> {
        let prepared = Session::prepare_event(event)?;
        self.session.append_prepared(
            prepared,
            self.reserved_events,
            self.reserved_retained_json_bytes,
        )
    }

    /// Commit a preferred event when it fits, otherwise commit the protected fallback.
    pub fn settle(
        &mut self,
        claim: &mut EventClaim,
        preferred: NewEvent,
    ) -> Result<ClaimedAppend, AppendError> {
        self.validate_claim(claim)?;
        let preferred = Session::prepare_event(preferred)?;
        self.session.validate_prepared(&preferred)?;
        let other_events = self.reserved_events - 1;
        let other_bytes = self.reserved_retained_json_bytes - claim.reserved_retained_json_bytes;
        let preferred_fits = self
            .session
            .events
            .len()
            .checked_add(1)
            .and_then(|value| value.checked_add(other_events))
            .is_some_and(|value| value <= MAX_SESSION_EVENTS)
            && self
                .session
                .retained_json_bytes
                .checked_add(preferred.retained_json_bytes)
                .and_then(|value| value.checked_add(other_bytes))
                .is_some_and(|value| value <= MAX_SESSION_RETAINED_JSON_BYTES);
        let (selected, fallback) = if preferred_fits {
            (preferred, false)
        } else {
            (claim.fallback.clone(), true)
        };
        let seq = self
            .session
            .append_prepared(selected, other_events, other_bytes)?
            .seq();
        claim.settled = true;
        self.reserved_events = other_events;
        self.reserved_retained_json_bytes = other_bytes;
        Ok(if fallback {
            ClaimedAppend::Fallback(seq)
        } else {
            ClaimedAppend::Preferred(seq)
        })
    }

    /// Commit the exact fallback template protected by a claim.
    pub fn settle_exact(&mut self, claim: &mut EventClaim) -> Result<EventSeq, AppendError> {
        self.validate_claim(claim)?;
        let other_events = self.reserved_events - 1;
        let other_bytes = self.reserved_retained_json_bytes - claim.reserved_retained_json_bytes;
        let seq = self
            .session
            .append_prepared(claim.fallback.clone(), other_events, other_bytes)?
            .seq();
        claim.settled = true;
        self.reserved_events = other_events;
        self.reserved_retained_json_bytes = other_bytes;
        Ok(seq)
    }

    /// Read the exact committed session while retaining exclusive append ownership.
    #[must_use]
    pub fn session(&self) -> &Session {
        self.session
    }

    /// Explicitly stop protecting a claim that the caller will never publish.
    pub fn release(&mut self, claim: &mut EventClaim) -> Result<(), AppendError> {
        self.validate_claim(claim)?;
        self.reserved_events -= 1;
        self.reserved_retained_json_bytes -= claim.reserved_retained_json_bytes;
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
        if self
            .session
            .retained_json_bytes
            .checked_add(next_reserved_bytes)
            .is_none_or(|total| total > MAX_SESSION_RETAINED_JSON_BYTES)
        {
            return Err(AppendError::ReservedRetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
                reserved: next_reserved_bytes,
            });
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
        let fallback = Session::prepare_event(fallback)?;
        if fallback.retained_json_bytes > claim.reserved_retained_json_bytes {
            return Err(AppendError::ClaimPayloadTooLarge {
                reserved: claim.reserved_retained_json_bytes,
                actual: fallback.retained_json_bytes,
            });
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
    ) -> Result<EventSeq, AppendError> {
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
        let seq = self
            .session
            .append_prepared(preferred, other_events, other_bytes)?
            .seq();
        claim.settled = true;
        self.reserved_events = other_events;
        self.reserved_retained_json_bytes = other_bytes;
        Ok(seq)
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
    let mut projection = Projection::empty();
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
            .with_event(event, &events[..index])
            .map_err(|source| ReplayError { index, source })?;
    }
    Ok(projection)
}
