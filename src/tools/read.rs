use tokio_util::sync::CancellationToken;

use super::{
    MAX_READ_FILE_BYTES, MAX_READ_LINE_CHARS, MAX_READ_SELECTED_BYTES, MAX_TOOL_CONTENT_BYTES,
    arguments::ReadArgs,
    error::{ToolCallError, ToolCallResult},
    json_string_content_bytes, text_block_encoded_bytes,
    workspace::Workspace,
};

pub(crate) async fn execute(
    workspace: &Workspace,
    arguments: ReadArgs,
    cancellation: &CancellationToken,
) -> ToolCallResult<String> {
    let path = workspace.resolve(&arguments.file_path)?;
    let file = workspace
        .read_file(&path, MAX_READ_FILE_BYTES, cancellation)
        .await?;
    if file.bytes.contains(&0) {
        return Err(ToolCallError::not_text(&path.display));
    }
    let text =
        std::str::from_utf8(&file.bytes).map_err(|_| ToolCallError::not_text(&path.display))?;
    let total = text.lines().count();
    let start = arguments.offset - 1;
    if (total == 0 && start != 0) || (total > 0 && start >= total) {
        return Err(ToolCallError::not_found(&path.display));
    }

    let mut rendered_lines = Vec::new();
    let mut selected_bytes = 0_usize;
    let mut rendered_body_json_bytes = 0_usize;
    let mut cursor = start;
    let first_line = start + 1;
    let header_json_bytes = json_string_content_bytes(&format!(
        "<path>{}</path>\n<type>file</type>\n<content>\n",
        path.display
    ));
    let suffix_json_bytes = json_string_content_bytes("\n</content>");
    let requested_end = start.saturating_add(arguments.limit).min(total);
    let mut output_capped = false;
    for line in text
        .lines()
        .skip(start)
        .take(requested_end.saturating_sub(start))
    {
        let displayed = truncate_line(line);
        let added = displayed.len() + usize::from(!rendered_lines.is_empty());
        if selected_bytes.saturating_add(added) > MAX_READ_SELECTED_BYTES {
            output_capped = true;
            break;
        }
        let rendered = format!("{}: {displayed}", cursor + 1);
        let next_body_json_bytes = rendered_body_json_bytes
            .saturating_add(json_string_content_bytes(&rendered))
            .saturating_add(if rendered_lines.is_empty() { 0 } else { 2 });
        let next_cursor = cursor + 1;
        let reserved_footer_bytes = maximum_footer_bytes(first_line, next_cursor, total);
        let projected_json_bytes = header_json_bytes
            .saturating_add(next_body_json_bytes)
            .saturating_add(4)
            .saturating_add(reserved_footer_bytes)
            .saturating_add(suffix_json_bytes);
        if text_block_encoded_bytes(projected_json_bytes) > MAX_TOOL_CONTENT_BYTES {
            output_capped = true;
            break;
        }
        selected_bytes += added;
        rendered_body_json_bytes = next_body_json_bytes;
        rendered_lines.push(rendered);
        cursor = next_cursor;
    }

    let last_line = cursor;
    let footer = if output_capped {
        format!(
            "(Output capped. Showing lines {first_line}-{last_line}. Use offset={} to continue.)",
            cursor + 1
        )
    } else if cursor < total {
        format!(
            "(Showing lines {first_line}-{last_line} of {total}. Use offset={} to continue.)",
            cursor + 1
        )
    } else {
        format!("(End of file - total {total} lines)")
    };

    let body = rendered_lines.join("\n");
    Ok(format!(
        "<path>{}</path>\n<type>file</type>\n<content>\n{}{}{}\n</content>",
        path.display,
        body,
        if body.is_empty() { "" } else { "\n\n" },
        footer
    ))
}

fn truncate_line(line: &str) -> String {
    let total = line.chars().count();
    if total <= MAX_READ_LINE_CHARS {
        return line.to_owned();
    }
    let prefix = line.chars().take(MAX_READ_LINE_CHARS).collect::<String>();
    format!("{prefix}... (line truncated to {MAX_READ_LINE_CHARS} chars)")
}

fn maximum_footer_bytes(first_line: usize, cursor: usize, total: usize) -> usize {
    let cap = format!(
        "(Output capped. Showing lines {first_line}-{cursor}. Use offset={} to continue.)",
        cursor + 1
    );
    let page = format!(
        "(Showing lines {first_line}-{cursor} of {total}. Use offset={} to continue.)",
        cursor + 1
    );
    let end = format!("(End of file - total {total} lines)");
    json_string_content_bytes(&cap)
        .max(json_string_content_bytes(&page))
        .max(json_string_content_bytes(&end))
}

#[cfg(test)]
mod tests {
    use super::truncate_line;

    #[test]
    fn line_split_drops_only_the_final_lf_entry() {
        assert_eq!("".lines().collect::<Vec<_>>(), Vec::<&str>::new());
        assert_eq!("a\n".lines().collect::<Vec<_>>(), vec!["a"]);
        assert_eq!("a\n\n".lines().collect::<Vec<_>>(), vec!["a", ""]);
        assert_eq!("a\r\n".lines().collect::<Vec<_>>(), vec!["a"]);
    }

    #[test]
    fn line_truncation_uses_the_upstream_fixed_display_limit() {
        let line = "x".repeat(2_001);
        let rendered = truncate_line(&line);
        assert!(rendered.starts_with(&"x".repeat(2_000)));
        assert!(rendered.ends_with("... (line truncated to 2000 chars)"));
    }
}
