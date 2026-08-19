//! Bounded, read-only facts for the current Inspect view and last joined Review.

use std::{fmt, fmt::Write as _, sync::Arc};

use thiserror::Error;

use crate::{
    agent::TurnOutcome,
    session::{
        ApprovalOutcome, CommittedUiEvent, CommittedUiKind, EventSeq, StepId, TurnId,
        UiAssistantBlockKind, UiAssistantContent, UiOpaquePayload, UiTokenUsage,
        UiTurnEndCancelCause, UiTurnEndReason, UiUserSource, UnixMillis,
    },
};

use super::{
    projector::ToolActivity,
    timeline::{TimelineTone, ToolCardView, WorkReceiptView},
    visible::render_visible_owned_bounded,
};

pub(crate) const MAX_INSPECT_ROWS: usize = 512;
pub(crate) const MAX_INSPECT_TEXT_BYTES: usize = 512 * 1024;
pub(crate) const MAX_INSPECT_REASONING_BYTES: usize = 256 * 1024;
pub(crate) const MAX_REVIEW_ACTIVITIES: usize = 256;
pub(crate) const MAX_REVIEW_TEXT_BYTES: usize = 144 * 1024;
pub(crate) const MAX_DETAIL_SOURCE_LINES: usize = 4 * 1024;
pub(crate) const MAX_DETAIL_TEXT_BYTES: usize = 1024 * 1024;
const MAX_INSPECT_REASONING_BLOCKS: usize = 128;
const MAX_INSPECT_REASONING_OMISSION_STEPS: usize = 128;
const MAX_INSPECT_FIELD_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewMode {
    Focus,
    Inspect,
    Review,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ViewRequest {
    mode: ViewMode,
    offset: usize,
    revision: u64,
}

impl ViewRequest {
    pub(crate) const fn mode(self) -> ViewMode {
        self.mode
    }

    pub(crate) const fn offset(self) -> usize {
        self.offset
    }
}

#[derive(Debug)]
pub(crate) struct ViewState {
    requested: ViewRequest,
    committed: ViewRequest,
    committed_total_rows: usize,
    committed_page_rows: usize,
}

impl Default for ViewState {
    fn default() -> Self {
        let focus = ViewRequest {
            mode: ViewMode::Focus,
            offset: 0,
            revision: 0,
        };
        Self {
            requested: focus,
            committed: focus,
            committed_total_rows: 0,
            committed_page_rows: 0,
        }
    }
}

impl ViewState {
    pub(crate) const fn requested(&self) -> ViewRequest {
        self.requested
    }

    pub(crate) const fn committed(&self) -> ViewRequest {
        self.committed
    }

    pub(crate) fn request_mode(&mut self, mode: ViewMode) -> Result<bool, ViewError> {
        if self.requested.mode == mode {
            return Ok(false);
        }
        self.requested.mode = mode;
        self.requested.offset = 0;
        self.bump_revision()?;
        Ok(true)
    }

    pub(crate) fn toggle_inspect(&mut self) -> Result<(), ViewError> {
        let next = if self.requested.mode == ViewMode::Focus {
            ViewMode::Inspect
        } else {
            ViewMode::Focus
        };
        let _ = self.request_mode(next)?;
        Ok(())
    }

    pub(crate) fn switch_detail(&mut self) -> Result<(), ViewError> {
        let next = match self.requested.mode {
            ViewMode::Focus | ViewMode::Review => ViewMode::Inspect,
            ViewMode::Inspect => ViewMode::Review,
        };
        let _ = self.request_mode(next)?;
        Ok(())
    }

    pub(crate) fn request_offset(&mut self, offset: usize) -> Result<bool, ViewError> {
        if self.requested.mode == ViewMode::Focus || self.requested.offset == offset {
            return Ok(false);
        }
        self.requested.offset = offset;
        self.bump_revision()?;
        Ok(true)
    }

    pub(crate) fn scroll_lines(&mut self, delta: isize) -> Result<bool, ViewError> {
        let maximum = self
            .committed_total_rows
            .saturating_sub(self.committed_page_rows);
        let offset = self
            .requested
            .offset
            .saturating_add_signed(delta)
            .min(maximum);
        self.request_offset(offset)
    }

    pub(crate) fn scroll_page(&mut self, down: bool) -> Result<bool, ViewError> {
        let page = self.committed_page_rows.max(1);
        let delta = isize::try_from(page).map_err(|_| ViewError::Limit)?;
        self.scroll_lines(if down { delta } else { -delta })
    }

    pub(crate) fn scroll_end(&mut self) -> Result<bool, ViewError> {
        self.request_offset(
            self.committed_total_rows
                .saturating_sub(self.committed_page_rows),
        )
    }

    pub(crate) fn commit(
        &mut self,
        request: ViewRequest,
        actual_offset: usize,
        total_rows: usize,
        page_rows: usize,
    ) -> bool {
        if request.revision != self.requested.revision || request.mode != self.requested.mode {
            return false;
        }
        self.requested.offset = actual_offset;
        self.committed = ViewRequest {
            mode: request.mode,
            offset: actual_offset,
            revision: request.revision,
        };
        self.committed_total_rows = total_rows;
        self.committed_page_rows = page_rows;
        true
    }

    fn bump_revision(&mut self) -> Result<(), ViewError> {
        self.requested.revision = self
            .requested
            .revision
            .checked_add(1)
            .ok_or(ViewError::Limit)?;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FactKind {
    TurnStart,
    TurnEnd,
    StepStart,
    StepEnd,
    UserMessage,
    AssistantFinal,
    ToolRequested,
    ToolResult,
    ApprovalAsked,
    ApprovalDecided,
    RetryScheduled,
    RetryStarted,
    RequestContext,
    CompactionStarted,
    CompactionSummarized,
    CompactionEnded,
    CompactionPrune,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DetailTone {
    Plain,
    Muted,
    Accent,
    Positive,
    Caution,
    Negative,
    Code,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum ViewError {
    #[error("CLI_OUTPUT_CAPACITY")]
    Capacity,
    #[error("CLI_OUTPUT_LIMIT")]
    Limit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FactStamp {
    seq: EventSeq,
    time: UnixMillis,
}

struct InspectRow {
    stamp: FactStamp,
    kind: FactKind,
    turn: Option<TurnId>,
    step: Option<StepId>,
    label: String,
    detail: Option<String>,
}

impl fmt::Debug for InspectRow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InspectRow")
            .field("stamp", &self.stamp)
            .field("kind", &self.kind)
            .field("turn", &self.turn)
            .field("step", &self.step)
            .field("label_bytes", &self.label.len())
            .field("detail_bytes", &self.detail.as_ref().map_or(0, String::len))
            .finish()
    }
}

struct ReasoningBlock {
    step: StepId,
    index: u64,
    text: String,
    authoritative: bool,
    first_stamp: FactStamp,
    last_stamp: FactStamp,
    fragments: usize,
    original_bytes: usize,
    omitted_bytes: usize,
}

#[derive(Debug)]
struct ReasoningStepOmission {
    step: StepId,
    bytes: usize,
}

impl fmt::Debug for ReasoningBlock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReasoningBlock")
            .field("step", &self.step)
            .field("index", &self.index)
            .field("bytes", &self.text.len())
            .field("authoritative", &self.authoritative)
            .field("first_stamp", &self.first_stamp)
            .field("last_stamp", &self.last_stamp)
            .field("fragments", &self.fragments)
            .field("original_bytes", &self.original_bytes)
            .field("omitted_bytes", &self.omitted_bytes)
            .finish()
    }
}

pub(crate) struct ContextEstimate {
    at_next_seq: EventSeq,
    provider: Option<String>,
    model: Option<String>,
    used_tokens: u64,
    window_tokens: Option<u64>,
    sampled_after_turn: Option<TurnId>,
}

impl fmt::Debug for ContextEstimate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ContextEstimate")
            .field("at_next_seq", &self.at_next_seq)
            .field(
                "provider_bytes",
                &self.provider.as_ref().map_or(0, String::len),
            )
            .field("model_bytes", &self.model.as_ref().map_or(0, String::len))
            .field("used_tokens", &self.used_tokens)
            .field("window_tokens", &self.window_tokens)
            .field("sampled_after_turn", &self.sampled_after_turn)
            .finish()
    }
}

impl ContextEstimate {
    pub(crate) fn new(
        at_next_seq: EventSeq,
        provider: Option<&str>,
        model: Option<&str>,
        used_tokens: u64,
        window_tokens: Option<u64>,
        sampled_after_turn: Option<TurnId>,
    ) -> Result<Self, ViewError> {
        Ok(Self {
            at_next_seq,
            provider: provider.map(bounded_field).transpose()?,
            model: model.map(bounded_field).transpose()?,
            used_tokens,
            window_tokens,
            sampled_after_turn,
        })
    }

