use std::sync::atomic::{AtomicI64, Ordering};

use deepseek_harness_cli::{
    model::{LlmCallConfig, NonNegativeSafeInteger},
    session::{
        AppendError, Clock, ClockError, EpochHeader, EventKind, EventValidationError, NewEvent,
        RequestContext, RequestHeaderReason, Session, TurnId, UnixMillis,
    },
};
use serde_json::{Value, json};

fn oracle() -> Value {
    serde_json::from_str(include_str!("fixtures/session/upstream_phase1_oracle.json")).unwrap()
}

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

fn turn(value: u64) -> TurnId {
    TurnId::new(value).unwrap()
}

#[test]
fn latest_request_header_is_a_canonical_full_replacement() {
    let mut session = Session::with_clock("headers", IncrementingClock::new(1)).unwrap();
    assert!(session.request_header().is_none());
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::RequestHeader {
            header: EpochHeader {
                config: LlmCallConfig::new("mock", "first").unwrap(),
                adapter_defaults: None,
                system: Some("instructions".into()),
                tools: None,
            },
            reason: RequestHeaderReason::Initial,
        }))
        .unwrap();
    assert_eq!(
        session.request_header().unwrap().system.as_deref(),
        Some("instructions")
    );

    session
        .append(NewEvent::log(EventKind::RequestHeader {
            header: EpochHeader {
                config: LlmCallConfig::new("mock", "second").unwrap(),
                adapter_defaults: None,
                system: Some(String::new()),
                tools: Some(Vec::new()),
            },
            reason: RequestHeaderReason::Change,
        }))
        .unwrap();
    let latest = session.request_header().unwrap();
    assert_eq!(latest.config.model(), "second");
    assert!(latest.system.is_none());
    assert!(latest.tools.is_none());
    assert_eq!(
        Session::replay(session.events()).unwrap().request_header(),
        Some(latest)
    );
}

#[test]
fn latest_request_context_replaces_old_capacity_and_keeps_current_extensions() {
    let snapshot = json!({
        "header": { "version": 0, "id": "contexts", "createdAt": 1 },
        "events": [
            { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
            {
                "type": "request/context",
                "seq": 1,
                "time": 2,
                "data": {
                    "provider": "mock",
                    "model": "first",
                    "contextWindow": 128000,
                    "oldExtra": true
                }
            },
            {
                "type": "request/context",
                "seq": 2,
                "time": 3,
                "data": {
                    "provider": "mock",
                    "model": "second",
                    "currentExtra": null
                }
            },
            { "type": "session/end-seed", "seq": 3, "time": 4, "data": {} }
        ]
    });
    let session = Session::from_json(&snapshot.to_string(), IncrementingClock::new(10)).unwrap();
    let context = session.request_context().unwrap();
    assert_eq!(context.model(), Some("second"));
    assert_eq!(context.context_window(), None);
    assert_eq!(context.raw().as_value()["currentExtra"], Value::Null);
    assert!(context.raw().as_value().get("oldExtra").is_none());
    assert_eq!(
        Session::replay(session.events()).unwrap().request_context(),
        Some(context)
    );
}

#[test]
fn request_context_constructor_enforces_safe_integer_capacity() {
    let context = RequestContext::new(
        "mock",
        "model",
        Some(NonNegativeSafeInteger::new(128_000).unwrap()),
    )
    .unwrap();
    assert_eq!(context.context_window().unwrap().get(), 128_000);
}

#[test]
fn legacy_fallback_reason_cannot_enter_through_the_live_public_api() {
    let mut session = Session::with_clock("legacy-reason", IncrementingClock::new(1)).unwrap();
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    let before_events = session.events().to_vec();
    let before_state = session.state();
    let before_seq = session.next_seq();

    let result = session.append(NewEvent::log(EventKind::RequestHeader {
        header: EpochHeader {
            config: LlmCallConfig::new("mock", "model").unwrap(),
            adapter_defaults: None,
            system: None,
            tools: None,
        },
        reason: RequestHeaderReason::Other("fallback".to_owned()),
    }));

    assert!(matches!(
        result,
        Err(AppendError::Validation(
            EventValidationError::LegacyRequestHeaderReason
        ))
    ));
    assert_eq!(session.events(), before_events);
    assert_eq!(session.state(), before_state);
    assert_eq!(session.next_seq(), before_seq);
    assert!(session.request_header().is_none());

    for reason in ["initial", "resume", "change"] {
        let result = session.append(NewEvent::log(EventKind::RequestHeader {
            header: EpochHeader {
                config: LlmCallConfig::new("mock", "model").unwrap(),
                adapter_defaults: None,
                system: None,
                tools: None,
            },
            reason: RequestHeaderReason::Other(reason.to_owned()),
        }));
        assert!(matches!(
            result,
            Err(AppendError::Validation(
                EventValidationError::NonCanonicalRequestHeaderReason { reason: actual }
            )) if actual == reason
        ));
        assert_eq!(session.events(), before_events);
        assert_eq!(session.state(), before_state);
        assert_eq!(session.next_seq(), before_seq);
    }
}

#[test]
fn official_request_projection_fixture_matches_live_upstream_results() {
    let oracle = oracle();
    let expected = &oracle["projections"];
    // The two filtered arrays no longer form a relationally valid chronological
    // log, so compare projections through the original full oracle events.
    let raw_headers = expected["rawHeaderEvents"].as_array().unwrap();
    let raw_contexts = expected["rawContextEvents"].as_array().unwrap();
    assert_eq!(raw_headers.len(), 3);
    assert_eq!(raw_contexts.len(), 3);

    let full_events = json!([
        { "type": "turn/start", "seq": 0, "time": 1, "data": { "turn": 1 } },
        raw_headers[0].clone(),
        raw_contexts[0].clone(),
        raw_headers[1].clone(),
        raw_contexts[1].clone(),
        { "type": "todo/write", "seq": 5, "time": 1, "data": { "todos": [] } },
        raw_headers[2].clone(),
        raw_contexts[2].clone(),
        { "type": "turn/end", "seq": 8, "time": 1, "data": { "turn": 1, "reason": { "kind": "completed" } } },
        { "type": "session/end-seed", "seq": 9, "time": 1, "data": {} }
    ]);
    let mut renumbered = full_events;
    for (index, event) in renumbered.as_array_mut().unwrap().iter_mut().enumerate() {
        event["seq"] = Value::from(index as u64);
    }
    let session = Session::from_json(
        &json!({
            "header": { "version": 0, "id": "oracle-projections", "createdAt": 1 },
            "events": renumbered
        })
        .to_string(),
        IncrementingClock::new(10),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(session.request_header().unwrap()).unwrap(),
        expected["finalLiveHeader"]
    );
    assert_eq!(
        session.request_context().unwrap().raw().as_value(),
        &expected["finalContext"]
    );
}
