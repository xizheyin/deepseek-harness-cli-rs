use std::fmt::Write as _;

use crate::tui::visible::VisibleChar;

pub(super) const VISIBLE_SCRATCH_BYTES: usize = 8 * 1024;
const MAX_VISIBLE_ESCAPE_BYTES: usize = 10;

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

            match VisibleChar::classify(character, escape_line_feed) {
                VisibleChar::LineFeed => {
                    self.append_piece("\n", &mut emit)?;
                    self.at_line_start = true;
                }
                VisibleChar::ShortEscape(escape) => self.append_piece(escape, &mut emit)?,
                VisibleChar::UnicodeEscape(value) => {
                    self.ensure_room(MAX_VISIBLE_ESCAPE_BYTES, &mut emit)?;
                    // Writing to String is infallible. Capacity was reserved
                    // before this bounded (at most ten-byte) escape.
                    write!(&mut self.scratch, "\\u{{{value:x}}}")
                        .expect("writing to a String cannot fail");
                }
                VisibleChar::Printable(character) => {
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

    pub(super) fn force_line_start(&mut self, at_line_start: bool) {
        self.scratch.clear();
        self.at_line_start = at_line_start;
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
    use super::{VISIBLE_SCRATCH_BYTES, render_for_test};

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
