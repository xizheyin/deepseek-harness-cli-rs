use std::fmt::Write as _;

pub(super) const VISIBLE_SCRATCH_BYTES: usize = 8 * 1024;
const MAX_VISIBLE_ESCAPE_BYTES: usize = 10;

// Unicode 16.0 General_Category=Cf, frozen because Rust 1.85 ships that table.
// Keeping the data here makes the terminal-safety boundary reviewable without
// adding a large Unicode classification dependency.
const UNICODE_16_FORMAT_RANGES: &[(u32, u32)] = &[
    (0x00AD, 0x00AD),
    (0x0600, 0x0605),
    (0x061C, 0x061C),
    (0x06DD, 0x06DD),
    (0x070F, 0x070F),
    (0x0890, 0x0891),
    (0x08E2, 0x08E2),
    (0x180E, 0x180E),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x2064),
    (0x2066, 0x206F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x110BD, 0x110BD),
    (0x110CD, 0x110CD),
    (0x13430, 0x1343F),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0001, 0xE0001),
    (0xE0020, 0xE007F),
];

/// Converts untrusted text into bytes that a terminal cannot interpret as
/// cursor movement, clipboard access, or bidi formatting.
///
/// The caller owns output framing and backpressure. This type keeps only one
/// fixed-size scratch buffer and reports each ready chunk synchronously.
pub(super) struct VisibleRenderer {
    scratch: String,
    at_line_start: bool,
}

impl VisibleRenderer {
    pub(super) fn new() -> Self {
        Self {
            scratch: String::with_capacity(VISIBLE_SCRATCH_BYTES),
            at_line_start: true,
        }
    }

    pub(super) fn render_fragment<E>(
        &mut self,
        input: &str,
        trusted_line_prefix: Option<&'static str>,
        emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.render_fragment_with_line_policy(input, trusted_line_prefix, false, emit)
    }

    /// Render one untrusted table field without allowing it to create another
    /// physical output row. Unix paths may legally contain LF, so session-list
    /// output needs a stricter boundary than ordinary conversational text.
    pub(super) fn render_single_line_fragment<E>(
        &mut self,
        input: &str,
        emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.render_fragment_with_line_policy(input, None, true, emit)
    }

    fn render_fragment_with_line_policy<E>(
        &mut self,
        input: &str,
        trusted_line_prefix: Option<&'static str>,
        escape_line_feed: bool,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        for character in input.chars() {
            if self.at_line_start {
                if let Some(prefix) = trusted_line_prefix {
                    self.append_piece(prefix, &mut emit)?;
                }
                self.at_line_start = false;
            }

            match character {
                '\n' if !escape_line_feed => {
                    self.append_piece("\n", &mut emit)?;
                    self.at_line_start = true;
                }
                '\n' => self.append_piece("\\n", &mut emit)?,
                '\t' => self.append_piece("\\t", &mut emit)?,
                '\r' => self.append_piece("\\r", &mut emit)?,
                character if must_escape(character) => {
                    self.ensure_room(MAX_VISIBLE_ESCAPE_BYTES, &mut emit)?;
                    // Writing to String is infallible. Capacity was reserved
                    // before this bounded (at most ten-byte) escape.
                    write!(&mut self.scratch, "\\u{{{:x}}}", u32::from(character))
                        .expect("writing to a String cannot fail");
                }
                character => {
                    let mut encoded = [0_u8; 4];
                    self.append_piece(character.encode_utf8(&mut encoded), &mut emit)?;
                }
            }
        }
        self.flush(&mut emit)
    }

    pub(super) fn ensure_line_start<E>(
        &mut self,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        if !self.at_line_start {
            self.append_piece("\n", &mut emit)?;
            self.at_line_start = true;
        }
        self.flush(&mut emit)
    }

    pub(super) fn render_trusted<E>(
        &mut self,
        trusted: &'static str,
        mut emit: impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.flush(&mut emit)?;
        if !trusted.is_empty() {
            emit(trusted)?;
            self.at_line_start = trusted.ends_with('\n');
        }
        Ok(())
    }

    pub(super) fn is_at_line_start(&self) -> bool {
        self.at_line_start
    }

    pub(super) fn force_line_boundary_on_next_output(&mut self) {
        self.scratch.clear();
        self.at_line_start = false;
    }

    fn append_piece<E>(
        &mut self,
        piece: &str,
        emit: &mut impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        self.ensure_room(piece.len(), emit)?;
        self.scratch.push_str(piece);
        Ok(())
    }

    fn ensure_room<E>(
        &mut self,
        needed: usize,
        emit: &mut impl FnMut(&str) -> Result<(), E>,
    ) -> Result<(), E> {
        if self.scratch.len().saturating_add(needed) > VISIBLE_SCRATCH_BYTES {
            self.flush(emit)?;
        }
        // All pieces are a Unicode scalar, a fixed escape, or a trusted role
        // prefix. Product-owned prefixes are deliberately much smaller than
        // the scratch buffer.
        debug_assert!(needed <= VISIBLE_SCRATCH_BYTES);
        Ok(())
    }

