use std::fmt;

use serde_json::Value;
use thiserror::Error;

use crate::session::{
    ApprovalOutcome, CommittedUiKind, EventSeq, StepId, TOOL_OUTCOME_UNKNOWN, TurnId, UiIdentity,
    UiOpaquePayload, UiTokenUsage, UiTurnEndReason, UiUserSource,
};

const MAX_PROJECTED_TOOLS: usize = 256;
const MAX_PROJECTED_APPROVALS: usize = 256;
const MAX_SUMMARY_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolActivityState {
    Preparing,
    AwaitingApproval,
    Allowed,
    Completed,
    Failed,
    Denied,
    Cancelled,
    Unavailable,
    OutcomeUnknown,
}

impl ToolActivityState {
    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Failed
                | Self::Denied
                | Self::Cancelled
                | Self::Unavailable
                | Self::OutcomeUnknown
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolActivityOrigin {
    CorrelatedCall,
    UnattributedResult,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PatchActivityOperation {
    Create,
    Update,
}

pub(crate) struct ToolActivity {
    pub(crate) turn: TurnId,
    pub(crate) step: StepId,
    pub(crate) call_id: UiIdentity,
    pub(crate) name: UiIdentity,
    pub(crate) origin: ToolActivityOrigin,
    pub(crate) summary: Option<String>,
    pub(crate) state: ToolActivityState,
    pub(crate) is_error: Option<bool>,
    pub(crate) failure_code: Option<String>,
    pub(crate) payload_omitted: bool,
    pub(crate) result_bytes: usize,
    pub(crate) meta_bytes: usize,
    pub(crate) committed_effect: Option<bool>,
    pub(crate) patch_path: Option<String>,
    pub(crate) patch_operation: Option<PatchActivityOperation>,
    pub(crate) patch_additions: Option<usize>,
    pub(crate) patch_removals: Option<usize>,
    pub(crate) patch_cleanup_warning: Option<bool>,
    pub(crate) started_process: Option<bool>,
    pub(crate) shell_exit_code: Option<i64>,
    pub(crate) shell_signal: Option<String>,
    pub(crate) shell_timed_out: Option<bool>,
    pub(crate) plugin_id: Option<String>,
    pub(crate) plugin_dispatched: Option<bool>,
    pub(crate) plugin_peer_settled: Option<bool>,
    pub(crate) plugin_quiescent: Option<bool>,
    pub(crate) conflicting_effect: bool,
}

impl fmt::Debug for ToolActivity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolActivity")
            .field("turn", &self.turn)
            .field("step", &self.step)
            .field("call_id_bytes", &self.call_id.len())
            .field("name_bytes", &self.name.len())
            .field("origin", &self.origin)
            .field(
                "summary_bytes",
                &self.summary.as_ref().map_or(0, String::len),
            )
            .field("state", &self.state)
            .field("is_error", &self.is_error)
            .field(
                "failure_code_bytes",
                &self.failure_code.as_ref().map_or(0, String::len),
            )
            .field("payload_omitted", &self.payload_omitted)
            .field("result_bytes", &self.result_bytes)
            .field("meta_bytes", &self.meta_bytes)
            .field("committed_effect", &self.committed_effect)
            .field(
                "patch_path_bytes",
                &self.patch_path.as_ref().map_or(0, String::len),
            )
            .field("patch_operation", &self.patch_operation)
            .field("patch_additions", &self.patch_additions)
            .field("patch_removals", &self.patch_removals)
            .field("patch_cleanup_warning", &self.patch_cleanup_warning)
            .field("started_process", &self.started_process)
            .field("shell_exit_code", &self.shell_exit_code)
            .field(
                "shell_signal_bytes",
                &self.shell_signal.as_ref().map_or(0, String::len),
            )
            .field("shell_timed_out", &self.shell_timed_out)
            .field(
                "plugin_id_bytes",
                &self.plugin_id.as_ref().map_or(0, String::len),
            )
            .field("plugin_dispatched", &self.plugin_dispatched)
            .field("plugin_peer_settled", &self.plugin_peer_settled)
            .field("plugin_quiescent", &self.plugin_quiescent)
            .field("conflicting_effect", &self.conflicting_effect)
            .finish()
    }
}

struct ApprovalLink {
    id: UiIdentity,
    turn: TurnId,
    step: StepId,
    call_id: UiIdentity,
}

impl fmt::Debug for ApprovalLink {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApprovalLink")
            .field("id_bytes", &self.id.len())
            .field("turn", &self.turn)
            .field("step", &self.step)
            .field("call_id_bytes", &self.call_id.len())
            .finish()
    }
}

struct ToolResultFact<'a> {
    turn: TurnId,
    step: StepId,
    call_id: &'a UiIdentity,
    is_error: bool,
    failure_code: Option<&'a str>,
    content: &'a UiOpaquePayload,
    meta: &'a UiOpaquePayload,
    surface_replacement_target: Option<EventSeq>,
}

#[derive(Clone, Copy)]
struct PendingPrune {
    target: EventSeq,
    shadowed_tokens: u64,
}

pub(crate) struct ContextStatus {
    pub(crate) provider: Option<UiIdentity>,
    pub(crate) model: Option<UiIdentity>,
    pub(crate) window: Option<u64>,
}

impl fmt::Debug for ContextStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextStatus")
            .field(
                "provider_bytes",
                &self.provider.as_ref().map_or(0, |value| value.len()),
            )
            .field(
                "model_bytes",
                &self.model.as_ref().map_or(0, |value| value.len()),
            )
            .field("window", &self.window)
            .finish()
    }
}

pub(crate) struct CompactionStatus {
    pub(crate) id: UiIdentity,
    pub(crate) phase: CompactionPhase,
    pub(crate) shadowed_tokens: Option<u64>,
    pub(crate) error_code: Option<String>,
}

