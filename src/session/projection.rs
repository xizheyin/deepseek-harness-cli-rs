//! Pure turn/step/tool and model-visible surface projection.

use std::collections::BTreeSet;

use crate::{
    json_value::json_values_equal,
    model::{CallId, Message},
};

use super::{
    EventKind, EventSeq, MAX_SOURCE_EVENT_SEQS, SessionEvent, StepId, SurfaceOp, TurnId,
    codec::event_data_value,
    error::{EventValidationError, SurfaceError, TransitionError},
    event::TOOL_NOT_STARTED,
};

#[derive(Clone, Debug, Eq, PartialEq)]
enum Boundary {
    Idle,
    Turn {
        turn: TurnId,
        next_step: StepId,
    },
    Step {
        turn: TurnId,
        step: StepId,
        pending_calls: Vec<CallId>,
    },
}

/// Read-only summary reconstructed from the committed event prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    open_turn: Option<TurnId>,
    open_step: Option<StepId>,
    next_turn: TurnId,
    pending_calls: Vec<CallId>,
    surface_nodes: Vec<EventSeq>,
    request_header: Option<super::EpochHeader>,
    request_context: Option<super::RequestContext>,
}

impl SessionState {
    /// Currently open turn, if the log ends inside one.
    #[must_use]
    pub fn open_turn(&self) -> Option<TurnId> {
        self.open_turn
    }

    /// Currently open step, if the log ends inside one.
    #[must_use]
    pub fn open_step(&self) -> Option<StepId> {
        self.open_step
    }

    /// Turn number required by the next `turn/start` event.
    #[must_use]
    pub fn next_turn(&self) -> TurnId {
        self.next_turn
    }

    /// Tool calls recorded but not concluded in the current step.
    #[must_use]
    pub fn pending_calls(&self) -> &[CallId] {
        &self.pending_calls
    }

    /// Event sequences on the current model-visible surface.
    #[must_use]
    pub fn surface_nodes(&self) -> &[EventSeq] {
        &self.surface_nodes
    }

    /// Latest canonical full request header, if one has been logged.
    #[must_use]
    pub fn request_header(&self) -> Option<&super::EpochHeader> {
        self.request_header.as_ref()
    }

