use std::sync::atomic::{AtomicI64, Ordering};

use deepseek_harness_cli::{
    model::CallId,
    session::{
        AppendError, ApprovalAskedEvent, ApprovalDecidedEvent, ApprovalOutcome, ApprovalRequestId,
        Clock, ClockError, EventKind, EventValidationError, NewEvent, Session, SessionError,
        StepId, TransitionError, TurnId, UnixMillis,
    },
};
use serde_json::{Value, json};

struct IncrementingClock(AtomicI64);

impl Clock for IncrementingClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        UnixMillis::new(self.0.fetch_add(1, Ordering::SeqCst))
            .map_err(|error| ClockError::new(error.to_string()))
    }
}

fn turn(value: u64) -> TurnId {
    TurnId::new(value).unwrap()
}

fn step(value: u64) -> StepId {
    StepId::new(value).unwrap()
}

fn open_step(id: &str) -> Session {
    let mut session = Session::with_clock(id, IncrementingClock(AtomicI64::new(1_000))).unwrap();
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::step_start(turn(1), step(1))))
        .unwrap();
    session
}

fn asked(id: &str) -> ApprovalAskedEvent {
    ApprovalAskedEvent::new(
        ApprovalRequestId::new(id),
        "apply_patch",
        Some(CallId::new("call-1")),
        Some("workspace file change".to_owned()),
    )
    .unwrap()
}

fn decided(id: &str, outcome: ApprovalOutcome) -> ApprovalDecidedEvent {
    ApprovalDecidedEvent::new(ApprovalRequestId::new(id), outcome).unwrap()
}

#[test]
fn approval_pair_round_trips_and_stays_log_only() {
    let mut session = open_step("approval-roundtrip");
    session
        .append(NewEvent::log(EventKind::approval_asked(asked(
            "approval-1",
        ))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::approval_decided(decided(
            "approval-1",
            ApprovalOutcome::AllowedOnce,
        ))))
        .unwrap();

    assert!(session.messages().is_empty());
    assert!(session.state().pending_approvals().is_empty());
    let encoded: Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(encoded["events"][2]["type"], "approval/asked");
    assert_eq!(encoded["events"][2]["data"]["id"], "approval-1");
    assert_eq!(encoded["events"][2]["data"]["toolName"], "apply_patch");
    assert_eq!(encoded["events"][2]["data"]["callId"], "call-1");
    assert!(encoded["events"][2].get("surfaceOp").is_none());
    assert_eq!(encoded["events"][3]["type"], "approval/decided");
    assert_eq!(encoded["events"][3]["data"]["outcome"], "allowed-once");

    let mut with_extension = encoded;
    with_extension["events"][2]["data"]["futureFact"] = json!({ "kept": true });
    let loaded = Session::from_json(
        &with_extension.to_string(),
        IncrementingClock(AtomicI64::new(2_000)),
    )
    .unwrap();
    let first = loaded.to_json().unwrap();
    let second = loaded.to_json().unwrap();
    assert_eq!(first, second);
    let replayed: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(replayed["events"][2]["data"]["futureFact"]["kept"], true);
    assert!(loaded.messages().is_empty());
}

#[test]
fn invalid_approval_transitions_are_atomic() {
    let mut outside =
        Session::with_clock("approval-outside", IncrementingClock(AtomicI64::new(1_000))).unwrap();
    let before = outside.to_json().unwrap();
    assert!(matches!(
        outside.append(NewEvent::log(EventKind::approval_asked(asked("outside")))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::EventOutsideTurn { .. }
        )))
    ));
    assert_eq!(outside.to_json().unwrap(), before);

    assert!(
        ApprovalAskedEvent::new(ApprovalRequestId::new(""), "apply_patch", None, None).is_err()
    );
    assert!(
        ApprovalAskedEvent::new(
            ApprovalRequestId::new("approval-empty-tool"),
            "",
            None,
            None,
        )
        .is_err()
    );

    let mut session = open_step("approval-invalid");
    session
        .append(NewEvent::log(EventKind::approval_asked(asked(
            "approval-1",
        ))))
        .unwrap();
    let before = session.to_json().unwrap();
    assert!(matches!(
        session.append(NewEvent::log(EventKind::step_end(turn(1), step(1)))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::ApprovalStillPending { .. }
        )))
    ));
    assert_eq!(session.to_json().unwrap(), before);
    assert!(matches!(
        session.append(NewEvent::log(EventKind::approval_asked(asked(
            "approval-1"
        )))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::ApprovalIdAlreadyPending { .. }
        )))
    ));
    assert_eq!(session.to_json().unwrap(), before);

    assert!(matches!(
        session.append(NewEvent::log(EventKind::approval_decided(decided(
            "other",
            ApprovalOutcome::Rejected,
        )))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::ApprovalDecisionWithoutRequest { .. }
        )))
    ));
    assert_eq!(session.to_json().unwrap(), before);

    session
        .append(NewEvent::log(EventKind::approval_decided(decided(
            "approval-1",
            ApprovalOutcome::Rejected,
        ))))
        .unwrap();
    let settled = session.to_json().unwrap();
    assert!(matches!(
        session.append(NewEvent::log(EventKind::approval_asked(asked(
            "approval-1"
        )))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::ApprovalIdAlreadyOwned { .. }
        )))
    ));
    assert_eq!(session.to_json().unwrap(), settled);
    assert!(matches!(
        session.append(NewEvent::log(EventKind::approval_decided(decided(
            "approval-1",
            ApprovalOutcome::Rejected,
        )))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::ApprovalDecisionWithoutRequest { .. }
        )))
    ));
    assert_eq!(session.to_json().unwrap(), settled);

    let unknown_outcome = json!({
        "header": { "version": 0, "id": "approval-unknown", "createdAt": 10 },
        "events": [
            { "type": "turn/start", "seq": 0, "time": 11, "data": { "turn": 1 } },
            {
                "type": "approval/asked",
                "seq": 1,
                "time": 12,
                "data": { "id": "approval-1", "toolName": "apply_patch" }
            },
            {
                "type": "approval/decided",
                "seq": 2,
                "time": 13,
                "data": { "id": "approval-1", "outcome": "future-outcome" }
            }
        ]
    });
    assert!(matches!(
        Session::from_json(
            &unknown_outcome.to_string(),
            IncrementingClock(AtomicI64::new(2_000)),
        ),
        Err(SessionError::Codec(_))
    ));

    let mut pending_turn = Session::with_clock(
        "approval-pending-turn",
        IncrementingClock(AtomicI64::new(3_000)),
    )
    .unwrap();
    pending_turn
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    pending_turn
        .append(NewEvent::log(EventKind::approval_asked(asked(
            "turn-pending",
        ))))
        .unwrap();
    let before = pending_turn.to_json().unwrap();
    assert!(matches!(
        pending_turn.append(NewEvent::log(EventKind::turn_end(
            turn(1),
            deepseek_harness_cli::session::TurnEndReason::Completed,
        ))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::ApprovalStillPending { .. }
        )))
    ));
    assert_eq!(pending_turn.to_json().unwrap(), before);
}

