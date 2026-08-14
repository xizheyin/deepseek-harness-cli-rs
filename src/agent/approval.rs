use std::{future::Future, pin::Pin};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    model::CallId,
    session::{ApprovalOutcome, ApprovalRequestId},
};

pub const MAX_APPROVAL_PREVIEW_BYTES: usize = 64 * 1024;
pub const MAX_APPROVAL_REASON_BYTES: usize = 4 * 1024;

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

/// Bounded, immutable question retained by one prepared mutation.
#[derive(Clone, Eq, PartialEq)]
pub struct ApprovalPrompt {
    reason: Option<String>,
    preview: String,
}

impl std::fmt::Debug for ApprovalPrompt {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApprovalPrompt")
            .field("reason_present", &self.reason.is_some())
            .field("reason_bytes", &self.reason.as_ref().map_or(0, String::len))
            .field("preview_bytes", &self.preview.len())
            .finish()
    }
}

impl ApprovalPrompt {
    pub fn new(
        reason: Option<String>,
        preview: impl Into<String>,
    ) -> Result<Self, ApprovalPromptError> {
        let preview = preview.into();
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
        Ok(Self { reason, preview })
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

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ApprovalPromptError {
    #[error("approval preview is {actual} bytes; expected 1 to {maximum}")]
    InvalidPreview { maximum: usize, actual: usize },
    #[error("approval reason is {actual} bytes; maximum is {maximum}")]
    InvalidReason { maximum: usize, actual: usize },
    #[error("approval reason contains an unsafe control character")]
    InvalidReasonCharacters,
}

/// Owned request passed to the approval UI without filesystem authority.
#[derive(Clone, Eq, PartialEq)]
pub struct ApprovalRequest {
    id: ApprovalRequestId,
    tool_name: String,
    call_id: CallId,
    reason: Option<String>,
    preview: String,
}

impl std::fmt::Debug for ApprovalRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApprovalRequest")
            .field("id", &self.id)
            .field("tool_name", &self.tool_name)
            .field("call_id", &self.call_id)
            .field("reason_present", &self.reason.is_some())
            .field("reason_bytes", &self.reason.as_ref().map_or(0, String::len))
            .field("preview_bytes", &self.preview.len())
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
            preview: prompt.preview.clone(),
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
