use thiserror::Error;

use crate::{
    agent::TurnProposal,
    entropy::EntropySource,
    model::{ContentBlock, Message, MessageSource},
    session::{EventSeq, Session, SessionId, TurnId},
};

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum IdentityError {
    #[error("CLI_ENTROPY_UNAVAILABLE")]
    EntropyUnavailable,
    #[error("CLI_AGENT_UNAVAILABLE")]
    SessionSequenceExhausted,
    #[error("CLI_AGENT_UNAVAILABLE")]
    InvalidUserMessage,
}

pub(super) struct PreparedUserTurn {
    pub(super) start_seq: EventSeq,
    pub(super) turn: TurnId,
    pub(super) proposal: TurnProposal,
}

pub(super) fn new_session_id(entropy: EntropySource) -> Result<SessionId, IdentityError> {
    let id = entropy
        .uuid_v4()
        .map_err(|_| IdentityError::EntropyUnavailable)?;
    Ok(SessionId::new(format!("session-{id}")))
}

pub(super) fn prepare_user_turn(
    session: &Session,
    prompt: &str,
) -> Result<PreparedUserTurn, IdentityError> {
    let start_seq = session
        .next_seq()
        .ok_or(IdentityError::SessionSequenceExhausted)?;
    let turn = session.state().next_turn();
    let content = ContentBlock::text(prompt).map_err(|_| IdentityError::InvalidUserMessage)?;
    let source = MessageSource::user().map_err(|_| IdentityError::InvalidUserMessage)?;
    let message = Message::user(format!("user-{turn}"), vec![content], source)
        .map_err(|_| IdentityError::InvalidUserMessage)?;
    Ok(PreparedUserTurn {
        start_seq,
        turn,
        proposal: TurnProposal::Enter(vec![message]),
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        agent::TurnProposal,
        entropy::{EntropyError, EntropySource},
        model::ContentBlockKind,
        session::{EventKind, NewEvent, Session, TurnEndReason, TurnId},
    };

    use super::{IdentityError, new_session_id, prepare_user_turn};

    fn zeroes(bytes: &mut [u8]) -> Result<(), EntropyError> {
        bytes.fill(0);
        Ok(())
    }

    fn failing(_bytes: &mut [u8]) -> Result<(), EntropyError> {
        Err(EntropyError)
    }

    #[test]
    fn session_id_is_a_prefixed_rfc4122_uuid_v4() {
        let id = new_session_id(EntropySource::injected(zeroes)).unwrap();
        assert_eq!(id.as_str(), "session-00000000-0000-4000-8000-000000000000");
    }

    #[test]
    fn session_id_entropy_failure_is_stable_and_opaque() {
        let error = new_session_id(EntropySource::injected(failing)).unwrap_err();
        assert_eq!(error, IdentityError::EntropyUnavailable);
        assert_eq!(error.to_string(), "CLI_ENTROPY_UNAVAILABLE");
        assert!(!format!("{error:?}").contains("getrandom"));
    }

    #[test]
    fn user_identity_comes_from_the_authoritative_next_turn_without_entropy() {
        let mut session = Session::new("user-identity").unwrap();
        let first = prepare_user_turn(&session, " exact prompt \n").unwrap();
        assert_eq!(first.start_seq.get(), 0);
        assert_eq!(first.turn, TurnId::new(1).unwrap());
        let TurnProposal::Enter(messages) = first.proposal else {
            panic!("a user prompt must enter the turn")
        };
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].id().as_str(), "user-1");
        assert!(matches!(
            messages[0].content()[0].kind(),
            ContentBlockKind::Text { text } if text == " exact prompt \n"
        ));

        let turn = TurnId::new(1).unwrap();
        session
            .append(NewEvent::log(EventKind::turn_start(turn)))
            .unwrap();
        session
            .append(NewEvent::log(EventKind::turn_end(
                turn,
                TurnEndReason::Completed,
            )))
            .unwrap();
        let second = prepare_user_turn(&session, "again").unwrap();
        assert_eq!(second.start_seq.get(), 2);
        assert_eq!(second.turn, TurnId::new(2).unwrap());
        let TurnProposal::Enter(messages) = second.proposal else {
            panic!("a user prompt must enter the turn")
        };
        assert_eq!(messages[0].id().as_str(), "user-2");
    }
}
