use std::{fmt, sync::Arc};

use super::presentation::{PresentationError, PresentedChunkBuilder, TextStyle};

const MAX_LINE_PREFIX_BYTES: usize = 64;
const MAX_INLINE_CODE_BYTES: usize = 4 * 1024;
const MAX_FENCE_BYTES: usize = 64 * 1024;
const FENCE_BLOCK_BYTES: usize = 1024;
const MAX_STYLE_RUNS: usize = 4 * 1024;
const MAX_MARKUP_FRAME_ITEMS: usize = 96 * 1024;
pub(crate) const MAX_MARKUP_FRAME_TEXT_BYTES: usize = 768 * 1024;
const MARKUP_ITEM_HEADROOM: usize = MAX_STYLE_RUNS * 2 + 16;
const DISPLAY_OMITTED: &str = "[assistant display omitted: presentation limit exceeded]";

#[derive(Clone)]
pub(crate) struct MarkupState {
    block: BlockState,
    line: LineState,
    inline: InlineState,
    inline_disabled: bool,
    style_runs: usize,
    last_style: Option<TextStyle>,
    degraded: bool,
    output_omitted: bool,
}

impl Default for MarkupState {
    fn default() -> Self {
        Self {
            block: BlockState::Markdown,
            line: LineState::prefix(),
            inline: InlineState::Text,
            inline_disabled: false,
            style_runs: 0,
            last_style: None,
            degraded: false,
            output_omitted: false,
        }
    }
}

impl fmt::Debug for MarkupState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MarkupState")
            .field("block", &self.block.label())
            .field("block_bytes", &self.block.bytes())
            .field("line_bytes", &self.line.bytes())
            .field("inline_bytes", &self.inline.bytes())
            .field("style_runs", &self.style_runs)
            .field("degraded", &self.degraded)
            .field("output_omitted", &self.output_omitted)
            .finish()
    }
}

#[derive(Clone)]
enum BlockState {
    Markdown,
    FenceHeld(FenceHeld),
    FencePlain(FencePlain),
}

impl BlockState {
    fn label(&self) -> &'static str {
        match self {
            Self::Markdown => "Markdown",
            Self::FenceHeld(_) => "FenceHeld",
            Self::FencePlain(_) => "FencePlain",
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Markdown | Self::FencePlain(_) => 0,
            Self::FenceHeld(fence) => fence.buffer.len(),
        }
    }
}

#[derive(Clone)]
struct FenceHeld {
    kind: FenceKind,
    buffer: ChunkBuffer,
    closer: CloserTracker,
}

