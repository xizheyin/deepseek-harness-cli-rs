use std::{future::Future, pin::Pin, sync::Arc};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    model::CallId,
    session::{ApprovalOutcome, ApprovalRequestId},
};

pub const MAX_APPROVAL_PREVIEW_BYTES: usize = 64 * 1024;
pub const MAX_APPROVAL_REASON_BYTES: usize = 4 * 1024;
pub(crate) const MAX_APPROVAL_PATCH_PATH_BYTES: usize = 4 * 1024;

/// Closed operation facts produced by the built-in patch preparation path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ApprovalPatchOperation {
    Create,
    Update,
}

/// One byte of provenance for each physical row in a canonical patch preview.
///
/// The renderer must not infer these roles from text prefixes: hunk content can
/// itself begin with strings such as `--- a/` or `+++ b/`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ApprovalDiffRowKind {
    FileHeader,
    Hunk,
    Context,
    Addition,
    Removal,
    NoNewline,
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct CanonicalPatchApproval {
    operation: ApprovalPatchOperation,
    path: Arc<str>,
    rows: Arc<[ApprovalDiffRowKind]>,
    additions: usize,
    removals: usize,
    hunks: usize,
}

impl std::fmt::Debug for CanonicalPatchApproval {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CanonicalPatchApproval")
            .field("operation", &self.operation)
            .field("path_bytes", &self.path.len())
            .field("rows", &self.rows.len())
            .field("additions", &self.additions)
            .field("removals", &self.removals)
            .field("hunks", &self.hunks)
            .finish()
    }
}

impl CanonicalPatchApproval {
    #[must_use]
    pub(crate) const fn operation(&self) -> ApprovalPatchOperation {
        self.operation
    }

    #[must_use]
    pub(crate) fn path(&self) -> &str {
        &self.path
    }

    #[must_use]
    pub(crate) fn rows(&self) -> &[ApprovalDiffRowKind] {
        &self.rows
    }

    #[must_use]
    pub(crate) const fn additions(&self) -> usize {
        self.additions
    }

    #[must_use]
    pub(crate) const fn removals(&self) -> usize {
        self.removals
    }

    #[must_use]
    pub(crate) const fn hunks(&self) -> usize {
        self.hunks
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum ApprovalPreviewKind {
    Opaque,
    CanonicalPatch(Arc<CanonicalPatchApproval>),
}

impl std::fmt::Debug for ApprovalPreviewKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Opaque => formatter.write_str("Opaque"),
            Self::CanonicalPatch(value) => value.fmt(formatter),
        }
    }
}

/// Static Phase 5 decision applied to every prepared file mutation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum FileChangePolicy {
    Allow,
    Deny,
    #[default]
    Ask,
}

/// Static Phase 6 decision applied to every prepared foreground shell action.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShellPolicy {
    Allow,
    Deny,
    #[default]
    Ask,
}

/// Static Phase 10 decision applied to every configured plugin action.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PluginPolicy {
    Allow,
    Deny,
    #[default]
    Ask,
}

/// Bounded, immutable question retained by one prepared mutation.
#[derive(Clone, Eq, PartialEq)]
pub struct ApprovalPrompt {
    reason: Option<String>,
    preview: Arc<str>,
    preview_kind: ApprovalPreviewKind,
}

impl std::fmt::Debug for ApprovalPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApprovalPrompt")
            .field("reason_present", &self.reason.is_some())
            .field("reason_bytes", &self.reason.as_ref().map_or(0, String::len))
            .field("preview_bytes", &self.preview.len())
            .field("preview_kind", &self.preview_kind)
            .finish()
    }
}

