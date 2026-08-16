use std::{
    mem::size_of,
    sync::atomic::{AtomicI64, Ordering},
};

use crate::model::{ContentBlock, Message, MessageSource, StreamChunk};
use tokio::sync::mpsc::error::TryRecvError;

use super::{
    AppendError, Clock, ClockError, CommittedUiEvent, CommittedUiKind, EventKind, EventSeq,
    NewEvent, Session, SourceSeqBitmap, StepId, SurfaceIntent, ToolFailure, TurnId,
    UiAssistantContent, UiObserverAttachError, UnixMillis,
};

#[derive(Debug)]
struct IncrementingClock(AtomicI64);

impl IncrementingClock {
    fn new(start: i64) -> Self {
        Self(AtomicI64::new(start))
    }
}

impl Clock for IncrementingClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        UnixMillis::new(self.0.fetch_add(1, Ordering::SeqCst))
            .map_err(|error| ClockError::new(error.to_string()))
    }
}

fn session(id: &str) -> Session {
    Session::with_clock(id, IncrementingClock::new(1_000)).unwrap()
}

fn turn(value: u64) -> TurnId {
    TurnId::new(value).unwrap()
}

fn step(value: u64) -> StepId {
    StepId::new(value).unwrap()
}

#[test]
fn observer_attaches_only_to_a_fresh_session_once() {
    let mut fresh = session("fresh-observer");
    let receiver = fresh.attach_ui_observer().unwrap();
    assert!(matches!(
        fresh.attach_ui_observer(),
        Err(UiObserverAttachError::AlreadyAttached)
    ));
    drop(receiver);
    assert!(matches!(
        fresh.attach_ui_observer(),
        Err(UiObserverAttachError::AlreadyAttached)
    ));

    let mut nonempty = session("nonempty-observer");
    nonempty.append(NewEvent::log(EventKind::EndSeed)).unwrap();
    assert!(matches!(
        nonempty.attach_ui_observer(),
        Err(UiObserverAttachError::NotFresh)
    ));

    let seed_header = nonempty.header().clone();
    let seed_events = nonempty.events().to_vec();
    let mut seeded =
        Session::from_seed(seed_header, &seed_events, IncrementingClock::new(2_000)).unwrap();
    assert!(matches!(
        seeded.attach_ui_observer(),
        Err(UiObserverAttachError::NotFresh)
    ));
}

#[test]
fn rejected_append_emits_no_projection() {
    let mut session = session("rejected-observer-event");
    let mut receiver = session.attach_ui_observer().unwrap();
    let budget_before = session.remaining_budget();

    let error = session
        .append(NewEvent::log(EventKind::step_end(turn(1), step(1))))
        .unwrap_err();

    assert!(matches!(error, AppendError::Validation(_)));
    assert!(session.events().is_empty());
    assert_eq!(session.remaining_budget(), budget_before);
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    assert!(!receiver.is_producer_faulted());
}

