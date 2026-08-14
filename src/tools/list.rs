use tokio_util::sync::CancellationToken;

use super::{
    MAX_LIST_ENTRIES, MAX_LIST_SCANNED_ENTRIES, MAX_TOOL_CONTENT_BYTES, MAX_TRAVERSAL_PATH_BYTES,
    arguments::ListArgs,
    error::{ToolCallError, ToolCallResult},
    json_string_content_bytes, text_block_encoded_bytes,
    workspace::{EntryKind, Workspace},
};

pub(crate) async fn execute(
    workspace: &Workspace,
    arguments: ListArgs,
    cancellation: &CancellationToken,
) -> ToolCallResult<String> {
    let path = workspace.resolve(&arguments.path)?;
    let mut entries = workspace
        .read_directory(
            &path,
            MAX_LIST_SCANNED_ENTRIES,
            MAX_TRAVERSAL_PATH_BYTES,
            cancellation,
        )
        .await?;
    entries.sort_by(|left, right| left.name.as_bytes().cmp(right.name.as_bytes()));

    if entries.is_empty() {
        return Ok(format!("Directory {} is empty", path.display));
    }

    let total = entries.len();
    let mut output = format!("Directory {}\n", path.display);
    let mut encoded_content_bytes = json_string_content_bytes(&output);
    let mut shown = 0_usize;
    for entry in entries.iter().take(MAX_LIST_ENTRIES) {
        let rendered = match entry.kind {
            EntryKind::File => match entry.size {
                Some(size) => format!("file\t{}\t{size} bytes\n", entry.name),
                None => format!("file\t{}\n", entry.name),
            },
            EntryKind::Directory => format!("directory\t{}/\n", entry.name),
            EntryKind::Symlink => format!("symlink\t{}@\n", entry.name),
            EntryKind::Other => format!("other\t{}\n", entry.name),
        };
        let next_shown = shown + 1;
        let footer = footer(next_shown, total);
        let next_encoded_content_bytes =
            encoded_content_bytes.saturating_add(json_string_content_bytes(&rendered));
        let final_encoded_content_bytes =
            next_encoded_content_bytes.saturating_add(json_string_content_bytes(&footer));
        if text_block_encoded_bytes(final_encoded_content_bytes) > MAX_TOOL_CONTENT_BYTES {
            break;
        }
        output.push_str(&rendered);
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
        format!("Showing {shown} of {total} entries; remaining entries were not saved.")
    } else {
        format!("{total} entries")
    }
}
