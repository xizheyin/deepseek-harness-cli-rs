use std::{fmt, ops::ControlFlow};

use thiserror::Error;

pub(crate) const MAX_PASTE_BYTES: usize = 64 * 1024;
const MAX_CSI_BYTES: usize = 32;
const PASTE_END: &[u8] = b"\x1b[201~";

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct InputEpoch(u64);

#[derive(Eq, PartialEq)]
pub(crate) struct DecodedInput {
    pub(crate) epoch: InputEpoch,
    pub(crate) event: InputEvent,
}

impl fmt::Debug for DecodedInput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DecodedInput")
            .field("epoch", &self.epoch)
            .field("event", &self.event)
            .finish()
    }
}

#[derive(Eq, PartialEq)]
pub(crate) enum InputEvent {
    Key(Key),
    PasteStarted,
    Paste(String),
    PasteRejected(InputError),
    Rejected(InputError),
}

impl fmt::Debug for InputEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Key(key) => key.fmt(formatter),
            Self::PasteStarted => formatter.write_str("PasteStarted"),
            Self::Paste(text) => formatter
                .debug_struct("Paste")
                .field("bytes", &text.len())
                .finish(),
            Self::PasteRejected(error) => {
                formatter.debug_tuple("PasteRejected").field(error).finish()
            }
            Self::Rejected(error) => formatter.debug_tuple("Rejected").field(error).finish(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum Key {
    Char(char),
    Enter,
    Newline,
    Escape,
    Eof,
    Tab,
    BackTab,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Backspace,
    Delete,
    WordErase,
    ClearBefore,
    ClearAfter,
    Yank,
    Undo,
    ReverseSearch,
}

impl fmt::Debug for Key {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Char(_) => "Char",
            Self::Enter => "Enter",
            Self::Newline => "Newline",
            Self::Escape => "Escape",
            Self::Eof => "Eof",
            Self::Tab => "Tab",
            Self::BackTab => "BackTab",
            Self::Left => "Left",
            Self::Right => "Right",
            Self::Up => "Up",
            Self::Down => "Down",
            Self::Home => "Home",
            Self::End => "End",
            Self::Backspace => "Backspace",
            Self::Delete => "Delete",
            Self::WordErase => "WordErase",
            Self::ClearBefore => "ClearBefore",
            Self::ClearAfter => "ClearAfter",
            Self::Yank => "Yank",
            Self::Undo => "Undo",
            Self::ReverseSearch => "ReverseSearch",
        })
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum InputError {
    #[error("CLI_INPUT_INVALID_UTF8")]
    InvalidUtf8,
    #[error("CLI_INPUT_UNKNOWN_SEQUENCE")]
    UnknownSequence,
    #[error("CLI_INPUT_INCOMPLETE_SEQUENCE")]
    IncompleteSequence,
    #[error("CLI_INPUT_SEQUENCE_TOO_LONG")]
    SequenceTooLong,
    #[error("CLI_INPUT_CONTROL_BYTE")]
    ControlByte,
    #[error("CLI_INPUT_PASTE_TOO_LARGE")]
    PasteTooLarge,
    #[error("CLI_INPUT_CAPACITY")]
    Capacity,
    #[error("CLI_INPUT_EPOCH_EXHAUSTED")]
    EpochExhausted,
}

pub(crate) struct KeyDecoder {
    epoch: u64,
    state: DecodeState,
    utf8: [u8; 4],
    utf8_len: usize,
}

impl fmt::Debug for KeyDecoder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyDecoder")
            .field("epoch", &self.epoch)
            .field("state", &self.state.kind())
            .field("utf8_pending_bytes", &self.utf8_len)
            .finish()
    }
}

enum DecodeState {
    Ground,
    Escape,
    Csi(SmallSequence),
    CsiDrain,
    Ss3,
    Paste(PasteState),
}

impl DecodeState {
    fn kind(&self) -> &'static str {
        match self {
            Self::Ground => "ground",
            Self::Escape => "escape",
            Self::Csi(_) => "csi",
            Self::CsiDrain => "csi-drain",
            Self::Ss3 => "ss3",
            Self::Paste(_) => "paste",
        }
    }
}

