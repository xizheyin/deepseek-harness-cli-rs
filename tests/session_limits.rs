use std::sync::atomic::{AtomicI64, Ordering};

use deepseek_harness_cli::{
    model::{
        ContentBlock, JsonValue, JsonValueError, MAX_JSON_VALUE_BYTES, Message, MessageSource,
    },
    session::{
        AppendError, Clock, ClockError, CodecError, EventKind, EventValidationError, HeaderError,
        MAX_SESSION_EVENTS, MAX_SESSION_HEADER_BYTES, MAX_SESSION_RETAINED_JSON_BYTES,
        MAX_SESSION_SNAPSHOT_BYTES, MAX_SOURCE_EVENT_SEQS, NewEvent, ReplayError, Session,
        SessionError, SessionHeader, SurfaceError, SurfaceIntent, UnixMillis,
    },
};
use serde_json::{Value, json};

struct IncrementingClock(AtomicI64);

impl IncrementingClock {
    fn new(first: i64) -> Self {
        Self(AtomicI64::new(first))
    }
}

impl Clock for IncrementingClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        UnixMillis::new(self.0.fetch_add(1, Ordering::SeqCst))
            .map_err(|error| ClockError::new(error.to_string()))
    }
}

#[test]
fn opaque_json_and_header_limits_fail_explicitly() {
    let oversized = Value::String("x".repeat(MAX_JSON_VALUE_BYTES));
    assert!(matches!(
        JsonValue::new(oversized),
        Err(JsonValueError::TooLarge {
            maximum: MAX_JSON_VALUE_BYTES,
            ..
        })
    ));

    let header = json!({
        "version": 0,
        "id": "large-header",
        "createdAt": 1,
        "extension": "x".repeat(MAX_SESSION_HEADER_BYTES)
    });
    assert!(matches!(
        SessionHeader::from_value(header),
        Err(HeaderError::TooLarge {
            maximum: MAX_SESSION_HEADER_BYTES,
            ..
        })
    ));
}

#[test]
fn failed_header_update_does_not_publish_partial_metadata() {
    let mut header = SessionHeader::new("header", UnixMillis::new(1).unwrap()).unwrap();
    let oversized_absolute_path = format!("/{}", "x".repeat(MAX_SESSION_HEADER_BYTES));
    assert!(matches!(
        header.set_cwd(oversized_absolute_path),
        Err(HeaderError::TooLarge { .. })
    ));
    assert_eq!(header.cwd(), None);
    assert!(serde_json::to_value(header).unwrap().get("cwd").is_none());
}

#[test]
fn snapshot_and_event_count_limits_stop_unbounded_imports() {
    let oversized_snapshot = " ".repeat(MAX_SESSION_SNAPSHOT_BYTES + 1);
    assert!(matches!(
        Session::from_json(&oversized_snapshot, IncrementingClock::new(1)),
        Err(SessionError::Codec(CodecError::SnapshotTooLarge {
            maximum: MAX_SESSION_SNAPSHOT_BYTES,
            ..
        }))
    ));

    let too_many_events = vec![json!({}); MAX_SESSION_EVENTS + 1];
    let snapshot = json!({
        "header": { "version": 0, "id": "too-many", "createdAt": 1 },
        "events": too_many_events
    })
    .to_string();
    assert!(matches!(
        Session::from_json(&snapshot, IncrementingClock::new(1)),
        Err(SessionError::Codec(CodecError::TooManyEvents {
            maximum: MAX_SESSION_EVENTS,
            ..
        }))
    ));
}

#[test]
fn provenance_list_limit_precedes_quadratic_or_relational_work() {
    let snapshot = json!({
        "header": { "version": 0, "id": "sources", "createdAt": 1 },
        "events": [{
            "type": "user/message",
            "seq": 0,
            "time": 1,
            "data": {
                "id": "user",
                "role": "user",
                "content": [],
                "source": { "kind": "user" }
            },
            "sourceEventSeqs": vec![0_u64; MAX_SOURCE_EVENT_SEQS + 1],
            "surfaceOp": "append"
        }]
    })
    .to_string();
    assert!(matches!(
        Session::from_json(&snapshot, IncrementingClock::new(1)),
        Err(SessionError::Replay(ReplayError {
            source: EventValidationError::Surface(SurfaceError::TooManySources {
                maximum: MAX_SOURCE_EVENT_SEQS,
                ..
            }),
            ..
        }))
    ));
}

#[test]
fn live_session_has_an_aggregate_payload_budget_and_fails_atomically() {
    let mut session = Session::with_clock("live-budget", IncrementingClock::new(1)).unwrap();
    let chunk = "x".repeat(1024 * 1024);
    let mut rejected = false;
    for index in 0..32 {
        let message = Message::user(
            format!("message-{index}"),
            vec![ContentBlock::text(chunk.clone()).unwrap()],
            MessageSource::user().unwrap(),
        )
        .unwrap();
        let before_len = session.events().len();
        let before_seq = session.next_seq();
        let before_state = session.state();
        let before_messages = session.messages();
        match session.append(NewEvent::surface(
            EventKind::user_message(message),
            SurfaceIntent::append(),
        )) {
            Ok(_) => {}
            Err(AppendError::RetainedJsonLimit { maximum }) => {
                assert_eq!(maximum, MAX_SESSION_RETAINED_JSON_BYTES);
                assert_eq!(session.events().len(), before_len);
                assert_eq!(session.next_seq(), before_seq);
                assert_eq!(session.state(), before_state);
                assert_eq!(session.messages(), before_messages);
                rejected = true;
                break;
            }
            Err(error) => panic!("unexpected append error: {error}"),
        }
    }
    assert!(
        rejected,
        "aggregate budget must reject before 32 MiB is retained"
    );

    let small = Message::user(
        "after-rejection",
        vec![ContentBlock::text("small").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap();
    session
        .append(NewEvent::surface(
            EventKind::user_message(small),
            SurfaceIntent::append(),
        ))
        .unwrap();
    assert_eq!(session.events().len(), session.messages().len());
}