impl fmt::Debug for CompactionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CompactionStatus")
            .field("id_bytes", &self.id.len())
            .field("phase", &self.phase)
            .field("shadowed_tokens", &self.shadowed_tokens)
            .field(
                "error_code_bytes",
                &self.error_code.as_ref().map_or(0, String::len),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CompactionPhase {
    Started,
    Summarized,
    Completed,
    Failed,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub(crate) enum UiProjectionError {
    #[error("CLI_UI_CAPACITY")]
    Capacity,
}

/// Bounded semantic state derived only from committed Session facts.
///
/// Renderers consume this state; they do not correlate call IDs or reinterpret
/// tool completion on their own.
#[derive(Default)]
pub(crate) struct UiProjector {
    tools: Vec<ToolActivity>,
    approvals: Vec<ApprovalLink>,
    context: Option<ContextStatus>,
    compaction: Option<CompactionStatus>,
    last_usage: Option<UiTokenUsage>,
    last_human_prompt_bytes: Option<usize>,
    last_human_omitted_parts: usize,
    retry_count: u32,
    omitted_tool_facts: usize,
    omitted_approval_facts: usize,
    last_prune_shadowed_tokens: Option<u64>,
    pending_prune: Option<PendingPrune>,
    orphan_prune_markers: usize,
    conflicting_facts: usize,
    compaction_usage: Option<UiTokenUsage>,
    degraded: bool,
}

impl fmt::Debug for UiProjector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiProjector")
            .field("tool_count", &self.tools.len())
            .field("approval_count", &self.approvals.len())
            .field("has_context", &self.context.is_some())
            .field(
                "compaction_phase",
                &self.compaction.as_ref().map(|item| item.phase),
            )
            .field("status", &self.status())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct UiProjectorStatus {
    pub(crate) last_usage: Option<UiTokenUsage>,
    pub(crate) last_human_prompt_bytes: Option<usize>,
    pub(crate) last_human_omitted_parts: usize,
    pub(crate) retry_count: u32,
    pub(crate) omitted_tool_facts: usize,
    pub(crate) omitted_approval_facts: usize,
    pub(crate) last_prune_shadowed_tokens: Option<u64>,
    pub(crate) pending_prune_shadowed_tokens: Option<u64>,
    pub(crate) orphan_prune_markers: usize,
    pub(crate) conflicting_facts: usize,
    pub(crate) compaction_usage: Option<UiTokenUsage>,
    pub(crate) degraded: bool,
}

impl UiProjector {
    pub(crate) fn observe(&mut self, event: &CommittedUiKind) -> Result<(), UiProjectionError> {
        self.resolve_pending_prune(event);
        match event {
            CommittedUiKind::UserMessage { source, content } => match source {
                UiUserSource::Human => {
                    self.last_human_prompt_bytes = Some(content.original_bytes());
                    self.last_human_omitted_parts = content.omitted_parts();
                }
                UiUserSource::Context { plugin, form } => {
                    let _ = (plugin, form, content.original_bytes());
                }
                UiUserSource::Other { kind } => {
                    let _ = (kind, content.original_bytes());
                }
            },
            CommittedUiKind::UsageSample { usage, .. } => self.last_usage = Some(*usage),
            CommittedUiKind::AssistantMessage {
                provider,
                model,
                usage,
                ..
            } => {
                let route_changed = self.context.as_ref().is_some_and(|context| {
                    context.provider.as_ref() != Some(provider)
                        || context.model.as_ref() != Some(model)
                });
                if self.context.is_none() || route_changed {
                    self.context = Some(ContextStatus {
                        provider: Some(provider.clone()),
                        model: Some(model.clone()),
                        // A window belongs to the request context that named
                        // it. Never combine an older window with a new route.
                        window: None,
                    });
                }
                if let Some(usage) = usage {
                    self.last_usage = Some(*usage);
                }
            }
            CommittedUiKind::ToolRequested {
                turn,
                step,
                call_id,
                name,
                arguments,
                ..
            } => self.request_tool(*turn, *step, call_id, name, arguments)?,
            CommittedUiKind::ApprovalAsked {
                id,
                tool_name,
                call_id,
                ..
            } => {
                if let Some(call_id) = call_id {
                    if let Some((turn, step)) = self.link_approval(id, call_id, tool_name)? {
                        self.set_tool_state(
                            turn,
                            step,
                            call_id,
                            ToolActivityState::AwaitingApproval,
                        )?;
                    }
                }
            }
            CommittedUiKind::ApprovalDecided { id, outcome } => {
                if let Some(link) = self.approvals.iter().find(|link| &link.id == id) {
                    let turn = link.turn;
                    let step = link.step;
                    let call_id = link.call_id.clone();
                    let state = match outcome {
                        ApprovalOutcome::AllowedOnce => ToolActivityState::Allowed,
                        ApprovalOutcome::Rejected => ToolActivityState::Denied,
                        ApprovalOutcome::Cancelled => ToolActivityState::Cancelled,
                        ApprovalOutcome::Unavailable => ToolActivityState::Unavailable,
                    };
                    self.set_tool_state(turn, step, &call_id, state)?;
                }
            }
            CommittedUiKind::ToolResult {
                turn,
                step,
                call_id,
                is_error,
                failure,
                content,
                meta,
                surface_replacement_target,
                ..
            } => self.finish_tool(ToolResultFact {
                turn: *turn,
                step: *step,
                call_id,
                is_error: *is_error,
                failure_code: failure.as_ref().map(|item| item.code.as_str()),
                content,
                meta,
                surface_replacement_target: *surface_replacement_target,
            })?,
            CommittedUiKind::RequestContextChanged {
                provider,
                model,
                context_window,
            } => {
                self.context = Some(ContextStatus {
                    provider: provider.clone(),
                    model: model.clone(),
                    window: *context_window,
                });
            }
            CommittedUiKind::CompactionStarted {
                id,
                turn,
                trigger,
                shadowed_nodes,
            } => {
                let _ = (turn, trigger, shadowed_nodes);
                self.compaction = Some(CompactionStatus {
                    id: id.clone(),
                    phase: CompactionPhase::Started,
                    shadowed_tokens: None,
                    error_code: None,
                });
            }
            CommittedUiKind::CompactionSummarized {
                id,
                shadowed_tokens,
                provider,
                model,
                usage,
            } => {
                let _ = (provider, model);
                self.ensure_compaction(id)?;
                if let Some(compaction) = self.compaction.as_mut() {
                    compaction.phase = CompactionPhase::Summarized;
                    compaction.shadowed_tokens = Some(*shadowed_tokens);
                }
                if let Some(usage) = usage {
                    self.compaction_usage = Some(*usage);
                }
            }
            CommittedUiKind::CompactionEnded { id, turn, error } => {
                let _ = turn;
                self.ensure_compaction(id)?;
                if let Some(compaction) = self.compaction.as_mut() {
                    compaction.phase = if error.is_some() {
                        CompactionPhase::Failed
                    } else {
                        CompactionPhase::Completed
                    };
                    compaction.error_code = error.as_ref().and_then(|item| item.code.clone());
                    if let Some(error) = error {
                        let _ = &error.message;
                    }
                }
            }
            CommittedUiKind::CompactionPruneMarked {
                target,
                shadowed_tokens,
            } => {
                self.pending_prune = Some(PendingPrune {
                    target: *target,
                    shadowed_tokens: *shadowed_tokens,
                });
            }
            CommittedUiKind::RetryScheduled {
                provider,
                delay_ms,
                max_retries,
                failure_code,
                failure_message,
                ..
            } => {
                let _ = (
                    provider,
                    delay_ms,
                    max_retries,
                    failure_code,
                    failure_message,
                );
                self.retry_count = self.retry_count.saturating_add(1);
            }
            CommittedUiKind::TurnEnd { turn, reason } => {
                if let UiTurnEndReason::Aborted { cause } = reason {
                    let _ = cause;
                }
                for tool in &mut self.tools {
                    if tool.turn == *turn
                        && tool.is_error.is_none()
                        && matches!(
                            tool.state,
                            ToolActivityState::Preparing
                                | ToolActivityState::AwaitingApproval
                                | ToolActivityState::Allowed
                        )
                    {
                        tool.state = ToolActivityState::OutcomeUnknown;
                    }
                }
            }
            CommittedUiKind::TurnStart { .. } => {
                self.tools.clear();
                self.approvals.clear();
                self.compaction = None;
                self.last_usage = None;
                self.last_human_prompt_bytes = None;
                self.last_human_omitted_parts = 0;
                self.retry_count = 0;
                self.omitted_tool_facts = 0;
                self.omitted_approval_facts = 0;
                self.last_prune_shadowed_tokens = None;
                self.pending_prune = None;
                self.orphan_prune_markers = 0;
                self.conflicting_facts = 0;
                self.compaction_usage = None;
                self.degraded = false;
            }
            CommittedUiKind::StepStart { .. }
            | CommittedUiKind::StepEnd { .. }
            | CommittedUiKind::AssistantTextDelta { .. }
            | CommittedUiKind::AssistantReasoningDelta { .. }
            | CommittedUiKind::RetryStarted { .. } => {}
            CommittedUiKind::TypeOnly { event_type } => {
                if *event_type == "session/end-seed" {
                    self.compaction = None;
                    self.pending_prune = None;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn tools(&self) -> &[ToolActivity] {
        &self.tools
    }

    pub(crate) fn context(&self) -> Option<&ContextStatus> {
        self.context.as_ref()
    }

    pub(crate) fn compaction(&self) -> Option<&CompactionStatus> {
        self.compaction.as_ref()
    }

    pub(crate) fn status(&self) -> UiProjectorStatus {
        UiProjectorStatus {
            last_usage: self.last_usage,
            last_human_prompt_bytes: self.last_human_prompt_bytes,
            last_human_omitted_parts: self.last_human_omitted_parts,
            retry_count: self.retry_count,
            omitted_tool_facts: self.omitted_tool_facts,
            omitted_approval_facts: self.omitted_approval_facts,
            last_prune_shadowed_tokens: self.last_prune_shadowed_tokens,
            pending_prune_shadowed_tokens: self.pending_prune.map(|item| item.shadowed_tokens),
            orphan_prune_markers: self.orphan_prune_markers,
            conflicting_facts: self.conflicting_facts,
            compaction_usage: self.compaction_usage,
            degraded: self.degraded,
        }
    }

    pub(crate) fn mark_degraded(&mut self) {
        self.degraded = true;
    }

    fn request_tool(
        &mut self,
        turn: TurnId,
        step: StepId,
        call_id: &UiIdentity,
        name: &UiIdentity,
        arguments: &UiOpaquePayload,
    ) -> Result<(), UiProjectionError> {
        if self
            .tools
            .iter()
            .any(|tool| tool.turn == turn && tool.step == step && &tool.call_id == call_id)
        {
            // A second committed intent with the same scoped call ID is never
            // a second execution. DurableStrict rejects it; imported/memory
            // facts degrade to one card with an explicit conflict count.
            self.conflicting_facts = self.conflicting_facts.saturating_add(1);
            return Ok(());
        }
        if self.tools.len() == MAX_PROJECTED_TOOLS {
            self.omitted_tool_facts = self.omitted_tool_facts.saturating_add(1);
            return Ok(());
        }
        self.tools
            .try_reserve(1)
            .map_err(|_| UiProjectionError::Capacity)?;
        self.tools.push(ToolActivity {
            turn,
            step,
            call_id: call_id.clone(),
            name: name.clone(),
            origin: ToolActivityOrigin::CorrelatedCall,
            summary: summarize_arguments(name.as_str(), arguments)?,
            state: ToolActivityState::Preparing,
            is_error: None,
            failure_code: None,
            payload_omitted: arguments.was_omitted(),
            result_bytes: 0,
            meta_bytes: 0,
            committed_effect: None,
            patch_path: None,
            patch_operation: None,
            patch_additions: None,
            patch_removals: None,
            patch_cleanup_warning: None,
            started_process: None,
            shell_exit_code: None,
            shell_signal: None,
            shell_timed_out: None,
            plugin_id: None,
            plugin_dispatched: None,
            plugin_peer_settled: None,
            plugin_quiescent: None,
            conflicting_effect: false,
        });
        Ok(())
    }

    fn link_approval(
        &mut self,
        id: &UiIdentity,
        call_id: &UiIdentity,
        tool_name: &UiIdentity,
    ) -> Result<Option<(TurnId, StepId)>, UiProjectionError> {
        if self.approvals.iter().any(|link| &link.id == id) {
            self.conflicting_facts = self.conflicting_facts.saturating_add(1);
            return Ok(None);
        }
        let mut candidates = self.tools.iter().rev().filter(|tool| {
            &tool.call_id == call_id && &tool.name == tool_name && !tool.state.is_terminal()
        });
        let Some(candidate) = candidates.next() else {
            return Ok(None);
        };
        let (turn, step) = (candidate.turn, candidate.step);
        let ambiguous = candidates.next().is_some();
        drop(candidates);
        if ambiguous {
            self.conflicting_facts = self.conflicting_facts.saturating_add(1);
            return Ok(None);
        }
        if self.approvals.len() == MAX_PROJECTED_APPROVALS {
            self.omitted_approval_facts = self.omitted_approval_facts.saturating_add(1);
            return Ok(None);
        }
        self.approvals
            .try_reserve(1)
            .map_err(|_| UiProjectionError::Capacity)?;
        self.approvals.push(ApprovalLink {
            id: id.clone(),
            turn,
            step,
            call_id: call_id.clone(),
        });
        Ok(Some((turn, step)))
    }

    fn set_tool_state(
        &mut self,
        turn: TurnId,
        step: StepId,
        call_id: &UiIdentity,
        state: ToolActivityState,
    ) -> Result<(), UiProjectionError> {
        let Some(tool) = self
            .tools
            .iter_mut()
            .find(|tool| tool.turn == turn && tool.step == step && &tool.call_id == call_id)
        else {
            // A memory-compatible or imported Session can contain a valid
            // generic approval without one of the bounded tool cards retained
            // by this live projector. Keep the Session fact and degrade the UI.
            return Ok(());
        };
        if tool.state.is_terminal() {
            // Memory-compatible/imported logs can settle a generic approval
            // after the correlated result. Preserve the stronger result fact.
            return Ok(());
        }
        tool.state = state;
        Ok(())
    }

    fn finish_tool(&mut self, fact: ToolResultFact<'_>) -> Result<(), UiProjectionError> {
        let ToolResultFact {
            turn,
            step,
            call_id,
            is_error,
            failure_code,
            content,
            meta,
            surface_replacement_target,
        } = fact;
        if surface_replacement_target.is_some() {
            // This is a historical surface rewrite, normally the second half
            // of a prune pair. It is not another tool execution.
            return Ok(());
        }
        if !self
            .tools
            .iter()
            .any(|tool| tool.turn == turn && tool.step == step && &tool.call_id == call_id)
        {
            if self.tools.len() == MAX_PROJECTED_TOOLS {
                self.omitted_tool_facts = self.omitted_tool_facts.saturating_add(1);
                return Ok(());
            }
            self.tools
                .try_reserve(1)
                .map_err(|_| UiProjectionError::Capacity)?;
            self.tools.push(ToolActivity {
                turn,
                step,
                call_id: call_id.clone(),
                name: UiIdentity::from_static("unattributed result"),
                origin: ToolActivityOrigin::UnattributedResult,
                summary: None,
                state: ToolActivityState::Preparing,
                is_error: None,
                failure_code: None,
                payload_omitted: false,
                result_bytes: 0,
                meta_bytes: 0,
                committed_effect: None,
                patch_path: None,
                patch_operation: None,
                patch_additions: None,
                patch_removals: None,
                patch_cleanup_warning: None,
                started_process: None,
                shell_exit_code: None,
                shell_signal: None,
                shell_timed_out: None,
                plugin_id: None,
                plugin_dispatched: None,
                plugin_peer_settled: None,
                plugin_quiescent: None,
                conflicting_effect: false,
            });
        }
        let tool = self
            .tools
            .iter_mut()
            .find(|tool| tool.turn == turn && tool.step == step && &tool.call_id == call_id)
            .expect("a missing tool was inserted immediately above");
        if tool.is_error.is_some() || matches!(tool.state, ToolActivityState::OutcomeUnknown) {
            self.conflicting_facts = self.conflicting_facts.saturating_add(1);
            return Ok(());
        }
        if !matches!(
            tool.state,
            ToolActivityState::Denied
                | ToolActivityState::Cancelled
                | ToolActivityState::Unavailable
        ) {
            tool.state = if failure_code == Some(TOOL_OUTCOME_UNKNOWN) {
                ToolActivityState::OutcomeUnknown
            } else if is_error || failure_code.is_some() {
                ToolActivityState::Failed
            } else {
                ToolActivityState::Completed
            };
        }
        tool.is_error = Some(is_error);
        tool.failure_code = failure_code.map(copy).transpose()?;
        tool.result_bytes = content.original_bytes();
        tool.meta_bytes = meta.original_bytes();
        if let Some(meta) = meta
            .as_str()
            .and_then(|value| serde_json::from_str::<Value>(value).ok())
        {
            if tool.name.as_str() == "apply_patch" {
                if let Some(facts) = patch_facts(&meta) {
                    tool.committed_effect = Some(facts.committed);
                    tool.patch_path = Some(bounded_summary(facts.path)?);
                    tool.patch_operation = Some(facts.operation);
                    let (additions, removals) = unified_diffstat(facts.diff);
                    tool.patch_additions = Some(additions);
                    tool.patch_removals = Some(removals);
                    tool.patch_cleanup_warning = facts.cleanup_warning;
                }
            }
            if tool.name.as_str() == "bash" {
                if let Some(facts) = shell_facts(&meta) {
                    tool.started_process = Some(facts.started);
                    tool.shell_exit_code = facts.exit_code;
                    tool.shell_signal = facts.signal.map(copy).transpose()?;
                    tool.shell_timed_out = facts.timed_out;
                }
            }
            if let Some(facts) = plugin_facts(&meta) {
                tool.plugin_id = Some(bounded_summary(facts.id)?);
                tool.plugin_dispatched = Some(facts.dispatched);
                tool.plugin_peer_settled = Some(facts.peer_settled);
                tool.plugin_quiescent = Some(facts.quiescent);
            }
        }
        tool.conflicting_effect = matches!(
            tool.state,
            ToolActivityState::Denied
                | ToolActivityState::Cancelled
                | ToolActivityState::Unavailable
        ) && (tool.committed_effect == Some(true)
            || tool.started_process == Some(true)
            || tool.plugin_dispatched == Some(true));
        Ok(())
    }

    fn ensure_compaction(&mut self, id: &UiIdentity) -> Result<(), UiProjectionError> {
        if self
            .compaction
            .as_ref()
            .is_some_and(|compaction| &compaction.id == id)
        {
            return Ok(());
        }
        self.conflicting_facts = self.conflicting_facts.saturating_add(1);
        self.compaction = Some(CompactionStatus {
            id: id.clone(),
            phase: CompactionPhase::Started,
            shadowed_tokens: None,
            error_code: None,
        });
        Ok(())
    }

    fn resolve_pending_prune(&mut self, event: &CommittedUiKind) {
        let Some(pending) = self.pending_prune.take() else {
            return;
        };
        let replacement_matches = matches!(
            event,
            CommittedUiKind::ToolResult {
                surface_replacement_target: Some(target),
                ..
            } if *target == pending.target
        );
        if replacement_matches {
            self.last_prune_shadowed_tokens = Some(pending.shadowed_tokens);
        } else {
            self.orphan_prune_markers = self.orphan_prune_markers.saturating_add(1);
        }
    }
}

struct PatchFacts<'a> {
    path: &'a str,
    operation: PatchActivityOperation,
    diff: &'a str,
    committed: bool,
    cleanup_warning: Option<bool>,
}

fn patch_facts(meta: &Value) -> Option<PatchFacts<'_>> {
    let fields = meta.as_object()?;
    const ALLOWED: &[&str] = &["path", "operation", "diff", "committed", "cleanupWarning"];
    if fields.keys().any(|key| !ALLOWED.contains(&key.as_str()))
        || !fields.get("path").is_some_and(Value::is_string)
        || !fields
            .get("operation")
            .and_then(Value::as_str)
            .is_some_and(|value| matches!(value, "create" | "update"))
        || !fields.get("diff").is_some_and(Value::is_string)
        || fields
            .get("cleanupWarning")
            .is_some_and(|value| !value.is_boolean())
    {
        return None;
    }
    Some(PatchFacts {
        path: fields.get("path")?.as_str()?,
        operation: match fields.get("operation")?.as_str()? {
            "create" => PatchActivityOperation::Create,
            "update" => PatchActivityOperation::Update,
            _ => return None,
        },
        diff: fields.get("diff")?.as_str()?,
        committed: fields.get("committed")?.as_bool()?,
        cleanup_warning: fields.get("cleanupWarning").and_then(Value::as_bool),
    })
}

fn unified_diffstat(diff: &str) -> (usize, usize) {
    let mut additions = 0_usize;
    let mut removals = 0_usize;
    let mut in_hunk = false;
    for line in diff.lines() {
        if line.starts_with("@@") {
            in_hunk = true;
        } else if in_hunk && line.starts_with('+') {
            additions = additions.saturating_add(1);
        } else if in_hunk && line.starts_with('-') {
            removals = removals.saturating_add(1);
        }
    }
    (additions, removals)
}

struct PluginFacts<'a> {
    id: &'a str,
    dispatched: bool,
    peer_settled: bool,
    quiescent: bool,
}

fn plugin_facts(meta: &Value) -> Option<PluginFacts<'_>> {
    let fields = meta.as_object()?;
    const KEYS: &[&str] = &["kind", "pluginId", "dispatched", "peerSettled", "quiescent"];
    if fields.len() != KEYS.len()
        || KEYS.iter().any(|key| !fields.contains_key(*key))
        || fields.get("kind")?.as_str()? != "plugin"
    {
        return None;
    }
    Some(PluginFacts {
        id: fields.get("pluginId")?.as_str()?,
        dispatched: fields.get("dispatched")?.as_bool()?,
        peer_settled: fields.get("peerSettled")?.as_bool()?,
        quiescent: fields.get("quiescent")?.as_bool()?,
    })
}

struct ShellFacts<'a> {
    started: bool,
    exit_code: Option<i64>,
    signal: Option<&'a str>,
    timed_out: Option<bool>,
}

fn shell_facts(meta: &Value) -> Option<ShellFacts<'_>> {
    let fields = meta.as_object()?;
    if fields.get("kind").and_then(Value::as_str) != Some("foreground") {
        return None;
    }
    let started = fields.get("started").and_then(Value::as_bool)?;
    let exit_code = match fields.get("exitCode")? {
        Value::Null => None,
        value => Some(value.as_i64()?),
    };
    let signal = match fields.get("signal")? {
        Value::Null => None,
        value => Some(value.as_str()?),
    };
    if !started {
        const ALLOWED: &[&str] = &[
            "kind",
            "started",
            "exitCode",
            "signal",
            "timeoutMs",
            "workdir",
        ];
        if fields.keys().any(|key| !ALLOWED.contains(&key.as_str()))
            || exit_code.is_some()
            || signal.is_some()
            || fields.get("timeoutMs").is_some_and(|value| !value.is_u64())
            || fields
                .get("workdir")
                .is_some_and(|value| !value.is_string())
        {
            return None;
        }
        return Some(ShellFacts {
            started,
            exit_code,
            signal,
            timed_out: None,
        });
    }
    const REQUIRED: &[&str] = &[
        "kind",
        "started",
        "exitCode",
        "signal",
        "timedOut",
        "aborted",
        "outputLimitExceeded",
        "pipeSetupFailed",
        "pipeReadFailed",
        "signalDeliveryFailed",
        "pipeDrainTimedOut",
        "timeoutMs",
        "workdir",
        "stdoutTruncated",
        "stderrTruncated",
    ];
    if fields.len() != REQUIRED.len()
        || REQUIRED.iter().any(|key| !fields.contains_key(*key))
        || REQUIRED[4..11]
            .iter()
            .chain(REQUIRED[13..].iter())
            .any(|key| !fields.get(*key).is_some_and(Value::is_boolean))
        || !fields.get("timeoutMs").is_some_and(Value::is_u64)
        || !fields.get("workdir").is_some_and(Value::is_string)
    {
        return None;
    }
    let exit_is_valid = exit_code.is_some_and(|code| (0..=255).contains(&code));
    let signal_is_valid = signal.is_some_and(|name| !name.is_empty());
    let exited = exit_is_valid && signal.is_none();
    let signalled = exit_code.is_none() && signal_is_valid;
    if exited == signalled {
        return None;
    }
    Some(ShellFacts {
        started,
        exit_code,
        signal,
        timed_out: fields.get("timedOut").and_then(Value::as_bool),
    })
}

