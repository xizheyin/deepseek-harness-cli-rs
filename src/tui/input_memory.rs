use std::{collections::VecDeque, fmt};

use thiserror::Error;

use super::composer::{Composer, ComposerError, MAX_PROMPT_BYTES};

const MAX_QUEUE_ITEMS: usize = 8;
const MAX_QUEUE_BYTES: usize = 256 * 1024;
const MAX_HISTORY_ITEMS: usize = 128;
const MAX_HISTORY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct LocalPromptId(u64);

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum InputMemoryError {
    #[error("CLI_INPUT_EMPTY")]
    Empty,
    #[error("CLI_INPUT_PROMPT_TOO_LARGE")]
    PromptTooLarge,
    #[error("CLI_INPUT_QUEUE_FULL")]
    QueueFull,
    #[error("CLI_INPUT_QUEUE_BYTES")]
    QueueBytes,
    #[error("CLI_INPUT_CAPACITY")]
    Capacity,
    #[error("CLI_INPUT_ID_EXHAUSTED")]
    IdExhausted,
    #[error("CLI_INPUT_STATE_INVALID")]
    InvalidState,
    #[error("CLI_INPUT_COMPOSER")]
    Composer,
}

impl From<ComposerError> for InputMemoryError {
    fn from(_: ComposerError) -> Self {
        Self::Composer
    }
}

struct QueuedPrompt {
    id: LocalPromptId,
    text: String,
}

impl fmt::Debug for QueuedPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueuedPrompt")
            .field("id", &self.id)
            .field("bytes", &self.text.len())
            .finish()
    }
}

#[derive(Default)]
pub(crate) struct PromptQueue {
    prompts: VecDeque<QueuedPrompt>,
    total_bytes: usize,
    next_id: u64,
    reserved_front: Option<LocalPromptId>,
}

impl fmt::Debug for PromptQueue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptQueue")
            .field("count", &self.prompts.len())
            .field("total_bytes", &self.total_bytes)
            .field("next_id", &self.next_id)
            .field("front_reserved", &self.reserved_front.is_some())
            .finish()
    }
}

impl PromptQueue {
    pub(crate) fn len(&self) -> usize {
        self.prompts
            .len()
            .saturating_sub(usize::from(self.reserved_front.is_some()))
    }

    pub(crate) fn total_bytes(&self) -> usize {
        let reserved_bytes = self
            .reserved_front
            .and_then(|id| self.prompts.front().filter(|prompt| prompt.id == id))
            .map_or(0, |prompt| prompt.text.len());
        self.total_bytes.saturating_sub(reserved_bytes)
    }

    pub(crate) fn enqueue_from(
        &mut self,
        composer: &mut Composer,
    ) -> Result<LocalPromptId, InputMemoryError> {
        let bytes = composer.byte_len();
        if bytes == 0 {
            return Err(InputMemoryError::Empty);
        }
        if bytes > MAX_PROMPT_BYTES {
            return Err(InputMemoryError::PromptTooLarge);
        }
        if self.prompts.len() == MAX_QUEUE_ITEMS {
            return Err(InputMemoryError::QueueFull);
        }
        let next_total = self
            .total_bytes
            .checked_add(bytes)
            .ok_or(InputMemoryError::QueueBytes)?;
        if next_total > MAX_QUEUE_BYTES {
            return Err(InputMemoryError::QueueBytes);
        }
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or(InputMemoryError::IdExhausted)?;
        self.prompts
            .try_reserve(1)
            .map_err(|_| InputMemoryError::Capacity)?;
        let text = composer.take_draft()?;
        let id = LocalPromptId(next_id);
        self.prompts.push_back(QueuedPrompt { id, text });
        self.total_bytes = next_total;
        self.next_id = next_id;
        Ok(id)
    }

