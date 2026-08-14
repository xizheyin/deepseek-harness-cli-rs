use std::{collections::BTreeSet, path::Path};

use diffy::{Line, Patch};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{
        ApprovalPrompt, MutationDeclineReason, PreparedToolMutation, ToolCommitOutcome,
        ToolExecutionResult, ToolExecutorError, ToolPreparation,
    },
    model::{ContentBlock, JsonValue},
    session::ToolFailure,
};

use super::{
    MAX_DIRECTORY_DEPTH,
    error::ToolCallError,
    workspace::{Workspace, WorkspaceCommitStatus, WorkspaceMutationOperation},
};

pub(crate) const MAX_PATCH_BYTES: usize = 256 * 1024;
const MAX_PATCH_HUNKS: usize = 1_024;
const MAX_PATCH_LINES: usize = 100_000;
const MAX_PATCH_LINE_BYTES: usize = 64 * 1024;
const MAX_MUTATION_FILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_MUTATION_FILE_LINES: usize = 100_000;
const MAX_MUTATION_FILE_LINE_BYTES: usize = 1024 * 1024;
const MAX_CANONICAL_DIFF_JSON_BYTES: usize = 64 * 1024;
const MAX_MUTATION_RESULT_EVENT_BYTES: usize = 128 * 1024;

pub(crate) async fn prepare(
    workspace: &Workspace,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<ToolPreparation, ToolExecutorError> {
    let input = match parse_arguments(arguments) {
        Ok(input) => input,
        Err(error) => return error.into_execution_result().map(ToolPreparation::Complete),
    };
    if cancellation.is_cancelled() {
        return ToolCallError::aborted()
            .into_execution_result()
            .map(ToolPreparation::Complete);
    }
    let parsed = match parse_patch(&input) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_execution_result().map(ToolPreparation::Complete),
    };
    let resolved = match workspace.resolve(&parsed.path) {
        Ok(path) => path,
        Err(error) => return error.into_execution_result().map(ToolPreparation::Complete),
    };
    let target = match workspace
        .prepare_mutation(
            resolved,
            parsed.operation,
            MAX_MUTATION_FILE_BYTES,
            cancellation,
        )
        .await
    {
        Ok(target) => target,
        Err(error) => return error.into_execution_result().map(ToolPreparation::Complete),
    };
    let before_raw = target.baseline().unwrap_or_default();
    let before = match normalize_existing_text(before_raw) {
        Ok(value) => value,
        Err(error) => return error.into_execution_result().map(ToolPreparation::Complete),
    };
    let candidate = match apply_strict(&before.normalized, &parsed.patch, cancellation) {
        Ok(candidate) => candidate,
        Err(error) => return error.into_execution_result().map(ToolPreparation::Complete),
    };
    if candidate == before.normalized {
        return ToolCallError::model(
            "PatchError",
            "NO_CHANGES",
            "the patch does not change the target file",
        )
        .into_execution_result()
        .map(ToolPreparation::Complete);
    }
    if let Err(error) = validate_file_text(&candidate) {
        return error.into_execution_result().map(ToolPreparation::Complete);
    }
    let candidate_raw = match before.line_endings {
        LineEndings::Lf => candidate.as_bytes().to_vec(),
        LineEndings::CrLf => candidate.replace('\n', "\r\n").into_bytes(),
    };
    if candidate_raw.len() > MAX_MUTATION_FILE_BYTES {
        return ToolCallError::model(
            "PatchError",
            "PATCH_TOO_LARGE",
            "the patched file exceeds the mutation file limit",
        )
        .into_execution_result()
        .map(ToolPreparation::Complete);
    }
    let canonical_diff = match canonical_diff(
        &parsed.path,
        parsed.operation,
        &before.normalized,
        &parsed.patch,
        cancellation,
    ) {
        Ok(diff) => diff,
        Err(error) => return error.into_execution_result().map(ToolPreparation::Complete),
    };
    let meta_probe =
        match mutation_meta(&parsed.path, parsed.operation, &canonical_diff, false, true) {
            Ok(meta) => meta,
            Err(error) => return Err(error),
        };
    if canonical_diff.is_empty() || meta_probe.encoded_len() > MAX_CANONICAL_DIFF_JSON_BYTES {
        return ToolCallError::model(
            "PatchError",
            "DIFF_TOO_LARGE",
            "the complete canonical approval diff exceeds the configured limit",
        )
        .into_execution_result()
        .map(ToolPreparation::Complete);
    }
    let prompt = ApprovalPrompt::new(
        approval_reason(parsed.operation, &parsed.path),
        canonical_diff.clone(),
    )
    .map_err(|_| ToolExecutorError::new("approval prompt normalization failed"))?;

    let decline_path = parsed.path.clone();
    let decline_diff = canonical_diff.clone();
    let decline_operation = parsed.operation;
    let commit_path = parsed.path;
    let commit_diff = canonical_diff;
    let commit_operation = parsed.operation;
    // Every post-publication result is fully built before the commit
    // capability exists. After link/rename succeeds, no fallible JSON/model
    // construction remains that could erase the truthful committed fact.
    let committed_success = ToolCommitOutcome::committed(success_result(
        &commit_path,
        commit_operation,
        &commit_diff,
    )?)?;
    let committed_durability = ToolCommitOutcome::committed(failure_result(
        &commit_path,
        commit_operation,
        &commit_diff,
        true,
        "FsError",
        "FILE_COMMITTED_DURABILITY_UNCERTAIN",
        "the file changed, but directory synchronization was uncertain",
        false,
    )?)?;
    let committed_durability_and_cleanup = ToolCommitOutcome::committed(failure_result(
        &commit_path,
        commit_operation,
        &commit_diff,
        true,
        "FsError",
        "FILE_COMMITTED_DURABILITY_UNCERTAIN",
        "the file changed, but directory synchronization and staging cleanup were uncertain",
        true,
    )?)?;
    let committed_cleanup_warning = ToolCommitOutcome::committed(failure_result(
        &commit_path,
        commit_operation,
        &commit_diff,
        true,
        "FsError",
        "FILE_COMMITTED_CLEANUP_WARNING",
        "the file changed and was synchronized, but private staging cleanup failed",
        true,
    )?)?;
    let not_committed_path = commit_path.clone();
    let not_committed_diff = commit_diff.clone();
    let mutation = PreparedToolMutation::new(
        prompt,
        MAX_MUTATION_RESULT_EVENT_BYTES,
        Box::new(move |reason| {
            decline_result(&decline_path, decline_operation, &decline_diff, reason)
        }),
        Box::new(move |token| {
            let status = target
                .commit(&candidate_raw, &token)
                .map_err(|_| ToolExecutorError::new("workspace mutation commit failed"))?;
            match status {
                WorkspaceCommitStatus::Committed {
                    durability_uncertain: false,
                    cleanup_warning: false,
                } => Ok(committed_success),
                WorkspaceCommitStatus::Committed {
                    durability_uncertain: true,
                    cleanup_warning: false,
                } => Ok(committed_durability),
                WorkspaceCommitStatus::Committed {
                    durability_uncertain: true,
                    cleanup_warning: true,
                } => Ok(committed_durability_and_cleanup),
                WorkspaceCommitStatus::Committed {
                    durability_uncertain: false,
                    cleanup_warning: true,
                } => Ok(committed_cleanup_warning),
                WorkspaceCommitStatus::NotCommitted {
                    error,
                    cleanup_warning,
                } => {
                    let (name, code, message) = error.into_model_parts()?;
                    let message = if cleanup_warning {
                        format!("{message}; private staging cleanup may be incomplete")
                    } else {
                        message
                    };
                    ToolCommitOutcome::not_committed(failure_result(
                        &not_committed_path,
                        commit_operation,
                        &not_committed_diff,
                        false,
                        name,
                        code,
                        &message,
                        cleanup_warning,
                    )?)
                }
            }
        }),
    )?;
    Ok(ToolPreparation::Mutation(mutation))
}