    pub(crate) fn status_line(&self) -> Result<String, ViewError> {
        let mut output = view_string(192)?;
        output.push_str("Session context estimate ");
        write_compact_number(&mut output, self.used_tokens)?;
        if let Some(window) = self.window_tokens.filter(|window| *window != 0) {
            output.push_str(" / ");
            write_compact_number(&mut output, window)?;
            let percent = u128::from(self.used_tokens)
                .saturating_mul(100)
                .checked_div(u128::from(window))
                .unwrap_or(0);
            write!(&mut output, " · {percent}%").map_err(|_| ViewError::Capacity)?;
        }
        if let Some(model) = self.model.as_deref() {
            output.push_str(" · ");
            output.push_str(model);
        }
        if let Some(turn) = self.sampled_after_turn {
            write!(&mut output, " · after turn {}", turn.get()).map_err(|_| ViewError::Capacity)?;
        }
        write!(
            &mut output,
            " · sampled before seq {}",
            self.at_next_seq.get()
        )
        .map_err(|_| ViewError::Capacity)?;
        Ok(output)
    }
}

pub(crate) struct ReviewActivity {
    tone: TimelineTone,
    headline: String,
    detail: Option<String>,
}

impl fmt::Debug for ReviewActivity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReviewActivity")
            .field("tone", &self.tone)
            .field("headline_bytes", &self.headline.len())
            .field("detail_bytes", &self.detail.as_ref().map_or(0, String::len))
            .finish()
    }
}

pub(crate) struct JoinedTurnView {
    turn: TurnId,
    turn_end_seq: EventSeq,
    receipt: Arc<WorkReceiptView>,
    activities: Vec<ReviewActivity>,
    omitted_activities: usize,
}

impl fmt::Debug for JoinedTurnView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinedTurnView")
            .field("turn", &self.turn)
            .field("turn_end_seq", &self.turn_end_seq)
            .field("receipt", &self.receipt)
            .field("activity_count", &self.activities.len())
            .field("omitted_activities", &self.omitted_activities)
            .finish()
    }
}

impl JoinedTurnView {
    /// The caller must first verify the committed turn/end anchor against the
    /// returned TurnOutcome. This constructor only freezes the already-joined
    /// presentation facts.
    pub(crate) fn from_joined_receipt(
        outcome: &TurnOutcome,
        receipt: Arc<WorkReceiptView>,
        tools: &[ToolActivity],
    ) -> Result<Self, ViewError> {
        let matching = tools.iter().filter(|tool| tool.turn == outcome.turn());
        let total_activities = matching.clone().count();
        let count = total_activities.min(MAX_REVIEW_ACTIVITIES);
        let mut activities = Vec::new();
        activities
            .try_reserve_exact(count)
            .map_err(|_| ViewError::Capacity)?;
        let mut retained_bytes = receipt
            .headline()
            .len()
            .checked_add(receipt.counters().map_or(0, str::len))
            .and_then(|bytes| bytes.checked_add(receipt.effects().map_or(0, str::len)))
            .ok_or(ViewError::Limit)?;
        for tool in matching.take(MAX_REVIEW_ACTIVITIES) {
            let card = ToolCardView::from_activity(tool).map_err(|_| ViewError::Capacity)?;
            let next = retained_bytes
                .checked_add(card.headline().len())
                .and_then(|bytes| bytes.checked_add(card.detail().map_or(0, str::len)))
                .ok_or(ViewError::Limit)?;
            if next > MAX_REVIEW_TEXT_BYTES {
                break;
            }
            retained_bytes = next;
            activities.push(ReviewActivity {
                tone: card.tone(),
                headline: copy_text(card.headline())?,
                detail: card.detail().map(copy_text).transpose()?,
            });
        }
        let omitted_activities = total_activities
            .saturating_sub(activities.len())
            .max(outcome.tool_calls().saturating_sub(activities.len()));
        Ok(Self {
            turn: outcome.turn(),
            turn_end_seq: outcome.turn_end_seq(),
            receipt,
            omitted_activities,
            activities,
        })
    }

    pub(crate) const fn turn(&self) -> TurnId {
        self.turn
    }

    pub(crate) const fn turn_end_seq(&self) -> EventSeq {
        self.turn_end_seq
    }

    pub(crate) fn receipt(&self) -> &WorkReceiptView {
        &self.receipt
    }
}

struct JoinedReceipt {
    turn: TurnId,
    turn_end_seq: EventSeq,
    receipt: Arc<WorkReceiptView>,
}

impl fmt::Debug for JoinedReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinedReceipt")
            .field("turn", &self.turn)
            .field("turn_end_seq", &self.turn_end_seq)
            .field("receipt", &self.receipt)
            .finish()
    }
}

pub(crate) struct ViewArchive {
    resumed_live_seam: bool,
    turn: Option<TurnId>,
    turn_ended: bool,
    rows: Vec<InspectRow>,
    metadata_bytes: usize,
    retained_text_bytes: usize,
    omitted_rows: usize,
    reasoning: Vec<ReasoningBlock>,
    reasoning_step_omissions: Vec<ReasoningStepOmission>,
    reasoning_bytes: usize,
    omitted_reasoning_bytes: usize,
    latest_usage: Option<UiTokenUsage>,
    context_estimate: Option<ContextEstimate>,
    joined_receipt: Option<JoinedReceipt>,
    joined_review: Option<JoinedTurnView>,
    inspect_degraded: bool,
    review_join_failed_for: Option<TurnId>,
}

impl fmt::Debug for ViewArchive {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ViewArchive")
            .field("resumed_live_seam", &self.resumed_live_seam)
            .field("turn", &self.turn)
            .field("turn_ended", &self.turn_ended)
            .field("row_count", &self.rows.len())
            .field("metadata_bytes", &self.metadata_bytes)
            .field("retained_text_bytes", &self.retained_text_bytes)
            .field("omitted_rows", &self.omitted_rows)
            .field("reasoning_blocks", &self.reasoning.len())
            .field(
                "reasoning_step_omissions",
                &self.reasoning_step_omissions.len(),
            )
            .field("reasoning_bytes", &self.reasoning_bytes)
            .field("omitted_reasoning_bytes", &self.omitted_reasoning_bytes)
            .field("has_usage", &self.latest_usage.is_some())
            .field("has_context_estimate", &self.context_estimate.is_some())
            .field("has_joined_receipt", &self.joined_receipt.is_some())
            .field("has_joined_review", &self.joined_review.is_some())
            .field("inspect_degraded", &self.inspect_degraded)
            .field("review_join_failed_for", &self.review_join_failed_for)
            .finish()
    }
}

impl ViewArchive {
    pub(crate) fn new(resumed_live_seam: bool) -> Self {
        Self {
            resumed_live_seam,
            turn: None,
            turn_ended: false,
            rows: Vec::new(),
            metadata_bytes: 0,
            retained_text_bytes: 0,
            omitted_rows: 0,
            reasoning: Vec::new(),
            reasoning_step_omissions: Vec::new(),
            reasoning_bytes: 0,
            omitted_reasoning_bytes: 0,
            latest_usage: None,
            context_estimate: None,
            joined_receipt: None,
            joined_review: None,
            inspect_degraded: false,
            review_join_failed_for: None,
        }
    }

    pub(crate) fn observe(&mut self, event: &CommittedUiEvent) {
        if self.try_observe(event).is_err() {
            self.inspect_degraded = true;
        }
    }

    pub(crate) fn set_context_estimate(&mut self, estimate: Option<ContextEstimate>) {
        self.context_estimate = estimate;
    }

    pub(crate) fn freeze_review(&mut self, review: JoinedTurnView) {
        self.joined_review = Some(review);
        self.review_join_failed_for = None;
    }

    pub(crate) fn freeze_receipt(
        &mut self,
        turn: TurnId,
        turn_end_seq: EventSeq,
        receipt: Arc<WorkReceiptView>,
    ) {
        self.joined_receipt = Some(JoinedReceipt {
            turn,
            turn_end_seq,
            receipt,
        });
    }

    pub(crate) fn joined_receipt(
        &self,
        turn: TurnId,
        turn_end_seq: EventSeq,
    ) -> Option<&WorkReceiptView> {
        self.joined_receipt
            .as_ref()
            .filter(|joined| joined.turn == turn && joined.turn_end_seq == turn_end_seq)
            .map(|joined| joined.receipt.as_ref())
    }

