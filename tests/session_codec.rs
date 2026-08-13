use std::sync::atomic::{AtomicI64, Ordering};

use deepseek_harness_cli::{
    model::{Message, MessageRole},
    session::{
        Clock, ClockError, CodecError, EventKind, EventSeq, EventValidationError, HeaderError,
        ReplayError, Session, SessionError, SessionHeader, SurfaceError, TurnId, UnixMillis,
    },
};
use serde_json::{Value, json};

const UPSTREAM_COMMIT: &str = "47f943859bef60e4160492346772ded9b24f765a";

struct IncrementingClock {
    next: AtomicI64,
}

impl IncrementingClock {
    fn new(first: i64) -> Self {
        Self {
            next: AtomicI64::new(first),
        }
    }
}

impl Clock for IncrementingClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        UnixMillis::new(self.next.fetch_add(1, Ordering::SeqCst))
            .map_err(|error| ClockError::new(error.to_string()))
    }
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/session/upstream_phase1_oracle.json")).unwrap()
}

fn snapshot_json(header: &Value, events: &Value) -> String {
    json!({ "header": header, "events": events }).to_string()
}

fn minimal_snapshot(events: Value) -> String {
    json!({
        "header": { "version": 0, "id": "loaded", "createdAt": 10 },
        "events": events,
    })
    .to_string()
}

#[test]
fn official_behavior_fixture_replays_to_expected_surface_and_messages() {
    let fixture = fixture();
    assert_eq!(fixture["fixture"]["baselineCommit"], UPSTREAM_COMMIT);
    let canonical = &fixture["canonicalTrace"];
    let session = Session::from_json(
        &snapshot_json(&canonical["header"], &canonical["events"]),
        IncrementingClock::new(2_000),
    )
    .unwrap();

    assert_eq!(session.first_live_seq(), 9);
    assert_eq!(session.events().len(), 10);
    assert!(matches!(
        session.events().last().map(|event| event.kind()),
        Some(EventKind::EndSeed)
    ));
    assert_eq!(
        session
            .state()
            .surface_nodes()
            .iter()
            .map(|seq| seq.get())
            .collect::<Vec<_>>(),
        canonical["surfaceNodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_u64().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        session
            .messages()
            .iter()
            .map(|message| message.id().as_str())
            .collect::<Vec<_>>(),
        [
            "message-user-1",
            "message-assistant-1",
            "message-tool-result-1"
        ]
    );
    assert_eq!(
        session
            .messages()
            .iter()
            .map(Message::role)
            .collect::<Vec<_>>(),
        [MessageRole::User, MessageRole::Assistant, MessageRole::User]
    );
    assert_eq!(
        serde_json::to_value(session.messages()).unwrap(),
        canonical["derivedMessages"]
    );
    assert_eq!(session.state().next_turn(), TurnId::new(2).unwrap());

    let encoded: Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(encoded["header"], canonical["header"]);
    assert_eq!(
        &encoded["events"].as_array().unwrap()[..9],
        canonical["events"].as_array().unwrap()
    );
}

#[test]
fn official_extension_fixture_round_trips_without_losing_payload_fields() {
    let fixture = fixture();
    let preservation = &fixture["preservation"];
    let session = Session::from_json(
        &snapshot_json(
            &preservation["storedSessionHeader"],
            &preservation["appendedEvents"],
        ),
        IncrementingClock::new(2_000),
    )
    .unwrap();
    let encoded: Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(encoded["header"], preservation["storedSessionHeader"]);
    assert_eq!(
        &encoded["events"].as_array().unwrap()[..3],
        preservation["replayedSeedEvents"].as_array().unwrap()
    );
    assert_eq!(
        serde_json::to_value(session.messages()).unwrap(),
        preservation["replayedDerivedMessages"]
    );
}

#[test]
fn official_numeric_tool_result_rewrite_fixture_is_accepted() {
    let fixture = fixture();
    let rewrite = &fixture["numericToolResultRewrite"];
    assert_eq!(rewrite["outcome"], "ACCEPTED");
    let mut events = rewrite["events"].clone();
    // Preserve the cross-language distinction tested by the oracle: the old
    // event uses JSON `1`, while the replacement enters Rust as JSON `1.0`.
    events[7]["data"]["meta"]["score"] = Value::Number(serde_json::Number::from_f64(1.0).unwrap());
    let session = Session::from_json(
        &snapshot_json(
            &json!({
                "version": 0,
                "id": "oracle-numeric-rewrite",
                "createdAt": 1_700_000_000_000_i64
            }),
            &events,
        ),
        IncrementingClock::new(2_000),
    )
    .unwrap();
    assert_eq!(
        session
            .state()
            .surface_nodes()
            .iter()
            .map(|seq| seq.get())
            .collect::<Vec<_>>(),
        rewrite["surfaceNodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|seq| seq.as_u64().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        serde_json::to_value(session.messages()).unwrap(),
        rewrite["derivedMessages"]
    );
}

#[test]
fn snapshot_encoding_is_deterministic_and_reopening_an_end_marker_is_idempotent() {
    let fixture = fixture();
    let canonical = &fixture["canonicalTrace"];
    let session = Session::from_json(
        &snapshot_json(&canonical["header"], &canonical["events"]),
        IncrementingClock::new(2_000),
    )
    .unwrap();
    let first = session.to_json().unwrap();
    let second = session.to_json().unwrap();
    assert_eq!(first, second);

    let reopened = Session::from_json(&first, IncrementingClock::new(3_000)).unwrap();
    assert_eq!(reopened.first_live_seq(), session.events().len());
    assert_eq!(reopened.events(), session.events());
    assert_eq!(reopened.messages(), session.messages());
    assert_eq!(reopened.to_json().unwrap(), first);
}

#[test]
fn fresh_explicit_empty_and_open_tail_seeds_have_distinct_marker_behavior() {
    let fresh = Session::with_clock("fresh", IncrementingClock::new(10)).unwrap();
    assert!(fresh.events().is_empty());

    let header = SessionHeader::new("empty-seed", UnixMillis::new(20).unwrap()).unwrap();
    let empty = Session::from_seed(header, &[], IncrementingClock::new(21)).unwrap();
    assert_eq!(empty.first_live_seq(), 0);
    assert_eq!(empty.events().len(), 1);
    assert!(matches!(empty.events()[0].kind(), EventKind::EndSeed));

    let open_json = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": -1, "data": { "turn": 1 } }
    ]));
    let open = Session::from_json(&open_json, IncrementingClock::new(30)).unwrap();
    assert_eq!(open.first_live_seq(), 1);
    assert_eq!(open.events().len(), 2);
    assert_eq!(open.state().open_turn(), Some(TurnId::new(1).unwrap()));
    assert!(matches!(open.events()[1].kind(), EventKind::EndSeed));
}

