use std::{
    fmt,
    fmt::Write as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

use super::{
    dock::{DockError, DockFrame},
    presentation::{PresentedChunk, PresentedItem, TextStyle},
};

const MAX_SCREEN_WRITE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SCREEN_GRAPHEME_BYTES: usize = 1024;
/// Clears uncertain visible coordinates while keeping application input safe
/// for an in-process reattach. ED2 deliberately sacrifices the current
/// viewport so a partial draft or approval cannot enter native history.
pub(crate) const POISON_REATTACH_BYTES: &[u8] =
    b"\x1b[r\x1b[?6l\x1b[2J\x1b[H\x1b[?2004h\x1b[?25l\x1b[0m";

/// Clears uncertain coordinates and returns terminal-owned modes to the
/// parent shell before suspend, exit, or an unrecoverable failure.
pub(crate) const POISON_TEARDOWN_BYTES: &[u8] =
    b"\x1b[r\x1b[?6l\x1b[2J\x1b[H\x1b[?2004l\x1b[?25h\x1b[0m";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScreenSize {
    pub(crate) rows: u16,
    pub(crate) columns: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Ledger {
    size: ScreenSize,
    dock_rows: u16,
    transcript_row: u16,
    transcript_column: u16,
    line_full: bool,
    wrap_seal: Option<WrapSeal>,
    tail_cluster: Option<TailCluster>,
    generation: u64,
}

#[derive(Clone, Eq, PartialEq)]
struct WrapSeal {
    text: String,
    style: TextStyle,
    start_column: u16,
}

impl fmt::Debug for WrapSeal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WrapSeal")
            .field("bytes", &self.text.len())
            .field("style", &self.style)
            .field("start_column", &self.start_column)
            .finish()
    }
}

#[derive(Clone, Eq, PartialEq)]
struct TailCluster {
    text: String,
    cells: usize,
}