    pub(crate) fn joined_review(&self) -> Option<&JoinedTurnView> {
        self.joined_review.as_ref()
    }

    pub(crate) fn turn(&self) -> Option<TurnId> {
        self.turn
    }

    #[cfg(test)]
    pub(crate) fn is_degraded(&self) -> bool {
        self.inspect_degraded || self.omitted_rows != 0 || self.omitted_reasoning_bytes != 0
    }

    pub(crate) fn mark_review_join_failed(&mut self, turn: TurnId) {
        self.review_join_failed_for = Some(turn);
    }

    fn try_observe(&mut self, event: &CommittedUiEvent) -> Result<(), ViewError> {
        let stamp = FactStamp {
            seq: event.seq,
            time: event.time,
        };
        match &event.kind {
            CommittedUiKind::TurnStart { turn } => {
                self.reset_turn(*turn, event.time);
                self.push_row(
                    stamp,
                    FactKind::TurnStart,
                    Some(*turn),
                    None,
                    "Turn started",
                    None,
                )?;
            }
            CommittedUiKind::TurnEnd { turn, reason } => {
                self.turn_ended = true;
                let (label, detail) = turn_end_fact(reason)?;
                self.push_row(stamp, FactKind::TurnEnd, Some(*turn), None, label, detail)?;
            }
            CommittedUiKind::StepStart { turn, step } => self.push_row(
                stamp,
                FactKind::StepStart,
                Some(*turn),
                Some(*step),
                "Step started",
                None,
            )?,
            CommittedUiKind::StepEnd { turn, step } => self.push_row(
                stamp,
                FactKind::StepEnd,
                Some(*turn),
                Some(*step),
                "Step ended",
                None,
            )?,
            CommittedUiKind::AssistantReasoningDelta {
                step, index, text, ..
            } => self.append_reasoning(stamp, *step, *index, text, false)?,
            CommittedUiKind::AssistantMessage {
                turn,
                step,
                content,
                usage,
                ..
            } => {
                self.replace_authoritative_reasoning(stamp, *step, content)?;
                if let Some(usage) = usage {
                    self.latest_usage = Some(*usage);
                }
                self.push_row(
                    stamp,
                    FactKind::AssistantFinal,
                    Some(*turn),
                    Some(*step),
                    "Assistant message committed",
                    usage.map(|usage| usage_detail(&usage)).transpose()?,
                )?;
            }
            CommittedUiKind::UsageSample { usage, .. } => self.latest_usage = Some(*usage),
            CommittedUiKind::ToolRequested {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => {
                let label = joined_fields("Tool requested", name.as_str())?;
                let detail = joined_fields(
                    &identity_availability("call ID", call_id)?,
                    &payload_detail("arguments", arguments)?,
                )?;
                self.push_row(
                    stamp,
                    FactKind::ToolRequested,
                    Some(*turn),
                    Some(*step),
                    label,
                    Some(detail),
                )?;
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
            } => {
                let label = if surface_replacement_target.is_some() {
                    "Surface replacement"
                } else if *is_error || failure.is_some() {
                    "Tool result · error"
                } else {
                    "Tool result · recorded"
                };
                let mut detail = payload_detail("result", content)?;
                append_metadata_piece(&mut detail, &identity_availability("call ID", call_id)?)?;
                append_metadata_piece(&mut detail, &payload_detail("meta", meta)?)?;
                if let Some(target) = surface_replacement_target {
                    append_metadata_piece(&mut detail, &format!("replaces seq {}", target.get()))?;
                }
                if let Some(failure) = failure {
                    append_metadata_piece(&mut detail, &failure.code)?;
                }
                self.push_row(
                    stamp,
                    FactKind::ToolResult,
                    Some(*turn),
                    Some(*step),
                    label,
                    Some(detail),
                )?;
            }
            CommittedUiKind::ApprovalAsked {
                id,
                tool_name,
                call_id,
                reason,
            } => {
                let label = joined_fields("Approval asked", tool_name.as_str())?;
                let mut detail = identity_availability("request ID", id)?;
                if let Some(call_id) = call_id {
                    append_metadata_piece(
                        &mut detail,
                        &identity_availability("call ID", call_id)?,
                    )?;
                }
                if let Some(reason) = reason {
                    append_metadata_piece(
                        &mut detail,
                        &format!("reason field {} displayed bytes", reason.len()),
                    )?;
                }
                self.push_row(
                    stamp,
                    FactKind::ApprovalAsked,
                    self.turn,
                    None,
                    label,
                    Some(detail),
                )?;
            }
            CommittedUiKind::ApprovalDecided { id, outcome } => {
                let detail = identity_availability("request ID", id)?;
                self.push_row(
                    stamp,
                    FactKind::ApprovalDecided,
                    self.turn,
                    None,
                    approval_outcome_label(*outcome),
                    Some(detail),
                )?;
            }
            CommittedUiKind::RetryScheduled {
                retry,
                provider,
                delay_ms,
                max_retries,
                failure_code,
                failure_message,
                ..
            } => {
                let mut detail = view_string(192 + provider.len() + failure_code.len())?;
                write!(
                    &mut detail,
                    "{} · {} ms · {} · message field {} displayed bytes",
                    provider.as_str(),
                    delay_ms,
                    failure_code,
                    failure_message.len()
                )
                .map_err(|_| ViewError::Capacity)?;
                if let Some(max) = max_retries {
                    write!(&mut detail, " · max {}", max.get()).map_err(|_| ViewError::Capacity)?;
                }
                self.push_row(
                    stamp,
                    FactKind::RetryScheduled,
                    self.turn,
                    None,
                    joined_fields("Retry scheduled", &retry.get().to_string())?,
                    Some(detail),
                )?;
            }
            CommittedUiKind::RetryStarted { retry, .. } => self.push_row(
                stamp,
                FactKind::RetryStarted,
                self.turn,
                None,
                joined_fields("Retry started", &retry.get().to_string())?,
                None,
            )?,
            CommittedUiKind::RequestContextChanged {
                provider,
                model,
                context_window,
            } => {
                let mut detail = view_string(192)?;
                append_optional_identity(&mut detail, "provider", provider.as_ref())?;
                append_optional_identity(&mut detail, "model", model.as_ref())?;
                if let Some(window) = context_window {
                    append_metadata_piece(&mut detail, &format!("window {window}"))?;
                }
                self.push_row(
                    stamp,
                    FactKind::RequestContext,
                    self.turn,
                    None,
                    "Request context committed",
                    (!detail.is_empty()).then_some(detail),
                )?;
            }
            CommittedUiKind::CompactionStarted {
                id,
                turn,
                trigger,
                shadowed_nodes,
            } => {
                let mut detail = identity_availability("compaction ID", id)?;
                if let Some(trigger) = trigger {
                    append_metadata_piece(&mut detail, &format!("trigger {trigger:?}"))?;
                }
                if let Some(nodes) = shadowed_nodes {
                    append_metadata_piece(&mut detail, &format!("{nodes} shadowed nodes"))?;
                }
                self.push_row(
                    stamp,
                    FactKind::CompactionStarted,
                    *turn,
                    None,
                    "Context summary started",
                    Some(detail),
                )?;
            }
            CommittedUiKind::CompactionSummarized {
                id,
                shadowed_tokens,
                usage,
                ..
            } => {
                let mut detail = identity_availability("compaction ID", id)?;
                append_metadata_piece(
                    &mut detail,
                    &format!("{shadowed_tokens} estimated tokens in shadowed nodes"),
                )?;
                if let Some(usage) = usage {
                    append_metadata_piece(&mut detail, &usage_detail(usage)?)?;
                }
                self.push_row(
                    stamp,
                    FactKind::CompactionSummarized,
                    self.turn,
                    None,
                    "Context summary prepared",
                    Some(detail),
                )?;
            }
            CommittedUiKind::CompactionEnded { id, turn, error } => {
                let mut detail = identity_availability("compaction ID", id)?;
                if let Some(error) = error {
                    if let Some(code) = error.code.as_deref() {
                        append_metadata_piece(&mut detail, code)?;
                    }
                    append_metadata_piece(
                        &mut detail,
                        &format!("message field {} displayed bytes", error.message.len()),
                    )?;
                }
                self.push_row(
                    stamp,
                    FactKind::CompactionEnded,
                    *turn,
                    None,
                    if error.is_some() {
                        "Context summary failed"
                    } else {
                        "Context summary committed"
                    },
                    Some(detail),
                )?;
            }
            CommittedUiKind::CompactionPruneMarked {
                target,
                shadowed_tokens,
            } => {
                let detail = format!(
                    "target seq {} · {shadowed_tokens} estimated tokens in the shadowed node",
                    target.get()
                );
                self.push_row(
                    stamp,
                    FactKind::CompactionPrune,
                    self.turn,
                    None,
                    "Prune marker committed · replacement pending",
                    Some(detail),
                )?;
            }
            CommittedUiKind::UserMessage { source, content } => {
                let label = match source {
                    UiUserSource::Human => "Human message committed",
                    UiUserSource::Context { .. } => "Context message committed",
                    UiUserSource::Other { .. } => "Other user-role message committed",
                };
                let mut detail = payload_detail("content", content)?;
                match source {
                    UiUserSource::Human => {}
                    UiUserSource::Context { plugin, form } => {
                        append_metadata_piece(
                            &mut detail,
                            &format!("context plugin field {} displayed bytes", plugin.len()),
                        )?;
                        if let Some(form) = form {
                            append_metadata_piece(&mut detail, &format!("form {form:?}"))?;
                        }
                    }
                    UiUserSource::Other { kind } => append_metadata_piece(
                        &mut detail,
                        &format!("source kind field {} displayed bytes", kind.len()),
                    )?,
                }
                self.push_row(
                    stamp,
                    FactKind::UserMessage,
                    self.turn,
                    None,
                    label,
                    Some(detail),
                )?;
            }
            CommittedUiKind::AssistantTextDelta { .. } => {}
            CommittedUiKind::TypeOnly { .. } => {}
        }
        Ok(())
    }

    fn reset_turn(&mut self, turn: TurnId, time: UnixMillis) {
        self.turn = Some(turn);
        let _ = time;
        self.turn_ended = false;
        self.rows.clear();
        self.metadata_bytes = 0;
        self.retained_text_bytes = 0;
        self.omitted_rows = 0;
        self.reasoning.clear();
        self.reasoning_step_omissions.clear();
        self.reasoning_bytes = 0;
        self.omitted_reasoning_bytes = 0;
        self.latest_usage = None;
        self.inspect_degraded = false;
    }

    #[allow(clippy::too_many_arguments)]
    fn push_row(
        &mut self,
        stamp: FactStamp,
        kind: FactKind,
        turn: Option<TurnId>,
        step: Option<StepId>,
        label: impl AsRef<str>,
        detail: Option<String>,
    ) -> Result<(), ViewError> {
        let label = bounded_field(label.as_ref())?;
        let detail = detail.map(|value| bounded_field(&value)).transpose()?;
        let bytes = label
            .len()
            .checked_add(detail.as_ref().map_or(0, String::len))
            .ok_or(ViewError::Limit)?;
        let next = self
            .metadata_bytes
            .checked_add(bytes)
            .ok_or(ViewError::Limit)?;
        let retained_next = self
            .retained_text_bytes
            .checked_add(bytes)
            .ok_or(ViewError::Limit)?;
        if self.rows.len() == MAX_INSPECT_ROWS || retained_next > MAX_INSPECT_TEXT_BYTES {
            self.omitted_rows = self.omitted_rows.saturating_add(1);
            return Ok(());
        }
        self.rows.try_reserve(1).map_err(|_| ViewError::Capacity)?;
        self.rows.push(InspectRow {
            stamp,
            kind,
            turn,
            step,
            label,
            detail,
        });
        self.metadata_bytes = next;
        self.retained_text_bytes = retained_next;
        Ok(())
    }

    fn append_reasoning(
        &mut self,
        stamp: FactStamp,
        step: StepId,
        index: u64,
        text: &str,
        authoritative: bool,
    ) -> Result<(), ViewError> {
        if text.is_empty() {
            return Ok(());
        }
        let next = self
            .reasoning_bytes
            .checked_add(text.len())
            .ok_or(ViewError::Limit)?;
        let retained_next = self
            .retained_text_bytes
            .checked_add(text.len())
            .ok_or(ViewError::Limit)?;
        let position = self
            .reasoning
            .iter()
            .position(|block| block.step == step && block.index == index);
        let position = if let Some(position) = position {
            position
        } else {
            if self.reasoning.len() == MAX_INSPECT_REASONING_BLOCKS {
                self.record_step_reasoning_omission(step, text.len())?;
                return Ok(());
            }
            self.reasoning
                .try_reserve(1)
                .map_err(|_| ViewError::Capacity)?;
            self.reasoning.push(ReasoningBlock {
                step,
                index,
                text: String::new(),
                authoritative,
                first_stamp: stamp,
                last_stamp: stamp,
                fragments: 0,
                original_bytes: 0,
                omitted_bytes: 0,
            });
            self.reasoning.len() - 1
        };
        let block = &mut self.reasoning[position];
        block.last_stamp = stamp;
        block.fragments = block.fragments.saturating_add(1);
        block.original_bytes = block.original_bytes.saturating_add(text.len());
        block.authoritative |= authoritative;
        if next > MAX_INSPECT_REASONING_BYTES || retained_next > MAX_INSPECT_TEXT_BYTES {
            block.omitted_bytes = block.omitted_bytes.saturating_add(text.len());
            self.omitted_reasoning_bytes = self.omitted_reasoning_bytes.saturating_add(text.len());
            return Ok(());
        }
        block
            .text
            .try_reserve(text.len())
            .map_err(|_| ViewError::Capacity)?;
        block.text.push_str(text);
        self.reasoning_bytes = next;
        self.retained_text_bytes = retained_next;
        Ok(())
    }

    fn replace_authoritative_reasoning(
        &mut self,
        stamp: FactStamp,
        step: StepId,
        content: &UiAssistantContent,
    ) -> Result<(), ViewError> {
        let UiAssistantContent::Indexed(blocks) = content else {
            return Ok(());
        };

        let old_retained = self
            .reasoning
            .iter()
            .filter(|block| block.step == step)
            .try_fold(0_usize, |total, block| total.checked_add(block.text.len()))
            .ok_or(ViewError::Limit)?;
        let old_omitted = self
            .reasoning
            .iter()
            .filter(|block| block.step == step)
            .try_fold(0_usize, |total, block| {
                total.checked_add(block.omitted_bytes)
            })
            .ok_or(ViewError::Limit)?;
        let old_step_omitted = self
            .reasoning_step_omissions
            .iter()
            .find(|omission| omission.step == step)
            .map_or(0, |omission| omission.bytes);
        let retained_blocks = self
            .reasoning
            .iter()
            .filter(|block| block.step != step)
            .count();
        let base_reasoning = self.reasoning_bytes.saturating_sub(old_retained);
        let base_total = self.retained_text_bytes.saturating_sub(old_retained);
        let base_omitted = self
            .omitted_reasoning_bytes
            .saturating_sub(old_omitted)
            .saturating_sub(old_step_omitted);

        let candidates = blocks
            .iter()
            .filter(|block| block.kind == UiAssistantBlockKind::Reasoning);
        let candidate_count = candidates.clone().count();
        let capacity =
            candidate_count.min(MAX_INSPECT_REASONING_BLOCKS.saturating_sub(retained_blocks));
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(capacity)
            .map_err(|_| ViewError::Capacity)?;
        let mut replacement_retained = 0_usize;
        let mut replacement_omitted = 0_usize;
        for block in candidates {
            if retained_blocks.saturating_add(replacement.len()) == MAX_INSPECT_REASONING_BLOCKS {
                replacement_omitted = replacement_omitted
                    .checked_add(block.text.len())
                    .ok_or(ViewError::Limit)?;
                continue;
            }
            let reasoning_room = MAX_INSPECT_REASONING_BYTES
                .saturating_sub(base_reasoning)
                .saturating_sub(replacement_retained);
            let total_room = MAX_INSPECT_TEXT_BYTES
                .saturating_sub(base_total)
                .saturating_sub(replacement_retained);
            let retained_len = utf8_prefix_len(&block.text, reasoning_room.min(total_room));
            let mut text = view_string(retained_len)?;
            text.push_str(&block.text[..retained_len]);
            let omitted_bytes = block.text.len().saturating_sub(retained_len);
            replacement_retained = replacement_retained
                .checked_add(retained_len)
                .ok_or(ViewError::Limit)?;
            replacement_omitted = replacement_omitted
                .checked_add(omitted_bytes)
                .ok_or(ViewError::Limit)?;
            replacement.push(ReasoningBlock {
                step,
                index: u64::from(block.index),
                text,
                authoritative: true,
                first_stamp: stamp,
                last_stamp: stamp,
                fragments: 1,
                original_bytes: block.text.len(),
                omitted_bytes,
            });
        }

        let final_count = retained_blocks
            .checked_add(replacement.len())
            .ok_or(ViewError::Limit)?;
        let mut next_blocks = Vec::new();
        next_blocks
            .try_reserve_exact(final_count)
            .map_err(|_| ViewError::Capacity)?;
        for block in std::mem::take(&mut self.reasoning) {
            if block.step != step {
                next_blocks.push(block);
            }
        }
        next_blocks.extend(replacement);
        self.reasoning = next_blocks;
        self.reasoning_step_omissions
            .retain(|omission| omission.step != step);
        self.reasoning_bytes = base_reasoning
            .checked_add(replacement_retained)
            .ok_or(ViewError::Limit)?;
        self.retained_text_bytes = base_total
            .checked_add(replacement_retained)
            .ok_or(ViewError::Limit)?;
        self.omitted_reasoning_bytes = base_omitted
            .checked_add(replacement_omitted)
            .ok_or(ViewError::Limit)?;
        Ok(())
    }

    fn record_step_reasoning_omission(
        &mut self,
        step: StepId,
        bytes: usize,
    ) -> Result<(), ViewError> {
        if let Some(omission) = self
            .reasoning_step_omissions
            .iter_mut()
            .find(|omission| omission.step == step)
        {
            omission.bytes = omission.bytes.checked_add(bytes).ok_or(ViewError::Limit)?;
        } else {
            if self.reasoning_step_omissions.len() == MAX_INSPECT_REASONING_OMISSION_STEPS {
                self.inspect_degraded = true;
                self.omitted_reasoning_bytes = self
                    .omitted_reasoning_bytes
                    .checked_add(bytes)
                    .ok_or(ViewError::Limit)?;
                return Ok(());
            }
            self.reasoning_step_omissions
                .try_reserve(1)
                .map_err(|_| ViewError::Capacity)?;
            self.reasoning_step_omissions
                .push(ReasoningStepOmission { step, bytes });
        }
        self.omitted_reasoning_bytes = self
            .omitted_reasoning_bytes
            .checked_add(bytes)
            .ok_or(ViewError::Limit)?;
        Ok(())
    }
}

pub(crate) struct DetailLine {
    tone: DetailTone,
    text: String,
}

impl fmt::Debug for DetailLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetailLine")
            .field("tone", &self.tone)
            .field("bytes", &self.text.len())
            .finish()
    }
}