    /// Latest full route-capacity record, if one has been logged.
    #[must_use]
    pub fn request_context(&self) -> Option<&super::RequestContext> {
        self.request_context.as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Projection {
    next_turn: TurnId,
    boundary: Boundary,
    surface_nodes: Vec<EventSeq>,
    request_header: Option<super::EpochHeader>,
    request_context: Option<super::RequestContext>,
}

impl Projection {
    pub(crate) fn empty() -> Self {
        Self {
            next_turn: TurnId::first(),
            boundary: Boundary::Idle,
            surface_nodes: Vec::new(),
            request_header: None,
            request_context: None,
        }
    }

    /// Validate and apply one candidate to a detached projection clone.
    pub(crate) fn with_event(
        &self,
        event: &SessionEvent,
        committed_events: &[SessionEvent],
    ) -> Result<Self, EventValidationError> {
        event.kind.validate()?;
        let mut next = self.clone();
        next.apply_transition(event, committed_events)?;
        next.apply_surface(event, committed_events)?;
        Ok(next)
    }

    pub(crate) fn state(&self) -> SessionState {
        let (open_turn, open_step, pending_calls) = match &self.boundary {
            Boundary::Idle => (None, None, Vec::new()),
            Boundary::Turn { turn, .. } => (Some(*turn), None, Vec::new()),
            Boundary::Step {
                turn,
                step,
                pending_calls,
                ..
            } => (Some(*turn), Some(*step), pending_calls.clone()),
        };
        SessionState {
            open_turn,
            open_step,
            next_turn: self.next_turn,
            pending_calls,
            surface_nodes: self.surface_nodes.clone(),
            request_header: self.request_header.clone(),
            request_context: self.request_context.clone(),
        }
    }

    pub(crate) fn messages(&self, events: &[SessionEvent]) -> Vec<Message> {
        self.surface_nodes
            .iter()
            .filter_map(|seq| {
                let index = usize::try_from(seq.get()).ok()?;
                let event = events.get(index)?;
                match &event.kind {
                    EventKind::UserMessage { message } | EventKind::ToolResult { message, .. } => {
                        Some(message.clone())
                    }
                    EventKind::AssistantMessage { message, .. }
                        if !message.content().is_empty() =>
                    {
                        Some(message.clone())
                    }
                    _ => None,
                }
            })
            .collect()
    }

    pub(crate) fn request_header(&self) -> Option<&super::EpochHeader> {
        self.request_header.as_ref()
    }

    pub(crate) fn request_context(&self) -> Option<&super::RequestContext> {
        self.request_context.as_ref()
    }

    fn apply_transition(
        &mut self,
        event: &SessionEvent,
        committed_events: &[SessionEvent],
    ) -> Result<(), TransitionError> {
        match &event.kind {
            EventKind::TurnStart { turn } => {
                if let Some(open) = self.open_turn() {
                    return Err(TransitionError::TurnAlreadyOpen {
                        open,
                        attempted: *turn,
                    });
                }
                if *turn != self.next_turn {
                    return Err(TransitionError::WrongNextTurn {
                        expected: self.next_turn,
                        actual: *turn,
                    });
                }
                self.boundary = Boundary::Turn {
                    turn: *turn,
                    next_step: StepId::first(),
                };
            }
            EventKind::TurnEnd { turn, .. } => {
                let open = self.open_turn();
                if open != Some(*turn) {
                    return Err(TransitionError::WrongTurnEnd {
                        open,
                        actual: *turn,
                    });
                }
                if let Boundary::Step { step, .. } = self.boundary {
                    return Err(TransitionError::TurnEndWhileStepOpen { turn: *turn, step });
                }
                self.next_turn = turn
                    .successor()
                    .ok_or(TransitionError::IdentifierExhausted)?;
                self.boundary = Boundary::Idle;
            }
            EventKind::StepStart { turn, step } => match self.boundary.clone() {
                Boundary::Idle => {
                    return Err(TransitionError::StepOutsideTurn {
                        open: None,
                        actual: *turn,
                    });
                }
                Boundary::Step {
                    turn: open_turn,
                    step: open_step,
                    ..
                } => {
                    if open_turn != *turn {
                        return Err(TransitionError::StepOutsideTurn {
                            open: Some(open_turn),
                            actual: *turn,
                        });
                    }
                    return Err(TransitionError::StepAlreadyOpen {
                        open: open_step,
                        attempted: *step,
                    });
                }
                Boundary::Turn {
                    turn: open_turn,
                    next_step,
                } => {
                    if open_turn != *turn {
                        return Err(TransitionError::StepOutsideTurn {
                            open: Some(open_turn),
                            actual: *turn,
                        });
                    }
                    if next_step != *step {
                        return Err(TransitionError::WrongNextStep {
                            turn: *turn,
                            expected: next_step,
                            actual: *step,
                        });
                    }
                    self.boundary = Boundary::Step {
                        turn: *turn,
                        step: *step,
                        pending_calls: Vec::new(),
                    };
                }
            },
            EventKind::StepEnd { turn, step } => {
                self.require_open_step("step/end", *turn, *step)?;
                let next_step = step
                    .successor()
                    .ok_or(TransitionError::IdentifierExhausted)?;
                self.boundary = Boundary::Turn {
                    turn: *turn,
                    next_step,
                };
            }
            EventKind::AssistantChunk { turn, step, .. }
            | EventKind::AssistantMessage { turn, step, .. }
            | EventKind::ToolCall { turn, step, .. } => {
                self.require_open_step(event.kind.event_type_static(), *turn, *step)?;
                if let EventKind::ToolCall { call_id, .. } = &event.kind {
                    if let Boundary::Step { pending_calls, .. } = &mut self.boundary {
                        if !pending_calls.contains(call_id) {
                            pending_calls.push(call_id.clone());
                        }
                    }
                }
            }
            EventKind::ToolResult {
                turn,
                step,
                message,
                error,
                ..
            } => {
                if matches!(event.surface_op, Some(SurfaceOp::Replace(_))) {
                    if self.open_turn().is_none() {
                        return Err(TransitionError::EventOutsideTurn {
                            event_type: "tool/result replacement",
                        });
                    }
                } else {
                    self.require_open_step("tool/result", *turn, *step)?;
                    let call_id = message.validate_tool_result().map_err(|_| {
                        TransitionError::MissingToolCall {
                            call_id: CallId::new("invalid"),
                        }
                    })?;
                    let synthetic_not_started = message.tool_result_is_error()
                        && error
                            .as_ref()
                            .is_some_and(|failure| failure.code == TOOL_NOT_STARTED);
                    let Boundary::Step { pending_calls, .. } = &mut self.boundary else {
                        return Err(self.wrong_open_step("tool/result", *turn, *step));
                    };
                    let Some(index) = pending_calls.iter().position(|pending| pending == call_id)
                    else {
                        if synthetic_not_started {
                            return Ok(());
                        }
                        return Err(TransitionError::MissingToolCall {
                            call_id: call_id.clone(),
                        });
                    };
                    pending_calls.remove(index);
                }
            }
            EventKind::RequestHeader { header, .. } => {
                if self.open_turn().is_none() {
                    return Err(TransitionError::EventOutsideTurn {
                        event_type: event.kind.event_type_static(),
                    });
                }
                self.request_header = Some(header.canonicalized());
            }
            EventKind::RequestContext { context } => {
                if self.open_turn().is_none() {
                    return Err(TransitionError::EventOutsideTurn {
                        event_type: event.kind.event_type_static(),
                    });
                }
                self.request_context = Some(context.clone());
            }
            EventKind::LlmRetry { retry } => {
                self.validate_retry(retry, committed_events)?;
            }
            EventKind::LlmRetryStarted { started } => {
                self.validate_retry_started(started, committed_events)?;
            }
            EventKind::TodoWrite { .. } => {
                if self.open_turn().is_none() {
                    return Err(TransitionError::EventOutsideTurn {
                        event_type: event.kind.event_type_static(),
                    });
                }
            }
            EventKind::UserMessage { .. } | EventKind::EndSeed | EventKind::Unknown { .. } => {}
        }
        Ok(())
    }

    fn apply_surface(
        &mut self,
        event: &SessionEvent,
        committed_events: &[SessionEvent],
    ) -> Result<(), SurfaceError> {
        if !event.kind.is_surface_eligible() {
            if event.surface_op.is_some() || event.source_event_seqs.is_some() {
                return Err(SurfaceError::MetadataOnIneligibleEvent {
                    event_type: event.kind.event_type().to_owned(),
                });
            }
            return Ok(());
        }
        let operation =
            event
                .surface_op
                .as_ref()
                .ok_or_else(|| SurfaceError::MissingOperation {
                    event_type: event.kind.event_type().to_owned(),
                })?;
        self.validate_sources(event)?;
        match operation {
            SurfaceOp::Append(_) => self.surface_nodes.push(event.seq),
            SurfaceOp::Replace(replacement) => {
                let start_index = self
                    .surface_nodes
                    .iter()
                    .position(|seq| *seq == replacement.start)
                    .ok_or(SurfaceError::StartNotFound(replacement.start))?;
                let end_index = self
                    .surface_nodes
                    .iter()
                    .position(|seq| *seq == replacement.end)
                    .ok_or(SurfaceError::EndNotFound(replacement.end))?;
                if start_index > end_index {
                    return Err(SurfaceError::ReversedRange {
                        start: replacement.start,
                        end: replacement.end,
                    });
                }
                let shadowed = &self.surface_nodes[start_index..=end_index];
                let sources = event.source_event_seqs.as_deref().unwrap_or_default();
                for shadowed_seq in shadowed {
                    if !sources.contains(shadowed_seq) {
                        return Err(SurfaceError::MissingShadowedSource(*shadowed_seq));
                    }
                }
                if matches!(event.kind, EventKind::ToolResult { .. }) {
                    self.validate_tool_result_rewrite(event, shadowed, committed_events)?;
                }
                self.surface_nodes
                    .splice(start_index..=end_index, [event.seq]);
            }
        }
        Ok(())
    }

    fn validate_retry(
        &self,
        retry: &super::LlmRetryEvent,
        history: &[SessionEvent],
    ) -> Result<(), TransitionError> {
        self.require_open_step("llm/retry", retry.turn(), retry.step())?;
        let expected_provider = self
            .request_header
            .as_ref()
            .map(|header| header.config.provider())
            .unwrap_or_default();
        if retry.provider() != expected_provider {
            return Err(TransitionError::RetryProviderMismatch {
                expected: expected_provider.to_owned(),
                actual: retry.provider().to_owned(),
            });
        }

        let prior = history
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::LlmRetry { retry: prior }
                    if prior.turn() == retry.turn()
                        && prior.step() == retry.step()
                        && prior.provider() == retry.provider()
                        && prior.policy_key() == retry.policy_key() =>
                {
                    Some(prior)
                }
                _ => None,
            })
            .next_back();
        let expected = match prior {
            Some(prior) => prior
                .retry()
                .successor()
                .ok_or(TransitionError::IdentifierExhausted)?,
            None => super::RetryNumber::first(),
        };
        if retry.retry() != expected {
            return Err(TransitionError::WrongRetryNumber {
                expected,
                actual: retry.retry(),
            });
        }
        if let Some(prior) = prior {
            if prior.retry_id() != retry.retry_id() {
                return Err(TransitionError::RetryChainIdMismatch {
                    expected: prior.retry_id().clone(),
                    actual: retry.retry_id().clone(),
                });
            }
        } else if history.iter().any(|event| match &event.kind {
            EventKind::LlmRetry { retry: prior } => prior.retry_id() == retry.retry_id(),
            EventKind::LlmRetryStarted { started } => started.retry_id() == retry.retry_id(),
            _ => false,
        }) {
            return Err(TransitionError::RetryIdAlreadyOwned {
                retry_id: retry.retry_id().clone(),
            });
        }
        Ok(())
    }

