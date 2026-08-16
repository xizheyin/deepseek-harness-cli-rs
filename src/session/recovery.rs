//! Read-only, bounded cold scanning for durable JSONL sessions.

use std::{
    io::Read,
    sync::atomic::{AtomicBool, Ordering},
};

use aws_lc_rs::digest::{Context, SHA256};
use serde_json::Value;

use crate::{
    json_value::JsonValue,
    model::{ContentBlock, Message},
    workspace_authority::WorkspaceIdentity,
};

use super::{
    ApprovalDecidedEvent, ApprovalOutcome, CodecError, EventKind, EventSeq, EventValidationError,
    MAX_SAFE_INTEGER, NewEvent, SESSION_FORMAT_VERSION, SessionEvent, SessionHeader, SessionId,
    StoreError, SurfaceIntent, TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN, ToolFailure,
    TransitionError, TurnEndReason, UnixMillis,
    codec::{decode_event, kind_data_value},
    jsonl::{MAX_JOURNAL_EVENT_LINE_BYTES, MAX_JOURNAL_HEADER_LINE_BYTES},
    projection::{Projection, RecoveryApproval, ValidationPolicy},
};

const SCAN_BLOCK_BYTES: usize = 64 * 1024;
const MAX_DURABLE_JOURNAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_DURABLE_LOGICAL_EVENTS: u64 = 1_000_000;
const MAX_RECOVERY_ROWS: usize = 68;
const TOOL_NOT_STARTED_TEXT: &str = "The tool call was interrupted before the Harness recorded it as started. Retry it if it is still needed.";
const TOOL_OUTCOME_UNKNOWN_TEXT: &str = "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly.";
const APPROVAL_REJECTED_TEXT: &str =
    "The tool call was not started because its approval request was rejected.";
const APPROVAL_CANCELLED_TEXT: &str = "The tool call was not started because its approval request was cancelled. No tool body was dispatched.";
const APPROVAL_UNAVAILABLE_TEXT: &str =
    "The tool call was not started because approval was unavailable.";

/// One fully prevalidated, bounded suffix owned by the resume lifecycle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryPlan {
    events: Vec<SessionEvent>,
    resume_seed_len: u64,
    repaired_calls: usize,
    unknown_outcomes: usize,
    not_started: usize,
}

/// Sealed proof that the cold scanner matched one complete recovery row.
///
/// Its field and constructor stay private to this module. Other Session code
/// can consume the proof, but cannot manufacture a broad "historical" escape
/// hatch for an arbitrary event.
pub(super) struct RecoveryAdmission<'a> {
    event: &'a SessionEvent,
}

impl<'a> RecoveryAdmission<'a> {
    fn new(event: &'a SessionEvent) -> Self {
        Self { event }
    }

    pub(super) fn event(&self) -> &'a SessionEvent {
        self.event
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecoveryCursor {
    root_last_real_seq: Option<EventSeq>,
    confirmed: bool,
}

#[derive(Clone, Copy)]
struct ColdEventContext<'a> {
    session_id: &'a SessionId,
    expected_seq: Option<EventSeq>,
    previous_time: Option<UnixMillis>,
    logical_events: u64,
    previous_ends_with_seed: bool,
}

/// Compact, non-secret description of an interrupted basic compaction bracket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecoveryCompactionStage {
    Started,
    Summarized,
    Replaced,
}

/// Sanitized, bounded facts shown before resume is allowed to mutate a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryReport {
    truncated_bytes: u64,
    repaired_calls: Vec<RecoveryCallReport>,
    closes_step: bool,
    closes_turn: bool,
    adds_seed_marker: bool,
    interrupted_compaction: Option<RecoveryCompactionStage>,
    orphan_prune_markers: u64,
}

impl RecoveryReport {
    pub(crate) fn truncated_bytes(&self) -> u64 {
        self.truncated_bytes
    }

    pub(crate) fn repaired_calls(&self) -> &[RecoveryCallReport] {
        &self.repaired_calls
    }

    pub(crate) fn closes_step(&self) -> bool {
        self.closes_step
    }

    pub(crate) fn closes_turn(&self) -> bool {
        self.closes_turn
    }

    pub(crate) fn adds_seed_marker(&self) -> bool {
        self.adds_seed_marker
    }

    pub(crate) fn interrupted_compaction(&self) -> Option<RecoveryCompactionStage> {
        self.interrupted_compaction
    }

    pub(crate) fn orphan_prune_markers(&self) -> u64 {
        self.orphan_prune_markers
    }