    #[cfg(test)]
    fn front_admission(&mut self) -> Option<PromptAdmission<'_>> {
        if self.reserved_front.is_some() {
            return None;
        }
        let id = self.prompts.front()?.id;
        Some(PromptAdmission { queue: self, id })
    }

    pub(crate) fn reserve_front(&mut self) -> Result<ReservedPrompt, InputMemoryError> {
        if self.reserved_front.is_some() {
            return Err(InputMemoryError::InvalidState);
        }
        let front = self.prompts.front().ok_or(InputMemoryError::Empty)?;
        let text = copy_prompt(&front.text)?;
        let id = front.id;
        self.reserved_front = Some(id);
        Ok(ReservedPrompt { id, text })
    }

    pub(crate) fn release_reserved(&mut self, id: LocalPromptId) -> Result<(), InputMemoryError> {
        if self.reserved_front != Some(id)
            || self.prompts.front().map(|prompt| prompt.id) != Some(id)
        {
            return Err(InputMemoryError::InvalidState);
        }
        self.reserved_front = None;
        Ok(())
    }

    pub(crate) fn commit_reserved(
        &mut self,
        id: LocalPromptId,
    ) -> Result<AdmittedPrompt, InputMemoryError> {
        if self.reserved_front != Some(id) {
            return Err(InputMemoryError::InvalidState);
        }
        let prompt = self
            .prompts
            .front()
            .filter(|prompt| prompt.id == id)
            .ok_or(InputMemoryError::InvalidState)?;
        let next_total = self
            .total_bytes
            .checked_sub(prompt.text.len())
            .ok_or(InputMemoryError::InvalidState)?;
        let prompt = self
            .prompts
            .pop_front()
            .ok_or(InputMemoryError::InvalidState)?;
        self.total_bytes = next_total;
        self.reserved_front = None;
        Ok(AdmittedPrompt {
            id: prompt.id,
            text: prompt.text,
        })
    }

    pub(crate) fn recall_latest(
        &mut self,
        composer: &mut Composer,
    ) -> Result<bool, InputMemoryError> {
        let Some(latest) = self.prompts.back() else {
            return Ok(false);
        };
        if self.reserved_front == Some(latest.id) {
            return Ok(false);
        }
        let without_latest = self
            .total_bytes
            .checked_sub(latest.text.len())
            .ok_or(InputMemoryError::InvalidState)?;
        let next_total = without_latest
            .checked_add(composer.byte_len())
            .ok_or(InputMemoryError::QueueBytes)?;
        if next_total > MAX_QUEUE_BYTES {
            return Err(InputMemoryError::QueueBytes);
        }
        let latest = self
            .prompts
            .pop_back()
            .ok_or(InputMemoryError::InvalidState)?;
        let cursor = latest.text.len();
        let (draft, _) = match composer.swap_draft(latest.text, cursor) {
            Ok(previous) => previous,
            Err(text) => {
                self.prompts.push_back(QueuedPrompt {
                    id: latest.id,
                    text,
                });
                return Err(InputMemoryError::Composer);
            }
        };
        if !draft.is_empty() {
            self.prompts.push_back(QueuedPrompt {
                id: latest.id,
                text: draft,
            });
        }
        self.total_bytes = next_total;
        Ok(true)
    }
}

pub(crate) struct ReservedPrompt {
    id: LocalPromptId,
    text: String,
}

impl fmt::Debug for ReservedPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReservedPrompt")
            .field("id", &self.id)
            .field("bytes", &self.text.len())
            .finish()
    }
}

impl ReservedPrompt {
    pub(crate) fn id(&self) -> LocalPromptId {
        self.id
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

#[must_use = "dropping an admission keeps the prompt at the front of the queue"]
#[cfg(test)]
struct PromptAdmission<'a> {
    queue: &'a mut PromptQueue,
    id: LocalPromptId,
}

#[cfg(test)]
impl fmt::Debug for PromptAdmission<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptAdmission")
            .field("id", &self.id)
            .field("bytes", &self.text().map(str::len))
            .finish()
    }
}

