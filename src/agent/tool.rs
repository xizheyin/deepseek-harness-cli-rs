use std::{future::Future, pin::Pin};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    model::{CallId, ContentBlock, JsonValue, MAX_MESSAGE_CONTENT_BLOCKS, ModelError},
    session::ToolFailure,
};

use super::MAX_AGENT_TOOL_RESULT_BYTES;

/// Future returned by one tool implementation without spawning detached work.
pub type ToolExecutionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutorError>> + Send + 'a>>;

/// Validated invocation presented only after its durable `tool/call` commits.
pub struct ToolExecutionRequest {
    call_id: CallId,
    name: String,
    raw_arguments: String,
    arguments: JsonValue,
}

impl std::fmt::Debug for ToolExecutionRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolExecutionRequest")
            .field("call_id", &self.call_id)
            .field("name", &self.name)
            .field("argument_bytes", &self.raw_arguments.len())
            .finish()
    }
}

impl ToolExecutionRequest {
    pub(crate) fn new(
        call_id: CallId,
        name: String,
        raw_arguments: String,
        arguments: JsonValue,
    ) -> Self {
        Self {
            call_id,
            name,
            raw_arguments,
            arguments,
        }
    }

    #[must_use]
    pub fn call_id(&self) -> &CallId {
        &self.call_id
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn raw_arguments(&self) -> &str {
        &self.raw_arguments
    }

    #[must_use]
    pub fn arguments(&self) -> &JsonValue {
        &self.arguments
    }
}

/// Normalized model-facing result returned by a fake or future real tool pipeline.
pub struct ToolExecutionResult {
    content: Vec<ContentBlock>,
    is_error: bool,
    error: Option<ToolFailure>,
    meta: Option<JsonValue>,
    concludes_turn: bool,
}

impl std::fmt::Debug for ToolExecutionResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolExecutionResult")
            .field("content_blocks", &self.content.len())
            .field("is_error", &self.is_error)
            .field("error_present", &self.error.is_some())
            .field("meta_present", &self.meta.is_some())
            .field("concludes_turn", &self.concludes_turn)
            .finish()
    }
}

impl ToolExecutionResult {
    pub fn success(content: Vec<ContentBlock>) -> Result<Self, ModelError> {
        Self::new(content, false, None, None, false)
    }

    pub fn model_error(content: Vec<ContentBlock>, error: ToolFailure) -> Result<Self, ModelError> {
        Self::new(content, true, Some(error), None, false)
    }

    pub fn new(
        content: Vec<ContentBlock>,
        is_error: bool,
        error: Option<ToolFailure>,
        meta: Option<JsonValue>,
        concludes_turn: bool,
    ) -> Result<Self, ModelError> {
        if is_error != error.is_some() || (is_error && concludes_turn) {
            return Err(ModelError::InvalidShape {
                subject: "tool execution result",
                detail: "success omits failure metadata; failure requires metadata and cannot conclude the turn"
                    .to_owned(),
            });
        }
        if content.len() > MAX_MESSAGE_CONTENT_BLOCKS {
            return Err(ModelError::TooManyContentBlocks {
                maximum: MAX_MESSAGE_CONTENT_BLOCKS,
                actual: content.len(),
            });
        }
        if error.as_ref().is_some_and(|value| {
            value.name.is_empty()
                || value.code.is_empty()
                || value.name.len() > 256
                || value.code.len() > 256
        }) {
            return Err(ModelError::InvalidShape {
                subject: "tool execution result",
                detail: "failure name/code must be 1 to 256 bytes".to_owned(),
            });
        }
        let retained_bytes = content
            .iter()
            .try_fold(0_usize, |total, block| {
                total.checked_add(block.raw().encoded_len())
            })
            .and_then(|total| {
                meta.as_ref()
                    .map_or(Some(total), |value| total.checked_add(value.encoded_len()))
            })
            .and_then(|total| {
                error.as_ref().map_or(Some(total), |value| {
                    total
                        .checked_add(value.name.len())
                        .and_then(|total| total.checked_add(value.code.len()))
                })
            })
            .unwrap_or(usize::MAX);
        if retained_bytes > MAX_AGENT_TOOL_RESULT_BYTES {
            return Err(ModelError::InvalidShape {
                subject: "tool execution result",
                detail: format!(
                    "retained content is {retained_bytes} bytes; maximum is {MAX_AGENT_TOOL_RESULT_BYTES}"
                ),
            });
        }
        Ok(Self {
            content,
            is_error,
            error,
            meta,
            concludes_turn,
        })
    }

    #[must_use]
    pub fn content(&self) -> &[ContentBlock] {
        &self.content
    }

    #[must_use]
    pub fn is_error(&self) -> bool {
        self.is_error
    }

    #[must_use]
    pub fn error(&self) -> Option<&ToolFailure> {
        self.error.as_ref()
    }

    #[must_use]
    pub fn meta(&self) -> Option<&JsonValue> {
        self.meta.as_ref()
    }

    #[must_use]
    pub fn concludes_turn(&self) -> bool {
        self.concludes_turn
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Vec<ContentBlock>,
        bool,
        Option<ToolFailure>,
        Option<JsonValue>,
        bool,
    ) {
        (
            self.content,
            self.is_error,
            self.error,
            self.meta,
            self.concludes_turn,
        )
    }
}

/// Infrastructure failure at the executor seam, distinct from a normal tool error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("tool executor failed")]
pub struct ToolExecutorError;

impl ToolExecutorError {
    #[must_use]
    pub fn new(_message: impl Into<String>) -> Self {
        // Extension errors may contain credentials, paths, or model-provided
        // text. The Phase 3 seam deliberately keeps only the failure class.
        Self
    }
}

/// Minimal Phase 3 tool seam; policy, approval, and real registries arrive later.
pub trait ToolExecutor: Send + Sync {
    /// Build and promptly return a lazy future. Implementations must not perform
    /// the actual tool side effect synchronously before the future is polled. Each
    /// poll must return promptly; the future must check the child cancellation
    /// token before its first side effect and must own/clean up all work it
    /// starts rather than spawning detached background work. Returned content
    /// and `is_error` are model-visible; every result field, including failure
    /// and extension metadata, is durable. Implementations and their policy
    /// layer must not include credentials or data they were not authorized to
    /// disclose.
    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_>;
}

/// Default executor for a loop that exposes no executable tools.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoTools;

impl ToolExecutor for NoTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("no tool executor is configured")) })
    }
}