impl DetailLine {
    pub(crate) const fn tone(&self) -> DetailTone {
        self.tone
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

pub(crate) struct DetailDocument {
    mode: ViewMode,
    title: String,
    lines: Vec<DetailLine>,
    source_bytes: usize,
    omitted: bool,
}

impl fmt::Debug for DetailDocument {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DetailDocument")
            .field("mode", &self.mode)
            .field("title_bytes", &self.title.len())
            .field("lines", &self.lines.len())
            .field("source_bytes", &self.source_bytes)
            .field("omitted", &self.omitted)
            .finish()
    }
}

impl DetailDocument {
    #[cfg(test)]
    pub(crate) fn from_lines_for_test(
        mode: ViewMode,
        title: &str,
        lines: &[(DetailTone, &str)],
    ) -> Self {
        let mut builder = DetailBuilder::new(mode, title.to_owned()).unwrap();
        for (tone, text) in lines {
            builder.push(*tone, text).unwrap();
        }
        builder.finish().unwrap()
    }

    pub(crate) fn inspect(archive: &ViewArchive) -> Result<Self, ViewError> {
        let title = archive.turn().map_or_else(
            || "INSPECT · no live turn".to_owned(),
            |turn| {
                format!(
                    "INSPECT · turn {} · {}",
                    turn.get(),
                    if archive.turn_ended {
                        "settled"
                    } else {
                        "live"
                    }
                )
            },
        );
        let mut builder = DetailBuilder::new(ViewMode::Inspect, title)?;
        if archive.resumed_live_seam {
            builder.push(
                DetailTone::Caution,
                "Earlier Session details are unavailable; this view starts at the resumed live seam.",
            )?;
        }
        if let Some(context) = archive.context_estimate.as_ref() {
            builder.push(DetailTone::Muted, &context.status_line()?)?;
        }
        if let Some(usage) = archive.latest_usage {
            builder.push(DetailTone::Muted, &usage_detail(&usage)?)?;
        }
        builder.push(DetailTone::Accent, "REASONING")?;
        if archive.reasoning.is_empty() {
            builder.push(DetailTone::Muted, "No retained reasoning in this turn.")?;
        }
        for block in &archive.reasoning {
            builder.push(
                DetailTone::Muted,
                &format!(
                    "Step {} · block {} · {}",
                    block.step.get(),
                    block.index,
                    if block.authoritative {
                        "authoritative final"
                    } else {
                        "committed stream"
                    }
                ),
            )?;
            builder.push(
                DetailTone::Muted,
                &format!(
                    "  seq {}-{} · time {}..{} unix-ms · {} fragment(s) · {} / {} bytes retained",
                    block.first_stamp.seq.get(),
                    block.last_stamp.seq.get(),
                    block.first_stamp.time.get(),
                    block.last_stamp.time.get(),
                    block.fragments,
                    block.text.len(),
                    block.original_bytes
                ),
            )?;
            builder.push_multiline(DetailTone::Code, &block.text)?;
        }
        if archive.omitted_reasoning_bytes != 0 {
            builder.push(
                DetailTone::Caution,
                &format!(
                    "[reasoning details omitted: {} source bytes]",
                    archive.omitted_reasoning_bytes
                ),
            )?;
        }
        builder.push(DetailTone::Accent, "COMMITTED FACTS")?;
        for row in &archive.rows {
            let mut label = view_string(160 + row.label.len())?;
            write!(&mut label, "seq {}", row.stamp.seq.get()).map_err(|_| ViewError::Capacity)?;
            write!(&mut label, " · time {} unix-ms", row.stamp.time.get())
                .map_err(|_| ViewError::Capacity)?;
            if let Some(turn) = row.turn {
                write!(&mut label, " · turn {}", turn.get()).map_err(|_| ViewError::Capacity)?;
            }
            if let Some(step) = row.step {
                write!(&mut label, " · step {}", step.get()).map_err(|_| ViewError::Capacity)?;
            }
            write!(&mut label, " · {}", row.label).map_err(|_| ViewError::Capacity)?;
            builder.push(tone_for_fact(row.kind), &label)?;
            if let Some(detail) = row.detail.as_deref() {
                builder.push(DetailTone::Muted, &format!("  {detail}"))?;
            }
        }
        if archive.omitted_rows != 0 {
            builder.push(
                DetailTone::Caution,
                &format!(
                    "[Inspect details incomplete: {} row(s) omitted]",
                    archive.omitted_rows
                ),
            )?;
        } else if archive.inspect_degraded {
            builder.push(DetailTone::Caution, "[Inspect details incomplete]")?;
        }
        builder.finish()
    }