#[derive(Clone)]
struct FencePlain {
    closer: CloserTracker,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FenceKind {
    Code,
    Diff,
}

#[derive(Clone)]
enum LineState {
    Prefix(String),
    Body(LineFormat),
}

impl LineState {
    fn prefix() -> Self {
        Self::Prefix(String::new())
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Prefix(prefix) => prefix.len(),
            Self::Body(_) => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineFormat {
    Paragraph,
    Heading,
    List,
    Quote,
}

impl LineFormat {
    fn body_style(self) -> TextStyle {
        match self {
            Self::Paragraph | Self::List => TextStyle::Assistant,
            Self::Heading => TextStyle::Heading,
            Self::Quote => TextStyle::Quote,
        }
    }

    fn marker_style(self) -> TextStyle {
        match self {
            Self::Paragraph => TextStyle::Assistant,
            Self::Heading => TextStyle::Heading,
            Self::List => TextStyle::Accent,
            Self::Quote => TextStyle::Quote,
        }
    }
}

#[derive(Clone)]
enum InlineState {
    Text,
    Pending(String),
}

impl InlineState {
    fn bytes(&self) -> usize {
        match self {
            Self::Text => 0,
            Self::Pending(pending) => pending.len(),
        }
    }
}

enum PrefixDecision {
    NeedMore,
    Literal,
    Body {
        format: LineFormat,
        marker_bytes: usize,
    },
    FenceOpen(FenceKind),
}

#[derive(Clone, Default)]
struct CloserTracker {
    candidate: String,
    viable: bool,
}

impl CloserTracker {
    fn new() -> Self {
        Self {
            candidate: String::new(),
            viable: true,
        }
    }

    fn observe(&mut self, text: &str) -> Result<(), PresentationError> {
        if !self.viable || text.is_empty() {
            return Ok(());
        }
        let next = self
            .candidate
            .len()
            .checked_add(text.len())
            .ok_or(PresentationError::Limit)?;
        if next > MAX_LINE_PREFIX_BYTES {
            self.candidate.clear();
            self.viable = false;
            return Ok(());
        }
        self.candidate
            .try_reserve(text.len())
            .map_err(|_| PresentationError::Capacity)?;
        self.candidate.push_str(text);
        let bytes = self.candidate.as_bytes();
        self.viable = if bytes.len() <= 3 {
            bytes.iter().all(|byte| *byte == b'`')
        } else {
            bytes[..3] == *b"```" && bytes[3..].iter().all(|byte| *byte == b' ')
        };
        if !self.viable {
            self.candidate.clear();
        }
        Ok(())
    }

    fn finish_line(&mut self) -> bool {
        let closing = self.is_closing();
        self.candidate.clear();
        self.viable = true;
        closing
    }

    fn is_closing(&self) -> bool {
        self.viable
            && self.candidate.len() >= 3
            && self.candidate.as_bytes()[..3] == *b"```"
            && self.candidate.as_bytes()[3..]
                .iter()
                .all(|byte| *byte == b' ')
    }
}

#[derive(Clone, Default)]
struct ChunkBuffer {
    blocks: Arc<Vec<Arc<str>>>,
    tail: String,
    bytes: usize,
}

enum BufferError {
    Capacity,
    Limit,
}

impl ChunkBuffer {
    fn len(&self) -> usize {
        self.bytes
    }

    fn append(&mut self, text: &str) -> Result<(), BufferError> {
        if text.is_empty() {
            return Ok(());
        }
        let next_bytes = self
            .bytes
            .checked_add(text.len())
            .ok_or(BufferError::Limit)?;
        if next_bytes > MAX_FENCE_BYTES {
            return Err(BufferError::Limit);
        }

        let mut remaining = text;
        while !remaining.is_empty() {
            let available = FENCE_BLOCK_BYTES.saturating_sub(self.tail.len());
            let take = boundary_at_or_before(remaining, available);
            if take != 0 {
                self.tail
                    .try_reserve(take)
                    .map_err(|_| BufferError::Capacity)?;
                self.tail.push_str(&remaining[..take]);
                remaining = &remaining[take..];
            }
            if !remaining.is_empty() || self.tail.len() == FENCE_BLOCK_BYTES {
                self.seal_tail()?;
            }
        }
        self.bytes = next_bytes;
        Ok(())
    }

    fn seal_tail(&mut self) -> Result<(), BufferError> {
        if self.tail.is_empty() {
            return Ok(());
        }
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(self.blocks.len().saturating_add(1))
            .map_err(|_| BufferError::Capacity)?;
        blocks.extend(self.blocks.iter().cloned());
        blocks.push(Arc::from(std::mem::take(&mut self.tail).into_boxed_str()));
        self.blocks = Arc::new(blocks);
        Ok(())
    }

    fn collect(&self) -> Result<String, PresentationError> {
        let mut text = String::new();
        text.try_reserve_exact(self.bytes)
            .map_err(|_| PresentationError::Capacity)?;
        for block in self.blocks.iter() {
            text.push_str(block);
        }
        text.push_str(&self.tail);
        Ok(text)
    }
}

fn boundary_at_or_before(text: &str, limit: usize) -> usize {
    let mut end = text.len().min(limit);
    while end != 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

impl MarkupState {
    pub(crate) fn has_pending_source(&self) -> bool {
        self.block.bytes() != 0 || self.line.bytes() != 0 || self.inline.bytes() != 0
    }

    pub(crate) fn push(
        &mut self,
        text: &str,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if self.output_omitted {
            return Ok(());
        }
        if !self.has_output_budget(estimated_literal_items(text), text.len(), builder) {
            self.omit_output(builder, at_line_start)?;
            return Ok(());
        }
        for segment in text.split_inclusive('\n') {
            let (content, line_feed) = segment
                .strip_suffix('\n')
                .map_or((segment, false), |content| (content, true));
            if self.degraded {
                self.push_literal_segment(content, line_feed, builder, at_line_start)?;
                continue;
            }
            let block = std::mem::replace(&mut self.block, BlockState::Markdown);
            self.block = match block {
                BlockState::Markdown => {
                    self.push_markdown_content(content, builder, at_line_start)?;
                    if line_feed {
                        self.end_markdown_line(builder, at_line_start)?;
                    }
                    std::mem::replace(&mut self.block, BlockState::Markdown)
                }
                BlockState::FenceHeld(fence) => self.push_held_fence_segment(
                    fence,
                    segment,
                    content,
                    line_feed,
                    builder,
                    at_line_start,
                )?,
                BlockState::FencePlain(fence) => self.push_plain_fence_segment(
                    fence,
                    content,
                    line_feed,
                    builder,
                    at_line_start,
                )?,
            };
            if self.output_omitted {
                break;
            }
        }
        Ok(())
    }

    pub(crate) fn omit_remaining_display(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if self.output_omitted {
            return Ok(());
        }
        self.omit_output(builder, at_line_start)
    }

    pub(crate) fn finish_authoritative(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        self.finish_inner(true, builder, at_line_start)
    }

    pub(crate) fn abort_plain(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        self.finish_inner(false, builder, at_line_start)
    }

    fn finish_inner(
        &mut self,
        authoritative: bool,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if self.output_omitted {
            *self = Self::default();
            return Ok(());
        }
        let block = std::mem::replace(&mut self.block, BlockState::Markdown);
        match block {
            BlockState::Markdown => self.finish_markdown_line(builder, at_line_start)?,
            BlockState::FenceHeld(fence) => {
                let text = fence.buffer.collect()?;
                if authoritative && fence.closer.is_closing() {
                    self.render_closed_fence(fence.kind, &text, builder, at_line_start)?;
                } else {
                    self.push_literal_text(&text, builder, at_line_start)?;
                }
            }
            BlockState::FencePlain(_) => {}
        }
        *self = Self::default();
        Ok(())
    }

    fn push_markdown_content(
        &mut self,
        content: &str,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        let mut offset = 0;
        while offset < content.len() {
            if self.degraded {
                self.emit(
                    TextStyle::Assistant,
                    &content[offset..],
                    builder,
                    at_line_start,
                )?;
                return Ok(());
            }
            if matches!(self.line, LineState::Body(_)) {
                let format = match self.line {
                    LineState::Body(format) => format,
                    LineState::Prefix(_) => unreachable!(),
                };
                self.push_inline(
                    format.body_style(),
                    &content[offset..],
                    builder,
                    at_line_start,
                )?;
                return Ok(());
            }

            let character = content[offset..]
                .chars()
                .next()
                .ok_or(PresentationError::InvalidText)?;
            let character_bytes = character.len_utf8();
            let prefix_len = match &self.line {
                LineState::Prefix(prefix) => prefix.len(),
                LineState::Body(_) => 0,
            };
            if prefix_len
                .checked_add(character_bytes)
                .is_none_or(|next| next > MAX_LINE_PREFIX_BYTES)
            {
                let literal = matches!(
                    &self.line,
                    LineState::Prefix(prefix) if prefix.starts_with("```")
                );
                if literal {
                    self.resolve_literal_prefix(builder, at_line_start)?;
                } else {
                    self.resolve_prefix(
                        PrefixDecision::Body {
                            format: LineFormat::Paragraph,
                            marker_bytes: 0,
                        },
                        builder,
                        at_line_start,
                    )?;
                }
                continue;
            }
            let prefix = match &mut self.line {
                LineState::Prefix(prefix) => prefix,
                LineState::Body(_) => unreachable!(),
            };
            prefix
                .try_reserve(character_bytes)
                .map_err(|_| PresentationError::Capacity)?;
            prefix.push(character);
            offset += character_bytes;

            let decision = match &self.line {
                LineState::Prefix(prefix) => classify_prefix(prefix, false),
                LineState::Body(_) => unreachable!(),
            };
            if !matches!(decision, PrefixDecision::NeedMore) {
                if matches!(decision, PrefixDecision::Literal) {
                    self.resolve_literal_prefix(builder, at_line_start)?;
                } else {
                    self.resolve_prefix(decision, builder, at_line_start)?;
                }
            }
        }
        Ok(())
    }

    fn resolve_prefix(
        &mut self,
        decision: PrefixDecision,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        let PrefixDecision::Body {
            format,
            marker_bytes,
        } = decision
        else {
            return Err(PresentationError::InvalidText);
        };
        let prefix = match std::mem::replace(&mut self.line, LineState::Body(format)) {
            LineState::Prefix(prefix) => prefix,
            LineState::Body(_) => return Err(PresentationError::InvalidText),
        };
        if marker_bytes > prefix.len() || !prefix.is_char_boundary(marker_bytes) {
            return Err(PresentationError::InvalidText);
        }
        self.emit(
            format.marker_style(),
            &prefix[..marker_bytes],
            builder,
            at_line_start,
        )?;
        self.push_inline(
            format.body_style(),
            &prefix[marker_bytes..],
            builder,
            at_line_start,
        )
    }

    fn resolve_literal_prefix(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        let prefix = match std::mem::replace(&mut self.line, LineState::Body(LineFormat::Paragraph))
        {
            LineState::Prefix(prefix) => prefix,
            LineState::Body(_) => return Err(PresentationError::InvalidText),
        };
        self.inline_disabled = true;
        self.emit(TextStyle::Assistant, &prefix, builder, at_line_start)
    }

    fn end_markdown_line(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if let LineState::Prefix(prefix) = &self.line {
            let decision = classify_prefix(prefix, true);
            if let PrefixDecision::FenceOpen(kind) = decision {
                let prefix = match std::mem::replace(&mut self.line, LineState::prefix()) {
                    LineState::Prefix(prefix) => prefix,
                    LineState::Body(_) => return Err(PresentationError::InvalidText),
                };
                let mut opener = prefix;
                opener
                    .try_reserve(1)
                    .map_err(|_| PresentationError::Capacity)?;
                opener.push('\n');
                let mut buffer = ChunkBuffer::default();
                buffer.append(&opener).map_err(map_buffer_error)?;
                self.inline = InlineState::Text;
                self.inline_disabled = false;
                self.block = BlockState::FenceHeld(FenceHeld {
                    kind,
                    buffer,
                    closer: CloserTracker::new(),
                });
                return Ok(());
            }
            if matches!(decision, PrefixDecision::Literal) {
                self.resolve_literal_prefix(builder, at_line_start)?;
            } else {
                self.resolve_prefix(decision, builder, at_line_start)?;
            }
        }
        let format = match self.line {
            LineState::Body(format) => format,
            LineState::Prefix(_) => return Err(PresentationError::InvalidText),
        };
        self.flush_inline(format.body_style(), builder, at_line_start)?;
        self.emit_line_feed(builder, at_line_start)?;
        self.line = LineState::prefix();
        self.inline_disabled = false;
        Ok(())
    }

    fn finish_markdown_line(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if let LineState::Prefix(prefix) = &self.line {
            let decision = classify_prefix(prefix, true);
            if matches!(
                decision,
                PrefixDecision::FenceOpen(_) | PrefixDecision::Literal
            ) {
                self.resolve_literal_prefix(builder, at_line_start)?;
            } else {
                self.resolve_prefix(decision, builder, at_line_start)?;
            }
        }
        let format = match self.line {
            LineState::Body(format) => format,
            LineState::Prefix(_) => return Err(PresentationError::InvalidText),
        };
        self.flush_inline(format.body_style(), builder, at_line_start)
    }

    fn push_inline(
        &mut self,
        base_style: TextStyle,
        text: &str,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if text.is_empty() {
            return Ok(());
        }
        if self.inline_disabled || self.degraded {
            return self.emit(base_style, text, builder, at_line_start);
        }
        let mut remaining = text;
        while !remaining.is_empty() {
            let state = std::mem::replace(&mut self.inline, InlineState::Text);
            match state {
                InlineState::Text => {
                    if let Some(index) = remaining.find('`') {
                        self.emit(base_style, &remaining[..index], builder, at_line_start)?;
                        let mut pending = String::new();
                        pending
                            .try_reserve_exact(1)
                            .map_err(|_| PresentationError::Capacity)?;
                        pending.push('`');
                        self.inline = InlineState::Pending(pending);
                        remaining = &remaining[index + 1..];
                    } else {
                        self.emit(base_style, remaining, builder, at_line_start)?;
                        return Ok(());
                    }
                }
                InlineState::Pending(mut pending) => {
                    if let Some(index) = remaining.find('`') {
                        let needed = index.saturating_add(1);
                        if pending
                            .len()
                            .checked_add(needed)
                            .is_some_and(|next| next <= MAX_INLINE_CODE_BYTES)
                        {
                            pending
                                .try_reserve(needed)
                                .map_err(|_| PresentationError::Capacity)?;
                            pending.push_str(&remaining[..=index]);
                            self.emit(TextStyle::Code, &pending, builder, at_line_start)?;
                            remaining = &remaining[index + 1..];
                        } else {
                            self.emit(base_style, &pending, builder, at_line_start)?;
                            self.emit(base_style, remaining, builder, at_line_start)?;
                            self.inline_disabled = true;
                            return Ok(());
                        }
                    } else if pending
                        .len()
                        .checked_add(remaining.len())
                        .is_some_and(|next| next <= MAX_INLINE_CODE_BYTES)
                    {
                        pending
                            .try_reserve(remaining.len())
                            .map_err(|_| PresentationError::Capacity)?;
                        pending.push_str(remaining);
                        self.inline = InlineState::Pending(pending);
                        return Ok(());
                    } else {
                        self.emit(base_style, &pending, builder, at_line_start)?;
                        self.emit(base_style, remaining, builder, at_line_start)?;
                        self.inline_disabled = true;
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn flush_inline(
        &mut self,
        base_style: TextStyle,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if let InlineState::Pending(pending) =
            std::mem::replace(&mut self.inline, InlineState::Text)
        {
            self.emit(base_style, &pending, builder, at_line_start)?;
        }
        Ok(())
    }

    fn push_held_fence_segment(
        &mut self,
        mut fence: FenceHeld,
        segment: &str,
        content: &str,
        line_feed: bool,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<BlockState, PresentationError> {
        match fence.buffer.append(segment) {
            Ok(()) => {
                fence.closer.observe(content)?;
                if line_feed && fence.closer.finish_line() {
                    let text = fence.buffer.collect()?;
                    self.render_closed_fence(fence.kind, &text, builder, at_line_start)?;
                    Ok(BlockState::Markdown)
                } else {
                    Ok(BlockState::FenceHeld(fence))
                }
            }
            Err(BufferError::Capacity) => Err(PresentationError::Capacity),
            Err(BufferError::Limit) => {
                let buffered = fence.buffer.collect()?;
                self.push_literal_text(&buffered, builder, at_line_start)?;
                self.push_literal_segment(content, line_feed, builder, at_line_start)?;
                fence.closer.observe(content)?;
                if line_feed && fence.closer.finish_line() {
                    Ok(BlockState::Markdown)
                } else {
                    Ok(BlockState::FencePlain(FencePlain {
                        closer: fence.closer,
                    }))
                }
            }
        }
    }

    fn push_plain_fence_segment(
        &mut self,
        mut fence: FencePlain,
        content: &str,
        line_feed: bool,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<BlockState, PresentationError> {
        self.push_literal_segment(content, line_feed, builder, at_line_start)?;
        fence.closer.observe(content)?;
        if line_feed && fence.closer.finish_line() {
            Ok(BlockState::Markdown)
        } else {
            Ok(BlockState::FencePlain(fence))
        }
    }

    fn render_closed_fence(
        &mut self,
        kind: FenceKind,
        text: &str,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if !self.has_output_budget(estimated_literal_items(text), text.len(), builder) {
            self.omit_output(builder, at_line_start)?;
            return Ok(());
        }
        let mut lines = text.split_inclusive('\n').peekable();
        let mut first = true;
        while let Some(segment) = lines.next() {
            let (content, line_feed) = segment
                .strip_suffix('\n')
                .map_or((segment, false), |content| (content, true));
            let style = if first || lines.peek().is_none() {
                TextStyle::Muted
            } else {
                match kind {
                    FenceKind::Code => TextStyle::Code,
                    FenceKind::Diff => diff_style(content),
                }
            };
            self.emit(style, content, builder, at_line_start)?;
            if line_feed {
                self.emit_line_feed(builder, at_line_start)?;
            }
            first = false;
        }
        Ok(())
    }

    fn push_literal_text(
        &mut self,
        text: &str,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        for segment in text.split_inclusive('\n') {
            let (content, line_feed) = segment
                .strip_suffix('\n')
                .map_or((segment, false), |content| (content, true));
            self.push_literal_segment(content, line_feed, builder, at_line_start)?;
        }
        Ok(())
    }

    fn push_literal_segment(
        &mut self,
        content: &str,
        line_feed: bool,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        self.emit(TextStyle::Assistant, content, builder, at_line_start)?;
        if line_feed {
            self.emit_line_feed(builder, at_line_start)?;
        }
        Ok(())
    }

    fn emit(
        &mut self,
        requested: TextStyle,
        text: &str,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if text.is_empty() {
            return Ok(());
        }
        let mut style = if self.degraded {
            TextStyle::Assistant
        } else {
            requested
        };
        if style != TextStyle::Assistant && self.last_style != Some(style) {
            if self.style_runs == MAX_STYLE_RUNS {
                self.degraded = true;
                style = TextStyle::Assistant;
            } else {
                self.style_runs += 1;
            }
        }
        builder.push_text(style, text)?;
        self.last_style = Some(style);
        *at_line_start = false;
        Ok(())
    }

    fn emit_line_feed(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        builder.push_line_feed()?;
        self.last_style = None;
        *at_line_start = true;
        Ok(())
    }

    fn has_output_budget(
        &self,
        source_items: usize,
        source_bytes: usize,
        builder: &PresentedChunkBuilder,
    ) -> bool {
        let items_fit = builder
            .item_count()
            .checked_add(source_items)
            .and_then(|items| items.checked_add(MARKUP_ITEM_HEADROOM))
            .is_some_and(|items| items <= MAX_MARKUP_FRAME_ITEMS);
        let bytes_fit = builder
            .text_bytes()
            .checked_add(source_bytes)
            .is_some_and(|bytes| bytes <= MAX_MARKUP_FRAME_TEXT_BYTES);
        items_fit && bytes_fit
    }

    fn omit_output(
        &mut self,
        builder: &mut PresentedChunkBuilder,
        at_line_start: &mut bool,
    ) -> Result<(), PresentationError> {
        if !*at_line_start {
            builder.push_line_feed()?;
        }
        builder.push_text(TextStyle::Muted, DISPLAY_OMITTED)?;
        builder.push_line_feed()?;
        self.block = BlockState::Markdown;
        self.line = LineState::prefix();
        self.inline = InlineState::Text;
        self.inline_disabled = false;
        self.last_style = None;
        self.output_omitted = true;
        *at_line_start = true;
        Ok(())
    }
}

fn estimated_literal_items(text: &str) -> usize {
    let mut items = 0usize;
    for segment in text.split_inclusive('\n') {
        let (content, line_feed) = segment
            .strip_suffix('\n')
            .map_or((segment, false), |content| (content, true));
        if !content.is_empty() {
            items = items.saturating_add(1);
        }
        if line_feed {
            items = items.saturating_add(1);
        }
    }
    items
}

fn map_buffer_error(error: BufferError) -> PresentationError {
    match error {
        BufferError::Capacity => PresentationError::Capacity,
        BufferError::Limit => PresentationError::Limit,
    }
}

fn classify_prefix(prefix: &str, end_of_line: bool) -> PrefixDecision {
    let bytes = prefix.as_bytes();
    if bytes.is_empty() {
        return if end_of_line {
            PrefixDecision::Body {
                format: LineFormat::Paragraph,
                marker_bytes: 0,
            }
        } else {
            PrefixDecision::NeedMore
        };
    }
    match bytes[0] {
        b'#' => classify_heading(bytes, end_of_line),
        b'-' | b'*' | b'+' => classify_two_byte_marker(bytes, LineFormat::List, end_of_line),
        b'>' => classify_two_byte_marker(bytes, LineFormat::Quote, end_of_line),
        b'0'..=b'9' => classify_ordered_marker(bytes, end_of_line),
        b'`' => classify_fence(prefix, end_of_line),
        _ => PrefixDecision::Body {
            format: LineFormat::Paragraph,
            marker_bytes: 0,
        },
    }
}

fn classify_heading(bytes: &[u8], end_of_line: bool) -> PrefixDecision {
    let count = bytes.iter().take_while(|byte| **byte == b'#').count();
    if count > 3 {
        return paragraph();
    }
    match bytes.get(count) {
        Some(b' ') => PrefixDecision::Body {
            format: LineFormat::Heading,
            marker_bytes: count + 1,
        },
        Some(_) => paragraph(),
        None if end_of_line => paragraph(),
        None => PrefixDecision::NeedMore,
    }
}

fn classify_two_byte_marker(bytes: &[u8], format: LineFormat, end_of_line: bool) -> PrefixDecision {
    match bytes.get(1) {
        Some(b' ') => PrefixDecision::Body {
            format,
            marker_bytes: 2,
        },
        Some(_) => paragraph(),
        None if end_of_line => paragraph(),
        None => PrefixDecision::NeedMore,
    }
}

fn classify_ordered_marker(bytes: &[u8], end_of_line: bool) -> PrefixDecision {
    let digits = bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    if digits > 3 {
        return paragraph();
    }
    match bytes.get(digits) {
        Some(b'.') => match bytes.get(digits + 1) {
            Some(b' ') => PrefixDecision::Body {
                format: LineFormat::List,
                marker_bytes: digits + 2,
            },
            Some(_) => paragraph(),
            None if end_of_line => paragraph(),
            None => PrefixDecision::NeedMore,
        },
        Some(_) => paragraph(),
        None if end_of_line => paragraph(),
        None => PrefixDecision::NeedMore,
    }
}

fn classify_fence(prefix: &str, end_of_line: bool) -> PrefixDecision {
    let bytes = prefix.as_bytes();
    if bytes.len() < 3 && bytes.iter().all(|byte| *byte == b'`') {
        return if end_of_line {
            paragraph()
        } else {
            PrefixDecision::NeedMore
        };
    }
    if !bytes.starts_with(b"```") {
        return paragraph();
    }
    if !end_of_line {
        return PrefixDecision::NeedMore;
    }
    let label = prefix[3..].trim_matches(' ');
    if label.len() > 32
        || !label.is_ascii()
        || !label
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'+' | b'.' | b'-'))
    {
        return PrefixDecision::Literal;
    }
    PrefixDecision::FenceOpen(
        if label.eq_ignore_ascii_case("diff") || label.eq_ignore_ascii_case("patch") {
            FenceKind::Diff
        } else {
            FenceKind::Code
        },
    )
}

fn paragraph() -> PrefixDecision {
    PrefixDecision::Body {
        format: LineFormat::Paragraph,
        marker_bytes: 0,
    }
}

fn diff_style(line: &str) -> TextStyle {
    if line.starts_with("--- ") || line.starts_with("+++ ") {
        TextStyle::DiffHeader
    } else if is_hunk_header(line) {
        TextStyle::DiffHunk
    } else if line.starts_with('+') {
        TextStyle::DiffAdd
    } else if line.starts_with('-') {
        TextStyle::DiffRemove
    } else if line == "\\ No newline at end of file" {
        TextStyle::Muted
    } else {
        TextStyle::Code
    }
}

fn is_hunk_header(line: &str) -> bool {
    let Some(mut rest) = line.strip_prefix("@@ -") else {
        return false;
    };
    let Some(after_old) = consume_range(rest) else {
        return false;
    };
    rest = after_old;
    let Some(after_separator) = rest.strip_prefix(" +") else {
        return false;
    };
    let Some(after_new) = consume_range(after_separator) else {
        return false;
    };
    after_new.starts_with(" @@")
}

fn consume_range(input: &str) -> Option<&str> {
    let digits = input.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let mut rest = &input[digits..];
    if let Some(after_comma) = rest.strip_prefix(',') {
        let count = after_comma.bytes().take_while(u8::is_ascii_digit).count();
        if count == 0 {
            return None;
        }
        rest = &after_comma[count..];
    }
    Some(rest)
}

#[cfg(test)]
mod tests {
    use super::{
        DISPLAY_OMITTED, MARKUP_ITEM_HEADROOM, MAX_FENCE_BYTES, MAX_INLINE_CODE_BYTES,
        MAX_MARKUP_FRAME_ITEMS, MAX_MARKUP_FRAME_TEXT_BYTES, MAX_STYLE_RUNS, MarkupState,
    };
    use crate::tui::presentation::{PresentedChunk, PresentedItem, TextStyle};

    fn render(chunks: &[&str]) -> PresentedChunk {
        let mut state = MarkupState::default();
        let mut builder = PresentedChunk::builder();
        let mut at_line_start = true;
        for chunk in chunks {
            state.push(chunk, &mut builder, &mut at_line_start).unwrap();
        }
        state
            .finish_authoritative(&mut builder, &mut at_line_start)
            .unwrap();
        builder.finish()
    }

    fn render_aborted(chunks: &[&str]) -> PresentedChunk {
        let mut state = MarkupState::default();
        let mut builder = PresentedChunk::builder();
        let mut at_line_start = true;
        for chunk in chunks {
            state.push(chunk, &mut builder, &mut at_line_start).unwrap();
        }
        state.abort_plain(&mut builder, &mut at_line_start).unwrap();
        builder.finish()
    }

    fn plain_text(chunk: &PresentedChunk) -> String {
        let mut output = String::new();
        for item in chunk.items() {
            match item {
                PresentedItem::Text { text, .. } => output.push_str(text),
                PresentedItem::LineFeed => output.push('\n'),
            }
        }
        output
    }

    fn styled_text(chunk: &PresentedChunk, style: TextStyle) -> String {
        let mut output = String::new();
        for item in chunk.items() {
            if let PresentedItem::Text {
                style: item_style,
                text,
            } = item
            {
                if *item_style == style {
                    output.push_str(text);
                }
            }
        }
        output
    }

    fn semantic_shape(chunk: &PresentedChunk) -> Vec<(Option<TextStyle>, usize)> {
        chunk
            .items()
            .iter()
            .map(|item| match item {
                PresentedItem::Text { style, text } => (Some(*style), text.len()),
                PresentedItem::LineFeed => (None, 1),
            })
            .collect()
    }

    #[test]
    fn semantic_blocks_keep_every_source_byte_and_use_closed_styles() {
        let source = concat!(
            "# Heading\n",
            "- item with `inline` code\n",
            "> quoted text\n",
            "```rust\n",
            "fn main() {}\n",
            "```\n",
            "```diff\n",
            "--- a/file\n",
            "+++ b/file\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
            "```\n",
        );
        let chunk = render(&[source]);
        assert_eq!(plain_text(&chunk), source);
        assert!(styled_text(&chunk, TextStyle::Heading).contains("# Heading"));
        assert!(styled_text(&chunk, TextStyle::Code).contains("`inline`"));
        assert!(styled_text(&chunk, TextStyle::Quote).contains("quoted text"));
        assert!(styled_text(&chunk, TextStyle::DiffHeader).contains("--- a/file"));
        assert!(styled_text(&chunk, TextStyle::DiffHunk).contains("@@ -1 +1 @@"));
        assert!(styled_text(&chunk, TextStyle::DiffRemove).contains("-old"));
        assert!(styled_text(&chunk, TextStyle::DiffAdd).contains("+new"));
    }

    #[test]
    fn every_two_chunk_split_has_the_same_text_and_styles() {
        let source = "## 标题\n- `值`\n```diff\n-old\n+新\n```\n";
        let whole = render(&[source]);
        let expected_shape = semantic_shape(&whole);
        for split in (0..=source.len()).filter(|index| source.is_char_boundary(*index)) {
            let split_render = render(&[&source[..split], &source[split..]]);
            assert_eq!(plain_text(&split_render), source, "split {split}");
            assert_eq!(
                semantic_shape(&split_render),
                expected_shape,
                "split {split}"
            );
        }
    }

    #[test]
    fn unclosed_inline_and_fence_finish_as_plain_without_losing_text() {
        for source in ["before `secret", "```rust\nsecret\n"] {
            let chunk = render(&[source]);
            assert_eq!(plain_text(&chunk), source);
            assert!(chunk.items().iter().all(|item| matches!(
                item,
                PresentedItem::LineFeed
                    | PresentedItem::Text {
                        style: TextStyle::Assistant,
                        ..
                    }
            )));
            assert!(styled_text(&chunk, TextStyle::Code).is_empty());
        }
    }

    #[test]
    fn a_closing_fence_at_the_authoritative_eof_is_still_closed() {
        let source = "```rust\ncode-at-eof\n```";
        let chunk = render(&["``", "`rust\ncode-at-eof\n``", "`"]);
        assert_eq!(plain_text(&chunk), source);
        assert_eq!(styled_text(&chunk, TextStyle::Code), "code-at-eof");
    }

    #[test]
    fn an_aborted_eof_closer_never_promotes_partial_output_to_code() {
        let source = "```rust\npartial\n```";
        let chunk = render_aborted(&["```rust\npartial\n``", "`"]);
        assert_eq!(plain_text(&chunk), source);
        assert!(chunk.items().iter().all(|item| matches!(
            item,
            PresentedItem::LineFeed
                | PresentedItem::Text {
                    style: TextStyle::Assistant,
                    ..
                }
        )));
    }

    #[test]
    fn fence_shaped_but_invalid_openers_remain_entirely_literal() {
        let eof_only = render(&["```rust"]);
        assert_eq!(plain_text(&eof_only), "```rust");
        assert!(styled_text(&eof_only, TextStyle::Code).is_empty());

        let long_label = "r".repeat(33);
        let source = format!("```{long_label}\nbody\n```\n");
        let chunk = render(&[&source]);
        assert_eq!(plain_text(&chunk), source);
        assert!(chunk.items().iter().all(|item| matches!(
            item,
            PresentedItem::LineFeed
                | PresentedItem::Text {
                    style: TextStyle::Assistant,
                    ..
                }
        )));
    }

    #[test]
    fn inline_limit_exact_styles_and_one_over_degrades_atomically() {
        let exact = format!("`{}`", "x".repeat(MAX_INLINE_CODE_BYTES - 2));
        let exact_chunk = render(&[&exact]);
        assert_eq!(plain_text(&exact_chunk), exact);
        assert_eq!(styled_text(&exact_chunk, TextStyle::Code), exact);

        let over = format!("`{}` after", "s".repeat(MAX_INLINE_CODE_BYTES - 1));
        let over_chunk = render(&[&over]);
        assert_eq!(plain_text(&over_chunk), over);
        assert!(styled_text(&over_chunk, TextStyle::Code).is_empty());
    }

    #[test]
    fn language_label_limit_accepts_32_ascii_bytes_and_rejects_one_more() {
        let exact_label = "r".repeat(32);
        let exact = format!("```{exact_label}\ncode-exact\n```\n");
        let exact_chunk = render(&[&exact]);
        assert_eq!(plain_text(&exact_chunk), exact);
        assert_eq!(styled_text(&exact_chunk, TextStyle::Code), "code-exact");

        let over_label = "r".repeat(33);
        let over = format!("```{over_label}\ncode-over\n```\n");
        let over_chunk = render(&[&over]);
        assert_eq!(plain_text(&over_chunk), over);
        assert!(!styled_text(&over_chunk, TextStyle::Code).contains("code-over"));
    }

    #[test]
    fn fence_limit_exact_styles_and_one_over_degrades_independently_of_chunks() {
        let exact_body = "x".repeat(MAX_FENCE_BYTES - 9);
        let exact = format!("```\n{exact_body}\n```\n");
        assert_eq!(exact.len(), MAX_FENCE_BYTES);
        let exact_chunk = render(&[&exact]);
        assert_eq!(plain_text(&exact_chunk), exact);
        assert_eq!(styled_text(&exact_chunk, TextStyle::Code), exact_body);

        let over_body = "y".repeat(MAX_FENCE_BYTES - 8);
        let over = format!("```\n{over_body}\n```\n");
        assert_eq!(over.len(), MAX_FENCE_BYTES + 1);
        let whole = render(&[&over]);
        let split = render(&[&over[..1024], &over[1024..]]);
        assert_eq!(plain_text(&whole), over);
        assert_eq!(plain_text(&split), over);
        assert_eq!(semantic_shape(&whole), semantic_shape(&split));
        assert!(styled_text(&whole, TextStyle::Code).is_empty());
    }

    #[test]
    fn exact_fence_budget_with_maximum_short_lines_stays_below_item_capacity() {
        let body = "x\n".repeat((MAX_FENCE_BYTES - 8) / 2);
        let source = format!("```\n{body}```\n");
        assert_eq!(source.len(), MAX_FENCE_BYTES);
        let chunk = render(&[&source]);
        assert_eq!(plain_text(&chunk), source);
        assert_eq!(
            styled_text(&chunk, TextStyle::Code).len(),
            MAX_STYLE_RUNS - 1
        );
        assert!(chunk.items().len() < 128 * 1024);
    }

    #[test]
    fn ordinary_line_item_budget_exact_and_one_over_degrade_without_error() {
        let exact_lines = (MAX_MARKUP_FRAME_ITEMS - MARKUP_ITEM_HEADROOM) / 2;
        let exact = "x\n".repeat(exact_lines);
        let exact_chunk = render(&[&exact]);
        assert_eq!(plain_text(&exact_chunk), exact);

        let over = "x\n".repeat(exact_lines + 1);
        let over_chunk = render(&[&over]);
        assert_eq!(plain_text(&over_chunk), format!("{DISPLAY_OMITTED}\n"));
    }

    #[test]
    fn ordinary_text_byte_budget_exact_and_one_over_degrade_without_error() {
        let exact = "x".repeat(MAX_MARKUP_FRAME_TEXT_BYTES);
        let exact_chunk = render(&[&exact]);
        assert_eq!(plain_text(&exact_chunk), exact);

        let over = "x".repeat(MAX_MARKUP_FRAME_TEXT_BYTES + 1);
        let over_chunk = render(&[&over]);
        assert_eq!(plain_text(&over_chunk), format!("{DISPLAY_OMITTED}\n"));
    }

    #[test]
    fn style_run_limit_is_deterministic_and_one_over_becomes_plain() {
        let exact = "`x`a".repeat(MAX_STYLE_RUNS);
        let exact_chunk = render(&[&exact]);
        assert_eq!(plain_text(&exact_chunk), exact);
        assert_eq!(
            styled_text(&exact_chunk, TextStyle::Code)
                .matches("`x`")
                .count(),
            MAX_STYLE_RUNS
        );

        let over = "`x`a".repeat(MAX_STYLE_RUNS + 1);
        let over_chunk = render(&[&over[..over.len() / 2], &over[over.len() / 2..]]);
        assert_eq!(plain_text(&over_chunk), over);
        assert_eq!(
            styled_text(&over_chunk, TextStyle::Code)
                .matches("`x`")
                .count(),
            MAX_STYLE_RUNS
        );
    }

    #[test]
    fn diff_headers_precede_add_remove_and_malformed_hunks_stay_code() {
        let source = "```diff\n--- a\n+++ b\n@@ nope @@\n-list\n+item\n```\n";
        let chunk = render(&[source]);
        assert_eq!(styled_text(&chunk, TextStyle::DiffHeader), "--- a+++ b");
        assert!(styled_text(&chunk, TextStyle::Code).contains("@@ nope @@"));
        assert_eq!(styled_text(&chunk, TextStyle::DiffRemove), "-list");
        assert_eq!(styled_text(&chunk, TextStyle::DiffAdd), "+item");
    }

    #[test]
    fn entities_are_literal_and_cannot_create_terminal_or_line_controls() {
        let source = "# &#27; &#x202e; &#10;\n";
        let chunk = render(&[source]);
        assert_eq!(plain_text(&chunk), source);
        assert!(!plain_text(&chunk).contains('\u{1b}'));
        assert_eq!(plain_text(&chunk).matches('\n').count(), 1);
    }

    #[test]
    fn pending_source_is_redacted_from_debug() {
        let mut state = MarkupState::default();
        let mut builder = PresentedChunk::builder();
        let mut at_line_start = true;
        state
            .push("`secret-token", &mut builder, &mut at_line_start)
            .unwrap();
        assert!(!format!("{state:?}").contains("secret-token"));
    }
}