#[test]
fn committed_events_arrive_once_in_sequence_after_claim_settlement() {
    let mut session = session("settled-observer-events");
    let mut receiver = session.attach_ui_observer().unwrap();
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();

    let mut reservation = session.reservation();
    let [mut claim] = reservation
        .claim_batch([NewEvent::log(EventKind::step_start(turn(1), step(1)))])
        .unwrap()
        .try_into()
        .unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap().kind,
        CommittedUiKind::TurnStart { turn: value } if value == turn(1)
    ));
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

    reservation.settle_exact(&mut claim).unwrap();
    drop(reservation);

    let event = receiver.try_recv().unwrap();
    assert_eq!(event.seq, EventSeq::new(1).unwrap());
    assert_eq!(event.time, UnixMillis::new(1_002).unwrap());
    assert!(matches!(
        event.kind,
        CommittedUiKind::StepStart {
            turn: event_turn,
            step: event_step,
        } if event_turn == turn(1) && event_step == step(1)
    ));
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn append_receipt_owns_the_committed_surface_message_after_later_appends() {
    let mut session = session("owned-append-receipt");
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    let message = Message::user(
        "user-1",
        vec![ContentBlock::text("hello").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap();
    let receipt = session
        .append(NewEvent::surface(
            EventKind::user_message(message.clone()),
            SurfaceIntent::append(),
        ))
        .unwrap();

    session.append(NewEvent::log(EventKind::EndSeed)).unwrap();

    assert_eq!(receipt.seq(), EventSeq::new(1).unwrap());
    assert_eq!(receipt.time(), UnixMillis::new(1_002).unwrap());
    assert_eq!(receipt.event_type(), "user/message");
    assert!(!receipt.observer_faulted());
    assert_eq!(receipt.committed_message(), Some(&message));
}

#[test]
fn observer_full_faults_only_after_the_session_commit() {
    let mut session = session("full-observer");
    let mut receiver = session.attach_ui_observer_for_test(1).unwrap();

    let first = session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    let second = session
        .append(NewEvent::log(EventKind::step_start(turn(1), step(1))))
        .unwrap();

    assert_eq!(session.events().len(), 2);
    assert_eq!(session.next_seq(), Some(EventSeq::new(2).unwrap()));
    assert_eq!(session.state().open_step(), Some(step(1)));
    assert!(!first.observer_faulted());
    assert!(second.observer_faulted());
    assert!(receiver.is_producer_faulted());
    assert_eq!(receiver.try_recv().unwrap().seq, EventSeq::new(0).unwrap());
    assert!(matches!(
        receiver.try_recv(),
        Err(TryRecvError::Disconnected)
    ));

    let mut reservation = session.reservation();
    let [mut claim] = reservation
        .claim_batch([NewEvent::log(EventKind::step_end(turn(1), step(1)))])
        .unwrap()
        .try_into()
        .unwrap();
    reservation.settle_exact(&mut claim).unwrap();
    assert_eq!(reservation.session().events().len(), 3);
    assert_eq!(reservation.session().state().open_step(), None);
    assert!(matches!(
        receiver.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
}

#[test]
fn injected_projection_failure_faults_only_after_commit() {
    let mut session = session("projection-failure");
    let mut receiver = session.attach_ui_observer().unwrap();
    receiver.fail_next_projection_for_test();

    let receipt = session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();

    assert_eq!(session.events().len(), 1);
    assert_eq!(session.state().open_turn(), Some(turn(1)));
    assert!(receipt.observer_faulted());
    assert!(receiver.is_producer_faulted());
    assert!(matches!(
        receiver.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
    let later = session
        .append(NewEvent::log(EventKind::step_start(turn(1), step(1))))
        .unwrap();
    assert!(later.observer_faulted());
    assert_eq!(session.events().len(), 2);
    assert!(matches!(
        receiver.try_recv(),
        Err(TryRecvError::Disconnected)
    ));
}

#[test]
fn closed_receiver_detaches_without_faulting_or_rolling_back() {
    let mut session = session("closed-observer");
    let receiver = session.attach_ui_observer().unwrap();
    let fault = receiver.fault_handle_for_test();
    drop(receiver);

    let receipt = session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();

    assert_eq!(session.events().len(), 1);
    assert!(!receipt.observer_faulted());
    assert!(!fault.load(Ordering::SeqCst));
    assert!(matches!(
        session.attach_ui_observer(),
        Err(UiObserverAttachError::AlreadyAttached)
    ));
}

#[test]
fn fresh_capacity_theorem_holds_without_a_consumer() {
    let mut session = session("observer-capacity");
    let mut receiver = session.attach_ui_observer().unwrap();
    for _ in 0..super::MAX_SESSION_EVENTS {
        session.append(NewEvent::log(EventKind::EndSeed)).unwrap();
    }
    assert!(!receiver.is_producer_faulted());
    for expected in 0..super::MAX_SESSION_EVENTS {
        assert_eq!(
            receiver.try_recv().unwrap().seq,
            EventSeq::from_index(expected).unwrap()
        );
    }
    assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn source_bitmap_maps_word_edges_and_has_a_fixed_allocation() {
    let sources = [50_000_u64, 50_063, 50_064, 54_095]
        .into_iter()
        .map(|value| EventSeq::new(value).unwrap())
        .collect::<Vec<_>>();
    let bitmap = SourceSeqBitmap::from_sources(&sources).unwrap();

    assert_eq!(bitmap.base_for_test(), EventSeq::new(50_000).unwrap());
    assert_eq!(bitmap.word_len_for_test(), 64);
    assert!(bitmap.word_capacity_for_test() <= 128);
    assert!(bitmap.allocated_bytes_for_test() <= 1_024);
    for source in sources {
        assert!(bitmap.contains(source));
    }
    assert!(!bitmap.contains(EventSeq::new(50_062).unwrap()));
    assert!(!bitmap.contains(EventSeq::new(49_999).unwrap()));
    assert_eq!(4_096 * 1_024, 4 * 1_024 * 1_024);
}

#[test]
fn source_bitmap_rejects_a_span_larger_than_one_provider_attempt() {
    assert!(
        SourceSeqBitmap::from_sources(&[
            EventSeq::new(50_000).unwrap(),
            EventSeq::new(54_096).unwrap(),
        ])
        .is_err()
    );
    assert!(SourceSeqBitmap::capacity_is_acceptable_for_test(128));
    assert!(!SourceSeqBitmap::capacity_is_acceptable_for_test(129));
    let mut overallocated = Vec::with_capacity(129);
    overallocated.resize(64, 0_u64);
    assert!(SourceSeqBitmap::from_words_for_test(overallocated).is_err());
}

#[test]
fn projection_omits_opaque_and_tool_result_bodies() {
    const SECRET: &str = "SENTINEL_MUST_NOT_ENTER_UI_PROJECTION";
    let mut session = session("minimal-projection");
    let mut receiver = session.attach_ui_observer().unwrap();
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::step_start(turn(1), step(1))))
        .unwrap();
    let chunk_seq = session
        .append(NewEvent::log(EventKind::assistant_chunk(
            turn(1),
            step(1),
            StreamChunk::text_delta(0, "visible streamed text").unwrap(),
        )))
        .unwrap()
        .seq();
    let assistant = Message::assistant(
        "assistant-minimal",
        vec![
            ContentBlock::text("visible final text").unwrap(),
            ContentBlock::reasoning("visible reasoning").unwrap(),
            ContentBlock::from_value(serde_json::json!({
                "type": "plugin-secret",
                "payload": SECRET,
            }))
            .unwrap(),
        ],
        "mock",
        "mock-model",
    )
    .unwrap();
    session
        .append(NewEvent::surface(
            EventKind::assistant_message(turn(1), step(1), assistant),
            SurfaceIntent::append().with_sources(vec![chunk_seq]),
        ))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::tool_call(
            turn(1),
            step(1),
            "call-minimal",
            "read",
            format!(r#"{{"patch":"{}{}"}}"#, SECRET, "x".repeat(700)),
        )))
        .unwrap();
    session
        .append(NewEvent::surface(
            EventKind::ToolResult {
                turn: turn(1),
                step: step(1),
                message: Message::tool_result(
                    "tool-minimal",
                    "call-minimal",
                    vec![ContentBlock::text(SECRET).unwrap()],
                    true,
                )
                .unwrap(),
                error: Some(ToolFailure {
                    name: "ReadError".to_owned(),
                    code: "READ_FAILED".to_owned(),
                }),
                meta: Some(
                    crate::model::JsonValue::new(serde_json::json!({
                        "secret": SECRET,
                    }))
                    .unwrap(),
                ),
            },
            SurfaceIntent::append(),
        ))
        .unwrap();

    let mut projected = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        projected.push(event);
    }
    let debug = format!("{projected:?}");
    assert!(!debug.contains(SECRET));
    assert!(debug.contains("visible streamed text"));
    assert!(debug.contains("visible final text"));
    assert!(debug.contains("visible reasoning"));
    assert!(debug.contains("READ_FAILED"));
    assert!(matches!(
        &projected[3].kind,
        CommittedUiKind::AssistantMessage { sources, .. }
            if sources.contains(chunk_seq)
    ));
    assert!(matches!(
        &projected[4].kind,
        CommittedUiKind::ToolRequested {
            arguments_preview,
            arguments_truncated: true,
            ..
        } if arguments_preview == "arguments omitted"
    ));
    assert!(size_of::<CommittedUiEvent>() <= 512);
}

fn projected_assistant_content(id: &str, blocks: Vec<ContentBlock>) -> UiAssistantContent {
    let mut session = session(id);
    let mut receiver = session.attach_ui_observer().unwrap();
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::step_start(turn(1), step(1))))
        .unwrap();
    let message =
        Message::assistant(format!("assistant-{id}"), blocks, "mock", "mock-model").unwrap();
    session
        .append(NewEvent::surface(
            EventKind::assistant_message(turn(1), step(1), message),
            SurfaceIntent::append(),
        ))
        .unwrap();
    let _ = receiver.try_recv().unwrap();
    let _ = receiver.try_recv().unwrap();
    match receiver.try_recv().unwrap().kind {
        CommittedUiKind::AssistantMessage { content, .. } => content,
        other => panic!("expected assistant projection, got {other:?}"),
    }
}

#[test]
fn assistant_projection_uses_indexed_and_degraded_bounds_without_opaque_amplification() {
    let indexed = projected_assistant_content(
        "indexed-128",
        (0..128).map(|_| ContentBlock::text("x").unwrap()).collect(),
    );
    assert!(matches!(
        indexed,
        UiAssistantContent::Indexed(blocks) if blocks.len() == 128
    ));

    let degraded = projected_assistant_content(
        "degraded-129",
        (0..129).map(|_| ContentBlock::text("x").unwrap()).collect(),
    );
    assert!(matches!(
        degraded,
        UiAssistantContent::Degraded { text } if text.len() == 129
    ));

    let opaque = projected_assistant_content(
        "opaque-4096",
        (0..crate::model::MAX_MESSAGE_CONTENT_BLOCKS)
            .map(|_| ContentBlock::from_value(serde_json::json!({"type": "opaque"})).unwrap())
            .collect(),
    );
    assert!(matches!(
        opaque,
        UiAssistantContent::Indexed(blocks)
            if blocks.is_empty() && blocks.capacity() == 0
    ));

    let exact = projected_assistant_content(
        "indexed-4mib",
        vec![ContentBlock::text("x".repeat(4 * 1024 * 1024)).unwrap()],
    );
    assert!(matches!(exact, UiAssistantContent::Indexed(_)));

    let one_over = projected_assistant_content(
        "degraded-4mib-plus-one",
        vec![ContentBlock::text("x".repeat(4 * 1024 * 1024 + 1)).unwrap()],
    );
    assert!(matches!(
        one_over,
        UiAssistantContent::Degraded { text } if text.len() == 4 * 1024 * 1024 + 1
    ));
}