#[cfg(test)]
impl PromptAdmission<'_> {
    fn id(&self) -> LocalPromptId {
        self.id
    }

    fn text(&self) -> Option<&str> {
        self.queue
            .prompts
            .front()
            .filter(|prompt| prompt.id == self.id)
            .map(|prompt| prompt.text.as_str())
    }

    fn commit(self) -> Result<AdmittedPrompt, InputMemoryError> {
        let front = self
            .queue
            .prompts
            .front()
            .filter(|prompt| prompt.id == self.id)
            .ok_or(InputMemoryError::InvalidState)?;
        let next_total = self
            .queue
            .total_bytes
            .checked_sub(front.text.len())
            .ok_or(InputMemoryError::InvalidState)?;
        let prompt = self
            .queue
            .prompts
            .pop_front()
            .ok_or(InputMemoryError::InvalidState)?;
        self.queue.total_bytes = next_total;
        Ok(AdmittedPrompt {
            id: prompt.id,
            text: prompt.text,
        })
    }
}

pub(crate) struct AdmittedPrompt {
    id: LocalPromptId,
    text: String,
}

impl fmt::Debug for AdmittedPrompt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdmittedPrompt")
            .field("id", &self.id)
            .field("bytes", &self.text.len())
            .finish()
    }
}

impl AdmittedPrompt {
    pub(crate) fn id(&self) -> LocalPromptId {
        self.id
    }

    pub(crate) fn into_text(self) -> String {
        self.text
    }
}

#[derive(Default)]
pub(crate) struct PromptHistory {
    prompts: VecDeque<String>,
    total_bytes: usize,
    truncated: bool,
}

impl fmt::Debug for PromptHistory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PromptHistory")
            .field("count", &self.prompts.len())
            .field("total_bytes", &self.total_bytes)
            .field("truncated", &self.truncated)
            .finish()
    }
}

impl PromptHistory {
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.prompts.len()
    }

    #[cfg(test)]
    pub(crate) fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    #[cfg(test)]
    pub(crate) fn is_truncated(&self) -> bool {
        self.truncated
    }

    pub(crate) fn record_committed(&mut self, prompt: &str) -> Result<bool, InputMemoryError> {
        if prompt.len() > MAX_PROMPT_BYTES {
            self.truncated = true;
            return Ok(false);
        }
        let mut copy = String::new();
        copy.try_reserve_exact(prompt.len()).map_err(|_| {
            self.truncated = true;
            InputMemoryError::Capacity
        })?;
        copy.push_str(prompt);
        self.prompts.try_reserve(1).map_err(|_| {
            self.truncated = true;
            InputMemoryError::Capacity
        })?;

        let mut evict = 0_usize;
        let mut next_total = self
            .total_bytes
            .checked_add(copy.len())
            .ok_or(InputMemoryError::Capacity)?;
        loop {
            let remaining = self
                .prompts
                .len()
                .checked_sub(evict)
                .ok_or(InputMemoryError::InvalidState)?;
            if remaining < MAX_HISTORY_ITEMS && next_total <= MAX_HISTORY_BYTES {
                break;
            }
            let old = self
                .prompts
                .get(evict)
                .ok_or(InputMemoryError::InvalidState)?;
            next_total = next_total
                .checked_sub(old.len())
                .ok_or(InputMemoryError::InvalidState)?;
            evict += 1;
        }
        if evict != 0 {
            self.truncated = true;
        }
        for _ in 0..evict {
            let _ = self.prompts.pop_front();
        }
        self.prompts.push_back(copy);
        self.total_bytes = next_total;
        Ok(true)
    }

    pub(crate) fn newest_at(&self, offset: usize) -> Option<&str> {
        self.prompts
            .len()
            .checked_sub(offset.checked_add(1)?)
            .and_then(|index| self.prompts.get(index))
            .map(String::as_str)
    }
}

struct HistoryNavigation {
    offset_from_newest: usize,
    saved_draft: String,
    saved_cursor: usize,
}

