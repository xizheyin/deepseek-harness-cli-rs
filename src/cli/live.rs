use std::{fmt, fmt::Write as _, sync::Arc};

use thiserror::Error;

use crate::session::{
    ApprovalOutcome, CommittedUiEvent, CommittedUiKind, EventSeq, SourceSeqBitmap,
    TurnEndCancelCause, TurnEndReason, UiAssistantBlockKind, UiAssistantContent, UiIdentity,
    UiTurnEndCancelCause, UiTurnEndReason,
};
use crate::tui::{
    approval_preview::present_canonical_patch,
    markup::{MAX_MARKUP_FRAME_TEXT_BYTES, MarkupState},
    presentation::{PresentationError, PresentedChunk, PresentedChunkBuilder, TextStyle},
    projector::UiProjector,
    timeline::{TimelineTone, ToolCardView, WorkReceiptView},
    view::{ContextEstimate, DetailDocument, JoinedTurnView, ViewArchive},
    visible::{render_visible_owned, render_visible_owned_bounded},
};

use crate::agent::{ApprovalPreviewKind, TurnOutcome};

use super::{
    render::VisibleRenderer,
    theme::{UiRole, UiTheme},
};

const MAX_ATTEMPT_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTEMPT_BLOCKS: usize = 128;
const FRAME_SOURCE_CHUNK_BYTES: usize = 512;
pub(super) const FRAME_OUTPUT_CHUNK_BYTES: usize = 8 * 1024;
const MAX_DOCK_NOTICE_BYTES: usize = 4 * 1024;

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
    enhanced_frame: EnhancedFrame,
    dock_notice: DockNoticeUpdate,
}

#[derive(Debug)]
enum EnhancedFrame {
    Same,
    Suppress,
    Replace(LiveFrame),
}

enum DockNoticeUpdate {
    Keep,
    Clear,
    Set(String),
}

impl fmt::Debug for DockNoticeUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => formatter.write_str("Keep"),
            Self::Clear => formatter.write_str("Clear"),
            Self::Set(value) => formatter
                .debug_struct("Set")
                .field("bytes", &value.len())
                .finish(),
        }
    }
}

impl LiveUpdate {
    pub(super) fn take_frame(&mut self, enhanced: bool) -> Option<LiveFrame> {
        let frame = self.frame.take();
        if !enhanced {
            return frame;
        }
        match std::mem::replace(&mut self.enhanced_frame, EnhancedFrame::Suppress) {
            EnhancedFrame::Same => frame,
            EnhancedFrame::Suppress => None,
            EnhancedFrame::Replace(replacement) => Some(replacement),
        }
    }

    pub(super) fn apply_dock_notice(&mut self, notice: &mut Option<String>) -> bool {
        match std::mem::replace(&mut self.dock_notice, DockNoticeUpdate::Keep) {
            DockNoticeUpdate::Keep => false,
            DockNoticeUpdate::Clear => {
                let changed = notice.is_some();
                *notice = None;
                changed
            }
            DockNoticeUpdate::Set(value) => {
                let changed = notice.as_ref() != Some(&value);
                *notice = Some(value);
                changed
            }
        }
    }
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

enum LivePart {
    TrustedLine(&'static str),
    TrustedOwned(String),
    TrustedInline(&'static str),
    AssistantMarkup {
        key: MarkupStreamKey,
        text: String,
    },
    AssistantMarkupFinish {
        key: Option<MarkupStreamKey>,
    },
    AssistantMarkupAbort,
    Untrusted {
        role: UiRole,
        text: String,
    },
    LinearOnlyUntrusted {
        role: UiRole,
        text: String,
    },
    ApprovalPreview {
        kind: ApprovalPreviewKind,
        text: Arc<str>,
    },
    UntrustedStyled {
        style: TextStyle,
        text: String,
    },
}

impl fmt::Debug for LivePart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TrustedLine(text) => formatter
                .debug_struct("TrustedLine")
                .field("bytes", &text.len())
                .finish(),
            Self::TrustedOwned(text) => formatter
                .debug_struct("TrustedOwned")
                .field("bytes", &text.len())
                .finish(),
            Self::TrustedInline(text) => formatter
                .debug_struct("TrustedInline")
                .field("bytes", &text.len())
                .finish(),
            Self::AssistantMarkup { key, text } => formatter
                .debug_struct("AssistantMarkup")
                .field("key", key)
                .field("bytes", &text.len())
                .finish(),
            Self::AssistantMarkupFinish { key } => formatter
                .debug_struct("AssistantMarkupFinish")
                .field("key", key)
                .finish(),
            Self::AssistantMarkupAbort => formatter.write_str("AssistantMarkupAbort"),
            Self::Untrusted { role, text } => formatter
                .debug_struct("Untrusted")
                .field("role", role)
                .field("bytes", &text.len())
                .finish(),
            Self::LinearOnlyUntrusted { role, text } => formatter
                .debug_struct("LinearOnlyUntrusted")
                .field("role", role)
                .field("bytes", &text.len())
                .finish(),
            Self::ApprovalPreview { kind, text } => formatter
                .debug_struct("ApprovalPreview")
                .field("kind", kind)
                .field("bytes", &text.len())
                .finish(),
            Self::UntrustedStyled { style, text } => formatter
                .debug_struct("UntrustedStyled")
                .field("style", style)
                .field("bytes", &text.len())
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MarkupStreamKey {
    turn: crate::session::TurnId,
    step: crate::session::StepId,
    block: u64,
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
            role: UiRole::Dsh,
            text,
        });
        Ok(Self { parts })
    }

    pub(super) fn help() -> Result<Self, LiveRenderError> {
        Self::trusted(
            "[commands]\n/inspect  show committed turn facts\n/review  show the last joined turn summary\n/help  show this help\n/exit  exit dsh\n/quit  exit dsh\n",
        )
    }

    pub(super) fn notice(value: &'static str) -> Result<Self, LiveRenderError> {
        Self::trusted(value)
    }

    pub(super) fn detail_document(document: &DetailDocument) -> Result<Self, LiveRenderError> {
        let mut text = String::new();
        text.try_reserve(document.title().len().saturating_add(1))
            .map_err(|_| LiveRenderError)?;
        text.push_str(document.title());
        text.push('\n');
        for line in document.lines() {
            text.try_reserve(line.text().len().saturating_add(1))
                .map_err(|_| LiveRenderError)?;
            text.push_str(line.text());
            text.push('\n');
        }
        let mut parts = try_parts(1)?;
        parts.push(LivePart::UntrustedStyled {
            style: TextStyle::Plain,
            text,
        });
        Ok(Self { parts })
    }

    fn markup_abort() -> Result<Self, LiveRenderError> {
        let mut parts = try_parts(1)?;
        parts.push(LivePart::AssistantMarkupAbort);
        Ok(Self { parts })
    }

    pub(super) fn human_message(text: String) -> Result<Self, LiveRenderError> {
        let mut parts = try_parts(2)?;
        parts.push(LivePart::Untrusted {
            role: UiRole::User,
            text,
        });
        parts.push(LivePart::TrustedInline("\n\n"));
        Ok(Self { parts })
    }

    pub(super) fn stopped(skipped: usize) -> Result<Self, LiveRenderError> {
        let mut text = String::new();
        text.try_reserve_exact(64).map_err(|_| LiveRenderError)?;
        writeln!(&mut text, "stopped; skipped {skipped} updates").map_err(|_| LiveRenderError)?;
        let mut parts = try_parts(1)?;
        parts.push(LivePart::Untrusted {
            role: UiRole::Dsh,
            text,
        });
        Ok(Self { parts })
    }

    pub(super) fn approval(
        tool_name: &str,
        call_id: Option<&str>,
        reason: Option<&str>,
        preview: Arc<str>,
        preview_kind: &ApprovalPreviewKind,
        retry: bool,
    ) -> Result<Self, LiveRenderError> {
        let mut parts = try_parts(12)?;
        parts.push(LivePart::TrustedLine(if retry {
            "[approval answer not recognized]\n"
        } else {
            "[approval requested]\n"
        }));
        let canonical_patch = matches!(preview_kind, ApprovalPreviewKind::CanonicalPatch(_));
        push_approval_metadata_line(&mut parts, UiRole::Tool, tool_name, canonical_patch)?;
        if let Some(call_id) = call_id {
            push_approval_metadata_line(&mut parts, UiRole::Call, call_id, canonical_patch)?;
        }
        if let Some(reason) = reason {
            push_approval_metadata_line(&mut parts, UiRole::Reason, reason, canonical_patch)?;
        }
        parts.push(LivePart::ApprovalPreview {
            kind: preview_kind.clone(),
            text: preview,
        });
        parts.push(LivePart::TrustedInline("\n"));
        Ok(Self { parts })
    }

