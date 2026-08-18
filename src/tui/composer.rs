use std::{collections::VecDeque, fmt, ops::Range};

use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

use super::visible::VisibleChar;

pub(crate) const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_UNDO_RECORDS: usize = 128;
const MAX_UNDO_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_YANK_BYTES: usize = MAX_PROMPT_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CursorPosition {
    pub(crate) logical_row: usize,
    pub(crate) display_column: usize,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum ComposerError {
    #[error("CLI_INPUT_PROMPT_TOO_LARGE")]
    PromptTooLarge,
    #[error("CLI_INPUT_CAPACITY")]
    Capacity,
    #[error("CLI_INPUT_STATE_INVALID")]
    InvalidState,
    #[error("CLI_INPUT_REVISION_EXHAUSTED")]
    RevisionExhausted,
}

struct UndoRecord {
    start: usize,
    inserted_bytes: usize,
    deleted: String,
    cursor_before: usize,
}

impl fmt::Debug for UndoRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UndoRecord")
            .field("start", &self.start)
            .field("inserted_bytes", &self.inserted_bytes)
            .field("deleted_bytes", &self.deleted.len())
            .field("cursor_before", &self.cursor_before)
            .finish()
    }
}

#[derive(Default)]
pub(crate) struct Composer {
    text: String,
    cursor: usize,
    content_revision: u64,
    preferred_column: Option<usize>,
    undo: VecDeque<UndoRecord>,
    undo_payload_bytes: usize,
    yank: String,
}

impl fmt::Debug for Composer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Composer")
            .field("text_bytes", &self.text.len())
            .field("cursor", &self.cursor)
            .field("content_revision", &self.content_revision)
            .field("preferred_column", &self.preferred_column)
            .field("undo_records", &self.undo.len())
            .field("undo_payload_bytes", &self.undo_payload_bytes)
            .field("yank_bytes", &self.yank.len())
            .finish()
    }
}