impl fmt::Debug for HistoryNavigation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoryNavigation")
            .field("offset_from_newest", &self.offset_from_newest)
            .field("saved_draft_bytes", &self.saved_draft.len())
            .field("saved_cursor", &self.saved_cursor)
            .finish()
    }
}

#[derive(Default)]
pub(crate) struct InputMemory {
    composer: Composer,
    queue: PromptQueue,
    history: PromptHistory,
    history_navigation: Option<HistoryNavigation>,
}

impl fmt::Debug for InputMemory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InputMemory")
            .field("composer", &self.composer)
            .field("queue", &self.queue)
            .field("history", &self.history)
            .field("history_navigation", &self.history_navigation)
            .finish()
    }
}

impl InputMemory {
    pub(crate) fn composer(&self) -> &Composer {
        &self.composer
    }

    #[cfg(test)]
    fn composer_mut(&mut self) -> &mut Composer {
        &mut self.composer
    }

    pub(crate) fn queue(&self) -> &PromptQueue {
        &self.queue
    }

    #[cfg(test)]
    fn queue_mut(&mut self) -> &mut PromptQueue {
        &mut self.queue
    }

    #[cfg(test)]
    fn history(&self) -> &PromptHistory {
        &self.history
    }

    #[cfg(test)]
    fn history_mut(&mut self) -> &mut PromptHistory {
        &mut self.history
    }

    pub(crate) fn insert_char(&mut self, character: char) -> Result<(), InputMemoryError> {
        let before = self.composer.content_revision();
        self.composer.insert_char(character)?;
        if self.composer.content_revision() != before {
            self.history_navigation = None;
        }
        Ok(())
    }

    pub(crate) fn insert_text(&mut self, text: &str) -> Result<(), InputMemoryError> {
        let before = self.composer.content_revision();
        self.composer.insert_text(text)?;
        if self.composer.content_revision() != before {
            self.history_navigation = None;
        }
        Ok(())
    }

    pub(crate) fn insert_paste(&mut self, text: &str) -> Result<(), InputMemoryError> {
        let before = self.composer.content_revision();
        self.composer.insert_paste(text)?;
        if self.composer.content_revision() != before {
            self.history_navigation = None;
        }
        Ok(())
    }

    pub(crate) fn insert_newline(&mut self) -> Result<(), InputMemoryError> {
        self.composer.insert_newline()?;
        self.history_navigation = None;
        Ok(())
    }

    pub(crate) fn move_left(&mut self) -> bool {
        self.composer.move_left()
    }

    pub(crate) fn move_right(&mut self) -> bool {
        self.composer.move_right()
    }

    pub(crate) fn move_line_start(&mut self) -> bool {
        self.composer.move_line_start()
    }

    pub(crate) fn move_line_end(&mut self) -> bool {
        self.composer.move_line_end()
    }

    pub(crate) fn move_up_or_history(&mut self, width: usize) -> Result<bool, InputMemoryError> {
        if self.composer.move_up(width)? {
            Ok(true)
        } else if self.queue.len() != 0 {
            self.recall_latest_queue()
        } else {
            self.history_previous()
        }
    }

    pub(crate) fn move_down_or_history(&mut self, width: usize) -> Result<bool, InputMemoryError> {
        if self.composer.move_down(width)? {
            Ok(true)
        } else {
            self.history_next()
        }
    }

    pub(crate) fn backspace(&mut self) -> Result<bool, InputMemoryError> {
        let changed = self.composer.backspace()?;
        if changed {
            self.history_navigation = None;
        }
        Ok(changed)
    }

    pub(crate) fn delete(&mut self) -> Result<bool, InputMemoryError> {
        let changed = self.composer.delete()?;
        if changed {
            self.history_navigation = None;
        }
        Ok(changed)
    }

    pub(crate) fn erase_word(&mut self) -> Result<bool, InputMemoryError> {
        let changed = self.composer.erase_word()?;
        if changed {
            self.history_navigation = None;
        }
        Ok(changed)
    }