impl fmt::Debug for TailCluster {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TailCluster")
            .field("bytes", &self.text.len())
            .field("cells", &self.cells)
            .finish()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ScreenState {
    Detached,
    Ready(Ledger),
    Poisoned,
}

pub(crate) struct InlineScreen {
    state: ScreenState,
    poisoned: Arc<AtomicBool>,
}

impl fmt::Debug for InlineScreen {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("InlineScreen")
            .field("state", &self.state)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum InlineScreenError {
    #[error("CLI_TERMINAL_TOO_SMALL")]
    TooSmall,
    #[error("CLI_OUTPUT_CAPACITY")]
    Capacity,
    #[error("CLI_OUTPUT_LIMIT")]
    Limit,
    #[error("CLI_OUTPUT_STATE")]
    InvalidState,
    #[error("CLI_OUTPUT_POISONED")]
    Poisoned,
}

impl From<DockError> for InlineScreenError {
    fn from(value: DockError) -> Self {
        match value {
            DockError::TooSmall => Self::TooSmall,
            DockError::Capacity => Self::Capacity,
            DockError::Limit => Self::Limit,
            DockError::InvalidState => Self::InvalidState,
        }
    }
}

pub(crate) struct PendingScreenWrite {
    bytes: String,
    written: usize,
    base_generation: Option<u64>,
    next: ScreenState,
    poisoned: Arc<AtomicBool>,
    committed: bool,
}

impl fmt::Debug for PendingScreenWrite {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PendingScreenWrite")
            .field("bytes", &self.bytes.len())
            .field("written", &self.written)
            .field("base_generation", &self.base_generation)
            .finish()
    }
}

impl Default for InlineScreen {
    fn default() -> Self {
        Self {
            state: ScreenState::Detached,
            poisoned: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl InlineScreen {
    pub(crate) fn stage_attach(
        &self,
        size: ScreenSize,
        dock: &DockFrame,
        styled: bool,
    ) -> Result<PendingScreenWrite, InlineScreenError> {
        if self.state != ScreenState::Detached || self.poisoned.load(Ordering::Acquire) {
            return Err(InlineScreenError::InvalidState);
        }
        validate_frame(size, dock)?;
        let dock_rows = dock.rows()?;
        let transcript_row = size
            .rows
            .checked_sub(dock_rows)
            .filter(|row| *row != 0)
            .ok_or(InlineScreenError::TooSmall)?;
        let mut bytes = screen_buffer()?;
        bytes.push_str("\x1b[r\x1b[?6l\x1b[?2004h\x1b[?25l");
        push_cup(&mut bytes, size.rows, 1);
        for _ in 0..=dock_rows {
            bytes.push_str("\r\n");
        }
        dock.render_bottom(&mut bytes, styled)?;
        finish_screen_write(
            bytes,
            None,
            ScreenState::Ready(Ledger {
                size,
                dock_rows,
                transcript_row,
                transcript_column: 1,
                line_full: false,
                wrap_seal: None,
                tail_cluster: None,
                generation: 1,
            }),
            Arc::clone(&self.poisoned),
        )
    }

    pub(crate) fn stage_dock(
        &self,
        dock: &DockFrame,
        styled: bool,
    ) -> Result<PendingScreenWrite, InlineScreenError> {
        let ledger = self.ready()?;
        validate_ready_frame(&ledger, dock)?;
        let mut bytes = screen_buffer()?;
        dock.clear_bottom(&mut bytes)?;
        dock.render_bottom(&mut bytes, styled)?;
        let base_generation = ledger.generation;
        let next = next_generation(ledger)?;
        finish_screen_write(
            bytes,
            Some(base_generation),
            ScreenState::Ready(next),
            Arc::clone(&self.poisoned),
        )
    }

    pub(crate) fn stage_transcript(
        &self,
        chunk: &PresentedChunk,
        dock: &DockFrame,
        styled: bool,
    ) -> Result<PendingScreenWrite, InlineScreenError> {
        let mut ledger = self.ready()?;
        validate_ready_frame(&ledger, dock)?;
        let columns = ledger.size.columns;
        if columns == 0 {
            return Err(InlineScreenError::TooSmall);
        }
        let mut bytes = screen_buffer()?;
        bytes
            .try_reserve(chunk.text_bytes())
            .map_err(|_| InlineScreenError::Capacity)?;
        dock.clear_bottom(&mut bytes)?;
        let mut active_style = TextStyle::Plain;
        if ledger.line_full {
            let seal = ledger
                .wrap_seal
                .as_ref()
                .ok_or(InlineScreenError::InvalidState)?;
            push_cup(&mut bytes, ledger.transcript_row, seal.start_column);
            push_style(&mut bytes, seal.style, styled);
            bytes
                .try_reserve(seal.text.len())
                .map_err(|_| InlineScreenError::Capacity)?;
            bytes.push_str(&seal.text);
            active_style = seal.style;
        } else {
            push_cup(&mut bytes, ledger.transcript_row, ledger.transcript_column);
        }
        let mut may_extend_tail = true;
        for item in chunk.items() {
            match item {
                PresentedItem::LineFeed => {
                    push_style(&mut bytes, TextStyle::Plain, styled);
                    active_style = TextStyle::Plain;
                    bytes.push('\n');
                    advance_line_feed(&mut ledger);
                    may_extend_tail = false;
                }
                PresentedItem::Text { style, text } => {
                    for grapheme in text.graphemes(true) {
                        let cells = UnicodeWidthStr::width(grapheme);
                        let display = if cells > usize::from(columns)
                            || grapheme.len() > MAX_SCREEN_GRAPHEME_BYTES
                        {
                            "[wide grapheme]"
                        } else {
                            grapheme
                        };
                        let extension = if may_extend_tail {
                            may_extend_tail = false;
                            extend_tail_cluster(ledger.tail_cluster.as_ref(), display)?
                        } else {
                            None
                        };
                        let display_cells = UnicodeWidthStr::width(display);
                        let cells = extension.as_ref().map_or(Ok(display_cells), |extended| {
                            let previous = ledger
                                .tail_cluster
                                .as_ref()
                                .ok_or(InlineScreenError::InvalidState)?;
                            extended
                                .cells
                                .checked_sub(previous.cells)
                                .ok_or(InlineScreenError::InvalidState)
                        })?;
                        if cells != 0
                            && (ledger.line_full
                                || usize::from(ledger.transcript_column.saturating_sub(1))
                                    .checked_add(cells)
                                    .is_none_or(|next| next > usize::from(columns)))
                        {
                            advance_soft_wrap(&mut ledger);
                        }
                        if active_style != *style {
                            push_style(&mut bytes, *style, styled);
                            active_style = *style;
                        }
                        bytes
                            .try_reserve(display.len())
                            .map_err(|_| InlineScreenError::Capacity)?;
                        bytes.push_str(display);
                        if cells == 0 {
                            if ledger.line_full {
                                let seal = ledger
                                    .wrap_seal
                                    .as_mut()
                                    .ok_or(InlineScreenError::InvalidState)?;
                                let next = seal
                                    .text
                                    .len()
                                    .checked_add(display.len())
                                    .filter(|next| *next <= MAX_SCREEN_GRAPHEME_BYTES)
                                    .ok_or(InlineScreenError::Limit)?;
                                seal.text
                                    .try_reserve(next - seal.text.len())
                                    .map_err(|_| InlineScreenError::Capacity)?;
                                seal.text.push_str(display);
                            }
                            ledger.tail_cluster = Some(extension.unwrap_or(TailCluster {
                                text: display.to_owned(),
                                cells: display_cells,
                            }));
                            continue;
                        }
                        let start_column = ledger.transcript_column;
                        let next = usize::from(ledger.transcript_column.saturating_sub(1))
                            .checked_add(cells)
                            .ok_or(InlineScreenError::Limit)?;
                        if next == usize::from(columns) {
                            ledger.transcript_column = columns;
                            ledger.line_full = true;
                            ledger.wrap_seal = Some(WrapSeal {
                                text: display.to_owned(),
                                style: *style,
                                start_column,
                            });
                        } else {
                            ledger.transcript_column =
                                u16::try_from(next + 1).map_err(|_| InlineScreenError::Limit)?;
                            ledger.line_full = false;
                            ledger.wrap_seal = None;
                        }
                        ledger.tail_cluster = Some(extension.unwrap_or(TailCluster {
                            text: display.to_owned(),
                            cells: display_cells,
                        }));
                    }
                }
            }
        }
        if active_style != TextStyle::Plain {
            push_style(&mut bytes, TextStyle::Plain, styled);
        }
        reserve_dock_space(&mut bytes, &mut ledger);
        dock.render_bottom(&mut bytes, styled)?;
        ledger = next_generation(ledger)?;
        finish_screen_write(
            bytes,
            Some(ledger.generation - 1),
            ScreenState::Ready(ledger),
            Arc::clone(&self.poisoned),
        )
    }

    pub(crate) fn stage_resize(
        &self,
        size: ScreenSize,
        dock: &DockFrame,
        styled: bool,
    ) -> Result<PendingScreenWrite, InlineScreenError> {
        let mut ledger = self.ready()?;
        validate_frame(size, dock)?;
        let dock_rows = dock.rows()?;
        if size.columns == 0 {
            return Err(InlineScreenError::InvalidState);
        }
        let transcript_row = size
            .rows
            .checked_sub(dock_rows)
            .filter(|row| *row != 0)
            .ok_or(InlineScreenError::TooSmall)?;
        let mut bytes = screen_buffer()?;
        bytes.push_str("\x1b[r\x1b[?6l\x1b[?25l");
        let old_dock_start = ledger
            .size
            .rows
            .checked_sub(ledger.dock_rows)
            .and_then(|row| row.checked_add(1))
            .ok_or(InlineScreenError::InvalidState)?;
        let old_dock_end = ledger.size.rows.min(size.rows);
        if old_dock_start <= old_dock_end {
            for row in old_dock_start..=old_dock_end {
                push_cup(&mut bytes, row, 1);
                bytes.push_str("\x1b[2K");
            }
        }
        push_cup(&mut bytes, size.rows, 1);
        for _ in 0..=dock_rows {
            bytes.push_str("\r\n");
        }
        dock.render_bottom(&mut bytes, styled)?;
        ledger.size = size;
        ledger.dock_rows = dock_rows;
        ledger.transcript_row = transcript_row;
        // A resize happens in the terminal emulator before SIGWINCH reaches
        // us, so the old partial-line coordinates are no longer portable.
        // Establish a fresh transcript boundary instead of guessing and
        // overwriting either the old text or the dock. The presenter restarts
        // its role prefix after this transaction commits.
        ledger.transcript_column = 1;
        ledger.line_full = false;
        ledger.wrap_seal = None;
        ledger.tail_cluster = None;
        ledger = next_generation(ledger)?;
        finish_screen_write(
            bytes,
            Some(ledger.generation - 1),
            ScreenState::Ready(ledger),
            Arc::clone(&self.poisoned),
        )
    }

    pub(crate) fn stage_detach(&self) -> Result<PendingScreenWrite, InlineScreenError> {
        let ledger = self.ready()?;
        let mut bytes = screen_buffer()?;
        let start = ledger
            .transcript_row
            .checked_add(1)
            .ok_or(InlineScreenError::Limit)?;
        for row in start..=ledger.size.rows {
            push_cup(&mut bytes, row, 1);
            bytes.push_str("\x1b[2K");
        }
        push_cup(&mut bytes, ledger.transcript_row, ledger.transcript_column);
        bytes.push_str("\r\n\x1b[r\x1b[?6l\x1b[?2004l\x1b[?25h\x1b[0m");
        finish_screen_write(
            bytes,
            Some(ledger.generation),
            ScreenState::Detached,
            Arc::clone(&self.poisoned),
        )
    }

    pub(crate) fn commit(
        &mut self,
        mut write: PendingScreenWrite,
    ) -> Result<(), InlineScreenError> {
        if self.poisoned.load(Ordering::Acquire)
            || write.written != write.bytes.len()
            || generation(&self.state) != write.base_generation
        {
            self.state = ScreenState::Poisoned;
            self.poisoned.store(true, Ordering::Release);
            return Err(InlineScreenError::InvalidState);
        }
        self.state = write.next.clone();
        write.committed = true;
        Ok(())
    }

    pub(crate) fn abort(&mut self, write: PendingScreenWrite) {
        if write.written != 0 {
            self.state = ScreenState::Poisoned;
            self.poisoned.store(true, Ordering::Release);
        }
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        self.state == ScreenState::Poisoned || self.poisoned.load(Ordering::Acquire)
    }

    pub(crate) fn is_detached(&self) -> bool {
        self.state == ScreenState::Detached && !self.poisoned.load(Ordering::Acquire)
    }

    /// Discards coordinate knowledge only after the caller has successfully
    /// sent the fixed, coordinate-free visual reset. The next operation must
    /// be a fresh attach; no transcript state is replayed.
    pub(crate) fn recover_after_visual_reset(&mut self) {
        self.state = ScreenState::Detached;
        self.poisoned.store(false, Ordering::Release);
    }

    fn ready(&self) -> Result<Ledger, InlineScreenError> {
        if self.poisoned.load(Ordering::Acquire) {
            return Err(InlineScreenError::Poisoned);
        }
        match &self.state {
            ScreenState::Ready(ledger) => Ok(ledger.clone()),
            ScreenState::Poisoned => Err(InlineScreenError::Poisoned),
            ScreenState::Detached => Err(InlineScreenError::InvalidState),
        }
    }
}

impl PendingScreenWrite {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes.as_bytes()[self.written..]
    }

    pub(crate) fn advance(&mut self, count: usize) -> Result<(), InlineScreenError> {
        self.written = self
            .written
            .checked_add(count)
            .filter(|written| *written <= self.bytes.len())
            .ok_or(InlineScreenError::InvalidState)?;
        Ok(())
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.written == self.bytes.len()
    }

    pub(crate) fn has_started(&self) -> bool {
        self.written != 0
    }
}

impl Drop for PendingScreenWrite {
    fn drop(&mut self) {
        if !self.committed && self.written != 0 {
            self.poisoned.store(true, Ordering::Release);
        }
    }
}

fn generation(state: &ScreenState) -> Option<u64> {
    match state {
        ScreenState::Ready(ledger) => Some(ledger.generation),
        ScreenState::Detached | ScreenState::Poisoned => None,
    }
}

fn next_generation(mut ledger: Ledger) -> Result<Ledger, InlineScreenError> {
    ledger.generation = ledger
        .generation
        .checked_add(1)
        .ok_or(InlineScreenError::Limit)?;
    Ok(ledger)
}

fn validate_frame(size: ScreenSize, dock: &DockFrame) -> Result<(), InlineScreenError> {
    if size.rows != dock.terminal_rows() || size.columns != dock.terminal_columns() {
        return Err(InlineScreenError::InvalidState);
    }
    Ok(())
}

fn validate_ready_frame(ledger: &Ledger, dock: &DockFrame) -> Result<(), InlineScreenError> {
    validate_frame(ledger.size, dock)?;
    if dock.rows()? != ledger.dock_rows || dock.output_bottom() != ledger.transcript_row {
        return Err(InlineScreenError::InvalidState);
    }
    Ok(())
}

fn extend_tail_cluster(
    previous: Option<&TailCluster>,
    next: &str,
) -> Result<Option<TailCluster>, InlineScreenError> {
    let Some(previous) = previous else {
        return Ok(None);
    };
    let bytes = previous
        .text
        .len()
        .checked_add(next.len())
        .ok_or(InlineScreenError::Limit)?;
    if bytes > MAX_SCREEN_GRAPHEME_BYTES {
        return Ok(None);
    }
    let mut combined = String::new();
    combined
        .try_reserve_exact(bytes)
        .map_err(|_| InlineScreenError::Capacity)?;
    combined.push_str(&previous.text);
    combined.push_str(next);
    let mut graphemes = combined.graphemes(true);
    let Some(cluster) = graphemes.next() else {
        return Ok(None);
    };
    if cluster.len() != combined.len() || graphemes.next().is_some() {
        return Ok(None);
    }
    Ok(Some(TailCluster {
        cells: UnicodeWidthStr::width(combined.as_str()),
        text: combined,
    }))
}

fn advance_line_feed(ledger: &mut Ledger) {
    if ledger.transcript_row < ledger.size.rows {
        ledger.transcript_row += 1;
    }
    ledger.transcript_column = 1;
    ledger.line_full = false;
    ledger.wrap_seal = None;
    ledger.tail_cluster = None;
}

fn advance_soft_wrap(ledger: &mut Ledger) {
    if ledger.transcript_row < ledger.size.rows {
        ledger.transcript_row += 1;
    }
    ledger.transcript_column = 1;
    ledger.line_full = false;
    ledger.wrap_seal = None;
}

fn reserve_dock_space(output: &mut String, ledger: &mut Ledger) {
    let output_bottom = ledger.size.rows - ledger.dock_rows;
    let scrolls = ledger.transcript_row.saturating_sub(output_bottom);
    if scrolls == 0 {
        return;
    }
    push_cup(output, ledger.size.rows, 1);
    for _ in 0..scrolls {
        output.push('\n');
    }
    ledger.transcript_row -= scrolls;
}

fn push_style(output: &mut String, style: TextStyle, styled: bool) {
    if !styled {
        return;
    }
    output.push_str(match style {
        TextStyle::Plain => "\x1b[0m",
        TextStyle::Muted => "\x1b[2m",
        TextStyle::Accent => "\x1b[1;36m",
        TextStyle::User => "\x1b[1;35m",
        TextStyle::Assistant => "\x1b[36m",
        TextStyle::Warning => "\x1b[1;33m",
        TextStyle::Error => "\x1b[1;31m",
        TextStyle::Success => "\x1b[32m",
    });
}

fn screen_buffer() -> Result<String, InlineScreenError> {
    let mut output = String::new();
    output
        .try_reserve_exact(32 * 1024)
        .map_err(|_| InlineScreenError::Capacity)?;
    Ok(output)
}

fn finish_screen_write(
    bytes: String,
    base_generation: Option<u64>,
    next: ScreenState,
    poisoned: Arc<AtomicBool>,
) -> Result<PendingScreenWrite, InlineScreenError> {
    if bytes.len() > MAX_SCREEN_WRITE_BYTES {
        return Err(InlineScreenError::Limit);
    }
    Ok(PendingScreenWrite {
        bytes,
        written: 0,
        base_generation,
        next,
        poisoned,
        committed: false,
    })
}

fn push_cup(output: &mut String, row: u16, column: u16) {
    write!(output, "\x1b[{row};{column}H")
        .expect("writing a bounded cursor-position command cannot fail");
}

#[cfg(test)]
mod tests {
    use super::{InlineScreen, POISON_REATTACH_BYTES, POISON_TEARDOWN_BYTES, ScreenSize};
    use crate::tui::{
        composer::Composer,
        dock::{DockFrame, DockInteraction, DockModel},
        input_memory::PromptQueue,
        presentation::{PresentedChunk, TextStyle},
        terminal_model::{HistoryPolicy, MiniTerminal},
    };

    fn dock<'a>(composer: &'a Composer, queue: &'a PromptQueue) -> DockFrame {
        dock_at(composer, queue, 24, 80)
    }

    fn dock_at(composer: &Composer, queue: &PromptQueue, rows: u16, columns: u16) -> DockFrame {
        DockFrame::layout(
            DockModel {
                interaction: DockInteraction::Running,
                composer,
                queue,
                notice: None,
            },
            rows,
            columns,
        )
        .unwrap()
    }

    fn commit(screen: &mut InlineScreen, mut write: super::PendingScreenWrite) -> String {
        let bytes = String::from_utf8(write.bytes().to_vec()).unwrap();
        let length = write.bytes().len();
        write.advance(length).unwrap();
        screen.commit(write).unwrap();
        bytes
    }

    fn apply(
        screen: &mut InlineScreen,
        terminal: &mut MiniTerminal,
        write: super::PendingScreenWrite,
    ) -> String {
        let bytes = commit(screen, write);
        terminal.feed(bytes.as_bytes());
        bytes
    }

    fn chunk(text: &str, line_feed: bool) -> PresentedChunk {
        let mut builder = PresentedChunk::builder();
        builder.push_text(TextStyle::Assistant, text).unwrap();
        if line_feed {
            builder.push_line_feed().unwrap();
        }
        builder.finish()
    }

    fn cursor_state(screen: &InlineScreen) -> (u16, u16, bool, Option<usize>) {
        match &screen.state {
            super::ScreenState::Ready(ledger) => (
                ledger.transcript_row,
                ledger.transcript_column,
                ledger.line_full,
                ledger.tail_cluster.as_ref().map(|tail| tail.cells),
            ),
            state => panic!("expected ready screen, got {state:?}"),
        }
    }

    #[test]
    fn attach_uses_only_full_screen_scrolling_and_never_sets_margins() {
        let composer = Composer::default();
        let queue = PromptQueue::default();
        let dock = dock(&composer, &queue);
        let mut screen = InlineScreen::default();
        let attach = screen
            .stage_attach(
                ScreenSize {
                    rows: 24,
                    columns: 80,
                },
                &dock,
                true,
            )
            .unwrap();
        let bytes = commit(&mut screen, attach);
        assert!(bytes.starts_with("\x1b[r\x1b[?6l\x1b[?2004h\x1b[?25l"));
        assert_eq!(
            bytes.matches("\r\n").count(),
            usize::from(dock.rows().unwrap()) + 1
        );
        assert!(!bytes.contains(";15r"));
    }

    #[test]
    fn poisoned_recovery_keeps_paste_framed_until_the_shell_owns_input() {
        let reattach = std::str::from_utf8(POISON_REATTACH_BYTES).unwrap();
        let teardown = std::str::from_utf8(POISON_TEARDOWN_BYTES).unwrap();
        assert!(reattach.contains("\x1b[2J"));
        assert!(reattach.contains("\x1b[?2004h"));
        assert!(!reattach.contains("\x1b[?2004l"));
        assert!(teardown.contains("\x1b[2J"));
        assert!(teardown.contains("\x1b[?2004l"));
        assert!(!teardown.contains("\x1b[?2004h"));
    }

    #[test]
    fn input_redraw_never_moves_or_scrolls_the_transcript() {
        let mut composer = Composer::default();
        let queue = PromptQueue::default();
        let first = dock(&composer, &queue);
        let mut screen = InlineScreen::default();
        let attach = screen
            .stage_attach(
                ScreenSize {
                    rows: 24,
                    columns: 80,
                },
                &first,
                true,
            )
            .unwrap();
        let _ = commit(&mut screen, attach);

        composer.insert_text("draft").unwrap();
        let next = dock(&composer, &queue);
        let redraw = screen.stage_dock(&next, true).unwrap();
        let bytes = commit(&mut screen, redraw);
        assert!(!bytes.contains('\n'));
        assert!(!bytes.contains("\x1b[r"));
    }

    #[test]
    fn partial_transcript_continues_after_many_dock_redraws() {
        let mut composer = Composer::default();
        let queue = PromptQueue::default();
        let mut frame = dock(&composer, &queue);
        let mut screen = InlineScreen::default();
        let attach = screen
            .stage_attach(
                ScreenSize {
                    rows: 24,
                    columns: 80,
                },
                &frame,
                true,
            )
            .unwrap();
        let _ = commit(&mut screen, attach);

        let mut first = PresentedChunk::builder();
        first
            .push_text(TextStyle::Assistant, "busy-partial")
            .unwrap();
        let write = screen
            .stage_transcript(&first.finish(), &frame, true)
            .unwrap();
        let _ = commit(&mut screen, write);
        for _ in 0..100 {
            composer.insert_char('x').unwrap();
            frame = dock(&composer, &queue);
            let redraw = screen.stage_dock(&frame, true).unwrap();
            let bytes = commit(&mut screen, redraw);
            assert!(!bytes.contains("busy-partial"));
        }

        let mut continuation = PresentedChunk::builder();
        continuation
            .push_text(TextStyle::Assistant, " first-turn-finished")
            .unwrap();
        continuation.push_line_feed().unwrap();
        let write = screen
            .stage_transcript(&continuation.finish(), &frame, true)
            .unwrap();
        let bytes = commit(&mut screen, write);
        assert!(bytes.contains(" first-turn-finished"));
        assert!(!bytes.contains("busy-partial"));
        assert!(!screen.is_poisoned());
    }

    #[test]
    fn terminal_models_preserve_prior_screen_and_never_scroll_the_dock_into_history() {
        for policy in [
            HistoryPolicy::FullScreenOnly,
            HistoryPolicy::TopAnchoredRegion,
        ] {
            let mut terminal = MiniTerminal::prefilled(24, 80, policy);
            let mut screen = InlineScreen::default();
            let composer = Composer::default();
            let queue = PromptQueue::default();
            let frame = dock(&composer, &queue);
            let attach = screen
                .stage_attach(
                    ScreenSize {
                        rows: 24,
                        columns: 80,
                    },
                    &frame,
                    true,
                )
                .unwrap();
            apply(&mut screen, &mut terminal, attach);

            let lines = terminal.all_lines();
            for number in 1..=24 {
                let marker = format!("PRE{number:02}");
                assert_eq!(
                    lines.iter().filter(|line| *line == &marker).count(),
                    1,
                    "{policy:?} lost or duplicated {marker}"
                );
            }
            assert!(!terminal.partial_margin_seen());
            assert!(terminal.history().iter().all(|line| {
                !line.contains("Working") && !line.contains("Enter queue") && !line.contains('❯')
            }));
        }
    }

    #[test]
    fn native_soft_wrap_and_partial_continuation_survive_one_hundred_dock_redraws() {
        for policy in [
            HistoryPolicy::FullScreenOnly,
            HistoryPolicy::TopAnchoredRegion,
        ] {
            let mut terminal = MiniTerminal::blank(24, 80, policy);
            let mut screen = InlineScreen::default();
            let mut composer = Composer::default();
            let queue = PromptQueue::default();
            let mut frame = dock(&composer, &queue);
            let attach = screen
                .stage_attach(
                    ScreenSize {
                        rows: 24,
                        columns: 80,
                    },
                    &frame,
                    true,
                )
                .unwrap();
            apply(&mut screen, &mut terminal, attach);

            let long = "x".repeat(100);
            let write = screen
                .stage_transcript(&chunk(&long, true), &frame, true)
                .unwrap();
            let bytes = apply(&mut screen, &mut terminal, write);
            assert!(
                bytes.contains(&long),
                "the renderer must not insert a hard line break into source text"
            );

            let write = screen
                .stage_transcript(&chunk("busy-partial", false), &frame, true)
                .unwrap();
            apply(&mut screen, &mut terminal, write);
            for _ in 0..100 {
                composer.insert_char('x').unwrap();
                frame = dock(&composer, &queue);
                let redraw = screen.stage_dock(&frame, true).unwrap();
                apply(&mut screen, &mut terminal, redraw);
            }
            let write = screen
                .stage_transcript(&chunk(" first-turn-finished", true), &frame, true)
                .unwrap();
            apply(&mut screen, &mut terminal, write);
            let mut drain = PresentedChunk::builder();
            for _ in 0..24 {
                drain.push_line_feed().unwrap();
            }
            let write = screen
                .stage_transcript(&drain.finish(), &frame, true)
                .unwrap();
            apply(&mut screen, &mut terminal, write);

            let lines = terminal.all_lines();
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.as_str() == "busy-partial first-turn-finished")
                    .count(),
                1,
                "{policy:?} split or duplicated the partial transcript"
            );
            assert!(terminal.history().iter().all(|line| {
                !line.contains("Working") && !line.contains("Enter queue") && !line.contains('❯')
            }));
        }
    }

