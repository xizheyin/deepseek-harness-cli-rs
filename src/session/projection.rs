//! Pure turn/step/tool and model-visible surface projection.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::{
    json_value::json_values_equal,
    model::{CallId, Message},
};

use super::{
    ApprovalOutcome, ApprovalRequestId, EventKind, EventSeq, MAX_SOURCE_EVENT_SEQS, SessionEvent,
    StepId, SurfaceOp, TurnId,
    codec::event_data_value,
    error::{EventValidationError, SurfaceError, TransitionError},
    event::{RECOVERY_TOOL_RESULT_ID_PREFIX, TOOL_NOT_STARTED},
    recovery::RecoveryAdmission,
};

const MAX_DURABLE_TOOL_CALLS_PER_STEP: usize = 64;

/// Closed validation boundary for the released in-memory format and the
/// recoverable durable format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ValidationPolicy {
    MemoryCompatible,
    DurableStrict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ValidationAdmission {
    Ordinary,
    HistoricalScan,
}

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
        declared_calls: Vec<DurableDeclaredCall>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DurableDeclaredCall {
    id: CallId,
    name: String,
    declaration: Message,
    block_index: usize,
    intent_seq: Option<EventSeq>,
    approval: Option<DurableApproval>,
    result_seen: bool,
}

impl DurableDeclaredCall {
    fn arguments(&self) -> Option<&str> {
        let block = self.declaration.content().get(self.block_index)?;
        let crate::model::ContentBlockKind::ToolCall { arguments, .. } = block.kind() else {
            return None;
        };
        Some(arguments)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DurableApproval {
    Pending {
        id: ApprovalRequestId,
    },
    Decided {
        id: ApprovalRequestId,
        outcome: ApprovalOutcome,
    },
}

/// Bounded durable facts needed to derive one deterministic recovery suffix.
///
/// This deliberately omits tool arguments and message bodies. Recovery never
/// re-dispatches a tool; it only needs identity, ordering, durable intent, and
/// approval/result state to close an interrupted step truthfully.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoverySnapshot {
    turn: Option<TurnId>,
    step: Option<StepId>,
    calls: Vec<RecoveryCall>,
}

impl RecoverySnapshot {
    pub(crate) fn turn(&self) -> Option<TurnId> {
        self.turn
    }

    pub(crate) fn step(&self) -> Option<StepId> {
        self.step
    }