    fn flush<E>(&mut self, emit: &mut impl FnMut(&str) -> Result<(), E>) -> Result<(), E> {
        if !self.scratch.is_empty() {
            emit(&self.scratch)?;
            self.scratch.clear();
        }
        Ok(())
    }
}

fn must_escape(character: char) -> bool {
    let value = u32::from(character);
    UNICODE_16_FORMAT_RANGES
        .iter()
        .any(|(start, end)| (*start..=*end).contains(&value))
        || matches!(
            value,
            0x0000..=0x001F
                | 0x007F..=0x009F
                | 0x034F
                | 0x115F..=0x1160
                | 0x17B4..=0x17B5
                | 0x180B..=0x180F
                | 0x2028..=0x2029
                | 0x2065
                | 0x3164
                | 0xFE00..=0xFE0F
                | 0xFFA0
                | 0x13440..=0x13455
                | 0xE0100..=0xE01EF
        )
}

#[cfg(test)]
fn render_for_test(input: &str, prefix: Option<&'static str>) -> String {
    let mut rendered = String::new();
    VisibleRenderer::new()
        .render_fragment(input, prefix, |chunk| {
            rendered.push_str(chunk);
            Ok::<_, std::convert::Infallible>(())
        })
        .expect("the test sink is infallible");
    rendered
}

#[cfg(test)]
mod tests {
    use super::{UNICODE_16_FORMAT_RANGES, VISIBLE_SCRATCH_BYTES, must_escape, render_for_test};

    #[test]
    fn sanitizer_table_is_frozen_to_rust_1_85_unicode_16() {
        assert_eq!(char::UNICODE_VERSION, (16, 0, 0));
        for &(start, end) in UNICODE_16_FORMAT_RANGES {
            for value in start..=end {
                let character = char::from_u32(value).expect("the frozen range contains scalars");
                assert!(must_escape(character), "U+{value:04X} was not escaped");
            }
        }
    }

    #[test]
    fn visible_renderer_preserves_printable_unicode_and_line_feeds() {
        assert_eq!(
            render_for_test("plain 中文 🦀\nnext", None),
            "plain 中文 🦀\nnext"
        );
    }

    #[test]
    fn visible_renderer_escapes_terminal_controls_and_bidi_formatting() {
        let hostile = concat!(
            "a\t\r\x1b]52;c;secret\x07\u{0085}",
            "\u{034f}\u{061c}\u{200e}\u{200f}",
            "\u{202a}\u{202e}\u{2066}\u{2069}",
            "\u{2028}\u{2029}\u{feff}\u{e0001}\u{e0020}z"
        );
        let rendered = render_for_test(hostile, None);

        assert!(!rendered.contains('\x1b'));
        assert!(!rendered.contains('\x07'));
        assert_eq!(
            rendered,
            concat!(
                "a\\t\\r\\u{1b}]52;c;secret\\u{7}\\u{85}",
                "\\u{34f}\\u{61c}\\u{200e}\\u{200f}",
                "\\u{202a}\\u{202e}\\u{2066}\\u{2069}",
                "\\u{2028}\\u{2029}\\u{feff}\\u{e0001}\\u{e0020}z"
            )
        );
    }

    #[test]
    fn interactive_roles_are_repeated_after_each_preserved_line_feed() {
        assert_eq!(
            render_for_test("first\nsecond\n", Some("assistant | ")),
            "assistant | first\nassistant | second\n"
        );
        assert_eq!(render_for_test("", Some("assistant | ")), "");
    }

    #[test]
    fn scratch_boundary_never_changes_visible_output() {
        let input = format!(
            "{}\u{202e}{}",
            "x".repeat(VISIBLE_SCRATCH_BYTES - 1),
            "🦀".repeat(8)
        );
        let rendered = render_for_test(&input, None);
        assert_eq!(
            rendered,
            format!(
                "{}\\u{{202e}}{}",
                "x".repeat(VISIBLE_SCRATCH_BYTES - 1),
                "🦀".repeat(8)
            )
        );
    }

    #[test]
    fn table_boundaries_escape_only_the_tabled_code_points() {
        let input = "\u{05ff}\u{0600}\u{0605}\u{0606}\u{180d}\u{180e}\u{180f}\u{1810}\u{fe0f}\u{fe10}\u{e007f}\u{e0080}";
        assert_eq!(
            render_for_test(input, None),
            "\u{05ff}\\u{600}\\u{605}\u{0606}\\u{180d}\\u{180e}\\u{180f}\u{1810}\\u{fe0f}\u{fe10}\\u{e007f}\u{e0080}"
        );
    }
}