    fn validate_retry_started(
        &self,
        started: &super::LlmRetryStartedEvent,
        history: &[SessionEvent],
    ) -> Result<(), TransitionError> {
        // The upstream retry companion correlates this event with its durable
        // schedule. It does not require the referenced step to still be open:
        // a delayed callback may publish `started` after `step/end`.
        let scheduled = history.iter().any(|event| match &event.kind {
            EventKind::LlmRetry { retry } => {
                retry.retry_id() == started.retry_id()
                    && retry.retry() == started.retry()
                    && retry.turn() == started.turn()
                    && retry.step() == started.step()
            }
            _ => false,
        });
        if !scheduled {
            return Err(TransitionError::RetryStartedWithoutSchedule {
                retry_id: started.retry_id().clone(),
                retry: started.retry(),
            });
        }
        let already_started = history.iter().any(|event| match &event.kind {
            EventKind::LlmRetryStarted { started: prior } => {
                prior.retry_id() == started.retry_id() && prior.retry() == started.retry()
            }
            _ => false,
        });
        if already_started {
            return Err(TransitionError::RetryStartedTwice {
                retry_id: started.retry_id().clone(),
                retry: started.retry(),
            });
        }
        Ok(())
    }

    fn validate_sources(&self, event: &SessionEvent) -> Result<(), SurfaceError> {
        let Some(sources) = &event.source_event_seqs else {
            return Ok(());
        };
        if sources.len() > MAX_SOURCE_EVENT_SEQS {
            return Err(SurfaceError::TooManySources {
                maximum: MAX_SOURCE_EVENT_SEQS,
                actual: sources.len(),
            });
        }
        if sources.is_empty() && !matches!(event.kind, EventKind::AssistantMessage { .. }) {
            return Err(SurfaceError::EmptySources);
        }
        let mut unique = BTreeSet::new();
        for source in sources {
            if !unique.insert(*source) {
                return Err(SurfaceError::DuplicateSource(*source));
            }
        }
        for source in sources {
            if *source >= event.seq {
                return Err(SurfaceError::SourceNotEarlier {
                    source_seq: *source,
                    current: event.seq,
                });
            }
        }
        Ok(())
    }