impl Composer {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.text.len()
    }

    pub(crate) fn content_revision(&self) -> u64 {
        self.content_revision
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn cursor_position(&self) -> CursorPosition {
        let before = &self.text[..self.cursor];
        let logical_row = before.bytes().filter(|byte| *byte == b'\n').count();
        let line = before.rsplit_once('\n').map_or(before, |(_, line)| line);
        CursorPosition {
            logical_row,
            display_column: visible_input_width(line),
        }
    }

    #[cfg(test)]
    pub(crate) fn visual_cursor_position(
        &self,
        width: usize,
    ) -> Result<CursorPosition, ComposerError> {
        if width == 0 {
            return Err(ComposerError::InvalidState);
        }
        Ok(visual_position_at(&self.text, self.cursor, width))
    }

    pub(crate) fn insert_char(&mut self, character: char) -> Result<(), ComposerError> {
        let mut encoded = [0_u8; 4];
        self.insert_text(character.encode_utf8(&mut encoded))
    }

    pub(crate) fn insert_text(&mut self, text: &str) -> Result<(), ComposerError> {
        if text.is_empty() {
            return Ok(());
        }
        self.apply_edit(self.cursor..self.cursor, text, false)
    }

    pub(crate) fn insert_paste(&mut self, text: &str) -> Result<(), ComposerError> {
        let mut normalized = String::new();
        normalized
            .try_reserve_exact(text.len())
            .map_err(|_| ComposerError::Capacity)?;
        let mut characters = text.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\r' {
                if characters.peek() == Some(&'\n') {
                    let _ = characters.next();
                }
                normalized.push('\n');
            } else {
                normalized.push(character);
            }
        }
        self.insert_text(&normalized)
    }

    pub(crate) fn insert_newline(&mut self) -> Result<(), ComposerError> {
        self.insert_text("\n")
    }

    pub(crate) fn move_left(&mut self) -> bool {
        let Some(previous) = previous_grapheme_start(&self.text, self.cursor) else {
            return false;
        };
        self.cursor = previous;
        self.preferred_column = None;
        true
    }

    pub(crate) fn move_right(&mut self) -> bool {
        let Some(next) = next_grapheme_end(&self.text, self.cursor) else {
            return false;
        };
        self.cursor = next;
        self.preferred_column = None;
        true
    }

    pub(crate) fn move_line_start(&mut self) -> bool {
        let start = line_start(&self.text, self.cursor);
        let moved = self.cursor != start;
        self.cursor = start;
        self.preferred_column = None;
        moved
    }

    pub(crate) fn move_line_end(&mut self) -> bool {
        let end = line_end(&self.text, self.cursor);
        let moved = self.cursor != end;
        self.cursor = end;
        self.preferred_column = None;
        moved
    }

    pub(crate) fn move_up(&mut self, width: usize) -> Result<bool, ComposerError> {
        self.move_vertical(width, false)
    }

    pub(crate) fn move_down(&mut self, width: usize) -> Result<bool, ComposerError> {
        self.move_vertical(width, true)
    }

    pub(crate) fn backspace(&mut self) -> Result<bool, ComposerError> {
        let Some(start) = previous_grapheme_start(&self.text, self.cursor) else {
            return Ok(false);
        };
        self.apply_edit(start..self.cursor, "", false)?;
        Ok(true)
    }

    pub(crate) fn delete(&mut self) -> Result<bool, ComposerError> {
        let Some(end) = next_grapheme_end(&self.text, self.cursor) else {
            return Ok(false);
        };
        self.apply_edit(self.cursor..end, "", false)?;
        Ok(true)
    }

    pub(crate) fn erase_word(&mut self) -> Result<bool, ComposerError> {
        let mut start = self.cursor;
        while let Some((previous, grapheme)) = previous_grapheme(&self.text, start) {
            if !grapheme.chars().all(char::is_whitespace) {
                break;
            }
            start = previous;
        }
        while let Some((previous, grapheme)) = previous_grapheme(&self.text, start) {
            if grapheme.chars().all(char::is_whitespace) {
                break;
            }
            start = previous;
        }
        if start == self.cursor {
            return Ok(false);
        }
        self.apply_edit(start..self.cursor, "", true)?;
        Ok(true)
    }

    pub(crate) fn clear_before_cursor(&mut self) -> Result<bool, ComposerError> {
        let start = line_start(&self.text, self.cursor);
        if start == self.cursor {
            return Ok(false);
        }
        self.apply_edit(start..self.cursor, "", true)?;
        Ok(true)
    }

    pub(crate) fn clear_after_cursor(&mut self) -> Result<bool, ComposerError> {
        let end = line_end(&self.text, self.cursor);
        if end == self.cursor {
            return Ok(false);
        }
        self.apply_edit(self.cursor..end, "", true)?;
        Ok(true)
    }

    pub(crate) fn yank(&mut self) -> Result<bool, ComposerError> {
        if self.yank.is_empty() {
            return Ok(false);
        }
        let yank = try_copy(&self.yank)?;
        self.insert_text(&yank)?;
        Ok(true)
    }

    pub(crate) fn undo(&mut self) -> Result<bool, ComposerError> {
        let Some(record) = self.undo.back() else {
            return Ok(false);
        };
        let end = record
            .start
            .checked_add(record.inserted_bytes)
            .ok_or(ComposerError::InvalidState)?;
        if end > self.text.len()
            || !self.text.is_char_boundary(record.start)
            || !self.text.is_char_boundary(end)
        {
            return Err(ComposerError::InvalidState);
        }
        let final_len = self
            .text
            .len()
            .checked_sub(record.inserted_bytes)
            .and_then(|length| length.checked_add(record.deleted.len()))
            .ok_or(ComposerError::InvalidState)?;
        if final_len > MAX_PROMPT_BYTES {
            return Err(ComposerError::InvalidState);
        }
        let extra = final_len.saturating_sub(self.text.len());
        self.text
            .try_reserve(extra)
            .map_err(|_| ComposerError::Capacity)?;
        let next_revision = self
            .content_revision
            .checked_add(1)
            .ok_or(ComposerError::RevisionExhausted)?;
        let next_undo_payload_bytes = self
            .undo_payload_bytes
            .checked_sub(record.deleted.len())
            .ok_or(ComposerError::InvalidState)?;

        let record = self.undo.pop_back().ok_or(ComposerError::InvalidState)?;
        self.undo_payload_bytes = next_undo_payload_bytes;
        self.text
            .replace_range(record.start..end, record.deleted.as_str());
        self.cursor = record.cursor_before;
        self.content_revision = next_revision;
        self.preferred_column = None;
        debug_assert!(self.invariants_hold());
        Ok(true)
    }

    pub(crate) fn replace_all(&mut self, text: &str, cursor: usize) -> Result<(), ComposerError> {
        if text.len() > MAX_PROMPT_BYTES
            || cursor > text.len()
            || !text.is_char_boundary(cursor)
            || !is_grapheme_boundary(text, cursor)
        {
            return Err(ComposerError::InvalidState);
        }
        let replacement = try_copy(text)?;
        let next_revision = self
            .content_revision
            .checked_add(1)
            .ok_or(ComposerError::RevisionExhausted)?;
        self.text = replacement;
        self.cursor = cursor;
        self.clear_undo();
        self.content_revision = next_revision;
        self.preferred_column = None;
        debug_assert!(self.invariants_hold());
        Ok(())
    }

    pub(super) fn take_draft(&mut self) -> Result<String, ComposerError> {
        let next_revision = self
            .content_revision
            .checked_add(1)
            .ok_or(ComposerError::RevisionExhausted)?;
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.clear_undo();
        self.content_revision = next_revision;
        self.preferred_column = None;
        debug_assert!(self.invariants_hold());
        Ok(text)
    }

    pub(super) fn restore_draft(&mut self, text: String, cursor: usize) -> Result<(), String> {
        if !self.text.is_empty()
            || text.len() > MAX_PROMPT_BYTES
            || cursor > text.len()
            || !text.is_char_boundary(cursor)
            || !is_grapheme_boundary(&text, cursor)
        {
            return Err(text);
        }
        let Some(next_revision) = self.content_revision.checked_add(1) else {
            return Err(text);
        };
        self.text = text;
        self.cursor = cursor;
        self.clear_undo();
        self.content_revision = next_revision;
        self.preferred_column = None;
        debug_assert!(self.invariants_hold());
        Ok(())
    }

    pub(super) fn swap_draft(
        &mut self,
        text: String,
        cursor: usize,
    ) -> Result<(String, usize), String> {
        if text.len() > MAX_PROMPT_BYTES
            || cursor > text.len()
            || !text.is_char_boundary(cursor)
            || !is_grapheme_boundary(&text, cursor)
        {
            return Err(text);
        }
        let Some(next_revision) = self.content_revision.checked_add(1) else {
            return Err(text);
        };
        let previous = std::mem::replace(&mut self.text, text);
        let previous_cursor = self.cursor;
        self.cursor = cursor;
        self.clear_undo();
        self.content_revision = next_revision;
        self.preferred_column = None;
        debug_assert!(self.invariants_hold());
        Ok((previous, previous_cursor))
    }

    fn apply_edit(
        &mut self,
        range: Range<usize>,
        replacement: &str,
        update_yank: bool,
    ) -> Result<(), ComposerError> {
        if range.start > range.end
            || range.end > self.text.len()
            || !self.text.is_char_boundary(range.start)
            || !self.text.is_char_boundary(range.end)
            || !is_grapheme_boundary(&self.text, range.start)
            || !is_grapheme_boundary(&self.text, range.end)
        {
            return Err(ComposerError::InvalidState);
        }
        let removed_bytes = range.end - range.start;
        let final_len = self
            .text
            .len()
            .checked_sub(removed_bytes)
            .and_then(|length| length.checked_add(replacement.len()))
            .ok_or(ComposerError::PromptTooLarge)?;
        if final_len > MAX_PROMPT_BYTES {
            return Err(ComposerError::PromptTooLarge);
        }
        let next_revision = self
            .content_revision
            .checked_add(1)
            .ok_or(ComposerError::RevisionExhausted)?;
        let deleted = try_copy(&self.text[range.clone()])?;
        let next_yank = if update_yank {
            if deleted.len() > MAX_YANK_BYTES {
                return Err(ComposerError::PromptTooLarge);
            }
            Some(try_copy(&deleted)?)
        } else {
            None
        };
        self.undo
            .try_reserve(1)
            .map_err(|_| ComposerError::Capacity)?;
        self.text
            .try_reserve(final_len.saturating_sub(self.text.len()))
            .map_err(|_| ComposerError::Capacity)?;

        let mut evict = 0_usize;
        let mut undo_bytes = self
            .undo_payload_bytes
            .checked_add(deleted.len())
            .ok_or(ComposerError::Capacity)?;
        loop {
            let remaining_records = self
                .undo
                .len()
                .checked_sub(evict)
                .ok_or(ComposerError::InvalidState)?;
            if remaining_records < MAX_UNDO_RECORDS && undo_bytes <= MAX_UNDO_PAYLOAD_BYTES {
                break;
            }
            let old = self.undo.get(evict).ok_or(ComposerError::InvalidState)?;
            undo_bytes = undo_bytes
                .checked_sub(old.deleted.len())
                .ok_or(ComposerError::InvalidState)?;
            evict += 1;
        }

        let start = range.start;
        let cursor_before = self.cursor;
        self.text.replace_range(range, replacement);
        self.cursor = boundary_at_or_after(&self.text, start + replacement.len());
        for _ in 0..evict {
            let _ = self.undo.pop_front();
        }
        self.undo.push_back(UndoRecord {
            start,
            inserted_bytes: replacement.len(),
            deleted,
            cursor_before,
        });
        self.undo_payload_bytes = undo_bytes;
        if let Some(yank) = next_yank {
            self.yank = yank;
        }
        self.content_revision = next_revision;
        self.preferred_column = None;
        debug_assert!(self.invariants_hold());
        Ok(())
    }

    fn clear_undo(&mut self) {
        self.undo.clear();
        self.undo_payload_bytes = 0;
    }

    fn move_vertical(&mut self, width: usize, down: bool) -> Result<bool, ComposerError> {
        if width == 0 {
            return Err(ComposerError::InvalidState);
        }
        let current = visual_position_at(&self.text, self.cursor, width);
        let target_row = if down {
            current
                .logical_row
                .checked_add(1)
                .ok_or(ComposerError::InvalidState)?
        } else {
            let Some(row) = current.logical_row.checked_sub(1) else {
                return Ok(false);
            };
            row
        };
        let preferred = self.preferred_column.unwrap_or(current.display_column);
        let Some(cursor) = closest_boundary_on_row(&self.text, target_row, preferred, width) else {
            return Ok(false);
        };
        self.cursor = cursor;
        self.preferred_column = Some(preferred);
        Ok(true)
    }

    fn invariants_hold(&self) -> bool {
        self.text.len() <= MAX_PROMPT_BYTES
            && self.cursor <= self.text.len()
            && self.text.is_char_boundary(self.cursor)
            && is_grapheme_boundary(&self.text, self.cursor)
            && self.undo.len() <= MAX_UNDO_RECORDS
            && self.undo_payload_bytes <= MAX_UNDO_PAYLOAD_BYTES
            && self.yank.len() <= MAX_YANK_BYTES
            && self.undo_payload_bytes
                == self
                    .undo
                    .iter()
                    .map(|record| record.deleted.len())
                    .sum::<usize>()
    }
}

