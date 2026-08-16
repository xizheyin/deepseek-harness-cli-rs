use std::{fmt, fmt::Write as _};

use thiserror::Error;

use crate::session::{
    ApprovalOutcome, CommittedUiEvent, CommittedUiKind, EventSeq, SourceSeqBitmap,
    UiAssistantBlockKind, UiAssistantContent, UiTurnEndReason,
};

use super::render::VisibleRenderer;

const MAX_ATTEMPT_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTEMPT_BLOCKS: usize = 128;
const FRAME_SOURCE_CHUNK_BYTES: usize = 512;
pub(super) const FRAME_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;

const ASSISTANT_ROLE: &str = "assistant | ";
const REASONING_ROLE: &str = "reasoning | ";
const TOOL_ROLE: &str = "tool | ";
const ARGUMENTS_ROLE: &str = "arguments | ";
const CALL_ROLE: &str = "call | ";
const REASON_ROLE: &str = "reason | ";
const PREVIEW_ROLE: &str = "preview | ";
const APPROVAL_ROLE: &str = "dsh approval > ";
const DSH_ROLE: &str = "dsh | ";
const ERROR_ROLE: &str = "error | ";

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("CLI_OUTPUT_FAILED")]
pub(super) struct LiveRenderError;

#[derive(Debug)]
pub(super) enum LiveLifecycle {
    None,
    ApprovalAsked {
        id: String,
        tool_name: String,
        call_id: Option<String>,
        reason: Option<String>,
    },
    ApprovalDecided {
        id: String,
        outcome: ApprovalOutcome,
    },
    TurnEnded {
        turn: crate::session::TurnId,
    },
}

#[derive(Debug)]
pub(super) struct LiveUpdate {
    pub(super) frame: Option<LiveFrame>,
    pub(super) lifecycle: LiveLifecycle,
}

pub(super) struct LiveFrame {
    parts: Vec<LivePart>,
}

impl fmt::Debug for LiveFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveFrame")
            .field("part_count", &self.parts.len())
            .finish()
    }
}

