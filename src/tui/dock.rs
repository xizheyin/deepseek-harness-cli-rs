use std::{collections::VecDeque, fmt, fmt::Write as _};

use thiserror::Error;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr;

use super::{
    composer::Composer,
    input_memory::PromptQueue,
    view::{DetailDocument, DetailTone},
    visible::{VisibleTextError, render_visible_owned},
};

pub(crate) const MIN_ENHANCED_COLUMNS: u16 = 44;
pub(crate) const MIN_ENHANCED_ROWS: u16 = 12;
pub(crate) const MIN_DOCK_COLUMNS: u16 = 12;
pub(crate) const MIN_DOCK_ROWS: u16 = 5;
const MAX_DOCK_ROWS: usize = 24;
const MAX_DETAIL_WRAPPED_ROWS: usize = 4 * 1024;
const DETAIL_WRAPPED_OMISSION: &str = "[omitted] wrapped row limit";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DockInteraction {
    Idle,
    Running,
    Approval(DockApprovalSelection),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DockApprovalSelection {
    AllowOnce,
    Reject,
    Cancel,
}

pub(crate) struct DockModel<'a> {
    pub(crate) interaction: DockInteraction,
    pub(crate) composer: &'a Composer,
    pub(crate) queue: &'a PromptQueue,
    pub(crate) notice: Option<&'a str>,
}

impl fmt::Debug for DockModel<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DockModel")
            .field("interaction", &self.interaction)
            .field("composer_bytes", &self.composer.byte_len())
            .field("queued", &self.queue.len())
            .field("notice_bytes", &self.notice.map(str::len))
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DockRole {
    Queue,
    Divider,
    Composer,
    Hint,
    Notice,
    ApprovalChoice,
    ApprovalWarning,
    DetailTitle,
    DetailPlain,
    DetailMuted,
    DetailAccent,
    DetailPositive,
    DetailCaution,
    DetailNegative,
    DetailCode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DetailViewport {
    pub(crate) offset: usize,
    pub(crate) total_rows: usize,
    pub(crate) page_rows: usize,
    pub(crate) truncated: bool,
}

struct DockLine {
    role: DockRole,
    text: String,
}

impl fmt::Debug for DockLine {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DockLine")
            .field("role", &self.role)
            .field("bytes", &self.text.len())
            .finish()
    }
}

pub(crate) struct DockFrame {
    lines: Vec<DockLine>,
    cursor_row: u16,
    cursor_column: u16,
    width: u16,
    terminal_rows: u16,
    output_bottom: u16,
    software_cursor: bool,
}