struct ParsedPatch<'a> {
    operation: WorkspaceMutationOperation,
    path: String,
    patch: Patch<'a, str>,
}

fn parse_arguments(arguments: &Value) -> Result<String, ToolCallError> {
    let Some(fields) = arguments.as_object() else {
        return Err(ToolCallError::invalid_args(
            "apply_patch arguments must be one closed object",
        ));
    };
    if fields.len() != 1 {
        return Err(ToolCallError::invalid_args(
            "apply_patch accepts only the required patch field",
        ));
    }
    let Some(patch) = fields.get("patch").and_then(Value::as_str) else {
        return Err(ToolCallError::invalid_args(
            "apply_patch.patch must be a non-null string",
        ));
    };
    if patch.is_empty() || patch.len() > MAX_PATCH_BYTES {
        return Err(ToolCallError::model(
            "PatchError",
            "PATCH_TOO_LARGE",
            format!("patch input must contain 1 to {MAX_PATCH_BYTES} UTF-8 bytes"),
        ));
    }
    Ok(patch.to_owned())
}

fn parse_patch(input: &str) -> Result<ParsedPatch<'_>, ToolCallError> {
    if !input.ends_with('\n')
        || input
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(invalid_patch(
            "patch must use safe LF text, end with one newline, and contain no unsafe controls",
        ));
    }
    let lines = input.split_inclusive('\n').collect::<Vec<_>>();
    if lines.len() > MAX_PATCH_LINES || lines.iter().any(|line| line.len() > MAX_PATCH_LINE_BYTES) {
        return Err(ToolCallError::model(
            "PatchError",
            "PATCH_TOO_LARGE",
            "patch line count or line length exceeds the configured limit",
        ));
    }
    let raw_hunk_count = lines.iter().filter(|line| line.starts_with("@@ ")).count();
    if raw_hunk_count > MAX_PATCH_HUNKS {
        return Err(ToolCallError::model(
            "PatchError",
            "PATCH_TOO_LARGE",
            "patch contains too many hunks",
        ));
    }
    validate_no_newline_markers(&lines)?;
    let Some(original) = lines.first().copied() else {
        return Err(invalid_patch("patch has no file header"));
    };
    let Some(modified) = lines.get(1).copied() else {
        return Err(invalid_patch("patch has an incomplete file header"));
    };
    let (operation, path) = if original == "--- /dev/null\n" {
        let path = strict_header_path(modified, "+++ b/")?;
        (WorkspaceMutationOperation::Create, path)
    } else {
        let original_path = strict_header_path(original, "--- a/")?;
        let modified_path = strict_header_path(modified, "+++ b/")?;
        if original_path != modified_path {
            return Err(ToolCallError::model(
                "PatchError",
                "UNSUPPORTED_PATCH",
                "rename, copy, and multi-path patches are unsupported",
            ));
        }
        (WorkspaceMutationOperation::Update, original_path)
    };
    let patch = Patch::from_str(input).map_err(|_| invalid_patch("patch syntax is invalid"))?;
    if patch.hunks().is_empty() {
        return Err(invalid_patch("patch must contain at least one hunk"));
    }
    if patch.hunks().len() > MAX_PATCH_HUNKS {
        return Err(ToolCallError::model(
            "PatchError",
            "PATCH_TOO_LARGE",
            "patch contains too many hunks",
        ));
    }
    let hunk_headers = lines
        .iter()
        .filter(|line| line.starts_with("@@ "))
        .copied()
        .collect::<Vec<_>>();
    if hunk_headers.len() != patch.hunks().len()
        || hunk_headers
            .iter()
            .zip(patch.hunks())
            .any(|(actual, hunk)| {
                **actual != format!("@@ -{} +{} @@\n", hunk.old_range(), hunk.new_range())
            })
        || patch.to_string() != input
    {
        return Err(invalid_patch(
            "patch contains unsupported headers, function context, or trailing data",
        ));
    }
    Ok(ParsedPatch {
        operation,
        path,
        patch,
    })
}

