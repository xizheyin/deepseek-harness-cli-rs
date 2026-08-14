pub(super) const MAX_INTERACTIVE_PROMPT_BYTES: usize = 1_000;
pub(super) const MAX_APPROVAL_RECORD_BYTES: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum InputRecordEvent {
    Record {
        text: String,
        terminated_by_lf: bool,
    },
    TooLarge,
    InvalidUtf8,
}

pub(super) struct CanonicalRecordParser {
    buffer: [u8; MAX_INTERACTIVE_PROMPT_BYTES],
    length: usize,
    limit: usize,
    draining_oversized: bool,
}

impl CanonicalRecordParser {
    pub(super) fn new(limit: usize) -> Self {
        debug_assert!((1..=MAX_INTERACTIVE_PROMPT_BYTES).contains(&limit));
        Self {
            buffer: [0; MAX_INTERACTIVE_PROMPT_BYTES],
            length: 0,
            limit: limit.clamp(1, MAX_INTERACTIVE_PROMPT_BYTES),
            draining_oversized: false,
        }
    }

    pub(super) fn reset(&mut self, limit: usize) {
        debug_assert!((1..=MAX_INTERACTIVE_PROMPT_BYTES).contains(&limit));
        self.length = 0;
        self.limit = limit.clamp(1, MAX_INTERACTIVE_PROMPT_BYTES);
        self.draining_oversized = false;
    }

    pub(super) fn feed(
        &mut self,
        bytes: &[u8],
        canonical_boundary_at_end: bool,
        mut emit: impl FnMut(InputRecordEvent),
    ) {
        for &byte in bytes {
            if self.draining_oversized {
                if byte == b'\n' {
                    self.draining_oversized = false;
                }
                continue;
            }
            if byte == b'\n' {
                self.emit_record(true, &mut emit);
                continue;
            }
            if self.length == self.limit {
                self.length = 0;
                self.draining_oversized = true;
                emit(InputRecordEvent::TooLarge);
                continue;
            }
            self.buffer[self.length] = byte;
            self.length += 1;
        }

        if canonical_boundary_at_end {
            if self.draining_oversized {
                self.draining_oversized = false;
            } else if self.length != 0 {
                self.emit_record(false, &mut emit);
            }
        }
    }