    pub(crate) fn clear_before_cursor(&mut self) -> Result<bool, InputMemoryError> {
        let changed = self.composer.clear_before_cursor()?;
        if changed {
            self.history_navigation = None;
        }
        Ok(changed)
    }

    pub(crate) fn clear_after_cursor(&mut self) -> Result<bool, InputMemoryError> {
        let changed = self.composer.clear_after_cursor()?;
        if changed {
            self.history_navigation = None;
        }
        Ok(changed)
    }

    pub(crate) fn yank(&mut self) -> Result<bool, InputMemoryError> {
        let changed = self.composer.yank()?;
        if changed {
            self.history_navigation = None;
        }
        Ok(changed)
    }

    pub(crate) fn undo(&mut self) -> Result<bool, InputMemoryError> {
        let changed = self.composer.undo()?;
        if changed {
            self.history_navigation = None;
        }
        Ok(changed)
    }

    pub(crate) fn take_draft_for_turn(&mut self) -> Result<String, InputMemoryError> {
        let draft = self.composer.take_draft()?;
        self.history_navigation = None;
        Ok(draft)
    }

    pub(crate) fn restore_uncommitted_draft(
        &mut self,
        draft: String,
        cursor: usize,
    ) -> Result<(), String> {
        self.composer.restore_draft(draft, cursor)
    }

    pub(crate) fn record_committed_human(
        &mut self,
        prompt: &str,
    ) -> Result<bool, InputMemoryError> {
        let rebased_offset = self
            .history_navigation
            .as_ref()
            .map(|navigation| {
                navigation
                    .offset_from_newest
                    .checked_add(1)
                    .ok_or(InputMemoryError::InvalidState)
            })
            .transpose()?;
        let stored = self.history.record_committed(prompt)?;
        if stored {
            if let (Some(navigation), Some(offset)) = (&mut self.history_navigation, rebased_offset)
            {
                navigation.offset_from_newest = offset;
            }
        }
        Ok(stored)
    }

    pub(crate) fn reserve_front(&mut self) -> Result<ReservedPrompt, InputMemoryError> {
        self.queue.reserve_front()
    }

    pub(crate) fn release_reserved(&mut self, id: LocalPromptId) -> Result<(), InputMemoryError> {
        self.queue.release_reserved(id)
    }

    pub(crate) fn commit_reserved(
        &mut self,
        id: LocalPromptId,
    ) -> Result<AdmittedPrompt, InputMemoryError> {
        self.queue.commit_reserved(id)
    }

    pub(crate) fn history_previous(&mut self) -> Result<bool, InputMemoryError> {
        let target_offset = self
            .history_navigation
            .as_ref()
            .map_or(0, |navigation| navigation.offset_from_newest + 1);
        let Some(target) = self.history.newest_at(target_offset) else {
            return Ok(false);
        };
        let target = copy_prompt(target)?;
        let initial_navigation = if self.history_navigation.is_none() {
            Some(HistoryNavigation {
                offset_from_newest: target_offset,
                saved_draft: copy_prompt(self.composer.text())?,
                saved_cursor: self.composer.cursor(),
            })
        } else {
            None
        };
        self.composer
            .replace_all(&target, target.len())
            .map_err(InputMemoryError::from)?;
        if let Some(navigation) = initial_navigation {
            self.history_navigation = Some(navigation);
        }
        if let Some(navigation) = &mut self.history_navigation {
            navigation.offset_from_newest = target_offset;
        }
        Ok(true)
    }