struct SmallSequence {
    bytes: [u8; MAX_CSI_BYTES],
    len: usize,
}

impl SmallSequence {
    fn new() -> Self {
        Self {
            bytes: [0; MAX_CSI_BYTES],
            len: 0,
        }
    }

    fn push(&mut self, byte: u8) -> bool {
        if self.len == self.bytes.len() {
            return false;
        }
        self.bytes[self.len] = byte;
        self.len += 1;
        true
    }

    fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

struct PasteState {
    bytes: Vec<u8>,
    seen_bytes: usize,
    end_match: usize,
    oversized: bool,
    allocation_failed: bool,
}

impl PasteState {
    fn new() -> Self {
        let mut bytes = Vec::new();
        let allocation_failed = bytes.try_reserve(4 * 1024).is_err();
        Self {
            bytes,
            seen_bytes: 0,
            end_match: 0,
            oversized: false,
            allocation_failed,
        }
    }

    fn push_content(&mut self, byte: u8) {
        self.seen_bytes = self.seen_bytes.saturating_add(1);
        if self.seen_bytes > MAX_PASTE_BYTES {
            self.oversized = true;
            return;
        }
        if !self.allocation_failed {
            if self.bytes.len() == self.bytes.capacity()
                && self
                    .bytes
                    .try_reserve((MAX_PASTE_BYTES - self.bytes.len()).min(4 * 1024))
                    .is_err()
            {
                self.allocation_failed = true;
                self.bytes.clear();
            } else {
                self.bytes.push(byte);
            }
        }
    }

    fn feed(&mut self, byte: u8) -> bool {
        if byte == PASTE_END[self.end_match] {
            self.end_match += 1;
            return self.end_match == PASTE_END.len();
        }

        for matched in PASTE_END.iter().take(self.end_match) {
            self.push_content(*matched);
        }
        self.end_match = 0;
        if byte == PASTE_END[0] {
            self.end_match = 1;
        } else {
            self.push_content(byte);
        }
        false
    }

    fn finish(self) -> InputEvent {
        if self.allocation_failed {
            return InputEvent::PasteRejected(InputError::Capacity);
        }
        if self.oversized {
            return InputEvent::PasteRejected(InputError::PasteTooLarge);
        }
        match String::from_utf8(self.bytes) {
            Ok(text) => InputEvent::Paste(text),
            Err(_) => InputEvent::PasteRejected(InputError::InvalidUtf8),
        }
    }
}

impl Default for KeyDecoder {
    fn default() -> Self {
        Self {
            epoch: 0,
            state: DecodeState::Ground,
            utf8: [0; 4],
            utf8_len: 0,
        }
    }
}

impl KeyDecoder {
    pub(crate) fn epoch(&self) -> InputEpoch {
        InputEpoch(self.epoch)
    }

    pub(crate) fn reset_epoch(&mut self) -> Result<InputEpoch, InputError> {
        let next = self
            .epoch
            .checked_add(1)
            .ok_or(InputError::EpochExhausted)?;
        self.epoch = next;
        self.reset_transient();
        Ok(InputEpoch(next))
    }

    pub(crate) fn escape_pending(&self) -> bool {
        matches!(
            self.state,
            DecodeState::Escape | DecodeState::Csi(_) | DecodeState::CsiDrain | DecodeState::Ss3
        )
    }

    pub(crate) fn expire_escape(&mut self) -> Option<DecodedInput> {
        let state = std::mem::replace(&mut self.state, DecodeState::Ground);
        let event = match state {
            DecodeState::Escape => InputEvent::Key(Key::Escape),
            DecodeState::Csi(_) | DecodeState::Ss3 => {
                InputEvent::Rejected(InputError::IncompleteSequence)
            }
            DecodeState::CsiDrain => return None,
            other => {
                self.state = other;
                return None;
            }
        };
        Some(DecodedInput {
            epoch: InputEpoch(self.epoch),
            event,
        })
    }