#[test]
fn approval_payload_bounds_and_explicit_nulls_are_rejected_at_the_boundary() {
    assert!(
        ApprovalAskedEvent::new(
            ApprovalRequestId::new("a".repeat(1_024)),
            "t".repeat(256),
            Some(CallId::new("c".repeat(1_024))),
            Some("r".repeat(4_096)),
        )
        .is_ok()
    );
    for invalid in [
        ApprovalAskedEvent::new(
            ApprovalRequestId::new("a".repeat(1_025)),
            "tool",
            None,
            None,
        ),
        ApprovalAskedEvent::new(
            ApprovalRequestId::new("approval"),
            "t".repeat(257),
            None,
            None,
        ),
        ApprovalAskedEvent::new(
            ApprovalRequestId::new("approval"),
            "tool",
            Some(CallId::new("c".repeat(1_025))),
            None,
        ),
        ApprovalAskedEvent::new(
            ApprovalRequestId::new("approval"),
            "tool",
            None,
            Some("r".repeat(4_097)),
        ),
        ApprovalAskedEvent::new(ApprovalRequestId::new("approval"), "tool\0name", None, None),
        ApprovalAskedEvent::new(
            ApprovalRequestId::new("approval"),
            "tool",
            None,
            Some("unsafe\u{001b}reason".to_owned()),
        ),
    ] {
        assert!(invalid.is_err());
    }

    for field in ["callId", "reason"] {
        let mut value = json!({
            "header": { "version": 0, "id": "approval-null", "createdAt": 10 },
            "events": [
                { "type": "turn/start", "seq": 0, "time": 11, "data": { "turn": 1 } },
                {
                    "type": "approval/asked",
                    "seq": 1,
                    "time": 12,
                    "data": { "id": "approval-1", "toolName": "apply_patch" }
                }
            ]
        });
        value["events"][1]["data"][field] = Value::Null;
        assert!(matches!(
            Session::from_json(&value.to_string(), IncrementingClock(AtomicI64::new(4_000)),),
            Err(SessionError::Codec(_))
        ));
    }
}

#[test]
fn unmatched_asked_replays_and_can_be_settled_by_matching_id() {
    let tail = json!({
        "header": { "version": 0, "id": "approval-tail", "createdAt": 10 },
        "events": [
            { "type": "turn/start", "seq": 0, "time": 11, "data": { "turn": 1 } },
            { "type": "step/start", "seq": 1, "time": 12, "data": { "turn": 1, "step": 1 } },
            {
                "type": "approval/asked",
                "seq": 2,
                "time": 13,
                "data": {
                    "id": "approval-1",
                    "toolName": "apply_patch",
                    "callId": "call-1",
                    "futureFact": 7
                }
            }
        ]
    });
    let mut session =
        Session::from_json(&tail.to_string(), IncrementingClock(AtomicI64::new(2_000))).unwrap();
    assert_eq!(
        session
            .state()
            .pending_approvals()
            .iter()
            .map(ApprovalRequestId::as_str)
            .collect::<Vec<_>>(),
        ["approval-1"]
    );
    assert!(matches!(
        session.append(NewEvent::log(EventKind::approval_decided(decided(
            "other",
            ApprovalOutcome::Unavailable,
        )))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::ApprovalDecisionWithoutRequest { .. }
        )))
    ));
    session
        .append(NewEvent::log(EventKind::approval_decided(decided(
            "approval-1",
            ApprovalOutcome::Unavailable,
        ))))
        .unwrap();
    assert!(session.state().pending_approvals().is_empty());
    let encoded: Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(encoded["events"][2]["data"]["futureFact"], 7);
}