fn validate_no_newline_markers(lines: &[&str]) -> Result<(), ToolCallError> {
    const MARKER: &str = "\\ No newline at end of file\n";

    let mut in_hunk = false;
    let mut old_at_eof = false;
    let mut new_at_eof = false;
    let mut previous_sides = None::<(bool, bool)>;

    for line in lines.iter().skip(2).copied() {
        if line.starts_with("@@ ") {
            if old_at_eof || new_at_eof {
                return Err(invalid_patch(
                    "a no-newline marker must describe the final hunk",
                ));
            }
            in_hunk = true;
            previous_sides = None;
            continue;
        }
        if line == MARKER {
            let Some((marks_old, marks_new)) = previous_sides.take() else {
                return Err(invalid_patch(
                    "a no-newline marker must follow one hunk content line",
                ));
            };
            old_at_eof |= marks_old;
            new_at_eof |= marks_new;
            continue;
        }
        if !in_hunk {
            return Err(invalid_patch("patch content must be inside one hunk"));
        }

        let consumed_sides = if line == "\n" || line.starts_with(' ') {
            (true, true)
        } else if line.starts_with('-') {
            (true, false)
        } else if line.starts_with('+') {
            (false, true)
        } else {
            return Err(invalid_patch("patch contains an invalid hunk line"));
        };
        if (consumed_sides.0 && old_at_eof) || (consumed_sides.1 && new_at_eof) {
            return Err(invalid_patch(
                "patch content cannot follow a no-newline end-of-file marker",
            ));
        }
        previous_sides = Some(consumed_sides);
    }
    Ok(())
}

fn strict_header_path(line: &str, prefix: &str) -> Result<String, ToolCallError> {
    let path = line
        .strip_prefix(prefix)
        .and_then(|value| value.strip_suffix('\n'))
        .ok_or_else(|| {
            ToolCallError::model(
                "PatchError",
                "UNSUPPORTED_PATCH",
                "file headers must use the exact a/ and b/ path form",
            )
        })?;
    if path.is_empty()
        || path.len() > 4_096
        || path.chars().any(char::is_control)
        || path.contains('\\')
        || path.split('/').count() > MAX_DIRECTORY_DEPTH
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        || Path::new(path).is_absolute()
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(ToolCallError::workspace_denied());
    }
    Ok(path.to_owned())
}

fn apply_strict(
    before: &str,
    patch: &Patch<'_, str>,
    cancellation: &CancellationToken,
) -> Result<String, ToolCallError> {
    let original = before.split_inclusive('\n').collect::<Vec<_>>();
    let mut old_cursor = 0_usize;
    let mut new_cursor = 0_usize;
    let mut output = String::with_capacity(before.len().min(MAX_MUTATION_FILE_BYTES));
    let mut candidate_at_eof = false;
    for hunk in patch.hunks() {
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        let old_index =
            range_index(hunk.old_range()).ok_or_else(|| invalid_patch("invalid old hunk range"))?;
        let new_index =
            range_index(hunk.new_range()).ok_or_else(|| invalid_patch("invalid new hunk range"))?;
        if old_index < old_cursor || old_index > original.len() {
            return Err(invalid_patch("hunk old range is outside the target"));
        }
        for line in &original[old_cursor..old_index] {
            if cancellation.is_cancelled() {
                return Err(ToolCallError::aborted());
            }
            push_candidate(&mut output, &mut candidate_at_eof, line)?;
            new_cursor = new_cursor
                .checked_add(1)
                .ok_or_else(|| invalid_patch("line count overflow"))?;
        }
        if new_index != new_cursor {
            return Err(invalid_patch(
                "hunk new range does not match its declared position",
            ));
        }
        let mut source = old_index;
        for line in hunk.lines() {
            if cancellation.is_cancelled() {
                return Err(ToolCallError::aborted());
            }
            match line {
                Line::Context(value) => {
                    if original.get(source).copied() != Some(*value) {
                        return Err(invalid_patch("patch context does not match the target"));
                    }
                    push_candidate(&mut output, &mut candidate_at_eof, value)?;
                    source += 1;
                    new_cursor += 1;
                }
                Line::Delete(value) => {
                    if original.get(source).copied() != Some(*value) {
                        return Err(invalid_patch("patch deletion does not match the target"));
                    }
                    source += 1;
                }
                Line::Insert(value) => {
                    push_candidate(&mut output, &mut candidate_at_eof, value)?;
                    new_cursor += 1;
                }
            }
        }
        if source
            != old_index
                .checked_add(hunk.old_range().len())
                .ok_or_else(|| invalid_patch("hunk range overflow"))?
        {
            return Err(invalid_patch("hunk body does not match its old range"));
        }
        old_cursor = source;
    }
    for line in &original[old_cursor..] {
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        push_candidate(&mut output, &mut candidate_at_eof, line)?;
    }
    Ok(output)
}