fn summarize_arguments(
    name: &str,
    arguments: &UiOpaquePayload,
) -> Result<Option<String>, UiProjectionError> {
    let Some(arguments) = arguments.as_str() else {
        return Ok(None);
    };
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(arguments) else {
        return Ok(None);
    };
    let summary = match name {
        "list" => fields
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_owned(),
        "glob" | "grep" => fields
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_owned(),
        "read" => fields
            .get("file_path")
            .and_then(Value::as_str)
            .unwrap_or("file")
            .to_owned(),
        "bash" => fields
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("command")
            .to_owned(),
        "apply_patch" => "single-file patch".to_owned(),
        _ => return Ok(None),
    };
    bounded_summary(&summary).map(Some)
}

fn bounded_summary(value: &str) -> Result<String, UiProjectionError> {
    if value.len() <= MAX_SUMMARY_BYTES {
        return copy(value);
    }
    let mut end = MAX_SUMMARY_BYTES.saturating_sub("…".len());
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    let mut summary = String::new();
    summary
        .try_reserve_exact(end + "…".len())
        .map_err(|_| UiProjectionError::Capacity)?;
    summary.push_str(&value[..end]);
    summary.push('…');
    Ok(summary)
}

fn copy(value: &str) -> Result<String, UiProjectionError> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| UiProjectionError::Capacity)?;
    copy.push_str(value);
    Ok(copy)
}