    pub(crate) fn history_next(&mut self) -> Result<bool, InputMemoryError> {
        let Some(navigation) = self.history_navigation.as_ref() else {
            return Ok(false);
        };
        if navigation.offset_from_newest != 0 {
            let target_offset = navigation.offset_from_newest - 1;
            let target = self
                .history
                .newest_at(target_offset)
                .ok_or(InputMemoryError::InvalidState)?;
            let target = copy_prompt(target)?;
            self.composer
                .replace_all(&target, target.len())
                .map_err(InputMemoryError::from)?;
            if let Some(navigation) = &mut self.history_navigation {
                navigation.offset_from_newest = target_offset;
            }
            return Ok(true);
        }

        let navigation = self
            .history_navigation
            .take()
            .ok_or(InputMemoryError::InvalidState)?;
        let cursor = navigation.saved_cursor;
        match self.composer.swap_draft(navigation.saved_draft, cursor) {
            Ok(_) => Ok(true),
            Err(saved_draft) => {
                self.history_navigation = Some(HistoryNavigation {
                    offset_from_newest: 0,
                    saved_draft,
                    saved_cursor: cursor,
                });
                Err(InputMemoryError::Composer)
            }
        }
    }

    /// Finds the next older committed prompt containing the draft that was
    /// visible when history navigation began. Repeated calls keep that query
    /// stable, so showing a match never accidentally changes what is searched.
    pub(crate) fn reverse_search_previous(&mut self) -> Result<bool, InputMemoryError> {
        let start_offset = self
            .history_navigation
            .as_ref()
            .map_or(0, |navigation| navigation.offset_from_newest + 1);
        let query = self
            .history_navigation
            .as_ref()
            .map_or(self.composer.text(), |navigation| {
                navigation.saved_draft.as_str()
            });
        let Some((target_offset, target)) =
            (start_offset..self.history.prompts.len()).find_map(|offset| {
                let prompt = self.history.newest_at(offset)?;
                prompt.contains(query).then_some((offset, prompt))
            })
        else {
            return Ok(false);
        };
        let target = copy_prompt(target)?;
        let initial_navigation = if self.history_navigation.is_none() {
            Some(HistoryNavigation {
                offset_from_newest: target_offset,
                saved_draft: copy_prompt(self.composer.text())?,
                saved_cursor: self.composer.cursor(),
            })
        } else {
            None
        };
        self.composer
            .replace_all(&target, target.len())
            .map_err(InputMemoryError::from)?;
        if let Some(navigation) = initial_navigation {
            self.history_navigation = Some(navigation);
        }
        if let Some(navigation) = &mut self.history_navigation {
            navigation.offset_from_newest = target_offset;
        }
        Ok(true)
    }

    pub(crate) fn enqueue_draft(&mut self) -> Result<LocalPromptId, InputMemoryError> {
        let id = self.queue.enqueue_from(&mut self.composer)?;
        self.history_navigation = None;
        Ok(id)
    }

    pub(crate) fn recall_latest_queue(&mut self) -> Result<bool, InputMemoryError> {
        let recalled = self.queue.recall_latest(&mut self.composer)?;
        if recalled {
            self.history_navigation = None;
        }
        Ok(recalled)
    }
}

fn copy_prompt(prompt: &str) -> Result<String, InputMemoryError> {
    let mut copy = String::new();
    copy.try_reserve_exact(prompt.len())
        .map_err(|_| InputMemoryError::Capacity)?;
    copy.push_str(prompt);
    Ok(copy)
}

#[cfg(test)]
mod tests {
    use super::{
        InputMemory, InputMemoryError, MAX_HISTORY_BYTES, MAX_HISTORY_ITEMS, MAX_QUEUE_BYTES,
        MAX_QUEUE_ITEMS, PromptHistory,
    };
    use crate::tui::composer::MAX_PROMPT_BYTES;