impl fmt::Debug for DockFrame {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DockFrame")
            .field("rows", &self.lines.len())
            .field("cursor_row", &self.cursor_row)
            .field("cursor_column", &self.cursor_column)
            .field("width", &self.width)
            .field("terminal_rows", &self.terminal_rows)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum DockError {
    #[error("CLI_TERMINAL_TOO_SMALL")]
    TooSmall,
    #[error("CLI_OUTPUT_CAPACITY")]
    Capacity,
    #[error("CLI_OUTPUT_LIMIT")]
    Limit,
    #[error("CLI_OUTPUT_STATE")]
    InvalidState,
}

impl From<VisibleTextError> for DockError {
    fn from(value: VisibleTextError) -> Self {
        match value {
            VisibleTextError::Capacity => Self::Capacity,
            VisibleTextError::Limit => Self::Limit,
        }
    }
}

impl DockFrame {
    pub(crate) fn layout(model: DockModel<'_>, rows: u16, columns: u16) -> Result<Self, DockError> {
        if rows < MIN_DOCK_ROWS || columns < MIN_DOCK_COLUMNS {
            return Err(DockError::TooSmall);
        }
        let width = columns.checked_sub(1).ok_or(DockError::TooSmall)?;
        let width_usize = usize::from(width);
        let compact = rows < MIN_ENHANCED_ROWS || columns < MIN_ENHANCED_COLUMNS;
        // A fixed-height dock makes every redraw and supported resize a
        // replace-in-place operation. Extra history belongs in Inspect, not in
        // an ever-growing input surface.
        let composer_rows = if compact { 1 } else { 4 };
        // Reserve one cell for a software cursor. The enhanced renderer keeps
        // the real cursor on the physical last row so a resize does not anchor
        // reflow in the middle of the transcript.
        let wrapped = wrap_composer(model.composer, width_usize - 3, composer_rows)?;
        let mut lines = Vec::new();
        lines
            .try_reserve_exact(MAX_DOCK_ROWS)
            .map_err(|_| DockError::Capacity)?;

        let software_cursor = !matches!(model.interaction, DockInteraction::Approval(_));
        let composer_start = if let DockInteraction::Approval(selected) = model.interaction {
            lines.push(line(
                DockRole::Notice,
                fit_ascii(
                    if compact {
                        "Not applied"
                    } else {
                        "Approval required | proposed action above"
                    },
                    width_usize,
                ),
            ));
            lines.push(line(DockRole::Divider, "-".repeat(width_usize)));
            let choices = [
                (
                    DockApprovalSelection::AllowOnce,
                    "Allow once | apply exact preview",
                ),
                (DockApprovalSelection::Reject, "Reject | make no change"),
                (DockApprovalSelection::Cancel, "Stop turn | cancel work"),
            ];
            for (choice, label) in choices {
                if compact && choice != selected {
                    continue;
                }
                let label = if compact {
                    match choice {
                        DockApprovalSelection::AllowOnce => "Allow once",
                        DockApprovalSelection::Reject => "Reject",
                        DockApprovalSelection::Cancel => "Stop turn",
                    }
                } else {
                    label
                };
                let marker = if choice == selected { ">" } else { " " };
                lines.push(line(
                    DockRole::ApprovalChoice,
                    fit_ascii(&format!(" {marker} {label}"), width_usize),
                ));
            }
            if !compact {
                lines.push(line(
                    DockRole::ApprovalWarning,
                    fit_ascii("Not sandboxed | Reject is the safe default", width_usize),
                ));
            }
            lines.push(line(
                DockRole::Hint,
                fit_ascii(
                    if compact {
                        "Arrows + Enter"
                    } else {
                        "Arrow keys move | Enter confirms | Esc stops"
                    },
                    width_usize,
                ),
            ));
            0
        } else {
            if let Some(notice) = model.notice {
                let notice = render_visible_owned(notice, false)?;
                lines.push(line(DockRole::Notice, truncate_cells(&notice, width_usize)));
            } else if model.queue.len() != 0 {
                let queue = if columns < 60 {
                    format!("next: {} queued | Up edits newest", model.queue.len())
                } else {
                    format!(
                        "Next turn queued | {} item{} | {} KiB | Up edits newest",
                        model.queue.len(),
                        if model.queue.len() == 1 { "" } else { "s" },
                        model.queue.total_bytes().div_ceil(1024),
                    )
                };
                lines.push(line(DockRole::Queue, fit_ascii(&queue, width_usize)));
            } else {
                let status = match model.interaction {
                    DockInteraction::Idle => "Ready",
                    DockInteraction::Running => "Working | type the next prompt while dsh runs",
                    DockInteraction::Approval(_) => "Approval required",
                };
                lines.push(line(DockRole::Queue, fit_ascii(status, width_usize)));
            }
            lines.push(line(DockRole::Divider, "-".repeat(width_usize)));
            let composer_start = lines.len();
            for (index, content) in wrapped.rows.into_iter().enumerate() {
                let prefix = if index == 0 && !wrapped.hidden_above {
                    "❯ "
                } else if index == 0 {
                    "^ "
                } else {
                    "  "
                };
                let mut text = String::new();
                text.try_reserve_exact(prefix.len() + content.len())
                    .map_err(|_| DockError::Capacity)?;
                text.push_str(prefix);
                text.push_str(&content);
                lines.push(line(DockRole::Composer, text));
            }
            while lines.len() < composer_start + composer_rows {
                lines.push(line(DockRole::Composer, "  ".to_owned()));
            }
            let hint = match model.interaction {
                DockInteraction::Idle if compact => "Enter send",
                DockInteraction::Idle => "Enter send | Ctrl+J newline | Up history | ? help",
                DockInteraction::Running if compact => "Enter queue",
                DockInteraction::Running => {
                    "Enter queue | Ctrl+J newline | Ctrl+C stop current turn"
                }
                DockInteraction::Approval(_) => "Arrow keys move | Enter confirms | Esc stops",
            };
            let mut hint = if wrapped.hidden_below {
                format!("v more | {hint}")
            } else {
                hint.to_owned()
            };
            if model.notice.is_some() && model.queue.len() != 0 {
                hint = format!("next: {} queued | {hint}", model.queue.len());
            }
            lines.push(line(DockRole::Hint, fit_ascii(&hint, width_usize)));
            composer_start
        };
        if lines.len() > MAX_DOCK_ROWS || lines.len() >= usize::from(rows) {
            return Err(DockError::Limit);
        }
        let dock_rows = u16::try_from(lines.len()).map_err(|_| DockError::Limit)?;
        let output_bottom = rows.checked_sub(dock_rows).ok_or(DockError::TooSmall)?;
        if output_bottom == 0 {
            return Err(DockError::TooSmall);
        }
        let cursor_row = composer_start
            .checked_add(wrapped.cursor_row)
            .and_then(|row| u16::try_from(row).ok())
            .ok_or(DockError::Limit)?;
        let cursor_column = wrapped
            .cursor_column
            .checked_add(2)
            .and_then(|column| u16::try_from(column).ok())
            .ok_or(DockError::Limit)?;
        if cursor_column > width {
            return Err(DockError::InvalidState);
        }
        Ok(Self {
            lines,
            cursor_row,
            cursor_column,
            width,
            terminal_rows: rows,
            output_bottom,
            software_cursor,
        })
    }