#[derive(Debug)]
enum LivePart {
    TrustedLine(&'static str),
    TrustedInline(&'static str),
    Untrusted { role: &'static str, text: String },
}

impl LiveFrame {
    fn from_parts(parts: Vec<LivePart>) -> Option<Self> {
        (!parts.is_empty()).then_some(Self { parts })
    }

    fn trusted(value: &'static str) -> Result<Self, LiveRenderError> {
        let mut parts = try_parts(1)?;
        parts.push(LivePart::TrustedLine(value));
        Ok(Self { parts })
    }

    pub(super) fn into_pending(self) -> Result<PendingLiveFrame, LiveRenderError> {
        PendingLiveFrame::new(self)
    }

    pub(super) fn idle_prompt() -> Result<Self, LiveRenderError> {
        Self::trusted("dsh > ")
    }

    pub(super) fn startup_banner(session_id: &str, resumed: bool) -> Result<Self, LiveRenderError> {
        let state = if resumed { "resumed" } else { "new" };
        let mut text = String::new();
        text.try_reserve_exact(96).map_err(|_| LiveRenderError)?;
        writeln!(&mut text, "interactive; {state} session {session_id}")
            .map_err(|_| LiveRenderError)?;
        let mut parts = try_parts(1)?;
        parts.push(LivePart::Untrusted {
            role: DSH_ROLE,
            text,
        });
        Ok(Self { parts })
    }

    pub(super) fn help() -> Result<Self, LiveRenderError> {
        Self::trusted("[commands]\n/help  show this help\n/exit  exit dsh\n/quit  exit dsh\n")
    }

    pub(super) fn notice(value: &'static str) -> Result<Self, LiveRenderError> {
        Self::trusted(value)
    }

    pub(super) fn stopped(skipped: usize) -> Result<Self, LiveRenderError> {
        let mut text = String::new();
        text.try_reserve_exact(64).map_err(|_| LiveRenderError)?;
        writeln!(&mut text, "stopped; skipped {skipped} updates").map_err(|_| LiveRenderError)?;
        let mut parts = try_parts(1)?;
        parts.push(LivePart::Untrusted {
            role: DSH_ROLE,
            text,
        });
        Ok(Self { parts })
    }

    pub(super) fn approval(
        tool_name: &str,
        call_id: Option<&str>,
        reason: Option<&str>,
        preview: &str,
        retry: bool,
    ) -> Result<Self, LiveRenderError> {
        let mut parts = try_parts(12)?;
        parts.push(LivePart::TrustedLine(if retry {
            "[approval answer not recognized]\n"
        } else {
            "[approval requested]\n"
        }));
        push_untrusted_line(&mut parts, TOOL_ROLE, tool_name)?;
        if let Some(call_id) = call_id {
            push_untrusted_line(&mut parts, CALL_ROLE, call_id)?;
        }
        if let Some(reason) = reason {
            push_untrusted_line(&mut parts, REASON_ROLE, reason)?;
        }
        push_untrusted_line(&mut parts, PREVIEW_ROLE, preview)?;
        Ok(Self { parts })
    }

    pub(super) fn approval_ready(challenge: uuid::Uuid) -> Result<Self, LiveRenderError> {
        let mut answer = String::new();
        answer.try_reserve_exact(96).map_err(|_| LiveRenderError)?;
        writeln!(&mut answer, "allow {challenge} | reject | cancel")
            .map_err(|_| LiveRenderError)?;
        let mut parts = try_parts(2)?;
        parts.push(LivePart::TrustedLine("[approval input ready]\n"));
        parts.push(LivePart::Untrusted {
            role: APPROVAL_ROLE,
            text: answer,
        });
        Ok(Self { parts })
    }
}

pub(super) struct PendingLiveFrame {
    frame: LiveFrame,
    part_index: usize,
    text_offset: usize,
    output: String,
    written: usize,
}

impl PendingLiveFrame {
    fn new(frame: LiveFrame) -> Result<Self, LiveRenderError> {
        let mut output = String::new();
        output
            .try_reserve_exact(FRAME_OUTPUT_CHUNK_BYTES)
            .map_err(|_| LiveRenderError)?;
        Ok(Self {
            frame,
            part_index: 0,
            text_offset: 0,
            output,
            written: 0,
        })
    }

    pub(super) fn bytes(&self) -> &[u8] {
        &self.output.as_bytes()[self.written..]
    }

    pub(super) fn advance(&mut self, count: usize) -> Result<(), LiveRenderError> {
        self.written = self.written.checked_add(count).ok_or(LiveRenderError)?;
        if self.written > self.output.len() {
            return Err(LiveRenderError);
        }
        Ok(())
    }

    pub(super) fn prepare_next(
        &mut self,
        presenter: &mut InteractivePresenter,
    ) -> Result<bool, LiveRenderError> {
        if self.written < self.output.len() {
            return Ok(true);
        }
        self.output.clear();
        self.written = 0;
        while self.part_index < self.frame.parts.len() && self.output.is_empty() {
            match &self.frame.parts[self.part_index] {
                LivePart::TrustedLine(text) => {
                    presenter.render_trusted_line(text, |chunk| {
                        append_output(&mut self.output, chunk)
                    })?;
                    self.part_index += 1;
                    self.text_offset = 0;
                }
                LivePart::TrustedInline(text) => {
                    presenter.render_trusted_inline(text, |chunk| {
                        append_output(&mut self.output, chunk)
                    })?;
                    self.part_index += 1;
                    self.text_offset = 0;
                }
                LivePart::Untrusted { role, text } => {
                    let start = self.text_offset;
                    let mut end = start
                        .saturating_add(FRAME_SOURCE_CHUNK_BYTES)
                        .min(text.len());
                    while end > start && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    if end == start && start != text.len() {
                        return Err(LiveRenderError);
                    }
                    presenter.render_untrusted(role, &text[start..end], |chunk| {
                        append_output(&mut self.output, chunk)
                    })?;
                    self.text_offset = end;
                    if end == text.len() {
                        self.part_index += 1;
                        self.text_offset = 0;
                    }
                }
            }
        }
        Ok(!self.output.is_empty())
    }
}

fn append_output(output: &mut String, chunk: &str) -> Result<(), LiveRenderError> {
    let next = output
        .len()
        .checked_add(chunk.len())
        .ok_or(LiveRenderError)?;
    if next > FRAME_OUTPUT_CHUNK_BYTES {
        return Err(LiveRenderError);
    }
    output
        .try_reserve(chunk.len())
        .map_err(|_| LiveRenderError)?;
    output.push_str(chunk);
    Ok(())
}

pub(super) struct InteractivePresenter {
    visible: VisibleRenderer,
    active_role: Option<&'static str>,
}

impl InteractivePresenter {
    pub(super) fn new() -> Self {
        Self {
            visible: VisibleRenderer::new(),
            active_role: None,
        }
    }

    #[cfg(test)]
    pub(super) fn render<E>(
        &mut self,
        frame: &LiveFrame,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        for part in &frame.parts {
            match part {
                LivePart::TrustedLine(text) => self.render_trusted_line(text, &mut emit)?,
                LivePart::TrustedInline(text) => self.render_trusted_inline(text, &mut emit)?,
                LivePart::Untrusted { role, text } => {
                    self.render_untrusted(role, text, &mut emit)?
                }
            }
        }
        Ok(())
    }

    fn render_trusted_line<E>(
        &mut self,
        text: &'static str,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visible.ensure_line_start(&mut emit)?;
        self.visible.render_trusted(text, &mut emit)?;
        self.active_role = None;
        Ok(())
    }

    fn render_trusted_inline<E>(
        &mut self,
        text: &'static str,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        // A preceding untrusted field may itself end in LF. In that case omit
        // punctuation such as ` / ` or `: `; the following untrusted field
        // receives its own role prefix instead of leaving punctuation naked.
        if !self.visible.is_at_line_start() {
            self.visible.render_trusted(text, &mut emit)?;
        }
        if text.ends_with('\n') {
            self.active_role = None;
        }
        Ok(())
    }

    fn render_untrusted<E>(
        &mut self,
        role: &'static str,
        text: &str,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.active_role != Some(role) {
            self.visible.ensure_line_start(&mut emit)?;
        }
        self.visible.render_fragment(text, Some(role), &mut emit)?;
        self.active_role = Some(role);
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn finish_line<E>(
        &mut self,
        emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.active_role = None;
        self.visible.ensure_line_start(emit)
    }

    pub(super) fn discard_partly_written_frame(&mut self) {
        self.active_role = None;
        self.visible.force_line_boundary_on_next_output();
    }
}

pub(super) struct LiveRenderer {
    attempt: Option<AttemptState>,
}

impl LiveRenderer {
    pub(super) fn new() -> Self {
        Self { attempt: None }
    }

    pub(super) fn consume(
        &mut self,
        event: CommittedUiEvent,
    ) -> Result<LiveUpdate, LiveRenderError> {
        let seq = event.seq;
        let _time = event.time;
        let mut lifecycle = LiveLifecycle::None;
        let frame = match event.kind {
            CommittedUiKind::TurnStart { turn } => {
                let _ = turn;
                Some(LiveFrame::trusted("[working; press Ctrl+C to stop]\n")?)
            }
            CommittedUiKind::TurnEnd { turn, reason } => {
                self.attempt = None;
                lifecycle = LiveLifecycle::TurnEnded { turn };
                Some(turn_end_frame(reason)?)
            }
            CommittedUiKind::StepStart { turn, step } => {
                self.attempt = Some(AttemptState::new(turn, step));
                None
            }
            CommittedUiKind::StepEnd { turn, step } => {
                if self
                    .attempt
                    .as_ref()
                    .is_some_and(|attempt| attempt.turn == turn && attempt.step == step)
                {
                    self.attempt = None;
                }
                None
            }
            CommittedUiKind::AssistantTextDelta {
                turn,
                step,
                index,
                text,
            } => {
                self.retain_delta(turn, step, index, UiAssistantBlockKind::Text, seq, &text)?;
                LiveFrame::from_parts(single_untrusted(ASSISTANT_ROLE, text)?)
            }
            CommittedUiKind::AssistantReasoningDelta {
                turn,
                step,
                index,
                text,
            } => {
                self.retain_delta(
                    turn,
                    step,
                    index,
                    UiAssistantBlockKind::Reasoning,
                    seq,
                    &text,
                )?;
                LiveFrame::from_parts(single_untrusted(REASONING_ROLE, text)?)
            }
            CommittedUiKind::AssistantMessage {
                turn,
                step,
                content,
                sources,
            } => self.final_frame(turn, step, content, &sources)?,
            CommittedUiKind::ToolRequested {
                turn,
                step,
                call_id,
                name,
                arguments_preview,
                arguments_truncated,
            } => {
                let _ = (turn, step, call_id, arguments_truncated);
                let mut parts = try_parts(5)?;
                parts.push(LivePart::TrustedLine("[tool requested]\n"));
                parts.push(LivePart::Untrusted {
                    role: TOOL_ROLE,
                    text: name,
                });
                parts.push(LivePart::TrustedInline("\n"));
                parts.push(LivePart::Untrusted {
                    role: ARGUMENTS_ROLE,
                    text: arguments_preview,
                });
                parts.push(LivePart::TrustedInline("\n"));
                LiveFrame::from_parts(parts)
            }
            CommittedUiKind::ToolResult {
                turn,
                step,
                call_id,
                is_error,
                failure,
            } => {
                let _ = (turn, step, call_id);
                let mut parts = try_parts(5)?;
                parts.push(LivePart::TrustedLine(if is_error {
                    "[tool result: error]\n"
                } else {
                    "[tool result: success]\n"
                }));
                if let Some(failure) = failure {
                    parts.push(LivePart::Untrusted {
                        role: ERROR_ROLE,
                        text: failure.name,
                    });
                    parts.push(LivePart::TrustedInline(" / "));
                    parts.push(LivePart::Untrusted {
                        role: ERROR_ROLE,
                        text: failure.code,
                    });
                    parts.push(LivePart::TrustedInline("\n"));
                }
                LiveFrame::from_parts(parts)
            }
            CommittedUiKind::ApprovalAsked {
                id,
                tool_name,
                call_id,
                reason,
            } => {
                lifecycle = LiveLifecycle::ApprovalAsked {
                    id,
                    tool_name,
                    call_id,
                    reason,
                };
                None
            }
            CommittedUiKind::ApprovalDecided { id, outcome } => {
                lifecycle = LiveLifecycle::ApprovalDecided { id, outcome };
                Some(LiveFrame::trusted(match outcome {
                    ApprovalOutcome::AllowedOnce => "[approval: allowed once]\n",
                    ApprovalOutcome::Rejected => "[approval: rejected]\n",
                    ApprovalOutcome::Cancelled => "[approval: cancelled]\n",
                    ApprovalOutcome::Unavailable => "[approval: unavailable]\n",
                })?)
            }
            CommittedUiKind::RetryScheduled { retry_id, retry } => {
                let _ = (retry_id, retry);
                self.attempt = None;
                Some(LiveFrame::trusted("[model retry scheduled]\n")?)
            }
            CommittedUiKind::RetryStarted { retry_id, retry } => {
                let _ = (retry_id, retry);
                Some(LiveFrame::trusted("[model retry started]\n")?)
            }
            CommittedUiKind::TypeOnly { event_type } => {
                let _ = event_type;
                None
            }
        };
        Ok(LiveUpdate { frame, lifecycle })
    }

    fn retain_delta(
        &mut self,
        turn: crate::session::TurnId,
        step: crate::session::StepId,
        index: u64,
        kind: UiAssistantBlockKind,
        seq: EventSeq,
        text: &str,
    ) -> Result<(), LiveRenderError> {
        if self
            .attempt
            .as_ref()
            .is_none_or(|attempt| attempt.turn != turn || attempt.step != step)
        {
            self.attempt = Some(AttemptState::new(turn, step));
        }
        self.attempt
            .as_mut()
            .ok_or(LiveRenderError)?
            .retain(index, kind, seq, text)
    }

    fn final_frame(
        &mut self,
        turn: crate::session::TurnId,
        step: crate::session::StepId,
        content: UiAssistantContent,
        sources: &SourceSeqBitmap,
    ) -> Result<Option<LiveFrame>, LiveRenderError> {
        let attempt = self
            .attempt
            .take()
            .filter(|attempt| attempt.turn == turn && attempt.step == step);
        let state_degraded = attempt.as_ref().is_some_and(|attempt| attempt.degraded);
        match content {
            UiAssistantContent::Degraded { text } => {
                let mut parts = try_parts(2)?;
                parts.push(LivePart::TrustedLine(
                    "[final answer restated; streaming comparison limit reached]\n",
                ));
                if !text.is_empty() {
                    parts.push(LivePart::Untrusted {
                        role: ASSISTANT_ROLE,
                        text,
                    });
                }
                Ok(LiveFrame::from_parts(parts))
            }
            UiAssistantContent::Indexed(blocks) if state_degraded => {
                let mut parts = try_parts(blocks.len().saturating_add(1))?;
                parts.push(LivePart::TrustedLine(
                    "[final answer restated; streaming comparison limit reached]\n",
                ));
                for block in blocks {
                    if !block.text.is_empty() {
                        parts.push(LivePart::Untrusted {
                            role: role_for(block.kind),
                            text: block.text,
                        });
                    }
                }
                Ok(LiveFrame::from_parts(parts))
            }
            UiAssistantContent::Indexed(blocks) => {
                let retained_block_was_removed = attempt.as_ref().is_some_and(|attempt| {
                    attempt.blocks.iter().any(|streamed| {
                        !blocks.iter().any(|block| {
                            u64::from(block.index) == streamed.index && block.kind == streamed.kind
                        })
                    })
                });
                let has_mismatch = retained_block_was_removed
                    || blocks.iter().any(|block| {
                        attempt
                            .as_ref()
                            .and_then(|attempt| attempt.block(block.index.into(), block.kind))
                            .is_some_and(|streamed| {
                                streamed.compare(&block.text, sources) == Comparison::Mismatch
                            })
                    });
                let mut parts = try_parts(blocks.len().saturating_add(1))?;
                if has_mismatch {
                    parts.push(LivePart::TrustedLine("[final answer corrected]\n"));
                    for block in blocks {
                        if !block.text.is_empty() {
                            parts.push(LivePart::Untrusted {
                                role: role_for(block.kind),
                                text: block.text,
                            });
                        }
                    }
                    return Ok(LiveFrame::from_parts(parts));
                }
                for mut block in blocks {
                    let comparison = attempt
                        .as_ref()
                        .and_then(|attempt| attempt.block(block.index.into(), block.kind))
                        .map_or(Comparison::Prefix(0), |streamed| {
                            streamed.compare(&block.text, sources)
                        });
                    match comparison {
                        Comparison::Exact => {}
                        Comparison::Prefix(bytes) => {
                            if bytes < block.text.len() {
                                block.text.drain(..bytes);
                                parts.push(LivePart::Untrusted {
                                    role: role_for(block.kind),
                                    text: block.text,
                                });
                            }
                        }
                        Comparison::Mismatch => {
                            // The pre-pass above handles every mismatch by
                            // restating the complete authoritative answer.
                            return Err(LiveRenderError);
                        }
                    }
                }
                Ok(LiveFrame::from_parts(parts))
            }
        }
    }
}

fn single_untrusted(role: &'static str, text: String) -> Result<Vec<LivePart>, LiveRenderError> {
    let mut parts = try_parts(1)?;
    parts.push(LivePart::Untrusted { role, text });
    Ok(parts)
}

fn push_untrusted_line(
    parts: &mut Vec<LivePart>,
    role: &'static str,
    value: &str,
) -> Result<(), LiveRenderError> {
    let mut text = String::new();
    text.try_reserve_exact(value.len())
        .map_err(|_| LiveRenderError)?;
    text.push_str(value);
    parts.push(LivePart::Untrusted { role, text });
    parts.push(LivePart::TrustedInline("\n"));
    Ok(())
}

fn try_parts(capacity: usize) -> Result<Vec<LivePart>, LiveRenderError> {
    let mut parts = Vec::new();
    parts
        .try_reserve_exact(capacity)
        .map_err(|_| LiveRenderError)?;
    Ok(parts)
}

fn role_for(kind: UiAssistantBlockKind) -> &'static str {
    match kind {
        UiAssistantBlockKind::Text => ASSISTANT_ROLE,
        UiAssistantBlockKind::Reasoning => REASONING_ROLE,
    }
}

fn turn_end_frame(reason: UiTurnEndReason) -> Result<LiveFrame, LiveRenderError> {
    let frame = match reason {
        UiTurnEndReason::Completed => LiveFrame::trusted("[done]\n")?,
        UiTurnEndReason::Aborted => LiveFrame::trusted("[stopped]\n")?,
        UiTurnEndReason::Blocked => LiveFrame::trusted("[blocked]\n")?,
        UiTurnEndReason::MaxTokens => LiveFrame::trusted("[maximum tokens reached]\n")?,
        UiTurnEndReason::Interrupted => LiveFrame::trusted("[interrupted]\n")?,
        UiTurnEndReason::Error { code, message } => {
            let mut parts = try_parts(5)?;
            parts.push(LivePart::TrustedLine("[turn error]\n"));
            parts.push(LivePart::Untrusted {
                role: ERROR_ROLE,
                text: code,
            });
            parts.push(LivePart::TrustedInline(": "));
            parts.push(LivePart::Untrusted {
                role: ERROR_ROLE,
                text: message,
            });
            parts.push(LivePart::TrustedInline("\n"));
            LiveFrame { parts }
        }
        UiTurnEndReason::Other { kind } => {
            let mut parts = try_parts(3)?;
            parts.push(LivePart::TrustedLine("[turn ended]\n"));
            if let Some(kind) = kind {
                parts.push(LivePart::Untrusted {
                    role: ERROR_ROLE,
                    text: kind,
                });
                parts.push(LivePart::TrustedInline("\n"));
            }
            LiveFrame { parts }
        }
    };
    Ok(frame)
}

struct AttemptState {
    turn: crate::session::TurnId,
    step: crate::session::StepId,
    blocks: Vec<StreamedBlock>,
    retained_bytes: usize,
    degraded: bool,
}

impl AttemptState {
    fn new(turn: crate::session::TurnId, step: crate::session::StepId) -> Self {
        Self {
            turn,
            step,
            blocks: Vec::new(),
            retained_bytes: 0,
            degraded: false,
        }
    }

    fn retain(
        &mut self,
        index: u64,
        kind: UiAssistantBlockKind,
        seq: EventSeq,
        text: &str,
    ) -> Result<(), LiveRenderError> {
        if self.degraded {
            return Ok(());
        }
        let next_bytes = self
            .retained_bytes
            .checked_add(text.len())
            .ok_or(LiveRenderError)?;
        let existing = self
            .blocks
            .iter()
            .position(|block| block.index == index && block.kind == kind);
        if next_bytes > MAX_ATTEMPT_TEXT_BYTES
            || (existing.is_none() && self.blocks.len() == MAX_ATTEMPT_BLOCKS)
        {
            self.blocks.clear();
            self.retained_bytes = 0;
            self.degraded = true;
            return Ok(());
        }
        let position = if let Some(position) = existing {
            position
        } else {
            self.blocks.try_reserve(1).map_err(|_| LiveRenderError)?;
            self.blocks.push(StreamedBlock {
                index,
                kind,
                fragments: Vec::new(),
            });
            self.blocks.len() - 1
        };
        self.blocks[position].retain(seq, text)?;
        self.retained_bytes = next_bytes;
        Ok(())
    }

    fn block(&self, index: u64, kind: UiAssistantBlockKind) -> Option<&StreamedBlock> {
        self.blocks
            .iter()
            .find(|block| block.index == index && block.kind == kind)
    }
}

struct StreamedBlock {
    index: u64,
    kind: UiAssistantBlockKind,
    fragments: Vec<StreamedFragment>,
}

impl StreamedBlock {
    fn retain(&mut self, seq: EventSeq, text: &str) -> Result<(), LiveRenderError> {
        let mut copy = String::new();
        copy.try_reserve_exact(text.len())
            .map_err(|_| LiveRenderError)?;
        copy.push_str(text);
        self.fragments.try_reserve(1).map_err(|_| LiveRenderError)?;
        self.fragments.push(StreamedFragment { seq, text: copy });
        Ok(())
    }

    fn compare(&self, final_text: &str, sources: &SourceSeqBitmap) -> Comparison {
        if self
            .fragments
            .iter()
            .any(|fragment| !sources.contains(fragment.seq))
        {
            return Comparison::Mismatch;
        }
        let mut offset = 0_usize;
        for fragment in self
            .fragments
            .iter()
            .filter(|fragment| sources.contains(fragment.seq))
        {
            let Some(end) = offset.checked_add(fragment.text.len()) else {
                return Comparison::Mismatch;
            };
            if final_text.get(offset..end) != Some(fragment.text.as_str()) {
                return Comparison::Mismatch;
            }
            offset = end;
        }
        if offset == final_text.len() {
            Comparison::Exact
        } else {
            Comparison::Prefix(offset)
        }
    }
}

struct StreamedFragment {
    seq: EventSeq,
    text: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Comparison {
    Exact,
    Prefix(usize),
    Mismatch,
}

#[cfg(test)]
mod tests {
    use crate::session::{
        CommittedUiEvent, CommittedUiKind, EventSeq, SourceSeqBitmap, StepId, TurnId,
        UiAssistantBlock, UiAssistantBlockKind, UiAssistantContent, UiToolFailure, UiTurnEndReason,
        UnixMillis,
    };

    use super::{
        AttemptState, InteractivePresenter, LiveFrame, LiveRenderer, MAX_ATTEMPT_BLOCKS,
        MAX_ATTEMPT_TEXT_BYTES,
    };

    fn event(seq: u64, kind: CommittedUiKind) -> CommittedUiEvent {
        CommittedUiEvent {
            seq: EventSeq::new(seq).unwrap(),
            time: UnixMillis::new(1).unwrap(),
            kind,
        }
    }

    fn render(
        renderer: &mut LiveRenderer,
        presenter: &mut InteractivePresenter,
        event: CommittedUiEvent,
        output: &mut String,
    ) {
        if let Some(frame) = renderer.consume(event).unwrap().frame {
            presenter
                .render(&frame, |chunk| {
                    output.push_str(chunk);
                    Ok::<_, std::convert::Infallible>(())
                })
                .unwrap();
        }
    }

    #[test]
    fn startup_banner_names_the_session_and_lifecycle_safely() {
        let mut output = String::new();
        let mut presenter = InteractivePresenter::new();
        presenter
            .render(
                &LiveFrame::startup_banner("session-550e8400-e29b-41d4-a716-446655440000", true)
                    .unwrap(),
                |chunk| {
                    output.push_str(chunk);
                    Ok::<_, std::convert::Infallible>(())
                },
            )
            .unwrap();
        assert_eq!(
            output,
            "dsh | interactive; resumed session session-550e8400-e29b-41d4-a716-446655440000\n"
        );
    }

    #[test]
    fn turn_start_and_stopped_summary_are_fixed_status_frames() {
        let turn = TurnId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = InteractivePresenter::new();
        let mut output = String::new();
        render(
            &mut renderer,
            &mut presenter,
            event(0, CommittedUiKind::TurnStart { turn }),
            &mut output,
        );
        presenter
            .render(&LiveFrame::stopped(7).unwrap(), |chunk| {
                output.push_str(chunk);
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        assert_eq!(
            output,
            concat!(
                "[working; press Ctrl+C to stop]\n",
                "dsh | stopped; skipped 7 updates\n"
            )
        );
    }

    #[test]
    fn approval_frame_streams_the_complete_preview_challenge_and_ready_marker() {
        let challenge = uuid::Uuid::parse_str("00112233-4455-4677-8899-aabbccddeeff").unwrap();
        let frame = LiveFrame::approval(
            "apply_patch",
            Some("call-patch"),
            Some("update note.txt"),
            "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n",
            false,
        )
        .unwrap();
        let mut pending = frame.into_pending().unwrap();
        let mut presenter = InteractivePresenter::new();
        let mut output = Vec::new();
        while pending.prepare_next(&mut presenter).unwrap() {
            output.extend_from_slice(pending.bytes());
            let count = pending.bytes().len();
            pending.advance(count).unwrap();
        }
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("[approval requested]\n"));
        assert!(output.contains("preview | --- a/note.txt\n"));
        assert!(!output.contains(challenge.to_string().as_str()));
        let mut ready_output = String::new();
        InteractivePresenter::new()
            .render(&LiveFrame::approval_ready(challenge).unwrap(), |chunk| {
                ready_output.push_str(chunk);
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        assert_eq!(
            ready_output,
            concat!(
                "[approval input ready]\n",
                "dsh approval > allow 00112233-4455-4677-8899-aabbccddeeff | reject | cancel\n"
            )
        );
    }

    #[test]
    fn matching_final_text_only_appends_the_missing_suffix() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = InteractivePresenter::new();
        let mut output = String::new();
        render(
            &mut renderer,
            &mut presenter,
            event(
                0,
                CommittedUiKind::AssistantTextDelta {
                    turn,
                    step,
                    index: 0,
                    text: "hel".to_owned(),
                },
            ),
            &mut output,
        );
        render(
            &mut renderer,
            &mut presenter,
            event(
                1,
                CommittedUiKind::AssistantMessage {
                    turn,
                    step,
                    content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                        index: 0,
                        kind: UiAssistantBlockKind::Text,
                        text: "hello".to_owned(),
                    }]),
                    sources: SourceSeqBitmap::from_sources(&[EventSeq::new(0).unwrap()]).unwrap(),
                },
            ),
            &mut output,
        );
        presenter
            .finish_line(|chunk| {
                output.push_str(chunk);
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        assert_eq!(output, "assistant | hello\n");
    }

    #[test]
    fn mismatching_final_text_is_explicitly_rested_and_control_safe() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = InteractivePresenter::new();
        let mut output = String::new();
        render(
            &mut renderer,
            &mut presenter,
            event(
                0,
                CommittedUiKind::AssistantTextDelta {
                    turn,
                    step,
                    index: 0,
                    text: "old".to_owned(),
                },
            ),
            &mut output,
        );
        render(
            &mut renderer,
            &mut presenter,
            event(
                1,
                CommittedUiKind::AssistantMessage {
                    turn,
                    step,
                    content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                        index: 0,
                        kind: UiAssistantBlockKind::Text,
                        text: "new\r\u{202e}".to_owned(),
                    }]),
                    sources: SourceSeqBitmap::from_sources(&[EventSeq::new(0).unwrap()]).unwrap(),
                },
            ),
            &mut output,
        );
        assert_eq!(
            output,
            concat!(
                "assistant | old\n",
                "[final answer corrected]\n",
                "assistant | new\\r\\u{202e}"
            )
        );
    }

    #[test]
    fn one_mismatching_block_restates_the_complete_authoritative_answer() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = InteractivePresenter::new();
        let mut output = String::new();
        for (seq, index, text) in [(0, 0, "same\n"), (1, 1, "old\n")] {
            render(
                &mut renderer,
                &mut presenter,
                event(
                    seq,
                    CommittedUiKind::AssistantTextDelta {
                        turn,
                        step,
                        index,
                        text: text.to_owned(),
                    },
                ),
                &mut output,
            );
        }
        render(
            &mut renderer,
            &mut presenter,
            event(
                2,
                CommittedUiKind::AssistantMessage {
                    turn,
                    step,
                    content: UiAssistantContent::Indexed(vec![
                        UiAssistantBlock {
                            index: 0,
                            kind: UiAssistantBlockKind::Text,
                            text: "same\n".to_owned(),
                        },
                        UiAssistantBlock {
                            index: 1,
                            kind: UiAssistantBlockKind::Text,
                            text: "new\n".to_owned(),
                        },
                    ]),
                    sources: SourceSeqBitmap::from_sources(&[
                        EventSeq::new(0).unwrap(),
                        EventSeq::new(1).unwrap(),
                    ])
                    .unwrap(),
                },
            ),
            &mut output,
        );
        assert_eq!(
            output,
            concat!(
                "assistant | same\n",
                "assistant | old\n",
                "[final answer corrected]\n",
                "assistant | same\n",
                "assistant | new\n"
            )
        );
    }

