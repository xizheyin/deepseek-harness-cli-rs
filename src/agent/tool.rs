use std::{future::Future, pin::Pin};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    model::{CallId, ContentBlock, JsonValue, MAX_MESSAGE_CONTENT_BLOCKS, ModelError},
    session::ToolFailure,
};

use super::{MAX_AGENT_TOOL_RESULT_BYTES, approval::ApprovalPrompt};

/// Future returned by one tool implementation without spawning detached work.
pub type ToolExecutionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolExecutionResult, ToolExecutorError>> + Send + 'a>>;

/// Future returned by the side-effect-free tool preparation stage.
pub type ToolPreparationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolPreparation, ToolExecutorError>> + Send + 'a>>;

/// Result of preparing one durable tool call.
pub enum ToolPreparation {
    Complete(ToolExecutionResult),
    Mutation(PreparedToolMutation),
}

impl std::fmt::Debug for ToolPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete(result) => formatter.debug_tuple("Complete").field(result).finish(),
            Self::Mutation(mutation) => formatter.debug_tuple("Mutation").field(mutation).finish(),
        }
    }
}

/// Why a prepared mutation was closed without starting its commit capability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MutationDeclineReason {
    PolicyDenied,
    ApprovalRejected,
    ApprovalCancelled,
    ApprovalUnavailable,
    AbortedBeforeDispatch,
    Aborted,
    OutputBudgetExceeded,
}

/// Whether a completed commit changed the target's durable logical state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolCommitDisposition {
    NotCommitted,
    Committed,
}

/// Truthful outcome returned by the single-use blocking commit capability.
pub struct ToolCommitOutcome {
    disposition: ToolCommitDisposition,
    result: ToolExecutionResult,
}

impl std::fmt::Debug for ToolCommitOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolCommitOutcome")
            .field("disposition", &self.disposition)
            .field("result", &self.result)
            .finish()
    }
}

impl ToolCommitOutcome {
    pub fn committed(result: ToolExecutionResult) -> Result<Self, ToolExecutorError> {
        if committed_marker(&result) != Some(true) {
            return Err(ToolExecutorError::new(
                "a committed mutation result must retain committed=true metadata",
            ));
        }
        Ok(Self {
            disposition: ToolCommitDisposition::Committed,
            result,
        })
    }

    pub fn not_committed(result: ToolExecutionResult) -> Result<Self, ToolExecutorError> {
        if !result.is_error() || committed_marker(&result) != Some(false) {
            return Err(ToolExecutorError::new(
                "a non-committed mutation must return an error result with committed=false metadata",
            ));
        }
        Ok(Self {
            disposition: ToolCommitDisposition::NotCommitted,
            result,
        })
    }

    #[must_use]
    pub fn disposition(&self) -> ToolCommitDisposition {
        self.disposition
    }

    #[must_use]
    pub fn result(&self) -> &ToolExecutionResult {
        &self.result
    }

    pub(crate) fn into_parts(self) -> (ToolCommitDisposition, ToolExecutionResult) {
        (self.disposition, self.result)
    }
}

pub type ToolDeclineFn = Box<
    dyn FnOnce(MutationDeclineReason) -> Result<ToolExecutionResult, ToolExecutorError>
        + Send
        + 'static,
>;
pub type ToolCommitFn = Box<
    dyn FnOnce(CancellationToken) -> Result<ToolCommitOutcome, ToolExecutorError> + Send + 'static,
>;

/// Fully owned, single-use file mutation returned only after read-only preparation.
pub struct PreparedToolMutation {
    prompt: ApprovalPrompt,
    maximum_result_event_bytes: usize,
    decline: ToolDeclineFn,
    commit: ToolCommitFn,
}

impl std::fmt::Debug for PreparedToolMutation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedToolMutation")
            .field("prompt", &self.prompt)
            .field(
                "maximum_result_event_bytes",
                &self.maximum_result_event_bytes,
            )
            .field("single_use_commit", &true)
            .finish()
    }
}

impl PreparedToolMutation {
    pub fn new(
        prompt: ApprovalPrompt,
        maximum_result_event_bytes: usize,
        decline: ToolDeclineFn,
        commit: ToolCommitFn,
    ) -> Result<Self, ToolExecutorError> {
        if maximum_result_event_bytes == 0
            || maximum_result_event_bytes > super::MAX_AGENT_COMMITTED_TOOL_RESULT_EVENT_BYTES
        {
            return Err(ToolExecutorError::new(
                "prepared mutation result bound is outside the supported range",
            ));
        }
        Ok(Self {
            prompt,
            maximum_result_event_bytes,
            decline,
            commit,
        })
    }

    #[must_use]
    pub fn prompt(&self) -> &ApprovalPrompt {
        &self.prompt
    }

    #[must_use]
    pub fn maximum_result_event_bytes(&self) -> usize {
        self.maximum_result_event_bytes
    }

    pub(crate) fn decline(
        self,
        reason: MutationDeclineReason,
    ) -> Result<ToolExecutionResult, ToolExecutorError> {
        let result = (self.decline)(reason)?;
        if !result.is_error() || committed_marker(&result) != Some(false) {
            return Err(ToolExecutorError::new(
                "a declined mutation must return an error result with committed=false metadata",
            ));
        }
        Ok(result)
    }

    pub(crate) fn commit(
        self,
        cancellation: CancellationToken,
    ) -> Result<ToolCommitOutcome, ToolExecutorError> {
        (self.commit)(cancellation)
    }
}

fn committed_marker(result: &ToolExecutionResult) -> Option<bool> {
    result
        .meta()
        .and_then(|meta| meta.as_value().as_object())
        .and_then(|fields| fields.get("committed"))
        .and_then(serde_json::Value::as_bool)
}

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

/// Trusted in-process tool seam used by ordinary and approval-gated tools.
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

    /// Prepare one call without performing a mutation. This synchronous factory
    /// must return promptly. A `Mutation` must contain the complete read-only
    /// preview and defer every write to its single-use commit capability. Legacy
    /// trusted tools use the default adapter and complete through their existing
    /// lazy executor.
    fn prepare(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let execution = self.execute(request, cancellation);
        Box::pin(async move { execution.await.map(ToolPreparation::Complete) })
    }
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
