use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

use deepseek_harness_cli::{
    model::{
        ContentBlock, JsonValue, LlmCallConfig, LlmFailure, Message, MessageRole, MessageSource,
        NonNegativeSafeInteger, StreamChunk,
    },
    session::{
        AppendError, Clock, ClockError, EpochHeader, EventKind, EventSeq, EventValidationError,
        NewEvent, RequestContext, RequestHeaderReason, Session, StepId, SurfaceError,
        SurfaceIntent, TOOL_NOT_STARTED, TodoItem, TodoStatus, ToolFailure, TransitionError,
        TurnEndCancelCause, TurnEndReason, TurnId, UnixMillis,
    },
};
use serde_json::Value;
use serde_json::json;

fn oracle() -> Value {
    serde_json::from_str(include_str!("fixtures/session/upstream_phase1_oracle.json")).unwrap()
}

struct IncrementingClock {
    next: AtomicI64,
}

struct FailAfterHeader {
    calls: AtomicUsize,
}

struct NegativeAfterHeader {
    calls: AtomicUsize,
}

impl Clock for FailAfterHeader {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            UnixMillis::new(1_000).map_err(|error| ClockError::new(error.to_string()))
        } else {
            Err(ClockError::new("test clock failed"))
        }
    }
}

impl Clock for NegativeAfterHeader {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        let value = if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            1_000
        } else {
            -1
        };
        UnixMillis::new(value).map_err(|error| ClockError::new(error.to_string()))
    }
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

fn turn(value: u64) -> TurnId {
    TurnId::new(value).unwrap()
}

fn step(value: u64) -> StepId {
    StepId::new(value).unwrap()
}