#[cfg(test)]
mod tests {
    use crate::session::{
        ApprovalOutcome, CommittedUiKind, EventSeq, StepId, TurnId, UiIdentity, UiOpaquePayload,
        UiTokenUsage, UiToolFailure, UiTurnEndReason, UiUserSource,
    };

    use super::{MAX_PROJECTED_TOOLS, ToolActivityState, UiProjector};

    fn turn() -> TurnId {
        TurnId::new(1).unwrap()
    }

    fn step() -> StepId {
        StepId::new(1).unwrap()
    }

    fn second_step() -> StepId {
        StepId::new(2).unwrap()
    }

    fn usage(input_tokens: u64, output_tokens: u64) -> UiTokenUsage {
        UiTokenUsage {
            input_tokens,
            output_tokens,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
        }
    }

    fn id(value: &str) -> UiIdentity {
        UiIdentity::from_text_for_test(value)
    }

    #[test]
    fn one_call_id_updates_one_tool_activity_without_inventing_execution() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("call-read"),
                name: id("read"),
                arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/main.rs"}"#),
            })
            .unwrap();
        assert_eq!(projector.tools().len(), 1);
        assert_eq!(projector.tools()[0].state, ToolActivityState::Preparing);
        assert_eq!(projector.tools()[0].summary.as_deref(), Some("src/main.rs"));

        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call-read"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("[]"),
                meta: UiOpaquePayload::from_text_for_test(r#"{"committed":true,"started":true}"#),
                surface_replacement_target: None,
            })
            .unwrap();
        assert_eq!(projector.tools().len(), 1);
        assert_eq!(projector.tools()[0].state, ToolActivityState::Completed);
        assert_eq!(projector.tools()[0].committed_effect, None);
        assert_eq!(projector.tools()[0].started_process, None);
    }

    #[test]
    fn a_tool_failure_and_committed_patch_effect_remain_distinct_facts() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("call-patch"),
                name: id("apply_patch"),
                arguments: UiOpaquePayload::from_text_for_test(r#"{"patch":"diff"}"#),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call-patch"),
                is_error: false,
                failure: Some(UiToolFailure {
                    name: "DurabilityWarning".to_owned(),
                    code: "PATCH_COMMITTED_WITH_WARNING".to_owned(),
                }),
                content: UiOpaquePayload::from_text_for_test("[]"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"path":"src/lib.rs","operation":"update","diff":"--- a\n+++ b","committed":true}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let activity = &projector.tools()[0];
        assert_eq!(activity.state, ToolActivityState::Failed);
        assert_eq!(activity.committed_effect, Some(true));
        assert_eq!(
            activity.failure_code.as_deref(),
            Some("PATCH_COMMITTED_WITH_WARNING")
        );
    }

    #[test]
    fn approval_updates_the_same_tool_and_an_unsettled_turn_is_unknown() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("call-shell"),
                name: id("bash"),
                arguments: UiOpaquePayload::from_text_for_test(
                    r#"{"command":"cargo test","description":"test"}"#,
                ),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id("approval-shell"),
                tool_name: id("bash"),
                call_id: Some(id("call-shell")),
                reason: None,
            })
            .unwrap();
        assert_eq!(
            projector.tools()[0].state,
            ToolActivityState::AwaitingApproval
        );
        projector
            .observe(&CommittedUiKind::ApprovalDecided {
                id: id("approval-shell"),
                outcome: ApprovalOutcome::AllowedOnce,
            })
            .unwrap();
        assert_eq!(projector.tools()[0].state, ToolActivityState::Allowed);

        projector
            .observe(&CommittedUiKind::TurnEnd {
                turn: turn(),
                reason: UiTurnEndReason::Interrupted,
            })
            .unwrap();
        assert_eq!(
            projector.tools()[0].state,
            ToolActivityState::OutcomeUnknown
        );
    }

    #[test]
    fn a_late_memory_compatible_approval_decision_preserves_the_result() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("late-decision-call"),
                name: id("read"),
                arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/lib.rs"}"#),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id("late-decision"),
                tool_name: id("read"),
                call_id: Some(id("late-decision-call")),
                reason: None,
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("late-decision-call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("[]"),
                meta: UiOpaquePayload::from_text_for_test("{}"),
                surface_replacement_target: None,
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalDecided {
                id: id("late-decision"),
                outcome: ApprovalOutcome::AllowedOnce,
            })
            .unwrap();
        assert_eq!(projector.tools()[0].state, ToolActivityState::Completed);
    }

    #[test]
    fn conflicting_memory_compatible_tool_facts_degrade_without_resetting_the_card() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("conflicting-call"),
                name: id("read"),
                arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/lib.rs"}"#),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("conflicting-call"),
                name: id("read"),
                arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/main.rs"}"#),
            })
            .unwrap();
        let result = || CommittedUiKind::ToolResult {
            turn: turn(),
            step: step(),
            call_id: id("conflicting-call"),
            is_error: false,
            failure: None,
            content: UiOpaquePayload::from_text_for_test("[]"),
            meta: UiOpaquePayload::from_text_for_test("{}"),
            surface_replacement_target: None,
        };
        projector.observe(&result()).unwrap();
        projector.observe(&result()).unwrap();

        assert_eq!(projector.tools().len(), 1);
        assert_eq!(projector.tools()[0].name.as_str(), "read");
        assert_eq!(projector.tools()[0].state, ToolActivityState::Completed);
        assert_eq!(projector.status().conflicting_facts, 2);
    }

    #[test]
    fn a_duplicate_result_after_rejection_does_not_overwrite_the_first_result() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("rejected-call"),
                name: id("bash"),
                arguments: UiOpaquePayload::from_text_for_test("{}"),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id("rejected-approval"),
                tool_name: id("bash"),
                call_id: Some(id("rejected-call")),
                reason: None,
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalDecided {
                id: id("rejected-approval"),
                outcome: ApprovalOutcome::Rejected,
            })
            .unwrap();
        let result = |code: &'static str| CommittedUiKind::ToolResult {
            turn: turn(),
            step: step(),
            call_id: id("rejected-call"),
            is_error: true,
            failure: Some(UiToolFailure {
                name: "PermissionError".to_owned(),
                code: code.to_owned(),
            }),
            content: UiOpaquePayload::from_text_for_test("[]"),
            meta: UiOpaquePayload::from_text_for_test("{}"),
            surface_replacement_target: None,
        };
        projector.observe(&result("FIRST")).unwrap();
        projector.observe(&result("SECOND")).unwrap();
        assert_eq!(projector.tools()[0].state, ToolActivityState::Denied);
        assert_eq!(projector.tools()[0].failure_code.as_deref(), Some("FIRST"));
        assert_eq!(projector.status().conflicting_facts, 1);
    }

    #[test]
    fn semantic_debug_does_not_expose_argument_summaries() {
        const SECRET: &str = "SECRET_COMMAND_TOKEN";
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id(&format!("call-{SECRET}")),
                name: id("bash"),
                arguments: UiOpaquePayload::from_text_for_test(&format!(
                    r#"{{"command":"printf {SECRET}","description":"test"}}"#
                )),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id(&format!("approval-{SECRET}")),
                tool_name: id("bash"),
                call_id: Some(id(&format!("call-{SECRET}"))),
                reason: None,
            })
            .unwrap();
        assert!(!format!("{projector:?}").contains(SECRET));
        assert!(
            projector.tools()[0]
                .summary
                .as_deref()
                .is_some_and(|summary| summary.contains(SECRET))
        );
    }

    #[test]
    fn an_unmatched_generic_approval_degrades_without_faulting_the_ui() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id("approval-generic"),
                tool_name: id("future-tool"),
                call_id: Some(id("call-not-in-live-window")),
                reason: None,
            })
            .unwrap();
        assert!(projector.tools().is_empty());
    }

    #[test]
    fn an_approval_with_the_wrong_tool_name_does_not_attach_to_a_call() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("same-call"),
                name: id("read"),
                arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/lib.rs"}"#),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id("wrong-tool-approval"),
                tool_name: id("bash"),
                call_id: Some(id("same-call")),
                reason: None,
            })
            .unwrap();
        assert_eq!(projector.tools()[0].state, ToolActivityState::Preparing);
    }

    #[test]
    fn an_ambiguous_cross_step_approval_does_not_guess_a_tool_card() {
        let mut projector = UiProjector::default();
        for step in [step(), second_step()] {
            projector
                .observe(&CommittedUiKind::ToolRequested {
                    turn: turn(),
                    step,
                    call_id: id("ambiguous-call"),
                    name: id("read"),
                    arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/lib.rs"}"#),
                })
                .unwrap();
        }
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id("ambiguous-approval"),
                tool_name: id("read"),
                call_id: Some(id("ambiguous-call")),
                reason: None,
            })
            .unwrap();
        assert!(
            projector
                .tools()
                .iter()
                .all(|tool| tool.state == ToolActivityState::Preparing)
        );
        assert_eq!(projector.status().conflicting_facts, 1);
    }

    #[test]
    fn a_tool_activity_debug_view_redacts_names_and_failure_codes() {
        const SECRET: &str = "TOOL_ACTIVITY_SECRET";
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("secret-debug-call"),
                name: id(&format!("tool-{SECRET}")),
                arguments: UiOpaquePayload::from_text_for_test("{}"),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("secret-debug-call"),
                is_error: true,
                failure: Some(UiToolFailure {
                    name: "PluginError".to_owned(),
                    code: format!("ERROR_{SECRET}"),
                }),
                content: UiOpaquePayload::from_text_for_test("[]"),
                meta: UiOpaquePayload::from_text_for_test("{}"),
                surface_replacement_target: None,
            })
            .unwrap();
        assert!(!format!("{:?}", projector.tools()[0]).contains(SECRET));
    }

    #[test]
    fn the_same_call_id_in_two_steps_keeps_two_distinct_lifecycles() {
        let mut projector = UiProjector::default();
        for step in [step(), second_step()] {
            projector
                .observe(&CommittedUiKind::ToolRequested {
                    turn: turn(),
                    step,
                    call_id: id("reused-call"),
                    name: id("read"),
                    arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/lib.rs"}"#),
                })
                .unwrap();
            projector
                .observe(&CommittedUiKind::ToolResult {
                    turn: turn(),
                    step,
                    call_id: id("reused-call"),
                    is_error: false,
                    failure: None,
                    content: UiOpaquePayload::from_text_for_test("[]"),
                    meta: UiOpaquePayload::from_text_for_test("{}"),
                    surface_replacement_target: None,
                })
                .unwrap();
        }
        assert_eq!(projector.tools().len(), 2);
        assert_eq!(projector.tools()[0].step, step());
        assert_eq!(projector.tools()[1].step, second_step());
    }

    #[test]
    fn a_natural_nonzero_shell_exit_is_completed_but_not_claimed_as_success() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("shell-exit-one"),
                name: id("bash"),
                arguments: UiOpaquePayload::from_text_for_test(
                    r#"{"command":"false","description":"probe"}"#,
                ),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("shell-exit-one"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("[]"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"foreground","started":true,"exitCode":1,"signal":null,"timedOut":false,"aborted":false,"outputLimitExceeded":false,"pipeSetupFailed":false,"pipeReadFailed":false,"signalDeliveryFailed":false,"pipeDrainTimedOut":false,"timeoutMs":1000,"workdir":".","stdoutTruncated":false,"stderrTruncated":false}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let activity = &projector.tools()[0];
        assert_eq!(activity.state, ToolActivityState::Completed);
        assert_eq!(activity.started_process, Some(true));
        assert_eq!(activity.shell_exit_code, Some(1));
        assert_eq!(activity.shell_timed_out, Some(false));
    }

    #[test]
    fn forged_or_incomplete_shell_metadata_never_claims_a_process_started() {
        let cases = [
            r#"{"kind":"foreground","started":false,"exitCode":1,"signal":null}"#,
            r#"{"kind":"foreground","started":false,"exitCode":null,"signal":null,"timeoutMs":false,"workdir":123}"#,
            r#"{"kind":"foreground","started":true,"exitCode":1,"signal":"TERM","timedOut":false,"aborted":false,"outputLimitExceeded":false,"pipeSetupFailed":false,"pipeReadFailed":false,"signalDeliveryFailed":false,"pipeDrainTimedOut":false,"timeoutMs":1000,"workdir":".","stdoutTruncated":false,"stderrTruncated":false}"#,
            r#"{"kind":"foreground","started":true,"exitCode":0,"signal":"","timedOut":false,"aborted":false,"outputLimitExceeded":false,"pipeSetupFailed":false,"pipeReadFailed":false,"signalDeliveryFailed":false,"pipeDrainTimedOut":false,"timeoutMs":1000,"workdir":".","stdoutTruncated":false,"stderrTruncated":false}"#,
            r#"{"kind":"foreground","started":true,"exitCode":1,"signal":null}"#,
        ];
        for (index, meta) in cases.into_iter().enumerate() {
            let mut projector = UiProjector::default();
            let call_id = id(&format!("forged-shell-{index}"));
            projector
                .observe(&CommittedUiKind::ToolRequested {
                    turn: turn(),
                    step: step(),
                    call_id: call_id.clone(),
                    name: id("bash"),
                    arguments: UiOpaquePayload::from_text_for_test(
                        r#"{"command":"true","description":"probe"}"#,
                    ),
                })
                .unwrap();
            projector
                .observe(&CommittedUiKind::ToolResult {
                    turn: turn(),
                    step: step(),
                    call_id,
                    is_error: false,
                    failure: None,
                    content: UiOpaquePayload::from_text_for_test("[]"),
                    meta: UiOpaquePayload::from_text_for_test(meta),
                    surface_replacement_target: None,
                })
                .unwrap();
            assert_eq!(projector.tools()[0].started_process, None);
            assert_eq!(projector.tools()[0].shell_exit_code, None);
        }
    }

    #[test]
    fn compaction_usage_and_independent_pruning_do_not_overwrite_conversation_usage() {
        let mut projector = UiProjector::default();
        let conversation = usage(80, 5);
        let summary = usage(20, 2);
        projector
            .observe(&CommittedUiKind::UsageSample {
                turn: turn(),
                step: step(),
                usage: conversation,
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::CompactionStarted {
                id: id("compact-1"),
                turn: Some(turn()),
                trigger: None,
                shadowed_nodes: Some(3),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::CompactionSummarized {
                id: id("compact-1"),
                shadowed_tokens: 64,
                provider: id("mock"),
                model: id("summary"),
                usage: Some(summary),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::CompactionEnded {
                id: id("compact-1"),
                turn: Some(turn()),
                error: None,
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::CompactionPruneMarked {
                target: EventSeq::new(9).unwrap(),
                shadowed_tokens: 11,
            })
            .unwrap();
        assert_eq!(projector.status().last_prune_shadowed_tokens, None);
        assert_eq!(projector.status().pending_prune_shadowed_tokens, Some(11));
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("historical-result"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("[]"),
                meta: UiOpaquePayload::from_text_for_test("{}"),
                surface_replacement_target: Some(EventSeq::new(9).unwrap()),
            })
            .unwrap();

        let status = projector.status();
        assert_eq!(status.last_usage, Some(conversation));
        assert_eq!(status.compaction_usage, Some(summary));
        assert_eq!(status.last_prune_shadowed_tokens, Some(11));
        assert_eq!(status.pending_prune_shadowed_tokens, None);
        assert_eq!(status.orphan_prune_markers, 0);
        assert_eq!(
            projector.compaction().and_then(|item| item.shadowed_tokens),
            Some(64)
        );
    }

    #[test]
    fn end_seed_clears_an_incomplete_memory_compatible_compaction() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::CompactionStarted {
                id: id("incomplete-compaction"),
                turn: None,
                trigger: None,
                shadowed_nodes: Some(1),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::TypeOnly {
                event_type: "session/end-seed",
            })
            .unwrap();
        assert!(projector.compaction().is_none());
    }

    #[test]
    fn tool_card_capacity_degrades_at_one_over_without_faulting_session_facts() {
        let mut projector = UiProjector::default();
        for index in 0..=MAX_PROJECTED_TOOLS {
            projector
                .observe(&CommittedUiKind::ToolRequested {
                    turn: turn(),
                    step: step(),
                    call_id: id(&format!("call-{index}")),
                    name: id("read"),
                    arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/lib.rs"}"#),
                })
                .unwrap();
        }
        assert_eq!(projector.tools().len(), MAX_PROJECTED_TOOLS);
        assert_eq!(projector.status().omitted_tool_facts, 1);
    }

    #[test]
    fn a_new_turn_clears_turn_local_usage_prompt_and_compaction_facts() {
        let mut projector = UiProjector::default();
        projector
            .observe(&CommittedUiKind::UserMessage {
                source: UiUserSource::Human,
                content: UiOpaquePayload::from_text_for_test("first prompt"),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::UsageSample {
                turn: turn(),
                step: step(),
                usage: usage(12, 3),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::CompactionStarted {
                id: id("old-compaction"),
                turn: Some(turn()),
                trigger: None,
                shadowed_nodes: Some(2),
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::TurnEnd {
                turn: turn(),
                reason: UiTurnEndReason::Completed,
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::TurnStart {
                turn: TurnId::new(2).unwrap(),
            })
            .unwrap();

        let status = projector.status();
        assert_eq!(status.last_usage, None);
        assert_eq!(status.last_human_prompt_bytes, None);
        assert!(projector.compaction().is_none());
    }
}
