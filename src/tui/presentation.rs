use std::fmt;

use thiserror::Error;

use super::visible::must_escape;

const MAX_PRESENTED_ITEMS: usize = 128 * 1024;
const MAX_PRESENTED_TEXT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TextStyle {
    Plain,
    Muted,
    Accent,
    User,
    Assistant,
    Warning,
    Error,
    Success,
}

pub(crate) enum PresentedItem {
    Text { style: TextStyle, text: String },
    LineFeed,
}

impl fmt::Debug for PresentedItem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Text { style, text } => formatter
                .debug_struct("Text")
                .field("style", style)
                .field("bytes", &text.len())
                .finish(),
            Self::LineFeed => formatter.write_str("LineFeed"),
        }
    }
}

pub(crate) struct PresentedChunk {
    items: Vec<PresentedItem>,
    text_bytes: usize,
}

impl fmt::Debug for PresentedChunk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PresentedChunk")
            .field("items", &self.items.len())
            .field("text_bytes", &self.text_bytes)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum PresentationError {
    #[error("CLI_OUTPUT_CAPACITY")]
    Capacity,
    #[error("CLI_OUTPUT_LIMIT")]
    Limit,
    #[error("CLI_OUTPUT_STATE")]
    InvalidText,
}

impl PresentedChunk {
    pub(crate) fn builder() -> PresentedChunkBuilder {
        PresentedChunkBuilder {
            items: Vec::new(),
            text_bytes: 0,
        }
    }

    pub(crate) fn items(&self) -> &[PresentedItem] {
        &self.items
    }

    pub(crate) fn text_bytes(&self) -> usize {
        self.text_bytes
    }
}

pub(crate) struct PresentedChunkBuilder {
    items: Vec<PresentedItem>,
    text_bytes: usize,
}

impl PresentedChunkBuilder {
    pub(crate) fn push_text(
        &mut self,
        style: TextStyle,
        text: &str,
    ) -> Result<(), PresentationError> {
        if text.is_empty() {
            return Ok(());
        }
        if text
            .chars()
            .any(|character| character.is_control() || must_escape(character))
        {
            return Err(PresentationError::InvalidText);
        }
        let next_bytes = self
            .text_bytes
            .checked_add(text.len())
            .ok_or(PresentationError::Limit)?;
        if next_bytes > MAX_PRESENTED_TEXT_BYTES {
            return Err(PresentationError::Limit);
        }
        if let Some(PresentedItem::Text {
            style: previous_style,
            text: previous,
        }) = self.items.last_mut()
        {
            if *previous_style == style {
                previous
                    .try_reserve(text.len())
                    .map_err(|_| PresentationError::Capacity)?;
                previous.push_str(text);
                self.text_bytes = next_bytes;
                return Ok(());
            }
        }
        if self.items.len() == MAX_PRESENTED_ITEMS {
            return Err(PresentationError::Limit);
        }
        self.items
            .try_reserve(1)
            .map_err(|_| PresentationError::Capacity)?;
        let mut copy = String::new();
        copy.try_reserve_exact(text.len())
            .map_err(|_| PresentationError::Capacity)?;
        copy.push_str(text);
        self.items.push(PresentedItem::Text { style, text: copy });
        self.text_bytes = next_bytes;
        Ok(())
    }

    pub(crate) fn push_line_feed(&mut self) -> Result<(), PresentationError> {
        if self.items.len() == MAX_PRESENTED_ITEMS {
            return Err(PresentationError::Limit);
        }
        self.items
            .try_reserve(1)
            .map_err(|_| PresentationError::Capacity)?;
        self.items.push(PresentedItem::LineFeed);
        Ok(())
    }

    pub(crate) fn finish(self) -> PresentedChunk {
        PresentedChunk {
            items: self.items,
            text_bytes: self.text_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PresentationError, PresentedChunk, TextStyle};

    #[test]
    fn adjacent_styles_merge_but_line_feeds_remain_structural() {
        let mut builder = PresentedChunk::builder();
        builder.push_text(TextStyle::Assistant, "hello").unwrap();
        builder.push_text(TextStyle::Assistant, " world").unwrap();
        builder.push_line_feed().unwrap();
        builder.push_text(TextStyle::Muted, "done").unwrap();
        let chunk = builder.finish();
        assert_eq!(chunk.items().len(), 3);
        assert_eq!(chunk.text_bytes(), 15);
        assert!(!format!("{chunk:?}").contains("hello"));
    }

    #[test]
    fn terminal_controls_and_newlines_cannot_hide_in_text_runs() {
        for text in [
            "bad\ntext",
            "bad\rtext",
            "bad\u{1b}text",
            "bad\ttext",
            "bad\u{202e}text",
        ] {
            let mut builder = PresentedChunk::builder();
            assert_eq!(
                builder.push_text(TextStyle::Plain, text),
                Err(PresentationError::InvalidText)
            );
        }
    }
}