    pub(crate) fn review(archive: &ViewArchive) -> Result<Self, ViewError> {
        let Some(review) = archive.joined_review() else {
            let mut builder =
                DetailBuilder::new(ViewMode::Review, "REVIEW · no joined turn".to_owned())?;
            builder.push(
                DetailTone::Muted,
                if archive.review_join_failed_for.is_some() {
                    "The newest settled turn could not be joined to a trustworthy Review."
                } else if archive.resumed_live_seam {
                    "Earlier review details are unavailable after this resume seam."
                } else {
                    "Complete a turn before opening Review."
                },
            )?;
            return builder.finish();
        };
        let mut builder = DetailBuilder::new(
            ViewMode::Review,
            format!(
                "REVIEW · turn {} · end seq {} · settled",
                review.turn().get(),
                review.turn_end_seq().get()
            ),
        )?;
        if let Some(failed_turn) = archive
            .review_join_failed_for
            .filter(|failed| *failed != review.turn())
        {
            builder.push(
                DetailTone::Caution,
                &format!(
                    "Review for turn {} is unavailable; showing the last joined turn.",
                    failed_turn.get()
                ),
            )?;
        }
        builder.push(
            detail_tone(review.receipt().tone()),
            review.receipt().headline(),
        )?;
        if let Some(counters) = review.receipt().counters() {
            builder.push(DetailTone::Muted, counters)?;
        }
        if let Some(effects) = review.receipt().effects() {
            builder.push(DetailTone::Muted, effects)?;
        }
        builder.push(DetailTone::Accent, "ACTIONS")?;
        if review.activities.is_empty() {
            builder.push(DetailTone::Muted, "No tool activity in this joined turn.")?;
        }
        for activity in &review.activities {
            builder.push(detail_tone(activity.tone), &activity.headline)?;
            if let Some(detail) = activity.detail.as_deref() {
                builder.push(DetailTone::Muted, &format!("  {detail}"))?;
            }
        }
        if review.omitted_activities != 0 {
            builder.push(
                DetailTone::Caution,
                &format!(
                    "[Review details incomplete: {} action(s) omitted]",
                    review.omitted_activities
                ),
            )?;
        }
        builder.push(
            DetailTone::Muted,
            "Summary only · canonical diffs and full command records were not retained by this view.",
        )?;
        builder.finish()
    }

