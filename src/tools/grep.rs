use std::path::Path;

use globset::{GlobBuilder, GlobMatcher};
use regex::bytes::{Regex, RegexBuilder};
use tokio_util::sync::CancellationToken;

use super::{
    MAX_GREP_FILE_BYTES, MAX_GREP_INPUT_BYTES, MAX_GREP_LINE_BYTES, MAX_GREP_MATCHES,
    MAX_GREP_PREVIEW_BYTES, MAX_GREP_RESULTS, MAX_REGEX_COMPILED_BYTES, MAX_TOOL_CONTENT_BYTES,
    MAX_VISITED_ENTRIES,
    arguments::GrepArgs,
    error::{ToolCallError, ToolCallResult},
    json_string_content_bytes, text_block_encoded_bytes,
    workspace::{EntryKind, Workspace, WorkspaceFile},
};

struct Match {
    path: String,
    line_number: usize,
    preview: String,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    arguments: GrepArgs,
    cancellation: &CancellationToken,
) -> ToolCallResult<String> {
    let expression = RegexBuilder::new(&arguments.pattern)
        .size_limit(MAX_REGEX_COMPILED_BYTES)
        .build()
        .map_err(|_| {
            ToolCallError::invalid_pattern("grep.pattern is not a valid regular expression")
        })?;
    let include_has_separator = arguments
        .include
        .as_deref()
        .is_some_and(|pattern| pattern.contains('/'));
    let include = arguments.include.as_deref().map(compile_glob).transpose()?;
    let target = workspace.resolve(&arguments.path)?;
    let kind = workspace.classify(&target, cancellation).await?;
    let mut files = if kind == EntryKind::File {
        vec![WorkspaceFile {
            relative: target.relative.clone(),
            display: target.display.clone(),
            modified: std::time::SystemTime::UNIX_EPOCH,
        }]
    } else if kind == EntryKind::Directory {
        workspace
            .walk_files(&target, MAX_VISITED_ENTRIES, cancellation)
            .await?
    } else {
        return Err(ToolCallError::not_regular_file(&target.display));
    };
    files.sort_by(|left, right| left.display.as_bytes().cmp(right.display.as_bytes()));

    let mut total_input = 0_usize;
    let mut matches = Vec::new();
    for file in files {
        if !include_matches(
            include.as_ref(),
            include_has_separator,
            &target.relative,
            &file,
        ) {
            continue;
        }
        let read_limit = next_read_limit(total_input)?;
        let read = workspace
            .read_file(
                &super::workspace::ResolvedPath {
                    relative: file.relative.clone(),
                    display: file.display.clone(),
                },
                read_limit,
                cancellation,
            )
            .await
            .map_err(|error| {
                if read_limit < MAX_GREP_FILE_BYTES && error.has_code("FS_TOO_LARGE") {
                    aggregate_limit_error()
                } else {
                    error
                }
            })?;
        total_input = total_input
            .checked_add(read.bytes.len())
            .ok_or_else(|| ToolCallError::search_limit("grep input byte count overflow"))?;
        if total_input > MAX_GREP_INPUT_BYTES {
            return Err(ToolCallError::search_limit(format!(
                "grep scans more than {MAX_GREP_INPUT_BYTES} bytes"
            )));
        }
        if read.bytes.contains(&0) {
            continue;
        }
        collect_matches(&expression, &file.display, &read.bytes, &mut matches)?;
    }
    if matches.is_empty() {
        return Ok("No matches found".to_owned());
    }
    render(matches)
}

fn next_read_limit(total_input: usize) -> ToolCallResult<usize> {
    let remaining = MAX_GREP_INPUT_BYTES
        .checked_sub(total_input)
        .ok_or_else(aggregate_limit_error)?;
    Ok(remaining.min(MAX_GREP_FILE_BYTES))
}

fn aggregate_limit_error() -> ToolCallError {
    ToolCallError::search_limit(format!("grep scans more than {MAX_GREP_INPUT_BYTES} bytes"))
}

fn compile_glob(pattern: &str) -> ToolCallResult<GlobMatcher> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|_| ToolCallError::invalid_pattern("grep.include is not a valid glob"))
}

fn include_matches(
    include: Option<&GlobMatcher>,
    has_separator: bool,
    root: &Path,
    file: &WorkspaceFile,
) -> bool {
    let Some(include) = include else {
        return true;
    };
    let relative = file.relative.strip_prefix(root).unwrap_or(&file.relative);
    if has_separator {
        include.is_match(relative)
    } else {
        let candidate = if relative.as_os_str().is_empty() {
            file.relative.as_path()
        } else {
            relative
        };
        candidate
            .file_name()
            .is_some_and(|name| include.is_match(name))
    }
}

fn collect_matches(
    expression: &Regex,
    path: &str,
    bytes: &[u8],
    output: &mut Vec<Match>,
) -> ToolCallResult<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let content = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    for (line_index, mut line) in content.split(|byte| *byte == b'\n').enumerate() {
        let line_number = line_index + 1;
        if line.ends_with(b"\r") {
            line = &line[..line.len() - 1];
        }
        if line.len() > MAX_GREP_LINE_BYTES {
            return Err(ToolCallError::search_limit(format!(
                "grep encountered a line longer than {MAX_GREP_LINE_BYTES} bytes"
            )));
        }
        if !expression.is_match(line) {
            continue;
        }
        if output.len() >= MAX_GREP_MATCHES {
            return Err(ToolCallError::search_limit(format!(
                "grep has more than {MAX_GREP_MATCHES} matching lines"
            )));
        }
        output.push(Match {
            path: path.to_owned(),
            line_number,
            preview: preview(line),
        });
    }
    Ok(())
}