    fn validate_tool_result_rewrite(
        &self,
        replacement: &SessionEvent,
        shadowed: &[EventSeq],
        events: &[SessionEvent],
    ) -> Result<(), SurfaceError> {
        if shadowed.len() != 1 {
            return Err(SurfaceError::ToolResultMultipleTargets);
        }
        let index =
            usize::try_from(shadowed[0].get()).map_err(|_| SurfaceError::ToolResultWrongTarget)?;
        let Some(original) = events.get(index) else {
            return Err(SurfaceError::ToolResultWrongTarget);
        };
        if !matches!(original.kind, EventKind::ToolResult { .. })
            || !matches!(replacement.kind, EventKind::ToolResult { .. })
        {
            return Err(SurfaceError::ToolResultWrongTarget);
        }
        let mut original_data =
            event_data_value(original).map_err(|_| SurfaceError::ToolResultChangedIdentity)?;
        let mut replacement_data =
            event_data_value(replacement).map_err(|_| SurfaceError::ToolResultChangedIdentity)?;
        if !mask_tool_result_content(&mut original_data)
            || !mask_tool_result_content(&mut replacement_data)
            || !json_values_equal(&original_data, &replacement_data)
        {
            return Err(SurfaceError::ToolResultChangedIdentity);
        }
        Ok(())
    }