fn range_index(range: diffy::HunkRange) -> Option<usize> {
    if range.is_empty() {
        Some(range.start())
    } else {
        range.start().checked_sub(1)
    }
}

fn push_candidate(
    output: &mut String,
    candidate_at_eof: &mut bool,
    value: &str,
) -> Result<(), ToolCallError> {
    if *candidate_at_eof {
        return Err(invalid_patch(
            "patch content cannot follow a no-newline end-of-file line",
        ));
    }
    if value.len() > MAX_MUTATION_FILE_LINE_BYTES
        || output
            .len()
            .checked_add(value.len())
            .is_none_or(|next| next > MAX_MUTATION_FILE_BYTES)
    {
        return Err(ToolCallError::model(
            "PatchError",
            "PATCH_TOO_LARGE",
            "the patched file exceeds the configured size or line limit",
        ));
    }
    output.push_str(value);
    *candidate_at_eof = !value.ends_with('\n');
    Ok(())
}

struct NormalizedText {
    normalized: String,
    line_endings: LineEndings,
}

#[derive(Clone, Copy)]
enum LineEndings {
    Lf,
    CrLf,
}

fn normalize_existing_text(raw: &[u8]) -> Result<NormalizedText, ToolCallError> {
    let text = std::str::from_utf8(raw).map_err(|_| {
        ToolCallError::model(
            "FsError",
            "FILE_NOT_TEXT",
            "the update target is not valid UTF-8 text",
        )
    })?;
    if text.contains('\0') {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_NOT_TEXT",
            "the update target contains NUL bytes",
        ));
    }
    let has_crlf = text.contains("\r\n");
    let without_crlf = text.replace("\r\n", "");
    if without_crlf.contains('\r') || (has_crlf && without_crlf.contains('\n')) {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_NOT_TEXT",
            "the update target has mixed or unsupported line endings",
        ));
    }
    let normalized = if has_crlf {
        text.replace("\r\n", "\n")
    } else {
        text.to_owned()
    };
    validate_file_text(&normalized)?;
    Ok(NormalizedText {
        normalized,
        line_endings: if has_crlf {
            LineEndings::CrLf
        } else {
            LineEndings::Lf
        },
    })
}

