#[cfg(test)]
use std::convert::Infallible;

use thiserror::Error;

use crate::{
    model::{ContentBlockKind, Message},
    session::{EventKind, EventSeq, SessionEvent, TurnEndReason, TurnId},
};

use super::render::VisibleRenderer;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(super) enum ScriptSummaryError {
    #[error("the script turn has no durable turn/end event")]
    MissingTurnEnd,
    #[error("the script turn has more than one durable turn/end event")]
    DuplicateTurnEnd,
}

pub(super) struct ScriptTurnSummary<'a> {
    message: Option<&'a Message>,
    reason: &'a TurnEndReason,
}

impl ScriptTurnSummary<'_> {
    pub(super) fn exit_code(&self) -> u8 {
        u8::from(!matches!(self.reason, TurnEndReason::Completed))
    }

    pub(super) fn write_stdout<E>(
        &self,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut renderer = VisibleRenderer::new();
        if let Some(message) = self.message {
            for block in message.content() {
                if let ContentBlockKind::Text { text } = block.kind() {
                    renderer.render_fragment(text, None, &mut emit)?;
                }
            }
        }
        renderer.render_fragment("\n", None, emit)
    }

    pub(super) fn write_stderr<E>(
        &self,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        let TurnEndReason::Error { error } = self.reason else {
            return Ok(());
        };
        emit("error [")?;
        let mut renderer = VisibleRenderer::new();
        renderer.render_fragment(error.code(), None, &mut emit)?;
        emit("]: ")?;
        renderer.render_fragment(error.message(), None, &mut emit)?;
        emit("\n")
    }
}

pub(super) fn summarize_turn(
    events: &[SessionEvent],
    start: EventSeq,
    turn: TurnId,
) -> Result<ScriptTurnSummary<'_>, ScriptSummaryError> {
    let mut latest_nonempty = None;
    let mut end_reason = None;
    for event in events
        .iter()
        .filter(|event| event.seq().get() >= start.get())
    {
        match event.kind() {
            EventKind::AssistantMessage {
                turn: event_turn,
                message,
                ..
            } if *event_turn == turn => {
                let has_text = message.content().iter().any(|block| {
                    matches!(block.kind(), ContentBlockKind::Text { text } if !text.is_empty())
                });
                if has_text {
                    latest_nonempty = Some(message);
                }
            }
            EventKind::TurnEnd {
                turn: event_turn,
                reason,
            } if *event_turn == turn => {
                if end_reason.replace(reason).is_some() {
                    return Err(ScriptSummaryError::DuplicateTurnEnd);
                }
            }
            _ => {}
        }
    }
    let reason = end_reason.ok_or(ScriptSummaryError::MissingTurnEnd)?;
    Ok(ScriptTurnSummary {
        message: latest_nonempty,
        reason,
    })
}

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
struct OwnedScriptSummary {
    stdout: String,
    stderr: String,
    exit: u8,
}

#[cfg(test)]
fn summarize_turn_for_test(
    events: &[SessionEvent],
    start: u64,
    turn: TurnId,
) -> Result<OwnedScriptSummary, ScriptSummaryError> {
    let summary = summarize_turn(
        events,
        EventSeq::new(start).map_err(|_| ScriptSummaryError::MissingTurnEnd)?,
        turn,
    )?;
    let mut stdout = String::new();
    summary
        .write_stdout(|chunk| {
            stdout.push_str(chunk);
            Ok::<_, Infallible>(())
        })
        .expect("the test sink is infallible");
    let mut stderr = String::new();
    summary
        .write_stderr(|chunk| {
            stderr.push_str(chunk);
            Ok::<_, Infallible>(())
        })
        .expect("the test sink is infallible");
    Ok(OwnedScriptSummary {
        stdout,
        stderr,
        exit: summary.exit_code(),
    })
}

#[cfg(test)]
mod tests {
    use crate::{
        model::{ContentBlock, LlmFailure, Message},
        session::{EventKind, NewEvent, Session, SurfaceIntent, TurnEndReason, TurnId},
    };

    use super::{ScriptSummaryError, summarize_turn_for_test};

    fn append_assistant_step(
        session: &mut Session,
        turn: TurnId,
        step_number: u64,
        blocks: Vec<ContentBlock>,
    ) {
        let step = crate::session::StepId::new(step_number).unwrap();
        session
            .append(NewEvent::log(EventKind::step_start(turn, step)))
            .unwrap();
        let message = Message::assistant(
            format!("assistant-{step_number}"),
            blocks,
            "mock",
            "mock-model",
        )
        .unwrap();
        session
            .append(NewEvent::surface(
                EventKind::assistant_message(turn, step, message),
                SurfaceIntent::append(),
            ))
            .unwrap();
        session
            .append(NewEvent::log(EventKind::step_end(turn, step)))
            .unwrap();
    }

