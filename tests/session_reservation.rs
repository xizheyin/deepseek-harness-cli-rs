use std::sync::atomic::{AtomicI64, Ordering};

use deepseek_harness_cli::session::{
    AppendError, ClaimedAppend, Clock, ClockError, EventKind, MAX_SESSION_EVENTS, NewEvent,
    Session, StepId, TodoItem, TodoStatus, TurnEndReason, TurnId, UnixMillis,
};

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

fn step(value: u64) -> StepId {
    StepId::new(value).unwrap()
}

fn session(id: &str) -> Session {
    Session::with_clock(id, IncrementingClock::new(1_000)).unwrap()
}

#[test]
fn a_claim_protects_the_last_event_slot_until_it_is_settled() {
    let mut session = session("reservation-last-slot");
    let mut reservation = session.reservation();
    let [mut turn_end] = reservation
        .claim_batch([NewEvent::log(EventKind::turn_end(
            turn(1),
            TurnEndReason::Completed,
        ))])
        .unwrap()
        .try_into()
        .unwrap();

    reservation
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    while reservation.session().events().len() < MAX_SESSION_EVENTS - 1 {
        reservation
            .append(NewEvent::log(EventKind::EndSeed))
            .unwrap();
    }

    assert!(matches!(
        reservation.append(NewEvent::log(EventKind::EndSeed)),
        Err(AppendError::ReservedEventLimit { reserved: 1, .. })
    ));
    let seq = reservation.settle_exact(&mut turn_end).unwrap();
    assert_eq!(usize::try_from(seq.get()).unwrap(), MAX_SESSION_EVENTS - 1);
    assert_eq!(reservation.session().events().len(), MAX_SESSION_EVENTS);
    assert_eq!(reservation.session().state().open_turn(), None);
}

#[test]
fn failed_settlement_keeps_the_claim_available() {
    let mut session = session("reservation-retry");
    let mut reservation = session.reservation();
    let [mut fallback] = reservation
        .claim_batch([NewEvent::log(EventKind::EndSeed)])
        .unwrap()
        .try_into()
        .unwrap();

    assert!(matches!(
        reservation.settle(
            &mut fallback,
            NewEvent::log(EventKind::step_end(turn(1), step(1))),
        ),
        Err(AppendError::Validation(_))
    ));
    reservation.settle_exact(&mut fallback).unwrap();
    assert!(matches!(
        reservation.session().events()[0].kind(),
        EventKind::EndSeed
    ));
}

#[test]
fn settlement_falls_back_when_the_preferred_payload_would_invade_other_claims() {
    let mut session = session("reservation-bytes");
    session
        .append(NewEvent::log(EventKind::turn_start(turn(1))))
        .unwrap();
    for index in 0..2 {
        session
            .append(NewEvent::log(EventKind::TodoWrite {
                todos: vec![TodoItem {
                    content: format!("{index}{}", "x".repeat(5 * 1024 * 1024)),
                    status: TodoStatus::Pending,
                }],
            }))
            .unwrap();
    }

    let mut reservation = session.reservation();
    let mut claims = reservation
        .claim_batch([
            NewEvent::log(EventKind::EndSeed),
            NewEvent::log(EventKind::EndSeed),
        ])
        .unwrap();
    let mut first = claims.remove(0);
    let mut second = claims.remove(0);
    let preferred = NewEvent::log(EventKind::TodoWrite {
        todos: vec![TodoItem {
            content: "y".repeat(7 * 1024 * 1024),
            status: TodoStatus::Pending,
        }],
    });

    assert_eq!(
        reservation.settle(&mut first, preferred).unwrap(),
        ClaimedAppend::Fallback(reservation.session().events().last().unwrap().seq())
    );
    reservation.settle_exact(&mut second).unwrap();
    assert!(matches!(
        reservation.session().events().last().unwrap().kind(),
        EventKind::EndSeed
    ));
}

#[test]
fn a_claim_cannot_be_consumed_by_another_reservation() {
    let mut session = session("reservation-owner");
    let mut first = session.reservation();
    let [mut claim] = first
        .claim_batch([NewEvent::log(EventKind::EndSeed)])
        .unwrap()
        .try_into()
        .unwrap();
    drop(first);

    let mut second = session.reservation();
    assert!(matches!(
        second.settle_exact(&mut claim),
        Err(AppendError::InvalidClaim)
    ));
    assert!(second.session().events().is_empty());
}
