//! Append-only session log and deterministic projections.

mod clock;
mod codec;
mod error;
mod event;
mod projection;

pub use clock::{Clock, SystemClock};
pub use codec::{MAX_SESSION_EVENTS, MAX_SESSION_RETAINED_JSON_BYTES, MAX_SESSION_SNAPSHOT_BYTES};
pub use error::{
    AppendError, ClockError, CodecError, EventValidationError, HeaderError, NumberError,
    ReplayError, SessionError, SurfaceError, TransitionError,
};
pub use event::{
    EpochHeader, EventKind, EventSeq, MAX_SAFE_INTEGER, MAX_SESSION_HEADER_BYTES,
    MAX_SOURCE_EVENT_SEQS, NewEvent, RequestContext, RequestHeaderReason, SESSION_FORMAT_VERSION,
    SessionEvent, SessionHeader, SessionId, SessionOrigin, StepId, SurfaceAppend, SurfaceIntent,
    SurfaceOp, SurfaceReplace, TOOL_NOT_STARTED, TodoItem, TodoStatus, ToolFailure,
    TurnEndCancelCause, TurnEndReason, TurnId, UnixMillis,
};
pub use projection::SessionState;

use crate::model::{JsonValue, Message};

use self::{codec::decode_snapshot, projection::Projection};

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
        if self.events.len() >= MAX_SESSION_EVENTS {
            return Err(AppendError::EventLimit {
                maximum: MAX_SESSION_EVENTS,
            });
        }
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
        let next_retained_json_bytes = self
            .retained_json_bytes
            .checked_add(original_data.encoded_len())
            .filter(|total| *total <= MAX_SESSION_RETAINED_JSON_BYTES)
            .ok_or(AppendError::RetainedJsonLimit {
                maximum: MAX_SESSION_RETAINED_JSON_BYTES,
            })?;
        let seq = EventSeq::from_index(self.events.len()).ok_or(AppendError::SequenceExhausted)?;
        let time = self.clock.now()?;
        if time.get() < 0 {
            return Err(ClockError::new("live event clock returned a negative timestamp").into());
        }
        let candidate = SessionEvent::from_new(seq, time, event, original_data);
        let next_projection = self.projection.with_event(&candidate, &self.events)?;
        self.events
            .try_reserve(1)
            .map_err(|_| AppendError::Capacity)?;
        self.events.push(candidate);
        self.projection = next_projection;
        self.retained_json_bytes = next_retained_json_bytes;
        // The push above makes this index valid, and no fallible work occurs after it.
        let committed_index = self.events.len() - 1;
        Ok(&self.events[committed_index])
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
