use globset::GlobBuilder;
use tokio_util::sync::CancellationToken;

use super::{
    MAX_GLOB_MATCHES, MAX_GLOB_RESULTS, MAX_TOOL_CONTENT_BYTES, MAX_VISITED_ENTRIES,
    arguments::GlobArgs,
    error::{ToolCallError, ToolCallResult},
    json_string_content_bytes, text_block_encoded_bytes,
    workspace::{Workspace, WorkspaceFile},
};

pub(crate) async fn execute(
    workspace: &Workspace,
    arguments: GlobArgs,
    cancellation: &CancellationToken,
) -> ToolCallResult<String> {
    let root = workspace.resolve(&arguments.path)?;
    let matcher = GlobBuilder::new(&arguments.pattern)
        .literal_separator(true)
        .build()
        .map_err(|_| ToolCallError::invalid_pattern("glob.pattern is not a valid glob"))?
        .compile_matcher();
    let pattern_has_separator = arguments.pattern.contains('/');
    let files = workspace
        .walk_files(&root, MAX_VISITED_ENTRIES, cancellation)
        .await?;
    let mut matches = Vec::new();
    for file in files {
        let relative_to_root = file
            .relative
            .strip_prefix(&root.relative)
            .unwrap_or(&file.relative);
        let matched = if pattern_has_separator {
            matcher.is_match(relative_to_root)
        } else {
            relative_to_root
                .file_name()
                .is_some_and(|name| matcher.is_match(name))
        };
        if matched {
            push_match(&mut matches, file)?;
        }
    }
    if matches.is_empty() {
        return Ok("No files found".to_owned());
    }
    matches.sort_by(compare_files);
    render(matches)
}

fn push_match(matches: &mut Vec<WorkspaceFile>, file: WorkspaceFile) -> ToolCallResult<()> {
    if matches.len() >= MAX_GLOB_MATCHES {
        return Err(ToolCallError::search_limit(format!(
            "glob has more than {MAX_GLOB_MATCHES} matches"
        )));
    }
    matches.push(file);
    Ok(())
}

fn compare_files(left: &WorkspaceFile, right: &WorkspaceFile) -> std::cmp::Ordering {
    left.modified
        .cmp(&right.modified)
        .then_with(|| left.display.as_bytes().cmp(right.display.as_bytes()))
}

fn render(matches: Vec<WorkspaceFile>) -> ToolCallResult<String> {
    let total = matches.len();
    let mut output = String::new();
    let mut encoded_content_bytes = 0_usize;
    let mut shown = 0_usize;
    for file in matches.iter().take(MAX_GLOB_RESULTS) {
        let separator = if output.is_empty() { "" } else { "\n" };
        let next_shown = shown + 1;
        let footer = footer(next_shown, total);
        let next_encoded_content_bytes = encoded_content_bytes
            .saturating_add(json_string_content_bytes(separator))
            .saturating_add(json_string_content_bytes(&file.display));
        let final_encoded_content_bytes =
            next_encoded_content_bytes.saturating_add(json_string_content_bytes(&footer));
        if text_block_encoded_bytes(final_encoded_content_bytes) > MAX_TOOL_CONTENT_BYTES {
            break;
        }
        output.push_str(separator);
        output.push_str(&file.display);
        encoded_content_bytes = next_encoded_content_bytes;
        shown = next_shown;
    }
    output.push_str(&footer(shown, total));
    if text_block_encoded_bytes(json_string_content_bytes(&output)) > MAX_TOOL_CONTENT_BYTES {
        return Err(ToolCallError::output_limit());
    }
    Ok(output)
}

fn footer(shown: usize, total: usize) -> String {
    if shown < total {
        format!("\n\nShowing {shown} of {total} files; the complete result was not saved.")
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::SystemTime};

    use super::{MAX_GLOB_MATCHES, WorkspaceFile, push_match};

    #[test]
    fn stored_match_budget_accepts_the_limit_and_rejects_one_more() {
        let mut matches = Vec::new();
        for index in 0..MAX_GLOB_MATCHES {
            let display = format!("file-{index}");
            push_match(
                &mut matches,
                WorkspaceFile {
                    relative: PathBuf::from(&display),
                    display,
                    modified: SystemTime::UNIX_EPOCH,
                },
            )
            .unwrap();
        }
        assert_eq!(matches.len(), MAX_GLOB_MATCHES);
        assert!(
            push_match(
                &mut matches,
                WorkspaceFile {
                    relative: PathBuf::from("one-over"),
                    display: "one-over".to_owned(),
                    modified: SystemTime::UNIX_EPOCH,
                },
            )
            .is_err()
        );
    }
}