    fn emit_record(&mut self, terminated_by_lf: bool, emit: &mut impl FnMut(InputRecordEvent)) {
        let event = match std::str::from_utf8(&self.buffer[..self.length]) {
            Ok(text) => InputRecordEvent::Record {
                text: text.to_owned(),
                terminated_by_lf,
            },
            Err(_) => InputRecordEvent::InvalidUtf8,
        };
        self.length = 0;
        emit(event);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum IdleInput {
    Redraw,
    Help,
    Exit,
    Submit(String),
}

pub(super) fn classify_idle_record(record: &str, _terminated_by_lf: bool) -> IdleInput {
    let command = record.trim_matches(|character: char| character.is_ascii_whitespace());
    match command {
        "" => IdleInput::Redraw,
        "/help" => IdleInput::Help,
        "/exit" | "/quit" => IdleInput::Exit,
        _ => IdleInput::Submit(record.to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{CanonicalRecordParser, IdleInput, InputRecordEvent, classify_idle_record};

    fn feed(
        parser: &mut CanonicalRecordParser,
        bytes: &[u8],
        boundary: bool,
    ) -> Vec<InputRecordEvent> {
        let mut events = Vec::new();
        parser.feed(bytes, boundary, |event| events.push(event));
        events
    }

    #[test]
    fn exact_limit_is_accepted_and_one_byte_over_is_drained() {
        let mut parser = CanonicalRecordParser::new(1_000);
        let mut exact = vec![b'x'; 1_000];
        exact.push(b'\n');
        assert_eq!(
            feed(&mut parser, &exact, true),
            vec![InputRecordEvent::Record {
                text: "x".repeat(1_000),
                terminated_by_lf: true,
            }]
        );

        let mut over = vec![b'x'; 1_001];
        over.push(b'\n');
        assert_eq!(
            feed(&mut parser, &over, true),
            vec![InputRecordEvent::TooLarge]
        );
        assert_eq!(
            feed(&mut parser, b"next\n", true),
            vec![InputRecordEvent::Record {
                text: "next".to_owned(),
                terminated_by_lf: true,
            }]
        );
    }

    #[test]
    fn utf8_limits_are_bytes_and_invalid_records_are_rejected_without_echoing() {
        let mut parser = CanonicalRecordParser::new(1_000);
        let exact = format!("{}\n", "é".repeat(500));
        assert!(matches!(
            feed(&mut parser, exact.as_bytes(), true).as_slice(),
            [InputRecordEvent::Record { text, .. }] if text.len() == 1_000
        ));
        let over = format!("{}\n", "é".repeat(501));
        assert_eq!(
            feed(&mut parser, over.as_bytes(), true),
            vec![InputRecordEvent::TooLarge]
        );
        assert_eq!(
            feed(&mut parser, &[0xff, b'\n'], true),
            vec![InputRecordEvent::InvalidUtf8]
        );
    }

    #[test]
    fn an_oversized_record_drains_to_its_boundary_before_accepting_another() {
        let mut parser = CanonicalRecordParser::new(4);
        assert_eq!(
            feed(&mut parser, b"abcde", false),
            vec![InputRecordEvent::TooLarge]
        );
        assert!(feed(&mut parser, b"still discarded", false).is_empty());
        assert_eq!(
            feed(&mut parser, b"\nok\n", true),
            vec![InputRecordEvent::Record {
                text: "ok".to_owned(),
                terminated_by_lf: true,
            }]
        );
    }

    #[test]
    fn multiple_lines_and_a_non_lf_veof_record_keep_their_boundaries() {
        let mut parser = CanonicalRecordParser::new(16);
        assert_eq!(
            feed(&mut parser, b"one\n\ntwo", true),
            vec![
                InputRecordEvent::Record {
                    text: "one".to_owned(),
                    terminated_by_lf: true,
                },
                InputRecordEvent::Record {
                    text: String::new(),
                    terminated_by_lf: true,
                },
                InputRecordEvent::Record {
                    text: "two".to_owned(),
                    terminated_by_lf: false,
                },
            ]
        );
    }

    #[test]
    fn resetting_an_input_fence_discards_partial_and_oversized_state() {
        let mut parser = CanonicalRecordParser::new(4);
        assert!(feed(&mut parser, b"ab", false).is_empty());
        parser.reset(8);
        assert_eq!(
            feed(&mut parser, b"fresh\n", true),
            vec![InputRecordEvent::Record {
                text: "fresh".to_owned(),
                terminated_by_lf: true,
            }]
        );

        assert_eq!(
            feed(&mut parser, b"123456789", false),
            vec![InputRecordEvent::TooLarge]
        );
        parser.reset(4);
        assert_eq!(
            feed(&mut parser, b"ok\n", true),
            vec![InputRecordEvent::Record {
                text: "ok".to_owned(),
                terminated_by_lf: true,
            }]
        );
    }

    #[test]
    fn idle_classification_trims_only_for_commands_and_preserves_prompt_bytes() {
        assert_eq!(classify_idle_record("  \t", true), IdleInput::Redraw);
        assert_eq!(classify_idle_record("  /help \t", true), IdleInput::Help);
        assert_eq!(classify_idle_record(" /exit ", false), IdleInput::Exit);
        assert_eq!(classify_idle_record("\t/quit", true), IdleInput::Exit);
        assert_eq!(
            classify_idle_record("  ordinary prompt  ", false),
            IdleInput::Submit("  ordinary prompt  ".to_owned())
        );
        assert_eq!(
            classify_idle_record("/unknown", true),
            IdleInput::Submit("/unknown".to_owned())
        );
    }
}