    #[test]
    fn resize_reanchors_after_a_partial_line_without_overwriting_or_replaying_it() {
        let mut terminal = MiniTerminal::blank(24, 80, HistoryPolicy::FullScreenOnly);
        let mut screen = InlineScreen::default();
        let composer = Composer::default();
        let queue = PromptQueue::default();
        let frame = dock_at(&composer, &queue, 24, 80);
        let attach = screen
            .stage_attach(
                ScreenSize {
                    rows: 24,
                    columns: 80,
                },
                &frame,
                true,
            )
            .unwrap();
        apply(&mut screen, &mut terminal, attach);
        let write = screen
            .stage_transcript(&chunk("busy-partial", false), &frame, true)
            .unwrap();
        apply(&mut screen, &mut terminal, write);

        terminal.grow_top_anchored(30);
        let grown = dock_at(&composer, &queue, 30, 80);
        let resize = screen
            .stage_resize(
                ScreenSize {
                    rows: 30,
                    columns: 80,
                },
                &grown,
                true,
            )
            .unwrap();
        apply(&mut screen, &mut terminal, resize);
        let write = screen
            .stage_transcript(&chunk("continuation-after-resize", true), &grown, true)
            .unwrap();
        apply(&mut screen, &mut terminal, write);

        let lines = terminal.all_lines();
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == "busy-partial")
                .count(),
            1
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == "continuation-after-resize")
                .count(),
            1
        );
        assert!(terminal.history().iter().all(|line| {
            !line.contains("Working") && !line.contains("Enter queue") && !line.contains('❯')
        }));
        assert!(!terminal.partial_margin_seen());
    }

    #[test]
    fn poisoned_dock_recovery_never_scrolls_a_partial_private_surface_into_history() {
        let mut terminal = MiniTerminal::blank(24, 80, HistoryPolicy::FullScreenOnly);
        let mut screen = InlineScreen::default();
        let mut composer = Composer::default();
        let queue = PromptQueue::default();
        let initial = dock(&composer, &queue);
        let attach = screen
            .stage_attach(
                ScreenSize {
                    rows: 24,
                    columns: 80,
                },
                &initial,
                true,
            )
            .unwrap();
        apply(&mut screen, &mut terminal, attach);

        let secret = "draft-secret-must-not-enter-history";
        composer.insert_text(secret).unwrap();
        let private = dock(&composer, &queue);
        let mut redraw = screen.stage_dock(&private, true).unwrap();
        let prefix_end = redraw
            .bytes()
            .windows(secret.len())
            .position(|window| window == secret.as_bytes())
            .map(|start| start + secret.len())
            .expect("the private draft should be present in the staged dock");
        let visible_prefix = redraw.bytes()[..prefix_end].to_vec();
        terminal.feed(&visible_prefix);
        redraw.advance(prefix_end).unwrap();
        screen.abort(redraw);
        assert!(screen.is_poisoned());

        terminal.feed(POISON_REATTACH_BYTES);
        screen.recover_after_visual_reset();
        composer.clear_before_cursor().unwrap();
        let clean = dock(&composer, &queue);
        let attach = screen
            .stage_attach(
                ScreenSize {
                    rows: 24,
                    columns: 80,
                },
                &clean,
                true,
            )
            .unwrap();
        apply(&mut screen, &mut terminal, attach);

        assert!(
            terminal
                .all_lines()
                .iter()
                .all(|line| !line.contains(secret))
        );
        assert!(terminal.history().iter().all(|line| !line.contains(secret)));
    }

    #[test]
    fn split_emoji_clusters_use_the_same_cursor_geometry_as_one_chunk() {
        for (first, second, whole) in [("👍", "🏽X", "👍🏽X"), ("🇺", "🇸X", "🇺🇸X")]
        {
            let composer = Composer::default();
            let queue = PromptQueue::default();
            let frame = dock(&composer, &queue);

            let mut split = InlineScreen::default();
            let attach = split
                .stage_attach(
                    ScreenSize {
                        rows: 24,
                        columns: 80,
                    },
                    &frame,
                    true,
                )
                .unwrap();
            let _ = commit(&mut split, attach);
            let write = split
                .stage_transcript(&chunk(first, false), &frame, true)
                .unwrap();
            let _ = commit(&mut split, write);
            let write = split
                .stage_transcript(&chunk(second, false), &frame, true)
                .unwrap();
            let _ = commit(&mut split, write);

            let mut single = InlineScreen::default();
            let attach = single
                .stage_attach(
                    ScreenSize {
                        rows: 24,
                        columns: 80,
                    },
                    &frame,
                    true,
                )
                .unwrap();
            let _ = commit(&mut single, attach);
            let write = single
                .stage_transcript(&chunk(whole, false), &frame, true)
                .unwrap();
            let _ = commit(&mut single, write);

            assert_eq!(cursor_state(&split), cursor_state(&single));
        }
    }

    #[test]
    fn a_partial_write_poison_prevents_future_coordinate_guesses() {
        let composer = Composer::default();
        let queue = PromptQueue::default();
        let dock = dock(&composer, &queue);
        let mut screen = InlineScreen::default();
        let mut attach = screen
            .stage_attach(
                ScreenSize {
                    rows: 24,
                    columns: 80,
                },
                &dock,
                true,
            )
            .unwrap();
        attach.advance(1).unwrap();
        screen.abort(attach);
        assert!(screen.is_poisoned());
        assert!(screen.stage_dock(&dock, true).is_err());
    }
}