fn preview(line: &[u8]) -> String {
    let Ok(text) = std::str::from_utf8(line) else {
        return "(line is not valid UTF-8)".to_owned();
    };
    if text.len() <= MAX_GREP_PREVIEW_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_GREP_PREVIEW_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{} (line truncated)", &text[..end])
}

fn render(matches: Vec<Match>) -> ToolCallResult<String> {
    let total = matches.len();
    let mut output = format!("Found {total} matches\n");
    let mut encoded_content_bytes = json_string_content_bytes(&output);
    let mut current_path: Option<&str> = None;
    let mut shown = 0_usize;
    for found in matches.iter().take(MAX_GREP_RESULTS) {
        let heading = if current_path == Some(found.path.as_str()) {
            String::new()
        } else {
            format!("\n{}\n", found.path)
        };
        let line = format!("Line {}: {}\n", found.line_number, found.preview);
        let next_shown = shown + 1;
        let footer = footer(next_shown, total);
        let next_encoded_content_bytes = encoded_content_bytes
            .saturating_add(json_string_content_bytes(&heading))
            .saturating_add(json_string_content_bytes(&line));
        let final_encoded_content_bytes =
            next_encoded_content_bytes.saturating_add(json_string_content_bytes(&footer));
        if text_block_encoded_bytes(final_encoded_content_bytes) > MAX_TOOL_CONTENT_BYTES {
            break;
        }
        output.push_str(&heading);
        output.push_str(&line);
        encoded_content_bytes = next_encoded_content_bytes;
        current_path = Some(&found.path);
        shown = next_shown;
    }
    output.push_str(&footer(shown, total));
    if output.ends_with('\n') {
        output.pop();
    }
    if text_block_encoded_bytes(json_string_content_bytes(&output)) > MAX_TOOL_CONTENT_BYTES {
        return Err(ToolCallError::output_limit());
    }
    Ok(output)
}

fn footer(shown: usize, total: usize) -> String {
    if shown < total {
        format!("\nShowing {shown} of {total} matches; the complete result was not saved.")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use regex::bytes::Regex;

    use super::{
        MAX_GREP_FILE_BYTES, MAX_GREP_INPUT_BYTES, MAX_GREP_LINE_BYTES, MAX_GREP_MATCHES, Match,
        collect_matches, next_read_limit, preview,
    };

    #[test]
    fn aggregate_input_budget_never_starts_a_read_beyond_the_ceiling() {
        assert_eq!(next_read_limit(0).unwrap(), MAX_GREP_FILE_BYTES);
        assert_eq!(next_read_limit(MAX_GREP_INPUT_BYTES - 1).unwrap(), 1);
        assert_eq!(next_read_limit(MAX_GREP_INPUT_BYTES).unwrap(), 0);
        assert!(next_read_limit(MAX_GREP_INPUT_BYTES + 1).is_err());
    }

    #[test]
    fn empty_files_have_no_line_but_real_blank_lines_are_searchable() {
        let expression = Regex::new("^$").unwrap();
        let mut found = Vec::<Match>::new();
        collect_matches(&expression, "empty", b"", &mut found).unwrap();
        assert!(found.is_empty());

        collect_matches(&expression, "one", b"\n", &mut found).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line_number, 1);

        found.clear();
        collect_matches(&expression, "two", b"a\n\n", &mut found).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line_number, 2);
    }

    #[test]
    fn previews_preserve_utf8_boundaries_and_replace_invalid_text() {
        let text = format!("{}界", "x".repeat(1_999));
        let rendered = preview(text.as_bytes());
        assert!(rendered.ends_with(" (line truncated)"));
        assert!(std::str::from_utf8(rendered.as_bytes()).is_ok());
        assert_eq!(preview(&[0xff]), "(line is not valid UTF-8)");
    }

    #[test]
    fn line_and_stored_match_budgets_accept_the_limit_and_reject_one_more() {
        let expression = Regex::new("x").unwrap();
        let exact_line = vec![b'x'; MAX_GREP_LINE_BYTES];
        let mut found = Vec::new();
        collect_matches(&expression, "exact", &exact_line, &mut found).unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            collect_matches(
                &expression,
                "over",
                &vec![b'x'; MAX_GREP_LINE_BYTES + 1],
                &mut Vec::new(),
            )
            .is_err()
        );

        let exact_matches = b"x\n".repeat(MAX_GREP_MATCHES);
        found.clear();
        collect_matches(&expression, "matches", &exact_matches, &mut found).unwrap();
        assert_eq!(found.len(), MAX_GREP_MATCHES);
        assert!(
            collect_matches(&expression, "one-over", b"x", &mut found).is_err(),
            "the next matching line must fail before it is retained"
        );
    }

    #[test]
    fn rendering_preserves_trailing_whitespace_in_the_last_match() {
        let rendered = super::render(vec![Match {
            path: "notes.txt".to_owned(),
            line_number: 1,
            preview: "keep these spaces  ".to_owned(),
        }])
        .unwrap();
        assert!(rendered.ends_with("Line 1: keep these spaces  "));
    }
}