    pub(crate) fn feed(
        &mut self,
        bytes: &[u8],
        mut emit: impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        for byte in bytes.iter().copied() {
            let state = std::mem::replace(&mut self.state, DecodeState::Ground);
            let flow = match state {
                DecodeState::Ground => self.feed_ground(byte, &mut emit),
                DecodeState::Escape => self.feed_escape(byte, &mut emit),
                DecodeState::Csi(sequence) => self.feed_csi(sequence, byte, &mut emit),
                DecodeState::CsiDrain => {
                    if is_csi_final(byte) {
                        self.state = DecodeState::Ground;
                        self.emit(InputEvent::Rejected(InputError::SequenceTooLong), &mut emit)
                    } else {
                        self.state = DecodeState::CsiDrain;
                        ControlFlow::Continue(())
                    }
                }
                DecodeState::Ss3 => self.feed_ss3(byte, &mut emit),
                DecodeState::Paste(mut paste) => {
                    if paste.feed(byte) {
                        self.state = DecodeState::Ground;
                        self.emit(paste.finish(), &mut emit)
                    } else {
                        self.state = DecodeState::Paste(paste);
                        ControlFlow::Continue(())
                    }
                }
            };
            if flow.is_break() {
                return flow;
            }
        }
        ControlFlow::Continue(())
    }

    fn feed_ground(
        &mut self,
        byte: u8,
        emit: &mut impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if self.utf8_len != 0 || byte >= 0x80 {
            return self.feed_utf8(byte, emit);
        }
        self.feed_ascii(byte, emit)
    }

    fn feed_utf8(
        &mut self,
        byte: u8,
        emit: &mut impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if self.utf8_len != 0 && byte.is_ascii() {
            self.utf8_len = 0;
            return self.emit(InputEvent::Rejected(InputError::InvalidUtf8), emit);
        }
        if self.utf8_len == self.utf8.len() {
            self.utf8_len = 0;
            return self.emit(InputEvent::Rejected(InputError::InvalidUtf8), emit);
        }
        self.utf8[self.utf8_len] = byte;
        self.utf8_len += 1;
        match std::str::from_utf8(&self.utf8[..self.utf8_len]) {
            Ok(text) => {
                let character = text.chars().next();
                self.utf8_len = 0;
                match character {
                    Some(character) => self.emit(InputEvent::Key(Key::Char(character)), emit),
                    None => ControlFlow::Continue(()),
                }
            }
            Err(error) if error.error_len().is_some() || self.utf8_len == self.utf8.len() => {
                self.utf8_len = 0;
                self.emit(InputEvent::Rejected(InputError::InvalidUtf8), emit)
            }
            Err(_) => ControlFlow::Continue(()),
        }
    }

    fn feed_ascii(
        &mut self,
        byte: u8,
        emit: &mut impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        let event = match byte {
            0x1b => {
                self.state = DecodeState::Escape;
                return ControlFlow::Continue(());
            }
            b'\r' => InputEvent::Key(Key::Enter),
            b'\n' => InputEvent::Key(Key::Newline),
            b'\t' => InputEvent::Key(Key::Tab),
            0x01 => InputEvent::Key(Key::Home),
            0x04 => InputEvent::Key(Key::Eof),
            0x05 => InputEvent::Key(Key::End),
            0x08 | 0x7f => InputEvent::Key(Key::Backspace),
            0x0b => InputEvent::Key(Key::ClearAfter),
            0x12 => InputEvent::Key(Key::ReverseSearch),
            0x15 => InputEvent::Key(Key::ClearBefore),
            0x17 => InputEvent::Key(Key::WordErase),
            0x19 => InputEvent::Key(Key::Yank),
            0x1f => InputEvent::Key(Key::Undo),
            0x20..=0x7e => InputEvent::Key(Key::Char(char::from(byte))),
            _ => InputEvent::Rejected(InputError::ControlByte),
        };
        self.emit(event, emit)
    }