fn text_user(id: &str, text: &str) -> Message {
    Message::user(
        id,
        vec![ContentBlock::text(text).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn assistant(id: &str, content: Vec<ContentBlock>) -> Message {
    Message::assistant(id, content, "mock", "mock-model").unwrap()
}

fn session(id: &str) -> Session {
    Session::with_clock(id, IncrementingClock::new(1_000)).unwrap()
}

fn append_log(session: &mut Session, kind: EventKind) {
    session.append(NewEvent::log(kind)).unwrap();
}

fn append_surface(session: &mut Session, kind: EventKind) {
    session
        .append(NewEvent::surface(kind, SurfaceIntent::append()))
        .unwrap();
}

#[test]
fn completed_tool_flow_has_contiguous_events_and_correlated_messages() {
    let mut session = session("complete-tool");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    append_surface(
        &mut session,
        EventKind::user_message(text_user("user-1", "echo hello")),
    );
    append_log(
        &mut session,
        EventKind::assistant_chunk(
            turn(1),
            step(1),
            StreamChunk::text_delta(0, "working").unwrap(),
        ),
    );
    append_surface(
        &mut session,
        EventKind::assistant_message(
            turn(1),
            step(1),
            assistant(
                "assistant-1",
                vec![ContentBlock::tool_call("call-1", "echo", r#"{"text":"hello"}"#).unwrap()],
            ),
        ),
    );
    append_log(
        &mut session,
        EventKind::tool_call(turn(1), step(1), "call-1", "echo", r#"{"text":"hello"}"#),
    );
    append_surface(
        &mut session,
        EventKind::tool_result(
            turn(1),
            step(1),
            Message::tool_result(
                "tool-1",
                "call-1",
                vec![ContentBlock::text("hello").unwrap()],
                false,
            )
            .unwrap(),
        ),
    );
    append_log(&mut session, EventKind::step_end(turn(1), step(1)));
    append_log(
        &mut session,
        EventKind::turn_end(turn(1), TurnEndReason::Completed),
    );

    assert_eq!(
        session
            .events()
            .iter()
            .map(|event| event.seq().get())
            .collect::<Vec<_>>(),
        (0..9).collect::<Vec<_>>()
    );
    assert_eq!(
        session
            .messages()
            .iter()
            .map(Message::role)
            .collect::<Vec<_>>(),
        vec![MessageRole::User, MessageRole::Assistant, MessageRole::User]
    );
    assert_eq!(
        session.state().surface_nodes(),
        [
            EventSeq::new(2).unwrap(),
            EventSeq::new(4).unwrap(),
            EventSeq::new(6).unwrap()
        ]
    );
    assert_eq!(session.state().next_turn(), turn(2));
    assert_eq!(session.state().open_turn(), None);
}

#[test]
fn blocked_error_and_cancelled_flows_close_only_boundaries_they_opened() {
    let mut blocked = session("blocked");
    append_log(&mut blocked, EventKind::turn_start(turn(1)));
    append_log(
        &mut blocked,
        EventKind::turn_end(turn(1), TurnEndReason::Blocked),
    );
    assert!(blocked.events().iter().all(|event| {
        !matches!(
            event.kind(),
            EventKind::StepStart { .. } | EventKind::StepEnd { .. }
        )
    }));

    let mut failed = session("failed");
    append_log(&mut failed, EventKind::turn_start(turn(1)));
    append_log(&mut failed, EventKind::step_start(turn(1), step(1)));
    append_log(&mut failed, EventKind::step_end(turn(1), step(1)));
    append_log(
        &mut failed,
        EventKind::turn_end(
            turn(1),
            TurnEndReason::Error {
                error: LlmFailure::new("provider unavailable", "NETWORK").unwrap(),
            },
        ),
    );
    assert_eq!(failed.state().open_turn(), None);

    let mut cancelled = session("cancelled");
    append_log(&mut cancelled, EventKind::turn_start(turn(1)));
    append_log(&mut cancelled, EventKind::step_start(turn(1), step(1)));
    append_log(&mut cancelled, EventKind::step_end(turn(1), step(1)));
    append_log(
        &mut cancelled,
        EventKind::turn_end(
            turn(1),
            TurnEndReason::Aborted {
                reason: TurnEndCancelCause::User,
            },
        ),
    );
    assert_eq!(cancelled.state().open_turn(), None);
}

#[test]
fn retained_turn_end_reason_cannot_disagree_with_its_typed_kind() {
    let mut session = session("inconsistent-turn-end");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    let before_events = session.events().to_vec();
    let before_state = session.state();
    let before_seq = session.next_seq();

    let result = session.append(NewEvent::log(EventKind::turn_end(
        turn(1),
        TurnEndReason::Other {
            kind: Some("completed".to_owned()),
            raw: JsonValue::new(json!({ "kind": "future" })).unwrap(),
        },
    )));

    assert!(matches!(
        result,
        Err(AppendError::Validation(
            EventValidationError::InconsistentTurnEndReason
        ))
    ));
    assert_eq!(session.events(), before_events);
    assert_eq!(session.state(), before_state);
    assert_eq!(session.next_seq(), before_seq);
}

#[test]
fn illegal_turn_and_step_transitions_are_atomic() {
    let mut session = session("atomic-transition");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    let before_events = session.events().to_vec();
    let before_state = session.state();
    let before_seq = session.next_seq();

    let error = session
        .append(NewEvent::log(EventKind::turn_start(turn(2))))
        .unwrap_err();
    assert!(matches!(
        error,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::TurnAlreadyOpen { .. }
        ))
    ));
    assert_eq!(session.events(), before_events);
    assert_eq!(session.state(), before_state);
    assert_eq!(session.next_seq(), before_seq);

    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    let error = session
        .append(NewEvent::log(EventKind::assistant_chunk(
            turn(1),
            step(2),
            StreamChunk::text_delta(0, "wrong step").unwrap(),
        )))
        .unwrap_err();
    assert!(matches!(
        error,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::WrongOpenStep { .. }
        ))
    ));
    assert_eq!(session.state().open_step(), Some(step(1)));

    let error = session
        .append(NewEvent::log(EventKind::turn_end(
            turn(1),
            TurnEndReason::Completed,
        )))
        .unwrap_err();
    assert!(matches!(
        error,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::TurnEndWhileStepOpen { .. }
        ))
    ));
}

#[test]
fn numbering_and_enclosure_rules_reject_skips_without_advancing_state() {
    let mut session = session("numbering");
    let wrong_first = session
        .append(NewEvent::log(EventKind::turn_start(turn(2))))
        .unwrap_err();
    assert!(matches!(
        wrong_first,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::WrongNextTurn { .. }
        ))
    ));
    assert!(session.events().is_empty());

    append_log(&mut session, EventKind::turn_start(turn(1)));
    let skipped_step = session
        .append(NewEvent::log(EventKind::step_start(turn(1), step(2))))
        .unwrap_err();
    assert!(matches!(
        skipped_step,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::WrongNextStep { .. }
        ))
    ));
    assert_eq!(session.events().len(), 1);

    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    let nested_step = session
        .append(NewEvent::log(EventKind::step_start(turn(1), step(2))))
        .unwrap_err();
    assert!(matches!(
        nested_step,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::StepAlreadyOpen { .. }
        ))
    ));
    assert_eq!(session.state().open_step(), Some(step(1)));
}