fn try_copy(text: &str) -> Result<String, ComposerError> {
    let mut copy = String::new();
    copy.try_reserve_exact(text.len())
        .map_err(|_| ComposerError::Capacity)?;
    copy.push_str(text);
    Ok(copy)
}

fn previous_grapheme_start(text: &str, cursor: usize) -> Option<usize> {
    text[..cursor]
        .grapheme_indices(true)
        .next_back()
        .map(|(index, _)| index)
}

fn previous_grapheme(text: &str, cursor: usize) -> Option<(usize, &str)> {
    text[..cursor].grapheme_indices(true).next_back()
}

fn next_grapheme_end(text: &str, cursor: usize) -> Option<usize> {
    text[cursor..]
        .graphemes(true)
        .next()
        .map(|grapheme| cursor + grapheme.len())
}

fn line_start(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .grapheme_indices(true)
        .filter(|(_, grapheme)| grapheme.contains('\n'))
        .next_back()
        .map_or(0, |(start, grapheme)| start + grapheme.len())
}

fn line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .grapheme_indices(true)
        .find(|(_, grapheme)| grapheme.contains('\n'))
        .map_or(text.len(), |(start, _)| cursor + start)
}

fn is_grapheme_boundary(text: &str, cursor: usize) -> bool {
    cursor == text.len()
        || text
            .grapheme_indices(true)
            .any(|(boundary, _)| boundary == cursor)
}