    #[test]
    fn queue_limits_are_atomic_and_admission_is_fifo() {
        let mut input = InputMemory::default();
        for index in 0..MAX_QUEUE_ITEMS {
            input
                .composer_mut()
                .insert_text(&format!("prompt-{index}"))
                .unwrap();
            input.enqueue_draft().unwrap();
        }
        input.composer_mut().insert_text("ninth").unwrap();
        assert_eq!(input.enqueue_draft(), Err(InputMemoryError::QueueFull));
        assert_eq!(input.composer().text(), "ninth");

        let before = input.queue().len();
        {
            let admission = input.queue_mut().front_admission().unwrap();
            assert_eq!(admission.id(), super::LocalPromptId(1));
            assert_eq!(admission.text(), Some("prompt-0"));
            assert!(format!("{admission:?}").contains("bytes"));
        }
        assert_eq!(input.queue().len(), before);
        let admitted = input
            .queue_mut()
            .front_admission()
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(admitted.id(), super::LocalPromptId(1));
        assert_eq!(admitted.into_text(), "prompt-0");
        assert_eq!(input.queue().len(), MAX_QUEUE_ITEMS - 1);
    }

    #[test]
    fn queue_aggregate_exact_and_one_over_preserve_the_draft() {
        let mut input = InputMemory::default();
        for _ in 0..4 {
            input
                .composer_mut()
                .insert_text(&"x".repeat(MAX_PROMPT_BYTES))
                .unwrap();
            input.enqueue_draft().unwrap();
        }
        assert_eq!(input.queue().total_bytes(), MAX_QUEUE_BYTES);
        input.composer_mut().insert_char('y').unwrap();
        assert_eq!(input.enqueue_draft(), Err(InputMemoryError::QueueBytes));
        assert_eq!(input.composer().text(), "y");
    }

    #[test]
    fn recalling_latest_swaps_without_reordering_older_prompts() {
        let mut input = InputMemory::default();
        input.composer_mut().insert_text("first").unwrap();
        input.enqueue_draft().unwrap();
        input.composer_mut().insert_text("latest").unwrap();
        input.enqueue_draft().unwrap();
        input.composer_mut().insert_text("current").unwrap();
        assert!(input.recall_latest_queue().unwrap());
        assert_eq!(input.composer().text(), "latest");
        assert_eq!(input.queue().len(), 2);
        let first = input
            .queue_mut()
            .front_admission()
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(first.into_text(), "first");
        let second = input
            .queue_mut()
            .front_admission()
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(second.into_text(), "current");
    }

    #[test]
    fn an_in_flight_front_cannot_be_recalled_or_replaced() {
        let mut input = InputMemory::default();
        input.composer_mut().insert_text("in flight").unwrap();
        let id = input.enqueue_draft().unwrap();
        let reserved = input.reserve_front().unwrap();
        assert_eq!(reserved.id(), id);
        assert_eq!(reserved.text(), "in flight");
        assert_eq!(input.queue().len(), 0);
        assert_eq!(input.queue().total_bytes(), 0);

        input.insert_text("new draft").unwrap();
        assert!(!input.recall_latest_queue().unwrap());
        assert_eq!(input.composer().text(), "new draft");

        input.release_reserved(id).unwrap();
        assert_eq!(input.queue().len(), 1);
        assert!(input.recall_latest_queue().unwrap());
        assert_eq!(input.composer().text(), "in flight");

        input.enqueue_draft().unwrap();
        let reserved = input.reserve_front().unwrap();
        let admitted = input.commit_reserved(reserved.id()).unwrap();
        assert_eq!(admitted.into_text(), "new draft");
        assert_eq!(input.queue().len(), 1);
    }

    #[test]
    fn history_bounds_evict_whole_oldest_entries() {
        let mut history = PromptHistory::default();
        for index in 0..=MAX_HISTORY_ITEMS {
            history.record_committed(&format!("entry-{index}")).unwrap();
        }
        assert_eq!(history.len(), MAX_HISTORY_ITEMS);
        assert!(history.is_truncated());
        assert_eq!(history.newest_at(0), Some("entry-128"));
        assert_eq!(history.newest_at(MAX_HISTORY_ITEMS - 1), Some("entry-1"));

        let mut history = PromptHistory::default();
        for _ in 0..16 {
            history
                .record_committed(&"x".repeat(MAX_PROMPT_BYTES))
                .unwrap();
        }
        assert_eq!(history.total_bytes(), MAX_HISTORY_BYTES);
        history.record_committed("next").unwrap();
        assert!(history.total_bytes() <= MAX_HISTORY_BYTES);
        assert_eq!(history.len(), 16);
    }