    #[test]
    fn dedup_cache_accepts_exact_limits_and_degrades_at_one_over() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut bytes = AttemptState::new(turn, step);
        let exact = "x".repeat(MAX_ATTEMPT_TEXT_BYTES);
        bytes
            .retain(
                0,
                UiAssistantBlockKind::Text,
                EventSeq::new(0).unwrap(),
                &exact,
            )
            .unwrap();
        assert!(!bytes.degraded);
        assert_eq!(bytes.retained_bytes, MAX_ATTEMPT_TEXT_BYTES);
        bytes
            .retain(
                0,
                UiAssistantBlockKind::Text,
                EventSeq::new(1).unwrap(),
                "x",
            )
            .unwrap();
        assert!(bytes.degraded);
        assert!(bytes.blocks.is_empty());

        let mut blocks = AttemptState::new(turn, step);
        for index in 0..MAX_ATTEMPT_BLOCKS {
            blocks
                .retain(
                    u64::try_from(index).unwrap(),
                    UiAssistantBlockKind::Text,
                    EventSeq::new(u64::try_from(index).unwrap()).unwrap(),
                    "x",
                )
                .unwrap();
        }
        assert!(!blocks.degraded);
        assert_eq!(blocks.blocks.len(), MAX_ATTEMPT_BLOCKS);
        blocks
            .retain(
                u64::try_from(MAX_ATTEMPT_BLOCKS).unwrap(),
                UiAssistantBlockKind::Text,
                EventSeq::new(u64::try_from(MAX_ATTEMPT_BLOCKS).unwrap()).unwrap(),
                "x",
            )
            .unwrap();
        assert!(blocks.degraded);