fn boundary_at_or_after(text: &str, cursor: usize) -> usize {
    if cursor >= text.len() {
        return text.len();
    }
    text.grapheme_indices(true)
        .map(|(boundary, _)| boundary)
        .find(|boundary| *boundary >= cursor)
        .unwrap_or(text.len())
}

fn visible_input_width(line: &str) -> usize {
    let mut column = 0_usize;
    for grapheme in line.graphemes(true) {
        if grapheme.chars().any(|character| {
            VisibleChar::classify(character, false)
                .escaped_cell_width()
                .is_some()
        }) {
            for character in grapheme.chars() {
                let visible = VisibleChar::classify(character, false);
                column += visible.escaped_cell_width().unwrap_or_else(|| {
                    let mut encoded = [0_u8; 4];
                    UnicodeWidthStr::width(character.encode_utf8(&mut encoded))
                });
            }
        } else {
            column += UnicodeWidthStr::width(grapheme);
        }
    }
    column
}

fn visual_position_at(text: &str, cursor: usize, width: usize) -> CursorPosition {
    let mut position = CursorPosition {
        logical_row: 0,
        display_column: 0,
    };
    for (start, grapheme) in text.grapheme_indices(true) {
        if start >= cursor {
            break;
        }
        advance_visual_position(&mut position, grapheme, width);
    }
    position
}