    fn feed_escape(
        &mut self,
        byte: u8,
        emit: &mut impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if byte == b'[' {
            self.state = DecodeState::Csi(SmallSequence::new());
            return ControlFlow::Continue(());
        }
        if byte == b'O' {
            self.state = DecodeState::Ss3;
            return ControlFlow::Continue(());
        }
        self.state = DecodeState::Ground;
        if self.emit(InputEvent::Key(Key::Escape), emit).is_break() {
            return ControlFlow::Break(());
        }
        self.feed_ground(byte, emit)
    }

    fn feed_csi(
        &mut self,
        mut sequence: SmallSequence,
        byte: u8,
        emit: &mut impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        if byte == 0x1b {
            self.state = DecodeState::Escape;
            return self.emit(InputEvent::Rejected(InputError::UnknownSequence), emit);
        }
        if !sequence.push(byte) {
            self.state = if is_csi_final(byte) {
                DecodeState::Ground
            } else {
                DecodeState::CsiDrain
            };
            return self.emit(InputEvent::Rejected(InputError::SequenceTooLong), emit);
        }
        if !is_csi_final(byte) {
            if !(0x20..=0x3f).contains(&byte) {
                self.state = DecodeState::Ground;
                return self.emit(InputEvent::Rejected(InputError::UnknownSequence), emit);
            }
            self.state = DecodeState::Csi(sequence);
            return ControlFlow::Continue(());
        }

        self.state = DecodeState::Ground;
        match sequence.as_slice() {
            b"A" => self.emit(InputEvent::Key(Key::Up), emit),
            b"B" => self.emit(InputEvent::Key(Key::Down), emit),
            b"C" => self.emit(InputEvent::Key(Key::Right), emit),
            b"D" => self.emit(InputEvent::Key(Key::Left), emit),
            b"H" | b"1~" => self.emit(InputEvent::Key(Key::Home), emit),
            b"F" | b"4~" => self.emit(InputEvent::Key(Key::End), emit),
            b"3~" => self.emit(InputEvent::Key(Key::Delete), emit),
            b"Z" => self.emit(InputEvent::Key(Key::BackTab), emit),
            b"13;2u" => self.emit(InputEvent::Key(Key::Newline), emit),
            b"200~" => {
                self.state = DecodeState::Paste(PasteState::new());
                self.emit(InputEvent::PasteStarted, emit)
            }
            _ => self.emit(InputEvent::Rejected(InputError::UnknownSequence), emit),
        }
    }

    fn feed_ss3(
        &mut self,
        byte: u8,
        emit: &mut impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        self.state = DecodeState::Ground;
        match byte {
            b'A' => self.emit(InputEvent::Key(Key::Up), emit),
            b'B' => self.emit(InputEvent::Key(Key::Down), emit),
            b'C' => self.emit(InputEvent::Key(Key::Right), emit),
            b'D' => self.emit(InputEvent::Key(Key::Left), emit),
            b'H' => self.emit(InputEvent::Key(Key::Home), emit),
            b'F' => self.emit(InputEvent::Key(Key::End), emit),
            _ => self.emit(InputEvent::Rejected(InputError::UnknownSequence), emit),
        }
    }

    fn emit(
        &self,
        event: InputEvent,
        emit: &mut impl FnMut(DecodedInput) -> ControlFlow<()>,
    ) -> ControlFlow<()> {
        emit(DecodedInput {
            epoch: InputEpoch(self.epoch),
            event,
        })
    }