#[test]
fn unknown_required_events_fail_but_unknown_ignorable_events_are_preserved() {
    let fixture = fixture();
    assert_eq!(
        fixture["invariantRegistration"]["upstreamBareCoreUnknownRequired"]["outcome"],
        "ACCEPTED"
    );
    let required = minimal_snapshot(json!([
        { "type": "plugin/required", "seq": 0, "time": 1, "data": { "fact": 1 } }
    ]));
    assert!(matches!(
        Session::from_json(&required, IncrementingClock::new(10)),
        Err(SessionError::Codec(CodecError::UnknownRequiredEvent { .. }))
    ));

    let ignorable = minimal_snapshot(json!([
        {
            "type": "plugin/info",
            "seq": 0,
            "time": 1,
            "data": { "nested": [true, null, "kept"] },
            "ignorable": true
        },
        { "type": "session/end-seed", "seq": 1, "time": 2, "data": {} }
    ]));
    let session = Session::from_json(&ignorable, IncrementingClock::new(10)).unwrap();
    assert_eq!(session.events().len(), 2);
    assert!(session.events()[0].is_ignorable());
    assert!(matches!(
        session.events()[0].kind(),
        EventKind::Unknown { event_type, data }
            if event_type == "plugin/info" && data.as_value()["nested"][2] == "kept"
    ));
    let encoded: Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(encoded["events"][0]["type"], "plugin/info");
    assert_eq!(encoded["events"][0]["ignorable"], true);
    assert_eq!(encoded["events"][0]["data"]["nested"][2], "kept");
}

#[test]
fn exact_envelope_and_contiguous_sequence_fail_before_a_session_is_returned() {
    let extra = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 }, "extra": true }
    ]));
    assert!(matches!(
        Session::from_json(&extra, IncrementingClock::new(10)),
        Err(SessionError::Codec(CodecError::EventEnvelope { .. }))
    ));

    let false_marker = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 }, "ignorable": false }
    ]));
    assert!(matches!(
        Session::from_json(&false_marker, IncrementingClock::new(10)),
        Err(SessionError::Codec(CodecError::EventEnvelope { .. }))
    ));

    let gap = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
        { "type": "turn/end", "seq": 2, "time": 2, "data": { "turn": 1, "reason": { "kind": "completed" } } }
    ]));
    assert!(matches!(
        Session::from_json(&gap, IncrementingClock::new(10)),
        Err(SessionError::Replay(ReplayError {
            source: EventValidationError::NonContiguousSequence { .. },
            ..
        }))
    ));
}

