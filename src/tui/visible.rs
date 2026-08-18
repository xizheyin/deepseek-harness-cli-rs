use std::fmt::Write as _;

use thiserror::Error;

const MAX_VISIBLE_OWNED_BYTES: usize = 1024 * 1024;

// Unicode 16.0 General_Category=Cf, frozen because Rust 1.85 ships that table.
// Keeping the data here makes the terminal-safety boundary reviewable without
// adding a large Unicode classification dependency.
pub(crate) const UNICODE_16_FORMAT_RANGES: &[(u32, u32)] = &[
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VisibleChar {
    LineFeed,
    ShortEscape(&'static str),
    UnicodeEscape(u32),
    Printable(char),
}

impl VisibleChar {
    pub(crate) fn classify(character: char, escape_line_feed: bool) -> Self {
        match character {
            '\n' if !escape_line_feed => Self::LineFeed,
            '\n' => Self::ShortEscape("\\n"),
            '\t' => Self::ShortEscape("\\t"),
            '\r' => Self::ShortEscape("\\r"),
            character if must_escape(character) => Self::UnicodeEscape(u32::from(character)),
            character => Self::Printable(character),
        }
    }

    pub(crate) fn escaped_cell_width(self) -> Option<usize> {
        match self {
            Self::LineFeed => Some(0),
            Self::ShortEscape(escape) => Some(escape.len()),
            Self::UnicodeEscape(value) => Some(4 + hex_digits(value)),
            Self::Printable(_) => None,
        }
    }
}

pub(crate) fn must_escape(character: char) -> bool {
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

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum VisibleTextError {
    #[error("CLI_OUTPUT_CAPACITY")]
    Capacity,
    #[error("CLI_OUTPUT_LIMIT")]
    Limit,
}

pub(crate) fn render_visible_owned(
    input: &str,
    preserve_line_feed: bool,
) -> Result<String, VisibleTextError> {
    let mut output = String::new();
    output
        .try_reserve_exact(input.len())
        .map_err(|_| VisibleTextError::Capacity)?;
    for character in input.chars() {
        let visible = VisibleChar::classify(character, !preserve_line_feed);
        let needed = match visible {
            VisibleChar::LineFeed => 1,
            VisibleChar::ShortEscape(escape) => escape.len(),
            VisibleChar::UnicodeEscape(value) => 4 + hex_digits(value),
            VisibleChar::Printable(character) => character.len_utf8(),
        };
        let next = output
            .len()
            .checked_add(needed)
            .ok_or(VisibleTextError::Limit)?;
        if next > MAX_VISIBLE_OWNED_BYTES {
            return Err(VisibleTextError::Limit);
        }
        output
            .try_reserve(needed)
            .map_err(|_| VisibleTextError::Capacity)?;
        match visible {
            VisibleChar::LineFeed => output.push('\n'),
            VisibleChar::ShortEscape(escape) => output.push_str(escape),
            VisibleChar::UnicodeEscape(value) => {
                // The exact escape length was reserved above. Writing into a
                // String is therefore infallible for this bounded scalar.
                write!(&mut output, "\\u{{{value:x}}}")
                    .expect("writing a reserved Unicode escape cannot fail");
            }
            VisibleChar::Printable(character) => output.push(character),
        }
    }
    Ok(output)
}

fn hex_digits(mut value: u32) -> usize {
    let mut digits = 1;
    while value >= 16 {
        value /= 16;
        digits += 1;
    }
    digits
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_VISIBLE_OWNED_BYTES, UNICODE_16_FORMAT_RANGES, VisibleChar, must_escape,
        render_visible_owned,
    };

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
    fn visible_width_matches_the_literal_escape_spelling() {
        for (character, expected) in [('\t', 2), ('\r', 2), ('\u{7}', 5), ('\u{1b}', 6)] {
            assert_eq!(
                VisibleChar::classify(character, false).escaped_cell_width(),
                Some(expected)
            );
        }
        assert_eq!(
            VisibleChar::classify('\u{202e}', false).escaped_cell_width(),
            Some("\\u{202e}".len())
        );
    }

    #[test]
    fn owned_rendering_matches_the_streaming_safety_spelling() {
        assert_eq!(
            render_visible_owned("a\t\r\x1b\u{202e}\n中", true).unwrap(),
            "a\\t\\r\\u{1b}\\u{202e}\n中"
        );
    }

    #[test]
    fn owned_rendering_accepts_one_mib_and_rejects_one_byte_more() {
        let exact = "x".repeat(MAX_VISIBLE_OWNED_BYTES);
        assert_eq!(
            render_visible_owned(&exact, true).unwrap().len(),
            exact.len()
        );
        let over = "x".repeat(MAX_VISIBLE_OWNED_BYTES + 1);
        assert!(render_visible_owned(&over, true).is_err());
    }
}