    pub(crate) const fn mode(&self) -> ViewMode {
        self.mode
    }

    pub(crate) fn title(&self) -> &str {
        &self.title
    }

    pub(crate) fn lines(&self) -> &[DetailLine] {
        &self.lines
    }

    #[cfg(test)]
    pub(crate) const fn omitted(&self) -> bool {
        self.omitted
    }
}

struct DetailBuilder {
    mode: ViewMode,
    title: String,
    lines: Vec<DetailLine>,
    source_bytes: usize,
    omitted: bool,
}

impl DetailBuilder {
    fn new(mode: ViewMode, title: String) -> Result<Self, ViewError> {
        let title = render_visible_owned_bounded(&title, false, MAX_INSPECT_FIELD_BYTES)
            .map_err(|_| ViewError::Capacity)?
            .unwrap_or_else(|| "[view title omitted]".to_owned());
        let source_bytes = title.len();
        Ok(Self {
            mode,
            title,
            lines: Vec::new(),
            source_bytes,
            omitted: false,
        })
    }

    fn push(&mut self, tone: DetailTone, text: &str) -> Result<(), ViewError> {
        self.push_multiline(tone, text)
    }

    fn push_multiline(&mut self, tone: DetailTone, text: &str) -> Result<(), ViewError> {
        if self.omitted {
            return Ok(());
        }
        let remaining = MAX_DETAIL_TEXT_BYTES.saturating_sub(self.source_bytes);
        let Some(visible) =
            render_visible_owned_bounded(text, true, remaining).map_err(|_| ViewError::Capacity)?
        else {
            return self.omit();
        };
        let next = self
            .source_bytes
            .checked_add(visible.len())
            .ok_or(ViewError::Limit)?;
        if next > MAX_DETAIL_TEXT_BYTES {
            return self.omit();
        }
        let line_count = visible.split('\n').count();
        if self.lines.len().saturating_add(line_count) > MAX_DETAIL_SOURCE_LINES {
            return self.omit();
        }
        self.lines
            .try_reserve(line_count)
            .map_err(|_| ViewError::Capacity)?;
        for line in visible.split('\n') {
            self.lines.push(DetailLine {
                tone,
                text: copy_text(line)?,
            });
        }
        self.source_bytes = next;
        Ok(())
    }

    fn omit(&mut self) -> Result<(), ViewError> {
        if self.omitted {
            return Ok(());
        }
        let marker = copy_text("[view details omitted: presentation limit exceeded]")?;
        self.omitted = true;
        if self.lines.len() == MAX_DETAIL_SOURCE_LINES {
            let _ = self.lines.pop();
        }
        self.lines.try_reserve(1).map_err(|_| ViewError::Capacity)?;
        self.lines.push(DetailLine {
            tone: DetailTone::Caution,
            text: marker,
        });
        Ok(())
    }

    fn finish(self) -> Result<DetailDocument, ViewError> {
        Ok(DetailDocument {
            mode: self.mode,
            title: self.title,
            lines: self.lines,
            source_bytes: self.source_bytes,
            omitted: self.omitted,
        })
    }
}

fn payload_detail(name: &str, payload: &UiOpaquePayload) -> Result<String, ViewError> {
    let state = if payload.as_str().is_some() {
        "retained"
    } else {
        "omitted"
    };
    let mut output = view_string(128 + name.len())?;
    write!(
        &mut output,
        "{name} {} / {} bytes {state}",
        payload.retained_bytes(),
        payload.original_bytes(),
    )
    .map_err(|_| ViewError::Capacity)?;
    if payload.omitted_parts() != 0 {
        write!(
            &mut output,
            " · {} non-text part(s) omitted",
            payload.omitted_parts()
        )
        .map_err(|_| ViewError::Capacity)?;
    }
    Ok(output)
}

fn usage_detail(usage: &UiTokenUsage) -> Result<String, ViewError> {
    let mut output = view_string(192)?;
    write!(
        &mut output,
        "reported usage · input {} · output {}",
        usage.input_tokens, usage.output_tokens
    )
    .map_err(|_| ViewError::Capacity)?;
    if let Some(tokens) = usage.cache_read_tokens {
        write!(&mut output, " · cache read {tokens}").map_err(|_| ViewError::Capacity)?;
    }
    if let Some(tokens) = usage.cache_write_tokens {
        write!(&mut output, " · cache write {tokens}").map_err(|_| ViewError::Capacity)?;
    }
    if let Some(tokens) = usage.reasoning_tokens {
        write!(&mut output, " · reasoning {tokens}").map_err(|_| ViewError::Capacity)?;
    }
    Ok(output)
}

fn approval_outcome_label(outcome: ApprovalOutcome) -> &'static str {
    match outcome {
        ApprovalOutcome::AllowedOnce => "Approval allowed once",
        ApprovalOutcome::Rejected => "Approval rejected",
        ApprovalOutcome::Cancelled => "Approval cancelled",
        ApprovalOutcome::Unavailable => "Approval unavailable",
    }
}

fn tone_for_fact(kind: FactKind) -> DetailTone {
    match kind {
        FactKind::ToolResult | FactKind::TurnEnd | FactKind::ApprovalDecided => DetailTone::Plain,
        FactKind::RetryScheduled
        | FactKind::RetryStarted
        | FactKind::ApprovalAsked
        | FactKind::CompactionStarted
        | FactKind::CompactionSummarized
        | FactKind::CompactionPrune => DetailTone::Caution,
        FactKind::CompactionEnded => DetailTone::Plain,
        FactKind::TurnStart
        | FactKind::StepStart
        | FactKind::StepEnd
        | FactKind::UserMessage
        | FactKind::AssistantFinal
        | FactKind::ToolRequested
        | FactKind::RequestContext => DetailTone::Muted,
    }
}

fn turn_end_fact(reason: &UiTurnEndReason) -> Result<(&'static str, Option<String>), ViewError> {
    match reason {
        UiTurnEndReason::Completed => Ok(("Turn completed", None)),
        UiTurnEndReason::Blocked => Ok(("Turn blocked", None)),
        UiTurnEndReason::MaxTokens => Ok(("Turn reached token limit", None)),
        UiTurnEndReason::Interrupted => Ok(("Turn interrupted", None)),
        UiTurnEndReason::Aborted { cause } => {
            let cause = match cause {
                UiTurnEndCancelCause::User => "user",
                UiTurnEndCancelCause::Parent => "parent",
                UiTurnEndCancelCause::Hook => "hook",
                UiTurnEndCancelCause::Disposed => "disposed",
                UiTurnEndCancelCause::Legacy => "legacy",
            };
            Ok(("Turn aborted", Some(joined_fields("cause", cause)?)))
        }
        UiTurnEndReason::Error { code, message } => {
            let mut detail = joined_fields("error code", code)?;
            append_metadata_piece(
                &mut detail,
                &format!("message field {} displayed bytes", message.len()),
            )?;
            Ok(("Turn failed", Some(detail)))
        }
        UiTurnEndReason::Other { kind } => Ok((
            "Turn ended with an extension outcome",
            kind.as_deref()
                .map(|kind| joined_fields("outcome kind", kind))
                .transpose()?,
        )),
    }
}

fn detail_tone(tone: TimelineTone) -> DetailTone {
    match tone {
        TimelineTone::Accent => DetailTone::Accent,
        TimelineTone::Positive => DetailTone::Positive,
        TimelineTone::Caution => DetailTone::Caution,
        TimelineTone::Negative => DetailTone::Negative,
    }
}

fn append_optional_identity(
    output: &mut String,
    label: &str,
    value: Option<&crate::session::UiIdentity>,
) -> Result<(), ViewError> {
    if let Some(value) = value {
        append_metadata_piece(output, &joined_fields(label, value.as_str())?)?;
    }
    Ok(())
}