#[test]
fn invalid_headers_fail_but_known_payload_extensions_are_preserved() {
    let version = json!({
        "header": { "version": 1, "id": "loaded", "createdAt": 10 },
        "events": [],
    })
    .to_string();
    assert!(matches!(
        Session::from_json(&version, IncrementingClock::new(10)),
        Err(SessionError::Header(HeaderError::UnsupportedVersion { .. }))
    ));

    let negative_created = json!({
        "header": { "version": 0, "id": "loaded", "createdAt": -1 },
        "events": [],
    })
    .to_string();
    assert!(matches!(
        Session::from_json(&negative_created, IncrementingClock::new(10)),
        Err(SessionError::Header(HeaderError::NegativeCreatedAt))
    ));

    let extra_payload = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1, "pluginField": true } }
    ]));
    let session = Session::from_json(&extra_payload, IncrementingClock::new(10)).unwrap();
    let encoded: Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(encoded["events"][0]["data"]["pluginField"], true);
}

#[test]
fn malformed_current_vocabulary_is_a_fail_closed_import_difference() {
    let fixture = fixture();
    let admission = &fixture["knownPayloadAdmission"];
    assert_eq!(admission["upstreamOutcome"], "ACCEPTED");
    let rust_snapshot = minimal_snapshot(admission["events"].clone());
    assert!(matches!(
        Session::from_json(&rust_snapshot, IncrementingClock::new(10)),
        Err(SessionError::Codec(CodecError::EventPayload { event_type, .. }))
            if event_type == "request/header"
    ));
}

#[test]
fn future_outcomes_and_nullable_plugin_facts_remain_replayable() {
    let fixture = fixture();
    let forward = &fixture["forwardCompatibility"];
    let events = forward["events"].clone();
    let session = Session::from_json(
        &minimal_snapshot(events.clone()),
        IncrementingClock::new(20),
    )
    .unwrap();
    assert_eq!(session.request_context().unwrap().context_window(), None);
    assert_eq!(
        serde_json::to_value(session.messages()).unwrap(),
        forward["derivedMessages"]
    );
    assert_eq!(
        serde_json::to_value(session.request_header().unwrap()).unwrap(),
        forward["requestHeader"]
    );
    assert_eq!(
        session.request_context().unwrap().raw().as_value(),
        &forward["requestContext"]
    );
    assert!(matches!(
        session.events()[7].kind(),
        EventKind::TurnEnd {
            reason: deepseek_harness_cli::session::TurnEndReason::Other { .. },
            ..
        }
    ));

    let encoded: Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(
        &encoded["events"].as_array().unwrap()[..8],
        events.as_array().unwrap()
    );
    assert_eq!(session.events()[5].data().as_value()["meta"], Value::Null);
}

#[test]
fn unknown_ignorable_events_cannot_claim_model_surface_metadata() {
    let invalid = minimal_snapshot(json!([
        {
            "type": "plugin/info",
            "seq": 0,
            "time": 1,
            "data": {},
            "ignorable": true,
            "surfaceOp": "append"
        }
    ]));
    assert!(matches!(
        Session::from_json(&invalid, IncrementingClock::new(10)),
        Err(SessionError::Replay(ReplayError {
            source: EventValidationError::Surface(SurfaceError::MetadataOnIneligibleEvent { .. }),
            ..
        }))
    ));
}

#[test]
fn provenance_lists_distinguish_absent_empty_duplicate_and_future_sources() {
    let assistant_empty = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
        { "type": "step/start", "seq": 1, "time": 2, "data": { "turn": 1, "step": 1 } },
        {
            "type": "assistant/message",
            "seq": 2,
            "time": 3,
            "data": {
                "turn": 1,
                "step": 1,
                "message": {
                    "id": "empty",
                    "role": "assistant",
                    "content": [],
                    "source": { "kind": "model", "provider": "mock", "model": "mock" }
                }
            },
            "sourceEventSeqs": [],
            "surfaceOp": "append"
        }
    ]));
    let session = Session::from_json(&assistant_empty, IncrementingClock::new(10)).unwrap();
    assert!(session.messages().is_empty());
    assert_eq!(session.state().surface_nodes(), [EventSeq::new(2).unwrap()]);

    for sources in [json!([]), json!([0, 0]), json!([1])] {
        let invalid = minimal_snapshot(json!([
            {
                "type": "user/message",
                "seq": 0,
                "time": 1,
                "data": {
                    "id": "user",
                    "role": "user",
                    "content": [{ "type": "text", "text": "x" }],
                    "source": { "kind": "user" }
                },
                "sourceEventSeqs": sources,
                "surfaceOp": "append"
            }
        ]));
        assert!(matches!(
            Session::from_json(&invalid, IncrementingClock::new(10)),
            Err(SessionError::Replay(ReplayError {
                source: EventValidationError::Surface(_),
                ..
            }))
        ));
    }
}