#[test]
fn always_on_rust_invariant_is_a_verified_architecture_difference() {
    let oracle = oracle();
    let upstream = &oracle["invariantRegistration"]["upstreamWithoutCompanion"];
    assert_eq!(upstream["outcome"], "ACCEPTED");
    assert_eq!(upstream["committedLength"], 1);

    let mut session = session("always-on-invariant");
    let error = session
        .append(NewEvent::log(EventKind::turn_start(turn(2))))
        .unwrap_err();
    assert!(matches!(
        error,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::WrongNextTurn { .. }
        ))
    ));
    assert!(session.events().is_empty());
}

#[test]
fn clock_failure_cannot_publish_a_partially_timestamped_event() {
    let mut session = Session::with_clock(
        "clock-failure",
        FailAfterHeader {
            calls: AtomicUsize::new(0),
        },
    )
    .unwrap();
    let before = session.state();
    let error = session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap_err();
    assert!(matches!(error, AppendError::Clock(_)));
    assert!(session.events().is_empty());
    assert_eq!(session.state(), before);
    assert_eq!(session.next_seq(), Some(EventSeq::new(0).unwrap()));
}

#[test]
fn negative_live_clock_is_rejected_without_changing_session_state() {
    let mut session = Session::with_clock(
        "negative-clock",
        NegativeAfterHeader {
            calls: AtomicUsize::new(0),
        },
    )
    .unwrap();
    let before_state = session.state();
    let before_seq = session.next_seq();
    let error = session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap_err();
    assert!(matches!(error, AppendError::Clock(_)));
    assert!(session.events().is_empty());
    assert_eq!(session.state(), before_state);
    assert_eq!(session.next_seq(), before_seq);
}

#[test]
fn request_and_todo_snapshots_require_an_open_turn() {
    let mut session = session("turn-scoped-events");
    let context = RequestContext::new(
        "mock",
        "mock-model",
        Some(NonNegativeSafeInteger::new(128_000).unwrap()),
    )
    .unwrap();
    let outside = session
        .append(NewEvent::log(EventKind::RequestContext {
            context: context.clone(),
        }))
        .unwrap_err();
    assert!(matches!(
        outside,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::EventOutsideTurn { .. }
        ))
    ));

    append_log(&mut session, EventKind::turn_start(turn(1)));
    append_log(
        &mut session,
        EventKind::RequestHeader {
            header: EpochHeader {
                config: LlmCallConfig::from_parts(
                    "mock".to_owned(),
                    "mock-model".to_owned(),
                    None,
                    None,
                    Some(NonNegativeSafeInteger::new(4_096).unwrap()),
                    None,
                )
                .unwrap(),
                adapter_defaults: None,
                system: Some("You are a coding agent.".to_owned()),
                tools: None,
            },
            reason: RequestHeaderReason::Initial,
        },
    );
    append_log(&mut session, EventKind::RequestContext { context });
    append_log(
        &mut session,
        EventKind::TodoWrite {
            todos: vec![TodoItem {
                content: "inspect tests".to_owned(),
                status: TodoStatus::InProgress,
            }],
        },
    );
    assert_eq!(session.events().len(), 4);
    assert!(session.messages().is_empty());
}

#[test]
fn unresolved_calls_may_end_a_step_but_cannot_leak_into_the_next_step() {
    let mut session = session("unresolved-call");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    append_log(
        &mut session,
        EventKind::tool_call(turn(1), step(1), "call-1", "echo", "{}"),
    );
    append_log(&mut session, EventKind::step_end(turn(1), step(1)));
    append_log(&mut session, EventKind::step_start(turn(1), step(2)));

    let error = session
        .append(NewEvent::surface(
            EventKind::tool_result(
                turn(1),
                step(2),
                Message::tool_result("late", "call-1", vec![], false).unwrap(),
            ),
            SurfaceIntent::append(),
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::MissingToolCall { .. }
        ))
    ));
    assert!(session.state().pending_calls().is_empty());
}