    pub(crate) fn layout_detail(
        document: &DetailDocument,
        requested_offset: usize,
        rows: u16,
        columns: u16,
    ) -> Result<(Self, DetailViewport), DockError> {
        if rows < MIN_DOCK_ROWS || columns < MIN_DOCK_COLUMNS {
            return Err(DockError::TooSmall);
        }
        let width = columns.checked_sub(1).ok_or(DockError::TooSmall)?;
        let width_usize = usize::from(width);
        let panel_rows = usize::from(rows.saturating_sub(1)).min(MAX_DOCK_ROWS);
        if panel_rows < 4 {
            return Err(DockError::TooSmall);
        }
        let has_divider = panel_rows >= 7;
        let fixed_rows = 2 + usize::from(has_divider);
        let page_rows = panel_rows
            .checked_sub(fixed_rows)
            .filter(|rows| *rows != 0)
            .ok_or(DockError::TooSmall)?;
        let mut physical = Vec::new();
        physical
            .try_reserve(MAX_DETAIL_WRAPPED_ROWS.min(document.lines().len().saturating_mul(2)))
            .map_err(|_| DockError::Capacity)?;
        let omission_line = line(DockRole::DetailCaution, DETAIL_WRAPPED_OMISSION.to_owned());
        let mut truncated = false;
        for source in document.lines() {
            if wrap_detail_line(source.tone(), source.text(), width_usize, &mut physical)? {
                truncated = true;
                break;
            }
        }
        if truncated {
            let last = physical.last_mut().ok_or(DockError::InvalidState)?;
            *last = omission_line;
        }
        if physical.is_empty() {
            physical.push(line(
                DockRole::DetailMuted,
                "No retained details.".to_owned(),
            ));
        }
        let total_rows = physical.len();
        let offset = requested_offset.min(total_rows.saturating_sub(page_rows));
        let mut lines = Vec::new();
        lines
            .try_reserve_exact(panel_rows)
            .map_err(|_| DockError::Capacity)?;
        lines.push(line(
            DockRole::DetailTitle,
            fit_ascii(document.title(), width_usize),
        ));
        if has_divider {
            lines.push(line(DockRole::Divider, "-".repeat(width_usize)));
        }
        for source in physical.iter().skip(offset).take(page_rows) {
            lines.push(line(source.role, truncate_cells(&source.text, width_usize)));
        }
        while lines.len() < panel_rows - 1 {
            lines.push(line(DockRole::DetailPlain, String::new()));
        }
        let total_label = if truncated {
            format!("{total_rows}+")
        } else {
            total_rows.to_string()
        };
        let position = if total_rows <= page_rows && !truncated {
            format!("all {total_label} rows")
        } else {
            format!(
                "rows {}-{} / {total_label}",
                offset + 1,
                (offset + page_rows).min(total_rows)
            )
        };
        let footer = if columns >= 80 {
            format!("{position} · Up/Down/PgUp/PgDn scroll · Tab switch · Esc Focus")
        } else {
            format!("{position} · PgUp/Dn · Esc Focus")
        };
        lines.push(line(DockRole::Hint, fit_ascii(&footer, width_usize)));
        if lines.len() != panel_rows {
            return Err(DockError::InvalidState);
        }
        let dock_rows = u16::try_from(lines.len()).map_err(|_| DockError::Limit)?;
        let output_bottom = rows.checked_sub(dock_rows).ok_or(DockError::TooSmall)?;
        if output_bottom == 0 {
            return Err(DockError::TooSmall);
        }
        Ok((
            Self {
                lines,
                cursor_row: 0,
                cursor_column: 1,
                width,
                terminal_rows: rows,
                output_bottom,
                software_cursor: false,
            },
            DetailViewport {
                offset,
                total_rows,
                page_rows,
                truncated,
            },
        ))
    }

