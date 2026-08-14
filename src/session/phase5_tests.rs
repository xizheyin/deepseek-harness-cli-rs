use super::{
    AppendError, EventKind, MAX_SESSION_RETAINED_JSON_BYTES, NewEvent, Session, TodoItem,
    TodoStatus, TurnId,
};

fn todo(content: impl Into<String>) -> NewEvent {
    NewEvent::log(EventKind::TodoWrite {
        todos: vec![TodoItem {
            content: content.into(),
            status: TodoStatus::Pending,
        }],
    })
}

#[test]
fn phase5_claim_ceiling_can_grow_and_rebind_without_committing_early() {
    let mut session = Session::new("phase5-claim-grow").unwrap();
    session
        .append(NewEvent::log(EventKind::turn_start(
            TurnId::new(1).unwrap(),
        )))
        .unwrap();
    let mut reservation = session.reservation();
    let [mut claim] = reservation
        .claim_batch([NewEvent::log(EventKind::EndSeed)])
        .unwrap()
        .try_into()
        .unwrap();

    reservation
        .reserve_claim_retained_json_bytes(&mut claim, 512)
        .unwrap();
    reservation
        .rebind_claim_fallback(&mut claim, todo("bounded replacement"))
        .unwrap();
    assert_eq!(reservation.session().events().len(), 1);

    assert!(matches!(
        reservation.rebind_claim_fallback(&mut claim, todo("x".repeat(1_024))),
        Err(AppendError::ClaimPayloadTooLarge { reserved: 512, .. })
    ));
    assert_eq!(reservation.session().events().len(), 1);

    reservation.settle_exact(&mut claim).unwrap();
    assert!(matches!(
        reservation.session().events()[1].kind(),
        EventKind::TodoWrite { todos }
            if todos[0].content == "bounded replacement"
    ));
}

#[test]
fn phase5_claim_growth_failure_is_atomic_and_keeps_the_original_fallback() {
    let mut session = Session::new("phase5-claim-growth-failure").unwrap();
    let mut reservation = session.reservation();
    let [mut claim] = reservation
        .claim_batch([NewEvent::log(EventKind::EndSeed)])
        .unwrap()
        .try_into()
        .unwrap();

    assert!(matches!(
        reservation.reserve_claim_retained_json_bytes(&mut claim, MAX_SESSION_RETAINED_JSON_BYTES,),
        Err(AppendError::ReservedRetainedJsonLimit { .. })
    ));
    assert!(reservation.session().events().is_empty());
    reservation.settle_exact(&mut claim).unwrap();
    assert!(matches!(
        reservation.session().events()[0].kind(),
        EventKind::EndSeed
    ));
}

#[test]
fn phase5_preferred_only_never_substitutes_a_fallback_after_a_side_effect() {
    let mut session = Session::new("phase5-preferred-only").unwrap();
    let mut reservation = session.reservation();
    let [mut claim] = reservation
        .claim_batch([NewEvent::log(EventKind::EndSeed)])
        .unwrap()
        .try_into()
        .unwrap();
    reservation
        .reserve_claim_retained_json_bytes(&mut claim, 128)
        .unwrap();

    assert!(matches!(
        reservation.settle_preferred_only(&mut claim, todo("x".repeat(1_024))),
        Err(AppendError::ClaimPayloadTooLarge { reserved: 128, .. })
    ));
    assert!(reservation.session().events().is_empty());

    // The failed preferred-only attempt leaves the claim active; writing its
    // fallback here proves it was not silently consumed or substituted.
    reservation.settle_exact(&mut claim).unwrap();
    assert!(matches!(
        reservation.session().events()[0].kind(),
        EventKind::EndSeed
    ));
}