fn validate_file_text(value: &str) -> Result<(), ToolCallError> {
    if value
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(ToolCallError::model(
            "FsError",
            "FILE_NOT_TEXT",
            "the file contains an unsafe control character",
        ));
    }
    let mut lines = 0_usize;
    for line in value.split_inclusive('\n') {
        lines = lines.checked_add(1).ok_or_else(|| {
            ToolCallError::model("PatchError", "PATCH_TOO_LARGE", "file line count overflow")
        })?;
        if lines > MAX_MUTATION_FILE_LINES || line.len() > MAX_MUTATION_FILE_LINE_BYTES {
            return Err(ToolCallError::model(
                "PatchError",
                "PATCH_TOO_LARGE",
                "the file exceeds the configured line count or line length",
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CanonicalOp<'a> {
    Equal(&'a str),
    Delete(&'a str),
    Insert(&'a str),
}

impl<'a> CanonicalOp<'a> {
    fn is_equal(self) -> bool {
        matches!(self, Self::Equal(_))
    }

    fn consumes_old(self) -> bool {
        !matches!(self, Self::Insert(_))
    }

    fn consumes_new(self) -> bool {
        !matches!(self, Self::Delete(_))
    }

    fn rendered(self) -> (char, &'a str) {
        match self {
            Self::Equal(value) => (' ', value),
            Self::Delete(value) => ('-', value),
            Self::Insert(value) => ('+', value),
        }
    }
}

/// Build a three-context-line preview from the exact validated hunk
/// operations. This is linear in the bounded file/patch size and does not run
/// a fuzzy or potentially quadratic diff algorithm after approval facts have
/// been prepared.
fn canonical_diff<'a>(
    path: &str,
    operation: WorkspaceMutationOperation,
    before: &'a str,
    patch: &'a Patch<'_, str>,
    cancellation: &CancellationToken,
) -> Result<String, ToolCallError> {
    let original = before.split_inclusive('\n').collect::<Vec<_>>();
    let mut operations = Vec::new();
    operations
        .try_reserve(original.len().saturating_add(MAX_PATCH_LINES))
        .map_err(|_| ToolCallError::Infrastructure)?;
    let mut old_cursor = 0_usize;
    for hunk in patch.hunks() {
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        let old_index =
            range_index(hunk.old_range()).ok_or_else(|| invalid_patch("invalid old hunk range"))?;
        if old_index < old_cursor || old_index > original.len() {
            return Err(invalid_patch("hunk old range is outside the target"));
        }
        for line in &original[old_cursor..old_index] {
            operations.push(CanonicalOp::Equal(line));
        }
        let mut source = old_index;
        for line in hunk.lines() {
            if cancellation.is_cancelled() {
                return Err(ToolCallError::aborted());
            }
            match line {
                Line::Context(value) => {
                    if original.get(source).copied() != Some(*value) {
                        return Err(invalid_patch("patch context does not match the target"));
                    }
                    operations.push(CanonicalOp::Equal(value));
                    source = source
                        .checked_add(1)
                        .ok_or_else(|| invalid_patch("line count overflow"))?;
                }
                Line::Delete(value) => {
                    if original.get(source).copied() != Some(*value) {
                        return Err(invalid_patch("patch deletion does not match the target"));
                    }
                    operations.push(CanonicalOp::Delete(value));
                    source = source
                        .checked_add(1)
                        .ok_or_else(|| invalid_patch("line count overflow"))?;
                }
                Line::Insert(value) => operations.push(CanonicalOp::Insert(value)),
            }
        }
        old_cursor = source;
    }
    for line in &original[old_cursor..] {
        if cancellation.is_cancelled() {
            return Err(ToolCallError::aborted());
        }
        operations.push(CanonicalOp::Equal(line));
    }
    reject_redundant_edits(&operations)?;

    let mut clusters = Vec::<(usize, usize)>::new();
    clusters
        .try_reserve(patch.hunks().len())
        .map_err(|_| ToolCallError::Infrastructure)?;
    let mut equal_since_change = usize::MAX;
    for (index, operation) in operations.iter().copied().enumerate() {
        if operation.is_equal() {
            equal_since_change = equal_since_change.saturating_add(1);
        } else {
            if clusters.is_empty() || equal_since_change > 6 {
                clusters.push((index, index));
            } else if let Some(cluster) = clusters.last_mut() {
                cluster.1 = index;
            }
            equal_since_change = 0;
        }
    }
    if clusters.is_empty() {
        return Err(ToolCallError::model(
            "PatchError",
            "NO_CHANGES",
            "the patch does not contain a material line operation",
        ));
    }

    let mut old_prefix = Vec::new();
    let mut new_prefix = Vec::new();
    old_prefix
        .try_reserve(operations.len().saturating_add(1))
        .map_err(|_| ToolCallError::Infrastructure)?;
    new_prefix
        .try_reserve(operations.len().saturating_add(1))
        .map_err(|_| ToolCallError::Infrastructure)?;
    old_prefix.push(0_usize);
    new_prefix.push(0_usize);
    for operation in operations.iter().copied() {
        old_prefix.push(
            old_prefix
                .last()
                .copied()
                .unwrap_or_default()
                .checked_add(usize::from(operation.consumes_old()))
                .ok_or_else(|| invalid_patch("old line count overflow"))?,
        );
        new_prefix.push(
            new_prefix
                .last()
                .copied()
                .unwrap_or_default()
                .checked_add(usize::from(operation.consumes_new()))
                .ok_or_else(|| invalid_patch("new line count overflow"))?,
        );
    }

    let mut output = String::new();
    let original_header = match operation {
        WorkspaceMutationOperation::Create => "--- /dev/null\n".to_owned(),
        WorkspaceMutationOperation::Update => format!("--- a/{path}\n"),
    };
    push_diff_output(&mut output, &original_header)?;
    push_diff_output(&mut output, &format!("+++ b/{path}\n"))?;

    for (first_change, last_change) in clusters {
        let mut start = first_change;
        let mut leading = 0_usize;
        while start > 0 && leading < 3 {
            start -= 1;
            if operations[start].is_equal() {
                leading += 1;
            }
        }
        let mut end = last_change + 1;
        let mut trailing = 0_usize;
        while end < operations.len() && trailing < 3 {
            if operations[end].is_equal() {
                trailing += 1;
            }
            end += 1;
        }
        let old_len = old_prefix[end] - old_prefix[start];
        let new_len = new_prefix[end] - new_prefix[start];
        let old_range = format_hunk_range(old_prefix[start], old_len);
        let new_range = format_hunk_range(new_prefix[start], new_len);
        push_diff_output(&mut output, &format!("@@ -{old_range} +{new_range} @@\n"))?;
        for operation in operations[start..end].iter().copied() {
            if cancellation.is_cancelled() {
                return Err(ToolCallError::aborted());
            }
            let (prefix, value) = operation.rendered();
            if prefix == ' ' && value == "\n" {
                push_diff_output(&mut output, value)?;
            } else {
                push_diff_output(&mut output, &prefix.to_string())?;
                push_diff_output(&mut output, value)?;
            }
            if !value.ends_with('\n') {
                push_diff_output(&mut output, "\n\\ No newline at end of file\n")?;
            }
        }
    }
    Ok(output)
}

fn reject_redundant_edits(operations: &[CanonicalOp<'_>]) -> Result<(), ToolCallError> {
    let mut deleted = BTreeSet::new();
    let mut inserted = BTreeSet::new();
    for operation in operations.iter().copied() {
        match operation {
            CanonicalOp::Equal(_) => {
                deleted.clear();
                inserted.clear();
            }
            CanonicalOp::Delete(value) => {
                if value.is_empty() || inserted.contains(value) {
                    return Err(invalid_patch(
                        "one edit run redundantly deletes and inserts the same line",
                    ));
                }
                deleted.insert(value);
            }
            CanonicalOp::Insert(value) => {
                if value.is_empty() || deleted.contains(value) {
                    return Err(invalid_patch(
                        "one edit run redundantly deletes and inserts the same line",
                    ));
                }
                inserted.insert(value);
            }
        }
    }
    Ok(())
}

fn format_hunk_range(prefix_lines: usize, length: usize) -> String {
    let start = if length == 0 {
        prefix_lines
    } else {
        prefix_lines + 1
    };
    if length == 1 {
        start.to_string()
    } else {
        format!("{start},{length}")
    }
}

fn push_diff_output(output: &mut String, value: &str) -> Result<(), ToolCallError> {
    if output
        .len()
        .checked_add(value.len())
        .is_none_or(|next| next > crate::agent::MAX_APPROVAL_PREVIEW_BYTES)
    {
        return Err(ToolCallError::model(
            "PatchError",
            "DIFF_TOO_LARGE",
            "the complete canonical approval diff exceeds the configured limit",
        ));
    }
    output.push_str(value);
    Ok(())
}

fn mutation_meta(
    path: &str,
    operation: WorkspaceMutationOperation,
    diff: &str,
    committed: bool,
    cleanup_warning: bool,
) -> Result<JsonValue, ToolExecutorError> {
    let mut value = json!({
        "path": path,
        "operation": operation_name(operation),
        "diff": diff,
        "committed": committed
    });
    if cleanup_warning {
        let Some(fields) = value.as_object_mut() else {
            return Err(ToolExecutorError::new(
                "file mutation metadata normalization failed",
            ));
        };
        fields.insert("cleanupWarning".to_owned(), Value::Bool(true));
    }
    JsonValue::new(value)
        .map_err(|_| ToolExecutorError::new("file mutation metadata normalization failed"))
}

fn success_result(
    path: &str,
    operation: WorkspaceMutationOperation,
    diff: &str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    ToolExecutionResult::new(
        vec![
            ContentBlock::text(format!(
                "{} workspace file `{path}`.",
                match operation {
                    WorkspaceMutationOperation::Create => "Created",
                    WorkspaceMutationOperation::Update => "Updated",
                }
            ))
            .map_err(|_| ToolExecutorError::new("file mutation result normalization failed"))?,
        ],
        false,
        None,
        Some(mutation_meta(path, operation, diff, true, false)?),
        false,
    )
    .map_err(|_| ToolExecutorError::new("file mutation result normalization failed"))
}

#[allow(clippy::too_many_arguments)]
fn failure_result(
    path: &str,
    operation: WorkspaceMutationOperation,
    diff: &str,
    committed: bool,
    name: &str,
    code: &str,
    message: &str,
    cleanup_warning: bool,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    ToolExecutionResult::new(
        vec![
            ContentBlock::text(format!("Error: {message}"))
                .map_err(|_| ToolExecutorError::new("file mutation result normalization failed"))?,
        ],
        true,
        Some(ToolFailure {
            name: name.to_owned(),
            code: code.to_owned(),
        }),
        Some(mutation_meta(
            path,
            operation,
            diff,
            committed,
            cleanup_warning,
        )?),
        false,
    )
    .map_err(|_| ToolExecutorError::new("file mutation result normalization failed"))
}

fn decline_result(
    path: &str,
    operation: WorkspaceMutationOperation,
    diff: &str,
    reason: MutationDeclineReason,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let (name, code, message) = match reason {
        MutationDeclineReason::PolicyDenied => (
            "PolicyError",
            "POLICY_DENIED",
            "file changes are disabled by policy",
        ),
        MutationDeclineReason::ApprovalRejected => (
            "ApprovalError",
            "APPROVAL_REJECTED",
            "the file change was rejected",
        ),
        MutationDeclineReason::ApprovalCancelled => (
            "AbortError",
            "APPROVAL_CANCELLED",
            "the approval request was cancelled",
        ),
        MutationDeclineReason::ApprovalUnavailable => (
            "ApprovalError",
            "APPROVAL_UNAVAILABLE",
            "approval was unavailable, so the file was not changed",
        ),
        MutationDeclineReason::AbortedBeforeDispatch => (
            "AbortError",
            "ABORTED_BEFORE_DISPATCH",
            "the file change was cancelled before commit",
        ),
        MutationDeclineReason::Aborted => (
            "AbortError",
            "ABORTED",
            "the file change was cancelled before publication",
        ),
        MutationDeclineReason::OutputBudgetExceeded => (
            "ToolOutputError",
            "TOOL_OUTPUT_BUDGET_EXCEEDED",
            "the file change result could not fit safely in the session",
        ),
    };
    failure_result(path, operation, diff, false, name, code, message, false)
}

fn invalid_patch(message: &'static str) -> ToolCallError {
    ToolCallError::model("PatchError", "INVALID_PATCH", message)
}

fn operation_name(operation: WorkspaceMutationOperation) -> &'static str {
    match operation {
        WorkspaceMutationOperation::Create => "create",
        WorkspaceMutationOperation::Update => "update",
    }
}

fn operation_verb(operation: WorkspaceMutationOperation) -> &'static str {
    match operation {
        WorkspaceMutationOperation::Create => "Create",
        WorkspaceMutationOperation::Update => "Update",
    }
}

fn approval_reason(operation: WorkspaceMutationOperation, path: &str) -> Option<String> {
    let reason = format!("{} workspace file `{path}`", operation_verb(operation));
    (reason.len() <= crate::agent::MAX_APPROVAL_REASON_BYTES).then_some(reason)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        MAX_CANONICAL_DIFF_JSON_BYTES, MAX_DIRECTORY_DEPTH, MAX_MUTATION_FILE_LINE_BYTES,
        MAX_MUTATION_FILE_LINES, MAX_PATCH_BYTES, MAX_PATCH_HUNKS, MAX_PATCH_LINE_BYTES,
        MAX_PATCH_LINES, WorkspaceMutationOperation, apply_strict, approval_reason, canonical_diff,
        mutation_meta, parse_arguments, parse_patch, push_diff_output, validate_file_text,
    };
    use tokio_util::sync::CancellationToken;

    #[test]
    fn strict_application_rejects_offset_matching_and_trailing_garbage() {
        let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-second\n+changed\n";
        let parsed = parse_patch(patch).unwrap();
        assert!(apply_strict("first\nsecond\n", &parsed.patch, &CancellationToken::new()).is_err());

        assert!(parse_patch(&format!("{patch}trailing garbage\n")).is_err());
    }

    #[test]
    fn canonical_preview_preserves_no_newline_markers() {
        let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -2 +2 @@\n-two\n\\ No newline at end of file\n+TWO\n\\ No newline at end of file\n";
        let parsed = parse_patch(patch).unwrap();
        let cancellation = CancellationToken::new();
        assert_eq!(
            apply_strict("one\ntwo", &parsed.patch, &cancellation).unwrap(),
            "one\nTWO"
        );
        assert_eq!(
            canonical_diff(
                &parsed.path,
                parsed.operation,
                "one\ntwo",
                &parsed.patch,
                &cancellation,
            )
            .unwrap(),
            "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n one\n-two\n\\ No newline at end of file\n+TWO\n\\ No newline at end of file\n"
        );

        for invalid in [
            "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n-old\n+new\n\\ No newline at end of file\n next\n",
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n@@ -2 +2 @@\n-two\n+TWO\n",
        ] {
            assert!(
                parse_patch(invalid).is_err(),
                "accepted a non-final no-newline marker: {invalid:?}"
            );
        }

        let hidden_tail = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n";
        let parsed = parse_patch(hidden_tail).unwrap();
        assert!(
            apply_strict("old\nnext\n", &parsed.patch, &cancellation).is_err(),
            "a no-newline replacement must not absorb an unchanged file tail"
        );
    }

    #[test]
    fn canonical_preview_rejects_redundant_edits_and_is_self_parseable_with_blank_context() {
        let redundant = "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n-a\n+a\n-b\n+B\n";
        let parsed = parse_patch(redundant).unwrap();
        let cancellation = CancellationToken::new();
        assert!(
            canonical_diff(
                &parsed.path,
                parsed.operation,
                "a\nb\n",
                &parsed.patch,
                &cancellation,
            )
            .is_err()
        );

        let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -3 +3 @@\n-three\n+THREE\n";
        let parsed = parse_patch(patch).unwrap();
        let preview = canonical_diff(
            &parsed.path,
            parsed.operation,
            "one\n\nthree\n",
            &parsed.patch,
            &cancellation,
        )
        .unwrap();
        assert_eq!(
            preview,
            "--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n one\n\n-three\n+THREE\n"
        );
        let reparsed = parse_patch(&preview).unwrap();
        assert_eq!(
            apply_strict("one\n\nthree\n", &reparsed.patch, &cancellation).unwrap(),
            "one\n\nTHREE\n"
        );
    }

    #[test]
    fn strict_parser_accepts_header_like_content_and_rejects_unsupported_dialects() {
        let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1,2 +1,2 @@\n---- heading\n-+++ heading\n+--- HEADING\n++++ HEADING\n";
        let parsed = parse_patch(patch).unwrap();
        assert_eq!(
            apply_strict(
                "--- heading\n+++ heading\n",
                &parsed.patch,
                &CancellationToken::new(),
            )
            .unwrap(),
            "--- HEADING\n+++ HEADING\n"
        );

        for invalid in [
            "--- a/file.txt\t2026-01-01\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            "--- \"a/file.txt\"\n+++ \"b/file.txt\"\n@@ -1 +1 @@\n-old\n+new\n",
            "--- file.txt\n+++ file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            "--- a/file.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-old\n",
            "--- a/old.txt\n+++ b/new.txt\n@@ -1 +1 @@\n-old\n+new\n",
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\ntrailing\n",
            "--- a/file.txt\n+++ b/file.txt/\n@@ -1 +1 @@\n-old\n+new\n",
            "--- a/dir\\file.txt\n+++ b/dir\\file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            "--- a//tmp/file.txt\n+++ b//tmp/file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\nextra\n",
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\u{001b}\n",
        ] {
            assert!(
                parse_patch(invalid).is_err(),
                "unexpectedly accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn mutation_path_depth_accepts_64_components_and_rejects_65() {
        let exact_path = vec!["directory"; MAX_DIRECTORY_DEPTH].join("/");
        assert_eq!(exact_path.split('/').count(), 64);
        let exact_patch =
            format!("--- a/{exact_path}\n+++ b/{exact_path}\n@@ -1 +1 @@\n-old\n+new\n");
        assert!(parse_patch(&exact_patch).is_ok());

        let over_path = format!("{exact_path}/file.txt");
        assert_eq!(over_path.split('/').count(), 65);
        let over_patch = format!("--- a/{over_path}\n+++ b/{over_path}\n@@ -1 +1 @@\n-old\n+new\n");
        let Err(error) = parse_patch(&over_patch) else {
            panic!("a mutation path with 65 components must be rejected");
        };
        assert!(error.has_code("WORKSPACE_PATH_DENIED"));
    }

    #[test]
    fn mutation_path_limit_counts_utf8_encoded_bytes() {
        let exact_path = format!("{}a", "界".repeat((4_096 - 1) / "界".len()));
        assert_eq!(exact_path.len(), 4_096);
        assert!(exact_path.chars().count() < exact_path.len());
        let exact_patch =
            format!("--- a/{exact_path}\n+++ b/{exact_path}\n@@ -1 +1 @@\n-old\n+new\n");
        assert!(parse_patch(&exact_patch).is_ok());
        assert!(approval_reason(WorkspaceMutationOperation::Update, &exact_path).is_none());
        assert_eq!(
            approval_reason(WorkspaceMutationOperation::Update, "file.txt").as_deref(),
            Some("Update workspace file `file.txt`")
        );

        let over_path = format!("{exact_path}b");
        assert_eq!(over_path.len(), 4_097);
        let over_patch = format!("--- a/{over_path}\n+++ b/{over_path}\n@@ -1 +1 @@\n-old\n+new\n");
        let Err(error) = parse_patch(&over_patch) else {
            panic!("a mutation path with 4,097 UTF-8 bytes must be rejected");
        };
        assert!(error.has_code("WORKSPACE_PATH_DENIED"));
    }

    #[test]
    fn worst_case_mutation_meta_counts_compact_json_at_exact_and_one_over() {
        let base = mutation_meta("p", WorkspaceMutationOperation::Update, "", false, true).unwrap();
        let exact_diff_bytes = MAX_CANONICAL_DIFF_JSON_BYTES
            .checked_sub(base.encoded_len())
            .unwrap();

        let exact = mutation_meta(
            "p",
            WorkspaceMutationOperation::Update,
            &"x".repeat(exact_diff_bytes),
            false,
            true,
        )
        .unwrap();
        assert_eq!(exact.as_value()["cleanupWarning"], true);
        assert_eq!(exact.as_value()["committed"], false);
        assert_eq!(exact.encoded_len(), MAX_CANONICAL_DIFF_JSON_BYTES);
        assert_eq!(
            serde_json::to_vec(exact.as_value()).unwrap().len(),
            MAX_CANONICAL_DIFF_JSON_BYTES
        );

        let over = mutation_meta(
            "p",
            WorkspaceMutationOperation::Update,
            &"x".repeat(exact_diff_bytes + 1),
            false,
            true,
        )
        .unwrap();
        assert_eq!(over.encoded_len(), MAX_CANONICAL_DIFF_JSON_BYTES + 1);
        assert_eq!(
            serde_json::to_vec(over.as_value()).unwrap().len(),
            MAX_CANONICAL_DIFF_JSON_BYTES + 1
        );
    }

    #[test]
    fn patch_and_file_resource_boundaries_accept_exactly_the_limit() {
        assert!(parse_arguments(&json!({ "patch": "x".repeat(MAX_PATCH_BYTES) })).is_ok());
        assert!(parse_arguments(&json!({ "patch": "x".repeat(MAX_PATCH_BYTES + 1) })).is_err());

        let exact_line = format!(
            "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1 @@\n+{}\n",
            "x".repeat(MAX_PATCH_LINE_BYTES - 2)
        );
        assert!(parse_patch(&exact_line).is_ok());
        let over_line = format!(
            "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1 @@\n+{}\n",
            "x".repeat(MAX_PATCH_LINE_BYTES - 1)
        );
        assert!(parse_patch(&over_line).is_err());

        let mut exact_lines = String::from("--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1,99997 @@\n");
        exact_lines.push_str(&"+\n".repeat(MAX_PATCH_LINES - 3));
        assert!(parse_patch(&exact_lines).is_ok());
        exact_lines.push_str("+\n");
        assert!(parse_patch(&exact_lines).is_err());

        let mut exact_hunks = String::from("--- /dev/null\n+++ b/file.txt\n");
        for line in 1..=MAX_PATCH_HUNKS {
            exact_hunks.push_str(&format!("@@ -0,0 +{line} @@\n+x\n"));
        }
        assert!(parse_patch(&exact_hunks).is_ok());
        exact_hunks.push_str(&format!("@@ -0,0 +{} @@\n+x\n", MAX_PATCH_HUNKS + 1));
        assert!(parse_patch(&exact_hunks).is_err());

        assert!(validate_file_text(&"\n".repeat(MAX_MUTATION_FILE_LINES)).is_ok());
        assert!(validate_file_text(&"\n".repeat(MAX_MUTATION_FILE_LINES + 1)).is_err());
        assert!(
            validate_file_text(&format!(
                "{}\n",
                "x".repeat(MAX_MUTATION_FILE_LINE_BYTES - 1)
            ))
            .is_ok()
        );
        assert!(
            validate_file_text(&format!("{}\n", "x".repeat(MAX_MUTATION_FILE_LINE_BYTES))).is_err()
        );

        let mut diff = String::new();
        assert!(push_diff_output(&mut diff, &"x".repeat(MAX_CANONICAL_DIFF_JSON_BYTES)).is_ok());
        assert!(push_diff_output(&mut diff, "x").is_err());
    }
}