    #[test]
    fn history_navigation_restores_the_original_draft_and_cursor() {
        let mut input = InputMemory::default();
        input.history_mut().record_committed("older").unwrap();
        input.history_mut().record_committed("newer").unwrap();
        input.composer_mut().insert_text("draft").unwrap();
        input.composer_mut().move_left();
        let original_cursor = input.composer().cursor();

        assert!(input.history_previous().unwrap());
        assert_eq!(input.composer().text(), "newer");
        assert!(input.history_previous().unwrap());
        assert_eq!(input.composer().text(), "older");
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "newer");
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "draft");
        assert_eq!(input.composer().cursor(), original_cursor);
    }

    #[test]
    fn empty_or_failed_edits_do_not_detach_history_navigation() {
        let mut input = InputMemory::default();
        input.history_mut().record_committed("history").unwrap();
        input.composer_mut().insert_text("draft").unwrap();
        assert!(input.history_previous().unwrap());
        assert_eq!(input.composer().text(), "history");

        input.insert_text("").unwrap();
        assert_eq!(input.composer().text(), "history");
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "draft");

        assert!(input.history_previous().unwrap());
        assert!(input.insert_text(&"x".repeat(MAX_PROMPT_BYTES)).is_err());
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "draft");
    }

    #[test]
    fn a_new_committed_prompt_rebases_navigation_without_losing_the_saved_draft() {
        let mut input = InputMemory::default();
        input.history_mut().record_committed("older").unwrap();
        input.history_mut().record_committed("newer").unwrap();
        input.composer_mut().insert_text("saved draft").unwrap();
        assert!(input.history_previous().unwrap());
        assert_eq!(input.composer().text(), "newer");

        assert!(input.record_committed_human("latest").unwrap());
        assert_eq!(input.composer().text(), "newer");
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "latest");
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "saved draft");
    }

    #[test]
    fn reverse_search_moves_newest_to_older_without_changing_its_query() {
        let mut input = InputMemory::default();
        input
            .history_mut()
            .record_committed("older matching prompt")
            .unwrap();
        input
            .history_mut()
            .record_committed("unrelated prompt")
            .unwrap();
        input
            .history_mut()
            .record_committed("newest matching prompt")
            .unwrap();
        input.composer_mut().insert_text("matching").unwrap();
        input.composer_mut().move_left();
        let saved_cursor = input.composer().cursor();

        assert!(input.reverse_search_previous().unwrap());
        assert_eq!(input.composer().text(), "newest matching prompt");
        assert!(input.reverse_search_previous().unwrap());
        assert_eq!(input.composer().text(), "older matching prompt");
        assert!(!input.reverse_search_previous().unwrap());
        assert_eq!(input.composer().text(), "older matching prompt");

        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "unrelated prompt");
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "newest matching prompt");
        assert!(input.history_next().unwrap());
        assert_eq!(input.composer().text(), "matching");
        assert_eq!(input.composer().cursor(), saved_cursor);
    }

    #[test]
    fn oversized_history_is_skipped_and_debug_never_contains_prompts() {
        let mut input = InputMemory::default();
        assert!(
            !input
                .history_mut()
                .record_committed(&"x".repeat(MAX_PROMPT_BYTES + 1))
                .unwrap()
        );
        input.composer_mut().insert_text("SECRET_DRAFT").unwrap();
        input
            .history_mut()
            .record_committed("SECRET_HISTORY")
            .unwrap();
        let debug = format!("{input:?}");
        assert!(!debug.contains("SECRET_DRAFT"));
        assert!(!debug.contains("SECRET_HISTORY"));
        assert!(input.history().is_truncated());
    }
}