    pub(crate) fn needs_warning(&self) -> bool {
        self.truncated_bytes != 0
            || !self.repaired_calls.is_empty()
            || self.closes_step
            || self.closes_turn
            || self.interrupted_compaction.is_some()
            || self.orphan_prune_markers != 0
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        truncated_bytes: u64,
        repaired_calls: Vec<RecoveryCallReport>,
        closes_step: bool,
        closes_turn: bool,
        adds_seed_marker: bool,
    ) -> Self {
        Self {
            truncated_bytes,
            repaired_calls,
            closes_step,
            closes_turn,
            adds_seed_marker,
            interrupted_compaction: None,
            orphan_prune_markers: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_compaction_for_test(
        mut self,
        stage: Option<RecoveryCompactionStage>,
        orphan_prune_markers: u64,
    ) -> Self {
        self.interrupted_compaction = stage;
        self.orphan_prune_markers = orphan_prune_markers;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryCallReport {
    tool_label: String,
    call_fingerprint: String,
    code: String,
}

impl RecoveryCallReport {
    pub(crate) fn tool_label(&self) -> &str {
        &self.tool_label
    }

    pub(crate) fn call_fingerprint(&self) -> &str {
        &self.call_fingerprint
    }

    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        tool_label: impl Into<String>,
        call_fingerprint: impl Into<String>,
        code: impl Into<String>,
    ) -> Self {
        Self {
            tool_label: tool_label.into(),
            call_fingerprint: call_fingerprint.into(),
            code: code.into(),
        }
    }
}

/// Fully validated active state that can be installed without another
/// fallible allocation after the journal repair becomes durable.
pub(crate) struct RecoveredSeed {
    pub(crate) header: SessionHeader,
    pub(crate) projection: Projection,
    pub(crate) next_seq: EventSeq,
    pub(crate) first_live_seq: usize,
    pub(crate) logical_event_count: u64,
    pub(crate) accepted_journal_bytes: u64,
}

impl RecoveredSeed {
    pub(crate) fn new(
        header: SessionHeader,
        projection: Projection,
        next_seq: Option<EventSeq>,
        first_live_seq: u64,
        logical_event_count: u64,
        accepted_journal_bytes: u64,
    ) -> Result<Self, StoreError> {
        let next_seq = next_seq.ok_or(StoreError::Limit)?;
        if logical_event_count > MAX_DURABLE_LOGICAL_EVENTS
            || accepted_journal_bytes > MAX_DURABLE_JOURNAL_BYTES
        {
            return Err(StoreError::Limit);
        }
        if EventSeq::new(logical_event_count).ok() != Some(next_seq)
            || first_live_seq > logical_event_count
        {
            return Err(StoreError::Corrupt);
        }
        Ok(Self {
            header,
            projection,
            next_seq,
            first_live_seq: usize::try_from(first_live_seq).map_err(|_| StoreError::Limit)?,
            logical_event_count,
            accepted_journal_bytes,
        })
    }
}

impl RecoveryPlan {
    pub(crate) fn events(&self) -> &[SessionEvent] {
        &self.events
    }

    /// Logical seed length after closers and before a newly appended marker.
    pub(crate) fn resume_seed_len(&self) -> u64 {
        self.resume_seed_len
    }

    pub(crate) fn repaired_calls(&self) -> usize {
        self.repaired_calls
    }

    #[cfg(test)]
    pub(crate) fn unknown_outcomes(&self) -> usize {
        self.unknown_outcomes
    }

    #[cfg(test)]
    pub(crate) fn not_started(&self) -> usize {
        self.not_started
    }
}

/// Immutable read-only facts used by the later recovery-plan preflight.
pub(crate) struct ColdScan {
    header: SessionHeader,
    #[cfg(test)]
    workspace_identity: WorkspaceIdentity,
    projection: Projection,
    physical_bytes: u64,
    valid_bytes: u64,
    logical_events: u64,
    next_seq: Option<EventSeq>,
    last_event_time: Option<UnixMillis>,
    ends_with_seed: bool,
    physical_sha256: [u8; 32],
    recovery_cursor: Option<RecoveryCursor>,
}

impl ColdScan {
    pub(crate) fn header(&self) -> &SessionHeader {
        &self.header
    }

    #[cfg(test)]
    pub(crate) fn workspace_identity(&self) -> WorkspaceIdentity {
        self.workspace_identity
    }

    #[cfg(test)]
    pub(crate) fn projection(&self) -> &Projection {
        &self.projection
    }

    pub(crate) fn physical_bytes(&self) -> u64 {
        self.physical_bytes
    }

    pub(crate) fn valid_bytes(&self) -> u64 {
        self.valid_bytes
    }

    pub(crate) fn truncated_bytes(&self) -> u64 {
        self.physical_bytes - self.valid_bytes
    }

    pub(crate) fn logical_events(&self) -> u64 {
        self.logical_events
    }

    pub(crate) fn next_seq(&self) -> Option<EventSeq> {
        self.next_seq
    }

    #[cfg(test)]
    pub(crate) fn last_event_time(&self) -> Option<UnixMillis> {
        self.last_event_time
    }

    pub(crate) fn ends_with_seed(&self) -> bool {
        self.ends_with_seed
    }

    pub(crate) fn physical_sha256(&self) -> &[u8; 32] {
        &self.physical_sha256
    }

    pub(crate) fn prepare_recovery(
        &self,
        recovery_time: UnixMillis,
    ) -> Result<(RecoveryPlan, Projection), StoreError> {
        if !self.projection.compaction_recovery_is_consistent() {
            return Err(StoreError::Corrupt);
        }
        let last_prefix_seq = self.recovery_cursor.map_or_else(
            || {
                self.next_seq
                    .and_then(|next| next.get().checked_sub(1))
                    .and_then(|value| EventSeq::new(value).ok())
            },
            |cursor| cursor.root_last_real_seq,
        );
        build_recovery_plan(
            &self.projection,
            self.header.id(),
            self.next_seq,
            last_prefix_seq,
            self.last_event_time,
            recovery_time,
            self.ends_with_seed,
            self.logical_events,
        )
    }

    pub(crate) fn recovery_report(
        &self,
        plan: &RecoveryPlan,
    ) -> Result<RecoveryReport, StoreError> {
        let snapshot = self.projection.recovery_snapshot();
        let mut repaired_calls = Vec::new();
        repaired_calls
            .try_reserve_exact(plan.repaired_calls())
            .map_err(|_| StoreError::Limit)?;
        let known_tools = self
            .projection
            .request_header()
            .and_then(|header| header.tools.as_deref());
        for call in snapshot.calls().iter().filter(|call| !call.result_seen()) {
            let (_, code, _, _) = recovery_result_kind(call);
            let known = known_tools
                .is_some_and(|tools| tools.iter().any(|schema| schema.name() == call.name()));
            repaired_calls.push(RecoveryCallReport {
                tool_label: if known {
                    call.name().to_owned()
                } else {
                    "<unknown-tool>".to_owned()
                },
                call_fingerprint: call_fingerprint(call.id())?,
                code: code.to_owned(),
            });
        }
        Ok(RecoveryReport {
            truncated_bytes: self.truncated_bytes(),
            repaired_calls,
            closes_step: snapshot.step().is_some(),
            closes_turn: snapshot.turn().is_some(),
            adds_seed_marker: !self.ends_with_seed(),
            interrupted_compaction: self.projection.interrupted_compaction_stage(),
            orphan_prune_markers: self.projection.orphan_prune_markers(),
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn build_recovery_plan(
    projection: &Projection,
    session_id: &SessionId,
    next_seq: Option<EventSeq>,
    last_prefix_seq: Option<EventSeq>,
    closer_time: Option<UnixMillis>,
    marker_time: UnixMillis,
    ends_with_seed: bool,
    logical_events: u64,
) -> Result<(RecoveryPlan, Projection), StoreError> {
    let mut events = Vec::new();
    events
        .try_reserve_exact(MAX_RECOVERY_ROWS)
        .map_err(|_| StoreError::Limit)?;
    let mut next = next_seq;
    let snapshot = projection.recovery_snapshot();
    let mut repaired_calls = 0_usize;
    let mut unknown_outcomes = 0_usize;
    let mut not_started = 0_usize;

    if let Some(step) = snapshot.step() {
        let turn = snapshot.turn().ok_or(StoreError::Corrupt)?;
        let time = closer_time.ok_or(StoreError::Corrupt)?;

        // A decision is durable before any result which depends on it. There
        // can be only one pending approval, but iterating in declaration order
        // keeps the rule explicit and future-proof inside the fixed 64-call cap.
        for call in snapshot.calls().iter().filter(|call| {
            !call.result_seen() && matches!(call.approval(), RecoveryApproval::Pending { .. })
        }) {
            let RecoveryApproval::Pending { id } = call.approval() else {
                continue;
            };
            push_recovery_event(
                &mut events,
                &mut next,
                time,
                NewEvent::log(EventKind::approval_decided(
                    ApprovalDecidedEvent::new(id.clone(), ApprovalOutcome::Cancelled)
                        .map_err(|_| StoreError::Corrupt)?,
                )),
            )?;
        }

        for call in snapshot.calls().iter().filter(|call| !call.result_seen()) {
            let result_seq = next.ok_or(StoreError::Limit)?;
            let (failure_name, failure_code, text, source) = recovery_result_kind(call);
            if failure_code == TOOL_OUTCOME_UNKNOWN {
                unknown_outcomes = unknown_outcomes.checked_add(1).ok_or(StoreError::Limit)?;
            }
            if failure_code == TOOL_NOT_STARTED {
                not_started = not_started.checked_add(1).ok_or(StoreError::Limit)?;
            }
            repaired_calls = repaired_calls.checked_add(1).ok_or(StoreError::Limit)?;
            let message_id = recovery_message_id(
                session_id,
                last_prefix_seq,
                call.id(),
                call.intent_seq(),
                result_seq,
            )?;
            let message = Message::tool_result(
                message_id,
                call.id().clone(),
                vec![ContentBlock::text(text).map_err(|_| StoreError::Limit)?],
                true,
            )
            .map_err(|_| StoreError::Limit)?;
            let kind = EventKind::ToolResult {
                turn,
                step,
                message,
                error: Some(ToolFailure {
                    name: failure_name.to_owned(),
                    code: failure_code.to_owned(),
                }),
                meta: None,
            };
            let event = match source {
                Some(source) => {
                    NewEvent::surface(kind, SurfaceIntent::append().with_sources(vec![source]))
                }
                None => NewEvent::surface(kind, SurfaceIntent::append()),
            };
            push_recovery_event(&mut events, &mut next, time, event)?;
        }

        push_recovery_event(
            &mut events,
            &mut next,
            time,
            NewEvent::log(EventKind::step_end(turn, step)),
        )?;
    }

    if snapshot.turn().is_some() {
        let turn = snapshot.turn().ok_or(StoreError::Corrupt)?;
        let time = closer_time.ok_or(StoreError::Corrupt)?;
        push_recovery_event(
            &mut events,
            &mut next,
            time,
            NewEvent::log(EventKind::turn_end(turn, TurnEndReason::Interrupted)),
        )?;
    }

    let closer_count = u64::try_from(events.len()).map_err(|_| StoreError::Limit)?;
    let resume_seed_len = logical_events
        .checked_add(closer_count)
        .ok_or(StoreError::Limit)?;
    if !ends_with_seed {
        push_recovery_event(
            &mut events,
            &mut next,
            marker_time,
            NewEvent::log(EventKind::EndSeed),
        )?;
    }
    if events.len() > MAX_RECOVERY_ROWS {
        return Err(StoreError::Limit);
    }

    let mut recovered_projection = projection.clone();
    for event in &events {
        recovered_projection
            .apply_recovery_admission(RecoveryAdmission::new(event))
            .map_err(|_| StoreError::Corrupt)?;
    }
    Ok((
        RecoveryPlan {
            events,
            resume_seed_len: if ends_with_seed {
                logical_events
            } else {
                resume_seed_len
            },
            repaired_calls,
            unknown_outcomes,
            not_started,
        },
        recovered_projection,
    ))
}

fn recovery_result_kind(
    call: &super::projection::RecoveryCall,
) -> (&'static str, &'static str, &'static str, Option<EventSeq>) {
    let Some(intent_seq) = call.intent_seq() else {
        return (
            "ToolNotStartedError",
            TOOL_NOT_STARTED,
            TOOL_NOT_STARTED_TEXT,
            None,
        );
    };
    match call.approval() {
        RecoveryApproval::Decided {
            outcome: ApprovalOutcome::Rejected,
            ..
        } => (
            "ApprovalError",
            "APPROVAL_REJECTED",
            APPROVAL_REJECTED_TEXT,
            Some(intent_seq),
        ),
        RecoveryApproval::Pending { .. }
        | RecoveryApproval::Decided {
            outcome: ApprovalOutcome::Cancelled,
            ..
        } => (
            "AbortError",
            "APPROVAL_CANCELLED",
            APPROVAL_CANCELLED_TEXT,
            Some(intent_seq),
        ),
        RecoveryApproval::Decided {
            outcome: ApprovalOutcome::Unavailable,
            ..
        } => (
            "ApprovalError",
            "APPROVAL_UNAVAILABLE",
            APPROVAL_UNAVAILABLE_TEXT,
            Some(intent_seq),
        ),
        RecoveryApproval::None
        | RecoveryApproval::Decided {
            outcome: ApprovalOutcome::AllowedOnce,
            ..
        } => (
            "ToolOutcomeUnknownError",
            TOOL_OUTCOME_UNKNOWN,
            TOOL_OUTCOME_UNKNOWN_TEXT,
            Some(intent_seq),
        ),
    }
}

fn push_recovery_event(
    events: &mut Vec<SessionEvent>,
    next: &mut Option<EventSeq>,
    time: UnixMillis,
    event: NewEvent,
) -> Result<(), StoreError> {
    if events.len() >= MAX_RECOVERY_ROWS {
        return Err(StoreError::Limit);
    }
    let seq = next.ok_or(StoreError::Limit)?;
    let original_data =
        JsonValue::new(kind_data_value(&event.kind).map_err(|_| StoreError::Limit)?)
            .map_err(|_| StoreError::Limit)?;
    events.push(SessionEvent::from_new(seq, time, event, original_data));
    *next = seq
        .get()
        .checked_add(1)
        .and_then(|value| EventSeq::new(value).ok());
    Ok(())
}

fn recovery_message_id(
    session_id: &SessionId,
    last_prefix_seq: Option<EventSeq>,
    call_id: &crate::model::CallId,
    call_seq: Option<EventSeq>,
    result_seq: EventSeq,
) -> Result<String, StoreError> {
    let mut digest = Context::new(&SHA256);
    digest.update(b"dsh.recovery.tool-result.v1\0");
    digest_field(&mut digest, session_id.as_str().as_bytes())?;
    digest.update(
        &last_prefix_seq
            .map_or(u64::MAX, EventSeq::get)
            .to_be_bytes(),
    );
    digest_field(&mut digest, call_id.as_str().as_bytes())?;
    digest.update(&call_seq.map_or(u64::MAX, EventSeq::get).to_be_bytes());
    digest.update(&result_seq.get().to_be_bytes());
    let bytes = digest.finish();
    let mut id = String::from(super::event::RECOVERY_TOOL_RESULT_ID_PREFIX);
    id.try_reserve_exact(bytes.as_ref().len() * 2)
        .map_err(|_| StoreError::Limit)?;
    use std::fmt::Write as _;
    for byte in bytes.as_ref() {
        write!(&mut id, "{byte:02x}").map_err(|_| StoreError::Limit)?;
    }
    Ok(id)
}

fn digest_field(digest: &mut Context, field: &[u8]) -> Result<(), StoreError> {
    let length = u64::try_from(field.len()).map_err(|_| StoreError::Limit)?;
    digest.update(&length.to_be_bytes());
    digest.update(field);
    Ok(())
}

fn call_fingerprint(call_id: &crate::model::CallId) -> Result<String, StoreError> {
    let mut digest = Context::new(&SHA256);
    digest.update(b"dsh.recovery.report.call.v1\0");
    digest_field(&mut digest, call_id.as_str().as_bytes())?;
    let bytes = digest.finish();
    let mut fingerprint = String::from("call-");
    fingerprint
        .try_reserve_exact(24)
        .map_err(|_| StoreError::Limit)?;
    use std::fmt::Write as _;
    for byte in &bytes.as_ref()[..12] {
        write!(&mut fingerprint, "{byte:02x}").map_err(|_| StoreError::Limit)?;
    }
    Ok(fingerprint)
}

/// Scan from byte zero without retaining historical rows.
#[cfg(test)]
pub(crate) fn scan_jsonl(
    reader: impl Read,
    expected_id: &SessionId,
    cancelled: &AtomicBool,
) -> Result<ColdScan, StoreError> {
    scan_jsonl_validating_header(reader, expected_id, cancelled, |_, _| Ok(()))
}

/// Scan after a caller validates the durable workspace facts in the header.
///
/// The callback runs immediately after the bounded header is decoded and
/// before even one body row is parsed. Resume uses that ordering so a wrong
/// workspace cannot make dsh inspect or repair unrelated session content.
pub(crate) fn scan_jsonl_validating_header(
    mut reader: impl Read,
    expected_id: &SessionId,
    cancelled: &AtomicBool,
    mut validate_header: impl FnMut(&SessionHeader, WorkspaceIdentity) -> Result<(), StoreError>,
) -> Result<ColdScan, StoreError> {
    let mut digest = Context::new(&SHA256);
    let mut projection =
        Projection::for_session(ValidationPolicy::DurableStrict, expected_id.clone());
    let mut header = None;
    let mut workspace_identity = None;
    let mut line = Vec::new();
    line.try_reserve_exact(MAX_JOURNAL_HEADER_LINE_BYTES)
        .map_err(|_| StoreError::Limit)?;
    let mut scratch = [0_u8; SCAN_BLOCK_BYTES];
    let mut physical_bytes = 0_u64;
    let mut valid_bytes = 0_u64;
    let mut logical_events = 0_u64;
    let mut complete_body_lines = 0_u64;
    let mut next_seq = EventSeq::from_index(0);
    let mut last_event_time = None;
    let mut ends_with_seed = false;
    let mut recovery_cursor = None;
    let mut bad_suffix = false;

    loop {
        if cancelled.load(Ordering::Acquire) {
            return Err(StoreError::Cancelled);
        }
        let count = reader.read(&mut scratch).map_err(|_| StoreError::Io)?;
        if count == 0 {
            break;
        }
        let next_physical = physical_bytes
            .checked_add(u64::try_from(count).map_err(|_| StoreError::Limit)?)
            .ok_or(StoreError::Limit)?;
        if next_physical > MAX_DURABLE_JOURNAL_BYTES {
            return Err(StoreError::Limit);
        }
        digest.update(&scratch[..count]);
        let block_start = physical_bytes;
        physical_bytes = next_physical;

        let mut start = 0_usize;
        while start < count {
            let tail = &scratch[start..count];
            let newline = tail.iter().position(|byte| *byte == b'\n');
            let end = newline.map_or(count, |index| start + index + 1);
            let segment = &scratch[start..end];
            let maximum = if header.is_none() {
                MAX_JOURNAL_HEADER_LINE_BYTES
            } else {
                MAX_JOURNAL_EVENT_LINE_BYTES
            };
            if line
                .len()
                .checked_add(segment.len())
                .is_none_or(|length| length > maximum)
            {
                return Err(StoreError::Limit);
            }
            line.extend_from_slice(segment);
            if newline.is_some() {
                let line_end = block_start
                    .checked_add(u64::try_from(end).map_err(|_| StoreError::Limit)?)
                    .ok_or(StoreError::Limit)?;
                if header.is_none() {
                    let (decoded, identity) = decode_header_line(&line, expected_id)?;
                    validate_header(&decoded, identity)?;
                    header = Some(decoded);
                    workspace_identity = Some(identity);
                    valid_bytes = line_end;
                    line.try_reserve_exact(
                        MAX_JOURNAL_EVENT_LINE_BYTES.saturating_sub(line.capacity()),
                    )
                    .map_err(|_| StoreError::Limit)?;
                } else {
                    complete_body_lines = complete_body_lines
                        .checked_add(1)
                        .ok_or(StoreError::Limit)?;
                    if complete_body_lines > MAX_DURABLE_LOGICAL_EVENTS {
                        return Err(StoreError::Limit);
                    }
                    if bad_suffix {
                        if shallow_event_envelope(&line) {
                            return Err(StoreError::Corrupt);
                        }
                    } else {
                        let payload = line.strip_suffix(b"\n").ok_or(StoreError::Corrupt)?;
                        let value = match serde_json::from_slice::<Value>(payload) {
                            Ok(value) => value,
                            Err(_) => {
                                bad_suffix = true;
                                line.clear();
                                start = end;
                                continue;
                            }
                        };
                        let index =
                            usize::try_from(logical_events).map_err(|_| StoreError::Limit)?;
                        let event = match decode_event(value, index) {
                            Ok(event) => event,
                            Err(CodecError::UnknownRequiredEvent { .. }) => {
                                return Err(StoreError::Unsupported);
                            }
                            Err(_) => {
                                bad_suffix = true;
                                line.clear();
                                start = end;
                                continue;
                            }
                        };
                        if next_seq != Some(event.seq()) {
                            bad_suffix = true;
                            line.clear();
                            start = end;
                            continue;
                        }
                        apply_cold_event(
                            &mut projection,
                            &event,
                            ColdEventContext {
                                session_id: header.as_ref().ok_or(StoreError::Corrupt)?.id(),
                                expected_seq: next_seq,
                                previous_time: last_event_time,
                                logical_events,
                                previous_ends_with_seed: ends_with_seed,
                            },
                            &mut recovery_cursor,
                        )?;
                        logical_events = logical_events.checked_add(1).ok_or(StoreError::Limit)?;
                        next_seq = event
                            .seq()
                            .get()
                            .checked_add(1)
                            .and_then(|value| EventSeq::new(value).ok());
                        last_event_time = Some(event.time());
                        ends_with_seed = matches!(event.kind(), EventKind::EndSeed);
                        valid_bytes = line_end;
                    }
                }
                if cancelled.load(Ordering::Acquire) {
                    return Err(StoreError::Cancelled);
                }
                line.clear();
            }
            start = end;
        }
    }

    if cancelled.load(Ordering::Acquire) {
        return Err(StoreError::Cancelled);
    }
    let header = header.ok_or(StoreError::Corrupt)?;
    let workspace_identity = workspace_identity.ok_or(StoreError::Corrupt)?;
    #[cfg(not(test))]
    let _ = workspace_identity;
    let digest = digest.finish();
    let mut physical_sha256 = [0_u8; 32];
    physical_sha256.copy_from_slice(digest.as_ref());
    Ok(ColdScan {
        header,
        #[cfg(test)]
        workspace_identity,
        projection,
        physical_bytes,
        valid_bytes,
        logical_events,
        next_seq,
        last_event_time,
        ends_with_seed,
        physical_sha256,
        recovery_cursor,
    })
}

fn apply_cold_event(
    projection: &mut Projection,
    event: &SessionEvent,
    context: ColdEventContext<'_>,
    recovery_cursor: &mut Option<RecoveryCursor>,
) -> Result<(), StoreError> {
    if context.expected_seq != Some(event.seq()) {
        return Err(StoreError::Corrupt);
    }

    if context.previous_ends_with_seed && matches!(event.kind(), EventKind::EndSeed) {
        return Err(StoreError::Corrupt);
    }

    if let Some(cursor) = *recovery_cursor {
        if recovery_event_matches(
            projection,
            context.session_id,
            event,
            context.expected_seq,
            cursor.root_last_real_seq,
            context.previous_time,
            context.logical_events,
        )? {
            projection
                .apply_recovery_admission(RecoveryAdmission::new(event))
                .map_err(|_| StoreError::Corrupt)?;
            if matches!(event.kind(), EventKind::EndSeed) {
                *recovery_cursor = None;
            } else {
                *recovery_cursor = Some(RecoveryCursor {
                    root_last_real_seq: cursor.root_last_real_seq,
                    confirmed: cursor.confirmed || is_recovery_only_event(event),
                });
            }
            return Ok(());
        }
        if cursor.confirmed {
            return Err(StoreError::Corrupt);
        }
        // A synthetic approval decision is also an ordinary durable event, so
        // the cursor is tentative until the next recovery-only row proves that
        // this is a repair suffix. If that next row does not match the original
        // repair anchor, it may only continue through the ordinary admission
        // path. In particular, do not give a reserved recovery result a second
        // chance with `event.seq() - 1` as a fresh anchor: that would let two
        // separately forged repair prefixes be stitched together.
        return match projection.apply_scanned_event(event) {
            Ok(()) => {
                *recovery_cursor = None;
                Ok(())
            }
            Err(_) => Err(StoreError::Corrupt),
        };
    }

    let root_last_real_seq = event
        .seq()
        .get()
        .checked_sub(1)
        .and_then(|value| EventSeq::new(value).ok());
    if is_tentative_recovery_start(event)
        && recovery_event_matches(
            projection,
            context.session_id,
            event,
            context.expected_seq,
            root_last_real_seq,
            context.previous_time,
            context.logical_events,
        )?
    {
        projection
            .apply_scanned_event(event)
            .map_err(|_| StoreError::Corrupt)?;
        *recovery_cursor = Some(RecoveryCursor {
            root_last_real_seq,
            confirmed: false,
        });
        return Ok(());
    }

    match projection.apply_scanned_event(event) {
        Ok(()) => Ok(()),
        Err(EventValidationError::Transition(
            TransitionError::DurableRecoveryEventNotAllowed { .. },
        )) => {
            if !recovery_event_matches(
                projection,
                context.session_id,
                event,
                context.expected_seq,
                root_last_real_seq,
                context.previous_time,
                context.logical_events,
            )? {
                return Err(StoreError::Corrupt);
            }
            projection
                .apply_recovery_admission(RecoveryAdmission::new(event))
                .map_err(|_| StoreError::Corrupt)?;
            if !matches!(event.kind(), EventKind::EndSeed) {
                *recovery_cursor = Some(RecoveryCursor {
                    root_last_real_seq,
                    confirmed: true,
                });
            }
            Ok(())
        }
        Err(_) => Err(StoreError::Corrupt),
    }
}

#[allow(clippy::too_many_arguments)]
fn recovery_event_matches(
    projection: &Projection,
    session_id: &SessionId,
    event: &SessionEvent,
    expected_seq: Option<EventSeq>,
    root_last_real_seq: Option<EventSeq>,
    previous_time: Option<UnixMillis>,
    logical_events: u64,
) -> Result<bool, StoreError> {
    let (plan, _) = build_recovery_plan(
        projection,
        session_id,
        expected_seq,
        root_last_real_seq,
        previous_time,
        event.time(),
        false,
        logical_events,
    )?;
    Ok(plan.events().first() == Some(event))
}

fn is_tentative_recovery_start(event: &SessionEvent) -> bool {
    matches!(
        event.kind(),
        EventKind::ApprovalDecided { decided }
            if decided.outcome() == ApprovalOutcome::Cancelled
    )
}

fn is_recovery_only_event(event: &SessionEvent) -> bool {
    match event.kind() {
        EventKind::EndSeed
        | EventKind::TurnEnd {
            reason: TurnEndReason::Interrupted,
            ..
        } => true,
        EventKind::ToolResult { message, .. } => message
            .id()
            .as_str()
            .starts_with(super::event::RECOVERY_TOOL_RESULT_ID_PREFIX),
        _ => false,
    }
}

fn decode_header_line(
    line: &[u8],
    expected_id: &SessionId,
) -> Result<(SessionHeader, WorkspaceIdentity), StoreError> {
    let payload = line.strip_suffix(b"\n").ok_or(StoreError::Corrupt)?;
    let mut value: Value = serde_json::from_slice(payload).map_err(|_| StoreError::Corrupt)?;
    let fields = value.as_object_mut().ok_or(StoreError::Corrupt)?;
    if fields
        .remove("type")
        .and_then(|value| value.as_str().map(str::to_owned))
        .as_deref()
        != Some("session")
    {
        return Err(StoreError::Corrupt);
    }
    let version = fields
        .get("version")
        .and_then(Value::as_u64)
        .ok_or(StoreError::Corrupt)?;
    if version != SESSION_FORMAT_VERSION {
        return Err(StoreError::Unsupported);
    }
    let header = SessionHeader::from_value(value).map_err(|_| StoreError::Corrupt)?;
    header
        .validate_for(expected_id)
        .map_err(|_| StoreError::Corrupt)?;
    if header.cwd().is_none() {
        return Err(StoreError::Corrupt);
    }
    let raw = header
        .raw()
        .as_value()
        .as_object()
        .ok_or(StoreError::Corrupt)?;
    let delegation_depth = raw
        .get("delegationDepth")
        .and_then(Value::as_u64)
        .filter(|value| *value <= MAX_SAFE_INTEGER)
        .ok_or(StoreError::Corrupt)?;
    let _ = delegation_depth;
    let identity = raw
        .get("rustWorkspaceIdentity")
        .and_then(Value::as_object)
        .ok_or(StoreError::Corrupt)?;
    let device = canonical_hex_u64(
        identity
            .get("device")
            .and_then(Value::as_str)
            .ok_or(StoreError::Corrupt)?,
    )?;
    let inode = canonical_hex_u64(
        identity
            .get("inode")
            .and_then(Value::as_str)
            .ok_or(StoreError::Corrupt)?,
    )?;
    Ok((header, WorkspaceIdentity::from_raw(device, inode)))
}

fn canonical_hex_u64(value: &str) -> Result<u64, StoreError> {
    if value.is_empty() {
        return Err(StoreError::Corrupt);
    }
    let parsed = u64::from_str_radix(value, 16).map_err(|_| StoreError::Corrupt)?;
    if format!("{parsed:x}") != value {
        return Err(StoreError::Corrupt);
    }
    Ok(parsed)
}

fn shallow_event_envelope(line: &[u8]) -> bool {
    let Some(payload) = line.strip_suffix(b"\n") else {
        return false;
    };
    let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(payload) else {
        return false;
    };
    fields.get("type").is_some_and(Value::is_string)
        && fields
            .get("seq")
            .and_then(Value::as_u64)
            .is_some_and(|seq| seq <= MAX_SAFE_INTEGER)
        && fields
            .get("time")
            .and_then(Value::as_i64)
            .is_some_and(|time| {
                let maximum = i64::try_from(MAX_SAFE_INTEGER).unwrap_or(i64::MAX);
                (-maximum..=maximum).contains(&time)
            })
        && fields.contains_key("data")
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, sync::atomic::AtomicBool};

    use serde_json::{Value, json};

    use crate::{
        model::{ContentBlock, Message},
        session::{
            ApprovalAskedEvent, ApprovalOutcome, ApprovalRequestId, Clock, ClockError, EventKind,
            EventSeq, NewEvent, Session, SessionEvent, SessionId, StepId, SurfaceIntent,
            TOOL_NOT_STARTED, TOOL_OUTCOME_UNKNOWN, TurnEndReason, TurnId, UnixMillis,
            jsonl::{encode_event_line, encode_header_line},
        },
        workspace_authority::WorkspaceIdentity,
    };

    use super::{
        MAX_DURABLE_JOURNAL_BYTES, MAX_DURABLE_LOGICAL_EVENTS, RecoveredSeed,
        RecoveryCompactionStage, StoreError, scan_jsonl,
    };
    use crate::session::projection::{Projection, ValidationPolicy};

    const ID: &str = "session-550e8400-e29b-41d4-a716-446655440000";

    #[derive(Clone, Copy)]
    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> Result<UnixMillis, ClockError> {
            UnixMillis::new(7).map_err(|error| ClockError::new(error.to_string()))
        }
    }

    fn header_line() -> Vec<u8> {
        let header = crate::session::SessionHeader::new_durable(
            ID,
            UnixMillis::new(7).unwrap(),
            "/workspace".to_owned(),
            WorkspaceIdentity::new_for_test(0x1a, 0x2b),
        )
        .unwrap();
        encode_header_line(&header).unwrap()
    }

    fn scan_bytes(bytes: &[u8]) -> Result<super::ColdScan, StoreError> {
        scan_jsonl(
            Cursor::new(bytes),
            &SessionId::new(ID),
            &AtomicBool::new(false),
        )
    }

    fn open_tool_tail() -> (Vec<u8>, super::ColdScan) {
        let mut memory = Session::with_clock(ID, FixedClock).unwrap();
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        memory
            .append(NewEvent::log(EventKind::turn_start(turn)))
            .unwrap();
        memory
            .append(NewEvent::log(EventKind::step_start(turn, step)))
            .unwrap();
        let assistant = Message::assistant(
            "assistant",
            vec![
                ContentBlock::tool_call("call-a", "echo", "{\"a\":1}").unwrap(),
                ContentBlock::tool_call("call-b", "echo", "{\"b\":2}").unwrap(),
            ],
            "mock",
            "mock-model",
        )
        .unwrap();
        memory
            .append(NewEvent::surface(
                EventKind::assistant_message(turn, step, assistant),
                SurfaceIntent::append(),
            ))
            .unwrap();
        memory
            .append(NewEvent::log(EventKind::tool_call(
                turn,
                step,
                "call-a",
                "echo",
                "{\"a\":1}",
            )))
            .unwrap();
        let mut bytes = header_line();
        for event in memory.events() {
            bytes.extend_from_slice(&encode_event_line(event).unwrap());
        }
        let scan = scan_bytes(&bytes).unwrap();
        (bytes, scan)
    }

    fn end_seed_line(seq: u64) -> Vec<u8> {
        let mut line = format!(r#"{{"type":"session/end-seed","seq":{seq},"time":7,"data":{{}}}}"#)
            .into_bytes();
        line.push(b'\n');
        line
    }

    fn turn_line(kind: &str, seq: u64, turn: u64) -> Vec<u8> {
        let mut line = format!(
            r#"{{"type":"turn/{kind}","seq":{seq},"time":7,"data":{{"turn":{turn}{}}}}}"#,
            if kind == "end" {
                r#","reason":{"kind":"completed"}"#
            } else {
                ""
            }
        )
        .into_bytes();
        line.push(b'\n');
        line
    }

    fn compaction_wire_event(
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

    fn compaction_orphan_prefix(stage: RecoveryCompactionStage) -> Vec<SessionEvent> {
        let mut events = vec![
            compaction_wire_event("turn/start", 0, json!({ "turn": 1 }), None, None),
            compaction_wire_event(
                "user/message",
                1,
                json!({
                    "id": "old-context",
                    "role": "user",
                    "content": [{ "type": "text", "text": "old context" }],
                    "source": { "kind": "user" }
                }),
                Some(json!("append")),
                None,
            ),
            compaction_wire_event(
                "turn/end",
                2,
                json!({ "turn": 1, "reason": { "kind": "completed" } }),
                None,
                None,
            ),
            compaction_wire_event("turn/start", 3, json!({ "turn": 2 }), None, None),
            compaction_wire_event(
                "compaction/start",
                4,
                json!({
                    "compactionId": "resume-compaction",
                    "turn": 2,
                    "dispatch": {
                        "trigger": "pressure",
                        "sourceSurfaceGeneration": 1,
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
                        "sessionId": ID,
                        "purpose": "compaction",
                        "instruction": {
                            "id": "summary-instruction",
                            "role": "user",
                            "content": [{ "type": "text", "text": "Summarize." }],
                            "source": { "kind": "plugin", "plugin": "dsh.compaction" }
                        },
                        "instructionFormatVersion": 1
                    }
                }),
                None,
                None,
            ),
        ];
        if matches!(
            stage,
            RecoveryCompactionStage::Summarized | RecoveryCompactionStage::Replaced
        ) {
            events.push(compaction_wire_event(
                "compaction/summary",
                5,
                json!({
                    "compactionId": "resume-compaction",
                    "summary": [{ "type": "text", "text": "summary", "kept": true }],
                    "rawOutput": [
                        { "type": "reasoning", "text": "private" },
                        { "type": "text", "text": "summary", "kept": true }
                    ],
                    "llmStreamCall": true,
                    "shadowedRange": { "start": 1, "end": 1 },
                    "shadowedSeqs": [1],
                    "shadowedTokenCount": 2,
                    "provider": "summary-provider",
                    "model": "summary-model"
                }),
                None,
                None,
            ));
        }
        if stage == RecoveryCompactionStage::Replaced {
            events.push(compaction_wire_event(
                "user/message",
                6,
                json!({
                    "id": "checkpoint",
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.\n\n<compacted-summary>"
                        },
                        { "type": "text", "text": "summary", "kept": true },
                        { "type": "text", "text": "</compacted-summary>" }
                    ],
                    "source": {
                        "kind": "plugin",
                        "plugin": "compact",
                        "compactionId": "resume-compaction"
                    }
                }),
                Some(json!({ "op": "replace", "start": 1, "end": 1 })),
                Some(vec![4, 5, 1]),
            ));
        }
        events
    }

    fn context_overflow_compaction_prefix(stage: RecoveryCompactionStage) -> Vec<SessionEvent> {
        let pressure = compaction_orphan_prefix(stage);
        let mut events = pressure[..4].to_vec();
        events.push(compaction_wire_event(
            "step/start",
            4,
            json!({ "turn": 2, "step": 1 }),
            None,
            None,
        ));
        for event in &pressure[4..] {
            let mut value = serde_json::to_value(event).unwrap();
            let shifted = event.seq().get() + 1;
            value["seq"] = json!(shifted);
            if matches!(event.kind(), EventKind::CompactionStart { .. }) {
                value["data"]["dispatch"]["trigger"] = json!("context-overflow");
            }
            if matches!(event.kind(), EventKind::UserMessage { .. }) {
                value["sourceEventSeqs"] = json!([5, 6, 1]);
            }
            events.push(crate::session::codec::decode_event(value, shifted as usize).unwrap());
        }
        events
    }

    fn orphan_prune_prefix() -> Vec<SessionEvent> {
        vec![
            compaction_wire_event("turn/start", 0, json!({ "turn": 1 }), None, None),
            compaction_wire_event("step/start", 1, json!({ "turn": 1, "step": 1 }), None, None),
            compaction_wire_event(
                "assistant/message",
                2,
                json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "id": "assistant-prune",
                        "role": "assistant",
                        "content": [{
                            "type": "tool-call",
                            "id": "call-prune",
                            "name": "read",
                            "arguments": "{}"
                        }],
                        "source": {
                            "kind": "model",
                            "provider": "mock",
                            "model": "mock-model"
                        }
                    }
                }),
                Some(json!("append")),
                None,
            ),
            compaction_wire_event(
                "tool/call",
                3,
                json!({
                    "turn": 1,
                    "step": 1,
                    "callId": "call-prune",
                    "name": "read",
                    "arguments": "{}"
                }),
                None,
                None,
            ),
            compaction_wire_event(
                "tool/result",
                4,
                json!({
                    "turn": 1,
                    "step": 1,
                    "message": {
                        "id": "result-prune",
                        "role": "user",
                        "content": [{
                            "type": "tool-result",
                            "toolCallId": "call-prune",
                            "content": [{ "type": "text", "text": "large result" }],
                            "isError": false
                        }],
                        "source": { "kind": "tool", "callId": "call-prune" }
                    },
                    "meta": { "retained": true }
                }),
                Some(json!("append")),
                Some(vec![3]),
            ),
            compaction_wire_event(
                "compaction/prune",
                5,
                json!({
                    "shadowedRange": { "start": 4, "end": 4 },
                    "shadowedSeqs": [4],
                    "shadowedTokenCount": 12
                }),
                None,
                None,
            ),
        ]
    }

    #[test]
    fn scanner_folds_more_than_the_memory_event_limit_without_history() {
        let mut bytes = header_line();
        for turn in 1..=2_500 {
            let start_seq = (turn - 1) * 2;
            bytes.extend_from_slice(&turn_line("start", start_seq, turn));
            bytes.extend_from_slice(&turn_line("end", start_seq + 1, turn));
        }

        let scan = scan_bytes(&bytes).unwrap();
        assert_eq!(scan.logical_events(), 5_000);
        assert_eq!(scan.next_seq().unwrap().get(), 5_000);
        assert_eq!(scan.valid_bytes(), bytes.len() as u64);
        assert_eq!(scan.truncated_bytes(), 0);
        assert!(!scan.ends_with_seed());
        assert_eq!(scan.projection().state().open_turn(), None);
        assert_eq!(scan.header().id().as_str(), ID);
        assert_eq!(scan.workspace_identity().device(), 0x1a);
        assert_eq!(scan.workspace_identity().inode(), 0x2b);
        assert_eq!(scan.last_event_time(), Some(UnixMillis::new(7).unwrap()));
        assert_ne!(scan.physical_sha256(), &[0_u8; 32]);
    }

    #[test]
    fn cold_recovery_seals_each_legal_compaction_orphan_without_fabricating_an_end() {
        for stage in [
            RecoveryCompactionStage::Started,
            RecoveryCompactionStage::Summarized,
            RecoveryCompactionStage::Replaced,
        ] {
            let prefix = compaction_orphan_prefix(stage);
            let mut bytes = header_line();
            for event in &prefix {
                bytes.extend_from_slice(&encode_event_line(event).unwrap());
            }
            let scan = scan_bytes(&bytes).unwrap();
            let (plan, projection) = scan
                .prepare_recovery(UnixMillis::new(999).unwrap())
                .unwrap();
            let report = scan.recovery_report(&plan).unwrap();

            assert_eq!(report.interrupted_compaction(), Some(stage));
            assert!(report.needs_warning());
            assert_eq!(plan.events().len(), 2);
            assert!(matches!(
                plan.events()[0].kind(),
                EventKind::TurnEnd {
                    reason: TurnEndReason::Interrupted,
                    ..
                }
            ));
            assert!(matches!(plan.events()[1].kind(), EventKind::EndSeed));
            assert!(
                plan.events()
                    .iter()
                    .all(|event| !matches!(event.kind(), EventKind::CompactionEnd { .. }))
            );
            assert_eq!(projection.interrupted_compaction_stage(), None);
            assert_eq!(projection.state().open_turn(), None);
            let expected_surface = if stage == RecoveryCompactionStage::Replaced {
                EventSeq::new(6).unwrap()
            } else {
                EventSeq::new(1).unwrap()
            };
            assert_eq!(projection.state().surface_nodes(), &[expected_surface]);

            for event in plan.events() {
                bytes.extend_from_slice(&encode_event_line(event).unwrap());
            }
            let sealed = scan_bytes(&bytes).unwrap();
            let (empty, sealed_projection) = sealed
                .prepare_recovery(UnixMillis::new(1_001).unwrap())
                .unwrap();
            assert!(empty.events().is_empty());
            assert_eq!(sealed_projection.interrupted_compaction_stage(), None);
            assert!(!sealed.recovery_report(&empty).unwrap().needs_warning());
        }
    }

    #[test]
    fn context_overflow_orphan_closes_its_step_and_resumes_a_partial_repair() {
        for stage in [
            RecoveryCompactionStage::Started,
            RecoveryCompactionStage::Summarized,
            RecoveryCompactionStage::Replaced,
        ] {
            let mut bytes = header_line();
            for event in context_overflow_compaction_prefix(stage) {
                bytes.extend_from_slice(&encode_event_line(&event).unwrap());
            }
            let scan = scan_bytes(&bytes).unwrap();
            let (plan, projection) = scan
                .prepare_recovery(UnixMillis::new(998).unwrap())
                .unwrap();
            assert_eq!(
                scan.recovery_report(&plan)
                    .unwrap()
                    .interrupted_compaction(),
                Some(stage)
            );
            assert_eq!(plan.events().len(), 3);
            let expected_surface = if stage == RecoveryCompactionStage::Replaced {
                EventSeq::new(7).unwrap()
            } else {
                EventSeq::new(1).unwrap()
            };
            assert_eq!(projection.state().surface_nodes(), &[expected_surface]);
        }

        let prefix = context_overflow_compaction_prefix(RecoveryCompactionStage::Started);
        let mut bytes = header_line();
        for event in &prefix {
            bytes.extend_from_slice(&encode_event_line(event).unwrap());
        }
        let scan = scan_bytes(&bytes).unwrap();
        let (plan, _) = scan
            .prepare_recovery(UnixMillis::new(999).unwrap())
            .unwrap();
        let report = scan.recovery_report(&plan).unwrap();
        assert_eq!(
            report.interrupted_compaction(),
            Some(RecoveryCompactionStage::Started)
        );
        assert!(report.closes_step());
        assert!(report.closes_turn());
        assert_eq!(plan.events().len(), 3);
        assert!(matches!(plan.events()[0].kind(), EventKind::StepEnd { .. }));
        assert!(matches!(
            plan.events()[1].kind(),
            EventKind::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        assert!(matches!(plan.events()[2].kind(), EventKind::EndSeed));
        assert!(matches!(
            scan.projection().with_event(&plan.events()[0]).unwrap_err(),
            crate::session::EventValidationError::Transition(
                crate::session::TransitionError::DurableRecoveryEventNotAllowed {
                    event_type: "step/end"
                }
            )
        ));

        // Simulate a crash after only the first repair row. The next scan must
        // continue the same private recovery batch instead of inventing a new one.
        bytes.extend_from_slice(&encode_event_line(&plan.events()[0]).unwrap());
        let partial = scan_bytes(&bytes).unwrap();
        let (remaining, projection) = partial
            .prepare_recovery(UnixMillis::new(1_001).unwrap())
            .unwrap();
        assert_eq!(remaining.events().len(), 2);
        assert_eq!(remaining.events()[0], plan.events()[1]);
        assert!(matches!(remaining.events()[1].kind(), EventKind::EndSeed));
        assert_eq!(remaining.events()[1].seq(), plan.events()[2].seq());
        assert_eq!(
            remaining.events()[1].time(),
            UnixMillis::new(1_001).unwrap()
        );
        assert_eq!(projection.interrupted_compaction_stage(), None);
        assert!(matches!(
            partial
                .projection()
                .with_event(&remaining.events()[0])
                .unwrap_err(),
            crate::session::EventValidationError::Transition(
                crate::session::TransitionError::DurableRecoveryEventNotAllowed {
                    event_type: "turn/end"
                }
            )
        ));

        bytes.extend_from_slice(&encode_event_line(&remaining.events()[0]).unwrap());
        let after_turn = scan_bytes(&bytes).unwrap();
        let (seed_only, _) = after_turn
            .prepare_recovery(UnixMillis::new(1_002).unwrap())
            .unwrap();
        assert_eq!(seed_only.events().len(), 1);
        assert!(matches!(seed_only.events()[0].kind(), EventKind::EndSeed));
        bytes.extend_from_slice(&encode_event_line(&seed_only.events()[0]).unwrap());
        let sealed = scan_bytes(&bytes).unwrap();
        let (empty, _) = sealed
            .prepare_recovery(UnixMillis::new(1_003).unwrap())
            .unwrap();
        assert!(empty.events().is_empty());
    }

    #[test]
    fn cold_recovery_reports_and_clears_an_orphan_prune_marker() {
        let mut bytes = header_line();
        for event in orphan_prune_prefix() {
            bytes.extend_from_slice(&encode_event_line(&event).unwrap());
        }
        let scan = scan_bytes(&bytes).unwrap();
        let (plan, projection) = scan
            .prepare_recovery(UnixMillis::new(999).unwrap())
            .unwrap();
        let report = scan.recovery_report(&plan).unwrap();
        assert_eq!(report.orphan_prune_markers(), 1);
        assert!(report.needs_warning());
        assert_eq!(projection.orphan_prune_markers(), 0);
        assert_eq!(
            projection.state().surface_nodes(),
            &[EventSeq::new(2).unwrap(), EventSeq::new(4).unwrap()]
        );
        assert!(plan.events().iter().all(|event| !matches!(
            event.kind(),
            EventKind::ToolResult { .. } | EventKind::CompactionPrune { .. }
        )));

        for event in plan.events() {
            bytes.extend_from_slice(&encode_event_line(event).unwrap());
        }
        let sealed = scan_bytes(&bytes).unwrap();
        let (empty, _) = sealed
            .prepare_recovery(UnixMillis::new(1_001).unwrap())
            .unwrap();
        assert!(empty.events().is_empty());
        assert!(!sealed.recovery_report(&empty).unwrap().needs_warning());
    }

    #[test]
    fn cold_scan_rejects_a_complete_nonadjacent_compaction_body() {
        let mut bytes = header_line();
        for event in compaction_orphan_prefix(RecoveryCompactionStage::Started) {
            bytes.extend_from_slice(&encode_event_line(&event).unwrap());
        }
        let unrelated = compaction_wire_event("todo/write", 5, json!({ "todos": [] }), None, None);
        bytes.extend_from_slice(&encode_event_line(&unrelated).unwrap());

        assert!(matches!(scan_bytes(&bytes), Err(StoreError::Corrupt)));
    }

    #[test]
    fn recovered_seed_distinguishes_resource_limits_from_structural_corruption() {
        let header = crate::session::SessionHeader::new_durable(
            ID,
            UnixMillis::new(7).unwrap(),
            "/workspace".to_owned(),
            WorkspaceIdentity::new_for_test(0x1a, 0x2b),
        )
        .unwrap();
        let projection = || Projection::empty(ValidationPolicy::DurableStrict);

        assert!(matches!(
            RecoveredSeed::new(
                header.clone(),
                projection(),
                EventSeq::new(MAX_DURABLE_LOGICAL_EVENTS + 1).ok(),
                0,
                MAX_DURABLE_LOGICAL_EVENTS + 1,
                0,
            ),
            Err(StoreError::Limit)
        ));
        assert!(matches!(
            RecoveredSeed::new(
                header.clone(),
                projection(),
                EventSeq::new(0).ok(),
                0,
                0,
                MAX_DURABLE_JOURNAL_BYTES + 1,
            ),
            Err(StoreError::Limit)
        ));
        assert!(matches!(
            RecoveredSeed::new(header.clone(), projection(), EventSeq::new(1).ok(), 0, 0, 0,),
            Err(StoreError::Corrupt)
        ));
        assert!(matches!(
            RecoveredSeed::new(header, projection(), EventSeq::new(0).ok(), 1, 0, 0,),
            Err(StoreError::Corrupt)
        ));
    }

    #[test]
    fn torn_or_unverifiable_final_suffix_is_reported_without_mutation() {
        let mut torn = header_line();
        torn.extend_from_slice(&end_seed_line(0));
        let valid_bytes = torn.len() as u64;
        torn.extend_from_slice(b"{\"type\":\"turn/start\"");
        let scan = scan_bytes(&torn).unwrap();
        assert_eq!(scan.valid_bytes(), valid_bytes);
        assert_eq!(scan.truncated_bytes(), torn.len() as u64 - valid_bytes);

        let mut bad_line = header_line();
        bad_line.extend_from_slice(&end_seed_line(0));
        let valid_bytes = bad_line.len() as u64;
        bad_line.extend_from_slice(b"not-json\n");
        let scan = scan_bytes(&bad_line).unwrap();
        assert_eq!(scan.valid_bytes(), valid_bytes);
        assert_eq!(scan.truncated_bytes(), 9);
    }

    #[test]
    fn a_valid_envelope_after_a_bad_row_is_interior_corruption() {
        let mut bytes = header_line();
        bytes.extend_from_slice(&end_seed_line(0));
        bytes.extend_from_slice(b"not-json\n");
        bytes.extend_from_slice(&end_seed_line(1));
        assert!(matches!(scan_bytes(&bytes), Err(StoreError::Corrupt)));
    }

    #[test]
    fn a_final_sequence_gap_is_recoverable_but_a_later_envelope_is_not() {
        let mut final_gap = header_line();
        final_gap.extend_from_slice(&end_seed_line(0));
        let valid_bytes = final_gap.len() as u64;
        final_gap.extend_from_slice(&end_seed_line(2));
        let scan = scan_bytes(&final_gap).unwrap();
        assert_eq!(scan.valid_bytes(), valid_bytes);

        final_gap.extend_from_slice(&end_seed_line(3));
        assert!(matches!(scan_bytes(&final_gap), Err(StoreError::Corrupt)));
    }

    #[test]
    fn complete_durable_semantic_damage_is_never_treated_as_tail() {
        let mut memory = Session::with_clock("strict-source", FixedClock).unwrap();
        memory
            .append(NewEvent::log(EventKind::turn_start(
                TurnId::new(1).unwrap(),
            )))
            .unwrap();
        memory
            .append(NewEvent::log(EventKind::step_start(
                TurnId::new(1).unwrap(),
                StepId::new(1).unwrap(),
            )))
            .unwrap();
        let assistant = Message::assistant(
            "assistant",
            vec![ContentBlock::tool_call("call-1", "echo", "{}").unwrap()],
            "mock",
            "mock-model",
        )
        .unwrap();
        memory
            .append(NewEvent::surface(
                EventKind::assistant_message(
                    TurnId::new(1).unwrap(),
                    StepId::new(1).unwrap(),
                    assistant,
                ),
                SurfaceIntent::append(),
            ))
            .unwrap();
        memory
            .append(NewEvent::log(EventKind::step_end(
                TurnId::new(1).unwrap(),
                StepId::new(1).unwrap(),
            )))
            .unwrap();

        let mut bytes = header_line();
        for event in memory.events() {
            bytes.extend_from_slice(&encode_event_line(event).unwrap());
        }
        assert!(matches!(scan_bytes(&bytes), Err(StoreError::Corrupt)));
    }

    #[test]
    fn unknown_required_event_is_unsupported_and_pre_cancel_is_owned() {
        let mut bytes = header_line();
        bytes.extend_from_slice(
            b"{\"type\":\"future/required\",\"seq\":0,\"time\":7,\"data\":{}}\n",
        );
        assert!(matches!(scan_bytes(&bytes), Err(StoreError::Unsupported)));

        assert!(matches!(
            scan_jsonl(
                Cursor::new(&bytes),
                &SessionId::new(ID),
                &AtomicBool::new(true),
            ),
            Err(StoreError::Cancelled)
        ));
    }

    #[test]
    fn physical_length_and_digest_cover_an_uncommitted_tail() {
        let clean = header_line();
        let clean_scan = scan_bytes(&clean).unwrap();
        let mut torn = clean.clone();
        torn.extend_from_slice(b"torn");
        let torn_scan = scan_bytes(&torn).unwrap();
        assert_eq!(torn_scan.physical_bytes(), torn.len() as u64);
        assert_eq!(torn_scan.valid_bytes(), clean.len() as u64);
        assert_ne!(clean_scan.physical_sha256(), torn_scan.physical_sha256());
    }

    #[test]
    fn repair_plan_closes_tools_in_model_order_and_marks_the_seed_once() {
        let (_bytes, scan) = open_tool_tail();
        let marker_time = UnixMillis::new(999).unwrap();
        let (plan, projection) = scan.prepare_recovery(marker_time).unwrap();

        assert_eq!(plan.events().len(), 5);
        assert_eq!(plan.resume_seed_len(), 8);
        assert_eq!(plan.repaired_calls(), 2);
        assert_eq!(plan.unknown_outcomes(), 1);
        assert_eq!(plan.not_started(), 1);
        assert_eq!(plan.events()[0].seq().get(), 4);
        assert_eq!(plan.events()[4].seq().get(), 8);
        assert!(
            plan.events()[..4]
                .iter()
                .all(|event| event.time().get() == 7)
        );
        assert_eq!(plan.events()[4].time(), marker_time);

        let EventKind::ToolResult {
            message,
            error: Some(error),
            ..
        } = plan.events()[0].kind()
        else {
            panic!("first recovery event must be a tool result");
        };
        assert_eq!(message.validate_tool_result().unwrap().as_str(), "call-a");
        assert!(
            message
                .id()
                .as_str()
                .starts_with("dsh-recovery-tool-result-v1-")
        );
        assert_eq!(error.code, TOOL_OUTCOME_UNKNOWN);
        assert_eq!(plan.events()[0].source_event_seqs().unwrap()[0].get(), 3);

        let EventKind::ToolResult {
            message,
            error: Some(error),
            ..
        } = plan.events()[1].kind()
        else {
            panic!("second recovery event must be a tool result");
        };
        assert_eq!(message.validate_tool_result().unwrap().as_str(), "call-b");
        assert_eq!(error.code, TOOL_NOT_STARTED);
        assert_eq!(plan.events()[1].source_event_seqs(), None);
        assert!(matches!(plan.events()[2].kind(), EventKind::StepEnd { .. }));
        assert!(matches!(
            plan.events()[3].kind(),
            EventKind::TurnEnd {
                reason: TurnEndReason::Interrupted,
                ..
            }
        ));
        assert!(matches!(plan.events()[4].kind(), EventKind::EndSeed));
        assert_eq!(projection.state().open_turn(), None);

        let (same, _) = scan.prepare_recovery(marker_time).unwrap();
        assert_eq!(plan, same);
    }

    #[test]
    fn scanner_accepts_only_the_exact_written_repair_prefix() {
        let (mut bytes, scan) = open_tool_tail();
        let (plan, _) = scan
            .prepare_recovery(UnixMillis::new(999).unwrap())
            .unwrap();
        bytes.extend_from_slice(&encode_event_line(&plan.events()[0]).unwrap());
        bytes.extend_from_slice(&encode_event_line(&plan.events()[1]).unwrap());
        let partial = scan_bytes(&bytes).unwrap();
        let (missing, _) = partial
            .prepare_recovery(UnixMillis::new(1_001).unwrap())
            .unwrap();
        assert!(matches!(
            missing.events()[0].kind(),
            EventKind::StepEnd { .. }
        ));
        assert_eq!(missing.events().len(), 3);

        let mut tampered = header_line();
        let prefix_len = scan.valid_bytes() as usize - header_line().len();
        tampered.extend_from_slice(&bytes[header_line().len()..header_line().len() + prefix_len]);
        tampered.extend_from_slice(&encode_event_line(&plan.events()[0]).unwrap());
        let line = encode_event_line(&plan.events()[1]).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(line.strip_suffix(b"\n").unwrap()).unwrap();
        value["data"]["message"]["id"] = serde_json::Value::String("tampered".to_owned());
        tampered.extend_from_slice(&serde_json::to_vec(&value).unwrap());
        tampered.push(b'\n');
        assert!(matches!(scan_bytes(&tampered), Err(StoreError::Corrupt)));
    }

    #[test]
    fn partial_repair_keeps_one_anchor_and_rejects_a_stitched_batch() {
        let (mut bytes, scan) = open_tool_tail();
        let (full, _) = scan
            .prepare_recovery(UnixMillis::new(999).unwrap())
            .unwrap();
        let EventKind::ToolResult {
            message: original_second,
            ..
        } = full.events()[1].kind()
        else {
            panic!("the second repair row must be a tool result");
        };
        bytes.extend_from_slice(&encode_event_line(&full.events()[0]).unwrap());

        let partial = scan_bytes(&bytes).unwrap();
        let (remaining, _) = partial
            .prepare_recovery(UnixMillis::new(1_001).unwrap())
            .unwrap();
        let EventKind::ToolResult {
            message: resumed_second,
            ..
        } = remaining.events()[0].kind()
        else {
            panic!("the resumed first row must be a tool result");
        };
        assert_eq!(resumed_second.id(), original_second.id());

        let wrong_id = super::recovery_message_id(
            &SessionId::new(ID),
            Some(full.events()[0].seq()),
            resumed_second.validate_tool_result().unwrap(),
            None,
            remaining.events()[0].seq(),
        )
        .unwrap();
        let line = encode_event_line(&remaining.events()[0]).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(line.strip_suffix(b"\n").unwrap()).unwrap();
        value["data"]["message"]["id"] = serde_json::Value::String(wrong_id);
        let mut stitched = bytes;
        stitched.extend_from_slice(&serde_json::to_vec(&value).unwrap());
        stitched.push(b'\n');
        assert!(matches!(scan_bytes(&stitched), Err(StoreError::Corrupt)));
    }

    #[test]
    fn a_confirmed_repair_cursor_rejects_an_inserted_ordinary_event() {
        let (mut bytes, scan) = open_tool_tail();
        let (plan, _) = scan
            .prepare_recovery(UnixMillis::new(999).unwrap())
            .unwrap();
        bytes.extend_from_slice(&encode_event_line(&plan.events()[0]).unwrap());
        bytes.extend_from_slice(
            b"{\"type\":\"todo/write\",\"seq\":5,\"time\":7,\"data\":{\"todos\":[]}}\n",
        );
        assert!(matches!(scan_bytes(&bytes), Err(StoreError::Corrupt)));
    }

    #[test]
    fn pending_approval_is_cancelled_before_its_recovery_result() {
        let mut memory = Session::with_clock(ID, FixedClock).unwrap();
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        memory
            .append(NewEvent::log(EventKind::turn_start(turn)))
            .unwrap();
        memory
            .append(NewEvent::log(EventKind::step_start(turn, step)))
            .unwrap();
        memory
            .append(NewEvent::surface(
                EventKind::assistant_message(
                    turn,
                    step,
                    Message::assistant(
                        "assistant",
                        vec![ContentBlock::tool_call("call-a", "echo", "{}").unwrap()],
                        "mock",
                        "mock-model",
                    )
                    .unwrap(),
                ),
                SurfaceIntent::append(),
            ))
            .unwrap();
        memory
            .append(NewEvent::log(EventKind::tool_call(
                turn, step, "call-a", "echo", "{}",
            )))
            .unwrap();
        memory
            .append(NewEvent::log(EventKind::approval_asked(
                ApprovalAskedEvent::new(
                    ApprovalRequestId::new("approval-a"),
                    "echo",
                    Some("call-a".into()),
                    None,
                )
                .unwrap(),
            )))
            .unwrap();
        let mut bytes = header_line();
        for event in memory.events() {
            bytes.extend_from_slice(&encode_event_line(event).unwrap());
        }
        let scan = scan_bytes(&bytes).unwrap();
        let (plan, _) = scan
            .prepare_recovery(UnixMillis::new(999).unwrap())
            .unwrap();
        assert!(matches!(
            plan.events()[0].kind(),
            EventKind::ApprovalDecided { decided }
                if decided.outcome() == ApprovalOutcome::Cancelled
        ));
        assert!(matches!(
            plan.events()[1].kind(),
            EventKind::ToolResult { error: Some(error), .. }
                if error.code == "APPROVAL_CANCELLED"
        ));

        let mut partial_bytes = bytes;
        partial_bytes.extend_from_slice(&encode_event_line(&plan.events()[0]).unwrap());
        let partial = scan_bytes(&partial_bytes).unwrap();
        let (remaining, _) = partial
            .prepare_recovery(UnixMillis::new(1_001).unwrap())
            .unwrap();
        let EventKind::ToolResult {
            message: original, ..
        } = plan.events()[1].kind()
        else {
            panic!("the original second row must be a tool result");
        };
        let EventKind::ToolResult {
            message: resumed, ..
        } = remaining.events()[0].kind()
        else {
            panic!("the resumed first row must be a tool result");
        };
        assert_eq!(resumed.id(), original.id());

        let call_seq = remaining.events()[0]
            .source_event_seqs()
            .and_then(|sources| sources.first())
            .copied();
        let stitched_id = super::recovery_message_id(
            &SessionId::new(ID),
            Some(plan.events()[0].seq()),
            resumed.validate_tool_result().unwrap(),
            call_seq,
            remaining.events()[0].seq(),
        )
        .unwrap();
        let line = encode_event_line(&remaining.events()[0]).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(line.strip_suffix(b"\n").unwrap()).unwrap();
        value["data"]["message"]["id"] = serde_json::Value::String(stitched_id);
        let mut stitched = partial_bytes;
        stitched.extend_from_slice(&serde_json::to_vec(&value).unwrap());
        stitched.push(b'\n');
        assert!(matches!(scan_bytes(&stitched), Err(StoreError::Corrupt)));
    }
}
