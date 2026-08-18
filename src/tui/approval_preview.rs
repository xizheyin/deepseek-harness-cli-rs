//! Semantic presentation for trusted, locally generated approval previews.
//!
//! This module never decides whether a preview is a patch. It only accepts the
//! closed provenance produced by the built-in patch preparation path.

use std::{fmt, fmt::Write as _};

use crate::agent::{ApprovalDiffRowKind, ApprovalPatchOperation, CanonicalPatchApproval};

use super::{
    presentation::{PresentationError, PresentedChunkBuilder, TextStyle},
    visible::{VisibleTextError, render_visible_owned},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalPreviewError {
    Capacity,
    Limit,
    InvalidState,
}

impl From<PresentationError> for ApprovalPreviewError {
    fn from(value: PresentationError) -> Self {
        match value {
            PresentationError::Capacity => Self::Capacity,
            PresentationError::Limit => Self::Limit,
            PresentationError::InvalidText => Self::InvalidState,
        }
    }
}

impl From<VisibleTextError> for ApprovalPreviewError {
    fn from(value: VisibleTextError) -> Self {
        match value {
            VisibleTextError::Capacity => Self::Capacity,
            VisibleTextError::Limit => Self::Limit,
        }
    }
}

pub(crate) fn present_canonical_patch(
    builder: &mut PresentedChunkBuilder,
    facts: &CanonicalPatchApproval,
    source: &str,
) -> Result<(), ApprovalPreviewError> {
    let visible_source = render_visible_owned(source, true)?;
    let visible_path = render_visible_owned(facts.path(), false)?;
    if !visible_source.ends_with('\n')
        || visible_source.bytes().filter(|byte| *byte == b'\n').count() != facts.rows().len()
    {
        return Err(ApprovalPreviewError::InvalidState);
    }

    builder.push_text(TextStyle::Warning, "Proposed ")?;
    builder.push_text(
        TextStyle::Warning,
        match facts.operation() {
            ApprovalPatchOperation::Create => "create",
            ApprovalPatchOperation::Update => "update",
        },
    )?;
    builder.push_text(TextStyle::Muted, " · not applied")?;
    builder.push_line_feed()?;
    builder.push_text(TextStyle::Plain, "  ")?;
    builder.push_text(TextStyle::Plain, &visible_path)?;
    builder.push_text(TextStyle::Muted, " · ")?;
    let mut statistics = String::new();
    statistics
        .try_reserve(96)
        .map_err(|_| ApprovalPreviewError::Capacity)?;
    write!(
        &mut statistics,
        "+{} -{} · {} hunk{}",
        facts.additions(),
        facts.removals(),
        facts.hunks(),
        if facts.hunks() == 1 { "" } else { "s" },
    )
    .map_err(|_| ApprovalPreviewError::Capacity)?;
    builder.push_text(TextStyle::Muted, &statistics)?;
    builder.push_line_feed()?;
    builder.push_text(TextStyle::Muted, "  One workspace file · no shell command")?;
    builder.push_line_feed()?;
    builder.push_line_feed()?;

    let mut lines = visible_source.split_inclusive('\n');
    for kind in facts.rows() {
        let line = lines.next().ok_or(ApprovalPreviewError::InvalidState)?;
        let content = line
            .strip_suffix('\n')
            .ok_or(ApprovalPreviewError::InvalidState)?;
        builder.push_text(style_for_row(*kind), content)?;
        builder.push_line_feed()?;
    }
    if lines.next().is_some() {
        return Err(ApprovalPreviewError::InvalidState);
    }
    Ok(())
}

const fn style_for_row(kind: ApprovalDiffRowKind) -> TextStyle {
    match kind {
        ApprovalDiffRowKind::FileHeader => TextStyle::DiffHeader,
        ApprovalDiffRowKind::Hunk => TextStyle::DiffHunk,
        ApprovalDiffRowKind::Context => TextStyle::Code,
        ApprovalDiffRowKind::Addition => TextStyle::DiffAdd,
        ApprovalDiffRowKind::Removal => TextStyle::DiffRemove,
        ApprovalDiffRowKind::NoNewline => TextStyle::Muted,
    }
}

impl fmt::Display for ApprovalPreviewError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Capacity => "CLI_OUTPUT_CAPACITY",
            Self::Limit => "CLI_OUTPUT_LIMIT",
            Self::InvalidState => "CLI_OUTPUT_STATE",
        })
    }
}