fn closest_boundary_on_row(
    text: &str,
    target_row: usize,
    preferred_column: usize,
    width: usize,
) -> Option<usize> {
    let mut position = CursorPosition {
        logical_row: 0,
        display_column: 0,
    };
    let mut best = (position.logical_row == target_row).then_some((
        position.display_column.abs_diff(preferred_column),
        position.display_column,
        0,
    ));
    for (start, grapheme) in text.grapheme_indices(true) {
        advance_visual_position(&mut position, grapheme, width);
        let boundary = start + grapheme.len();
        if position.logical_row == target_row {
            let candidate = (
                position.display_column.abs_diff(preferred_column),
                position.display_column,
                boundary,
            );
            if best.is_none_or(|best| candidate < best) {
                best = Some(candidate);
            }
        } else if position.logical_row > target_row && best.is_some() {
            break;
        }
    }
    best.map(|(_, _, boundary)| boundary)
}

fn advance_visual_position(position: &mut CursorPosition, grapheme: &str, width: usize) {
    if grapheme.contains('\n') {
        for character in grapheme.chars() {
            if character == '\n' {
                position.logical_row += 1;
                position.display_column = 0;
            } else {
                let mut encoded = [0_u8; 4];
                let text = character.encode_utf8(&mut encoded);
                advance_cells(position, visible_input_width(text), width);
            }
        }
        return;
    }
    advance_cells(position, visible_input_width(grapheme), width);
}

fn advance_cells(position: &mut CursorPosition, cell_width: usize, width: usize) {
    if cell_width == 0 {
        return;
    }
    if position.display_column != 0 && position.display_column + cell_width > width {
        position.logical_row += 1;
        position.display_column = 0;
    }
    let total = position.display_column + cell_width;
    position.logical_row += (total - 1) / width;
    position.display_column = ((total - 1) % width) + 1;
}