        let mut renderer = LiveRenderer {
            attempt: Some(blocks),
        };
        let mut presenter = InteractivePresenter::new();
        let mut output = String::new();
        render(
            &mut renderer,
            &mut presenter,
            event(
                200,
                CommittedUiKind::AssistantMessage {
                    turn,
                    step,
                    content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                        index: 0,
                        kind: UiAssistantBlockKind::Text,
                        text: "authoritative".to_owned(),
                    }]),
                    sources: SourceSeqBitmap::from_sources(&[]).unwrap(),
                },
            ),
            &mut output,
        );
        assert_eq!(
            output,
            concat!(
                "[final answer restated; streaming comparison limit reached]\n",
                "assistant | authoritative"
            )
        );
    }

    #[test]
    fn multiline_failures_and_turn_errors_keep_every_line_role_framed() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = InteractivePresenter::new();
        let mut output = String::new();
        render(
            &mut renderer,
            &mut presenter,
            event(
                0,
                CommittedUiKind::ToolResult {
                    turn,
                    step,
                    call_id: "call-1".to_owned(),
                    is_error: true,
                    failure: Some(UiToolFailure {
                        name: "NAME\n".to_owned(),
                        code: "CODE\nTAIL".to_owned(),
                    }),
                },
            ),
            &mut output,
        );
        render(
            &mut renderer,
            &mut presenter,
            event(
                1,
                CommittedUiKind::TurnEnd {
                    turn,
                    reason: UiTurnEndReason::Error {
                        code: "ERR\n".to_owned(),
                        message: "MESSAGE\nTAIL".to_owned(),
                    },
                },
            ),
            &mut output,
        );
        assert_eq!(
            output,
            concat!(
                "[tool result: error]\n",
                "error | NAME\n",
                "error | CODE\n",
                "error | TAIL\n",
                "[turn error]\n",
                "error | ERR\n",
                "error | MESSAGE\n",
                "error | TAIL\n"
            )
        );
    }

    #[test]
    fn tool_intent_is_requested_not_running_and_retry_closes_partial_state() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = InteractivePresenter::new();
        let mut output = String::new();
        render(
            &mut renderer,
            &mut presenter,
            event(
                0,
                CommittedUiKind::ToolRequested {
                    turn,
                    step,
                    call_id: "call-1".to_owned(),
                    name: "read\nspoof".to_owned(),
                    arguments_preview: "arguments omitted".to_owned(),
                    arguments_truncated: true,
                },
            ),
            &mut output,
        );
        assert!(output.contains("[tool requested]"));
        assert!(!output.contains("running"));
        assert!(output.contains("tool | spoof"));
    }
}