fn identity_availability(
    label: &str,
    value: &crate::session::UiIdentity,
) -> Result<String, ViewError> {
    let mut output = view_string(96 + label.len())?;
    write!(
        &mut output,
        "{label} {} bytes {}",
        value.original_bytes(),
        if value.was_omitted() {
            "omitted"
        } else {
            "retained"
        }
    )
    .map_err(|_| ViewError::Capacity)?;
    Ok(output)
}

fn append_metadata_piece(output: &mut String, piece: &str) -> Result<(), ViewError> {
    if piece.is_empty() {
        return Ok(());
    }
    let extra = piece
        .len()
        .checked_add(if output.is_empty() { 0 } else { 3 })
        .ok_or(ViewError::Limit)?;
    output.try_reserve(extra).map_err(|_| ViewError::Capacity)?;
    if !output.is_empty() {
        output.push_str(" · ");
    }
    output.push_str(piece);
    Ok(())
}

fn joined_fields(left: &str, right: &str) -> Result<String, ViewError> {
    let capacity = left
        .len()
        .checked_add(right.len())
        .and_then(|value| value.checked_add(3))
        .ok_or(ViewError::Limit)?;
    let mut output = view_string(capacity)?;
    output.push_str(left);
    output.push_str(" · ");
    output.push_str(right);
    Ok(output)
}

fn bounded_field(value: &str) -> Result<String, ViewError> {
    if value.len() <= MAX_INSPECT_FIELD_BYTES {
        return copy_text(value);
    }
    let mut output = view_string(64)?;
    write!(&mut output, "[omitted {}-byte field]", value.len()).map_err(|_| ViewError::Capacity)?;
    Ok(output)
}

fn copy_text(value: &str) -> Result<String, ViewError> {
    let mut output = view_string(value.len())?;
    output.push_str(value);
    Ok(output)
}

fn view_string(capacity: usize) -> Result<String, ViewError> {
    let mut output = String::new();
    output
        .try_reserve_exact(capacity)
        .map_err(|_| ViewError::Capacity)?;
    Ok(output)
}

fn write_compact_number(output: &mut String, value: u64) -> Result<(), ViewError> {
    if value >= 1_000_000 {
        write!(output, "{:.1}M", value as f64 / 1_000_000.0).map_err(|_| ViewError::Capacity)
    } else if value >= 1_000 {
        write!(output, "{:.1}k", value as f64 / 1_000.0).map_err(|_| ViewError::Capacity)
    } else {
        write!(output, "{value}").map_err(|_| ViewError::Capacity)
    }
}