#[cfg(test)]
mod tests {
    use super::{Composer, ComposerError, MAX_PROMPT_BYTES};

    #[test]
    fn grapheme_cursor_never_enters_combining_zwj_or_flag_clusters() {
        for cluster in ["e\u{301}", "👨‍👩‍👧‍👦", "👍🏽", "🇨🇳", "中"] {
            let mut composer = Composer::default();
            composer.insert_text(cluster).unwrap();
            assert_eq!(composer.cursor(), cluster.len());
            assert!(composer.move_left());
            assert_eq!(composer.cursor(), 0);
            assert!(composer.move_right());
            assert_eq!(composer.cursor(), cluster.len());
            assert!(composer.backspace().unwrap());
            assert!(composer.is_empty());
            assert!(composer.undo().unwrap());
            assert_eq!(composer.text(), cluster);
        }
    }

    #[test]
    fn inserting_joiners_recomputes_the_resulting_boundary() {
        let mut composer = Composer::default();
        composer.insert_text("👨👩").unwrap();
        composer.move_left();
        composer.insert_text("\u{200d}").unwrap();
        assert_eq!(composer.text(), "👨‍👩");
        assert_eq!(composer.cursor(), composer.text().len());
        assert!(composer.undo().unwrap());
        assert_eq!(composer.text(), "👨👩");
        assert_eq!(composer.cursor(), "👨".len());
    }

    #[test]
    fn prompt_limit_is_utf8_bytes_and_overflow_is_atomic() {
        let mut composer = Composer::default();
        composer
            .insert_text(&"é".repeat(MAX_PROMPT_BYTES / 2))
            .unwrap();
        let revision = composer.content_revision();
        let cursor = composer.cursor();
        assert_eq!(
            composer.insert_char('x'),
            Err(ComposerError::PromptTooLarge)
        );
        assert_eq!(composer.text().len(), MAX_PROMPT_BYTES);
        assert_eq!(composer.cursor(), cursor);
        assert_eq!(composer.content_revision(), revision);
        assert!(composer.undo().unwrap());
        assert!(composer.is_empty());
    }

    #[test]
    fn multiline_position_splits_lines_before_width_calculation() {
        let mut composer = Composer::default();
        composer.insert_text("ab\n中\tX\u{7}").unwrap();
        let position = composer.cursor_position();
        assert_eq!(position.logical_row, 1);
        assert_eq!(position.display_column, 10);
        composer.move_line_start();
        assert_eq!(composer.cursor_position().display_column, 0);
        composer.move_line_end();
        assert_eq!(composer.cursor_position().display_column, 10);
    }

    #[test]
    fn newline_delete_and_clear_after_are_independent_edits() {
        let mut composer = Composer::default();
        composer.insert_text("ab").unwrap();
        composer.move_left();
        assert!(composer.delete().unwrap());
        assert_eq!(composer.text(), "a");
        composer.insert_newline().unwrap();
        composer.insert_text("tail").unwrap();
        composer.move_line_start();
        assert!(composer.clear_after_cursor().unwrap());
        assert_eq!(composer.text(), "a\n");
        assert!(composer.undo().unwrap());
        assert_eq!(composer.text(), "a\ntail");
    }

    #[test]
    fn crlf_history_is_safe_and_new_paste_is_normalized_to_lf() {
        let mut composer = Composer::default();
        composer.replace_all("a\r\nb", 1).unwrap();
        composer.move_line_end();
        assert_eq!(composer.cursor(), 1);
        assert!(composer.move_right());
        assert_eq!(composer.cursor(), 3);
        assert_eq!(composer.visual_cursor_position(8).unwrap().logical_row, 1);
        assert!(composer.backspace().unwrap());
        assert_eq!(composer.text(), "ab");
        assert!(composer.undo().unwrap());
        assert_eq!(composer.text(), "a\r\nb");
        composer.move_line_start();
        assert_eq!(composer.cursor(), 3);

        let mut paste = Composer::default();
        paste.insert_paste("one\r\ntwo\rthree").unwrap();
        assert_eq!(paste.text(), "one\ntwo\nthree");
    }

