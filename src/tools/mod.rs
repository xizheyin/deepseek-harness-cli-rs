//! Workspace-confined deterministic tools, including approval-gated file changes.

mod arguments;
mod error;
mod glob;
mod grep;
mod list;
#[cfg(unix)]
mod patch;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod process;
mod read;
mod registry;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod shell;
mod workspace;

pub use error::ToolRegistryBuildError;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use registry::LocalToolRegistry;
pub use registry::ReadOnlyToolRegistry;
#[cfg(unix)]
pub use registry::WorkspaceToolRegistry;

const MAX_TOOL_CONTENT_BYTES: usize = 64 * 1024;
const MAX_READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_READ_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_READ_LINE_CHARS: usize = 2_000;
const MAX_READ_SELECTED_BYTES: usize = 50 * 1024;
const MAX_LIST_SCANNED_ENTRIES: usize = 10_000;
const MAX_LIST_ENTRIES: usize = 500;
const MAX_DIRECTORY_DEPTH: usize = 64;
const MAX_VISITED_ENTRIES: usize = 50_000;
const MAX_TRAVERSAL_PATH_BYTES: usize = 8 * 1024 * 1024;
const MAX_GLOB_MATCHES: usize = 10_000;
const MAX_GLOB_RESULTS: usize = 100;
const MAX_GREP_FILE_BYTES: usize = 8 * 1024 * 1024;
const MAX_GREP_INPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_GREP_LINE_BYTES: usize = 1024 * 1024;
const MAX_GREP_MATCHES: usize = 10_000;
const MAX_GREP_RESULTS: usize = 250;
const MAX_GREP_PREVIEW_BYTES: usize = 2_000;
const MAX_REGEX_COMPILED_BYTES: usize = 4 * 1024 * 1024;

// Compact serde_json encoding of {"text":"","type":"text"}. Keeping this
// constant next to the scanner makes every renderer budget the durable JSON,
// including escaping, rather than only the visible UTF-8 text.
const EMPTY_TEXT_BLOCK_JSON_BYTES: usize = 25;

fn json_string_content_bytes(value: &str) -> usize {
    value.chars().fold(0_usize, |total, character| {
        let encoded = match character {
            '"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            other => other.len_utf8(),
        };
        total.saturating_add(encoded)
    })
}

fn text_block_encoded_bytes(encoded_content_bytes: usize) -> usize {
    EMPTY_TEXT_BLOCK_JSON_BYTES.saturating_add(encoded_content_bytes)
}

#[cfg(test)]
mod encoding_tests {
    use crate::model::ContentBlock;

    use super::{EMPTY_TEXT_BLOCK_JSON_BYTES, json_string_content_bytes, text_block_encoded_bytes};

    #[test]
    fn text_budget_matches_the_actual_compact_json_encoding() {
        for value in [
            "",
            "plain",
            "quote: \" slash: \\",
            "line\ncarriage\rtab\tbackspace\u{0008}form\u{000c}",
            "control \u{0001} and emoji 界🙂",
        ] {
            let block = ContentBlock::text(value).unwrap();
            assert_eq!(
                text_block_encoded_bytes(json_string_content_bytes(value)),
                block.raw().encoded_len()
            );
        }
        assert_eq!(
            ContentBlock::text("").unwrap().raw().encoded_len(),
            EMPTY_TEXT_BLOCK_JSON_BYTES
        );
    }
}