    fn completed_session(blocks_by_step: Vec<Vec<ContentBlock>>, reason: TurnEndReason) -> Session {
        let mut session = Session::new("script-summary").unwrap();
        let turn = TurnId::new(1).unwrap();
        session
            .append(NewEvent::log(EventKind::turn_start(turn)))
            .unwrap();
        for (index, blocks) in blocks_by_step.into_iter().enumerate() {
            append_assistant_step(
                &mut session,
                turn,
                u64::try_from(index + 1).unwrap(),
                blocks,
            );
        }
        session
            .append(NewEvent::log(EventKind::turn_end(turn, reason)))
            .unwrap();
        session
    }

    #[test]
    fn summary_uses_the_latest_nonempty_assistant_text_without_separators() {
        let session = completed_session(
            vec![
                vec![ContentBlock::text("old").unwrap()],
                vec![
                    ContentBlock::text("new").unwrap(),
                    ContentBlock::reasoning("not stdout").unwrap(),
                    ContentBlock::text(" answer").unwrap(),
                ],
                vec![ContentBlock::text("").unwrap()],
            ],
            TurnEndReason::Completed,
        );

        let summary = summarize_turn_for_test(session.events(), 0, TurnId::new(1).unwrap())
            .expect("the turn is complete");
        assert_eq!(summary.stdout, "new answer\n");
        assert_eq!(summary.stderr, "");
        assert_eq!(summary.exit, 0);
    }

    #[test]
    fn summary_always_adds_one_line_feed_and_sanitizes_text() {
        let already_newline = completed_session(
            vec![vec![ContentBlock::text("answer\n").unwrap()]],
            TurnEndReason::Completed,
        );
        let summary =
            summarize_turn_for_test(already_newline.events(), 0, TurnId::new(1).unwrap()).unwrap();
        assert_eq!(summary.stdout, "answer\n\n");

        let hostile = completed_session(
            vec![vec![ContentBlock::text("a\r\x1b]52;c;x\x07").unwrap()]],
            TurnEndReason::Completed,
        );
        let summary =
            summarize_turn_for_test(hostile.events(), 0, TurnId::new(1).unwrap()).unwrap();
        assert_eq!(summary.stdout, "a\\r\\u{1b}]52;c;x\\u{7}\n");
    }

    #[test]
    fn summary_without_text_is_exactly_one_line_feed() {
        let session = completed_session(Vec::new(), TurnEndReason::Completed);
        let summary =
            summarize_turn_for_test(session.events(), 0, TurnId::new(1).unwrap()).unwrap();
        assert_eq!(summary.stdout.as_bytes(), b"\n");
        assert_eq!(summary.exit, 0);
    }

    #[test]
    fn durable_failure_uses_stderr_and_noncompleted_reasons_exit_one() {
        let failure = LlmFailure::new("unsafe\rmessage", "SERVER\u{202e}").unwrap();
        let session = completed_session(
            vec![vec![ContentBlock::text("partial").unwrap()]],
            TurnEndReason::Error { error: failure },
        );
        let summary =
            summarize_turn_for_test(session.events(), 0, TurnId::new(1).unwrap()).unwrap();
        assert_eq!(summary.stdout, "partial\n");
        assert_eq!(
            summary.stderr,
            "error [SERVER\\u{202e}]: unsafe\\rmessage\n"
        );
        assert_eq!(summary.exit, 1);

        for reason in [
            TurnEndReason::Blocked,
            TurnEndReason::MaxTokens,
            TurnEndReason::Interrupted,
        ] {
            let session = completed_session(Vec::new(), reason);
            let summary =
                summarize_turn_for_test(session.events(), 0, TurnId::new(1).unwrap()).unwrap();
            assert_eq!(
                (
                    summary.stdout.as_str(),
                    summary.stderr.as_str(),
                    summary.exit
                ),
                ("\n", "", 1)
            );
        }
    }

    #[test]
    fn summary_requires_the_matching_durable_turn_end() {
        let mut session = Session::new("missing-turn-end").unwrap();
        session
            .append(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .unwrap();
        assert_eq!(
            summarize_turn_for_test(session.events(), 0, TurnId::new(1).unwrap()),
            Err(ScriptSummaryError::MissingTurnEnd)
        );
    }
}