    fn open_turn(&self) -> Option<TurnId> {
        match self.boundary {
            Boundary::Idle => None,
            Boundary::Turn { turn, .. } | Boundary::Step { turn, .. } => Some(turn),
        }
    }

    fn open_step(&self) -> Option<StepId> {
        match self.boundary {
            Boundary::Step { step, .. } => Some(step),
            Boundary::Idle | Boundary::Turn { .. } => None,
        }
    }

    fn require_open_step(
        &self,
        event_type: &'static str,
        turn: TurnId,
        step: StepId,
    ) -> Result<(), TransitionError> {
        if self.open_turn() != Some(turn) || self.open_step() != Some(step) {
            return Err(self.wrong_open_step(event_type, turn, step));
        }
        Ok(())
    }

    fn wrong_open_step(
        &self,
        event_type: &'static str,
        turn: TurnId,
        step: StepId,
    ) -> TransitionError {
        TransitionError::WrongOpenStep {
            event_type,
            open_turn: self.open_turn(),
            open_step: self.open_step(),
            actual_turn: turn,
            actual_step: step,
        }
    }
}

impl EventKind {
    fn event_type_static(&self) -> &'static str {
        match self {
            Self::TurnStart { .. } => "turn/start",
            Self::TurnEnd { .. } => "turn/end",
            Self::StepStart { .. } => "step/start",
            Self::StepEnd { .. } => "step/end",
            Self::UserMessage { .. } => "user/message",
            Self::AssistantChunk { .. } => "assistant/chunk",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
            Self::TodoWrite { .. } => "todo/write",
            Self::RequestHeader { .. } => "request/header",
            Self::RequestContext { .. } => "request/context",
            Self::LlmRetry { .. } => "llm/retry",
            Self::LlmRetryStarted { .. } => "llm/retry-started",
            Self::EndSeed => "session/end-seed",
            Self::Unknown { .. } => "unknown",
        }
    }
}

fn mask_tool_result_content(data: &mut serde_json::Value) -> bool {
    let Some(block) = data
        .as_object_mut()
        .and_then(|data| data.get_mut("message"))
        .and_then(serde_json::Value::as_object_mut)
        .and_then(|message| message.get_mut("content"))
        .and_then(serde_json::Value::as_array_mut)
        .and_then(|content| (content.len() == 1).then(|| &mut content[0]))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return false;
    };
    block.insert("content".to_owned(), serde_json::Value::Null);
    true
}