    pub(crate) fn rows(&self) -> Result<u16, DockError> {
        u16::try_from(self.lines.len()).map_err(|_| DockError::Limit)
    }

    pub(crate) const fn terminal_rows(&self) -> u16 {
        self.terminal_rows
    }

    pub(crate) const fn terminal_columns(&self) -> u16 {
        self.width + 1
    }

    pub(crate) const fn output_bottom(&self) -> u16 {
        self.output_bottom
    }

    /// Writes only the owned bottom rows. Coordinate ownership remains with
    /// `InlineScreen`; this pure layout object never establishes a scrolling
    /// region or decides how transcript output moves.
    pub(crate) fn render_bottom(&self, output: &mut String, styled: bool) -> Result<(), DockError> {
        let start_row = self.output_bottom.checked_add(1).ok_or(DockError::Limit)?;
        output.push_str("\x1b[?25l");
        push_absolute_frame_lines(
            output,
            &self.lines,
            start_row,
            styled,
            self.software_cursor
                .then_some((self.cursor_row, self.cursor_column)),
        )?;
        push_cup(output, self.terminal_rows, 1);
        output.push_str("\x1b[0m");
        Ok(())
    }

    pub(crate) fn clear_bottom(&self, output: &mut String) -> Result<(), DockError> {
        let start = self.output_bottom.checked_add(1).ok_or(DockError::Limit)?;
        for row in start..=self.terminal_rows {
            push_cup(output, row, 1);
            output.push_str("\x1b[2K");
        }
        Ok(())
    }
}

fn push_absolute_frame_lines(
    output: &mut String,
    lines: &[DockLine],
    start_row: u16,
    styled: bool,
    software_cursor: Option<(u16, u16)>,
) -> Result<(), DockError> {
    for (index, line) in lines.iter().enumerate() {
        let row = start_row
            .checked_add(u16::try_from(index).map_err(|_| DockError::Limit)?)
            .ok_or(DockError::Limit)?;
        push_cup(output, row, 1);
        output.push_str("\x1b[2K");
        let cursor_column = software_cursor
            .filter(|(cursor_row, _)| usize::from(*cursor_row) == index)
            .map(|(_, cursor_column)| cursor_column);
        push_line(output, line, styled, cursor_column)?;
    }
    Ok(())
}

fn push_cup(output: &mut String, row: u16, column: u16) {
    write!(output, "\x1b[{row};{column}H")
        .expect("writing a bounded cursor-position command cannot fail");
}

fn line(role: DockRole, text: String) -> DockLine {
    DockLine { role, text }
}

struct WrappedComposer {
    rows: VecDeque<String>,
    cursor_row: usize,
    cursor_column: usize,
    hidden_above: bool,
    hidden_below: bool,
}

fn wrap_composer(
    composer: &Composer,
    width: usize,
    max_rows: usize,
) -> Result<WrappedComposer, DockError> {
    if width == 0 || max_rows == 0 {
        return Err(DockError::TooSmall);
    }
    let before = render_visible_owned(&composer.text()[..composer.cursor()], true)?;
    let after = render_visible_owned(&composer.text()[composer.cursor()..], true)?;
    let mut rows = VecDeque::new();
    rows.try_reserve(max_rows)
        .map_err(|_| DockError::Capacity)?;
    rows.push_back(String::new());
    let mut hidden_above = false;
    push_visible_segment(&before, width, max_rows, &mut rows, &mut hidden_above, true)?;
    let cursor_row = rows.len() - 1;
    let cursor_column = rows
        .back()
        .map_or(0, |row| UnicodeWidthStr::width(row.as_str()));
    let mut hidden_below = false;
    push_visible_segment(&after, width, max_rows, &mut rows, &mut hidden_below, false)?;
    Ok(WrappedComposer {
        rows,
        cursor_row,
        cursor_column,
        hidden_above,
        hidden_below,
    })
}

fn push_visible_segment(
    segment: &str,
    width: usize,
    max_rows: usize,
    rows: &mut VecDeque<String>,
    hidden: &mut bool,
    may_discard_front: bool,
) -> Result<(), DockError> {
    for grapheme in segment.graphemes(true) {
        if grapheme == "\n" {
            if !start_row(rows, max_rows, hidden, may_discard_front)? {
                return Ok(());
            }
            continue;
        }
        let cells = UnicodeWidthStr::width(grapheme);
        if cells > width {
            let placeholder = fit_ascii("[wide grapheme]", width);
            let current = rows
                .back()
                .map_or(0, |row| UnicodeWidthStr::width(row.as_str()));
            let placeholder_cells = UnicodeWidthStr::width(placeholder.as_str());
            if current != 0
                && current + placeholder_cells > width
                && !start_row(rows, max_rows, hidden, may_discard_front)?
            {
                return Ok(());
            }
            let row = rows.back_mut().ok_or(DockError::InvalidState)?;
            row.try_reserve(placeholder.len())
                .map_err(|_| DockError::Capacity)?;
            row.push_str(&placeholder);
            continue;
        }
        let current = rows
            .back()
            .map_or(0, |row| UnicodeWidthStr::width(row.as_str()));
        if current != 0
            && current + cells > width
            && !start_row(rows, max_rows, hidden, may_discard_front)?
        {
            return Ok(());
        }
        let row = rows.back_mut().ok_or(DockError::InvalidState)?;
        row.try_reserve(grapheme.len())
            .map_err(|_| DockError::Capacity)?;
        row.push_str(grapheme);
    }
    Ok(())
}

fn start_row(
    rows: &mut VecDeque<String>,
    max_rows: usize,
    hidden: &mut bool,
    may_discard_front: bool,
) -> Result<bool, DockError> {
    if rows.len() == max_rows {
        *hidden = true;
        if !may_discard_front {
            return Ok(false);
        }
        let _ = rows.pop_front();
    }
    rows.try_reserve(1).map_err(|_| DockError::Capacity)?;
    rows.push_back(String::new());
    Ok(true)
}

fn truncate_cells(text: &str, width: usize) -> String {
    let mut output = String::new();
    let mut cells = 0_usize;
    for grapheme in text.graphemes(true) {
        let next = cells + UnicodeWidthStr::width(grapheme);
        if next > width {
            break;
        }
        output.push_str(grapheme);
        cells = next;
    }
    output
}

fn fit_ascii(text: &str, width: usize) -> String {
    if UnicodeWidthStr::width(text) <= width {
        text.to_owned()
    } else if width > 3 {
        let mut shortened = truncate_cells(text, width - 3);
        shortened.push_str("...");
        shortened
    } else {
        String::new()
    }
}

fn wrap_detail_line(
    tone: DetailTone,
    text: &str,
    width: usize,
    output: &mut Vec<DockLine>,
) -> Result<bool, DockError> {
    if width == 0 {
        return Err(DockError::TooSmall);
    }
    let role = detail_role(tone);
    let mut current = String::new();
    let mut cells = 0_usize;
    for grapheme in text.graphemes(true) {
        let grapheme_cells = UnicodeWidthStr::width(grapheme);
        let display = if grapheme_cells > width || grapheme.len() > 1024 {
            "[wide grapheme]"
        } else {
            grapheme
        };
        let display_cells = UnicodeWidthStr::width(display);
        if cells != 0 && cells.saturating_add(display_cells) > width {
            if push_detail_physical(output, role, std::mem::take(&mut current))? {
                return Ok(true);
            }
            cells = 0;
        }
        current
            .try_reserve(display.len())
            .map_err(|_| DockError::Capacity)?;
        current.push_str(display);
        cells = cells.saturating_add(display_cells);
    }
    push_detail_physical(output, role, current)
}

fn push_detail_physical(
    output: &mut Vec<DockLine>,
    role: DockRole,
    text: String,
) -> Result<bool, DockError> {
    if output.len() == MAX_DETAIL_WRAPPED_ROWS {
        return Ok(true);
    }
    output.try_reserve(1).map_err(|_| DockError::Capacity)?;
    output.push(line(role, text));
    Ok(false)
}

const fn detail_role(tone: DetailTone) -> DockRole {
    match tone {
        DetailTone::Plain => DockRole::DetailPlain,
        DetailTone::Muted => DockRole::DetailMuted,
        DetailTone::Accent => DockRole::DetailAccent,
        DetailTone::Positive => DockRole::DetailPositive,
        DetailTone::Caution => DockRole::DetailCaution,
        DetailTone::Negative => DockRole::DetailNegative,
        DetailTone::Code => DockRole::DetailCode,
    }
}

fn push_line(
    output: &mut String,
    line: &DockLine,
    styled: bool,
    cursor_column: Option<u16>,
) -> Result<(), DockError> {
    if styled {
        output.push_str(match line.role {
            DockRole::Queue | DockRole::Hint => "\x1b[2m",
            DockRole::Divider => "\x1b[2;36m",
            DockRole::Composer => "\x1b[0m",
            DockRole::Notice => "\x1b[1;33m",
            DockRole::ApprovalChoice => "\x1b[1m",
            DockRole::ApprovalWarning => "\x1b[1;33m",
            DockRole::DetailTitle | DockRole::DetailAccent => "\x1b[1;36m",
            DockRole::DetailPlain => "\x1b[0m",
            DockRole::DetailMuted => "\x1b[2m",
            DockRole::DetailPositive => "\x1b[32m",
            DockRole::DetailCaution => "\x1b[1;33m",
            DockRole::DetailNegative => "\x1b[1;31m",
            DockRole::DetailCode => "\x1b[1m",
        });
    }
    output
        .try_reserve(line.text.len() + 4)
        .map_err(|_| DockError::Capacity)?;
    if let Some(column) = cursor_column {
        let split = byte_at_cell(&line.text, usize::from(column)).ok_or(DockError::InvalidState)?;
        output.push_str(&line.text[..split]);
        output.push_str("\x1b[7m \x1b[0m");
        output.push_str(&line.text[split..]);
    } else {
        output.push_str(&line.text);
    }
    if styled {
        output.push_str("\x1b[0m");
    }
    Ok(())
}

fn byte_at_cell(text: &str, target: usize) -> Option<usize> {
    let mut cells = 0_usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if cells == target {
            return Some(byte);
        }
        cells = cells.checked_add(UnicodeWidthStr::width(grapheme))?;
        if cells > target {
            return None;
        }
    }
    (cells == target).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::{
        DETAIL_WRAPPED_OMISSION, DockApprovalSelection, DockFrame, DockInteraction, DockModel,
        MAX_DETAIL_WRAPPED_ROWS,
    };
    use crate::tui::{
        composer::Composer,
        input_memory::PromptQueue,
        view::{DetailDocument, DetailTone, ViewMode},
    };

