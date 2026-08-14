use std::sync::atomic::{AtomicI64, Ordering};

use deepseek_harness_cli::{
    model::{FiniteNumber, LlmCallConfig, LlmFailure},
    session::{
        AppendError, Clock, ClockError, EpochHeader, EventKind, EventValidationError,
        LlmRetryEvent, LlmRetryStartedEvent, NewEvent, RequestHeaderReason, RetryId, RetryNumber,
        Session, StepId, TransitionError, TurnId, UnixMillis,
    },
};

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

fn retry(value: u64) -> RetryNumber {
    RetryNumber::new(value).unwrap()
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
        .append(NewEvent::log(EventKind::RequestHeader {
            header: EpochHeader {
                config: LlmCallConfig::new("mock", "model").unwrap(),
                adapter_defaults: None,
                system: None,
                tools: None,
            },
            reason: RequestHeaderReason::Initial,
        }))
        .unwrap();
    session
}

fn failure() -> LlmFailure {
    LlmFailure::new("try again", "SERVER").unwrap()
}

fn normal_event(number: u64, retry_id: &str) -> LlmRetryEvent {
    LlmRetryEvent::normal(
        RetryId::new(retry_id),
        turn(1),
        step(1),
        "mock",
        "policy-a",
        retry(number),
        retry(2),
        FiniteNumber::new(25.5).unwrap(),
        failure(),
    )
    .unwrap()
}

#[test]
fn retry_chain_is_one_based_correlated_and_round_trips() {
    let mut session = open_step("retry-roundtrip");
    session
        .append(NewEvent::log(EventKind::llm_retry(normal_event(
            1, "chain-a",
        ))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::llm_retry_started(
            LlmRetryStartedEvent::new(RetryId::new("chain-a"), turn(1), step(1), retry(1)).unwrap(),
        )))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::llm_retry(normal_event(
            2, "chain-a",
        ))))
        .unwrap();

    let json = session.to_json().unwrap();
    assert!(json.contains("llm/retry-started"));
    assert!(json.contains("\"delayMs\":25.5"));
    let loaded = Session::from_json(&json, IncrementingClock(AtomicI64::new(2_000))).unwrap();
    assert_eq!(loaded.events()[3..6], session.events()[3..6]);
}

#[test]
fn retry_provider_must_match_the_logged_request_header() {
    let mut session = open_step("retry-provider");
    let bad = LlmRetryEvent::normal(
        RetryId::new("chain"),
        turn(1),
        step(1),
        "other",
        "policy",
        retry(1),
        retry(2),
        FiniteNumber::new(1.0).unwrap(),
        failure(),
    )
    .unwrap();
    let before = session.events().len();
    assert!(matches!(
        session.append(NewEvent::log(EventKind::llm_retry(bad))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::RetryProviderMismatch { .. }
        )))
    ));
    assert_eq!(session.events().len(), before);
}

#[test]
fn retry_number_and_chain_identity_cannot_drift() {
    let mut session = open_step("retry-chain");
    session
        .append(NewEvent::log(EventKind::llm_retry(normal_event(
            1, "chain-a",
        ))))
        .unwrap();
    assert!(matches!(
        session.append(NewEvent::log(EventKind::llm_retry(normal_event(
            2, "chain-b",
        )))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::RetryChainIdMismatch { .. }
        )))
    ));
    assert!(matches!(
        session.append(NewEvent::log(EventKind::llm_retry(normal_event(
            1, "chain-a",
        )))),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::WrongRetryNumber { .. }
        )))
    ));
}

#[test]
fn retry_started_requires_one_unstarted_schedule() {
    let mut session = open_step("retry-started");
    let started = || {
        EventKind::llm_retry_started(
            LlmRetryStartedEvent::new(RetryId::new("chain"), turn(1), step(1), retry(1)).unwrap(),
        )
    };
    assert!(matches!(
        session.append(NewEvent::log(started())),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::RetryStartedWithoutSchedule { .. }
        )))
    ));
    session
        .append(NewEvent::log(EventKind::llm_retry(normal_event(
            1, "chain",
        ))))
        .unwrap();
    session.append(NewEvent::log(started())).unwrap();
    assert!(matches!(
        session.append(NewEvent::log(started())),
        Err(AppendError::Validation(EventValidationError::Transition(
            TransitionError::RetryStartedTwice { .. }
        )))
    ));
}

#[test]
fn retry_started_can_arrive_after_its_referenced_step_closed() {
    let mut session = open_step("retry-started-after-step");
    session
        .append(NewEvent::log(EventKind::llm_retry(normal_event(
            1, "chain",
        ))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::step_end(turn(1), step(1))))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::llm_retry_started(
            LlmRetryStartedEvent::new(RetryId::new("chain"), turn(1), step(1), retry(1)).unwrap(),
        )))
        .unwrap();

    let encoded = session.to_json().unwrap();
    let loaded = Session::from_json(&encoded, IncrementingClock(AtomicI64::new(2_000))).unwrap();
    assert!(matches!(
        loaded.events()[5].kind(),
        EventKind::LlmRetryStarted { .. }
    ));
}