    pub(super) fn approval_selector(output: String) -> Result<Self, LiveRenderError> {
        if output.is_empty() || output.len() > FRAME_OUTPUT_CHUNK_BYTES {
            return Err(LiveRenderError);
        }
        let mut parts = try_parts(1)?;
        parts.push(LivePart::TrustedOwned(output));
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
                LivePart::TrustedOwned(text) => {
                    presenter.render_trusted_owned(text, |chunk| {
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
                LivePart::AssistantMarkup { text, .. } => {
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
                    presenter.render_untrusted(UiRole::Assistant, &text[start..end], |chunk| {
                        append_output(&mut self.output, chunk)
                    })?;
                    self.text_offset = end;
                    if end == text.len() {
                        self.part_index += 1;
                        self.text_offset = 0;
                    }
                }
                LivePart::AssistantMarkupFinish { .. } => {
                    self.part_index += 1;
                    self.text_offset = 0;
                }
                LivePart::AssistantMarkupAbort => {
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
                    presenter.render_untrusted(*role, &text[start..end], |chunk| {
                        append_output(&mut self.output, chunk)
                    })?;
                    self.text_offset = end;
                    if end == text.len() {
                        self.part_index += 1;
                        self.text_offset = 0;
                    }
                }
                LivePart::LinearOnlyUntrusted { role, text } => {
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
                    presenter.render_untrusted(*role, &text[start..end], |chunk| {
                        append_output(&mut self.output, chunk)
                    })?;
                    self.text_offset = end;
                    if end == text.len() {
                        self.part_index += 1;
                        self.text_offset = 0;
                    }
                }
                LivePart::ApprovalPreview { text, .. } => {
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
                    presenter.render_untrusted(UiRole::Preview, &text[start..end], |chunk| {
                        append_output(&mut self.output, chunk)
                    })?;
                    self.text_offset = end;
                    if end == text.len() {
                        self.part_index += 1;
                        self.text_offset = 0;
                    }
                }
                LivePart::UntrustedStyled { text, .. } => {
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
                    presenter.render_untrusted_styled(&text[start..end], |chunk| {
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
    active_role: Option<UiRole>,
    theme: UiTheme,
}

#[derive(Clone)]
pub(super) struct EnhancedPresenter {
    at_line_start: bool,
    active_role: Option<UiRole>,
    markup_key: Option<MarkupStreamKey>,
    markup: MarkupState,
}

pub(super) struct PreparedPresentation {
    chunk: PresentedChunk,
    next: Box<EnhancedPresenter>,
}

impl fmt::Debug for PreparedPresentation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedPresentation")
            .field("chunk", &self.chunk)
            .finish()
    }
}

impl PreparedPresentation {
    pub(super) const fn chunk(&self) -> &PresentedChunk {
        &self.chunk
    }

    pub(super) fn force_next_line_boundary(&mut self) {
        self.next.force_line_boundary();
    }
}

impl EnhancedPresenter {
    pub(super) fn new() -> Self {
        Self {
            at_line_start: true,
            active_role: None,
            markup_key: None,
            markup: MarkupState::default(),
        }
    }

    pub(super) fn prepare(
        &self,
        frame: LiveFrame,
    ) -> Result<PreparedPresentation, LiveRenderError> {
        let mut next = self.clone();
        let chunk = next.present_mut(frame)?;
        Ok(PreparedPresentation {
            chunk,
            next: Box::new(next),
        })
    }

    pub(super) fn commit(&mut self, prepared: PreparedPresentation) {
        *self = *prepared.next;
    }

    pub(super) fn force_line_boundary(&mut self) {
        self.at_line_start = true;
        self.active_role = None;
    }

    fn present_mut(&mut self, frame: LiveFrame) -> Result<PresentedChunk, LiveRenderError> {
        let mut builder = PresentedChunk::builder();
        for part in frame.parts {
            match part {
                LivePart::AssistantMarkup { key, text } => {
                    self.push_assistant_markup(&mut builder, key, &text)?;
                }
                LivePart::AssistantMarkupFinish { key } => {
                    self.finish_markup_key_authoritatively(&mut builder, key)?;
                }
                LivePart::AssistantMarkupAbort => {
                    self.abort_active_markup(&mut builder)?;
                }
                LivePart::TrustedLine(text) => {
                    self.abort_active_markup(&mut builder)?;
                    self.ensure_line_start(&mut builder)?;
                    let (style, text) = enhanced_trusted_line(text);
                    self.push_text_with_lines(&mut builder, style, text)?;
                    self.active_role = None;
                }
                LivePart::TrustedOwned(text) => {
                    self.abort_active_markup(&mut builder)?;
                    self.ensure_line_start(&mut builder)?;
                    let text = strip_product_terminal_controls(&text)?;
                    self.push_text_with_lines(&mut builder, TextStyle::Warning, &text)?;
                    self.active_role = None;
                }
                LivePart::TrustedInline(text) => {
                    self.abort_active_markup(&mut builder)?;
                    if !self.at_line_start {
                        self.push_text_with_lines(&mut builder, TextStyle::Plain, text)?;
                    }
                    if text.ends_with('\n') {
                        self.active_role = None;
                    }
                }
                LivePart::Untrusted {
                    role: UiRole::Reasoning,
                    ..
                } => {}
                LivePart::Untrusted { role, text } => {
                    self.abort_active_markup(&mut builder)?;
                    let text = render_visible_owned(&text, true).map_err(|_| LiveRenderError)?;
                    self.push_role_fragment(&mut builder, role, &text)?;
                }
                LivePart::LinearOnlyUntrusted { .. } => {}
                LivePart::ApprovalPreview { kind, text } => {
                    self.abort_active_markup(&mut builder)?;
                    match kind {
                        ApprovalPreviewKind::Opaque => {
                            let text =
                                render_visible_owned(&text, true).map_err(|_| LiveRenderError)?;
                            self.push_role_fragment(&mut builder, UiRole::Preview, &text)?;
                        }
                        ApprovalPreviewKind::CanonicalPatch(facts) => {
                            self.ensure_line_start(&mut builder)?;
                            present_canonical_patch(&mut builder, &facts, &text)
                                .map_err(|_| LiveRenderError)?;
                            self.at_line_start = true;
                            self.active_role = None;
                        }
                    }
                }
                LivePart::UntrustedStyled { style, text } => {
                    self.abort_active_markup(&mut builder)?;
                    self.ensure_line_start(&mut builder)?;
                    let text = render_visible_owned(&text, true).map_err(|_| LiveRenderError)?;
                    self.push_text_with_lines(&mut builder, style, &text)?;
                    self.active_role = None;
                }
            }
        }
        Ok(builder.finish())
    }

    fn push_assistant_markup(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        key: MarkupStreamKey,
        text: &str,
    ) -> Result<(), LiveRenderError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.markup_key != Some(key) {
            self.abort_active_markup(builder)?;
            self.markup_key = Some(key);
        }
        if self.active_role != Some(UiRole::Assistant) {
            self.ensure_line_start(builder)?;
            builder
                .push_text(TextStyle::Accent, "DSH")
                .map_err(map_presentation_error)?;
            builder.push_line_feed().map_err(map_presentation_error)?;
            self.at_line_start = true;
        }
        match render_visible_owned_bounded(text, true, MAX_MARKUP_FRAME_TEXT_BYTES)
            .map_err(|_| LiveRenderError)?
        {
            Some(text) => self
                .markup
                .push(&text, builder, &mut self.at_line_start)
                .map_err(map_presentation_error)?,
            None => self
                .markup
                .omit_remaining_display(builder, &mut self.at_line_start)
                .map_err(map_presentation_error)?,
        }
        self.active_role = Some(UiRole::Assistant);
        Ok(())
    }

    fn abort_active_markup(
        &mut self,
        builder: &mut PresentedChunkBuilder,
    ) -> Result<(), LiveRenderError> {
        if self.markup_key.is_none() {
            return Ok(());
        }
        self.ensure_pending_markup_header(builder)?;
        self.markup
            .abort_plain(builder, &mut self.at_line_start)
            .map_err(map_presentation_error)?;
        self.markup_key = None;
        self.active_role = None;
        Ok(())
    }

    fn finish_markup_key_authoritatively(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        key: Option<MarkupStreamKey>,
    ) -> Result<(), LiveRenderError> {
        if self.markup_key.is_none() {
            return Ok(());
        }
        if key.is_some() && key != self.markup_key {
            return Ok(());
        }
        self.ensure_pending_markup_header(builder)?;
        self.markup
            .finish_authoritative(builder, &mut self.at_line_start)
            .map_err(map_presentation_error)?;
        self.markup_key = None;
        self.active_role = None;
        Ok(())
    }

    fn ensure_pending_markup_header(
        &mut self,
        builder: &mut PresentedChunkBuilder,
    ) -> Result<(), LiveRenderError> {
        if self.active_role != Some(UiRole::Assistant) && self.markup.has_pending_source() {
            self.ensure_line_start(builder)?;
            builder
                .push_text(TextStyle::Accent, "DSH")
                .map_err(map_presentation_error)?;
            builder.push_line_feed().map_err(map_presentation_error)?;
            self.at_line_start = true;
        }
        Ok(())
    }

    fn ensure_line_start(
        &mut self,
        builder: &mut PresentedChunkBuilder,
    ) -> Result<(), LiveRenderError> {
        if !self.at_line_start {
            builder.push_line_feed().map_err(map_presentation_error)?;
            self.at_line_start = true;
        }
        Ok(())
    }

    fn push_role_fragment(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        role: UiRole,
        text: &str,
    ) -> Result<(), LiveRenderError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.active_role != Some(role) && !self.at_line_start {
            self.ensure_line_start(builder)?;
        }
        let style = style_for_role(role);
        for segment in text.split_inclusive('\n') {
            let (content, line_feed) = segment
                .strip_suffix('\n')
                .map_or((segment, false), |content| (content, true));
            if self.at_line_start {
                builder
                    .push_text(style, prefix_for_role(role))
                    .map_err(map_presentation_error)?;
                self.at_line_start = false;
            }
            builder
                .push_text(style, content)
                .map_err(map_presentation_error)?;
            if line_feed {
                builder.push_line_feed().map_err(map_presentation_error)?;
                self.at_line_start = true;
            }
        }
        self.active_role = Some(role);
        Ok(())
    }

    fn push_text_with_lines(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        style: TextStyle,
        text: &str,
    ) -> Result<(), LiveRenderError> {
        for segment in text.split_inclusive('\n') {
            let (content, line_feed) = segment
                .strip_suffix('\n')
                .map_or((segment, false), |content| (content, true));
            builder
                .push_text(style, content)
                .map_err(map_presentation_error)?;
            if !content.is_empty() {
                self.at_line_start = false;
            }
            if line_feed {
                builder.push_line_feed().map_err(map_presentation_error)?;
                self.at_line_start = true;
            }
        }
        Ok(())
    }
}

fn map_presentation_error(_: PresentationError) -> LiveRenderError {
    LiveRenderError
}

fn style_for_role(role: UiRole) -> TextStyle {
    match role {
        UiRole::User => TextStyle::User,
        UiRole::Assistant => TextStyle::Assistant,
        UiRole::Reasoning | UiRole::Arguments | UiRole::Call => TextStyle::Muted,
        UiRole::Tool | UiRole::Dsh => TextStyle::Accent,
        UiRole::Reason | UiRole::Preview => TextStyle::Warning,
        UiRole::Error => TextStyle::Error,
    }
}

fn prefix_for_role(role: UiRole) -> &'static str {
    match role {
        UiRole::User => "YOU  ",
        UiRole::Assistant => "DSH  ",
        UiRole::Reasoning => "Thinking  ",
        UiRole::Tool => "Tool  ",
        UiRole::Arguments => "  args  ",
        UiRole::Call => "  call  ",
        UiRole::Reason => "  why  ",
        UiRole::Preview => "  | ",
        UiRole::Dsh => "dsh-rs  ",
        UiRole::Error => "Error  ",
    }
}

fn enhanced_trusted_line(text: &'static str) -> (TextStyle, &'static str) {
    match text {
        "[working; press Ctrl+C to stop]\n" => (TextStyle::Accent, "Working · Ctrl+C to stop\n"),
        "[tool requested]\n" => (TextStyle::Accent, "Tool requested\n"),
        "[tool result: success]\n" => (TextStyle::Success, "Tool finished\n"),
        "[tool result: error]\n" => (TextStyle::Error, "Tool failed\n"),
        "[approval requested]\n" => (TextStyle::Warning, "Approval required\n"),
        "[approval answer not recognized]\n" => {
            (TextStyle::Warning, "Choose an approval action again\n")
        }
        "[approval: allowed once]\n" => (TextStyle::Success, "Allowed once\n"),
        "[approval: rejected]\n" => (TextStyle::Warning, "Rejected\n"),
        "[approval: cancelled]\n" => (TextStyle::Warning, "Cancelled\n"),
        "[approval: unavailable]\n" => (TextStyle::Error, "Approval unavailable\n"),
        "[model retry scheduled]\n" => (TextStyle::Warning, "Model retry scheduled\n"),
        "[model retry started]\n" => (TextStyle::Warning, "Retrying model request\n"),
        "[done]\n" => (TextStyle::Success, "Done\n"),
        "[stopped]\n" => (TextStyle::Warning, "Stopped\n"),
        "[blocked]\n" => (TextStyle::Warning, "Blocked\n"),
        "[maximum tokens reached]\n" => (TextStyle::Warning, "Maximum tokens reached\n"),
        "[interrupted]\n" => (TextStyle::Warning, "Interrupted\n"),
        "[turn error]\n" => (TextStyle::Error, "Turn failed\n"),
        "[turn ended]\n" => (TextStyle::Warning, "Turn ended\n"),
        _ => (TextStyle::Plain, text),
    }
}

fn strip_product_terminal_controls(text: &str) -> Result<String, LiveRenderError> {
    let bytes = text.as_bytes();
    let mut output = String::new();
    output
        .try_reserve_exact(text.len())
        .map_err(|_| LiveRenderError)?;
    let mut index = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\x1b' => {
                if bytes.get(index + 1) != Some(&b'[') {
                    return Err(LiveRenderError);
                }
                index += 2;
                let mut found_final = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    index += 1;
                    if (0x40..=0x7e).contains(&byte) {
                        found_final = true;
                        break;
                    }
                }
                if !found_final {
                    return Err(LiveRenderError);
                }
            }
            b'\r' => index += 1,
            b'\n' => {
                output.push('\n');
                index += 1;
            }
            byte if byte < 0x20 || byte == 0x7f => return Err(LiveRenderError),
            _ => {
                let rest = &text[index..];
                let character = rest.chars().next().ok_or(LiveRenderError)?;
                output.push(character);
                index = index
                    .checked_add(character.len_utf8())
                    .ok_or(LiveRenderError)?;
            }
        }
    }
    Ok(output)
}