    #[test]
    fn responsive_frames_fit_44_80_and_112_columns() {
        for (rows, columns) in [(20, 44), (24, 80), (34, 112)] {
            let mut composer = Composer::default();
            composer
                .insert_text("修复 timeout，保留 e\u{301} 和 SECRET\u{202e}SAFE")
                .unwrap();
            let queue = PromptQueue::default();
            let frame = DockFrame::layout(
                DockModel {
                    interaction: DockInteraction::Idle,
                    composer: &composer,
                    queue: &queue,
                    notice: None,
                },
                rows,
                columns,
            )
            .unwrap();
            for line in &frame.lines {
                assert!(
                    unicode_width::UnicodeWidthStr::width(line.text.as_str())
                        <= usize::from(columns - 1)
                );
                assert!(!line.text.contains('\u{202e}'));
            }
            assert!(frame.cursor_column < columns);
        }
    }

    #[test]
    fn compact_rescue_frames_keep_input_and_approval_visible_at_15_by_6() {
        let mut composer = Composer::default();
        composer.insert_text("draft").unwrap();
        let queue = PromptQueue::default();
        for interaction in [
            DockInteraction::Running,
            DockInteraction::Approval(DockApprovalSelection::Reject),
        ] {
            let frame = DockFrame::layout(
                DockModel {
                    interaction,
                    composer: &composer,
                    queue: &queue,
                    notice: None,
                },
                6,
                15,
            )
            .unwrap();
            assert_eq!(frame.rows().unwrap(), 4);
            assert_eq!(frame.output_bottom(), 2);
            if matches!(interaction, DockInteraction::Approval(_)) {
                assert_eq!(frame.lines[0].text, "Not applied");
                assert!(
                    frame
                        .lines
                        .iter()
                        .any(|line| line.text.contains("> Reject"))
                );
            }
            assert!(
                frame.lines.iter().all(|line| {
                    unicode_width::UnicodeWidthStr::width(line.text.as_str()) <= 14
                })
            );
        }
    }