#[test]
fn malformed_specialized_messages_are_rejected_at_the_load_boundary() {
    let wrong_assistant = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
        { "type": "step/start", "seq": 1, "time": 2, "data": { "turn": 1, "step": 1 } },
        {
            "type": "assistant/message",
            "seq": 2,
            "time": 3,
            "data": {
                "turn": 1,
                "step": 1,
                "message": {
                    "id": "wrong-role",
                    "role": "user",
                    "content": [{ "type": "text", "text": "x" }],
                    "source": { "kind": "user" }
                }
            },
            "surfaceOp": "append"
        }
    ]));
    assert!(matches!(
        Session::from_json(&wrong_assistant, IncrementingClock::new(10)),
        Err(SessionError::Replay(ReplayError {
            source: EventValidationError::Model(_),
            ..
        }))
    ));

    let mismatched_result = minimal_snapshot(json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
        { "type": "step/start", "seq": 1, "time": 2, "data": { "turn": 1, "step": 1 } },
        { "type": "tool/call", "seq": 2, "time": 3, "data": {
            "turn": 1, "step": 1, "callId": "call-a", "name": "x", "arguments": "{}"
        } },
        {
            "type": "tool/result",
            "seq": 3,
            "time": 4,
            "data": {
                "turn": 1,
                "step": 1,
                "message": {
                    "id": "result",
                    "role": "user",
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": "call-b",
                        "content": [],
                        "isError": false
                    }],
                    "source": { "kind": "tool", "callId": "call-a" }
                }
            },
            "surfaceOp": "append"
        }
    ]));
    assert!(matches!(
        Session::from_json(&mismatched_result, IncrementingClock::new(10)),
        Err(SessionError::Replay(ReplayError {
            source: EventValidationError::Model(_),
            ..
        }))
    ));

    let empty_id = minimal_snapshot(json!([
        {
            "type": "user/message",
            "seq": 0,
            "time": 1,
            "data": {
                "id": "",
                "role": "user",
                "content": [],
                "source": { "kind": "user" }
            },
            "surfaceOp": "append"
        }
    ]));
    assert!(matches!(
        Session::from_json(&empty_id, IncrementingClock::new(10)),
        Err(SessionError::Codec(CodecError::EventPayload { .. }))
    ));
}

#[test]
fn replacement_endpoints_must_be_current_ordered_surface_nodes() {
    fn user(seq: u64, id: &str) -> Value {
        json!({
            "type": "user/message",
            "seq": seq,
            "time": seq + 1,
            "data": {
                "id": id,
                "role": "user",
                "content": [{ "type": "text", "text": id }],
                "source": { "kind": "user" }
            },
            "surfaceOp": "append"
        })
    }
    let cases = [
        (
            json!({ "op": "replace", "start": 99, "end": 1 }),
            json!([0, 1]),
        ),
        (
            json!({ "op": "replace", "start": 0, "end": 99 }),
            json!([0, 1]),
        ),
        (
            json!({ "op": "replace", "start": 1, "end": 0 }),
            json!([1, 0]),
        ),
    ];
    for (operation, sources) in cases {
        let replacement = json!({
            "type": "user/message",
            "seq": 2,
            "time": 3,
            "data": {
                "id": "summary",
                "role": "user",
                "content": [{ "type": "text", "text": "summary" }],
                "source": { "kind": "user" }
            },
            "sourceEventSeqs": sources,
            "surfaceOp": operation
        });
        let invalid = minimal_snapshot(Value::Array(vec![user(0, "a"), user(1, "b"), replacement]));
        assert!(matches!(
            Session::from_json(&invalid, IncrementingClock::new(10)),
            Err(SessionError::Replay(ReplayError {
                source: EventValidationError::Surface(_),
                ..
            }))
        ));
    }
}

#[test]
fn negative_zero_is_rejected_before_typed_deserialization_can_normalize_it() {
    let json = r#"{
        "header":{"version":0,"id":"loaded","createdAt":10},
        "events":[{"type":"turn/start","seq":0,"time":-0,"data":{"turn":1}}]
    }"#;
    assert!(matches!(
        Session::from_json(json, IncrementingClock::new(10)),
        Err(SessionError::Codec(CodecError::EventEnvelope { .. }))
    ));
}