impl InteractivePresenter {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_color(false)
    }

    pub(super) fn with_color(color: bool) -> Self {
        Self {
            visible: VisibleRenderer::new(),
            active_role: None,
            theme: UiTheme::from_color_enabled(color),
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
                LivePart::TrustedOwned(text) => self.render_trusted_owned(text, &mut emit)?,
                LivePart::TrustedInline(text) => self.render_trusted_inline(text, &mut emit)?,
                LivePart::AssistantMarkup { text, .. } => {
                    self.render_untrusted(UiRole::Assistant, text, &mut emit)?
                }
                LivePart::AssistantMarkupFinish { .. } => {}
                LivePart::AssistantMarkupAbort => {}
                LivePart::Untrusted { role, text } => {
                    self.render_untrusted(*role, text, &mut emit)?
                }
                LivePart::LinearOnlyUntrusted { role, text } => {
                    self.render_untrusted(*role, text, &mut emit)?
                }
                LivePart::ApprovalPreview { text, .. } => {
                    self.render_untrusted(UiRole::Preview, text, &mut emit)?
                }
                LivePart::UntrustedStyled { text, .. } => {
                    self.render_untrusted_styled(text, &mut emit)?
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
        self.visible
            .render_trusted(self.theme.trusted_line(text), &mut emit)?;
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

    fn render_trusted_owned<E>(
        &mut self,
        text: &str,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visible.ensure_line_start(&mut emit)?;
        if !text.is_empty() {
            emit(text)?;
        }
        self.visible.force_line_start(text.ends_with('\n'));
        self.active_role = None;
        Ok(())
    }

    fn render_untrusted<E>(
        &mut self,
        role: UiRole,
        text: &str,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.active_role != Some(role) {
            self.visible.ensure_line_start(&mut emit)?;
        }
        self.visible
            .render_fragment(text, Some(self.theme.role_prefix(role)), &mut emit)?;
        self.active_role = Some(role);
        Ok(())
    }

    fn render_untrusted_styled<E>(
        &mut self,
        text: &str,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.visible.ensure_line_start(&mut emit)?;
        self.visible.render_fragment(text, None, &mut emit)?;
        self.active_role = None;
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

    pub(super) fn observe_external_line_start(&mut self) {
        self.active_role = None;
        self.visible.force_line_start(true);
    }
}

pub(super) struct LiveRenderer {
    attempt: Option<AttemptState>,
    semantic: UiProjector,
    views: ViewArchive,
    turn_end: Option<TurnEndAnchor>,
}

struct TurnEndAnchor {
    turn: crate::session::TurnId,
    seq: EventSeq,
    reason: ReceiptReason,
}

enum ReceiptReason {
    Completed,
    Aborted(UiTurnEndCancelCause),
    Blocked,
    Error { code: String },
    MaxTokens,
    Interrupted,
    Other { kind: Option<String> },
}

impl fmt::Debug for TurnEndAnchor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TurnEndAnchor")
            .field("turn", &self.turn)
            .field("seq", &self.seq)
            .finish_non_exhaustive()
    }
}

impl LiveRenderer {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::for_session(false)
    }

    pub(super) fn for_session(resumed_live_seam: bool) -> Self {
        Self {
            attempt: None,
            semantic: UiProjector::default(),
            views: ViewArchive::new(resumed_live_seam),
            turn_end: None,
        }
    }

    pub(super) fn set_context_estimate(&mut self, estimate: Option<ContextEstimate>) {
        self.views.set_context_estimate(estimate);
    }

    pub(super) fn inspect_document(&self) -> Result<DetailDocument, LiveRenderError> {
        DetailDocument::inspect(&self.views).map_err(|_| LiveRenderError)
    }

    pub(super) fn review_document(&self) -> Result<DetailDocument, LiveRenderError> {
        DetailDocument::review(&self.views).map_err(|_| LiveRenderError)
    }

    pub(super) fn freeze_joined_review(&mut self, outcome: &TurnOutcome) {
        let Some(anchor) = self.turn_end.as_ref() else {
            return;
        };
        if !anchor.matches(outcome.turn(), outcome.turn_end_seq(), outcome.reason()) {
            return;
        }
        let receipt = match WorkReceiptView::from_outcome(
            outcome,
            self.semantic.tools(),
            self.semantic.status(),
        ) {
            Ok(receipt) => receipt,
            Err(_) => {
                self.views.mark_review_join_failed(outcome.turn());
                return;
            }
        };
        let receipt = Arc::new(receipt);
        self.views
            .freeze_receipt(outcome.turn(), outcome.turn_end_seq(), Arc::clone(&receipt));
        match JoinedTurnView::from_joined_receipt(outcome, receipt, self.semantic.tools()) {
            Ok(review) => self.views.freeze_review(review),
            Err(_) => self.views.mark_review_join_failed(outcome.turn()),
        }
    }

    pub(super) fn consume(
        &mut self,
        event: CommittedUiEvent,
    ) -> Result<LiveUpdate, LiveRenderError> {
        self.views.observe(&event);
        let first_tool_result = match &event.kind {
            CommittedUiKind::ToolResult {
                turn,
                step,
                call_id,
                surface_replacement_target: None,
                ..
            } => self
                .semantic
                .tools()
                .iter()
                .find(|tool| tool.turn == *turn && tool.step == *step && tool.call_id == *call_id)
                .is_none_or(|tool| tool.is_error.is_none()),
            _ => false,
        };
        let turn_end_anchor = TurnEndAnchor::from_event(&event)?;
        if self.semantic.observe(&event.kind).is_err() {
            // A presentation allocation failure must not cancel valid Agent
            // work or erase facts that were already projected. The product
            // view keeps the safe subset and labels its details incomplete.
            self.semantic.mark_degraded();
        }
        if let Some(anchor) = turn_end_anchor {
            self.turn_end = Some(anchor);
        }
        let semantic_status = self.semantic.status();
        let _ = (
            semantic_status.last_usage,
            semantic_status.last_human_prompt_bytes,
            semantic_status.last_human_omitted_parts,
            semantic_status.retry_count,
            semantic_status.omitted_tool_facts,
            semantic_status.omitted_approval_facts,
            semantic_status.last_prune_shadowed_tokens,
            semantic_status.pending_prune_shadowed_tokens,
            semantic_status.orphan_prune_markers,
            semantic_status.conflicting_facts,
            semantic_status.compaction_usage,
            semantic_status.degraded,
        );
        let seq = event.seq;
        let _time = event.time;
        let mut lifecycle = LiveLifecycle::None;
        let mut enhanced_frame = EnhancedFrame::Same;
        let mut dock_notice = DockNoticeUpdate::Keep;
        let frame = match event.kind {
            CommittedUiKind::TurnStart { turn } => {
                let _ = turn;
                self.turn_end = None;
                enhanced_frame = EnhancedFrame::Suppress;
                Some(LiveFrame::trusted("[working; press Ctrl+C to stop]\n")?)
            }
            CommittedUiKind::TurnEnd { turn, reason } => {
                self.attempt = None;
                lifecycle = LiveLifecycle::TurnEnded { turn };
                enhanced_frame = EnhancedFrame::Replace(LiveFrame::markup_abort()?);
                dock_notice = DockNoticeUpdate::Clear;
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
                Some(LiveFrame::markup_abort()?)
            }
            CommittedUiKind::UserMessage { source, content } => {
                let _ = (source, content);
                // Canonical-mode Phase 9 input is still terminal-echoed. The
                // long-lived Phase 11 composer will render this semantic fact
                // after echo is disabled.
                None
            }
            CommittedUiKind::AssistantTextDelta {
                turn,
                step,
                index,
                text,
            } => {
                self.retain_delta(turn, step, index, UiAssistantBlockKind::Text, seq, &text)?;
                LiveFrame::from_parts(single_assistant_markup(turn, step, index, text)?)
            }
            CommittedUiKind::AssistantReasoningDelta {
                turn,
                step,
                index,
                text,
            } => {
                enhanced_frame = EnhancedFrame::Suppress;
                self.retain_delta(
                    turn,
                    step,
                    index,
                    UiAssistantBlockKind::Reasoning,
                    seq,
                    &text,
                )?;
                LiveFrame::from_parts(single_untrusted(UiRole::Reasoning, text)?)
            }
            CommittedUiKind::UsageSample { turn, step, usage } => {
                let _ = (turn, step, usage);
                None
            }
            CommittedUiKind::AssistantMessage {
                turn,
                step,
                content,
                sources,
                provider,
                model,
                usage,
            } => {
                let _ = (provider, model, usage);
                self.final_frame(turn, step, content, &sources)?
            }
            CommittedUiKind::ToolRequested {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => {
                enhanced_frame = EnhancedFrame::Suppress;
                let semantic = self.semantic.tools().iter().find(|activity| {
                    activity.turn == turn && activity.step == step && activity.call_id == call_id
                });
                if let Some(semantic) = semantic {
                    let _ = (
                        &semantic.name,
                        semantic.origin,
                        &semantic.summary,
                        semantic.state,
                        semantic.is_error,
                        &semantic.failure_code,
                        semantic.payload_omitted,
                        semantic.result_bytes,
                        semantic.meta_bytes,
                        semantic.committed_effect,
                        semantic.started_process,
                        semantic.shell_exit_code,
                        &semantic.shell_signal,
                        semantic.shell_timed_out,
                    );
                    dock_notice = DockNoticeUpdate::Set(tool_activity_notice(semantic)?);
                }
                let _ = &arguments;
                let mut parts = try_parts(5)?;
                parts.push(LivePart::TrustedLine("[tool requested]\n"));
                parts.push(LivePart::Untrusted {
                    role: UiRole::Tool,
                    text: name.into_display(),
                });
                parts.push(LivePart::TrustedInline("\n"));
                parts.push(LivePart::Untrusted {
                    role: UiRole::Arguments,
                    text: "arguments omitted".to_owned(),
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
                content,
                meta,
                surface_replacement_target,
            } => {
                let _ = (turn, step, &content, &meta);
                if surface_replacement_target.is_some() {
                    // A prune replacement rewrites old surface payload. It is
                    // not a second tool execution and must not appear twice.
                    return Ok(LiveUpdate {
                        frame: None,
                        lifecycle: LiveLifecycle::None,
                        enhanced_frame: EnhancedFrame::Suppress,
                        dock_notice: DockNoticeUpdate::Keep,
                    });
                }
                dock_notice = self
                    .semantic
                    .tools()
                    .iter()
                    .rev()
                    .find(|tool| {
                        tool.turn == turn
                            && tool.is_error.is_none()
                            && matches!(
                                tool.state,
                                crate::tui::projector::ToolActivityState::Preparing
                                    | crate::tui::projector::ToolActivityState::AwaitingApproval
                                    | crate::tui::projector::ToolActivityState::Allowed
                            )
                    })
                    .map(tool_activity_notice)
                    .transpose()?
                    .map_or(DockNoticeUpdate::Clear, DockNoticeUpdate::Set);
                if first_tool_result {
                    let activity = self.semantic.tools().iter().find(|activity| {
                        activity.turn == turn
                            && activity.step == step
                            && activity.call_id == call_id
                    });
                    enhanced_frame = EnhancedFrame::Replace(if let Some(activity) = activity {
                        tool_card_frame(
                            &ToolCardView::from_activity(activity).map_err(|_| LiveRenderError)?,
                        )?
                    } else {
                        generic_tool_result_frame(
                            is_error,
                            failure.as_ref().map(|failure| failure.code.as_str()),
                        )?
                    });
                } else {
                    enhanced_frame = EnhancedFrame::Suppress;
                }
                let mut parts = try_parts(5)?;
                parts.push(LivePart::TrustedLine(if is_error {
                    "[tool result: error]\n"
                } else {
                    "[tool result: success]\n"
                }));
                if let Some(failure) = failure {
                    parts.push(LivePart::Untrusted {
                        role: UiRole::Error,
                        text: failure.name,
                    });
                    parts.push(LivePart::TrustedInline(" / "));
                    parts.push(LivePart::Untrusted {
                        role: UiRole::Error,
                        text: failure.code,
                    });
                    parts.push(LivePart::TrustedInline("\n"));
                }
                LiveFrame::from_parts(parts)
            }
            CommittedUiKind::RequestContextChanged {
                provider,
                model,
                context_window,
            } => {
                if let Some(context) = self.semantic.context() {
                    let _ = (&context.provider, &context.model, context.window);
                }
                let _ = (provider, model, context_window);
                None
            }
            CommittedUiKind::CompactionStarted { .. }
            | CommittedUiKind::CompactionSummarized { .. }
            | CommittedUiKind::CompactionEnded { .. }
            | CommittedUiKind::CompactionPruneMarked { .. } => {
                if let Some(compaction) = self.semantic.compaction() {
                    let _ = (
                        &compaction.id,
                        compaction.phase,
                        compaction.shadowed_tokens,
                        &compaction.error_code,
                    );
                }
                None
            }
            CommittedUiKind::ApprovalAsked {
                id,
                tool_name,
                call_id,
                reason,
            } => {
                lifecycle = LiveLifecycle::ApprovalAsked {
                    id: id.into_display(),
                    tool_name: tool_name.into_display(),
                    call_id: call_id.map(UiIdentity::into_display),
                    reason,
                };
                None
            }
            CommittedUiKind::ApprovalDecided { id, outcome } => {
                lifecycle = LiveLifecycle::ApprovalDecided {
                    id: id.into_display(),
                    outcome,
                };
                enhanced_frame = EnhancedFrame::Suppress;
                dock_notice = match outcome {
                    ApprovalOutcome::AllowedOnce => self
                        .semantic
                        .tools()
                        .iter()
                        .rev()
                        .find(|tool| {
                            tool.state == crate::tui::projector::ToolActivityState::Allowed
                                && tool.is_error.is_none()
                        })
                        .map(tool_approved_notice)
                        .transpose()?
                        .map_or(DockNoticeUpdate::Keep, DockNoticeUpdate::Set),
                    ApprovalOutcome::Rejected => {
                        DockNoticeUpdate::Set("Rejected; recording result".to_owned())
                    }
                    ApprovalOutcome::Cancelled => {
                        DockNoticeUpdate::Set("Cancelled; recording result".to_owned())
                    }
                    ApprovalOutcome::Unavailable => {
                        DockNoticeUpdate::Set("Approval unavailable".to_owned())
                    }
                };
                Some(LiveFrame::trusted(match outcome {
                    ApprovalOutcome::AllowedOnce => "[approval: allowed once]\n",
                    ApprovalOutcome::Rejected => "[approval: rejected]\n",
                    ApprovalOutcome::Cancelled => "[approval: cancelled]\n",
                    ApprovalOutcome::Unavailable => "[approval: unavailable]\n",
                })?)
            }
            CommittedUiKind::RetryScheduled {
                retry_id,
                retry,
                provider,
                delay_ms,
                max_retries,
                failure_code,
                failure_message,
            } => {
                let _ = (
                    retry_id,
                    retry,
                    provider,
                    delay_ms,
                    max_retries,
                    failure_code,
                    failure_message,
                );
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
        Ok(LiveUpdate {
            frame,
            lifecycle,
            enhanced_frame,
            dock_notice,
        })
    }

    pub(super) fn receipt_frame(
        &self,
        outcome: &TurnOutcome,
    ) -> Result<LiveFrame, LiveRenderError> {
        let anchor = self.turn_end.as_ref().ok_or(LiveRenderError)?;
        if !anchor.matches(outcome.turn(), outcome.turn_end_seq(), outcome.reason()) {
            return Err(LiveRenderError);
        }
        let mut parts = try_parts(self.semantic.tools().len().saturating_mul(5) + 7)?;
        for tool in self.semantic.tools().iter().filter(|tool| {
            tool.turn == outcome.turn()
                && tool.is_error.is_none()
                && matches!(
                    tool.state,
                    crate::tui::projector::ToolActivityState::Denied
                        | crate::tui::projector::ToolActivityState::Cancelled
                        | crate::tui::projector::ToolActivityState::Unavailable
                        | crate::tui::projector::ToolActivityState::OutcomeUnknown
                )
        }) {
            append_tool_card_parts(
                &mut parts,
                &ToolCardView::from_activity(tool).map_err(|_| LiveRenderError)?,
            )?;
        }
        let receipt = self
            .views
            .joined_receipt(outcome.turn(), outcome.turn_end_seq())
            .ok_or(LiveRenderError)?;
        append_receipt_parts(&mut parts, receipt)?;
        Ok(LiveFrame { parts })
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
                let mut parts = try_parts(4)?;
                parts.push(LivePart::AssistantMarkupAbort);
                parts.push(LivePart::TrustedLine(
                    "[final answer restated; streaming comparison limit reached]\n",
                ));
                if !text.is_empty() {
                    parts.push(LivePart::AssistantMarkup {
                        key: MarkupStreamKey {
                            turn,
                            step,
                            block: u64::MAX,
                        },
                        text,
                    });
                }
                parts.push(LivePart::AssistantMarkupFinish {
                    key: Some(MarkupStreamKey {
                        turn,
                        step,
                        block: u64::MAX,
                    }),
                });
                Ok(LiveFrame::from_parts(parts))
            }
            UiAssistantContent::Indexed(blocks) if state_degraded => {
                let mut parts = try_parts(blocks.len().saturating_mul(2).saturating_add(2))?;
                parts.push(LivePart::AssistantMarkupAbort);
                parts.push(LivePart::TrustedLine(
                    "[final answer restated; streaming comparison limit reached]\n",
                ));
                for block in blocks {
                    push_authoritative_block(
                        &mut parts,
                        turn,
                        step,
                        block.index.into(),
                        block.kind,
                        block.text,
                    )?;
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
                let mut parts = try_parts(blocks.len().saturating_mul(2).saturating_add(2))?;
                if has_mismatch {
                    parts.push(LivePart::AssistantMarkupAbort);
                    parts.push(LivePart::TrustedLine("[final answer corrected]\n"));
                    for block in blocks {
                        push_authoritative_block(
                            &mut parts,
                            turn,
                            step,
                            block.index.into(),
                            block.kind,
                            block.text,
                        )?;
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
                        Comparison::Exact => {
                            if block.kind == UiAssistantBlockKind::Text {
                                parts.push(LivePart::AssistantMarkupFinish {
                                    key: Some(MarkupStreamKey {
                                        turn,
                                        step,
                                        block: block.index.into(),
                                    }),
                                });
                            }
                        }
                        Comparison::Prefix(bytes) => {
                            if bytes < block.text.len() {
                                block.text.drain(..bytes);
                                push_authoritative_block(
                                    &mut parts,
                                    turn,
                                    step,
                                    block.index.into(),
                                    block.kind,
                                    block.text,
                                )?;
                            } else if block.kind == UiAssistantBlockKind::Text {
                                parts.push(LivePart::AssistantMarkupFinish {
                                    key: Some(MarkupStreamKey {
                                        turn,
                                        step,
                                        block: block.index.into(),
                                    }),
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

fn push_authoritative_block(
    parts: &mut Vec<LivePart>,
    turn: crate::session::TurnId,
    step: crate::session::StepId,
    block: u64,
    kind: UiAssistantBlockKind,
    text: String,
) -> Result<(), LiveRenderError> {
    match kind {
        UiAssistantBlockKind::Text => {
            if !text.is_empty() {
                parts.push(LivePart::AssistantMarkup {
                    key: MarkupStreamKey { turn, step, block },
                    text,
                });
            }
            parts.push(LivePart::AssistantMarkupFinish {
                key: Some(MarkupStreamKey { turn, step, block }),
            });
        }
        UiAssistantBlockKind::Reasoning => {
            if !text.is_empty() {
                parts.push(LivePart::Untrusted {
                    role: UiRole::Reasoning,
                    text,
                });
            }
        }
    }
    Ok(())
}

fn single_untrusted(role: UiRole, text: String) -> Result<Vec<LivePart>, LiveRenderError> {
    let mut parts = try_parts(1)?;
    parts.push(LivePart::Untrusted { role, text });
    Ok(parts)
}

fn single_assistant_markup(
    turn: crate::session::TurnId,
    step: crate::session::StepId,
    block: u64,
    text: String,
) -> Result<Vec<LivePart>, LiveRenderError> {
    let mut parts = try_parts(1)?;
    parts.push(LivePart::AssistantMarkup {
        key: MarkupStreamKey { turn, step, block },
        text,
    });
    Ok(parts)
}

fn push_untrusted_line(
    parts: &mut Vec<LivePart>,
    role: UiRole,
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

fn push_approval_metadata_line(
    parts: &mut Vec<LivePart>,
    role: UiRole,
    value: &str,
    linear_only: bool,
) -> Result<(), LiveRenderError> {
    if !linear_only {
        return push_untrusted_line(parts, role, value);
    }
    let mut text = String::new();
    text.try_reserve_exact(value.len())
        .map_err(|_| LiveRenderError)?;
    text.push_str(value);
    parts.push(LivePart::LinearOnlyUntrusted { role, text });
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

impl TurnEndAnchor {
    fn from_event(event: &CommittedUiEvent) -> Result<Option<Self>, LiveRenderError> {
        let CommittedUiKind::TurnEnd { turn, reason } = &event.kind else {
            return Ok(None);
        };
        Ok(Some(Self {
            turn: *turn,
            seq: event.seq,
            reason: ReceiptReason::from_ui(reason)?,
        }))
    }

    fn matches(&self, turn: crate::session::TurnId, seq: EventSeq, reason: &TurnEndReason) -> bool {
        self.turn == turn && self.seq == seq && self.reason.matches(reason)
    }
}

impl ReceiptReason {
    fn from_ui(reason: &UiTurnEndReason) -> Result<Self, LiveRenderError> {
        Ok(match reason {
            UiTurnEndReason::Completed => Self::Completed,
            UiTurnEndReason::Aborted { cause } => Self::Aborted(*cause),
            UiTurnEndReason::Blocked => Self::Blocked,
            UiTurnEndReason::Error { code, .. } => Self::Error {
                code: copy_frame_text(code)?,
            },
            UiTurnEndReason::MaxTokens => Self::MaxTokens,
            UiTurnEndReason::Interrupted => Self::Interrupted,
            UiTurnEndReason::Other { kind } => Self::Other {
                kind: kind.as_deref().map(copy_frame_text).transpose()?,
            },
        })
    }

    fn matches(&self, reason: &TurnEndReason) -> bool {
        match (self, reason) {
            (Self::Completed, TurnEndReason::Completed)
            | (Self::Blocked, TurnEndReason::Blocked)
            | (Self::MaxTokens, TurnEndReason::MaxTokens)
            | (Self::Interrupted, TurnEndReason::Interrupted) => true,
            (Self::Aborted(left), TurnEndReason::Aborted { reason: right }) => {
                matches!(
                    (left, right),
                    (UiTurnEndCancelCause::User, TurnEndCancelCause::User)
                        | (UiTurnEndCancelCause::Parent, TurnEndCancelCause::Parent)
                        | (UiTurnEndCancelCause::Hook, TurnEndCancelCause::Hook { .. })
                        | (UiTurnEndCancelCause::Disposed, TurnEndCancelCause::Disposed)
                        | (UiTurnEndCancelCause::Legacy, TurnEndCancelCause::Legacy)
                )
            }
            (Self::Error { code: left_code }, TurnEndReason::Error { error }) => {
                left_code == error.code()
            }
            (
                Self::Other { kind: left_kind },
                TurnEndReason::Other {
                    kind: right_kind, ..
                },
            ) => left_kind.as_deref() == right_kind.as_deref(),
            _ => false,
        }
    }
}

fn tool_card_frame(view: &ToolCardView) -> Result<LiveFrame, LiveRenderError> {
    let mut parts = try_parts(5)?;
    append_tool_card_parts(&mut parts, view)?;
    Ok(LiveFrame { parts })
}

fn tool_activity_notice(
    tool: &crate::tui::projector::ToolActivity,
) -> Result<String, LiveRenderError> {
    let label = match tool.name.as_str() {
        "list" => "Requested  List",
        "glob" => "Requested  Glob",
        "grep" => "Requested  Search",
        "read" => "Requested  Read",
        "apply_patch" => "Requested  Patch",
        "bash" => "Requested  Command",
        _ => "Tool requested",
    };
    let mut notice = String::new();
    notice
        .try_reserve_exact(MAX_DOCK_NOTICE_BYTES)
        .map_err(|_| LiveRenderError)?;
    notice.push_str(label);
    if let Some(summary) = tool.summary.as_deref() {
        notice.push_str("  ");
        let remaining = MAX_DOCK_NOTICE_BYTES.saturating_sub(notice.len());
        if summary.len() <= remaining {
            notice.push_str(summary);
        } else {
            let mut end = remaining.saturating_sub("...".len());
            while end != 0 && !summary.is_char_boundary(end) {
                end -= 1;
            }
            notice.push_str(&summary[..end]);
            notice.push_str("...");
        }
    }
    Ok(notice)
}

fn tool_approved_notice(
    tool: &crate::tui::projector::ToolActivity,
) -> Result<String, LiveRenderError> {
    let label = match tool.name.as_str() {
        "list" => "List",
        "glob" => "Glob",
        "grep" => "Search",
        "read" => "Read",
        "apply_patch" => "Patch",
        "bash" => "Command",
        _ => "Tool",
    };
    copy_frame_text(&format!("Approved; awaiting result  {label}"))
}

fn generic_tool_result_frame(
    is_error: bool,
    failure_code: Option<&str>,
) -> Result<LiveFrame, LiveRenderError> {
    let mut parts = try_parts(4)?;
    let has_error = is_error || failure_code.is_some();
    parts.push(LivePart::UntrustedStyled {
        style: if has_error {
            TextStyle::Error
        } else {
            TextStyle::Accent
        },
        text: if has_error {
            "Tool result recorded with an error".to_owned()
        } else {
            "Tool result recorded".to_owned()
        },
    });
    parts.push(LivePart::TrustedInline("\n"));
    parts.push(LivePart::UntrustedStyled {
        style: TextStyle::Muted,
        text: copy_frame_text(failure_code.unwrap_or("details incomplete"))?,
    });
    parts.push(LivePart::TrustedInline("\n\n"));
    Ok(LiveFrame { parts })
}

fn append_tool_card_parts(
    parts: &mut Vec<LivePart>,
    view: &ToolCardView,
) -> Result<(), LiveRenderError> {
    parts.push(LivePart::UntrustedStyled {
        style: style_for_tone(view.tone()),
        text: copy_frame_text(view.headline())?,
    });
    parts.push(LivePart::TrustedInline("\n"));
    if let Some(detail) = view.detail() {
        let mut indented = String::new();
        indented
            .try_reserve_exact(detail.len().saturating_add(2))
            .map_err(|_| LiveRenderError)?;
        indented.push_str("  ");
        indented.push_str(detail);
        parts.push(LivePart::UntrustedStyled {
            style: TextStyle::Muted,
            text: indented,
        });
        parts.push(LivePart::TrustedInline("\n"));
    }
    parts.push(LivePart::TrustedInline("\n"));
    Ok(())
}

fn append_receipt_parts(
    parts: &mut Vec<LivePart>,
    view: &WorkReceiptView,
) -> Result<(), LiveRenderError> {
    parts.push(LivePart::UntrustedStyled {
        style: style_for_tone(view.tone()),
        text: copy_frame_text(view.headline())?,
    });
    parts.push(LivePart::TrustedInline("\n"));
    for line in [view.counters(), view.effects()].into_iter().flatten() {
        let mut indented = String::new();
        indented
            .try_reserve_exact(line.len().saturating_add(2))
            .map_err(|_| LiveRenderError)?;
        indented.push_str("  ");
        indented.push_str(line);
        parts.push(LivePart::UntrustedStyled {
            style: TextStyle::Muted,
            text: indented,
        });
        parts.push(LivePart::TrustedInline("\n"));
    }
    parts.push(LivePart::TrustedInline("\n"));
    Ok(())
}

fn style_for_tone(tone: TimelineTone) -> TextStyle {
    match tone {
        TimelineTone::Accent => TextStyle::Accent,
        TimelineTone::Positive => TextStyle::Success,
        TimelineTone::Caution => TextStyle::Warning,
        TimelineTone::Negative => TextStyle::Error,
    }
}

fn copy_frame_text(value: &str) -> Result<String, LiveRenderError> {
    let mut copy = String::new();
    copy.try_reserve_exact(value.len())
        .map_err(|_| LiveRenderError)?;
    copy.push_str(value);
    Ok(copy)
}

fn turn_end_frame(reason: UiTurnEndReason) -> Result<LiveFrame, LiveRenderError> {
    let frame = match reason {
        UiTurnEndReason::Completed => LiveFrame::trusted("[done]\n")?,
        UiTurnEndReason::Aborted { cause } => {
            let _ = cause;
            LiveFrame::trusted("[stopped]\n")?
        }
        UiTurnEndReason::Blocked => LiveFrame::trusted("[blocked]\n")?,
        UiTurnEndReason::MaxTokens => LiveFrame::trusted("[maximum tokens reached]\n")?,
        UiTurnEndReason::Interrupted => LiveFrame::trusted("[interrupted]\n")?,
        UiTurnEndReason::Error { code, message } => {
            let mut parts = try_parts(5)?;
            parts.push(LivePart::TrustedLine("[turn error]\n"));
            parts.push(LivePart::Untrusted {
                role: UiRole::Error,
                text: code,
            });
            parts.push(LivePart::TrustedInline(": "));
            parts.push(LivePart::Untrusted {
                role: UiRole::Error,
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
                    role: UiRole::Error,
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
    use std::sync::Arc;

    use crate::agent::{
        ApprovalDiffRowKind, ApprovalPatchOperation, ApprovalPreviewKind, ApprovalPrompt,
        ApprovalRequest, TurnOutcome,
    };
    use crate::model::{CallId, LlmFailure};
    use crate::session::{
        ApprovalRequestId, CommittedUiEvent, CommittedUiKind, EventSeq, RetryNumber,
        SourceSeqBitmap, StepId, TurnEndReason, TurnId, UiAssistantBlock, UiAssistantBlockKind,
        UiAssistantContent, UiIdentity, UiOpaquePayload, UiToolFailure, UiTurnEndReason,
        UnixMillis,
    };
    use crate::tui::presentation::{PresentedItem, TextStyle};

    use super::{
        AttemptState, EnhancedPresenter, FRAME_OUTPUT_CHUNK_BYTES, InteractivePresenter, LiveFrame,
        LivePart, LiveRenderer, MAX_ATTEMPT_BLOCKS, MAX_ATTEMPT_TEXT_BYTES, TurnEndAnchor,
    };

    fn event(seq: u64, kind: CommittedUiKind) -> CommittedUiEvent {
        CommittedUiEvent {
            seq: EventSeq::new(seq).unwrap(),
            time: UnixMillis::new(1).unwrap(),
            kind,
        }
    }

    fn identity(value: &str) -> UiIdentity {
        UiIdentity::from_text_for_test(value)
    }

    fn presented_text(presentation: &super::PreparedPresentation) -> String {
        let mut text = String::new();
        for item in presentation.chunk().items() {
            match item {
                PresentedItem::Text { text: value, .. } => text.push_str(value),
                PresentedItem::LineFeed => text.push('\n'),
            }
        }
        text
    }

    fn presented_text_with_style(
        presentation: &super::PreparedPresentation,
        expected: TextStyle,
    ) -> String {
        let mut text = String::new();
        for item in presentation.chunk().items() {
            if let PresentedItem::Text { style, text: value } = item {
                if *style == expected {
                    text.push_str(value);
                }
            }
        }
        text
    }

    #[test]
    fn enhanced_presentation_state_changes_only_after_commit() {
        let mut presenter = EnhancedPresenter::new();
        let key = super::MarkupStreamKey {
            turn: TurnId::new(1).unwrap(),
            step: StepId::new(1).unwrap(),
            block: 0,
        };
        let frame = || LiveFrame {
            parts: vec![LivePart::AssistantMarkup {
                key,
                text: "before `secret".to_owned(),
            }],
        };
        let abandoned = presenter.prepare(frame()).unwrap();
        assert_eq!(presented_text(&abandoned), "DSH\nbefore ");

        let retry = presenter.prepare(frame()).unwrap();
        assert_eq!(presented_text(&retry), "DSH\nbefore ");
        presenter.commit(retry);

        let continuation = presenter
            .prepare(LiveFrame {
                parts: vec![LivePart::AssistantMarkup {
                    key,
                    text: " value`".to_owned(),
                }],
            })
            .unwrap();
        assert_eq!(presented_text(&continuation), "`secret value`");
    }

    fn enhanced_text(frame: LiveFrame) -> String {
        let presenter = EnhancedPresenter::new();
        let prepared = presenter.prepare(frame).unwrap();
        presented_text(&prepared)
    }

    #[test]
    fn enhanced_multiline_text_keeps_structural_line_feeds() {
        let key = super::MarkupStreamKey {
            turn: TurnId::new(1).unwrap(),
            step: StepId::new(1).unwrap(),
            block: 0,
        };
        let frame = LiveFrame {
            parts: vec![
                LivePart::AssistantMarkup {
                    key,
                    text: "first\nsecond\u{1b}\u{202e}\u{fff0}\u{e0000}".to_owned(),
                },
                LivePart::AssistantMarkupFinish { key: Some(key) },
            ],
        };
        let output = enhanced_text(frame);
        assert!(output.contains("DSH\nfirst\nsecond"));
        assert!(output.contains("\\u{1b}"));
        assert!(output.contains("\\u{202e}"));
        assert!(output.contains("\\u{fff0}"));
        assert!(output.contains("\\u{e0000}"));
        assert!(!output.contains('\u{202e}'));
        assert!(!output.contains("first\\nsecond"));
    }

    #[test]
    fn exact_final_without_a_suffix_flushes_pending_markup() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = EnhancedPresenter::new();

        let mut delta = renderer
            .consume(event(
                1,
                CommittedUiKind::AssistantTextDelta {
                    turn,
                    step,
                    index: 0,
                    text: "`unfinished".to_owned(),
                },
            ))
            .unwrap();
        let prepared = presenter.prepare(delta.take_frame(true).unwrap()).unwrap();
        assert_eq!(presented_text(&prepared), "DSH\n");
        presenter.commit(prepared);

        let mut final_message = renderer
            .consume(event(
                2,
                CommittedUiKind::AssistantMessage {
                    turn,
                    step,
                    content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                        index: 0,
                        kind: UiAssistantBlockKind::Text,
                        text: "`unfinished".to_owned(),
                    }]),
                    sources: SourceSeqBitmap::from_sources(&[EventSeq::new(1).unwrap()]).unwrap(),
                    provider: identity("mock"),
                    model: identity("mock-model"),
                    usage: None,
                },
            ))
            .unwrap();
        let prepared = presenter
            .prepare(final_message.take_frame(true).unwrap())
            .unwrap();
        assert_eq!(presented_text(&prepared), "`unfinished");
    }

    #[test]
    fn step_end_aborts_an_eof_closer_as_plain_and_resets_the_next_stream() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = EnhancedPresenter::new();
        let mut delta = renderer
            .consume(event(
                1,
                CommittedUiKind::AssistantTextDelta {
                    turn,
                    step,
                    index: 0,
                    text: "```rust\nprivate\n```".to_owned(),
                },
            ))
            .unwrap();
        let prepared = presenter.prepare(delta.take_frame(true).unwrap()).unwrap();
        assert_eq!(presented_text(&prepared), "DSH\n");
        presenter.commit(prepared);

        let mut end = renderer
            .consume(event(2, CommittedUiKind::StepEnd { turn, step }))
            .unwrap();
        let prepared = presenter.prepare(end.take_frame(true).unwrap()).unwrap();
        assert_eq!(presented_text(&prepared), "```rust\nprivate\n```");
        assert!(presented_text_with_style(&prepared, TextStyle::Code).is_empty());
        presenter.commit(prepared);

        let next_step = StepId::new(2).unwrap();
        let _ = renderer
            .consume(event(
                3,
                CommittedUiKind::StepStart {
                    turn,
                    step: next_step,
                },
            ))
            .unwrap();
        let mut next = renderer
            .consume(event(
                4,
                CommittedUiKind::AssistantTextDelta {
                    turn,
                    step: next_step,
                    index: 0,
                    text: "after".to_owned(),
                },
            ))
            .unwrap();
        let prepared = presenter.prepare(next.take_frame(true).unwrap()).unwrap();
        assert_eq!(presented_text(&prepared), "\nDSH\nafter");
    }

    #[test]
    fn corrected_final_aborts_old_fence_and_styles_only_the_authoritative_answer() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = EnhancedPresenter::new();

        let mut delta = renderer
            .consume(event(
                1,
                CommittedUiKind::AssistantTextDelta {
                    turn,
                    step,
                    index: 0,
                    text: "```rust\nold-body\n```".to_owned(),
                },
            ))
            .unwrap();
        let prepared = presenter.prepare(delta.take_frame(true).unwrap()).unwrap();
        assert_eq!(presented_text(&prepared), "DSH\n");
        presenter.commit(prepared);

        let mut final_message = renderer
            .consume(event(
                2,
                CommittedUiKind::AssistantMessage {
                    turn,
                    step,
                    content: UiAssistantContent::Indexed(vec![UiAssistantBlock {
                        index: 0,
                        kind: UiAssistantBlockKind::Text,
                        text: "```rust\nnew-body\n```\n".to_owned(),
                    }]),
                    sources: SourceSeqBitmap::from_sources(&[EventSeq::new(1).unwrap()]).unwrap(),
                    provider: identity("mock"),
                    model: identity("mock-model"),
                    usage: None,
                },
            ))
            .unwrap();
        let prepared = presenter
            .prepare(final_message.take_frame(true).unwrap())
            .unwrap();
        let text = presented_text(&prepared);
        assert_eq!(text.matches("old-body").count(), 1);
        assert_eq!(text.matches("new-body").count(), 1);
        let code = presented_text_with_style(&prepared, TextStyle::Code);
        assert!(!code.contains("old-body"));
        assert!(code.contains("new-body"));
    }

    #[test]
    fn retry_aborts_partial_markup_as_plain_and_does_not_leak_style_state() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut presenter = EnhancedPresenter::new();

        let mut delta = renderer
            .consume(event(
                1,
                CommittedUiKind::AssistantTextDelta {
                    turn,
                    step,
                    index: 0,
                    text: "```rust\nretry-partial\n```".to_owned(),
                },
            ))
            .unwrap();
        let prepared = presenter.prepare(delta.take_frame(true).unwrap()).unwrap();
        presenter.commit(prepared);

        let mut retry = renderer
            .consume(event(
                2,
                CommittedUiKind::RetryScheduled {
                    retry_id: identity("retry-1"),
                    retry: RetryNumber::new(1).unwrap(),
                    provider: identity("mock"),
                    delay_ms: 10.0,
                    max_retries: Some(RetryNumber::new(2).unwrap()),
                    failure_code: "RETRY".to_owned(),
                    failure_message: "temporary".to_owned(),
                },
            ))
            .unwrap();
        let prepared = presenter.prepare(retry.take_frame(true).unwrap()).unwrap();
        assert!(presented_text(&prepared).contains("retry-partial"));
        assert!(presented_text_with_style(&prepared, TextStyle::Code).is_empty());
        presenter.commit(prepared);

        let next_key = super::MarkupStreamKey {
            turn,
            step,
            block: 1,
        };
        let prepared = presenter
            .prepare(LiveFrame {
                parts: vec![
                    LivePart::AssistantMarkup {
                        key: next_key,
                        text: "after retry".to_owned(),
                    },
                    LivePart::AssistantMarkupFinish {
                        key: Some(next_key),
                    },
                ],
            })
            .unwrap();
        assert!(presented_text(&prepared).ends_with("DSH\nafter retry"));
    }

    #[test]
    fn hostile_assistant_display_expansion_is_omitted_once_and_the_next_stream_recovers() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let first = super::MarkupStreamKey {
            turn,
            step,
            block: 0,
        };
        let next = super::MarkupStreamKey {
            turn,
            step,
            block: 1,
        };
        let mut presenter = EnhancedPresenter::new();

        for hostile in ["x\n".repeat(50_000), "\u{202e}".repeat(100_000)] {
            let prepared = presenter
                .prepare(LiveFrame {
                    parts: vec![
                        LivePart::AssistantMarkup {
                            key: first,
                            text: hostile,
                        },
                        LivePart::AssistantMarkupFinish { key: Some(first) },
                    ],
                })
                .unwrap();
            let text = presented_text(&prepared);
            assert_eq!(
                text.matches("[assistant display omitted: presentation limit exceeded]")
                    .count(),
                1
            );
            assert!(!text.contains('\u{202e}'));
            presenter.commit(prepared);

            let prepared = presenter
                .prepare(LiveFrame {
                    parts: vec![
                        LivePart::AssistantMarkup {
                            key: next,
                            text: "after".to_owned(),
                        },
                        LivePart::AssistantMarkupFinish { key: Some(next) },
                    ],
                })
                .unwrap();
            assert!(presented_text(&prepared).ends_with("DSH\nafter"));
            presenter.commit(prepared);
        }
    }

    #[test]
    fn a_finish_for_an_old_block_cannot_flush_the_active_block() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let old = super::MarkupStreamKey {
            turn,
            step,
            block: 0,
        };
        let active = super::MarkupStreamKey {
            turn,
            step,
            block: 1,
        };
        let mut presenter = EnhancedPresenter::new();
        let prepared = presenter
            .prepare(LiveFrame {
                parts: vec![LivePart::AssistantMarkup {
                    key: active,
                    text: "`private".to_owned(),
                }],
            })
            .unwrap();
        presenter.commit(prepared);

        let stale = presenter
            .prepare(LiveFrame {
                parts: vec![LivePart::AssistantMarkupFinish { key: Some(old) }],
            })
            .unwrap();
        assert!(presented_text(&stale).is_empty());
        presenter.commit(stale);

        let current = presenter
            .prepare(LiveFrame {
                parts: vec![LivePart::AssistantMarkupFinish { key: Some(active) }],
            })
            .unwrap();
        assert_eq!(presented_text(&current), "`private");
    }

    #[test]
    fn enhanced_tools_emit_only_the_first_final_card() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let request = CommittedUiKind::ToolRequested {
            turn,
            step,
            call_id: identity("call-read"),
            name: identity("read"),
            arguments: UiOpaquePayload::from_text_for_test(r#"{"file_path":"src/main.rs"}"#),
        };
        let mut update = renderer.consume(event(1, request)).unwrap();
        let mut notice = None;
        assert!(update.apply_dock_notice(&mut notice));
        assert_eq!(notice.as_deref(), Some("Requested  Read  src/main.rs"));
        assert!(update.take_frame(true).is_none());
        assert!(update.frame.is_none());

        let result = || CommittedUiKind::ToolResult {
            turn,
            step,
            call_id: identity("call-read"),
            is_error: false,
            failure: None,
            content: UiOpaquePayload::from_text_for_test("secret result body"),
            meta: UiOpaquePayload::from_text_for_test("{}"),
            surface_replacement_target: None,
        };
        let mut first = renderer.consume(event(2, result())).unwrap();
        assert!(first.apply_dock_notice(&mut notice));
        assert!(notice.is_none());
        let output = enhanced_text(first.take_frame(true).unwrap());
        assert!(output.contains("Completed  Read"));
        assert!(output.contains("src/main.rs"));
        assert!(!output.contains("secret result body"));

        let mut duplicate = renderer.consume(event(3, result())).unwrap();
        assert!(duplicate.take_frame(true).is_none());
    }

    #[test]
    fn enhanced_tool_capacity_one_over_degrades_to_a_generic_card() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        for index in 0..256_u16 {
            renderer
                .consume(event(
                    u64::from(index),
                    CommittedUiKind::ToolRequested {
                        turn,
                        step,
                        call_id: identity(&format!("call-{index}")),
                        name: identity("read"),
                        arguments: UiOpaquePayload::from_text_for_test(
                            r#"{"file_path":"src/lib.rs"}"#,
                        ),
                    },
                ))
                .unwrap();
        }
        renderer
            .consume(event(
                256,
                CommittedUiKind::ToolRequested {
                    turn,
                    step,
                    call_id: identity("call-over"),
                    name: identity("read"),
                    arguments: UiOpaquePayload::from_text_for_test(
                        r#"{"file_path":"src/over.rs"}"#,
                    ),
                },
            ))
            .unwrap();
        let mut update = renderer
            .consume(event(
                257,
                CommittedUiKind::ToolResult {
                    turn,
                    step,
                    call_id: identity("call-over"),
                    is_error: false,
                    failure: None,
                    content: UiOpaquePayload::from_text_for_test("secret"),
                    meta: UiOpaquePayload::from_text_for_test("{}"),
                    surface_replacement_target: None,
                },
            ))
            .unwrap();
        let output = enhanced_text(update.take_frame(true).unwrap());
        assert!(output.contains("Tool result recorded"));
        assert!(output.contains("details incomplete"));
        assert!(!output.contains("secret"));

        let end_seq = EventSeq::new(258).unwrap();
        renderer
            .consume(event(
                end_seq.get(),
                CommittedUiKind::TurnEnd {
                    turn,
                    reason: UiTurnEndReason::Completed,
                },
            ))
            .unwrap();
        let outcome = TurnOutcome::completed_for_test(turn, end_seq, 257);
        renderer.freeze_joined_review(&outcome);
        let review = renderer.review_document().unwrap();
        let review_text = review
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(review_text.contains("1 action(s) omitted"));
        let receipt = enhanced_text(renderer.receipt_frame(&outcome).unwrap());
        assert!(receipt.contains("257 tool requests"));
        assert!(receipt.contains("details incomplete"));
    }

    #[test]
    fn one_tool_result_keeps_another_pending_request_in_the_dock() {
        let turn = TurnId::new(1).unwrap();
        let step = StepId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut notice = None;
        for (seq, call, path) in [(1, "call-a", "a.rs"), (2, "call-b", "b.rs")] {
            let mut update = renderer
                .consume(event(
                    seq,
                    CommittedUiKind::ToolRequested {
                        turn,
                        step,
                        call_id: identity(call),
                        name: identity("read"),
                        arguments: UiOpaquePayload::from_text_for_test(&format!(
                            r#"{{"file_path":"{path}"}}"#
                        )),
                    },
                ))
                .unwrap();
            update.apply_dock_notice(&mut notice);
        }
        assert_eq!(notice.as_deref(), Some("Requested  Read  b.rs"));
        let mut first_result = renderer
            .consume(event(
                3,
                CommittedUiKind::ToolResult {
                    turn,
                    step,
                    call_id: identity("call-a"),
                    is_error: false,
                    failure: None,
                    content: UiOpaquePayload::from_text_for_test("ignored"),
                    meta: UiOpaquePayload::from_text_for_test("{}"),
                    surface_replacement_target: None,
                },
            ))
            .unwrap();
        assert!(!first_result.apply_dock_notice(&mut notice));
        assert_eq!(notice.as_deref(), Some("Requested  Read  b.rs"));
    }

    #[test]
    fn enhanced_turn_status_waits_for_the_joined_receipt() {
        let turn = TurnId::new(1).unwrap();
        let mut renderer = LiveRenderer::new();
        let mut start = renderer
            .consume(event(1, CommittedUiKind::TurnStart { turn }))
            .unwrap();
        assert!(start.take_frame(true).is_none());
        let mut end = renderer
            .consume(event(
                2,
                CommittedUiKind::TurnEnd {
                    turn,
                    reason: UiTurnEndReason::Completed,
                },
            ))
            .unwrap();
        assert!(enhanced_text(end.take_frame(true).unwrap()).is_empty());
    }

    #[test]
    fn receipt_anchor_requires_the_exact_turn_end_sequence_and_reason() {
        let turn = TurnId::new(1).unwrap();
        let committed = event(
            9,
            CommittedUiKind::TurnEnd {
                turn,
                reason: UiTurnEndReason::Completed,
            },
        );
        let anchor = TurnEndAnchor::from_event(&committed).unwrap().unwrap();
        assert!(anchor.matches(turn, EventSeq::new(9).unwrap(), &TurnEndReason::Completed));
        assert!(!anchor.matches(turn, EventSeq::new(8).unwrap(), &TurnEndReason::Completed));
        assert!(!anchor.matches(
            TurnId::new(2).unwrap(),
            EventSeq::new(9).unwrap(),
            &TurnEndReason::Completed
        ));
        assert!(!anchor.matches(turn, EventSeq::new(9).unwrap(), &TurnEndReason::Blocked));

        let error_event = event(
            10,
            CommittedUiKind::TurnEnd {
                turn,
                reason: UiTurnEndReason::Error {
                    code: "PROVIDER_FAILED".to_owned(),
                    message: "[omitted 8192-byte text]".to_owned(),
                },
            },
        );
        let error_anchor = TurnEndAnchor::from_event(&error_event).unwrap().unwrap();
        let full_reason = TurnEndReason::Error {
            error: LlmFailure::new("x".repeat(8_192), "PROVIDER_FAILED").unwrap(),
        };
        assert!(error_anchor.matches(turn, EventSeq::new(10).unwrap(), &full_reason));

        let other_event = event(
            11,
            CommittedUiKind::TurnEnd {
                turn,
                reason: UiTurnEndReason::Other {
                    kind: Some("plugin-finished".to_owned()),
                },
            },
        );
        let other_anchor = TurnEndAnchor::from_event(&other_event).unwrap().unwrap();
        let matching = TurnEndReason::from_value(serde_json::json!({
            "kind": "plugin-finished",
            "extra": true
        }))
        .unwrap();
        let different = TurnEndReason::from_value(serde_json::json!({
            "kind": "plugin-blocked",
            "extra": true
        }))
        .unwrap();
        assert!(other_anchor.matches(turn, EventSeq::new(11).unwrap(), &matching));
        assert!(!other_anchor.matches(turn, EventSeq::new(11).unwrap(), &different));
    }

    #[test]
    fn mismatched_outcomes_cannot_replace_the_last_joined_review_or_receipt() {
        let turn = TurnId::new(1).unwrap();
        let end_seq = EventSeq::new(9).unwrap();
        let mut renderer = LiveRenderer::new();
        renderer
            .consume(event(
                end_seq.get(),
                CommittedUiKind::TurnEnd {
                    turn,
                    reason: UiTurnEndReason::Completed,
                },
            ))
            .unwrap();
        let joined = TurnOutcome::completed_for_test(turn, end_seq, 0);
        renderer.freeze_joined_review(&joined);
        let before_review = renderer
            .review_document()
            .unwrap()
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        let before_receipt = enhanced_text(renderer.receipt_frame(&joined).unwrap());

        for mismatch in [
            TurnOutcome::completed_for_test(turn, EventSeq::new(10).unwrap(), 7),
            TurnOutcome::completed_for_test(TurnId::new(2).unwrap(), end_seq, 7),
            TurnOutcome::blocked_for_test(turn, end_seq),
        ] {
            renderer.freeze_joined_review(&mismatch);
        }

        let after_review = renderer
            .review_document()
            .unwrap()
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(after_review, before_review);
        assert_eq!(
            enhanced_text(renderer.receipt_frame(&joined).unwrap()),
            before_receipt
        );
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
    fn semantic_compaction_events_remain_silent_in_the_phase_nine_renderer() {
        let mut renderer = LiveRenderer::new();
        let events = [
            CommittedUiKind::CompactionStarted {
                id: identity("compact-1"),
                turn: None,
                trigger: None,
                shadowed_nodes: Some(3),
            },
            CommittedUiKind::CompactionSummarized {
                id: identity("compact-1"),
                shadowed_tokens: 42,
                provider: identity("mock"),
                model: identity("summary"),
                usage: None,
            },
            CommittedUiKind::CompactionEnded {
                id: identity("compact-1"),
                turn: None,
                error: None,
            },
            CommittedUiKind::CompactionPruneMarked {
                target: EventSeq::new(9).unwrap(),
                shadowed_tokens: 7,
            },
        ];
        for (seq, kind) in events.into_iter().enumerate() {
            let update = renderer.consume(event(seq as u64, kind)).unwrap();
            assert!(update.frame.is_none());
            assert!(matches!(update.lifecycle, super::LiveLifecycle::None));
        }
        assert_eq!(
            renderer.semantic.status().pending_prune_shadowed_tokens,
            Some(7)
        );
        let replacement = renderer
            .consume(event(
                4,
                CommittedUiKind::ToolResult {
                    turn: TurnId::new(1).unwrap(),
                    step: StepId::new(1).unwrap(),
                    call_id: identity("historical-call"),
                    is_error: false,
                    failure: None,
                    content: UiOpaquePayload::from_text_for_test("[]"),
                    meta: UiOpaquePayload::from_text_for_test("{}"),
                    surface_replacement_target: Some(EventSeq::new(9).unwrap()),
                },
            ))
            .unwrap();
        assert!(replacement.frame.is_none());
        assert!(matches!(replacement.lifecycle, super::LiveLifecycle::None));
        assert_eq!(
            renderer.semantic.status().last_prune_shadowed_tokens,
            Some(7)
        );
        let inspect = renderer.inspect_document().unwrap();
        let inspect_text = inspect
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "Context summary started",
            "Context summary prepared",
            "Context summary committed",
            "Prune marker committed · replacement pending",
            "target seq 9",
            "7 estimated tokens in the shadowed node",
            "Surface replacement",
            "replaces seq 9",
        ] {
            assert!(inspect_text.contains(expected), "missing {expected:?}");
        }
        for overclaim in ["tokens removed", "tokens freed", "tokens saved"] {
            assert!(!inspect_text.contains(overclaim));
        }

        let mut failed = LiveRenderer::new();
        failed
            .consume(event(
                0,
                CommittedUiKind::CompactionEnded {
                    id: identity("compact-failed"),
                    turn: None,
                    error: Some(crate::session::UiCompactionError {
                        code: Some("SUMMARY_FAILED".to_owned()),
                        message: "SECRET_COMPACTION_BODY".to_owned(),
                    }),
                },
            ))
            .unwrap();
        let failed_text = failed
            .inspect_document()
            .unwrap()
            .lines()
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(failed_text.contains("Context summary failed"));
        assert!(failed_text.contains("SUMMARY_FAILED"));
        assert!(!failed_text.contains("Context summary committed"));
        assert!(!failed_text.contains("SECRET_COMPACTION_BODY"));
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
    fn approval_frame_streams_the_complete_preview_and_trusted_selector() {
        let frame = LiveFrame::approval(
            "apply_patch",
            Some("call-patch"),
            Some("update note.txt"),
            Arc::from("--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n"),
            &ApprovalPreviewKind::Opaque,
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
        let mut selector_output = String::new();
        InteractivePresenter::new()
            .render(
                &LiveFrame::approval_selector("[approval selector]\n".to_owned()).unwrap(),
                |chunk| {
                    selector_output.push_str(chunk);
                    Ok::<_, std::convert::Infallible>(())
                },
            )
            .unwrap();
        assert_eq!(selector_output, "[approval selector]\n");
        let oversized = "x".repeat(FRAME_OUTPUT_CHUNK_BYTES + 1);
        assert!(LiveFrame::approval_selector(oversized).is_err());
    }

    fn canonical_patch_request() -> ApprovalRequest {
        let source = concat!(
            "--- a/file.txt\n",
            "+++ b/file.txt\n",
            "@@ -1 +1 @@\n",
            "--- a/decoy\n",
            "+++ b/decoy\n",
        );
        let prompt = ApprovalPrompt::canonical_patch(
            Some("SECRET_DUPLICATE_REASON".to_owned()),
            source.to_owned(),
            ApprovalPatchOperation::Update,
            "file.txt".to_owned(),
            vec![
                ApprovalDiffRowKind::FileHeader,
                ApprovalDiffRowKind::FileHeader,
                ApprovalDiffRowKind::Hunk,
                ApprovalDiffRowKind::Removal,
                ApprovalDiffRowKind::Addition,
            ],
            1,
            1,
            1,
        )
        .unwrap();
        ApprovalRequest::new(
            ApprovalRequestId::new("approval-secret"),
            "apply_patch".to_owned(),
            CallId::new("SECRET_CALL_ID"),
            &prompt,
        )
    }

    #[test]
    fn canonical_patch_uses_signed_row_styles_without_exposing_internal_metadata() {
        let request = canonical_patch_request();
        let frame = LiveFrame::approval(
            request.tool_name(),
            Some(request.call_id().as_str()),
            request.reason(),
            request.preview_arc(),
            request.preview_kind(),
            false,
        )
        .unwrap();
        let prepared = EnhancedPresenter::new().prepare(frame).unwrap();
        let text = presented_text(&prepared);
        assert!(text.contains("Approval required\nProposed update · not applied\n"));
        assert!(text.contains("file.txt · +1 -1 · 1 hunk\n"));
        assert!(text.contains("One workspace file · no shell command\n"));
        let diff_start = text.find("--- a/file.txt").unwrap();
        assert_eq!(&text[diff_start..], request.preview());
        assert_eq!(text.matches("--- a/decoy").count(), 1);
        assert_eq!(text.matches("+++ b/decoy").count(), 1);
        assert!(!text.contains("apply_patch"));
        assert!(!text.contains("SECRET_CALL_ID"));
        assert!(!text.contains("SECRET_DUPLICATE_REASON"));
        assert_eq!(
            presented_text_with_style(&prepared, TextStyle::DiffHeader),
            "--- a/file.txt+++ b/file.txt"
        );
        assert_eq!(
            presented_text_with_style(&prepared, TextStyle::DiffRemove),
            "--- a/decoy"
        );
        assert_eq!(
            presented_text_with_style(&prepared, TextStyle::DiffAdd),
            "+++ b/decoy"
        );
        let debug = format!("{prepared:?}");
        assert!(!debug.contains("file.txt"));
        assert!(!debug.contains("decoy"));
    }

    #[test]
    fn opaque_patch_lookalike_never_gains_semantic_diff_styles() {
        let source = Arc::<str>::from("--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n");
        let frame = LiveFrame::approval(
            "apply_patch",
            Some("call-opaque"),
            Some("looks canonical"),
            source,
            &ApprovalPreviewKind::Opaque,
            false,
        )
        .unwrap();
        let prepared = EnhancedPresenter::new().prepare(frame).unwrap();
        assert!(presented_text(&prepared).contains("--- a/file.txt"));
        for style in [
            TextStyle::DiffHeader,
            TextStyle::DiffHunk,
            TextStyle::DiffAdd,
            TextStyle::DiffRemove,
        ] {
            assert!(presented_text_with_style(&prepared, style).is_empty());
        }
    }

    #[test]
    fn canonical_patch_path_and_payload_controls_are_visible_before_styling() {
        let path = "note\u{202e}.txt";
        let source = format!("--- /dev/null\n+++ b/{path}\n@@ -0,0 +1 @@\n+safe\u{202e}end\n");
        let prompt = ApprovalPrompt::canonical_patch(
            None,
            source,
            ApprovalPatchOperation::Create,
            path.to_owned(),
            vec![
                ApprovalDiffRowKind::FileHeader,
                ApprovalDiffRowKind::FileHeader,
                ApprovalDiffRowKind::Hunk,
                ApprovalDiffRowKind::Addition,
            ],
            1,
            0,
            1,
        )
        .unwrap();
        let request = ApprovalRequest::new(
            ApprovalRequestId::new("approval-visible"),
            "apply_patch".to_owned(),
            CallId::new("call-visible"),
            &prompt,
        );
        let frame = LiveFrame::approval(
            request.tool_name(),
            Some(request.call_id().as_str()),
            request.reason(),
            request.preview_arc(),
            request.preview_kind(),
            false,
        )
        .unwrap();
        let prepared = EnhancedPresenter::new().prepare(frame).unwrap();
        let text = presented_text(&prepared);
        assert!(text.contains("note\\u{202e}.txt"));
        assert!(text.contains("+safe\\u{202e}end"));
        assert!(!text.contains('\u{202e}'));
    }

    #[test]
    fn linear_canonical_patch_keeps_the_complete_phase_nine_record_without_escape_bytes() {
        let request = canonical_patch_request();
        let frame = LiveFrame::approval(
            request.tool_name(),
            Some(request.call_id().as_str()),
            request.reason(),
            request.preview_arc(),
            request.preview_kind(),
            false,
        )
        .unwrap();
        let mut output = String::new();
        InteractivePresenter::new()
            .render(&frame, |chunk| {
                output.push_str(chunk);
                Ok::<_, std::convert::Infallible>(())
            })
            .unwrap();
        assert_eq!(
            output,
            concat!(
                "[approval requested]\n",
                "tool | apply_patch\n",
                "call | SECRET_CALL_ID\n",
                "reason | SECRET_DUPLICATE_REASON\n",
                "preview | --- a/file.txt\n",
                "preview | +++ b/file.txt\n",
                "preview | @@ -1 +1 @@\n",
                "preview | --- a/decoy\n",
                "preview | +++ b/decoy\n",
            )
        );
        assert!(!output.contains('\u{1b}'));
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
                    provider: identity("mock"),
                    model: identity("mock-model"),
                    usage: None,
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
                    provider: identity("mock"),
                    model: identity("mock-model"),
                    usage: None,
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
                    provider: identity("mock"),
                    model: identity("mock-model"),
                    usage: None,
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

        let mut renderer = LiveRenderer::new();
        renderer.attempt = Some(blocks);
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
                    provider: identity("mock"),
                    model: identity("mock-model"),
                    usage: None,
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
                    call_id: identity("call-1"),
                    is_error: true,
                    failure: Some(UiToolFailure {
                        name: "NAME\n".to_owned(),
                        code: "CODE\nTAIL".to_owned(),
                    }),
                    content: UiOpaquePayload::from_text_for_test("[]"),
                    meta: UiOpaquePayload::from_text_for_test(""),
                    surface_replacement_target: None,
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
                    call_id: identity("call-1"),
                    name: identity("read\nspoof"),
                    arguments: UiOpaquePayload::from_text_for_test("{}"),
                },
            ),
            &mut output,
        );
        assert!(output.contains("[tool requested]"));
        assert!(!output.contains("running"));
        assert!(output.contains("tool | spoof"));
    }
}
