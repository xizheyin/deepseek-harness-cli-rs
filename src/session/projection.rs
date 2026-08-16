//! Pure turn/step/tool and model-visible surface projection.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::model::{CallId, Message, MessageSourceKind, NonNegativeSafeInteger};

use super::{
    ApprovalOutcome, ApprovalRequestId, CompactionEndError, CompactionId, CompactionRange,
    CompactionTrigger, EventKind, EventSeq, MAX_SAFE_INTEGER, MAX_SOURCE_EVENT_SEQS, SessionEvent,
    SessionId, StepId, SurfaceOp, TurnId,
    compaction::{
        COMPACTION_CHECKPOINT_PREFIX, COMPACTION_CHECKPOINT_SOURCE, COMPACTION_CHECKPOINT_SUFFIX,
        ModelVisibleDispatchSnapshot,
    },
    context_budget::{
        ContextBudgetError, SurfacePriceFacts, estimate_message, estimate_request_header,
        select_compactable_prefix,
    },
    error::{EventValidationError, SurfaceError, TransitionError},
    event::{RECOVERY_TOOL_RESULT_ID_PREFIX, TOOL_NOT_STARTED},
    journal_row::JournalRowLocator,
    recovery::{RecoveryAdmission, RecoveryCompactionStage},
    tool_result_pruner::{MaskedToolResultDigest, ToolResultSnapshot, masked_data_sha256},
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
    CompatibilityReplay,
    ColdScan,
    OwnedPrune,
    HistoricalScan,
}