impl ApprovalPrompt {
    pub fn new(
        reason: Option<String>,
        preview: impl Into<String>,
    ) -> Result<Self, ApprovalPromptError> {
        let preview = preview.into();
        validate_prompt(&reason, &preview)?;
        Ok(Self {
            reason,
            preview: Arc::from(preview),
            preview_kind: ApprovalPreviewKind::Opaque,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn canonical_patch(
        reason: Option<String>,
        preview: String,
        operation: ApprovalPatchOperation,
        path: String,
        rows: Vec<ApprovalDiffRowKind>,
        additions: usize,
        removals: usize,
        hunks: usize,
    ) -> Result<Self, ApprovalPromptError> {
        validate_prompt(&reason, &preview)?;
        let line_count = preview.bytes().filter(|byte| *byte == b'\n').count();
        let row_additions = rows
            .iter()
            .filter(|row| **row == ApprovalDiffRowKind::Addition)
            .count();
        let row_removals = rows
            .iter()
            .filter(|row| **row == ApprovalDiffRowKind::Removal)
            .count();
        let row_hunks = rows
            .iter()
            .filter(|row| **row == ApprovalDiffRowKind::Hunk)
            .count();
        let mut preview_lines = preview.split_inclusive('\n');
        let original_header = preview_lines.next();
        let modified_header = preview_lines.next();
        let headers_match = match operation {
            ApprovalPatchOperation::Create => original_header == Some("--- /dev/null\n"),
            ApprovalPatchOperation::Update => {
                original_header
                    .and_then(|line| line.strip_prefix("--- a/"))
                    .and_then(|line| line.strip_suffix('\n'))
                    == Some(path.as_str())
            }
        } && modified_header
            .and_then(|line| line.strip_prefix("+++ b/"))
            .and_then(|line| line.strip_suffix('\n'))
            == Some(path.as_str());
        if path.is_empty()
            || path.len() > MAX_APPROVAL_PATCH_PATH_BYTES
            || path.chars().any(char::is_control)
            || !preview.ends_with('\n')
            || line_count != rows.len()
            || rows.len() < 3
            || rows.first() != Some(&ApprovalDiffRowKind::FileHeader)
            || rows.get(1) != Some(&ApprovalDiffRowKind::FileHeader)
            || rows[2..].contains(&ApprovalDiffRowKind::FileHeader)
            || !headers_match
            || additions != row_additions
            || removals != row_removals
            || hunks == 0
            || hunks != row_hunks
        {
            return Err(ApprovalPromptError::InvalidCanonicalPatch);
        }
        Ok(Self {
            reason,
            preview: Arc::from(preview),
            preview_kind: ApprovalPreviewKind::CanonicalPatch(Arc::new(CanonicalPatchApproval {
                operation,
                path: Arc::from(path),
                rows: Arc::from(rows),
                additions,
                removals,
                hunks,
            })),
        })
    }

    #[must_use]
    pub(crate) fn preview_arc(&self) -> Arc<str> {
        Arc::clone(&self.preview)
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }
}

fn validate_prompt(reason: &Option<String>, preview: &str) -> Result<(), ApprovalPromptError> {
    if preview.is_empty() || preview.len() > MAX_APPROVAL_PREVIEW_BYTES {
        return Err(ApprovalPromptError::InvalidPreview {
            maximum: MAX_APPROVAL_PREVIEW_BYTES,
            actual: preview.len(),
        });
    }
    if let Some(value) = reason.as_ref() {
        if value.len() > MAX_APPROVAL_REASON_BYTES {
            return Err(ApprovalPromptError::InvalidReason {
                maximum: MAX_APPROVAL_REASON_BYTES,
                actual: value.len(),
            });
        }
        if value
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        {
            return Err(ApprovalPromptError::InvalidReasonCharacters);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ApprovalPromptError {
    #[error("approval preview is {actual} bytes; expected 1 to {maximum}")]
    InvalidPreview { maximum: usize, actual: usize },
    #[error("approval reason is {actual} bytes; maximum is {maximum}")]
    InvalidReason { maximum: usize, actual: usize },
    #[error("approval reason contains an unsafe control character")]
    InvalidReasonCharacters,
    #[error("canonical patch approval presentation is inconsistent")]
    InvalidCanonicalPatch,
}

/// Owned request passed to the approval UI without filesystem authority.
#[derive(Clone, Eq, PartialEq)]
pub struct ApprovalRequest {
    id: ApprovalRequestId,
    tool_name: String,
    call_id: CallId,
    reason: Option<String>,
    preview: Arc<str>,
    preview_kind: ApprovalPreviewKind,
}

impl std::fmt::Debug for ApprovalRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApprovalRequest")
            .field("id_bytes", &self.id.as_str().len())
            .field("tool_name_bytes", &self.tool_name.len())
            .field("call_id_bytes", &self.call_id.as_str().len())
            .field("reason_present", &self.reason.is_some())
            .field("reason_bytes", &self.reason.as_ref().map_or(0, String::len))
            .field("preview_bytes", &self.preview.len())
            .field("preview_kind", &self.preview_kind)
            .finish()
    }
}

impl ApprovalRequest {
    pub(crate) fn new(
        id: ApprovalRequestId,
        tool_name: String,
        call_id: CallId,
        prompt: &ApprovalPrompt,
    ) -> Self {
        Self {
            id,
            tool_name,
            call_id,
            reason: prompt.reason.clone(),
            preview: Arc::clone(&prompt.preview),
            preview_kind: prompt.preview_kind.clone(),
        }
    }

    #[must_use]
    pub fn id(&self) -> &ApprovalRequestId {
        &self.id
    }

    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    #[must_use]
    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    #[must_use]
    pub fn preview(&self) -> &str {
        &self.preview
    }

    #[must_use]
    pub(crate) fn preview_kind(&self) -> &ApprovalPreviewKind {
        &self.preview_kind
    }

    #[must_use]
    pub(crate) fn preview_arc(&self) -> Arc<str> {
        Arc::clone(&self.preview)
    }
}

/// Future returned by one approval provider.
pub type ApprovalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ApprovalOutcome, ApprovalProviderError>> + Send + 'a>>;

/// Fail-closed user-decision boundary. Extension text is never persisted.
pub trait ApprovalProvider: Send + Sync {
    /// Return promptly with a lazy future. The future must own and clean up any
    /// work it starts and cooperate with the supplied child token. Preview text
    /// is untrusted model input; terminal implementations must render it as
    /// escaped text rather than interpreting terminal control sequences.
    fn request(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_>;
}

/// Opaque approval-service infrastructure failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("approval provider failed")]
pub struct ApprovalProviderError;

impl ApprovalProviderError {
    #[must_use]
    pub fn new(_message: impl Into<String>) -> Self {
        Self
    }
}

/// Default provider used before a terminal approval UI is installed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoApprovalProvider;

impl ApprovalProvider for NoApprovalProvider {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        Box::pin(async { Ok(ApprovalOutcome::Unavailable) })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::{model::CallId, session::ApprovalRequestId};

    use super::{
        ApprovalDiffRowKind, ApprovalPatchOperation, ApprovalPreviewKind, ApprovalPrompt,
        ApprovalPromptError, ApprovalRequest, MAX_APPROVAL_PATCH_PATH_BYTES,
    };

    const DIFF: &str =
        "--- a/secret.txt\n+++ b/secret.txt\n@@ -1 +1 @@\n-old-secret\n+new-secret\n";

    fn rows() -> Vec<ApprovalDiffRowKind> {
        vec![
            ApprovalDiffRowKind::FileHeader,
            ApprovalDiffRowKind::FileHeader,
            ApprovalDiffRowKind::Hunk,
            ApprovalDiffRowKind::Removal,
            ApprovalDiffRowKind::Addition,
        ]
    }

    #[test]
    fn generic_diff_lookalike_stays_opaque_while_canonical_facts_are_closed() {
        let opaque = ApprovalPrompt::new(Some("SECRET_REASON".to_owned()), DIFF).unwrap();
        assert!(matches!(opaque.preview_kind, ApprovalPreviewKind::Opaque));

        let canonical = ApprovalPrompt::canonical_patch(
            Some("SECRET_REASON".to_owned()),
            DIFF.to_owned(),
            ApprovalPatchOperation::Update,
            "secret.txt".to_owned(),
            rows(),
            1,
            1,
            1,
        )
        .unwrap();
        let prompt_source = canonical.preview_arc();
        let request = ApprovalRequest::new(
            ApprovalRequestId::new("approval-secret"),
            "apply_patch".to_owned(),
            CallId::new("call-secret"),
            &canonical,
        );
        let request_source = request.preview_arc();
        assert!(Arc::ptr_eq(&prompt_source, &request_source));
        let ApprovalPreviewKind::CanonicalPatch(facts) = request.preview_kind() else {
            panic!("canonical patch provenance was lost");
        };
        assert_eq!(facts.operation(), ApprovalPatchOperation::Update);
        assert_eq!(facts.path(), "secret.txt");
        assert_eq!(facts.rows(), rows());
        assert_eq!(
            (facts.additions(), facts.removals(), facts.hunks()),
            (1, 1, 1)
        );

        for debug in [
            format!("{canonical:?}"),
            format!("{request:?}"),
            format!("{:?}", request.preview_kind()),
        ] {
            assert!(!debug.contains("SECRET_REASON"));
            assert!(!debug.contains("secret.txt"));
            assert!(!debug.contains("old-secret"));
            assert!(!debug.contains("call-secret"));
        }
    }

    #[test]
    fn canonical_patch_constructor_rejects_inconsistent_or_over_limit_facts() {
        let build = |path: String, rows: Vec<ApprovalDiffRowKind>, additions, removals, hunks| {
            ApprovalPrompt::canonical_patch(
                None,
                DIFF.to_owned(),
                ApprovalPatchOperation::Update,
                path,
                rows,
                additions,
                removals,
                hunks,
            )
        };
        assert!(build("secret.txt".to_owned(), rows(), 1, 1, 1).is_ok());
        assert_eq!(
            build("other.txt".to_owned(), rows(), 1, 1, 1),
            Err(ApprovalPromptError::InvalidCanonicalPatch)
        );
        assert_eq!(
            build("secret.txt".to_owned(), rows(), 2, 1, 1),
            Err(ApprovalPromptError::InvalidCanonicalPatch)
        );
        let mut missing_row = rows();
        missing_row.pop();
        assert_eq!(
            build("secret.txt".to_owned(), missing_row, 1, 1, 1),
            Err(ApprovalPromptError::InvalidCanonicalPatch)
        );
        let mut extra_header = rows();
        extra_header[3] = ApprovalDiffRowKind::FileHeader;
        assert_eq!(
            build("secret.txt".to_owned(), extra_header, 1, 0, 1),
            Err(ApprovalPromptError::InvalidCanonicalPatch)
        );
        assert_eq!(
            build("secret\n.txt".to_owned(), rows(), 1, 1, 1),
            Err(ApprovalPromptError::InvalidCanonicalPatch)
        );
        assert_eq!(
            build(
                "x".repeat(MAX_APPROVAL_PATCH_PATH_BYTES + 1),
                rows(),
                1,
                1,
                1,
            ),
            Err(ApprovalPromptError::InvalidCanonicalPatch)
        );

        let base = "--- a/p\n+++ b/p\n@@ -1 +1 @@\n";
        let context_rows = super::MAX_APPROVAL_PREVIEW_BYTES - base.len();
        let exact = format!("{base}{}", "\n".repeat(context_rows));
        let mut exact_rows = vec![
            ApprovalDiffRowKind::FileHeader,
            ApprovalDiffRowKind::FileHeader,
            ApprovalDiffRowKind::Hunk,
        ];
        exact_rows.extend(std::iter::repeat_n(
            ApprovalDiffRowKind::Context,
            context_rows,
        ));
        assert!(
            ApprovalPrompt::canonical_patch(
                None,
                exact.clone(),
                ApprovalPatchOperation::Update,
                "p".to_owned(),
                exact_rows.clone(),
                0,
                0,
                1,
            )
            .is_ok()
        );
        exact_rows.push(ApprovalDiffRowKind::Context);
        assert!(
            ApprovalPrompt::canonical_patch(
                None,
                format!("{exact}\n"),
                ApprovalPatchOperation::Update,
                "p".to_owned(),
                exact_rows,
                0,
                0,
                1,
            )
            .is_err()
        );
    }
}