#[test]
fn duplicate_call_ids_follow_upstream_set_semantics() {
    let mut session = session("duplicate-call");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    append_log(
        &mut session,
        EventKind::tool_call(turn(1), step(1), "same", "first", "{}"),
    );
    append_log(
        &mut session,
        EventKind::tool_call(turn(1), step(1), "same", "second", "{}"),
    );
    assert_eq!(session.state().pending_calls().len(), 1);
    append_surface(
        &mut session,
        EventKind::tool_result(
            turn(1),
            step(1),
            Message::tool_result("first-result", "same", vec![], false).unwrap(),
        ),
    );
    let second_result = session
        .append(NewEvent::surface(
            EventKind::tool_result(
                turn(1),
                step(1),
                Message::tool_result("second-result", "same", vec![], false).unwrap(),
            ),
            SurfaceIntent::append(),
        ))
        .unwrap_err();
    assert!(matches!(
        second_result,
        AppendError::Validation(EventValidationError::Transition(
            TransitionError::MissingToolCall { .. }
        ))
    ));
}

#[test]
fn synthetic_not_started_repair_result_is_the_only_result_without_a_call() {
    let mut session = session("repair-result");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    let repair = EventKind::ToolResult {
        turn: turn(1),
        step: step(1),
        message: Message::tool_result("repair", "crashed", vec![], true).unwrap(),
        error: Some(ToolFailure {
            name: "ToolNotStartedError".to_owned(),
            code: TOOL_NOT_STARTED.to_owned(),
        }),
        meta: None,
    };
    append_surface(&mut session, repair);
    append_log(&mut session, EventKind::step_end(turn(1), step(1)));
    append_log(
        &mut session,
        EventKind::turn_end(turn(1), TurnEndReason::Interrupted),
    );
    assert_eq!(session.state().open_turn(), None);
}

#[test]
fn surface_metadata_and_replacement_failures_do_not_commit() {
    let mut session = session("surface-atomic");
    let missing_marker = session
        .append(NewEvent::log(EventKind::user_message(text_user(
            "missing", "hidden",
        ))))
        .unwrap_err();
    assert!(matches!(
        missing_marker,
        AppendError::Validation(EventValidationError::Surface(
            SurfaceError::MissingOperation { .. }
        ))
    ));
    assert!(session.events().is_empty());

    let bad_marker = session
        .append(NewEvent::surface(
            EventKind::turn_start(turn(1)),
            SurfaceIntent::append(),
        ))
        .unwrap_err();
    assert!(matches!(
        bad_marker,
        AppendError::Validation(EventValidationError::Surface(
            SurfaceError::MetadataOnIneligibleEvent { .. }
        ))
    ));
    assert!(session.events().is_empty());
    assert_eq!(session.state().open_turn(), None);

    append_surface(
        &mut session,
        EventKind::user_message(text_user("one", "one")),
    );
    append_surface(
        &mut session,
        EventKind::user_message(text_user("two", "two")),
    );
    let before = session.events().to_vec();
    let error = session
        .append(NewEvent::surface(
            EventKind::user_message(text_user("summary", "summary")),
            SurfaceIntent::replace(
                EventSeq::new(0).unwrap(),
                EventSeq::new(1).unwrap(),
                vec![EventSeq::new(0).unwrap()],
            ),
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        AppendError::Validation(EventValidationError::Surface(
            SurfaceError::MissingShadowedSource(_)
        ))
    ));
    assert_eq!(session.events(), before);
    assert_eq!(
        session.state().surface_nodes(),
        [EventSeq::new(0).unwrap(), EventSeq::new(1).unwrap()]
    );

    session
        .append(NewEvent::surface(
            EventKind::user_message(text_user("summary", "summary")),
            SurfaceIntent::replace(
                EventSeq::new(0).unwrap(),
                EventSeq::new(1).unwrap(),
                vec![EventSeq::new(0).unwrap(), EventSeq::new(1).unwrap()],
            ),
        ))
        .unwrap();
    assert_eq!(session.events().len(), 3);
    assert_eq!(session.state().surface_nodes(), [EventSeq::new(2).unwrap()]);
    assert_eq!(session.messages()[0].id().as_str(), "summary");

    let encoded: serde_json::Value = serde_json::from_str(&session.to_json().unwrap()).unwrap();
    assert_eq!(
        encoded["events"][2]["surfaceOp"],
        serde_json::json!({ "op": "replace", "start": 0, "end": 1 })
    );
}