impl ValidationAdmission {
    fn is_unprivileged_durable(self) -> bool {
        matches!(self, Self::Ordinary | Self::ColdScan)
    }
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct OpenCompaction {
    id: CompactionId,
    source_command_id: Option<String>,
    owner: Option<TurnId>,
    start_seq: EventSeq,
    recipe: Option<CompactionRecipe>,
    phase: CompactionPhase,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompactionRecipe {
    trigger: CompactionTrigger,
    range: CompactionRange,
    shadowed_seqs: Arc<Vec<EventSeq>>,
    shadowed_token_count: u64,
    provider: String,
    model: String,
    max_tokens: Option<NonNegativeSafeInteger>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CompactionPhase {
    Started,
    Summarized {
        summary_seq: EventSeq,
        range: CompactionRange,
        shadowed_seqs: Arc<Vec<EventSeq>>,
        summary_blocks: Arc<Vec<crate::model::JsonValue>>,
        shadowed_token_count: NonNegativeSafeInteger,
    },
    Replaced {
        summary_seq: EventSeq,
        checkpoint_seq: EventSeq,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PruneShadowClaim {
    target_seq: EventSeq,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CompactionState {
    surface_generation: u64,
    open: Option<OpenCompaction>,
    prune_claim: Option<PruneShadowClaim>,
    orphan_prune_count: u64,
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
    session_id: Option<SessionId>,
    next_turn: TurnId,
    boundary: Boundary,
    surface_nodes: Arc<Vec<SurfaceNode>>,
    surface_tokens: u64,
    request_header: Option<super::EpochHeader>,
    request_header_seq: Option<EventSeq>,
    request_context: Option<super::RequestContext>,
    request_context_seq: Option<EventSeq>,
    compaction: CompactionState,
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
    estimated_tokens: u64,
    tool_delta: i64,
    tool_result: Option<ToolResultOrigin>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolResultOrigin {
    Memory {
        masked: MaskedToolResultDigest,
    },
    PendingDurable {
        masked: MaskedToolResultDigest,
    },
    Durable {
        masked: MaskedToolResultDigest,
        row: JournalRowLocator,
    },
}

#[derive(Clone, Copy)]
enum SurfaceRowBinding {
    Memory,
    PendingDurable,
    Durable(JournalRowLocator),
}

pub(super) struct PreparedDurableProjection {
    projection: Projection,
    pending_tool_result: Option<EventSeq>,
}

impl PreparedDurableProjection {
    pub(super) fn bind(mut self, row: JournalRowLocator) -> Option<Projection> {
        if let Some(seq) = self.pending_tool_result {
            if seq != row.seq() || !self.projection.bind_tool_result_row(seq, row) {
                return None;
            }
        }
        Some(self.projection)
    }
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
            session_id: None,
            next_turn: TurnId::first(),
            boundary: Boundary::Idle,
            surface_nodes: Arc::new(Vec::new()),
            surface_tokens: 0,
            request_header: None,
            request_header_seq: None,
            request_context: None,
            request_context_seq: None,
            compaction: CompactionState::default(),
            pending_approvals: Vec::new(),
            owned_approval_ids: Arc::new(BTreeSet::new()),
            retry_chains: Arc::new(BTreeMap::new()),
            retry_schedules: Arc::new(BTreeMap::new()),
        }
    }

    pub(crate) fn for_session(policy: ValidationPolicy, session_id: SessionId) -> Self {
        let mut projection = Self::empty(policy);
        projection.session_id = Some(session_id);
        projection
    }

    /// Validate and apply one candidate to a detached projection clone.
    pub(crate) fn with_event(&self, event: &SessionEvent) -> Result<Self, EventValidationError> {
        event.kind.validate()?;
        let mut next = self.clone();
        let compaction = next.next_compaction_state(event, ValidationAdmission::Ordinary)?;
        next.apply_transition(event, ValidationAdmission::Ordinary)?;
        next.apply_surface(event, SurfaceRowBinding::Memory)?;
        next.compaction = compaction;
        Ok(next)
    }

    /// Apply one event through the finite released-format compatibility
    /// reader. New live events must continue through `with_event`.
    pub(crate) fn with_compatible_event(
        &self,
        event: &SessionEvent,
    ) -> Result<Self, EventValidationError> {
        event.kind.validate()?;
        let mut next = self.clone();
        let compaction =
            next.next_compaction_state(event, ValidationAdmission::CompatibilityReplay)?;
        next.apply_transition(event, ValidationAdmission::CompatibilityReplay)?;
        next.apply_surface(event, SurfaceRowBinding::Memory)?;
        next.compaction = compaction;
        Ok(next)
    }

    /// Apply one ordinary cold-scanned row without cloning the active index.
    ///
    /// A cold scanner discards the entire candidate on a semantic failure, so
    /// it does not need live append's detached rollback clone. This keeps a
    /// long valid journal linear in its event count.
    pub(super) fn prepare_durable_event(
        &self,
        event: &SessionEvent,
    ) -> Result<PreparedDurableProjection, EventValidationError> {
        event.kind.validate()?;
        let mut next = self.clone();
        let compaction = next.next_compaction_state(event, ValidationAdmission::Ordinary)?;
        next.apply_transition(event, ValidationAdmission::Ordinary)?;
        next.apply_surface(event, SurfaceRowBinding::PendingDurable)?;
        next.compaction = compaction;
        Ok(PreparedDurableProjection {
            projection: next,
            pending_tool_result: matches!(event.kind(), EventKind::ToolResult { .. })
                .then_some(event.seq()),
        })
    }

    pub(super) fn prepare_owned_prune_event(
        &self,
        event: &SessionEvent,
    ) -> Result<PreparedDurableProjection, EventValidationError> {
        event.kind.validate()?;
        let mut next = self.clone();
        let compaction = next.next_compaction_state(event, ValidationAdmission::OwnedPrune)?;
        next.apply_transition(event, ValidationAdmission::OwnedPrune)?;
        next.apply_surface(event, SurfaceRowBinding::PendingDurable)?;
        next.compaction = compaction;
        Ok(PreparedDurableProjection {
            projection: next,
            pending_tool_result: matches!(event.kind(), EventKind::ToolResult { .. })
                .then_some(event.seq()),
        })
    }

    pub(super) fn apply_scanned_row(
        &mut self,
        event: &SessionEvent,
        row: JournalRowLocator,
    ) -> Result<(), EventValidationError> {
        event.kind.validate()?;
        let compaction = self.next_compaction_state(event, ValidationAdmission::ColdScan)?;
        self.apply_transition(event, ValidationAdmission::ColdScan)?;
        self.apply_surface(event, SurfaceRowBinding::Durable(row))?;
        self.compaction = compaction;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn apply_scanned_event(
        &mut self,
        event: &SessionEvent,
    ) -> Result<(), EventValidationError> {
        let row = super::jsonl::encode_event_line(event)
            .ok()
            .and_then(|bytes| JournalRowLocator::new(event.seq(), 0, &bytes))
            .ok_or(SurfaceError::ToolResultChangedIdentity)?;
        self.apply_scanned_row(event, row)
    }

    /// Apply one event admitted by recovery's private exact-match cursor.
    pub(super) fn apply_recovery_admission(
        &mut self,
        admission: RecoveryAdmission<'_>,
    ) -> Result<(), EventValidationError> {
        let event = admission.event();
        event.kind.validate()?;
        let compaction = self.next_compaction_state(event, ValidationAdmission::HistoricalScan)?;
        self.apply_transition(event, ValidationAdmission::HistoricalScan)?;
        let binding = admission.row().map_or(
            SurfaceRowBinding::PendingDurable,
            SurfaceRowBinding::Durable,
        );
        self.apply_surface(event, binding)?;
        self.compaction = compaction;
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

    pub(super) fn durable_tool_result_snapshot(&self, seq: EventSeq) -> Option<ToolResultSnapshot> {
        let node = self.surface_nodes.iter().find(|node| node.seq == seq)?;
        let ToolResultOrigin::Durable { masked, row } = node.tool_result? else {
            return None;
        };
        Some(ToolResultSnapshot::new(
            seq,
            node.message.clone()?,
            node.estimated_tokens,
            row,
            masked,
        ))
    }

    pub(super) fn durable_tool_result_seqs(&self) -> Result<Vec<EventSeq>, ()> {
        let candidate_count = self
            .surface_nodes
            .iter()
            .filter(|node| matches!(node.tool_result, Some(ToolResultOrigin::Durable { .. })))
            .count();
        let mut candidates = Vec::new();
        candidates
            .try_reserve_exact(candidate_count)
            .map_err(|_| ())?;
        candidates.extend(self.surface_nodes.iter().filter_map(|node| {
            matches!(node.tool_result, Some(ToolResultOrigin::Durable { .. })).then_some(node.seq)
        }));
        Ok(candidates)
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

    pub(crate) fn try_messages_with(&self, pending: &[Message]) -> Result<Vec<Message>, ()> {
        let committed = self
            .surface_nodes
            .iter()
            .filter(|node| node.message.is_some())
            .count();
        let total = committed.checked_add(pending.len()).ok_or(())?;
        let mut messages = Vec::new();
        messages.try_reserve_exact(total).map_err(|_| ())?;
        messages.extend(
            self.surface_nodes
                .iter()
                .filter_map(|node| node.message.clone()),
        );
        messages.extend(pending.iter().cloned());
        Ok(messages)
    }

    pub(crate) fn messages_equal(&self, expected: &[Message]) -> bool {
        self.surface_nodes
            .iter()
            .filter_map(|node| node.message.as_ref())
            .eq(expected.iter())
    }

    pub(crate) fn surface_generation(&self) -> u64 {
        self.compaction.surface_generation
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

    pub(crate) fn interrupted_compaction_stage(&self) -> Option<RecoveryCompactionStage> {
        self.compaction.open.as_ref().map(|open| match &open.phase {
            CompactionPhase::Started => RecoveryCompactionStage::Started,
            CompactionPhase::Summarized { .. } => RecoveryCompactionStage::Summarized,
            CompactionPhase::Replaced { .. } => RecoveryCompactionStage::Replaced,
        })
    }

    pub(crate) fn orphan_prune_markers(&self) -> u64 {
        self.compaction
            .orphan_prune_count
            .saturating_add(u64::from(self.compaction.prune_claim.is_some()))
    }

    pub(crate) fn compaction_recovery_is_consistent(&self) -> bool {
        self.compaction.open.is_none()
            || (self.pending_approvals.is_empty() && !self.has_pending_durable_call())
    }

    fn next_compaction_state(
        &self,
        event: &SessionEvent,
        admission: ValidationAdmission,
    ) -> Result<CompactionState, EventValidationError> {
        let mut next = self.compaction.clone();

        if self.policy == ValidationPolicy::DurableStrict
            && admission == ValidationAdmission::Ordinary
            && (matches!(event.kind, EventKind::CompactionPrune { .. })
                || matches!(event.kind, EventKind::ToolResult { .. })
                    && matches!(event.surface_op, Some(SurfaceOp::Replace(_))))
        {
            return Err(TransitionError::DurablePruneEventNotAllowed {
                event_type: event.kind.event_type_static(),
            }
            .into());
        }

        if self.policy == ValidationPolicy::DurableStrict
            && admission.is_unprivileged_durable()
            && next.open.is_some()
            && matches!(event.kind, EventKind::StepEnd { .. })
        {
            return Err(TransitionError::DurableRecoveryEventNotAllowed {
                event_type: "step/end",
            }
            .into());
        }

        let recovery_only_ordinary = self.policy == ValidationPolicy::DurableStrict
            && admission.is_unprivileged_durable()
            && (matches!(
                &event.kind,
                EventKind::EndSeed
                    | EventKind::TurnEnd {
                        reason: super::TurnEndReason::Interrupted,
                        ..
                    }
            ) || matches!(
                &event.kind,
                EventKind::ToolResult { message, .. }
                    if message.id().as_str().starts_with(RECOVERY_TOOL_RESULT_ID_PREFIX)
            ));
        if recovery_only_ordinary {
            return Ok(next);
        }

        if admission == ValidationAdmission::HistoricalScan {
            if matches!(event.kind, EventKind::EndSeed) {
                next.open = None;
                next.prune_claim = None;
                next.orphan_prune_count = 0;
            }
            Self::advance_surface_generation(&mut next, event)?;
            return Ok(next);
        }

        if self.policy == ValidationPolicy::MemoryCompatible
            && matches!(event.kind, EventKind::EndSeed)
        {
            next.open = None;
            next.prune_claim = None;
            next.orphan_prune_count = 0;
            return Ok(next);
        }

        let consumed_prune = self.apply_prune_adjacency(&mut next, event)?;

        if let Some(open) = next.open.clone() {
            next.open = self.advance_open_compaction(event, open, admission)?;
        } else {
            match &event.kind {
                EventKind::CompactionStart { start } => {
                    next.open = Some(self.validate_compaction_start(event.seq, start, admission)?);
                }
                EventKind::CompactionSummary { .. } => {
                    return Err(TransitionError::CompactionWithoutStart {
                        event_type: "compaction/summary",
                    }
                    .into());
                }
                EventKind::CompactionEnd { .. } => {
                    return Err(TransitionError::CompactionWithoutStart {
                        event_type: "compaction/end",
                    }
                    .into());
                }
                EventKind::CompactionPrune { prune } => {
                    let target = prune.shadowed_seqs()[0];
                    let Some(node) = self.surface_nodes.iter().find(|node| node.seq == target)
                    else {
                        return Err(SurfaceError::PruneTargetNotToolResult.into());
                    };
                    if node.kind != SurfaceNodeKind::ToolResult {
                        return Err(SurfaceError::PruneTargetNotToolResult.into());
                    }
                    if self.policy == ValidationPolicy::DurableStrict
                        && prune.shadowed_token_count().get() != node.estimated_tokens
                    {
                        return Err(SurfaceError::ShadowedTokenCountMismatch {
                            expected: node.estimated_tokens,
                            actual: prune.shadowed_token_count().get(),
                        }
                        .into());
                    }
                    next.prune_claim = Some(PruneShadowClaim { target_seq: target });
                }
                EventKind::UserMessage { message }
                    if matches!(event.surface_op, Some(SurfaceOp::Replace(_)))
                        && is_compaction_checkpoint_source(message) =>
                {
                    return Err(TransitionError::CompactionWithoutStart {
                        event_type: "user/message replacement",
                    }
                    .into());
                }
                EventKind::ToolResult { .. }
                    if self.policy == ValidationPolicy::DurableStrict
                        && matches!(event.surface_op, Some(SurfaceOp::Replace(_)))
                        && !consumed_prune =>
                {
                    return Err(SurfaceError::PruneReplacementWithoutMarker.into());
                }
                _ => {}
            }
        }

        Self::advance_surface_generation(&mut next, event)?;
        Ok(next)
    }

    fn advance_surface_generation(
        state: &mut CompactionState,
        event: &SessionEvent,
    ) -> Result<(), TransitionError> {
        if event.kind.is_surface_eligible() {
            state.surface_generation = state
                .surface_generation
                .checked_add(1)
                .filter(|generation| *generation <= MAX_SAFE_INTEGER)
                .ok_or(TransitionError::IdentifierExhausted)?;
        }
        Ok(())
    }

    fn apply_prune_adjacency(
        &self,
        state: &mut CompactionState,
        event: &SessionEvent,
    ) -> Result<bool, EventValidationError> {
        let Some(claim) = state.prune_claim.take() else {
            return Ok(false);
        };
        if let Some(SurfaceOp::Replace(replacement)) = &event.surface_op {
            let is_exact_tool_result = matches!(event.kind, EventKind::ToolResult { .. })
                && replacement.start == claim.target_seq
                && replacement.end == claim.target_seq
                && event.source_event_seqs.as_deref() == Some([claim.target_seq].as_slice());
            if is_exact_tool_result {
                return Ok(true);
            }
            // A prune marker prices exactly the immediately following rewrite. Letting a
            // different replacement pass would make consumers associate that price with the
            // wrong surface mutation.
            return Err(SurfaceError::PruneReplacementMismatch.into());
        }
        state.orphan_prune_count = state
            .orphan_prune_count
            .checked_add(1)
            .ok_or(TransitionError::IdentifierExhausted)?;
        Ok(false)
    }

    fn validate_compaction_start(
        &self,
        seq: EventSeq,
        start: &super::CompactionStartEvent,
        admission: ValidationAdmission,
    ) -> Result<OpenCompaction, EventValidationError> {
        if start.turn() != self.open_turn() {
            return Err(TransitionError::CompactionOwnerMismatch {
                event_type: "compaction/start",
            }
            .into());
        }
        let recipe = match start.dispatch() {
            Some(dispatch) => Some(self.validate_compaction_dispatch(seq, dispatch)?),
            None if admission != ValidationAdmission::CompatibilityReplay => {
                return Err(TransitionError::DurableCompactionDispatchRequired.into());
            }
            None => None,
        };
        Ok(OpenCompaction {
            id: start.compaction_id().clone(),
            source_command_id: start.source_command_id().map(str::to_owned),
            owner: start.turn(),
            start_seq: seq,
            recipe,
            phase: CompactionPhase::Started,
        })
    }

    fn validate_compaction_dispatch(
        &self,
        start_seq: EventSeq,
        dispatch: &ModelVisibleDispatchSnapshot,
    ) -> Result<CompactionRecipe, EventValidationError> {
        if dispatch.source_surface_generation().get() != self.compaction.surface_generation {
            return Err(TransitionError::CompactionDispatchMismatch(
                "surface generation changed before compaction/start",
            )
            .into());
        }
        if !self.pending_approvals.is_empty() || self.has_pending_durable_call() {
            return Err(TransitionError::CompactionDispatchMismatch(
                "compaction cannot start with unresolved approval or tool work",
            )
            .into());
        }
        let Some((range_start, range_end)) =
            self.surface_range_indices(dispatch.shadowed_range(), dispatch.shadowed_seqs())
        else {
            return Err(TransitionError::CompactionDispatchMismatch(
                "shadowed range is not the selected current surface",
            )
            .into());
        };
        if range_start != 0
            || dispatch
                .shadowed_seqs()
                .iter()
                .any(|shadowed| *shadowed >= start_seq)
            || dispatch.shadowed_seqs().len() > MAX_SOURCE_EVENT_SEQS - 2
        {
            return Err(TransitionError::CompactionDispatchMismatch(
                "shadowed range is not a bounded oldest surface prefix",
            )
            .into());
        }
        let retained_tokens = self.surface_nodes[range_end + 1..]
            .iter()
            .try_fold(0_u64, |total, node| {
                total.checked_add(node.estimated_tokens)
            })
            .ok_or(SurfaceError::TokenAccountingOverflow)?;
        let selected = select_compactable_prefix(
            self.surface_nodes.as_slice(),
            retained_tokens,
            MAX_SOURCE_EVENT_SEQS - 2,
            surface_price_facts,
        )
        .map_err(context_budget_surface_error)?;
        if selected.map(|selected| selected.end_exclusive) != Some(range_end + 1) {
            return Err(TransitionError::CompactionDispatchMismatch(
                "shadowed range is not the canonical balanced prefix",
            )
            .into());
        }
        let shadowed_token_count = self.surface_nodes[..=range_end]
            .iter()
            .try_fold(0_u64, |total, node| {
                total.checked_add(node.estimated_tokens)
            })
            .ok_or(SurfaceError::TokenAccountingOverflow)?;
        if self
            .session_id
            .as_ref()
            .is_some_and(|session_id| session_id != dispatch.session_id())
        {
            return Err(TransitionError::CompactionDispatchMismatch(
                "sessionId does not match the active Session",
            )
            .into());
        }
        if dispatch.request_header_seq() != self.request_header_seq {
            return Err(TransitionError::CompactionDispatchMismatch(
                "requestHeaderSeq is not the latest header",
            )
            .into());
        }
        if dispatch.request_context_seq() != self.request_context_seq {
            return Err(TransitionError::CompactionDispatchMismatch(
                "requestContextSeq is not the latest context",
            )
            .into());
        }
        if let Some(header) = &self.request_header {
            let canonical = header.canonicalized();
            if dispatch.system() != canonical.system.as_deref()
                || dispatch.tools() != canonical.tools.as_deref().unwrap_or_default()
            {
                return Err(TransitionError::CompactionDispatchMismatch(
                    "system or tools differ from the referenced request header",
                )
                .into());
            }
        }
        estimate_request_header(dispatch.system(), dispatch.tools())
            .map_err(context_budget_surface_error)?;
        match (dispatch.trigger(), &self.boundary) {
            (CompactionTrigger::Pressure | CompactionTrigger::HardLimit, Boundary::Turn { .. })
            | (CompactionTrigger::ContextOverflow, Boundary::Step { .. }) => {}
            (CompactionTrigger::Pressure | CompactionTrigger::HardLimit, _) => {
                return Err(TransitionError::CompactionDispatchMismatch(
                    "pressure and hard-limit compaction require an open turn without a step",
                )
                .into());
            }
            (CompactionTrigger::ContextOverflow, _) => {
                return Err(TransitionError::CompactionDispatchMismatch(
                    "context-overflow compaction requires an open step",
                )
                .into());
            }
        }
        Ok(CompactionRecipe {
            trigger: dispatch.trigger(),
            range: dispatch.shadowed_range(),
            shadowed_seqs: Arc::new(dispatch.shadowed_seqs().to_vec()),
            shadowed_token_count,
            provider: dispatch.prepared_call().config().provider().to_owned(),
            model: dispatch.prepared_call().config().model().to_owned(),
            max_tokens: dispatch.prepared_call().config().max_tokens(),
        })
    }

    fn has_pending_durable_call(&self) -> bool {
        match &self.boundary {
            Boundary::Step {
                pending_calls,
                declared_calls,
                ..
            } => !pending_calls.is_empty() || declared_calls.iter().any(|call| !call.result_seen),
            Boundary::Idle | Boundary::Turn { .. } => false,
        }
    }

    fn advance_open_compaction(
        &self,
        event: &SessionEvent,
        mut open: OpenCompaction,
        admission: ValidationAdmission,
    ) -> Result<Option<OpenCompaction>, EventValidationError> {
        match &event.kind {
            EventKind::CompactionStart { .. } => Err(TransitionError::CompactionAlreadyOpen {
                open: open.id.clone(),
            }
            .into()),
            EventKind::CompactionSummary { summary } => {
                self.validate_compaction_identity(
                    "compaction/summary",
                    &open,
                    summary.compaction_id(),
                    summary.source_command_id(),
                )?;
                if !matches!(open.phase, CompactionPhase::Started) {
                    return Err(TransitionError::CompactionBodyOutOfOrder {
                        event_type: "compaction/summary",
                    }
                    .into());
                }
                if !event_is_immediately_after(event.seq, open.start_seq) {
                    return Err(TransitionError::CompactionBodyOutOfOrder {
                        event_type: "compaction/summary",
                    }
                    .into());
                }
                if self.open_turn() != open.owner
                    || !self
                        .surface_range_matches(summary.shadowed_range(), summary.shadowed_seqs())
                {
                    return Err(TransitionError::CompactionDispatchMismatch(
                        "summary does not match its owner or selected surface",
                    )
                    .into());
                }
                if let Some(recipe) = &open.recipe {
                    if self.policy == ValidationPolicy::DurableStrict
                        && summary.shadowed_token_count().get() != recipe.shadowed_token_count
                    {
                        return Err(SurfaceError::ShadowedTokenCountMismatch {
                            expected: recipe.shadowed_token_count,
                            actual: summary.shadowed_token_count().get(),
                        }
                        .into());
                    }
                    if summary.shadowed_range() != recipe.range
                        || summary.shadowed_seqs() != recipe.shadowed_seqs.as_slice()
                        || summary.provider() != recipe.provider
                        || summary.model() != recipe.model
                        || summary.max_tokens() != recipe.max_tokens
                        || (self.policy == ValidationPolicy::DurableStrict
                            && !summary.is_llm_stream_call())
                    {
                        return Err(TransitionError::CompactionDispatchMismatch(
                            "summary differs from the prepared compaction call",
                        )
                        .into());
                    }
                }
                let summary_blocks = summary
                    .summary()
                    .iter()
                    .map(|block| block.raw().clone())
                    .collect::<Vec<_>>();
                open.phase = CompactionPhase::Summarized {
                    summary_seq: event.seq,
                    range: summary.shadowed_range(),
                    shadowed_seqs: Arc::new(summary.shadowed_seqs().to_vec()),
                    summary_blocks: Arc::new(summary_blocks),
                    shadowed_token_count: summary.shadowed_token_count(),
                };
                Ok(Some(open))
            }
            EventKind::UserMessage { message }
                if matches!(event.surface_op, Some(SurfaceOp::Replace(_))) =>
            {
                let CompactionPhase::Summarized {
                    summary_seq,
                    range,
                    shadowed_seqs,
                    summary_blocks,
                    shadowed_token_count,
                } = &open.phase
                else {
                    return Err(TransitionError::CompactionBodyOutOfOrder {
                        event_type: "user/message replacement",
                    }
                    .into());
                };
                if !event_is_immediately_after(event.seq, *summary_seq)
                    || !self.validate_checkpoint_replacement(
                        event,
                        message,
                        &open,
                        *summary_seq,
                        *range,
                        shadowed_seqs,
                        summary_blocks,
                    )
                {
                    return Err(SurfaceError::CompactionReplacementMismatch.into());
                }
                if self.policy == ValidationPolicy::DurableStrict {
                    let replacement_tokens =
                        estimate_message(message).map_err(context_budget_surface_error)?;
                    if replacement_tokens >= shadowed_token_count.get() {
                        return Err(SurfaceError::CompactionDoesNotShrink {
                            shadowed: shadowed_token_count.get(),
                            replacement: replacement_tokens,
                        }
                        .into());
                    }
                }
                open.phase = CompactionPhase::Replaced {
                    summary_seq: *summary_seq,
                    checkpoint_seq: event.seq,
                };
                open.recipe = None;
                Ok(Some(open))
            }
            EventKind::CompactionEnd { end } => {
                self.validate_compaction_identity(
                    "compaction/end",
                    &open,
                    end.compaction_id(),
                    end.source_command_id(),
                )?;
                if end.turn() != open.owner || self.open_turn() != open.owner {
                    return Err(TransitionError::CompactionOwnerMismatch {
                        event_type: "compaction/end",
                    }
                    .into());
                }
                if admission != ValidationAdmission::CompatibilityReplay
                    && end.error().is_some_and(CompactionEndError::is_legacy)
                {
                    return Err(TransitionError::DurableLegacyCompactionError.into());
                }
                let previous = match open.phase {
                    CompactionPhase::Started => open.start_seq,
                    CompactionPhase::Summarized { summary_seq, .. } => summary_seq,
                    CompactionPhase::Replaced { checkpoint_seq, .. } => checkpoint_seq,
                };
                if !event_is_immediately_after(event.seq, previous) {
                    return Err(TransitionError::CompactionBodyOutOfOrder {
                        event_type: "compaction/end",
                    }
                    .into());
                }
                if end.error().is_none() && !matches!(open.phase, CompactionPhase::Replaced { .. })
                {
                    return Err(TransitionError::CompactionSuccessWithoutReplacement.into());
                }
                Ok(None)
            }
            _ => Err(TransitionError::CompactionBoundaryCrossed {
                event_type: event.kind.event_type_static(),
            }
            .into()),
        }
    }

    fn validate_compaction_identity(
        &self,
        event_type: &'static str,
        open: &OpenCompaction,
        actual_id: &CompactionId,
        actual_source_command_id: Option<&str>,
    ) -> Result<(), TransitionError> {
        if &open.id != actual_id {
            return Err(TransitionError::CompactionIdMismatch {
                event_type,
                expected: open.id.clone(),
                actual: actual_id.clone(),
            });
        }
        if open.source_command_id.as_deref() != actual_source_command_id {
            return Err(TransitionError::CompactionSourceCommandMismatch { event_type });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn validate_checkpoint_replacement(
        &self,
        event: &SessionEvent,
        message: &Message,
        open: &OpenCompaction,
        summary_seq: EventSeq,
        range: CompactionRange,
        shadowed_seqs: &[EventSeq],
        summary_blocks: &[crate::model::JsonValue],
    ) -> bool {
        let Some(SurfaceOp::Replace(replacement)) = &event.surface_op else {
            return false;
        };
        if replacement.start != range.start() || replacement.end != range.end() {
            return false;
        }
        let mut expected_sources = Vec::with_capacity(shadowed_seqs.len() + 2);
        expected_sources.push(open.start_seq);
        expected_sources.push(summary_seq);
        expected_sources.extend_from_slice(shadowed_seqs);
        if event.source_event_seqs.as_deref() != Some(expected_sources.as_slice()) {
            return false;
        }
        let MessageSourceKind::Plugin { plugin, .. } = message.source().kind() else {
            return false;
        };
        let plugin_is_valid = plugin == COMPACTION_CHECKPOINT_SOURCE;
        if !plugin_is_valid || !checkpoint_source_matches(message, open) {
            return false;
        }
        let content = message.content();
        if content.len() != summary_blocks.len() + 2
            || !content.first().is_some_and(|block| {
                canonical_text_block_matches(block, COMPACTION_CHECKPOINT_PREFIX)
            })
            || !content.last().is_some_and(|block| {
                canonical_text_block_matches(block, COMPACTION_CHECKPOINT_SUFFIX)
            })
        {
            return false;
        }
        content[1..content.len() - 1]
            .iter()
            .zip(summary_blocks)
            .all(|(actual, expected)| actual.raw() == expected)
    }

    fn surface_range_matches(&self, range: CompactionRange, expected: &[EventSeq]) -> bool {
        self.surface_range_indices(range, expected).is_some()
    }

    fn surface_range_indices(
        &self,
        range: CompactionRange,
        expected: &[EventSeq],
    ) -> Option<(usize, usize)> {
        let start = self
            .surface_nodes
            .iter()
            .position(|node| node.seq == range.start())?;
        let end = self
            .surface_nodes
            .iter()
            .position(|node| node.seq == range.end())?;
        (start <= end
            && self.surface_nodes[start..=end]
                .iter()
                .map(|node| node.seq)
                .eq(expected.iter().copied()))
        .then_some((start, end))
    }

    fn apply_transition(
        &mut self,
        event: &SessionEvent,
        admission: ValidationAdmission,
    ) -> Result<(), TransitionError> {
        if self.policy == ValidationPolicy::DurableStrict
            && admission.is_unprivileged_durable()
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
            && admission.is_unprivileged_durable()
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
                self.request_header_seq = Some(event.seq);
            }
            EventKind::RequestContext { context } => {
                if self.open_turn().is_none() {
                    return Err(TransitionError::EventOutsideTurn {
                        event_type: event.kind.event_type_static(),
                    });
                }
                self.request_context = Some(context.clone());
                self.request_context_seq = Some(event.seq);
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
            EventKind::CompactionStart { .. }
            | EventKind::CompactionSummary { .. }
            | EventKind::CompactionEnd { .. }
            | EventKind::CompactionPrune { .. }
            | EventKind::UserMessage { .. }
            | EventKind::EndSeed
            | EventKind::Unknown { .. } => {}
        }
        Ok(())
    }

    fn apply_surface(
        &mut self,
        event: &SessionEvent,
        row_binding: SurfaceRowBinding,
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
            SurfaceOp::Append(_) => {
                let node = Self::surface_node(event, row_binding)?;
                let surface_tokens = self
                    .surface_tokens
                    .checked_add(node.estimated_tokens)
                    .ok_or(SurfaceError::TokenAccountingOverflow)?;
                Arc::make_mut(&mut self.surface_nodes).push(node);
                self.surface_tokens = surface_tokens;
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
                let node = Self::surface_node(event, row_binding)?;
                let removed_tokens = shadowed.iter().try_fold(0_u64, |total, shadowed| {
                    total.checked_add(shadowed.estimated_tokens)
                });
                let surface_tokens = self
                    .surface_tokens
                    .checked_sub(removed_tokens.ok_or(SurfaceError::TokenAccountingOverflow)?)
                    .and_then(|total| total.checked_add(node.estimated_tokens))
                    .ok_or(SurfaceError::TokenAccountingOverflow)?;
                Arc::make_mut(&mut self.surface_nodes).splice(start_index..=end_index, [node]);
                self.surface_tokens = surface_tokens;
            }
        }
        Ok(())
    }

    fn surface_node(
        event: &SessionEvent,
        row_binding: SurfaceRowBinding,
    ) -> Result<SurfaceNode, SurfaceError> {
        let (kind, message, tool_delta) = match &event.kind {
            EventKind::UserMessage { message } => (SurfaceNodeKind::User, Some(message.clone()), 0),
            EventKind::ToolResult { message, .. } => {
                (SurfaceNodeKind::ToolResult, Some(message.clone()), -1)
            }
            EventKind::AssistantMessage { message, .. } if !message.content().is_empty() => {
                let tool_calls = message
                    .content()
                    .iter()
                    .filter(|block| {
                        matches!(
                            block.kind(),
                            crate::model::ContentBlockKind::ToolCall { .. }
                        )
                    })
                    .count();
                let tool_delta =
                    i64::try_from(tool_calls).map_err(|_| SurfaceError::TokenAccountingOverflow)?;
                (
                    SurfaceNodeKind::Assistant,
                    Some(message.clone()),
                    tool_delta,
                )
            }
            EventKind::AssistantMessage { .. } => (SurfaceNodeKind::Assistant, None, 0),
            _ => return Err(SurfaceError::ToolResultWrongTarget),
        };
        let estimated_tokens = message
            .as_ref()
            .map(estimate_message)
            .transpose()
            .map_err(context_budget_surface_error)?
            .unwrap_or(0);
        let tool_result = if matches!(event.kind, EventKind::ToolResult { .. }) {
            let masked = masked_data_sha256(event.data().as_value())
                .map_err(|_| SurfaceError::ToolResultChangedIdentity)?;
            Some(match row_binding {
                SurfaceRowBinding::Memory => ToolResultOrigin::Memory { masked },
                SurfaceRowBinding::PendingDurable => ToolResultOrigin::PendingDurable { masked },
                SurfaceRowBinding::Durable(row) => {
                    if row.seq() != event.seq() {
                        return Err(SurfaceError::ToolResultChangedIdentity);
                    }
                    ToolResultOrigin::Durable { masked, row }
                }
            })
        } else {
            None
        };
        Ok(SurfaceNode {
            seq: event.seq,
            kind,
            message,
            estimated_tokens,
            tool_delta,
            tool_result,
        })
    }

    fn bind_tool_result_row(&mut self, seq: EventSeq, row: JournalRowLocator) -> bool {
        let Some(nodes) = Arc::get_mut(&mut self.surface_nodes) else {
            return false;
        };
        let Some(node) = nodes.iter_mut().find(|node| node.seq == seq) else {
            return false;
        };
        let Some(ToolResultOrigin::PendingDurable { masked }) = node.tool_result else {
            return false;
        };
        node.tool_result = Some(ToolResultOrigin::Durable { masked, row });
        true
    }

    pub(super) fn bind_recovery_tool_result_rows(
        &mut self,
        rows: impl IntoIterator<Item = JournalRowLocator>,
    ) -> bool {
        for row in rows {
            let Some(node) = self.surface_nodes.iter().find(|node| node.seq == row.seq()) else {
                continue;
            };
            if matches!(node.kind, SurfaceNodeKind::ToolResult)
                && !self.bind_tool_result_row(row.seq(), row)
            {
                return false;
            }
        }
        true
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
                if admission.is_unprivileged_durable() && canonical_not_started {
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
        let Some(original_origin) = shadowed[0].tool_result else {
            return Err(SurfaceError::ToolResultWrongTarget);
        };
        let original_identity = match original_origin {
            ToolResultOrigin::Memory { masked }
            | ToolResultOrigin::PendingDurable { masked }
            | ToolResultOrigin::Durable { masked, .. } => masked,
        };
        if !matches!(replacement.kind, EventKind::ToolResult { .. }) {
            return Err(SurfaceError::ToolResultWrongTarget);
        }
        let replacement_identity = masked_data_sha256(replacement.data().as_value())
            .map_err(|_| SurfaceError::ToolResultChangedIdentity)?;
        if original_identity != replacement_identity {
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

fn surface_price_facts(node: &SurfaceNode) -> SurfacePriceFacts {
    SurfacePriceFacts {
        tokens: node.estimated_tokens,
        tool_delta: node.tool_delta,
    }
}

fn context_budget_surface_error(error: ContextBudgetError) -> SurfaceError {
    match error {
        ContextBudgetError::TokenOverflow => SurfaceError::TokenAccountingOverflow,
        ContextBudgetError::UnbalancedToolSurface => SurfaceError::UnbalancedToolSurface,
    }
}

fn event_is_immediately_after(current: EventSeq, previous: EventSeq) -> bool {
    previous.get().checked_add(1) == Some(current.get())
}

fn checkpoint_source_matches(message: &Message, open: &OpenCompaction) -> bool {
    let Some(fields) = message.source().raw().as_value().as_object() else {
        return false;
    };
    let expected_len = if open.source_command_id.is_some() {
        4
    } else {
        3
    };
    fields.len() == expected_len
        && fields.get("kind").and_then(serde_json::Value::as_str) == Some("plugin")
        && fields.get("plugin").and_then(serde_json::Value::as_str)
            == Some(COMPACTION_CHECKPOINT_SOURCE)
        && fields
            .get("compactionId")
            .and_then(serde_json::Value::as_str)
            == Some(open.id.as_str())
        && match &open.source_command_id {
            Some(expected) => {
                fields
                    .get("sourceCommandId")
                    .and_then(serde_json::Value::as_str)
                    == Some(expected.as_str())
            }
            None => !fields.contains_key("sourceCommandId"),
        }
}

fn is_compaction_checkpoint_source(message: &Message) -> bool {
    let MessageSourceKind::Plugin { plugin, .. } = message.source().kind() else {
        return false;
    };
    plugin == COMPACTION_CHECKPOINT_SOURCE
        && message
            .source()
            .raw()
            .as_value()
            .get("compactionId")
            .and_then(serde_json::Value::as_str)
            .is_some()
}

fn canonical_text_block_matches(block: &crate::model::ContentBlock, expected: &str) -> bool {
    let Some(fields) = block.raw().as_value().as_object() else {
        return false;
    };
    fields.len() == 2
        && fields.get("type").and_then(serde_json::Value::as_str) == Some("text")
        && fields.get("text").and_then(serde_json::Value::as_str) == Some(expected)
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
            Self::CompactionStart { .. } => "compaction/start",
            Self::CompactionSummary { .. } => "compaction/summary",
            Self::CompactionEnd { .. } => "compaction/end",
            Self::CompactionPrune { .. } => "compaction/prune",
            Self::EndSeed => "session/end-seed",
            Self::Unknown { .. } => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use crate::{
        model::{CallId, ContentBlock, Message},
        session::{
            ApprovalAskedEvent, ApprovalDecidedEvent, ApprovalOutcome, ApprovalRequestId, Clock,
            ClockError, EventKind, EventSeq, NewEvent, Session, SessionEvent, SessionId, StepId,
            SurfaceError, SurfaceIntent, TOOL_NOT_STARTED, ToolFailure, TransitionError,
            TurnEndReason, TurnId, UnixMillis, journal_row::JournalRowLocator,
        },
    };

    use super::{
        COMPACTION_CHECKPOINT_PREFIX, COMPACTION_CHECKPOINT_SUFFIX, Projection, ValidationPolicy,
    };

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

    fn append_owned_prune(
        projection: &Projection,
        event: &SessionEvent,
    ) -> Result<Projection, crate::session::EventValidationError> {
        let row = crate::session::jsonl::encode_event_line(event).unwrap();
        let locator = JournalRowLocator::new(event.seq(), 0, &row).unwrap();
        projection
            .prepare_owned_prune_event(event)?
            .bind(locator)
            .ok_or_else(|| SurfaceError::ToolResultChangedIdentity.into())
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

    fn wire_event(
        event_type: &str,
        seq: u64,
        data: Value,
        surface_op: Option<Value>,
        sources: Option<Vec<u64>>,
    ) -> SessionEvent {
        let mut event = json!({
            "type": event_type,
            "seq": seq,
            "time": 7,
            "data": data,
        });
        if let Some(surface_op) = surface_op {
            event["surfaceOp"] = surface_op;
        }
        if let Some(sources) = sources {
            event["sourceEventSeqs"] = json!(sources);
        }
        crate::session::codec::decode_event(event, seq as usize).unwrap()
    }

    fn strict_compaction_trace() -> Vec<SessionEvent> {
        vec![
            wire_event("turn/start", 0, json!({ "turn": 1 }), None, None),
            wire_event(
                "user/message",
                1,
                json!({
                    "id": "user-before-compaction",
                    "role": "user",
                    "content": [{ "type": "text", "text": "x".repeat(4096) }],
                    "source": { "kind": "user" },
                }),
                Some(json!("append")),
                None,
            ),
            wire_event(
                "user/message",
                2,
                json!({
                    "id": "retained-after-compaction",
                    "role": "user",
                    "content": [{ "type": "text", "text": "latest context" }],
                    "source": { "kind": "user" },
                }),
                Some(json!("append")),
                None,
            ),
            wire_event(
                "turn/end",
                3,
                json!({ "turn": 1, "reason": { "kind": "completed" } }),
                None,
                None,
            ),
            wire_event("turn/start", 4, json!({ "turn": 2 }), None, None),
            wire_event(
                "compaction/start",
                5,
                json!({
                    "compactionId": "compact-strict-1",
                    "turn": 2,
                    "dispatch": {
                        "trigger": "pressure",
                        "sourceSurfaceGeneration": 2,
                        "shadowedRange": { "start": 1, "end": 1 },
                        "shadowedSeqs": [1],
                        "preparedCall": {
                            "config": { "provider": "summary-provider", "model": "summary-model" },
                            "adapterDefaults": {},
                            "retryPolicy": {
                                "mode": "always",
                                "retryableCodes": [],
                                "backoff": {
                                    "initialDelayMs": 1,
                                    "maxDelayMs": 1,
                                    "jitterRatio": 0
                                }
                            }
                        },
                        "system": "summary system",
                        "tools": [],
                        "sessionId": "strict-compaction",
                        "purpose": "compaction",
                        "instruction": {
                            "id": "compaction-instruction",
                            "role": "user",
                            "content": [{ "type": "text", "text": "Summarize safely." }],
                            "source": { "kind": "plugin", "plugin": "dsh.compaction" }
                        },
                        "instructionFormatVersion": 1
                    }
                }),
                None,
                None,
            ),
            wire_event(
                "compaction/summary",
                6,
                json!({
                    "compactionId": "compact-strict-1",
                    "summary": [{ "type": "text", "text": "condensed context", "kept": true }],
                    "rawOutput": [
                        { "type": "reasoning", "text": "private" },
                        { "type": "text", "text": "condensed context", "kept": true }
                    ],
                    "llmStreamCall": true,
                    "shadowedRange": { "start": 1, "end": 1 },
                    "shadowedSeqs": [1],
                    "shadowedTokenCount": 1032,
                    "provider": "summary-provider",
                    "model": "summary-model"
                }),
                None,
                None,
            ),
            wire_event(
                "user/message",
                7,
                json!({
                    "id": "compaction-checkpoint",
                    "role": "user",
                    "content": [
                        { "type": "text", "text": COMPACTION_CHECKPOINT_PREFIX },
                        { "type": "text", "text": "condensed context", "kept": true },
                        { "type": "text", "text": COMPACTION_CHECKPOINT_SUFFIX }
                    ],
                    "source": {
                        "kind": "plugin",
                        "plugin": "compact",
                        "compactionId": "compact-strict-1"
                    }
                }),
                Some(json!({ "op": "replace", "start": 1, "end": 1 })),
                Some(vec![5, 6, 1]),
            ),
            wire_event(
                "compaction/end",
                8,
                json!({ "compactionId": "compact-strict-1", "turn": 2 }),
                None,
                None,
            ),
        ]
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

    #[test]
    fn strict_compaction_requires_one_adjacent_four_event_bracket() {
        let events = strict_compaction_trace();
        assert!(Session::replay(&events).is_ok());
        let mut projection = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        for event in &events {
            projection = projection.with_event(event).unwrap();
        }

        assert_eq!(projection.interrupted_compaction_stage(), None);
        assert_eq!(projection.state().open_turn(), TurnId::new(2).ok());
        assert_eq!(
            projection.state().surface_nodes(),
            &[EventSeq::new(7).unwrap(), EventSeq::new(2).unwrap()]
        );
        assert_eq!(projection.messages().len(), 2);

        let mut without_replacement = events[..7].to_vec();
        without_replacement.push(wire_event(
            "compaction/end",
            7,
            json!({ "compactionId": "compact-strict-1", "turn": 2 }),
            None,
            None,
        ));
        let mut projection = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        let error = without_replacement
            .iter()
            .try_for_each(|event| {
                projection = projection.with_event(event)?;
                Ok::<_, crate::session::EventValidationError>(())
            })
            .unwrap_err();
        assert!(matches!(
            error,
            crate::session::EventValidationError::Transition(
                TransitionError::CompactionSuccessWithoutReplacement
            )
        ));
    }

    #[test]
    fn strict_compaction_checks_shadow_prices_and_requires_a_smaller_checkpoint() {
        let events = strict_compaction_trace();
        let mut projection = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        for event in &events[..6] {
            projection = projection.with_event(event).unwrap();
        }

        let mut wrong_price = serde_json::to_value(&events[6]).unwrap();
        wrong_price["data"]["shadowedTokenCount"] = json!(1031);
        let wrong_price = crate::session::codec::decode_event(wrong_price, 6).unwrap();
        assert!(matches!(
            projection.with_event(&wrong_price).unwrap_err(),
            crate::session::EventValidationError::Surface(
                SurfaceError::ShadowedTokenCountMismatch {
                    expected: 1032,
                    actual: 1031
                }
            )
        ));

        let mut large_summary = serde_json::to_value(&events[6]).unwrap();
        large_summary["data"]["summary"][0]["text"] = json!("y".repeat(4096));
        large_summary["data"]["rawOutput"][1]["text"] = json!("y".repeat(4096));
        let large_summary = crate::session::codec::decode_event(large_summary, 6).unwrap();
        let summarized = projection.with_event(&large_summary).unwrap();

        let mut nonshrinking = serde_json::to_value(&events[7]).unwrap();
        nonshrinking["data"]["content"][1]["text"] = json!("y".repeat(4096));
        let nonshrinking = crate::session::codec::decode_event(nonshrinking, 7).unwrap();
        assert!(matches!(
            summarized.with_event(&nonshrinking).unwrap_err(),
            crate::session::EventValidationError::Surface(SurfaceError::CompactionDoesNotShrink {
                shadowed: 1032,
                replacement: _
            })
        ));
    }

    #[test]
    fn strict_compaction_never_shadows_the_entire_surface() {
        let events = strict_compaction_trace();
        let mut projection = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        for event in &events[..5] {
            projection = projection.with_event(event).unwrap();
        }
        let mut whole_surface = serde_json::to_value(&events[5]).unwrap();
        whole_surface["data"]["dispatch"]["shadowedRange"] = json!({ "start": 1, "end": 2 });
        whole_surface["data"]["dispatch"]["shadowedSeqs"] = json!([1, 2]);
        let whole_surface = crate::session::codec::decode_event(whole_surface, 5).unwrap();

        assert!(matches!(
            projection.with_event(&whole_surface).unwrap_err(),
            crate::session::EventValidationError::Transition(
                TransitionError::CompactionDispatchMismatch(
                    "shadowed range is not the canonical balanced prefix"
                )
            )
        ));
        assert_eq!(
            projection.state().surface_nodes(),
            &[EventSeq::new(1).unwrap(), EventSeq::new(2).unwrap()]
        );
    }

    #[test]
    fn hot_replay_and_cold_scan_keep_identical_surface_prices() {
        let events = strict_compaction_trace();
        let mut hot = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        let mut cold = hot.clone();
        let mut compatible = Projection::empty(ValidationPolicy::MemoryCompatible);

        for event in &events {
            hot = hot.with_event(event).unwrap();
            cold.apply_scanned_event(event).unwrap();
            compatible = compatible.with_compatible_event(event).unwrap();
            assert_eq!(hot.surface_tokens, cold.surface_tokens);
            assert_eq!(hot.surface_tokens, compatible.surface_tokens);
            assert_eq!(hot.surface_nodes, cold.surface_nodes);
            assert_eq!(hot.surface_nodes, compatible.surface_nodes);
        }

        let messages = hot.messages();
        assert_eq!(
            hot.surface_tokens,
            messages
                .iter()
                .try_fold(0_u64, |total, message| {
                    total.checked_add(
                        crate::session::context_budget::estimate_message(message).unwrap(),
                    )
                })
                .unwrap()
        );
    }

    #[test]
    fn strict_prune_marker_uses_the_current_tool_result_price() {
        let prefix = [
            wire_event("turn/start", 0, json!({ "turn": 1 }), None, None),
            wire_event("step/start", 1, json!({ "turn": 1, "step": 1 }), None, None),
            wire_event(
                "assistant/message",
                2,
                json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "id": "assistant-prune-price",
                        "role": "assistant",
                        "content": [{
                            "type": "tool-call",
                            "id": "call-prune-price",
                            "name": "read",
                            "arguments": "{}"
                        }],
                        "source": { "kind": "model", "provider": "mock", "model": "mock" }
                    }
                }),
                Some(json!("append")),
                None,
            ),
            wire_event(
                "tool/call",
                3,
                json!({
                    "turn": 1,
                    "step": 1,
                    "callId": "call-prune-price",
                    "name": "read",
                    "arguments": "{}"
                }),
                None,
                None,
            ),
            wire_event(
                "tool/result",
                4,
                json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "id": "result-prune-price",
                        "role": "user",
                        "content": [{
                            "type": "tool-result",
                            "toolCallId": "call-prune-price",
                            "content": [{ "type": "text", "text": "x".repeat(47) }],
                            "isError": false
                        }],
                        "source": { "kind": "tool", "callId": "call-prune-price" }
                    },
                    "meta": { "retained": true }
                }),
                Some(json!("append")),
                Some(vec![3]),
            ),
        ];
        let mut projection = Projection::empty(ValidationPolicy::DurableStrict);
        for event in &prefix {
            projection = projection.with_event(event).unwrap();
        }
        let marker = |count| {
            wire_event(
                "compaction/prune",
                5,
                json!({
                    "shadowedRange": { "start": 4, "end": 4 },
                    "shadowedSeqs": [4],
                    "shadowedTokenCount": count
                }),
                None,
                None,
            )
        };

        assert!(matches!(
            projection.with_event(&marker(24)).unwrap_err(),
            crate::session::EventValidationError::Transition(
                TransitionError::DurablePruneEventNotAllowed {
                    event_type: "compaction/prune"
                }
            )
        ));
        assert!(matches!(
            append_owned_prune(&projection, &marker(23)).unwrap_err(),
            crate::session::EventValidationError::Surface(
                SurfaceError::ShadowedTokenCountMismatch {
                    expected: 24,
                    actual: 23
                }
            )
        ));
        let before_tokens = projection.surface_tokens;
        let replacement = wire_event(
            "tool/result",
            6,
            json!({
                "turn": 1,
                "step": 1,
                "message": {
                    "id": "result-prune-price",
                    "role": "user",
                    "content": [{
                        "type": "tool-result",
                        "toolCallId": "call-prune-price",
                        "content": [{
                            "type": "text",
                            "text": "xxxx\n\n[... tool result middle pruned ...]\n\nxxx"
                        }],
                        "isError": false
                    }],
                    "source": { "kind": "tool", "callId": "call-prune-price" }
                },
                "meta": { "retained": true }
            }),
            Some(json!({ "op": "replace", "start": 4, "end": 4 })),
            Some(vec![4]),
        );

        let mut hot = append_owned_prune(&projection, &marker(24)).unwrap();
        assert!(matches!(
            hot.with_event(&replacement).unwrap_err(),
            crate::session::EventValidationError::Transition(
                TransitionError::DurablePruneEventNotAllowed {
                    event_type: "tool/result"
                }
            )
        ));
        hot = append_owned_prune(&hot, &replacement).unwrap();
        assert_eq!(hot.surface_tokens, before_tokens);
        assert_eq!(
            hot.state().surface_nodes(),
            &[EventSeq::new(2).unwrap(), EventSeq::new(6).unwrap()]
        );
        assert_eq!(hot.orphan_prune_markers(), 0);

        let mut cold = projection.clone();
        cold.apply_scanned_event(&marker(24)).unwrap();
        cold.apply_scanned_event(&replacement).unwrap();
        assert_eq!(cold.state(), hot.state());
        assert_eq!(cold.surface_tokens, hot.surface_tokens);
        assert_eq!(cold.compaction, hot.compaction);
    }

    #[test]
    fn legacy_compaction_shapes_are_reader_only_not_live_append_shapes() {
        let start = wire_event(
            "compaction/start",
            0,
            json!({ "compactionId": "legacy", "turn": null }),
            None,
            None,
        );
        let end = wire_event(
            "compaction/end",
            1,
            json!({
                "compactionId": "legacy",
                "turn": null,
                "error": "legacy\nerror"
            }),
            None,
            None,
        );
        assert!(Session::replay(&[start.clone(), end]).is_ok());

        let mut live = Session::with_clock("live-legacy", FixedClock).unwrap();
        assert!(matches!(
            live.append(NewEvent::log(start.kind().clone()))
                .unwrap_err(),
            crate::session::AppendError::Validation(
                crate::session::EventValidationError::Transition(
                    TransitionError::DurableCompactionDispatchRequired
                )
            )
        ));
        assert!(live.events().is_empty());

        let strict = strict_compaction_trace();
        let mut projection = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        for event in &strict[..6] {
            projection = projection.with_event(event).unwrap();
        }
        let legacy_end = wire_event(
            "compaction/end",
            6,
            json!({
                "compactionId": "compact-strict-1",
                "turn": 2,
                "error": "legacy reader-only error"
            }),
            None,
            None,
        );
        assert!(matches!(
            projection.with_event(&legacy_end).unwrap_err(),
            crate::session::EventValidationError::Transition(
                TransitionError::DurableLegacyCompactionError
            )
        ));
    }

    #[test]
    fn strict_compaction_rejects_nonadjacent_or_mismatched_body_rows() {
        let events = strict_compaction_trace();
        let mut projection = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        for event in &events[..6] {
            projection = projection.with_event(event).unwrap();
        }
        let inserted = wire_event("todo/write", 6, json!({ "todos": [] }), None, None);
        assert!(matches!(
            projection.with_event(&inserted).unwrap_err(),
            crate::session::EventValidationError::Transition(
                TransitionError::CompactionBoundaryCrossed { .. }
            )
        ));

        let mut wrong_id = serde_json::to_value(&events[6]).unwrap();
        wrong_id["data"]["compactionId"] = json!("different");
        let wrong_id = crate::session::codec::decode_event(wrong_id, 6).unwrap();
        assert!(matches!(
            projection.with_event(&wrong_id).unwrap_err(),
            crate::session::EventValidationError::Transition(
                TransitionError::CompactionIdMismatch { .. }
            )
        ));

        let mut without_start = Projection::for_session(
            ValidationPolicy::DurableStrict,
            SessionId::new("strict-compaction"),
        );
        for event in &events[..5] {
            without_start = without_start.with_event(event).unwrap();
        }
        let checkpoint = wire_event(
            "user/message",
            5,
            json!({
                "id": "forged-checkpoint",
                "role": "user",
                "content": [
                    { "type": "text", "text": COMPACTION_CHECKPOINT_PREFIX },
                    { "type": "text", "text": "forged" },
                    { "type": "text", "text": COMPACTION_CHECKPOINT_SUFFIX }
                ],
                "source": {
                    "kind": "plugin",
                    "plugin": "compact",
                    "compactionId": "forged"
                }
            }),
            Some(json!({ "op": "replace", "start": 1, "end": 1 })),
            Some(vec![1]),
        );
        assert!(matches!(
            without_start.with_event(&checkpoint).unwrap_err(),
            crate::session::EventValidationError::Transition(
                TransitionError::CompactionWithoutStart { .. }
            )
        ));
        assert_eq!(
            without_start.state().surface_nodes(),
            &[EventSeq::new(1).unwrap(), EventSeq::new(2).unwrap()]
        );
    }

    #[test]
    fn strict_compaction_cannot_start_with_unresolved_tool_or_approval_work() {
        let start = |seq: u64| {
            let mut value = serde_json::to_value(&strict_compaction_trace()[5]).unwrap();
            value["seq"] = json!(seq);
            value["data"]["turn"] = json!(1);
            value["data"]["dispatch"]["trigger"] = json!("context-overflow");
            value["data"]["dispatch"]["sourceSurfaceGeneration"] = json!(1);
            value["data"]["dispatch"]["shadowedRange"] = json!({ "start": 2, "end": 2 });
            value["data"]["dispatch"]["shadowedSeqs"] = json!([2]);
            crate::session::codec::decode_event(value, seq as usize).unwrap()
        };

        let mut pending_call = Session::with_clock("pending-call", FixedClock).unwrap();
        let mut events = open_step(&mut pending_call, &[("call-1", "read", "{}")]);
        events.push(start(3));
        assert!(matches!(
            strict_prefix(&events).unwrap_err(),
            TransitionError::CompactionDispatchMismatch(
                "compaction cannot start with unresolved approval or tool work"
            )
        ));

        let mut pending_approval = Session::with_clock("pending-approval", FixedClock).unwrap();
        let mut events = open_step(&mut pending_approval, &[("call-1", "read", "{}")]);
        events.push(append(
            &mut pending_approval,
            NewEvent::log(EventKind::tool_call(turn(), step(), "call-1", "read", "{}")),
        ));
        events.push(append(
            &mut pending_approval,
            NewEvent::log(EventKind::approval_asked(
                ApprovalAskedEvent::new(
                    ApprovalRequestId::new("approval-1"),
                    "read",
                    Some(CallId::new("call-1")),
                    None,
                )
                .unwrap(),
            )),
        ));
        events.push(start(5));
        assert!(matches!(
            strict_prefix(&events).unwrap_err(),
            TransitionError::CompactionDispatchMismatch(
                "compaction cannot start with unresolved approval or tool work"
            )
        ));
    }

    #[test]
    fn prune_marker_is_consumed_only_by_its_exact_adjacent_tool_result_rewrite() {
        let events = vec![
            wire_event("turn/start", 0, json!({ "turn": 1 }), None, None),
            wire_event("step/start", 1, json!({ "turn": 1, "step": 1 }), None, None),
            wire_event(
                "tool/call",
                2,
                json!({
                    "turn": 1,
                    "step": 1,
                    "callId": "call-1",
                    "name": "read",
                    "arguments": "{}"
                }),
                None,
                None,
            ),
            wire_event(
                "tool/result",
                3,
                json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "id": "result-1",
                        "role": "user",
                        "source": { "kind": "tool", "callId": "call-1" },
                        "content": [{
                            "type": "tool-result",
                            "toolCallId": "call-1",
                            "content": [{ "type": "text", "text": "large result" }],
                            "isError": false
                        }]
                    },
                    "meta": { "kept": true }
                }),
                Some(json!("append")),
                None,
            ),
            wire_event(
                "compaction/prune",
                4,
                json!({
                    "shadowedRange": { "start": 3, "end": 3 },
                    "shadowedSeqs": [3],
                    "shadowedTokenCount": 10
                }),
                None,
                None,
            ),
            wire_event(
                "tool/result",
                5,
                json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "id": "result-1",
                        "role": "user",
                        "source": { "kind": "tool", "callId": "call-1" },
                        "content": [{
                            "type": "tool-result",
                            "toolCallId": "call-1",
                            "content": [{ "type": "text", "text": "pruned" }],
                            "isError": false
                        }]
                    },
                    "meta": { "kept": true }
                }),
                Some(json!({ "op": "replace", "start": 3, "end": 3 })),
                Some(vec![3]),
            ),
        ];
        let mut projection = Projection::empty(ValidationPolicy::MemoryCompatible);
        for event in &events {
            projection = projection.with_event(event).unwrap();
        }
        assert_eq!(
            projection.state().surface_nodes(),
            &[EventSeq::new(5).unwrap()]
        );
        assert_eq!(projection.orphan_prune_markers(), 0);

        let mut marker_only = Projection::empty(ValidationPolicy::MemoryCompatible);
        for event in &events[..5] {
            marker_only = marker_only.with_event(event).unwrap();
        }
        marker_only = marker_only
            .with_event(&wire_event(
                "todo/write",
                5,
                json!({ "todos": [] }),
                None,
                None,
            ))
            .unwrap();
        assert_eq!(marker_only.orphan_prune_markers(), 1);

        let mut mismatch = Projection::empty(ValidationPolicy::MemoryCompatible);
        for event in &events[..5] {
            mismatch = mismatch.with_event(event).unwrap();
        }
        let mut wrong = serde_json::to_value(&events[5]).unwrap();
        wrong["sourceEventSeqs"] = json!([2]);
        let wrong = crate::session::codec::decode_event(wrong, 5).unwrap();
        assert!(matches!(
            mismatch.with_event(&wrong).unwrap_err(),
            crate::session::EventValidationError::Surface(SurfaceError::PruneReplacementMismatch)
        ));

        let mut wrong_kind = Projection::empty(ValidationPolicy::MemoryCompatible);
        for event in &events[..5] {
            wrong_kind = wrong_kind.with_event(event).unwrap();
        }
        let unrelated_replacement = wire_event(
            "user/message",
            5,
            json!({
                "id": "unrelated-replacement",
                "role": "user",
                "content": [{ "type": "text", "text": "not a pruned tool result" }],
                "source": { "kind": "user" }
            }),
            Some(json!({ "op": "replace", "start": 3, "end": 3 })),
            Some(vec![3]),
        );
        assert!(matches!(
            wrong_kind.with_event(&unrelated_replacement).unwrap_err(),
            crate::session::EventValidationError::Surface(SurfaceError::PruneReplacementMismatch)
        ));
    }
}