    #[test]
    fn compact_rescue_has_exact_twelve_by_five_boundaries() {
        let mut composer = Composer::default();
        composer.insert_text("x").unwrap();
        let queue = PromptQueue::default();
        let frame = DockFrame::layout(
            DockModel {
                interaction: DockInteraction::Approval(DockApprovalSelection::Reject),
                composer: &composer,
                queue: &queue,
                notice: None,
            },
            5,
            12,
        )
        .unwrap();
        assert_eq!(frame.rows().unwrap(), 4);
        assert_eq!(frame.output_bottom(), 1);
        assert_eq!(frame.lines[0].text, "Not applied");
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.text.contains("> Reject"))
        );
        assert!(
            DockFrame::layout(
                DockModel {
                    interaction: DockInteraction::Idle,
                    composer: &composer,
                    queue: &queue,
                    notice: None,
                },
                5,
                11,
            )
            .is_err()
        );
        assert!(
            DockFrame::layout(
                DockModel {
                    interaction: DockInteraction::Idle,
                    composer: &composer,
                    queue: &queue,
                    notice: None,
                },
                4,
                12,
            )
            .is_err()
        );
    }

    #[test]
    fn full_approval_surface_keeps_status_choices_and_safety_text_at_supported_widths() {
        let composer = Composer::default();
        let queue = PromptQueue::default();
        for (rows, columns) in [(20, 44), (24, 80), (34, 112)] {
            let frame = DockFrame::layout(
                DockModel {
                    interaction: DockInteraction::Approval(DockApprovalSelection::Reject),
                    composer: &composer,
                    queue: &queue,
                    notice: None,
                },
                rows,
                columns,
            )
            .unwrap();
            assert_eq!(frame.lines.len(), 7);
            assert!(frame.lines[0].text.contains("Approval required"));
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("Allow once"))
            );
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("> Reject"))
            );
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("Stop turn"))
            );
            assert!(
                frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains("Not sandboxed"))
            );
            assert!(frame.lines.iter().all(|line| {
                unicode_width::UnicodeWidthStr::width(line.text.as_str())
                    <= usize::from(columns - 1)
            }));
        }
    }

    #[test]
    fn one_overwide_grapheme_is_replaced_without_autowrap() {
        let mut composer = Composer::default();
        let mut grapheme = String::from("क");
        grapheme.extend(std::iter::repeat_n('ा', 50));
        composer.insert_text(&grapheme).unwrap();
        let queue = PromptQueue::default();
        let frame = DockFrame::layout(
            DockModel {
                interaction: DockInteraction::Idle,
                composer: &composer,
                queue: &queue,
                notice: None,
            },
            20,
            44,
        )
        .unwrap();
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.text.contains("[wide grapheme]"))
        );
        for line in &frame.lines {
            assert!(unicode_width::UnicodeWidthStr::width(line.text.as_str()) <= 43);
        }
    }

    #[test]
    fn debug_never_contains_composer_or_notice_text() {
        let mut composer = Composer::default();
        composer.insert_text("SECRET_DRAFT").unwrap();
        let queue = PromptQueue::default();
        let model = DockModel {
            interaction: DockInteraction::Idle,
            composer: &composer,
            queue: &queue,
            notice: Some("SECRET_NOTICE"),
        };
        let model_debug = format!("{model:?}");
        assert!(!model_debug.contains("SECRET_DRAFT"));
        assert!(!model_debug.contains("SECRET_NOTICE"));
        let frame = DockFrame::layout(model, 24, 80).unwrap();
        assert!(!format!("{frame:?}").contains("SECRET_DRAFT"));
    }

    #[test]
    fn detail_wrapped_rows_exact_and_one_over_have_truthful_markers() {
        let exact_text = "x".repeat(43 * MAX_DETAIL_WRAPPED_ROWS);
        let exact_document = DetailDocument::from_lines_for_test(
            ViewMode::Inspect,
            "INSPECT",
            &[(DetailTone::Code, exact_text.as_str())],
        );
        let (exact, exact_viewport) =
            DockFrame::layout_detail(&exact_document, usize::MAX, 20, 44).unwrap();
        assert_eq!(exact_viewport.total_rows, MAX_DETAIL_WRAPPED_ROWS);
        assert!(!exact_viewport.truncated);
        assert!(
            !exact
                .lines
                .iter()
                .any(|line| line.text.contains(DETAIL_WRAPPED_OMISSION))
        );
        assert!(!exact.lines.last().unwrap().text.contains("4096+"));

        let one_over_text = "x".repeat(43 * MAX_DETAIL_WRAPPED_ROWS + 1);
        let one_over_document = DetailDocument::from_lines_for_test(
            ViewMode::Inspect,
            "INSPECT",
            &[(DetailTone::Code, one_over_text.as_str())],
        );
        let (one_over, one_over_viewport) =
            DockFrame::layout_detail(&one_over_document, usize::MAX, 20, 44).unwrap();
        assert_eq!(one_over_viewport.total_rows, MAX_DETAIL_WRAPPED_ROWS);
        assert!(one_over_viewport.truncated);
        assert_eq!(
            one_over
                .lines
                .iter()
                .filter(|line| line.text.contains(DETAIL_WRAPPED_OMISSION))
                .count(),
            1
        );
        assert!(one_over.lines.last().unwrap().text.contains("4096+"));
        assert!(
            one_over
                .lines
                .iter()
                .all(|line| { unicode_width::UnicodeWidthStr::width(line.text.as_str()) <= 43 })
        );
    }

    #[test]
    fn detail_frames_fit_supported_widths_and_report_truthful_viewports() {
        let document = DetailDocument::from_lines_for_test(
            ViewMode::Review,
            "REVIEW · SECRET\u{202e}SAFE",
            &[
                (DetailTone::Accent, "ACTIONS"),
                (DetailTone::Positive, "Updated  src/界面.rs"),
                (DetailTone::Negative, "Command failed  exit 1"),
                (
                    DetailTone::Plain,
                    "e\u{301} and a long detail that wraps without autowrap",
                ),
            ],
        );
        for (rows, columns, expected_rows, expected_page_rows) in
            [(20, 44, 19, 16), (24, 80, 23, 20), (34, 112, 24, 21)]
        {
            let (frame, viewport) =
                DockFrame::layout_detail(&document, usize::MAX, rows, columns).unwrap();
            assert_eq!(frame.rows().unwrap(), expected_rows);
            assert_eq!(viewport.page_rows, expected_page_rows);
            assert_eq!(
                viewport.offset,
                viewport.total_rows.saturating_sub(viewport.page_rows)
            );
            assert!(!viewport.truncated);
            assert!(!frame.software_cursor);
            assert!(frame.lines[0].text.contains("REVIEW"));
            assert!(
                !frame
                    .lines
                    .iter()
                    .any(|line| line.text.contains('\u{202e}'))
            );
            assert!(frame.lines.iter().all(|line| {
                unicode_width::UnicodeWidthStr::width(line.text.as_str())
                    <= usize::from(columns - 1)
            }));
        }
        assert!(!format!("{document:?}").contains("SECRET"));
    }

    #[test]
    fn detail_replaces_one_overlong_zero_width_grapheme() {
        let mut grapheme = String::from("क");
        grapheme.extend(std::iter::repeat_n('\u{93e}', 1_100));
        let document = DetailDocument::from_lines_for_test(
            ViewMode::Inspect,
            "INSPECT",
            &[(DetailTone::Code, grapheme.as_str())],
        );
        let (frame, _) = DockFrame::layout_detail(&document, 0, 20, 44).unwrap();
        assert!(
            frame
                .lines
                .iter()
                .any(|line| line.text.contains("[wide grapheme]"))
        );
        assert!(
            frame
                .lines
                .iter()
                .all(|line| { unicode_width::UnicodeWidthStr::width(line.text.as_str()) <= 43 })
        );
    }
}