    #[test]
    fn visual_up_and_down_preserve_the_preferred_cell_column() {
        let mut composer = Composer::default();
        composer.insert_text("abcdef\n中x\n123456").unwrap();
        assert_eq!(
            composer.visual_cursor_position(4).unwrap(),
            super::CursorPosition {
                logical_row: 4,
                display_column: 2,
            }
        );
        assert!(composer.move_up(4).unwrap());
        assert_eq!(composer.visual_cursor_position(4).unwrap().logical_row, 3);
        assert!(composer.move_up(4).unwrap());
        assert_eq!(
            composer.visual_cursor_position(4).unwrap(),
            super::CursorPosition {
                logical_row: 2,
                display_column: 2,
            }
        );
        assert!(composer.move_down(4).unwrap());
        assert_eq!(composer.visual_cursor_position(4).unwrap().logical_row, 3);
    }

    #[test]
    fn undo_bounds_evict_whole_oldest_records() {
        let mut composer = Composer::default();
        for _ in 0..64 {
            composer.insert_char('x').unwrap();
            composer.backspace().unwrap();
        }
        assert_eq!(composer.undo.len(), super::MAX_UNDO_RECORDS);
        composer.insert_char('y').unwrap();
        assert_eq!(composer.undo.len(), super::MAX_UNDO_RECORDS);

        let payload = "z".repeat(MAX_PROMPT_BYTES);
        let mut composer = Composer::default();
        for _ in 0..16 {
            composer.insert_text(&payload).unwrap();
            composer.clear_before_cursor().unwrap();
        }
        assert_eq!(composer.undo_payload_bytes, super::MAX_UNDO_PAYLOAD_BYTES);
        composer.insert_text(&payload).unwrap();
        composer.clear_before_cursor().unwrap();
        assert!(composer.undo_payload_bytes <= super::MAX_UNDO_PAYLOAD_BYTES);
        assert!(composer.undo.len() <= super::MAX_UNDO_RECORDS);
    }

    #[test]
    fn kill_yank_and_undo_are_grapheme_safe() {
        let mut composer = Composer::default();
        composer.insert_text("alpha 👨‍👩‍👧‍👦 beta").unwrap();
        assert!(composer.erase_word().unwrap());
        assert_eq!(composer.text(), "alpha 👨‍👩‍👧‍👦 ");
        assert!(composer.yank().unwrap());
        assert_eq!(composer.text(), "alpha 👨‍👩‍👧‍👦 beta");
        assert!(composer.undo().unwrap());
        assert_eq!(composer.text(), "alpha 👨‍👩‍👧‍👦 ");
        assert!(composer.undo().unwrap());
        assert_eq!(composer.text(), "alpha 👨‍👩‍👧‍👦 beta");
    }

    #[test]
    fn replacing_and_restoring_a_draft_cannot_cross_an_undo_boundary() {
        let mut composer = Composer::default();
        composer.insert_text("queued secret").unwrap();
        let draft = composer.take_draft().unwrap();
        assert!(composer.is_empty());
        assert!(!composer.undo().unwrap());
        composer
            .restore_draft(draft, "queued secret".len())
            .unwrap();
        assert_eq!(composer.text(), "queued secret");
        assert!(!composer.undo().unwrap());
    }

    #[test]
    fn debug_does_not_expose_prompt_yank_or_undo_text() {
        let mut composer = Composer::default();
        composer.insert_text("SECRET_DRAFT").unwrap();
        composer.clear_before_cursor().unwrap();
        let debug = format!("{composer:?}");
        assert!(!debug.contains("SECRET_DRAFT"));
        assert!(debug.contains("yank_bytes"));
    }

    #[test]
    fn replace_all_rejects_a_cursor_inside_a_grapheme() {
        let mut composer = Composer::default();
        let text = "e\u{301}";
        assert_eq!(
            composer.replace_all(text, 1),
            Err(ComposerError::InvalidState)
        );
        assert!(composer.is_empty());
    }
}