fn utf8_prefix_len(value: &str, maximum: usize) -> usize {
    let mut end = maximum.min(value.len());
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use crate::model::ContextForm;
    use crate::session::{
        CommittedUiEvent, CommittedUiKind, EventSeq, StepId, TurnId, UiAssistantBlock,
        UiAssistantBlockKind, UiAssistantContent, UiIdentity, UiOpaquePayload, UiToolFailure,
        UiUserSource, UnixMillis,
    };

    use super::{
        ContextEstimate, DetailBuilder, DetailDocument, DetailTone, FactKind, FactStamp,
        MAX_DETAIL_SOURCE_LINES, MAX_DETAIL_TEXT_BYTES, MAX_INSPECT_REASONING_BYTES,
        MAX_INSPECT_ROWS, MAX_INSPECT_TEXT_BYTES, ViewArchive, ViewMode, ViewState,
    };

    fn event(seq: u64, kind: CommittedUiKind) -> CommittedUiEvent {
        CommittedUiEvent {
            seq: EventSeq::new(seq).unwrap(),
            time: UnixMillis::new(1_000 + i64::try_from(seq).unwrap()).unwrap(),
            kind,
        }
    }

    fn turn() -> TurnId {
        TurnId::new(1).unwrap()
    }

    fn step() -> StepId {
        StepId::new(1).unwrap()
    }

    fn id(value: &str) -> UiIdentity {
        UiIdentity::from_text_for_test(value)
    }

    #[test]
    fn inspect_keeps_stamps_and_payload_availability_but_not_payload_bodies() {
        let secret = "SECRET_INSPECT_PAYLOAD";
        let mut archive = ViewArchive::new(false);
        archive.observe(&event(0, CommittedUiKind::TurnStart { turn: turn() }));
        archive.observe(&event(
            1,
            CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("call-read"),
                name: id("read"),
                arguments: UiOpaquePayload::from_text_for_test(secret),
            },
        ));
        archive.observe(&event(
            2,
            CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call-read"),
                is_error: true,
                failure: Some(UiToolFailure {
                    name: "ReadFailure".to_owned(),
                    code: "READ_FAILED".to_owned(),
                }),
                content: UiOpaquePayload::from_text_for_test(secret),
                meta: UiOpaquePayload::from_text_for_test(secret),
                surface_replacement_target: None,
            },
        ));
        let document = DetailDocument::inspect(&archive).unwrap();
        let visible = document
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("seq 1"));
        assert!(visible.contains("arguments 22 / 22 bytes retained"));
        assert!(visible.contains("result 22 / 22 bytes retained"));
        assert!(visible.contains("READ_FAILED"));
        assert!(!visible.contains(secret));
        assert!(!format!("{archive:?}").contains(secret));
    }

    #[test]
    fn authoritative_reasoning_replaces_streamed_fragments_without_duplication() {
        let mut archive = ViewArchive::new(false);
        archive.observe(&event(0, CommittedUiKind::TurnStart { turn: turn() }));
        archive.observe(&event(
            1,
            CommittedUiKind::AssistantReasoningDelta {
                turn: turn(),
                step: step(),
                index: 0,
                text: "partial secret reasoning".to_owned(),
            },
        ));
        archive.observe(&event(
            2,
            CommittedUiKind::AssistantMessage {
                turn: turn(),
                step: step(),
                content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                    index: 0,
                    kind: UiAssistantBlockKind::Reasoning,
                    text: "authoritative reasoning".to_owned(),
                }]),
                sources: crate::session::SourceSeqBitmap::from_sources(&[]).unwrap(),
                provider: id("deepseek"),
                model: id("model"),
                usage: None,
            },
        ));
        let document = DetailDocument::inspect(&archive).unwrap();
        let visible = document
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("authoritative reasoning"));
        assert!(!visible.contains("partial secret reasoning"));
    }

    #[test]
    fn row_and_reasoning_limits_degrade_without_erasing_retained_facts() {
        let mut archive = ViewArchive::new(false);
        archive.observe(&event(0, CommittedUiKind::TurnStart { turn: turn() }));
        for seq in 1..=u64::try_from(MAX_INSPECT_ROWS + 1).unwrap() {
            archive.observe(&event(
                seq,
                CommittedUiKind::StepStart {
                    turn: turn(),
                    step: step(),
                },
            ));
        }
        archive.observe(&event(
            600,
            CommittedUiKind::AssistantReasoningDelta {
                turn: turn(),
                step: step(),
                index: 0,
                text: "x".repeat(MAX_INSPECT_REASONING_BYTES),
            },
        ));
        archive.observe(&event(
            601,
            CommittedUiKind::AssistantReasoningDelta {
                turn: turn(),
                step: step(),
                index: 0,
                text: "y".to_owned(),
            },
        ));
        assert!(archive.is_degraded());
        assert_eq!(archive.rows.len(), MAX_INSPECT_ROWS);
        assert_eq!(archive.reasoning_bytes, MAX_INSPECT_REASONING_BYTES);
        assert_eq!(archive.omitted_reasoning_bytes, 1);
        let document = DetailDocument::inspect(&archive).unwrap();
        assert!(
            document
                .lines()
                .iter()
                .any(|line| line.text().contains("Inspect details incomplete"))
        );
        assert!(
            document
                .lines()
                .iter()
                .any(|line| line.text().contains("reasoning details omitted"))
        );
    }

    #[test]
    fn user_messages_keep_only_source_and_payload_availability() {
        let secret = "SECRET_USER_MESSAGE";
        let mut archive = ViewArchive::new(false);
        archive.observe(&event(0, CommittedUiKind::TurnStart { turn: turn() }));
        archive.observe(&event(
            1,
            CommittedUiKind::UserMessage {
                source: UiUserSource::Context {
                    plugin: "SECRET_PLUGIN_ID".to_owned(),
                    form: Some(ContextForm::Snapshot),
                },
                content: UiOpaquePayload::from_text_for_test(secret),
            },
        ));
        let document = DetailDocument::inspect(&archive).unwrap();
        let visible = document
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("Context message committed"));
        assert!(visible.contains("content 19 / 19 bytes retained"));
        assert!(visible.contains("context plugin field 16 displayed bytes"));
        assert!(visible.contains("form Snapshot"));
        assert!(!visible.contains(secret));
        assert!(!visible.contains("SECRET_PLUGIN_ID"));
        assert!(!format!("{archive:?}").contains("SECRET"));
    }

    #[test]
    fn inspect_uses_commit_timestamps_without_claiming_duration() {
        let mut archive = ViewArchive::new(false);
        archive.observe(&CommittedUiEvent {
            seq: EventSeq::new(0).unwrap(),
            time: UnixMillis::new(2_000).unwrap(),
            kind: CommittedUiKind::TurnStart { turn: turn() },
        });
        archive.observe(&CommittedUiEvent {
            seq: EventSeq::new(1).unwrap(),
            time: UnixMillis::new(1_000).unwrap(),
            kind: CommittedUiKind::StepStart {
                turn: turn(),
                step: step(),
            },
        });
        let visible = DetailDocument::inspect(&archive)
            .unwrap()
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(visible.contains("time 2000 unix-ms"));
        assert!(visible.contains("time 1000 unix-ms"));
        assert!(!visible.contains("+0 ms"));
        assert!(!visible.contains("duration"));
    }

    #[test]
    fn context_estimate_names_its_session_boundary_and_does_not_clamp_percent() {
        let estimate = ContextEstimate::new(
            EventSeq::new(10).unwrap(),
            Some("deepseek"),
            Some("deepseek-chat"),
            250,
            Some(100),
            Some(turn()),
        )
        .unwrap();
        let status = estimate.status_line().unwrap();
        assert!(status.contains("Session context estimate"));
        assert!(status.contains("250%"));
        assert!(status.contains("after turn 1"));
        assert!(status.contains("sampled before seq 10"));
        assert!(!status.contains("at seq"));
    }

    #[test]
    fn authoritative_reasoning_replacement_is_bounded_and_atomic_at_one_over() {
        let mut archive = ViewArchive::new(false);
        archive.observe(&event(0, CommittedUiKind::TurnStart { turn: turn() }));
        archive.observe(&event(
            1,
            CommittedUiKind::AssistantReasoningDelta {
                turn: turn(),
                step: step(),
                index: 0,
                text: "old streamed reasoning".to_owned(),
            },
        ));
        archive.observe(&event(
            2,
            CommittedUiKind::AssistantMessage {
                turn: turn(),
                step: step(),
                content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                    index: 0,
                    kind: UiAssistantBlockKind::Reasoning,
                    text: "z".repeat(MAX_INSPECT_REASONING_BYTES + 1),
                }]),
                sources: crate::session::SourceSeqBitmap::from_sources(&[]).unwrap(),
                provider: id("deepseek"),
                model: id("model"),
                usage: None,
            },
        ));
        assert_eq!(archive.reasoning.len(), 1);
        assert!(archive.reasoning[0].authoritative);
        assert_eq!(archive.reasoning[0].text.len(), MAX_INSPECT_REASONING_BYTES);
        assert_eq!(
            archive.reasoning[0].original_bytes,
            MAX_INSPECT_REASONING_BYTES + 1
        );
        assert_eq!(archive.reasoning[0].omitted_bytes, 1);
        assert_eq!(archive.omitted_reasoning_bytes, 1);
        assert!(!archive.reasoning[0].text.contains("old streamed reasoning"));
        let visible = DetailDocument::inspect(&archive)
            .unwrap()
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(visible.matches("reasoning details omitted").count(), 1);
    }

    #[test]
    fn authoritative_final_clears_stream_omissions_that_belong_to_the_same_step() {
        for final_blocks in [
            vec![UiAssistantBlock {
                index: 0,
                kind: UiAssistantBlockKind::Reasoning,
                text: "short final reasoning".to_owned(),
            }],
            Vec::new(),
        ] {
            let mut archive = ViewArchive::new(false);
            archive.observe(&event(0, CommittedUiKind::TurnStart { turn: turn() }));
            for index in 0..=u64::try_from(super::MAX_INSPECT_REASONING_BLOCKS).unwrap() {
                archive.observe(&event(
                    index + 1,
                    CommittedUiKind::AssistantReasoningDelta {
                        turn: turn(),
                        step: step(),
                        index,
                        text: "x".to_owned(),
                    },
                ));
            }
            assert_eq!(archive.omitted_reasoning_bytes, 1);
            archive.observe(&event(
                200,
                CommittedUiKind::AssistantMessage {
                    turn: turn(),
                    step: step(),
                    content: UiAssistantContent::Indexed(final_blocks),
                    sources: crate::session::SourceSeqBitmap::from_sources(&[]).unwrap(),
                    provider: id("deepseek"),
                    model: id("model"),
                    usage: None,
                },
            ));
            assert_eq!(archive.omitted_reasoning_bytes, 0);
            assert!(archive.reasoning_step_omissions.is_empty());
            assert!(archive.reasoning.iter().all(|block| block.authoritative));
        }
    }

    #[test]
    fn inspect_total_text_budget_accepts_exact_and_omits_one_over() {
        let mut archive = ViewArchive::new(false);
        let label = "m".repeat(4 * 1024);
        for seq in 0..128_u64 {
            archive
                .push_row(
                    FactStamp {
                        seq: EventSeq::new(seq).unwrap(),
                        time: UnixMillis::new(i64::try_from(seq).unwrap()).unwrap(),
                    },
                    FactKind::StepStart,
                    Some(turn()),
                    Some(step()),
                    &label,
                    None,
                )
                .unwrap();
        }
        assert_eq!(archive.retained_text_bytes, MAX_INSPECT_TEXT_BYTES);
        assert_eq!(archive.omitted_rows, 0);
        archive
            .push_row(
                FactStamp {
                    seq: EventSeq::new(129).unwrap(),
                    time: UnixMillis::new(129).unwrap(),
                },
                FactKind::StepStart,
                Some(turn()),
                Some(step()),
                "x",
                None,
            )
            .unwrap();
        assert_eq!(archive.retained_text_bytes, MAX_INSPECT_TEXT_BYTES);
        assert_eq!(archive.omitted_rows, 1);
    }

    #[test]
    fn detail_document_counts_newlines_and_always_marks_one_over() {
        let mut builder = DetailBuilder::new(ViewMode::Inspect, "T".to_owned()).unwrap();
        builder
            .push(DetailTone::Plain, &"x".repeat(MAX_DETAIL_TEXT_BYTES - 1))
            .unwrap();
        let exact = builder.finish().unwrap();
        assert_eq!(exact.source_bytes, MAX_DETAIL_TEXT_BYTES);
        assert!(!exact.omitted());

        let mut lines = DetailBuilder::new(ViewMode::Inspect, "T".to_owned()).unwrap();
        for _ in 0..MAX_DETAIL_SOURCE_LINES {
            lines.push(DetailTone::Plain, "").unwrap();
        }
        assert_eq!(lines.lines.len(), MAX_DETAIL_SOURCE_LINES);
        lines.push(DetailTone::Plain, "one over").unwrap();
        let omitted = lines.finish().unwrap();
        assert!(omitted.omitted());
        assert_eq!(omitted.lines().len(), MAX_DETAIL_SOURCE_LINES);
        assert_eq!(
            omitted.lines().last().unwrap().text(),
            "[view details omitted: presentation limit exceeded]"
        );
    }

    #[test]
    fn view_state_commits_only_the_screen_transaction_that_was_requested() {
        let mut state = ViewState::default();
        let focus = state.requested();
        state.toggle_inspect().unwrap();
        let inspect = state.requested();
        assert_eq!(inspect.mode(), ViewMode::Inspect);
        assert!(!state.commit(focus, 0, 0, 0));
        assert_eq!(state.committed().mode(), ViewMode::Focus);
        state.request_offset(12).unwrap();
        let scrolled = state.requested();
        assert!(!state.commit(inspect, 0, 100, 20));
        assert!(state.commit(scrolled, 9, 100, 20));
        assert_eq!(state.committed().mode(), ViewMode::Inspect);
        assert_eq!(state.committed().offset(), 9);
        assert_eq!(state.requested().offset(), 9);
        state.scroll_page(true).unwrap();
        assert_eq!(state.requested().offset(), 29);
        state.scroll_end().unwrap();
        assert_eq!(state.requested().offset(), 80);

        state.switch_detail().unwrap();
        assert_eq!(state.requested().mode(), ViewMode::Review);
        state.request_mode(ViewMode::Focus).unwrap();
        assert_eq!(state.requested().mode(), ViewMode::Focus);
        assert_eq!(state.requested().offset(), 0);
    }
}