    fn reset_transient(&mut self) {
        self.state = DecodeState::Ground;
        self.utf8_len = 0;
    }
}

fn is_csi_final(byte: u8) -> bool {
    (0x40..=0x7e).contains(&byte)
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use super::{InputError, InputEvent, Key, KeyDecoder, MAX_PASTE_BYTES};

    fn collect(decoder: &mut KeyDecoder, bytes: &[u8]) -> Vec<InputEvent> {
        let mut events = Vec::new();
        assert!(matches!(
            decoder.feed(bytes, |decoded| {
                events.push(decoded.event);
                ControlFlow::Continue(())
            }),
            ControlFlow::Continue(())
        ));
        events
    }

    #[test]
    fn decoder_reassembles_every_utf8_and_csi_fragmentation() {
        let family = "👨‍👩‍👧‍👦";
        for split in 0..=family.len() {
            let mut decoder = KeyDecoder::default();
            let mut events = collect(&mut decoder, &family.as_bytes()[..split]);
            events.extend(collect(&mut decoder, &family.as_bytes()[split..]));
            assert_eq!(
                events,
                family
                    .chars()
                    .map(|character| InputEvent::Key(Key::Char(character)))
                    .collect::<Vec<_>>()
            );
        }

        for split in 0..=3 {
            let mut decoder = KeyDecoder::default();
            let mut events = collect(&mut decoder, &b"\x1b[A"[..split]);
            events.extend(collect(&mut decoder, &b"\x1b[A"[split..]));
            assert_eq!(events, [InputEvent::Key(Key::Up)]);
        }
    }

    #[test]
    fn enter_newline_and_isolated_escape_remain_distinct() {
        let mut decoder = KeyDecoder::default();
        assert_eq!(
            collect(&mut decoder, b"\r\n"),
            [InputEvent::Key(Key::Enter), InputEvent::Key(Key::Newline)]
        );
        assert!(collect(&mut decoder, b"\x1b").is_empty());
        assert!(decoder.escape_pending());
        assert_eq!(
            decoder.expire_escape().unwrap().event,
            InputEvent::Key(Key::Escape)
        );
    }

    #[test]
    fn bracketed_paste_is_one_atomic_event_and_never_emits_enter() {
        let wire = b"\x1b[200~first\r\nsecond\x1b[201~";
        for split in 0..=wire.len() {
            let mut decoder = KeyDecoder::default();
            let mut events = collect(&mut decoder, &wire[..split]);
            events.extend(collect(&mut decoder, &wire[split..]));
            assert_eq!(
                events,
                [
                    InputEvent::PasteStarted,
                    InputEvent::Paste("first\r\nsecond".to_owned()),
                ]
            );
        }
    }

    #[test]
    fn paste_exact_limit_and_one_over_recover_at_the_end_marker() {
        let mut exact = Vec::new();
        exact.extend_from_slice(b"\x1b[200~");
        exact.extend(std::iter::repeat_n(b'x', MAX_PASTE_BYTES));
        exact.extend_from_slice(b"\x1b[201~");
        let mut decoder = KeyDecoder::default();
        let events = collect(&mut decoder, &exact);
        assert!(
            matches!(&events[..], [InputEvent::PasteStarted, InputEvent::Paste(text)] if text.len() == MAX_PASTE_BYTES)
        );

        let mut over = Vec::new();
        over.extend_from_slice(b"\x1b[200~");
        over.extend(std::iter::repeat_n(b'x', MAX_PASTE_BYTES + 1));
        over.extend_from_slice(b"\x1b[201~z");
        let mut decoder = KeyDecoder::default();
        assert_eq!(
            collect(&mut decoder, &over),
            [
                InputEvent::PasteStarted,
                InputEvent::PasteRejected(InputError::PasteTooLarge),
                InputEvent::Key(Key::Char('z')),
            ]
        );
    }

    #[test]
    fn invalid_utf8_unknown_csi_and_paste_never_become_confirmation() {
        let mut decoder = KeyDecoder::default();
        let mut events = collect(&mut decoder, &[0xf0, b'\r']);
        events.extend(collect(&mut decoder, b"\x1b[999~"));
        events.extend(collect(&mut decoder, b"\x1b[200~y\r\x1b[201~"));
        assert_eq!(
            events,
            [
                InputEvent::Rejected(InputError::InvalidUtf8),
                InputEvent::Rejected(InputError::UnknownSequence),
                InputEvent::PasteStarted,
                InputEvent::Paste("y\r".to_owned()),
            ]
        );

        let mut decoder = KeyDecoder::default();
        assert_eq!(
            collect(&mut decoder, b"\x1b[200~\xff\x1b[201~"),
            [
                InputEvent::PasteStarted,
                InputEvent::PasteRejected(InputError::InvalidUtf8),
            ]
        );
    }

    #[test]
    fn breaking_the_consumer_discards_the_rest_of_the_same_read() {
        let mut decoder = KeyDecoder::default();
        let mut events = Vec::new();
        assert!(matches!(
            decoder.feed(b"a\rb", |decoded| {
                events.push(decoded.event);
                if events.len() == 2 {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }),
            ControlFlow::Break(())
        ));
        assert_eq!(
            events,
            [InputEvent::Key(Key::Char('a')), InputEvent::Key(Key::Enter)]
        );
        assert_eq!(
            collect(&mut decoder, b"c"),
            [InputEvent::Key(Key::Char('c'))]
        );
    }

    #[test]
    fn a_completed_paste_can_discard_a_same_read_submit_suffix() {
        let mut decoder = KeyDecoder::default();
        let mut events = Vec::new();
        assert!(
            decoder
                .feed(b"\x1b[200~safe\x1b[201~\r\x1b[201~", |decoded| {
                    let completed = matches!(decoded.event, InputEvent::Paste(_));
                    events.push(decoded.event);
                    if completed {
                        ControlFlow::Break(())
                    } else {
                        ControlFlow::Continue(())
                    }
                })
                .is_break()
        );
        assert_eq!(
            events,
            [
                InputEvent::PasteStarted,
                InputEvent::Paste("safe".to_owned())
            ]
        );
        assert_eq!(
            collect(&mut decoder, b"z"),
            [InputEvent::Key(Key::Char('z'))]
        );
    }

    #[test]
    fn breaking_on_an_overlong_csi_keeps_draining_until_its_final_byte() {
        let mut wire = Vec::from(b"\x1b[".as_slice());
        wire.extend(std::iter::repeat_n(b'1', super::MAX_CSI_BYTES + 1));
        let mut decoder = KeyDecoder::default();
        let mut first = Vec::new();
        assert!(
            decoder
                .feed(&wire, |decoded| {
                    first.push(decoded.event);
                    ControlFlow::Break(())
                })
                .is_break()
        );
        assert_eq!(first, [InputEvent::Rejected(InputError::SequenceTooLong)]);

        assert_eq!(
            collect(&mut decoder, b"12~"),
            [InputEvent::Rejected(InputError::SequenceTooLong)]
        );
        assert_eq!(
            collect(&mut decoder, b"x"),
            [InputEvent::Key(Key::Char('x'))]
        );
    }

    #[test]
    fn epoch_reset_discards_partial_utf8_escape_and_paste_state() {
        let mut decoder = KeyDecoder::default();
        assert!(collect(&mut decoder, b"\xf0\x9f").is_empty());
        let epoch = decoder.reset_epoch().unwrap();
        assert_eq!(decoder.epoch(), epoch);
        assert_eq!(
            collect(&mut decoder, b"x"),
            [InputEvent::Key(Key::Char('x'))]
        );

        assert_eq!(
            collect(&mut decoder, b"\x1b[200~stale"),
            [InputEvent::PasteStarted]
        );
        decoder.reset_epoch().unwrap();
        assert_eq!(
            collect(&mut decoder, b"y"),
            [InputEvent::Key(Key::Char('y'))]
        );
    }

    #[test]
    fn ss3_arrows_and_escape_followed_by_text_are_unambiguous() {
        let mut decoder = KeyDecoder::default();
        assert_eq!(
            collect(&mut decoder, b"\x1bOA\x1ba"),
            [
                InputEvent::Key(Key::Up),
                InputEvent::Key(Key::Escape),
                InputEvent::Key(Key::Char('a')),
            ]
        );
    }

    #[test]
    fn debug_views_do_not_expose_typed_or_pasted_input() {
        let secret = InputEvent::Paste("SECRET_DRAFT".to_owned());
        assert!(!format!("{secret:?}").contains("SECRET_DRAFT"));
        assert_eq!(format!("{:?}", Key::Char('S')), "Char");
    }
}