    pub(crate) fn calls(&self) -> &[RecoveryCall] {
        &self.calls
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryCall {
    id: CallId,
    name: String,
    intent_seq: Option<EventSeq>,
    approval: RecoveryApproval,
    result_seen: bool,
}

impl RecoveryCall {
    pub(crate) fn id(&self) -> &CallId {
        &self.id
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn intent_seq(&self) -> Option<EventSeq> {
        self.intent_seq
    }

    pub(crate) fn approval(&self) -> &RecoveryApproval {
        &self.approval
    }

    pub(crate) fn result_seen(&self) -> bool {
        self.result_seen
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryApproval {
    None,
    Pending {
        id: ApprovalRequestId,
    },
    Decided {
        id: ApprovalRequestId,
        outcome: ApprovalOutcome,
    },
}

/// Read-only summary reconstructed from the committed event prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionState {
    open_turn: Option<TurnId>,
    open_step: Option<StepId>,
    next_turn: TurnId,
    pending_calls: Vec<CallId>,
    pending_approvals: Vec<ApprovalRequestId>,
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

    /// Approval questions that have no matching durable decision yet.
    #[must_use]
    pub fn pending_approvals(&self) -> &[ApprovalRequestId] {
        &self.pending_approvals
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
    policy: ValidationPolicy,
    next_turn: TurnId,
    boundary: Boundary,
    surface_nodes: Arc<Vec<SurfaceNode>>,
    request_header: Option<super::EpochHeader>,
    request_context: Option<super::RequestContext>,
    pending_approvals: Vec<ApprovalRequestId>,
    owned_approval_ids: Arc<BTreeSet<ApprovalRequestId>>,
    retry_chains: Arc<BTreeMap<RetryChainKey, RetryChainState>>,
    retry_schedules: Arc<BTreeMap<(super::RetryId, super::RetryNumber), RetryScheduleState>>,
}

/// One current model-visible node.
///
/// Keeping the shallow message handle here breaks the old `seq == Vec index`
/// assumption. Durable sessions can therefore retire the historical event row
/// after it is journaled without losing the next provider request.
#[derive(Clone, Debug, Eq, PartialEq)]
struct SurfaceNode {
    seq: EventSeq,
    kind: SurfaceNodeKind,
    message: Option<Message>,
    tool_result_identity: Option<Arc<serde_json::Value>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceNodeKind {
    User,
    Assistant,
    ToolResult,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct RetryChainKey {
    turn: TurnId,
    step: StepId,
    provider: String,
    policy_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetryChainState {
    retry_id: super::RetryId,
    latest: super::RetryNumber,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RetryScheduleState {
    turn: TurnId,
    step: StepId,
    started: bool,
}

impl Projection {
    pub(crate) fn empty(policy: ValidationPolicy) -> Self {
        Self {
            policy,
            next_turn: TurnId::first(),
            boundary: Boundary::Idle,
            surface_nodes: Arc::new(Vec::new()),
            request_header: None,
            request_context: None,
            pending_approvals: Vec::new(),
            owned_approval_ids: Arc::new(BTreeSet::new()),
            retry_chains: Arc::new(BTreeMap::new()),
            retry_schedules: Arc::new(BTreeMap::new()),
        }
    }

    /// Validate and apply one candidate to a detached projection clone.
    pub(crate) fn with_event(&self, event: &SessionEvent) -> Result<Self, EventValidationError> {
        event.kind.validate()?;
        let mut next = self.clone();
        next.apply_transition(event, ValidationAdmission::Ordinary)?;
        next.apply_surface(event)?;
        Ok(next)
    }

    /// Apply one ordinary cold-scanned row without cloning the active index.
    ///
    /// A cold scanner discards the entire candidate on a semantic failure, so
    /// it does not need live append's detached rollback clone. This keeps a
    /// long valid journal linear in its event count.
    pub(crate) fn apply_scanned_event(
        &mut self,
        event: &SessionEvent,
    ) -> Result<(), EventValidationError> {
        event.kind.validate()?;
        self.apply_transition(event, ValidationAdmission::Ordinary)?;
        self.apply_surface(event)?;
        Ok(())
    }

    /// Apply one event admitted by recovery's private exact-match cursor.
    pub(super) fn apply_recovery_admission(
        &mut self,
        admission: RecoveryAdmission<'_>,
    ) -> Result<(), EventValidationError> {
        let event = admission.event();
        event.kind.validate()?;
        self.apply_transition(event, ValidationAdmission::HistoricalScan)?;
        self.apply_surface(event)?;
        Ok(())
    }

    pub(crate) fn recovery_snapshot(&self) -> RecoverySnapshot {
        match &self.boundary {
            Boundary::Idle => RecoverySnapshot {
                turn: None,
                step: None,
                calls: Vec::new(),
            },
            Boundary::Turn { turn, .. } => RecoverySnapshot {
                turn: Some(*turn),
                step: None,
                calls: Vec::new(),
            },
            Boundary::Step {
                turn,
                step,
                declared_calls,
                ..
            } => RecoverySnapshot {
                turn: Some(*turn),
                step: Some(*step),
                calls: declared_calls
                    .iter()
                    .map(|call| RecoveryCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        intent_seq: call.intent_seq,
                        approval: match &call.approval {
                            None => RecoveryApproval::None,
                            Some(DurableApproval::Pending { id }) => {
                                RecoveryApproval::Pending { id: id.clone() }
                            }
                            Some(DurableApproval::Decided { id, outcome }) => {
                                RecoveryApproval::Decided {
                                    id: id.clone(),
                                    outcome: *outcome,
                                }
                            }
                        },
                        result_seen: call.result_seen,
                    })
                    .collect(),
            },
        }
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
            pending_approvals: self.pending_approvals.clone(),
            surface_nodes: self.surface_nodes.iter().map(|node| node.seq).collect(),
            request_header: self.request_header.clone(),
            request_context: self.request_context.clone(),
        }
    }

    pub(crate) fn messages(&self) -> Vec<Message> {
        self.surface_nodes
            .iter()
            .filter_map(|node| node.message.clone())
            .collect()
    }

    pub(crate) fn has_unresolved_surface_tool_calls(&self) -> bool {
        let mut unresolved = std::collections::BTreeMap::<CallId, usize>::new();
        for node in self.surface_nodes.iter() {
            let Some(message) = &node.message else {
                continue;
            };
            match node.kind {
                SurfaceNodeKind::Assistant => {
                    for block in message.content() {
                        if let crate::model::ContentBlockKind::ToolCall { id, .. } = block.kind() {
                            *unresolved.entry(id.clone()).or_default() += 1;
                        }
                    }
                }
                SurfaceNodeKind::ToolResult => {
                    let Ok(tool_call_id) = message.validate_tool_result() else {
                        return true;
                    };
                    let remove = unresolved.get_mut(tool_call_id).is_some_and(|count| {
                        *count -= 1;
                        *count == 0
                    });
                    if remove {
                        unresolved.remove(tool_call_id);
                    }
                }
                SurfaceNodeKind::User => {}
            }
        }
        !unresolved.is_empty()
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
        admission: ValidationAdmission,
    ) -> Result<(), TransitionError> {
        if self.policy == ValidationPolicy::DurableStrict
            && admission == ValidationAdmission::Ordinary
            && matches!(
                &event.kind,
                EventKind::EndSeed
                    | EventKind::TurnEnd {
                        reason: super::TurnEndReason::Interrupted,
                        ..
                    }
            )
        {
            return Err(TransitionError::DurableRecoveryEventNotAllowed {
                event_type: event.kind.event_type_static(),
            });
        }
        if self.policy == ValidationPolicy::DurableStrict
            && admission == ValidationAdmission::Ordinary
            && matches!(
                &event.kind,
                EventKind::ToolResult { message, .. }
                    if message.id().as_str().starts_with(RECOVERY_TOOL_RESULT_ID_PREFIX)
            )
        {
            return Err(TransitionError::DurableRecoveryEventNotAllowed {
                event_type: "tool/result",
            });
        }
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
                if let Some(approval_id) = self.pending_approvals.first() {
                    return Err(TransitionError::ApprovalStillPending {
                        event_type: "turn/end",
                        approval_id: approval_id.clone(),
                    });
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
                        declared_calls: Vec::new(),
                    };
                }
            },
            EventKind::StepEnd { turn, step } => {
                self.require_open_step("step/end", *turn, *step)?;
                if self.policy == ValidationPolicy::DurableStrict {
                    let Boundary::Step { declared_calls, .. } = &self.boundary else {
                        return Err(self.wrong_open_step("step/end", *turn, *step));
                    };
                    if let Some(call) = declared_calls.iter().find(|call| !call.result_seen) {
                        return Err(TransitionError::DurableCallStillPending {
                            event_type: "step/end",
                            call_id: call.id.clone(),
                        });
                    }
                }
                if let Some(approval_id) = self.pending_approvals.first() {
                    return Err(TransitionError::ApprovalStillPending {
                        event_type: "step/end",
                        approval_id: approval_id.clone(),
                    });
                }
                let next_step = step
                    .successor()
                    .ok_or(TransitionError::IdentifierExhausted)?;
                self.boundary = Boundary::Turn {
                    turn: *turn,
                    next_step,
                };
            }
            EventKind::AssistantChunk { turn, step, .. } => {
                self.require_open_step(event.kind.event_type_static(), *turn, *step)?;
            }
            EventKind::AssistantMessage {
                turn,
                step,
                message,
                ..
            } => {
                self.require_open_step(event.kind.event_type_static(), *turn, *step)?;
                self.register_durable_declarations(message)?;
            }
            EventKind::ToolCall {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => {
                self.require_open_step(event.kind.event_type_static(), *turn, *step)?;
                self.promote_durable_call(event.seq, call_id, name, arguments)?;
                if let Boundary::Step { pending_calls, .. } = &mut self.boundary {
                    if !pending_calls.contains(call_id) {
                        pending_calls.push(call_id.clone());
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
                    self.resolve_durable_call(event, message, error.as_ref(), admission)?;
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
                self.apply_retry(retry)?;
            }
            EventKind::LlmRetryStarted { started } => {
                self.apply_retry_started(started)?;
            }
            EventKind::ApprovalAsked { asked } => {
                if self.open_turn().is_none() {
                    return Err(TransitionError::EventOutsideTurn {
                        event_type: event.kind.event_type_static(),
                    });
                }
                if self.pending_approvals.contains(asked.id()) {
                    return Err(TransitionError::ApprovalIdAlreadyPending {
                        approval_id: asked.id().clone(),
                    });
                }
                if self.owned_approval_ids.contains(asked.id()) {
                    return Err(TransitionError::ApprovalIdAlreadyOwned {
                        approval_id: asked.id().clone(),
                    });
                }
                self.ask_durable_approval(asked)?;
                Arc::make_mut(&mut self.owned_approval_ids).insert(asked.id().clone());
                self.pending_approvals.push(asked.id().clone());
            }
            EventKind::ApprovalDecided { decided } => {
                if self.open_turn().is_none() {
                    return Err(TransitionError::EventOutsideTurn {
                        event_type: event.kind.event_type_static(),
                    });
                }
                let Some(index) = self
                    .pending_approvals
                    .iter()
                    .position(|pending| pending == decided.id())
                else {
                    return Err(TransitionError::ApprovalDecisionWithoutRequest {
                        approval_id: decided.id().clone(),
                    });
                };
                self.decide_durable_approval(decided)?;
                self.pending_approvals.remove(index);
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

    fn apply_surface(&mut self, event: &SessionEvent) -> Result<(), SurfaceError> {
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
            SurfaceOp::Append(_) => {
                Arc::make_mut(&mut self.surface_nodes).push(Self::surface_node(event)?);
            }
            SurfaceOp::Replace(replacement) => {
                let start_index = self
                    .surface_nodes
                    .iter()
                    .position(|node| node.seq == replacement.start)
                    .ok_or(SurfaceError::StartNotFound(replacement.start))?;
                let end_index = self
                    .surface_nodes
                    .iter()
                    .position(|node| node.seq == replacement.end)
                    .ok_or(SurfaceError::EndNotFound(replacement.end))?;
                if start_index > end_index {
                    return Err(SurfaceError::ReversedRange {
                        start: replacement.start,
                        end: replacement.end,
                    });
                }
                let shadowed = &self.surface_nodes[start_index..=end_index];
                let sources = event.source_event_seqs.as_deref().unwrap_or_default();
                for shadowed_node in shadowed {
                    if !sources.contains(&shadowed_node.seq) {
                        return Err(SurfaceError::MissingShadowedSource(shadowed_node.seq));
                    }
                }
                if matches!(event.kind, EventKind::ToolResult { .. }) {
                    self.validate_tool_result_rewrite(event, shadowed)?;
                }
                Arc::make_mut(&mut self.surface_nodes)
                    .splice(start_index..=end_index, [Self::surface_node(event)?]);
            }
        }
        Ok(())
    }

    fn surface_node(event: &SessionEvent) -> Result<SurfaceNode, SurfaceError> {
        let (kind, message) = match &event.kind {
            EventKind::UserMessage { message } => (SurfaceNodeKind::User, Some(message.clone())),
            EventKind::ToolResult { message, .. } => {
                (SurfaceNodeKind::ToolResult, Some(message.clone()))
            }
            EventKind::AssistantMessage { message, .. } if !message.content().is_empty() => {
                (SurfaceNodeKind::Assistant, Some(message.clone()))
            }
            EventKind::AssistantMessage { .. } => (SurfaceNodeKind::Assistant, None),
            _ => return Err(SurfaceError::ToolResultWrongTarget),
        };
        let tool_result_identity = if matches!(event.kind, EventKind::ToolResult { .. }) {
            let mut identity =
                event_data_value(event).map_err(|_| SurfaceError::ToolResultChangedIdentity)?;
            if !mask_tool_result_content(&mut identity) {
                return Err(SurfaceError::ToolResultChangedIdentity);
            }
            Some(Arc::new(identity))
        } else {
            None
        };
        Ok(SurfaceNode {
            seq: event.seq,
            kind,
            message,
            tool_result_identity,
        })
    }

    fn register_durable_declarations(&mut self, message: &Message) -> Result<(), TransitionError> {
        if self.policy != ValidationPolicy::DurableStrict {
            return Ok(());
        }
        let Boundary::Step { declared_calls, .. } = &mut self.boundary else {
            return Ok(());
        };
        for (block_index, block) in message.content().iter().enumerate() {
            let crate::model::ContentBlockKind::ToolCall { id, name, .. } = block.kind() else {
                continue;
            };
            validate_durable_call_identity(id, name)?;
            if declared_calls.len() >= MAX_DURABLE_TOOL_CALLS_PER_STEP {
                return Err(TransitionError::TooManyDurableToolCalls {
                    maximum: MAX_DURABLE_TOOL_CALLS_PER_STEP,
                });
            }
            if declared_calls.iter().any(|call| call.id == *id) {
                return Err(TransitionError::DuplicateDurableToolCall {
                    call_id: id.clone(),
                });
            }
            declared_calls.push(DurableDeclaredCall {
                id: id.clone(),
                name: name.clone(),
                declaration: message.clone(),
                block_index,
                intent_seq: None,
                approval: None,
                result_seen: false,
            });
        }
        Ok(())
    }

    fn promote_durable_call(
        &mut self,
        seq: EventSeq,
        call_id: &CallId,
        name: &str,
        arguments: &str,
    ) -> Result<(), TransitionError> {
        if self.policy != ValidationPolicy::DurableStrict {
            return Ok(());
        }
        validate_durable_call_identity(call_id, name)?;
        let Boundary::Step { declared_calls, .. } = &mut self.boundary else {
            return Ok(());
        };
        let Some(call) = declared_calls
            .iter_mut()
            .find(|call| call.intent_seq.is_none() && !call.result_seen)
        else {
            return Err(TransitionError::DurableToolCallWithoutDeclaration {
                call_id: call_id.clone(),
            });
        };
        if call.id != *call_id
            || call.name != name
            || call
                .arguments()
                .is_none_or(|declared| declared != arguments)
        {
            return Err(TransitionError::DurableToolCallMismatch {
                expected: call.id.clone(),
                actual: call_id.clone(),
            });
        }
        call.intent_seq = Some(seq);
        Ok(())
    }

    fn ask_durable_approval(
        &mut self,
        asked: &super::ApprovalAskedEvent,
    ) -> Result<(), TransitionError> {
        if self.policy != ValidationPolicy::DurableStrict {
            return Ok(());
        }
        if let Some(pending) = self.pending_approvals.first() {
            return Err(TransitionError::MultipleDurableApprovals {
                pending: pending.clone(),
            });
        }
        let Some(call_id) = asked.call_id() else {
            return Err(TransitionError::DurableApprovalWithoutCall);
        };
        let Boundary::Step { declared_calls, .. } = &mut self.boundary else {
            return Err(TransitionError::DurableApprovalCallMismatch {
                call_id: call_id.clone(),
            });
        };
        let Some(call) = declared_calls
            .iter_mut()
            .find(|call| call.id == *call_id && call.intent_seq.is_some() && !call.result_seen)
        else {
            return Err(TransitionError::DurableApprovalCallMismatch {
                call_id: call_id.clone(),
            });
        };
        if call.name != asked.tool_name() {
            return Err(TransitionError::DurableApprovalToolMismatch {
                expected: call.name.clone(),
                actual: asked.tool_name().to_owned(),
            });
        }
        if call.approval.is_some() {
            return Err(TransitionError::DurableApprovalRepeated {
                call_id: call_id.clone(),
            });
        }
        call.approval = Some(DurableApproval::Pending {
            id: asked.id().clone(),
        });
        Ok(())
    }

    fn decide_durable_approval(
        &mut self,
        decided: &super::ApprovalDecidedEvent,
    ) -> Result<(), TransitionError> {
        if self.policy != ValidationPolicy::DurableStrict {
            return Ok(());
        }
        let Boundary::Step { declared_calls, .. } = &mut self.boundary else {
            return Err(TransitionError::DurableApprovalDecisionMismatch {
                approval_id: decided.id().clone(),
            });
        };
        let Some(call) = declared_calls.iter_mut().find(|call| {
            matches!(
                &call.approval,
                Some(DurableApproval::Pending { id }) if id == decided.id()
            )
        }) else {
            return Err(TransitionError::DurableApprovalDecisionMismatch {
                approval_id: decided.id().clone(),
            });
        };
        call.approval = Some(DurableApproval::Decided {
            id: decided.id().clone(),
            outcome: decided.outcome(),
        });
        Ok(())
    }

    fn resolve_durable_call(
        &mut self,
        event: &SessionEvent,
        message: &Message,
        error: Option<&super::ToolFailure>,
        admission: ValidationAdmission,
    ) -> Result<(), TransitionError> {
        if self.policy != ValidationPolicy::DurableStrict {
            return Ok(());
        }
        let call_id = message.validate_tool_result().map_err(|_| {
            TransitionError::DurableToolResultMismatch {
                call_id: CallId::new("invalid"),
            }
        })?;
        let Boundary::Step { declared_calls, .. } = &mut self.boundary else {
            return Err(TransitionError::DurableToolResultMismatch {
                call_id: call_id.clone(),
            });
        };
        let Some(call) = declared_calls.iter_mut().find(|call| call.id == *call_id) else {
            return Err(TransitionError::DurableToolResultMismatch {
                call_id: call_id.clone(),
            });
        };
        if call.result_seen {
            return Err(TransitionError::DuplicateDurableToolResult {
                call_id: call_id.clone(),
            });
        }
        if matches!(call.approval, Some(DurableApproval::Pending { .. })) {
            return Err(TransitionError::DurableToolResultBeforeDecision {
                call_id: call_id.clone(),
            });
        }
        match call.intent_seq {
            Some(intent_seq) => {
                if event.source_event_seqs() != Some([intent_seq].as_slice()) {
                    return Err(TransitionError::DurableToolResultWrongSource {
                        call_id: call_id.clone(),
                    });
                }
            }
            None => {
                let canonical_not_started = event.source_event_seqs().is_none()
                    && message.tool_result_is_error()
                    && error.is_some_and(|failure| failure.code == TOOL_NOT_STARTED);
                if admission == ValidationAdmission::Ordinary && canonical_not_started {
                    return Err(TransitionError::DurableRecoveryEventNotAllowed {
                        event_type: "tool/result",
                    });
                }
                if !canonical_not_started {
                    return Err(TransitionError::DurableToolResultWithoutIntent {
                        call_id: call_id.clone(),
                    });
                }
            }
        }
        if let Some(DurableApproval::Decided { id, outcome }) = &call.approval {
            let expected = match outcome {
                ApprovalOutcome::AllowedOnce => None,
                ApprovalOutcome::Rejected => Some("APPROVAL_REJECTED"),
                ApprovalOutcome::Cancelled => Some("APPROVAL_CANCELLED"),
                ApprovalOutcome::Unavailable => Some("APPROVAL_UNAVAILABLE"),
            };
            if let Some(expected) = expected {
                if !message.tool_result_is_error()
                    || error.is_none_or(|failure| failure.code != expected)
                {
                    return Err(TransitionError::DurableApprovalResultMismatch {
                        approval_id: id.clone(),
                        call_id: call_id.clone(),
                    });
                }
            }
        }
        call.result_seen = true;
        Ok(())
    }

    fn apply_retry(&mut self, retry: &super::LlmRetryEvent) -> Result<(), TransitionError> {
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

        let key = RetryChainKey {
            turn: retry.turn(),
            step: retry.step(),
            provider: retry.provider().to_owned(),
            policy_key: retry.policy_key().to_owned(),
        };
        let prior = self.retry_chains.get(&key);
        let expected = match prior {
            Some(prior) => prior
                .latest
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
            if &prior.retry_id != retry.retry_id() {
                return Err(TransitionError::RetryChainIdMismatch {
                    expected: prior.retry_id.clone(),
                    actual: retry.retry_id().clone(),
                });
            }
        } else if self
            .retry_schedules
            .keys()
            .any(|(retry_id, _)| retry_id == retry.retry_id())
        {
            return Err(TransitionError::RetryIdAlreadyOwned {
                retry_id: retry.retry_id().clone(),
            });
        }
        Arc::make_mut(&mut self.retry_chains).insert(
            key,
            RetryChainState {
                retry_id: retry.retry_id().clone(),
                latest: retry.retry(),
            },
        );
        Arc::make_mut(&mut self.retry_schedules).insert(
            (retry.retry_id().clone(), retry.retry()),
            RetryScheduleState {
                turn: retry.turn(),
                step: retry.step(),
                started: false,
            },
        );
        Ok(())
    }

    fn apply_retry_started(
        &mut self,
        started: &super::LlmRetryStartedEvent,
    ) -> Result<(), TransitionError> {
        // The upstream retry companion correlates this event with its durable
        // schedule. It does not require the referenced step to still be open:
        // a delayed callback may publish `started` after `step/end`.
        let key = (started.retry_id().clone(), started.retry());
        let Some(schedule) = self.retry_schedules.get(&key) else {
            return Err(TransitionError::RetryStartedWithoutSchedule {
                retry_id: started.retry_id().clone(),
                retry: started.retry(),
            });
        };
        if schedule.turn != started.turn() || schedule.step != started.step() {
            return Err(TransitionError::RetryStartedWithoutSchedule {
                retry_id: started.retry_id().clone(),
                retry: started.retry(),
            });
        }
        if schedule.started {
            return Err(TransitionError::RetryStartedTwice {
                retry_id: started.retry_id().clone(),
                retry: started.retry(),
            });
        }
        Arc::make_mut(&mut self.retry_schedules)
            .get_mut(&key)
            .ok_or(TransitionError::RetryStartedWithoutSchedule {
                retry_id: started.retry_id().clone(),
                retry: started.retry(),
            })?
            .started = true;
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
        shadowed: &[SurfaceNode],
    ) -> Result<(), SurfaceError> {
        if shadowed.len() != 1 {
            return Err(SurfaceError::ToolResultMultipleTargets);
        }
        let Some(original_identity) = shadowed[0].tool_result_identity.as_deref() else {
            return Err(SurfaceError::ToolResultWrongTarget);
        };
        if !matches!(replacement.kind, EventKind::ToolResult { .. }) {
            return Err(SurfaceError::ToolResultWrongTarget);
        }
        let mut replacement_data =
            event_data_value(replacement).map_err(|_| SurfaceError::ToolResultChangedIdentity)?;
        if !mask_tool_result_content(&mut replacement_data)
            || !json_values_equal(original_identity, &replacement_data)
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

fn validate_durable_call_identity(id: &CallId, name: &str) -> Result<(), TransitionError> {
    if id.is_empty()
        || id.as_str().len() > 1_024
        || id.as_str().chars().any(char::is_control)
        || name.is_empty()
        || name.len() > 256
        || name.chars().any(char::is_control)
    {
        return Err(TransitionError::InvalidDurableToolCallIdentity {
            call_id: id.clone(),
        });
    }
    Ok(())
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
            Self::ApprovalAsked { .. } => "approval/asked",
            Self::ApprovalDecided { .. } => "approval/decided",
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

#[cfg(test)]
mod tests {
    use crate::{
        model::{CallId, ContentBlock, Message},
        session::{
            ApprovalAskedEvent, ApprovalDecidedEvent, ApprovalOutcome, ApprovalRequestId, Clock,
            ClockError, EventKind, EventSeq, NewEvent, Session, SessionEvent, StepId,
            SurfaceIntent, TOOL_NOT_STARTED, ToolFailure, TransitionError, TurnEndReason, TurnId,
            UnixMillis,
        },
    };

    use super::{Projection, ValidationPolicy};

    #[derive(Clone, Copy)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Result<UnixMillis, ClockError> {
            UnixMillis::new(7).map_err(|error| ClockError::new(error.to_string()))
        }
    }

    fn turn() -> TurnId {
        TurnId::new(1).unwrap()
    }

    fn step() -> StepId {
        StepId::new(1).unwrap()
    }

    fn assistant_with_calls(calls: &[(&str, &str, &str)]) -> Message {
        let content = calls
            .iter()
            .map(|(id, name, arguments)| ContentBlock::tool_call(*id, *name, *arguments).unwrap())
            .collect();
        Message::assistant("assistant", content, "mock", "mock-model").unwrap()
    }

    fn append(session: &mut Session, event: NewEvent) -> SessionEvent {
        session.append(event).unwrap();
        session.events().last().unwrap().clone()
    }

    fn open_step(session: &mut Session, calls: &[(&str, &str, &str)]) -> Vec<SessionEvent> {
        vec![
            append(session, NewEvent::log(EventKind::turn_start(turn()))),
            append(
                session,
                NewEvent::log(EventKind::step_start(turn(), step())),
            ),
            append(
                session,
                NewEvent::surface(
                    EventKind::assistant_message(turn(), step(), assistant_with_calls(calls)),
                    SurfaceIntent::append(),
                ),
            ),
        ]
    }

    fn strict_prefix(events: &[SessionEvent]) -> Result<Projection, TransitionError> {
        let mut projection = Projection::empty(ValidationPolicy::DurableStrict);
        for event in events {
            projection = projection.with_event(event).map_err(|error| match error {
                crate::session::EventValidationError::Transition(error) => error,
                other => panic!("unexpected validation error: {other}"),
            })?;
        }
        Ok(projection)
    }

    fn strict_scanned_prefix(events: &[SessionEvent]) -> Result<Projection, TransitionError> {
        let mut projection = Projection::empty(ValidationPolicy::DurableStrict);
        for event in events {
            projection
                .apply_scanned_event(event)
                .map_err(|error| match error {
                    crate::session::EventValidationError::Transition(error) => error,
                    other => panic!("unexpected validation error: {other}"),
                })?;
        }
        Ok(projection)
    }

    #[test]
    fn closed_dangling_call_is_strict_corruption_but_memory_compatible() {
        let mut session = Session::with_clock("memory-compatible", FixedClock).unwrap();
        let mut events = open_step(&mut session, &[("call-1", "echo", "{}")]);
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::step_end(turn(), step())),
        ));
        assert_eq!(session.state().open_step(), None);

        let error = strict_prefix(&events).unwrap_err();
        assert!(matches!(
            error,
            TransitionError::DurableCallStillPending { .. }
        ));
    }

    #[test]
    fn durable_trace_correlates_declaration_intent_approval_and_result() {
        let mut session = Session::with_clock("strict-complete", FixedClock).unwrap();
        let mut events = open_step(&mut session, &[("call-1", "echo", "{}")]);
        let call = append(
            &mut session,
            NewEvent::log(EventKind::tool_call(turn(), step(), "call-1", "echo", "{}")),
        );
        events.push(call.clone());
        let approval_id = ApprovalRequestId::new("approval-1");
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::approval_asked(
                ApprovalAskedEvent::new(
                    approval_id.clone(),
                    "echo",
                    Some(CallId::new("call-1")),
                    None,
                )
                .unwrap(),
            )),
        ));
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::approval_decided(
                ApprovalDecidedEvent::new(approval_id, ApprovalOutcome::AllowedOnce).unwrap(),
            )),
        ));
        events.push(append(
            &mut session,
            NewEvent::surface(
                EventKind::tool_result(
                    turn(),
                    step(),
                    Message::tool_result("result", "call-1", vec![], false).unwrap(),
                ),
                SurfaceIntent::append().with_sources(vec![call.seq()]),
            ),
        ));
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::step_end(turn(), step())),
        ));
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::turn_end(turn(), TurnEndReason::Completed)),
        ));

        let projection = strict_prefix(&events).unwrap();
        assert_eq!(projection.state().open_turn(), None);
        assert!(projection.state().pending_calls().is_empty());
        assert!(projection.state().pending_approvals().is_empty());
    }

    #[test]
    fn durable_intents_are_unique_ordered_and_exact() {
        let mut duplicate = Session::with_clock("duplicate", FixedClock).unwrap();
        let events = open_step(
            &mut duplicate,
            &[("same", "first", "{}"), ("same", "second", "{}")],
        );
        assert!(matches!(
            strict_prefix(&events).unwrap_err(),
            TransitionError::DuplicateDurableToolCall { .. }
        ));

        let mut reordered = Session::with_clock("reordered", FixedClock).unwrap();
        let mut events = open_step(
            &mut reordered,
            &[("call-a", "first", "{}"), ("call-b", "second", "{\"b\":1}")],
        );
        events.push(append(
            &mut reordered,
            NewEvent::log(EventKind::tool_call(
                turn(),
                step(),
                "call-b",
                "second",
                "{\"b\":1}",
            )),
        ));
        assert!(matches!(
            strict_prefix(&events).unwrap_err(),
            TransitionError::DurableToolCallMismatch { .. }
        ));
    }

    #[test]
    fn durable_result_cannot_precede_decision_or_contradict_it() {
        let mut session = Session::with_clock("approval-result", FixedClock).unwrap();
        let mut events = open_step(&mut session, &[("call-1", "echo", "{}")]);
        let call = append(
            &mut session,
            NewEvent::log(EventKind::tool_call(turn(), step(), "call-1", "echo", "{}")),
        );
        events.push(call.clone());
        let approval_id = ApprovalRequestId::new("approval-1");
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::approval_asked(
                ApprovalAskedEvent::new(
                    approval_id.clone(),
                    "echo",
                    Some(CallId::new("call-1")),
                    None,
                )
                .unwrap(),
            )),
        ));
        let result = append(
            &mut session,
            NewEvent::surface(
                EventKind::tool_result(
                    turn(),
                    step(),
                    Message::tool_result("result", "call-1", vec![], false).unwrap(),
                ),
                SurfaceIntent::append().with_sources(vec![call.seq()]),
            ),
        );
        let mut pending_result = events.clone();
        pending_result.push(result.clone());
        assert!(matches!(
            strict_prefix(&pending_result).unwrap_err(),
            TransitionError::DurableToolResultBeforeDecision { .. }
        ));

        events.push(append(
            &mut session,
            NewEvent::log(EventKind::approval_decided(
                ApprovalDecidedEvent::new(approval_id, ApprovalOutcome::Rejected).unwrap(),
            )),
        ));
        events.push(result);
        assert!(matches!(
            strict_prefix(&events).unwrap_err(),
            TransitionError::DurableApprovalResultMismatch { .. }
        ));
    }

    #[test]
    fn durable_not_started_repair_requires_an_assistant_declaration() {
        let mut session = Session::with_clock("repair", FixedClock).unwrap();
        let mut events = open_step(&mut session, &[("call-1", "echo", "{}")]);
        events.push(append(
            &mut session,
            NewEvent::surface(
                EventKind::ToolResult {
                    turn: turn(),
                    step: step(),
                    message: Message::tool_result("repair", "call-1", vec![], true).unwrap(),
                    error: Some(ToolFailure {
                        name: "ToolNotStartedError".to_owned(),
                        code: TOOL_NOT_STARTED.to_owned(),
                    }),
                    meta: None,
                },
                SurfaceIntent::append(),
            ),
        ));
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::step_end(turn(), step())),
        ));
        assert!(matches!(
            strict_prefix(&events).unwrap_err(),
            TransitionError::DurableRecoveryEventNotAllowed {
                event_type: "tool/result"
            }
        ));
        assert!(matches!(
            strict_scanned_prefix(&events).unwrap_err(),
            TransitionError::DurableRecoveryEventNotAllowed {
                event_type: "tool/result"
            }
        ));

        let isolated = vec![events[0].clone(), events[1].clone(), events[3].clone()];
        assert!(matches!(
            strict_scanned_prefix(&isolated).unwrap_err(),
            TransitionError::DurableToolResultMismatch { .. }
        ));
    }

    #[test]
    fn durable_end_seed_is_recovery_only() {
        let mut session = Session::with_clock("seed", FixedClock).unwrap();
        let seed = append(&mut session, NewEvent::log(EventKind::EndSeed));

        assert!(matches!(
            strict_prefix(std::slice::from_ref(&seed)).unwrap_err(),
            TransitionError::DurableRecoveryEventNotAllowed {
                event_type: "session/end-seed"
            }
        ));
        assert!(matches!(
            strict_scanned_prefix(&[seed]).unwrap_err(),
            TransitionError::DurableRecoveryEventNotAllowed {
                event_type: "session/end-seed"
            }
        ));
    }

    #[test]
    fn durable_tool_result_requires_the_exact_intent_source() {
        let mut session = Session::with_clock("wrong-source", FixedClock).unwrap();
        let mut events = open_step(&mut session, &[("call-1", "echo", "{}")]);
        events.push(append(
            &mut session,
            NewEvent::log(EventKind::tool_call(turn(), step(), "call-1", "echo", "{}")),
        ));
        events.push(append(
            &mut session,
            NewEvent::surface(
                EventKind::tool_result(
                    turn(),
                    step(),
                    Message::tool_result("result", "call-1", vec![], false).unwrap(),
                ),
                SurfaceIntent::append().with_sources(vec![EventSeq::new(1).unwrap()]),
            ),
        ));
        assert!(matches!(
            strict_prefix(&events).unwrap_err(),
            TransitionError::DurableToolResultWrongSource { .. }
        ));
    }
}
