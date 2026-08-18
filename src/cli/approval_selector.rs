use std::{fmt::Write as _, time::Duration};

use crate::session::ApprovalOutcome;

use super::{
    approval::{ApprovalAnswer, parse_approval_answer},
    input::MAX_APPROVAL_RECORD_BYTES,
};

pub(super) const ESCAPE_SEQUENCE_WAIT: Duration = Duration::from_millis(35);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ApprovalSelection {
    AllowOnce,
    Reject,
    Cancel,
}

impl ApprovalSelection {
    const fn previous(self) -> Self {
        match self {
            Self::AllowOnce => Self::Cancel,
            Self::Reject => Self::AllowOnce,
            Self::Cancel => Self::Reject,
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::AllowOnce => Self::Reject,
            Self::Reject => Self::Cancel,
            Self::Cancel => Self::AllowOnce,
        }
    }

    const fn outcome(self) -> ApprovalOutcome {
        match self {
            Self::AllowOnce => ApprovalOutcome::AllowedOnce,
            Self::Reject => ApprovalOutcome::Rejected,
            Self::Cancel => ApprovalOutcome::Cancelled,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EscapeState {
    None,
    Escape,
    ControlSequence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SelectorUpdate {
    None,
    Redraw,
    Decide(ApprovalOutcome),
    Eof,
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SelectorRenderError;

pub(super) struct ApprovalSelector {
    selected: ApprovalSelection,
    record: [u8; MAX_APPROVAL_RECORD_BYTES],
    record_len: usize,
    escape: EscapeState,
}

impl ApprovalSelector {
    pub(super) const fn new() -> Self {
        Self {
            // Enter must be safe even if a stale byte crosses the input fence.
            selected: ApprovalSelection::Reject,
            record: [0; MAX_APPROVAL_RECORD_BYTES],
            record_len: 0,
            escape: EscapeState::None,
        }
    }

    #[cfg(test)]
    const fn selected(&self) -> ApprovalSelection {
        self.selected
    }

    pub(super) fn render(
        &self,
        color: bool,
        compact: bool,
        redraw: bool,
    ) -> Result<String, SelectorRenderError> {
        let mut output = String::new();
        output
            .try_reserve_exact(512)
            .map_err(|_| SelectorRenderError)?;
        if color && redraw {
            output.push_str("\x1b[5A");
        }

        let title = if color {
            "\x1b[1;33m◆ Approval required\x1b[0m"
        } else {
            "[approval required]"
        };
        push_selector_line(&mut output, color && redraw, title)?;
        for (choice, label) in [
            (ApprovalSelection::AllowOnce, "Allow once"),
            (ApprovalSelection::Reject, "Reject"),
            (ApprovalSelection::Cancel, "Cancel"),
        ] {
            let selected = self.selected == choice;
            if color && redraw {
                output.push_str("\r\x1b[2K");
            }
            if color && selected {
                writeln!(&mut output, "  \x1b[1;30;43m › {label} \x1b[0m")
                    .map_err(|_| SelectorRenderError)?;
            } else if color {
                writeln!(&mut output, "     {label}").map_err(|_| SelectorRenderError)?;
            } else if selected {
                writeln!(&mut output, "  [x] {label}").map_err(|_| SelectorRenderError)?;
            } else {
                writeln!(&mut output, "  [ ] {label}").map_err(|_| SelectorRenderError)?;
            }
        }
        let hint = if compact {
            "arrows · Enter confirm · Esc cancel"
        } else {
            "↑/↓ or ←/→ move · Enter confirm · Esc cancel"
        };
        if color && redraw {
            output.push_str("\r\x1b[2K");
        }
        if color {
            writeln!(&mut output, "  \x1b[2m{hint}\x1b[0m").map_err(|_| SelectorRenderError)?;
        } else {
            writeln!(&mut output, "  {hint}").map_err(|_| SelectorRenderError)?;
        }
        Ok(output)
    }

    pub(super) const fn escape_is_pending(&self) -> bool {
        !matches!(self.escape, EscapeState::None)
    }

    pub(super) fn expire_escape(&mut self) -> SelectorUpdate {
        if self.escape_is_pending() {
            self.escape = EscapeState::None;
            self.record_len = 0;
            SelectorUpdate::Decide(ApprovalOutcome::Cancelled)
        } else {
            SelectorUpdate::None
        }
    }

    pub(super) fn feed(&mut self, bytes: &[u8], challenge: uuid::Uuid) -> SelectorUpdate {
        let mut redraw = false;
        for &byte in bytes {
            let update = match self.escape {
                EscapeState::Escape => self.feed_after_escape(byte),
                EscapeState::ControlSequence => self.feed_control_sequence(byte),
                EscapeState::None => self.feed_plain(byte, challenge),
            };
            match update {
                SelectorUpdate::None => {}
                SelectorUpdate::Redraw => redraw = true,
                decision @ (SelectorUpdate::Decide(_)
                | SelectorUpdate::Eof
                | SelectorUpdate::Invalid) => {
                    return decision;
                }
            }
        }
        if redraw {
            SelectorUpdate::Redraw
        } else {
            SelectorUpdate::None
        }
    }

    fn feed_after_escape(&mut self, byte: u8) -> SelectorUpdate {
        if byte == b'[' || byte == b'O' {
            self.escape = EscapeState::ControlSequence;
            SelectorUpdate::None
        } else {
            self.escape = EscapeState::None;
            self.record_len = 0;
            SelectorUpdate::Decide(ApprovalOutcome::Cancelled)
        }
    }

    fn feed_control_sequence(&mut self, byte: u8) -> SelectorUpdate {
        self.escape = EscapeState::None;
        if self.record_len != 0 {
            self.record_len = 0;
            return SelectorUpdate::Invalid;
        }
        match byte {
            b'A' | b'D' | b'Z' => {
                self.selected = self.selected.previous();
                SelectorUpdate::Redraw
            }
            b'B' | b'C' => {
                self.selected = self.selected.next();
                SelectorUpdate::Redraw
            }
            // Reject bracketed paste, modifier sequences, and every unknown
            // terminal sequence before any later pasted byte can authorize.
            _ => SelectorUpdate::Invalid,
        }
    }

    fn feed_plain(&mut self, byte: u8, challenge: uuid::Uuid) -> SelectorUpdate {
        match byte {
            0x1b => {
                self.escape = EscapeState::Escape;
                SelectorUpdate::None
            }
            b'\n' | b'\r' => self.confirm(challenge),
            0x04 => SelectorUpdate::Eof,
            b'\t' if self.record_len == 0 => {
                self.selected = self.selected.next();
                SelectorUpdate::Redraw
            }
            b'h' | b'k' if self.record_len == 0 => {
                self.selected = self.selected.previous();
                SelectorUpdate::Redraw
            }
            b'j' | b'l' if self.record_len == 0 => {
                self.selected = self.selected.next();
                SelectorUpdate::Redraw
            }
            0x08 | 0x7f if self.record_len != 0 => {
                self.record_len -= 1;
                self.update_shortcut_selection()
            }
            byte if byte.is_ascii_graphic() || byte == b' ' => {
                if self.record_len == self.record.len() {
                    self.record_len = 0;
                    return SelectorUpdate::Invalid;
                }
                self.record[self.record_len] = byte;
                self.record_len += 1;
                self.update_shortcut_selection()
            }
            _ => SelectorUpdate::Invalid,
        }
    }

    fn update_shortcut_selection(&mut self) -> SelectorUpdate {
        let selected = match &self.record[..self.record_len] {
            b"y" | b"yes" | b"allow" => Some(ApprovalSelection::AllowOnce),
            b"n" | b"no" | b"reject" => Some(ApprovalSelection::Reject),
            b"c" | b"cancel" => Some(ApprovalSelection::Cancel),
            _ => None,
        };
        if let Some(selected) = selected {
            let changed = self.selected != selected;
            self.selected = selected;
            if changed {
                return SelectorUpdate::Redraw;
            }
        }
        SelectorUpdate::None
    }

    fn confirm(&mut self, challenge: uuid::Uuid) -> SelectorUpdate {
        self.escape = EscapeState::None;
        if self.record_len == 0 {
            return SelectorUpdate::Decide(self.selected.outcome());
        }
        let record = std::str::from_utf8(&self.record[..self.record_len]);
        self.record_len = 0;
        match record
            .ok()
            .map(|record| parse_approval_answer(record, true, challenge))
        {
            Some(ApprovalAnswer::Decide(outcome)) => SelectorUpdate::Decide(outcome),
            Some(ApprovalAnswer::Retry) | None => SelectorUpdate::Invalid,
        }
    }
}

fn push_selector_line(
    output: &mut String,
    clear: bool,
    line: &str,
) -> Result<(), SelectorRenderError> {
    if clear {
        output.push_str("\r\x1b[2K");
    }
    writeln!(output, "{line}").map_err(|_| SelectorRenderError)
}

#[cfg(test)]
mod tests {
    use super::{ApprovalSelection, ApprovalSelector, SelectorUpdate};
    use crate::session::ApprovalOutcome;

    fn challenge() -> uuid::Uuid {
        uuid::Uuid::parse_str("00112233-4455-4677-8899-aabbccddeeff").unwrap()
    }

    #[test]
    fn reject_is_the_safe_default_and_enter_is_the_only_confirmation() {
        let mut selector = ApprovalSelector::new();
        assert_eq!(selector.selected(), ApprovalSelection::Reject);
        assert_eq!(selector.feed(b"y", challenge()), SelectorUpdate::Redraw);
        assert_eq!(selector.selected(), ApprovalSelection::AllowOnce);
        assert_eq!(
            selector.feed(b"\n", challenge()),
            SelectorUpdate::Decide(ApprovalOutcome::AllowedOnce)
        );

        let mut selector = ApprovalSelector::new();
        assert_eq!(
            selector.feed(b"\n", challenge()),
            SelectorUpdate::Decide(ApprovalOutcome::Rejected)
        );
    }

    #[test]
    fn fragmented_arrows_tab_and_vim_keys_move_without_authorizing() {
        let mut selector = ApprovalSelector::new();
        assert_eq!(selector.feed(b"\x1b", challenge()), SelectorUpdate::None);
        assert!(selector.escape_is_pending());
        assert_eq!(selector.feed(b"[", challenge()), SelectorUpdate::None);
        assert_eq!(selector.feed(b"A", challenge()), SelectorUpdate::Redraw);
        assert_eq!(selector.selected(), ApprovalSelection::AllowOnce);
        assert_eq!(selector.feed(b"\t", challenge()), SelectorUpdate::Redraw);
        assert_eq!(selector.selected(), ApprovalSelection::Reject);
        assert_eq!(
            selector.feed(b"\x1b[B", challenge()),
            SelectorUpdate::Redraw
        );
        assert_eq!(selector.selected(), ApprovalSelection::Cancel);
        assert_eq!(selector.feed(b"k", challenge()), SelectorUpdate::Redraw);
        assert_eq!(selector.selected(), ApprovalSelection::Reject);
        assert_eq!(selector.feed(b"h", challenge()), SelectorUpdate::Redraw);
        assert_eq!(selector.selected(), ApprovalSelection::AllowOnce);
        assert_eq!(selector.feed(b"j", challenge()), SelectorUpdate::Redraw);
        assert_eq!(selector.selected(), ApprovalSelection::Reject);
        assert_eq!(selector.feed(b"l", challenge()), SelectorUpdate::Redraw);
        assert_eq!(selector.selected(), ApprovalSelection::Cancel);
    }

    #[test]
    fn isolated_escape_cancels_and_unknown_sequences_fail_closed() {
        let mut selector = ApprovalSelector::new();
        assert_eq!(selector.feed(b"\x1b", challenge()), SelectorUpdate::None);
        assert_eq!(
            selector.expire_escape(),
            SelectorUpdate::Decide(ApprovalOutcome::Cancelled)
        );

        let mut selector = ApprovalSelector::new();
        assert_eq!(
            selector.feed(b"\x1b[200~y\n", challenge()),
            SelectorUpdate::Invalid
        );
        assert_eq!(selector.selected(), ApprovalSelection::Reject);
    }

    #[test]
    fn ctrl_d_remains_an_explicit_eof_in_cbreak_mode() {
        let mut selector = ApprovalSelector::new();
        assert_eq!(selector.feed(&[0x04], challenge()), SelectorUpdate::Eof);
    }

    #[test]
    fn exact_automation_records_remain_bounded_and_correlated() {
        let mut selector = ApprovalSelector::new();
        assert_eq!(
            selector.feed(b"allow 00112233-4455-4677-8899-aabbccddeeff\n", challenge(),),
            SelectorUpdate::Decide(ApprovalOutcome::AllowedOnce)
        );

        let mut selector = ApprovalSelector::new();
        assert_eq!(
            selector.feed(&[b'x'; 65], challenge()),
            SelectorUpdate::Invalid
        );
    }

    #[test]
    fn styled_redraw_is_product_owned_and_plain_output_has_no_escape_bytes() {
        let mut selector = ApprovalSelector::new();
        let plain = selector.render(false, false, false).unwrap();
        assert!(!plain.contains('\x1b'));
        assert!(plain.contains("[x] Reject"));
        assert!(plain.contains("Enter confirm"));

        assert_eq!(selector.feed(b"y", challenge()), SelectorUpdate::Redraw);
        let styled = selector.render(true, false, true).unwrap();
        assert!(styled.starts_with("\x1b[5A"));
        assert!(styled.contains("› Allow once"));
        assert!(styled.ends_with("\x1b[0m\n"));

        let narrow = selector.render(true, true, false).unwrap();
        assert!(!narrow.contains("\x1b[5A"));
        assert!(narrow.contains("Enter confirm"));
    }
}