#[test]
fn empty_assistant_remains_on_surface_but_not_in_model_history() {
    let mut session = session("empty-assistant");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    append_surface(
        &mut session,
        EventKind::assistant_message(turn(1), step(1), assistant("empty", vec![])),
    );
    assert_eq!(session.state().surface_nodes(), [EventSeq::new(2).unwrap()]);
    assert!(session.messages().is_empty());
}

#[test]
fn tool_result_replacement_changes_only_nested_model_content() {
    let mut session = session("tool-rewrite");
    append_log(&mut session, EventKind::turn_start(turn(1)));
    append_log(&mut session, EventKind::step_start(turn(1), step(1)));
    append_log(
        &mut session,
        EventKind::tool_call(turn(1), step(1), "call-1", "read", "{}"),
    );
    let original = Message::tool_result(
        "result-1",
        "call-1",
        vec![ContentBlock::text("very long output").unwrap()],
        false,
    )
    .unwrap();
    append_surface(
        &mut session,
        EventKind::tool_result(turn(1), step(1), original),
    );
    append_log(&mut session, EventKind::step_end(turn(1), step(1)));
    append_log(
        &mut session,
        EventKind::turn_end(turn(1), TurnEndReason::Completed),
    );
    append_log(&mut session, EventKind::turn_start(turn(2)));
    let original_seq = EventSeq::new(3).unwrap();
    session
        .append(NewEvent::surface(
            EventKind::tool_result(
                turn(1),
                step(1),
                Message::tool_result(
                    "result-1",
                    "call-1",
                    vec![ContentBlock::text("pruned").unwrap()],
                    false,
                )
                .unwrap(),
            ),
            SurfaceIntent::replace(original_seq, original_seq, vec![original_seq]),
        ))
        .unwrap();
    assert_eq!(session.events().len(), 8);
    assert_eq!(session.state().surface_nodes(), [EventSeq::new(7).unwrap()]);

    let before = session.events().len();
    let error = session
        .append(NewEvent::surface(
            EventKind::tool_result(
                turn(1),
                step(1),
                Message::tool_result(
                    "different-message-id",
                    "call-1",
                    vec![ContentBlock::text("bad").unwrap()],
                    false,
                )
                .unwrap(),
            ),
            SurfaceIntent::replace(
                EventSeq::new(7).unwrap(),
                EventSeq::new(7).unwrap(),
                vec![EventSeq::new(7).unwrap()],
            ),
        ))
        .unwrap_err();
    assert!(matches!(
        error,
        AppendError::Validation(EventValidationError::Surface(
            SurfaceError::ToolResultChangedIdentity
        ))
    ));
    assert_eq!(session.events().len(), before);
}

#[test]
fn every_incremental_prefix_matches_a_fresh_replay() {
    let mut session = session("prefix-replay");
    let events = [
        NewEvent::log(EventKind::turn_start(turn(1))),
        NewEvent::log(EventKind::step_start(turn(1), step(1))),
        NewEvent::surface(
            EventKind::user_message(text_user("user", "question")),
            SurfaceIntent::append(),
        ),
        NewEvent::surface(
            EventKind::assistant_message(
                turn(1),
                step(1),
                assistant("assistant", vec![ContentBlock::text("answer").unwrap()]),
            ),
            SurfaceIntent::append(),
        ),
        NewEvent::log(EventKind::step_end(turn(1), step(1))),
        NewEvent::log(EventKind::turn_end(turn(1), TurnEndReason::MaxTokens)),
    ];
    for event in events {
        session.append(event).unwrap();
        let replayed = Session::replay(session.events()).unwrap();
        assert_eq!(replayed.state(), &session.state());
        assert_eq!(replayed.messages(), session.messages());
    }
}

#[test]
fn projected_message_snapshots_cannot_reorder_or_remove_session_history() {
    let mut session = session("owned-projection");
    append_surface(
        &mut session,
        EventKind::user_message(text_user("one", "first")),
    );
    append_surface(
        &mut session,
        EventKind::user_message(text_user("two", "second")),
    );
    let mut caller_copy = session.messages();
    caller_copy.reverse();
    caller_copy.pop();
    assert_eq!(
        session
            .messages()
            .iter()
            .map(|message| message.id().as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(session.events().len(), 2);
}
