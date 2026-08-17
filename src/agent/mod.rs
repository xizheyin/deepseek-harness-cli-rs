//! Bounded, cancellable Agent Loop joining sessions, providers, and tools.

mod approval;
mod assembler;
mod compaction;
mod error;
#[cfg(test)]
mod phase6_tests;
#[cfg(test)]
mod phase7_tests;
mod retry;
mod tool;

use std::{
    collections::{BTreeMap, BTreeSet},
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
    time::Duration,
};

use futures_util::{FutureExt, StreamExt};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{
    entropy::EntropySource,
    model::{
        CallId, ContentBlock, ContentBlockKind, FinishReasonKind, JsonValue, LlmCallConfig,
        LlmFailure, Message, MessageRole, MessageSource, StreamChunkKind, ToolSchema,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderRequest, ProviderRequestDraft, ProviderRequestError, RetryMode,
    },
    session::{
        AppendError, AppendReceipt, ApprovalAskedEvent, ApprovalDecidedEvent, ApprovalOutcome,
        ApprovalRequestId, AttemptDisposition, AttemptResidentGuard, AttemptToken, BarrierError,
        ClaimedAppend, EpochHeader, EventClaim, EventKind, EventSeq, LlmRetryEvent,
        LlmRetryStartedEvent, NewEvent, PreparedAttempt, RequestContext, RequestHeaderReason,
        RetryId, RetryNumber, Session, SessionReadError, SessionReservation, StepId, SurfaceIntent,
        TOOL_OUTCOME_UNKNOWN, ToolFailure, ToolResultPrunePassCause, TurnEndCancelCause,
        TurnEndReason, TurnId,
    },
};

pub use approval::{
    ApprovalFuture, ApprovalPrompt, ApprovalPromptError, ApprovalProvider, ApprovalProviderError,
    ApprovalRequest, FileChangePolicy, MAX_APPROVAL_PREVIEW_BYTES, MAX_APPROVAL_REASON_BYTES,
    NoApprovalProvider, ShellPolicy,
};
pub use error::{AgentBuildError, AgentLoopError, AgentRuntimeError};
pub(crate) use tool::{
    ActionDeclineReason, ToolActionControl, ToolActionOutcome, ToolActionSetupControl,
    ToolActionSetupOutcome, ToolActionTurnStop, ToolDispatchBinding,
};
pub use tool::{
    MutationDeclineReason, NoTools, PreparedToolAction, PreparedToolActionSetup,
    PreparedToolMutation, ToolClaimProfile, ToolCommitDisposition, ToolCommitFn, ToolCommitOutcome,
    ToolDeclineFn, ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
    ToolExecutorError, ToolPreparation, ToolPreparationFuture,
};

use assembler::{AssembledAssistant, without_tool_calls};
use compaction::{CompactionOutcome, compact_once, pressure_reached, retained_token_target};
use retry::{RetryDecision, decide, policy_key};

pub const MAX_AGENT_STEPS_PER_TURN: usize = 64;
pub const MAX_AGENT_ATTEMPTS_PER_TURN: usize = 64;
pub const MAX_AGENT_RETRIES_PER_STEP: usize = 8;
pub const MAX_AGENT_TOOL_CALLS_PER_STEP: usize = 64;
pub const MAX_AGENT_TOOL_CALLS_PER_TURN: usize = 256;
pub const MAX_AGENT_OUTPUT_TOKENS_PER_REQUEST: u64 = 1_000_000;
pub const MAX_AGENT_REPORTED_OUTPUT_TOKENS: u64 = 4_000_000;
pub const MAX_AGENT_TURN_DURATION: Duration = Duration::from_secs(2 * 60 * 60);
pub const MAX_AGENT_TOOL_DURATION: Duration = Duration::from_secs(5 * 60);
pub const MAX_AGENT_ACTION_PREPARATION_DURATION: Duration = Duration::from_secs(5);
/// Extra time given to an already-started tool to observe cancellation and
/// release its own resources. This is separate from the normal tool timeout.
pub const MAX_AGENT_TOOL_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
pub const MAX_AGENT_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
pub const MAX_AGENT_TOOL_RESULT_BYTES: usize = 256 * 1024;
/// Maximum compact event size promised by a sealed foreground action.
pub const MAX_AGENT_ACTION_RESULT_EVENT_BYTES: usize = 128 * 1024;
/// Session-event capacity protected before an irreversible mutation starts.
pub const MAX_AGENT_COMMITTED_TOOL_RESULT_EVENT_BYTES: usize = 512 * 1024;
pub const MAX_AGENT_TOOL_RESULTS_PER_TURN_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_AGENT_FIXED_REQUEST_BYTES: usize = 4 * 1024 * 1024;
const AGENT_READY_WORK_BUDGET: usize = 32;

/// Configurable limits that also remain below fixed process safety ceilings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentLimits {
    max_steps_per_turn: usize,
    max_attempts_per_turn: usize,
    max_retries_per_step: usize,
    max_tool_calls_per_step: usize,
    max_tool_calls_per_turn: usize,
    max_output_tokens_per_request: u64,
    max_reported_output_tokens_per_turn: u64,
    turn_duration: Duration,
    tool_duration: Duration,
    max_tool_argument_bytes: usize,
    max_tool_result_bytes: usize,
    max_tool_results_per_turn_bytes: usize,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_steps_per_turn: 16,
            max_attempts_per_turn: 24,
            max_retries_per_step: 8,
            max_tool_calls_per_step: 16,
            max_tool_calls_per_turn: 64,
            max_output_tokens_per_request: MAX_AGENT_OUTPUT_TOKENS_PER_REQUEST,
            max_reported_output_tokens_per_turn: 1_000_000,
            turn_duration: Duration::from_secs(30 * 60),
            tool_duration: Duration::from_secs(30),
            max_tool_argument_bytes: MAX_AGENT_TOOL_ARGUMENT_BYTES,
            max_tool_result_bytes: MAX_AGENT_TOOL_RESULT_BYTES,
            max_tool_results_per_turn_bytes: MAX_AGENT_TOOL_RESULTS_PER_TURN_BYTES,
        }
    }
}

impl AgentLimits {
    pub fn with_max_steps_per_turn(mut self, value: usize) -> Result<Self, AgentBuildError> {
        validate_usize_limit("max_steps_per_turn", value, 1, MAX_AGENT_STEPS_PER_TURN)?;
        self.max_steps_per_turn = value;
        Ok(self)
    }

    pub fn with_max_attempts_per_turn(mut self, value: usize) -> Result<Self, AgentBuildError> {
        validate_usize_limit(
            "max_attempts_per_turn",
            value,
            1,
            MAX_AGENT_ATTEMPTS_PER_TURN,
        )?;
        self.max_attempts_per_turn = value;
        Ok(self)
    }

    pub fn with_max_retries_per_step(mut self, value: usize) -> Result<Self, AgentBuildError> {
        validate_usize_limit("max_retries_per_step", value, 0, MAX_AGENT_RETRIES_PER_STEP)?;
        self.max_retries_per_step = value;
        Ok(self)
    }

    pub fn with_max_tool_calls_per_step(mut self, value: usize) -> Result<Self, AgentBuildError> {
        validate_usize_limit(
            "max_tool_calls_per_step",
            value,
            0,
            MAX_AGENT_TOOL_CALLS_PER_STEP,
        )?;
        self.max_tool_calls_per_step = value;
        Ok(self)
    }

    pub fn with_max_tool_calls_per_turn(mut self, value: usize) -> Result<Self, AgentBuildError> {
        validate_usize_limit(
            "max_tool_calls_per_turn",
            value,
            0,
            MAX_AGENT_TOOL_CALLS_PER_TURN,
        )?;
        self.max_tool_calls_per_turn = value;
        Ok(self)
    }

    pub fn with_max_reported_output_tokens_per_turn(
        mut self,
        value: u64,
    ) -> Result<Self, AgentBuildError> {
        validate_u64_limit(
            "max_reported_output_tokens_per_turn",
            value,
            1,
            MAX_AGENT_REPORTED_OUTPUT_TOKENS,
        )?;
        self.max_reported_output_tokens_per_turn = value;
        Ok(self)
    }

    pub fn with_max_output_tokens_per_request(
        mut self,
        value: u64,
    ) -> Result<Self, AgentBuildError> {
        validate_u64_limit(
            "max_output_tokens_per_request",
            value,
            1,
            MAX_AGENT_OUTPUT_TOKENS_PER_REQUEST,
        )?;
        self.max_output_tokens_per_request = value;
        Ok(self)
    }

    pub fn with_turn_duration(mut self, value: Duration) -> Result<Self, AgentBuildError> {
        validate_duration("turn_duration", value, MAX_AGENT_TURN_DURATION)?;
        self.turn_duration = value;
        Ok(self)
    }

    pub fn with_tool_duration(mut self, value: Duration) -> Result<Self, AgentBuildError> {
        validate_duration("tool_duration", value, MAX_AGENT_TOOL_DURATION)?;
        self.tool_duration = value;
        Ok(self)
    }

    pub fn with_max_tool_argument_bytes(mut self, value: usize) -> Result<Self, AgentBuildError> {
        validate_usize_limit(
            "max_tool_argument_bytes",
            value,
            1,
            MAX_AGENT_TOOL_ARGUMENT_BYTES,
        )?;
        self.max_tool_argument_bytes = value;
        Ok(self)
    }

    pub fn with_max_tool_result_bytes(mut self, value: usize) -> Result<Self, AgentBuildError> {
        validate_usize_limit(
            "max_tool_result_bytes",
            value,
            1,
            MAX_AGENT_TOOL_RESULT_BYTES,
        )?;
        self.max_tool_result_bytes = value;
        Ok(self)
    }

    pub fn with_max_tool_results_per_turn_bytes(
        mut self,
        value: usize,
    ) -> Result<Self, AgentBuildError> {
        validate_usize_limit(
            "max_tool_results_per_turn_bytes",
            value,
            1,
            MAX_AGENT_TOOL_RESULTS_PER_TURN_BYTES,
        )?;
        self.max_tool_results_per_turn_bytes = value;
        Ok(self)
    }

    #[must_use]
    pub fn max_steps_per_turn(&self) -> usize {
        self.max_steps_per_turn
    }

    #[must_use]
    pub fn max_attempts_per_turn(&self) -> usize {
        self.max_attempts_per_turn
    }

    #[must_use]
    pub fn max_retries_per_step(&self) -> usize {
        self.max_retries_per_step
    }

    #[must_use]
    pub fn max_tool_calls_per_step(&self) -> usize {
        self.max_tool_calls_per_step
    }

    #[must_use]
    pub fn max_tool_calls_per_turn(&self) -> usize {
        self.max_tool_calls_per_turn
    }

    #[must_use]
    pub fn max_reported_output_tokens_per_turn(&self) -> u64 {
        self.max_reported_output_tokens_per_turn
    }

    #[must_use]
    pub fn max_output_tokens_per_request(&self) -> u64 {
        self.max_output_tokens_per_request
    }

    #[must_use]
    pub fn turn_duration(&self) -> Duration {
        self.turn_duration
    }

    #[must_use]
    pub fn tool_duration(&self) -> Duration {
        self.tool_duration
    }
}

fn validate_usize_limit(
    name: &'static str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), AgentBuildError> {
    if !(minimum..=maximum).contains(&value) {
        return Err(AgentBuildError::InvalidLimit {
            name,
            minimum: minimum as u64,
            maximum: maximum as u64,
            actual: value as u64,
        });
    }
    Ok(())
}

fn validate_u64_limit(
    name: &'static str,
    value: u64,
    minimum: u64,
    maximum: u64,
) -> Result<(), AgentBuildError> {
    if !(minimum..=maximum).contains(&value) {
        return Err(AgentBuildError::InvalidLimit {
            name,
            minimum,
            maximum,
            actual: value,
        });
    }
    Ok(())
}

fn validate_duration(
    name: &'static str,
    value: Duration,
    maximum: Duration,
) -> Result<(), AgentBuildError> {
    if value.is_zero() || value > maximum {
        return Err(AgentBuildError::InvalidLimit {
            name,
            minimum: 1,
            maximum: maximum.as_millis().min(u128::from(u64::MAX)) as u64,
            actual: value.as_millis().min(u128::from(u64::MAX)) as u64,
        });
    }
    Ok(())
}

/// Immutable request and safety configuration shared by every turn.
#[derive(Clone)]
pub struct AgentLoopConfig {
    call: LlmCallConfig,
    system: Option<String>,
    tools: Vec<ToolSchema>,
    limits: AgentLimits,
    file_change_policy: FileChangePolicy,
    shell_policy: ShellPolicy,
    approval_provider: Arc<dyn ApprovalProvider>,
}

impl std::fmt::Debug for AgentLoopConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AgentLoopConfig")
            .field("provider", &self.call.provider())
            .field("model", &self.call.model())
            .field("system_bytes", &self.system.as_ref().map_or(0, String::len))
            .field("tool_count", &self.tools.len())
            .field("limits", &self.limits)
            .field("file_change_policy", &self.file_change_policy)
            .field("shell_policy", &self.shell_policy)
            .field("approval_provider_configured", &true)
            .finish()
    }
}

impl AgentLoopConfig {
    #[must_use]
    pub fn new(call: LlmCallConfig) -> Self {
        Self {
            call,
            system: None,
            tools: Vec::new(),
            limits: AgentLimits::default(),
            file_change_policy: FileChangePolicy::Ask,
            shell_policy: ShellPolicy::Ask,
            approval_provider: Arc::new(NoApprovalProvider),
        }
    }

    pub fn with_system(mut self, system: impl Into<String>) -> Result<Self, AgentBuildError> {
        let system = system.into();
        self.system = (!system.is_empty()).then_some(system);
        self.validate_fixed_request_size()?;
        Ok(self)
    }

    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Result<Self, AgentBuildError> {
        if tools.len() > MAX_AGENT_TOOL_CALLS_PER_TURN {
            return Err(AgentBuildError::TooManyTools {
                maximum: MAX_AGENT_TOOL_CALLS_PER_TURN,
                actual: tools.len(),
            });
        }
        let mut names = BTreeSet::new();
        if tools.iter().any(|tool| {
            tool.name().is_empty()
                || tool.name().len() > 256
                || tool.name().chars().any(char::is_control)
                || !names.insert(tool.name())
        }) {
            return Err(AgentBuildError::InvalidToolNames);
        }
        self.tools = tools;
        self.validate_fixed_request_size()?;
        Ok(self)
    }

    #[must_use]
    pub fn with_limits(mut self, limits: AgentLimits) -> Self {
        self.limits = limits;
        self
    }

    #[must_use]
    pub fn with_file_change_approval(
        mut self,
        policy: FileChangePolicy,
        provider: Arc<dyn ApprovalProvider>,
    ) -> Self {
        self.file_change_policy = policy;
        self.approval_provider = provider;
        self
    }

    #[must_use]
    pub fn with_approval_provider(mut self, provider: Arc<dyn ApprovalProvider>) -> Self {
        self.approval_provider = provider;
        self
    }

    #[must_use]
    pub fn with_file_change_policy(mut self, policy: FileChangePolicy) -> Self {
        self.file_change_policy = policy;
        self
    }

    #[must_use]
    pub fn with_shell_policy(mut self, policy: ShellPolicy) -> Self {
        self.shell_policy = policy;
        self
    }

    #[must_use]
    pub fn file_change_policy(&self) -> FileChangePolicy {
        self.file_change_policy
    }

    #[must_use]
    pub fn shell_policy(&self) -> ShellPolicy {
        self.shell_policy
    }

    #[must_use]
    pub fn call(&self) -> &LlmCallConfig {
        &self.call
    }

    #[must_use]
    pub fn limits(&self) -> &AgentLimits {
        &self.limits
    }

    fn validate_fixed_request_size(&self) -> Result<(), AgentBuildError> {
        let actual = self
            .call
            .raw()
            .encoded_len()
            .checked_add(self.system.as_ref().map_or(0, String::len))
            .and_then(|total| {
                self.tools.iter().try_fold(total, |total, tool| {
                    total.checked_add(tool.raw().encoded_len())
                })
            })
            .unwrap_or(usize::MAX);
        if actual > MAX_AGENT_FIXED_REQUEST_BYTES {
            return Err(AgentBuildError::FixedRequestTooLarge {
                maximum: MAX_AGENT_FIXED_REQUEST_BYTES,
                actual,
            });
        }
        Ok(())
    }
}

/// Whether one submitted batch enters the loop or is rejected before a step.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnProposal {
    Enter(Vec<Message>),
    Reject,
}

/// Kind prefix used by an injectable opaque-ID source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentIdKind {
    Message,
    Retry,
    Approval,
}

impl AgentIdKind {
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Retry => "retry",
            Self::Approval => "approval",
        }
    }
}

/// Small trusted nondeterministic boundary used only for opaque IDs and retry jitter.
///
/// Both synchronous methods must return promptly and must not start detached
/// work. Generated IDs are written to the session, so implementations must not
/// include credentials or other unauthorized data.
pub trait AgentRuntime: Send + Sync {
    fn next_id(&self, kind: AgentIdKind) -> Result<String, AgentRuntimeError>;
    fn sample_unit(&self) -> Result<f64, AgentRuntimeError>;
}

#[derive(Clone, Copy)]
pub struct SystemAgentRuntime {
    entropy: EntropySource,
}

impl Default for SystemAgentRuntime {
    fn default() -> Self {
        Self {
            entropy: EntropySource::system(),
        }
    }
}

impl std::fmt::Debug for SystemAgentRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SystemAgentRuntime").finish()
    }
}

impl SystemAgentRuntime {
    #[cfg(test)]
    fn with_entropy_for_test(entropy: EntropySource) -> Self {
        Self { entropy }
    }
}

impl AgentRuntime for SystemAgentRuntime {
    fn next_id(&self, kind: AgentIdKind) -> Result<String, AgentRuntimeError> {
        let id = self
            .entropy
            .uuid_v4()
            .map_err(|_| AgentRuntimeError::EntropyUnavailable)?;
        Ok(format!("{}-{id}", kind.prefix()))
    }

    fn sample_unit(&self) -> Result<f64, AgentRuntimeError> {
        let value = self
            .entropy
            .random_u128()
            .map_err(|_| AgentRuntimeError::EntropyUnavailable)?;
        Ok(value as f64 / u128::MAX as f64)
    }
}

/// Counters and the exact reason committed by a finished turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOutcome {
    turn: TurnId,
    turn_end_seq: EventSeq,
    reason: TurnEndReason,
    final_message: Option<Message>,
    steps: usize,
    attempts: usize,
    retries: usize,
    tool_calls: usize,
    reported_output_tokens: u64,
}

impl TurnOutcome {
    #[must_use]
    pub fn turn(&self) -> TurnId {
        self.turn
    }

    /// Sequence of the durable `turn/end` that closed this outcome.
    #[must_use]
    pub fn turn_end_seq(&self) -> EventSeq {
        self.turn_end_seq
    }

    #[must_use]
    pub fn reason(&self) -> &TurnEndReason {
        &self.reason
    }

    /// Latest assistant message in this turn that contains non-empty text.
    ///
    /// The message is a shallow immutable handle, so callers do not need to
    /// rescan or retain the complete session history to render final output.
    #[must_use]
    pub fn final_message(&self) -> Option<&Message> {
        self.final_message.as_ref()
    }

    #[must_use]
    pub fn steps(&self) -> usize {
        self.steps
    }

    #[must_use]
    pub fn attempts(&self) -> usize {
        self.attempts
    }

    #[must_use]
    pub fn retries(&self) -> usize {
        self.retries
    }

    #[must_use]
    pub fn tool_calls(&self) -> usize {
        self.tool_calls
    }

    #[must_use]
    pub fn reported_output_tokens(&self) -> u64 {
        self.reported_output_tokens
    }
}

/// Stateful owner of one session and its request-header lifecycle.
pub struct AgentLoop {
    session: Session,
    provider: Arc<dyn ModelProvider>,
    tools: Arc<dyn ToolExecutor>,
    runtime: Arc<dyn AgentRuntime>,
    config: AgentLoopConfig,
    request_header_logged: bool,
    poisoned: bool,
}

impl AgentLoop {
    pub fn new(
        session: Session,
        provider: Arc<dyn ModelProvider>,
        tools: Arc<dyn ToolExecutor>,
        config: AgentLoopConfig,
    ) -> Result<Self, AgentBuildError> {
        Self::with_runtime(
            session,
            provider,
            tools,
            Arc::new(SystemAgentRuntime::default()),
            config,
        )
    }

    pub fn with_runtime(
        session: Session,
        provider: Arc<dyn ModelProvider>,
        tools: Arc<dyn ToolExecutor>,
        runtime: Arc<dyn AgentRuntime>,
        config: AgentLoopConfig,
    ) -> Result<Self, AgentBuildError> {
        Self::with_runtime_preserving_session(session, provider, tools, runtime, config)
            .map_err(|(error, _session)| error)
    }

    /// CLI assembly seam that returns the still-owned Session when validation
    /// fails. This matters for a resumed durable journal: the caller must run
    /// its async shutdown instead of letting an error path synchronously join
    /// the writer from `Drop` on Tokio's current thread.
    pub(crate) fn new_preserving_session(
        session: Session,
        provider: Arc<dyn ModelProvider>,
        tools: Arc<dyn ToolExecutor>,
        config: AgentLoopConfig,
    ) -> Result<Self, (AgentBuildError, Session)> {
        Self::with_runtime_preserving_session(
            session,
            provider,
            tools,
            Arc::new(SystemAgentRuntime::default()),
            config,
        )
    }

    fn with_runtime_preserving_session(
        session: Session,
        provider: Arc<dyn ModelProvider>,
        tools: Arc<dyn ToolExecutor>,
        runtime: Arc<dyn AgentRuntime>,
        config: AgentLoopConfig,
    ) -> Result<Self, (AgentBuildError, Session)> {
        if !session.state().pending_approvals().is_empty() {
            return Err((AgentBuildError::UnresolvedApproval, session));
        }
        if session.state().open_turn().is_some() {
            return Err((AgentBuildError::SessionNotIdle, session));
        }
        if session_has_unresolved_tool_calls(&session) {
            return Err((AgentBuildError::UnresolvedToolCall, session));
        }
        if let Err(error) = config.validate_fixed_request_size() {
            return Err((error, session));
        }
        Ok(Self {
            session,
            provider,
            tools,
            runtime,
            config,
            request_header_logged: false,
            poisoned: false,
        })
    }

    #[must_use]
    pub fn session(&self) -> &Session {
        &self.session
    }

    #[must_use]
    pub fn into_session(self) -> Session {
        self.session
    }

    /// Flush and join the Session writer before the CLI releases this owner.
    pub async fn shutdown(&mut self) -> Result<(), crate::session::SessionIoError> {
        self.session.shutdown().await
    }

    /// Run one bounded turn and settle every ordinary error/cancellation path.
    ///
    /// Cancellation is cooperative: cancel the supplied token, then keep
    /// awaiting this future until it returns so provider/tool cleanup and
    /// `step/end`/`turn/end` can commit. Dropping this future after polling is
    /// equivalent to a process crash and can leave an open tail for Phase 8
    /// recovery; async closing work cannot be performed from `Drop`.
    pub async fn run_turn(
        &mut self,
        proposal: TurnProposal,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, AgentLoopError> {
        if self.poisoned {
            return Err(AgentLoopError::Poisoned);
        }
        if self.session.state().open_turn().is_some() {
            return Err(AgentLoopError::SessionNotIdle);
        }
        if let TurnProposal::Enter(messages) = &proposal {
            if messages.len() > crate::provider::MAX_PROVIDER_MESSAGES {
                return Err(AgentLoopError::TooManyTurnMessages {
                    maximum: crate::provider::MAX_PROVIDER_MESSAGES,
                    actual: messages.len(),
                });
            }
            if messages
                .iter()
                .any(|message| message.validate_user_event().is_err())
            {
                return Err(AgentLoopError::InvalidTurnMessages);
            }
            let actual = messages.iter().try_fold(0_usize, |total, message| {
                let next = total
                    .checked_add(message.raw().encoded_len())
                    .unwrap_or(usize::MAX);
                (next <= crate::provider::MAX_PROVIDER_REQUEST_BYTES).then_some(next)
            });
            let actual = actual.unwrap_or(crate::provider::MAX_PROVIDER_REQUEST_BYTES + 1);
            if actual > crate::provider::MAX_PROVIDER_REQUEST_BYTES {
                return Err(AgentLoopError::TurnInputTooLarge {
                    maximum: crate::provider::MAX_PROVIDER_REQUEST_BYTES,
                    actual,
                    messages: messages.len(),
                });
            }
        }
        self.session.materialize_if_needed().await?;
        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let runtime = self.runtime.clone();
        let config = self.config.clone();
        let result = run_turn_inner(
            &mut self.session,
            provider.as_ref(),
            tools.as_ref(),
            runtime.as_ref(),
            &config,
            &mut self.request_header_logged,
            proposal,
            cancellation,
        )
        .await;
        if result.is_err()
            || session_has_unresolved_tool_calls(&self.session)
            || !self.session.state().pending_approvals().is_empty()
        {
            self.poisoned = true;
        }
        result
    }
}

#[derive(Default)]
struct Counters {
    steps: usize,
    attempts: usize,
    retries: usize,
    tool_calls: usize,
    reported_output_tokens: u64,
    tool_result_bytes: usize,
}

enum StepOutcome {
    Completed,
    Continue,
    MaxTokens,
    Cancelled,
    Error(LlmFailure),
}

struct UnstartedStepClaims {
    start: EventClaim,
    inputs: Vec<EventClaim>,
    end: EventClaim,
}

impl UnstartedStepClaims {
    fn new(mut claims: Vec<EventClaim>) -> Result<Self, AgentLoopError> {
        if claims.len() < 2 {
            return Err(AgentLoopError::Invariant(
                "step reservation did not return its closure claims",
            ));
        }
        let start = claims.remove(0);
        let end = claims.pop().ok_or(AgentLoopError::Invariant(
            "step reservation omitted its end claim",
        ))?;
        Ok(Self {
            start,
            inputs: claims,
            end,
        })
    }

    async fn enter(
        mut self,
        reservation: &mut SessionReservation<'_>,
    ) -> Result<EventClaim, AgentLoopError> {
        reservation.settle_exact_settled(&mut self.start).await?;
        for input in &mut self.inputs {
            reservation.settle_exact_settled(input).await?;
        }
        Ok(self.end)
    }

    fn release(mut self, reservation: &mut SessionReservation<'_>) -> Result<(), AgentLoopError> {
        reservation.release(&mut self.start)?;
        for input in &mut self.inputs {
            reservation.release(input)?;
        }
        reservation.release(&mut self.end)?;
        Ok(())
    }
}

enum AttemptPreparation {
    Ready(PreflightedRequest),
    PreparedFailure {
        prepared: PreparedProviderCall,
        failure: LlmFailure,
    },
    DeferredFailure(LlmFailure),
    HardLimit {
        prepared: Option<PreparedProviderCall>,
    },
}

enum HardLimitPruneOutcome {
    Progress,
    NoProgress,
    Cancelled,
    TurnError(LlmFailure),
}

fn preparation_context_window(preparation: &AttemptPreparation, session: &Session) -> Option<u64> {
    match preparation {
        AttemptPreparation::Ready(preflighted) => preflighted.prepared_call().context_window(),
        AttemptPreparation::PreparedFailure { prepared, .. }
        | AttemptPreparation::HardLimit {
            prepared: Some(prepared),
        } => prepared.context_window(),
        AttemptPreparation::DeferredFailure(_)
        | AttemptPreparation::HardLimit { prepared: None } => session
            .request_context()
            .and_then(RequestContext::context_window),
    }
    .map(|window| window.get())
}

fn pre_step_compaction_trigger(
    preparation: &AttemptPreparation,
    session: &Session,
    context_window: Option<u64>,
) -> Option<crate::session::CompactionTrigger> {
    if matches!(preparation, AttemptPreparation::HardLimit { .. }) {
        return Some(crate::session::CompactionTrigger::HardLimit);
    }
    (matches!(preparation, AttemptPreparation::Ready(_))
        && context_window.is_some_and(|window| {
            session
                .context_total_tokens()
                .is_ok_and(|total| pressure_reached(total, window))
        }))
    .then_some(crate::session::CompactionTrigger::Pressure)
}

struct PreflightedRequest {
    proposed: LlmCallConfig,
    messages: Vec<Message>,
    expected_surface_generation: u64,
    preflight: PreparedRequestPreflight,
}

impl PreflightedRequest {
    fn prepared_call(&self) -> &crate::provider::PreparedProviderCall {
        self.preflight.prepared_call()
    }

    fn matches_surface(&self, session: &Session) -> bool {
        session.surface_generation() == self.expected_surface_generation
            && session.messages_equal(&self.messages)
    }

    fn into_request(
        self,
        session: &Session,
        config: &AgentLoopConfig,
    ) -> Result<ProviderRequest, ProviderRequestError> {
        let mut draft = ProviderRequestDraft::new(&self.proposed, &self.messages)?;
        if let Some(system) = &config.system {
            draft = draft.with_system(system)?;
        }
        if !config.tools.is_empty() {
            draft = draft.with_tools(&config.tools)?;
        }
        draft = draft.with_session_id(session.id())?;
        draft.into_request(self.preflight)
    }
}

struct StepResolution {
    outcome: StepOutcome,
    latched_turn_stop: ToolActionTurnStop,
}

impl StepResolution {
    fn new(outcome: StepOutcome) -> Self {
        Self {
            outcome,
            latched_turn_stop: ToolActionTurnStop::None,
        }
    }

    fn with_stop(outcome: StepOutcome, stop: ToolStop) -> Self {
        Self {
            outcome,
            latched_turn_stop: match stop {
                ToolStop::None => ToolActionTurnStop::None,
                ToolStop::Cancelled => ToolActionTurnStop::CallerCancelled,
                ToolStop::TurnTimeout => ToolActionTurnStop::TurnTimeout,
            },
        }
    }
}

enum StreamOutcome {
    Finished(PreparedAttempt),
    Cancelled,
    Error(LlmFailure),
}

struct Driver<'a> {
    provider: &'a dyn ModelProvider,
    tools: &'a dyn ToolExecutor,
    runtime: &'a dyn AgentRuntime,
    config: &'a AgentLoopConfig,
    request_header_logged: &'a mut bool,
    counters: Counters,
    final_message: Option<Message>,
    observer_unavailable: bool,
    session_limit_failure: LlmFailure,
    durable_limit: Option<AppendError>,
    deadline: Instant,
}

impl Driver<'_> {
    fn observe_assistant_commit(&mut self, receipt: &AppendReceipt) {
        let Some(message) = receipt.committed_message() else {
            return;
        };
        if message.content().iter().any(
            |block| matches!(block.kind(), ContentBlockKind::Text { text } if !text.is_empty()),
        ) {
            self.final_message = Some(message.clone());
        }
    }

    fn failure_for_budget(
        &mut self,
        error: &AppendError,
        memory_failure: &LlmFailure,
    ) -> LlmFailure {
        if is_durable_session_limit(error) {
            self.latch_durable_limit(error);
            self.session_limit_failure.clone()
        } else {
            debug_assert!(is_memory_budget_error(error));
            memory_failure.clone()
        }
    }

    fn latch_durable_limit(&mut self, error: &AppendError) {
        if is_durable_session_limit(error) && self.durable_limit.is_none() {
            self.durable_limit = Some(error.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_inner(
    session: &mut Session,
    provider: &dyn ModelProvider,
    tools: &dyn ToolExecutor,
    runtime: &dyn AgentRuntime,
    config: &AgentLoopConfig,
    request_header_logged: &mut bool,
    proposal: TurnProposal,
    cancellation: CancellationToken,
) -> Result<TurnOutcome, AgentLoopError> {
    let turn = session.state().next_turn();
    let budget_reason = failure_reason(
        "AGENT_EVENT_BUDGET",
        "the session has no safe room for another agent event",
    )?;
    let session_limit_failure = failure_reason(
        "AGENT_SESSION_LIMIT",
        "the durable session reached its storage limit",
    )?;
    let durable = session.is_durable();
    let turn_fallback = TurnEndReason::Error {
        error: if durable {
            session_limit_failure.clone()
        } else {
            budget_reason.clone()
        },
    };
    let mut reservation = session.reservation();
    let mut opening = reservation
        .claim_batch([
            NewEvent::log(EventKind::turn_start(turn)),
            NewEvent::log(EventKind::turn_end(turn, turn_fallback.clone())),
        ])
        .map_err(AgentLoopError::Admission)?;
    let mut turn_start = opening.remove(0);
    let mut turn_end = opening.remove(0);
    reservation.settle_exact_settled(&mut turn_start).await?;

    let mut driver = Driver {
        provider,
        tools,
        runtime,
        config,
        request_header_logged,
        counters: Counters::default(),
        final_message: None,
        observer_unavailable: false,
        session_limit_failure,
        durable_limit: None,
        deadline: Instant::now() + config.limits.turn_duration,
    };

    let mut reason = if cancellation.is_cancelled() {
        TurnEndReason::Aborted {
            reason: TurnEndCancelCause::User,
        }
    } else {
        match proposal {
            TurnProposal::Reject => TurnEndReason::Blocked,
            TurnProposal::Enter(messages) if messages.is_empty() => TurnEndReason::Completed,
            TurnProposal::Enter(messages) => {
                run_entered_turn(
                    &mut reservation,
                    &mut driver,
                    turn,
                    messages,
                    &cancellation,
                    &budget_reason,
                )
                .await?
            }
        }
    };

    let settlement = reservation
        .settle_settled(
            &mut turn_end,
            NewEvent::log(EventKind::turn_end(turn, reason.clone())),
        )
        .await?;
    let (turn_end_seq, used_fallback) = match settlement {
        ClaimedAppend::Preferred(receipt) => (receipt.seq(), false),
        ClaimedAppend::Fallback(receipt) => (receipt.seq(), true),
    };
    if used_fallback {
        reason = turn_fallback;
    }
    reservation.flush_barrier().await?;
    if let Some(error) = driver.durable_limit.take() {
        return Err(error.into());
    }
    if used_fallback && durable {
        return Err(crate::session::StoreError::Limit.into());
    }
    Ok(TurnOutcome {
        turn,
        turn_end_seq,
        reason,
        final_message: driver.final_message,
        steps: driver.counters.steps,
        attempts: driver.counters.attempts,
        retries: driver.counters.retries,
        tool_calls: driver.counters.tool_calls,
        reported_output_tokens: driver.counters.reported_output_tokens,
    })
}

async fn run_entered_turn(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    turn: TurnId,
    mut messages: Vec<Message>,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<TurnEndReason, AgentLoopError> {
    let mut summary_attempted = false;
    loop {
        if cancellation.is_cancelled() {
            return Ok(TurnEndReason::Aborted {
                reason: TurnEndCancelCause::User,
            });
        }
        if Instant::now() >= driver.deadline {
            return Ok(TurnEndReason::Error {
                error: failure_reason("AGENT_TURN_TIMEOUT", "the agent turn timed out")?,
            });
        }
        if driver.counters.steps >= driver.config.limits.max_steps_per_turn {
            return Ok(TurnEndReason::Error {
                error: failure_reason("AGENT_MAX_STEPS", "the agent reached its step limit")?,
            });
        }

        let step = StepId::new((driver.counters.steps + 1) as u64)
            .map_err(|_| AgentLoopError::Invariant("step identifier exhausted"))?;
        let mut exact = Vec::with_capacity(messages.len() + 2);
        exact.push(NewEvent::log(EventKind::step_start(turn, step)));
        exact.extend(messages.iter().cloned().map(|message| {
            NewEvent::surface(EventKind::user_message(message), SurfaceIntent::append())
        }));
        exact.push(NewEvent::log(EventKind::step_end(turn, step)));
        let claims = match reservation.claim_batch(exact) {
            Ok(claims) => claims,
            Err(error) if is_budget_error(&error) => {
                return Ok(TurnEndReason::Error {
                    error: driver.failure_for_budget(&error, budget_failure),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let unstarted = UnstartedStepClaims::new(claims)?;
        let first_preparation = if driver.counters.attempts
            >= driver.config.limits.max_attempts_per_turn
        {
            None
        } else {
            let mut preparation =
                prepare_conversation_request(reservation.session(), driver, messages.as_slice())?;
            if let Some(reason) = pre_step_stop(driver, cancellation)? {
                unstarted.release(reservation)?;
                return Ok(reason);
            }
            if matches!(preparation, AttemptPreparation::HardLimit { .. }) {
                let prune_outcome =
                    prune_hard_limited_request(reservation, driver, cancellation, budget_failure)
                        .await?;
                if let Some(reason) = pre_step_stop(driver, cancellation)? {
                    unstarted.release(reservation)?;
                    return Ok(reason);
                }
                match prune_outcome {
                    HardLimitPruneOutcome::Progress => {
                        if let Some(reason) = pre_step_stop(driver, cancellation)? {
                            unstarted.release(reservation)?;
                            return Ok(reason);
                        }
                        preparation = prepare_conversation_request(
                            reservation.session(),
                            driver,
                            messages.as_slice(),
                        )?;
                        if let Some(reason) = pre_step_stop(driver, cancellation)? {
                            unstarted.release(reservation)?;
                            return Ok(reason);
                        }
                    }
                    HardLimitPruneOutcome::Cancelled => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Aborted {
                            reason: TurnEndCancelCause::User,
                        });
                    }
                    HardLimitPruneOutcome::TurnError(error) => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Error { error });
                    }
                    HardLimitPruneOutcome::NoProgress => {}
                }
            }

            let mut context_window =
                preparation_context_window(&preparation, reservation.session());
            let mut trigger =
                pre_step_compaction_trigger(&preparation, reservation.session(), context_window);
            if summary_attempted && trigger == Some(crate::session::CompactionTrigger::Pressure) {
                trigger = None;
            }
            if trigger == Some(crate::session::CompactionTrigger::Pressure) {
                match prune_hard_limited_request(reservation, driver, cancellation, budget_failure)
                    .await?
                {
                    HardLimitPruneOutcome::Progress => {
                        preparation = prepare_conversation_request(
                            reservation.session(),
                            driver,
                            messages.as_slice(),
                        )?;
                        context_window =
                            preparation_context_window(&preparation, reservation.session());
                        trigger = pre_step_compaction_trigger(
                            &preparation,
                            reservation.session(),
                            context_window,
                        );
                    }
                    HardLimitPruneOutcome::NoProgress => {}
                    HardLimitPruneOutcome::Cancelled => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Aborted {
                            reason: TurnEndCancelCause::User,
                        });
                    }
                    HardLimitPruneOutcome::TurnError(error) => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Error { error });
                    }
                }
                if let Some(reason) = pre_step_stop(driver, cancellation)? {
                    unstarted.release(reservation)?;
                    return Ok(reason);
                }
            }
            if summary_attempted {
                if trigger == Some(crate::session::CompactionTrigger::HardLimit) {
                    unstarted.release(reservation)?;
                    return Ok(TurnEndReason::Error {
                        error: context_limit_failure()?,
                    });
                }
                trigger = None;
            }
            if let Some(trigger) = trigger {
                let was_hard_limit = trigger == crate::session::CompactionTrigger::HardLimit;
                let attempts_before_summary = driver.counters.attempts;
                let outcome = compact_once(
                    reservation,
                    driver,
                    turn,
                    trigger,
                    retained_token_target(context_window),
                    cancellation,
                    budget_failure,
                )
                .await?;
                summary_attempted |= driver.counters.attempts > attempts_before_summary;
                match outcome {
                    CompactionOutcome::Progress => {
                        if let Some(reason) = pre_step_stop(driver, cancellation)? {
                            unstarted.release(reservation)?;
                            return Ok(reason);
                        }
                        preparation = prepare_conversation_request(
                            reservation.session(),
                            driver,
                            messages.as_slice(),
                        )?;
                    }
                    CompactionOutcome::NoProgress if was_hard_limit => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Error {
                            error: context_limit_failure()?,
                        });
                    }
                    CompactionOutcome::NoProgress => {}
                    CompactionOutcome::AdvisoryFailure(_) if was_hard_limit => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Error {
                            error: context_limit_failure()?,
                        });
                    }
                    CompactionOutcome::AdvisoryFailure(_) => {}
                    CompactionOutcome::Cancelled => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Aborted {
                            reason: TurnEndCancelCause::User,
                        });
                    }
                    CompactionOutcome::TurnError(error) => {
                        unstarted.release(reservation)?;
                        return Ok(TurnEndReason::Error { error });
                    }
                }
            }
            if matches!(preparation, AttemptPreparation::HardLimit { .. }) {
                unstarted.release(reservation)?;
                return Ok(TurnEndReason::Error {
                    error: context_limit_failure()?,
                });
            }
            Some(preparation)
        };
        let mut step_end = unstarted.enter(reservation).await?;
        messages.clear();
        driver.counters.steps += 1;

        let mut attempt_token = None;
        let mut resolution = match AssertUnwindSafe(run_step(
            reservation,
            driver,
            StepExecution {
                turn,
                step,
                cancellation,
                budget_failure,
            },
            first_preparation,
            &mut attempt_token,
        ))
        .catch_unwind()
        .await
        {
            Ok(Ok(resolution)) => resolution,
            Ok(Err(error)) if reservation.has_pending_preferred_only_result() => {
                // An irreversible tool side effect already produced this
                // exact result. A second pre-commit failure leaves Session as
                // its sole owner, so step/end cannot truthfully pass it. Keep
                // the original cause visible and let shutdown/recovery finish
                // the append-only tail.
                return Err(error);
            }
            Ok(Err(error)) if is_fatal_loop_error(&error) => return Err(error),
            Ok(Err(_)) | Err(_) => StepResolution::new(StepOutcome::Error(failure_reason(
                "AGENT_INTERNAL",
                "the agent stopped after an internal failure",
            )?)),
        };
        let disposition = if attempt_token.is_some() {
            if matches!(&resolution.outcome, StepOutcome::Cancelled) {
                Some(AttemptDisposition::Cancelled)
            } else if matches!(&resolution.outcome, StepOutcome::Error(_)) {
                Some(AttemptDisposition::Failed)
            } else {
                // A successful model response must already have committed its
                // assistant closure and retired the attempt. Surface a stable
                // internal error, but still close the reserved step/end
                // instead of leaving a half-open durable turn.
                resolution = StepResolution::new(StepOutcome::Error(failure_reason(
                    "AGENT_INTERNAL",
                    "the agent did not finish its provider attempt",
                )?));
                Some(AttemptDisposition::Failed)
            }
        } else {
            None
        };
        reservation
            .settle_step_end_with_attempt_settled(
                &mut step_end,
                attempt_token.as_ref(),
                disposition,
            )
            .await?;
        if let Some(token) = attempt_token.as_ref() {
            match dispatch_barrier(reservation).await? {
                DispatchBarrier::Ready => {}
                DispatchBarrier::ObserverUnavailable => {
                    driver.observer_unavailable = true;
                    resolution =
                        StepResolution::new(StepOutcome::Error(observer_unavailable_failure()?));
                }
            }
            reservation.retire_attempt(token)?;
            attempt_token.take();
        }
        if driver.durable_limit.is_some() {
            return Ok(TurnEndReason::Error {
                error: driver.session_limit_failure.clone(),
            });
        }
        // Even a completely ready final step must give signals and output
        // deadlines one scheduling turn before the turn is classified.
        tokio::task::yield_now().await;
        match resolution.latched_turn_stop {
            ToolActionTurnStop::CallerCancelled => {
                return Ok(TurnEndReason::Aborted {
                    reason: TurnEndCancelCause::User,
                });
            }
            ToolActionTurnStop::TurnTimeout => {
                return Ok(TurnEndReason::Error {
                    error: failure_reason("AGENT_TURN_TIMEOUT", "the agent turn timed out")?,
                });
            }
            ToolActionTurnStop::None => {
                if cancellation.is_cancelled() {
                    return Ok(TurnEndReason::Aborted {
                        reason: TurnEndCancelCause::User,
                    });
                }
                if Instant::now() >= driver.deadline {
                    return Ok(TurnEndReason::Error {
                        error: failure_reason("AGENT_TURN_TIMEOUT", "the agent turn timed out")?,
                    });
                }
            }
        }
        match resolution.outcome {
            StepOutcome::Continue => {}
            StepOutcome::Completed => return Ok(TurnEndReason::Completed),
            StepOutcome::MaxTokens => return Ok(TurnEndReason::MaxTokens),
            StepOutcome::Cancelled => {
                return Ok(TurnEndReason::Aborted {
                    reason: TurnEndCancelCause::User,
                });
            }
            StepOutcome::Error(error) => return Ok(TurnEndReason::Error { error }),
        }
    }
}

fn pre_step_stop(
    driver: &Driver<'_>,
    cancellation: &CancellationToken,
) -> Result<Option<TurnEndReason>, AgentLoopError> {
    if driver.durable_limit.is_some() {
        return Ok(Some(TurnEndReason::Error {
            error: driver.session_limit_failure.clone(),
        }));
    }
    if cancellation.is_cancelled() {
        return Ok(Some(TurnEndReason::Aborted {
            reason: TurnEndCancelCause::User,
        }));
    }
    if Instant::now() >= driver.deadline {
        return Ok(Some(TurnEndReason::Error {
            error: failure_reason("AGENT_TURN_TIMEOUT", "the agent turn timed out")?,
        }));
    }
    Ok(None)
}

async fn prune_hard_limited_request(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<HardLimitPruneOutcome, AgentLoopError> {
    let result = reservation.prune_oversized_tool_results(cancellation).await;
    match result {
        Ok(report) if report.replacements() > 0 => Ok(HardLimitPruneOutcome::Progress),
        Ok(_) => Ok(HardLimitPruneOutcome::NoProgress),
        Err(error) => {
            let progress = error.progress().replacements() > 0;
            match error.cause() {
                ToolResultPrunePassCause::Cancelled
                | ToolResultPrunePassCause::Read(SessionReadError::Cancelled) => {
                    Ok(HardLimitPruneOutcome::Cancelled)
                }
                ToolResultPrunePassCause::Read(SessionReadError::Storage(error)) => {
                    Err((*error).into())
                }
                ToolResultPrunePassCause::Read(SessionReadError::Corrupt) => {
                    Err(AppendError::DurablePoisoned.into())
                }
                ToolResultPrunePassCause::Barrier(BarrierError::ObserverUnavailable) => {
                    driver.observer_unavailable = true;
                    Ok(HardLimitPruneOutcome::TurnError(
                        observer_unavailable_failure()?,
                    ))
                }
                ToolResultPrunePassCause::Barrier(error) => Err(error.clone().into()),
                ToolResultPrunePassCause::Read(SessionReadError::Append(error)) => {
                    classify_prune_append_error(driver, error, budget_failure, progress)
                }
                ToolResultPrunePassCause::Pair(error) => {
                    let source = match error {
                        crate::session::PrunePairAppendError::BeforeMarker(source)
                        | crate::session::PrunePairAppendError::MarkerCommitted {
                            source, ..
                        } => source,
                    };
                    classify_prune_append_error(driver, source, budget_failure, progress)
                }
                ToolResultPrunePassCause::Capacity
                | ToolResultPrunePassCause::Transform(_)
                | ToolResultPrunePassCause::Read(SessionReadError::Changed) => Ok(if progress {
                    HardLimitPruneOutcome::Progress
                } else {
                    HardLimitPruneOutcome::NoProgress
                }),
            }
        }
    }
}

fn classify_prune_append_error(
    driver: &mut Driver<'_>,
    error: &AppendError,
    budget_failure: &LlmFailure,
    progress: bool,
) -> Result<HardLimitPruneOutcome, AgentLoopError> {
    if is_budget_error(error) {
        return Ok(HardLimitPruneOutcome::TurnError(
            driver.failure_for_budget(error, budget_failure),
        ));
    }
    let loop_error = AgentLoopError::Session(error.clone());
    if is_fatal_loop_error(&loop_error) {
        return Err(loop_error);
    }
    Ok(if progress {
        HardLimitPruneOutcome::Progress
    } else {
        HardLimitPruneOutcome::NoProgress
    })
}

fn prepare_conversation_request(
    session: &Session,
    driver: &Driver<'_>,
    pending_messages: &[Message],
) -> Result<AttemptPreparation, AgentLoopError> {
    if session.context_total_tokens().is_err() {
        return Ok(AttemptPreparation::DeferredFailure(failure_reason(
            "AGENT_INTERNAL",
            "the agent stopped after an internal failure",
        )?));
    }
    let source_surface_generation = session.surface_generation();
    let pending_generation = u64::try_from(pending_messages.len())
        .map_err(|_| AgentLoopError::Invariant("pending message count exceeded u64"))?;
    let expected_surface_generation = source_surface_generation
        .checked_add(pending_generation)
        .ok_or(AgentLoopError::Invariant("surface generation exhausted"))?;
    let proposed = match proposed_config(
        driver.config,
        session.request_header(),
        *driver.request_header_logged,
    ) {
        Ok(proposed) => proposed,
        Err(_) => {
            return Ok(AttemptPreparation::DeferredFailure(failure_reason(
                "AGENT_INTERNAL",
                "the agent stopped after an internal failure",
            )?));
        }
    };
    let messages = match session.try_messages_with(pending_messages) {
        Ok(messages) => messages,
        Err(()) => {
            return Ok(AttemptPreparation::DeferredFailure(failure_reason(
                "AGENT_REQUEST",
                "model request construction failed",
            )?));
        }
    };
    let draft = match conversation_request_draft(&proposed, &messages, driver.config, session) {
        Ok(draft) => draft,
        Err(error) if is_hard_request_limit(&error) => {
            return Ok(AttemptPreparation::HardLimit { prepared: None });
        }
        Err(error) => {
            return Ok(AttemptPreparation::DeferredFailure(failure_from_display(
                "AGENT_REQUEST",
                "model request construction failed",
                &error,
            )?));
        }
    };
    let preflight = match catch_unwind(AssertUnwindSafe(|| {
        driver.provider.preflight_request(draft)
    })) {
        Ok(Ok(preflight)) => preflight,
        Ok(Err(
            ProviderPreflightError::WireTooLarge { prepared, .. }
            | ProviderPreflightError::RequestLimit { prepared },
        )) => {
            return Ok(AttemptPreparation::HardLimit {
                prepared: Some(prepared),
            });
        }
        Ok(Err(ProviderPreflightError::Preparation(error))) => {
            return Ok(AttemptPreparation::DeferredFailure(failure_from_display(
                "AGENT_PROVIDER_PREPARE",
                "provider preparation failed",
                &error,
            )?));
        }
        Ok(Err(ProviderPreflightError::InvalidRequest { failure, prepared })) => {
            return Ok(AttemptPreparation::PreparedFailure { prepared, failure });
        }
        Err(_) => {
            return Ok(AttemptPreparation::DeferredFailure(failure_reason(
                "AGENT_PROVIDER_PANIC",
                "the provider panicked while preparing a request",
            )?));
        }
    };
    Ok(AttemptPreparation::Ready(PreflightedRequest {
        proposed,
        messages,
        expected_surface_generation,
        preflight,
    }))
}

fn conversation_request_draft<'a>(
    proposed: &'a LlmCallConfig,
    messages: &'a [Message],
    config: &'a AgentLoopConfig,
    session: &'a Session,
) -> Result<ProviderRequestDraft<'a>, ProviderRequestError> {
    let mut draft = ProviderRequestDraft::new(proposed, messages)?;
    if let Some(system) = &config.system {
        draft = draft.with_system(system)?;
    }
    if !config.tools.is_empty() {
        draft = draft.with_tools(&config.tools)?;
    }
    draft.with_session_id(session.id())
}

fn is_hard_request_limit(error: &ProviderRequestError) -> bool {
    matches!(
        error,
        ProviderRequestError::TooManyMessages { .. }
            | ProviderRequestError::TooLarge { .. }
            | ProviderRequestError::SizeOverflow
    )
}

fn context_limit_failure() -> Result<LlmFailure, AgentLoopError> {
    failure_reason(
        "AGENT_CONTEXT_LIMIT",
        "the conversation cannot be reduced to fit the model request limits",
    )
}

struct StepExecution<'a> {
    turn: TurnId,
    step: StepId,
    cancellation: &'a CancellationToken,
    budget_failure: &'a LlmFailure,
}

async fn run_step(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    execution: StepExecution<'_>,
    mut first_preparation: Option<AttemptPreparation>,
    attempt_token: &mut Option<AttemptToken>,
) -> Result<StepResolution, AgentLoopError> {
    let StepExecution {
        turn,
        step,
        cancellation,
        budget_failure,
    } = execution;
    let mut retry_chains: BTreeMap<(String, String), (RetryId, usize)> = BTreeMap::new();
    let mut retries_in_step = 0_usize;
    loop {
        if cancellation.is_cancelled() {
            return Ok(StepResolution::new(StepOutcome::Cancelled));
        }
        if Instant::now() >= driver.deadline {
            return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                "AGENT_TURN_TIMEOUT",
                "the agent turn timed out",
            )?)));
        }
        if driver.counters.attempts >= driver.config.limits.max_attempts_per_turn {
            return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                "AGENT_MAX_MODEL_ATTEMPTS",
                "the agent reached its model-attempt limit",
            )?)));
        }
        driver.counters.attempts += 1;

        let preparation = match first_preparation.take() {
            Some(preparation) => preparation,
            None => prepare_conversation_request(reservation.session(), driver, &[])?,
        };
        let (
            preflighted,
            effective_config,
            retry_policy,
            adapter_defaults,
            context_window,
            prepared_failure,
        ) = match preparation {
            AttemptPreparation::Ready(preflighted) => {
                let prepared = preflighted.prepared_call();
                let effective_config = prepared.config().clone();
                let retry_policy = prepared.retry_policy().clone();
                let adapter_defaults = prepared.adapter_defaults().clone();
                let context_window = prepared.context_window();
                (
                    Some(preflighted),
                    effective_config,
                    retry_policy,
                    adapter_defaults,
                    context_window,
                    None,
                )
            }
            AttemptPreparation::PreparedFailure { prepared, failure } => (
                None,
                prepared.config().clone(),
                prepared.retry_policy().clone(),
                prepared.adapter_defaults().clone(),
                prepared.context_window(),
                Some(failure),
            ),
            AttemptPreparation::DeferredFailure(failure) => {
                return Ok(StepResolution::new(StepOutcome::Error(failure)));
            }
            AttemptPreparation::HardLimit { prepared } => {
                let _context_window = prepared.and_then(|prepared| prepared.context_window());
                return Ok(StepResolution::new(StepOutcome::Error(
                    context_limit_failure()?,
                )));
            }
        };
        if !effective_config.max_tokens().is_some_and(|maximum| {
            maximum.get() > 0 && maximum.get() <= driver.config.limits.max_output_tokens_per_request
        }) {
            return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                "AGENT_MAX_OUTPUT_TOKENS",
                "the prepared model request exceeds the agent output-token limit",
            )?)));
        }
        let header = EpochHeader {
            config: effective_config.clone(),
            adapter_defaults: Some(adapter_defaults),
            system: driver.config.system.clone(),
            tools: (!driver.config.tools.is_empty()).then(|| driver.config.tools.clone()),
        }
        .canonicalized();

        let force_header = !*driver.request_header_logged;
        let header_changed = reservation
            .session()
            .request_header()
            .is_none_or(|previous| !previous.equivalent_to(&header));
        if force_header || header_changed {
            let reason = if force_header {
                if reservation.session().request_header().is_some() {
                    RequestHeaderReason::Resume
                } else {
                    RequestHeaderReason::Initial
                }
            } else {
                RequestHeaderReason::Change
            };
            match reservation
                .append_settled(NewEvent::log(EventKind::RequestHeader {
                    header: header.clone(),
                    reason,
                }))
                .await
            {
                Ok(_) => *driver.request_header_logged = true,
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepResolution::new(StepOutcome::Error(
                        driver.failure_for_budget(&error, budget_failure),
                    )));
                }
                Err(error) => return Err(error.into()),
            }
        }
        let context = RequestContext::new(
            effective_config.provider(),
            effective_config.model(),
            context_window,
        )?;
        let context_changed = reservation
            .session()
            .request_context()
            .is_none_or(|previous| !previous.equivalent_to(&context));
        if context_changed {
            match reservation
                .append_settled(NewEvent::log(EventKind::RequestContext {
                    context: context.clone(),
                }))
                .await
            {
                Ok(_) => {}
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepResolution::new(StepOutcome::Error(
                        driver.failure_for_budget(&error, budget_failure),
                    )));
                }
                Err(error) => return Err(error.into()),
            }
        }

        if let Some(failure) = prepared_failure {
            return Ok(StepResolution::new(StepOutcome::Error(failure)));
        }
        let preflighted = preflighted.ok_or(AgentLoopError::Invariant(
            "a prepared request disappeared before dispatch",
        ))?;

        if !preflighted.matches_surface(reservation.session()) {
            return Err(AgentLoopError::Invariant(
                "model-visible surface changed after provider preflight",
            ));
        }
        let request = match preflighted.into_request(reservation.session(), driver.config) {
            Ok(request) => request,
            Err(error) => {
                return Ok(StepResolution::new(StepOutcome::Error(
                    failure_from_display(
                        "AGENT_REQUEST",
                        "model request construction failed",
                        &error,
                    )?,
                )));
            }
        };

        if dispatch_barrier(reservation).await? == DispatchBarrier::ObserverUnavailable {
            driver.observer_unavailable = true;
            return Ok(StepResolution::new(StepOutcome::Error(
                observer_unavailable_failure()?,
            )));
        }
        if cancellation.is_cancelled() {
            return Ok(StepResolution::new(StepOutcome::Cancelled));
        }
        if Instant::now() >= driver.deadline {
            return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                "AGENT_TURN_TIMEOUT",
                "the agent turn timed out",
            )?)));
        }

        if attempt_token.is_some() {
            return Err(AgentLoopError::Invariant(
                "a previous provider attempt was not retired before dispatch",
            ));
        }
        *attempt_token = Some(reservation.begin_attempt(turn, step)?);
        let attempt_cancellation = cancellation.child_token();
        let stream = match catch_unwind(AssertUnwindSafe(|| {
            driver
                .provider
                .stream(request, attempt_cancellation.clone())
        })) {
            Ok(stream) => stream,
            Err(_) => {
                attempt_cancellation.cancel();
                return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                    "AGENT_PROVIDER_PANIC",
                    "the provider panicked while opening a stream",
                )?)));
            }
        };
        let streamed = consume_stream(
            reservation,
            driver,
            turn,
            step,
            attempt_token.as_ref().ok_or(AgentLoopError::Invariant(
                "provider attempt owner disappeared before streaming",
            ))?,
            stream,
            cancellation,
            budget_failure,
        )
        .await;
        if !matches!(streamed, Ok(StreamOutcome::Finished(_))) {
            attempt_cancellation.cancel();
        }
        let streamed = streamed?;
        let prepared_attempt = match streamed {
            StreamOutcome::Cancelled => {
                return Ok(StepResolution::new(StepOutcome::Cancelled));
            }
            StreamOutcome::Error(error) => {
                return Ok(StepResolution::new(StepOutcome::Error(error)));
            }
            StreamOutcome::Finished(prepared) => prepared,
        };

        let prepared = prepared_attempt.into_parts();
        let assembled = AssembledAssistant {
            content: prepared.content,
            usage: prepared.usage,
            finish: prepared.finish,
            replay_state: prepared.replay_state,
            _resident_guard: prepared.resident_guard,
        };
        let source_seqs = prepared.sources;

        // Cancellation can race with the provider's final item. Re-check it
        // before publishing an assistant message or starting any tool work.
        if cancellation.is_cancelled() {
            attempt_cancellation.cancel();
            return Ok(StepResolution::new(StepOutcome::Cancelled));
        }
        if Instant::now() >= driver.deadline {
            attempt_cancellation.cancel();
            return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                "AGENT_TURN_TIMEOUT",
                "the agent turn timed out",
            )?)));
        }

        let provider_failure = match assembled.finish.kind() {
            FinishReasonKind::Error { failure } | FinishReasonKind::Aborted { failure } => {
                Some(failure.clone())
            }
            _ => None,
        };
        if let Some(failure) = provider_failure {
            if cancellation.is_cancelled() {
                return Ok(StepResolution::new(StepOutcome::Cancelled));
            }
            let key = policy_key(&retry_policy)
                .map_err(|error| AgentLoopError::Serialization(error.to_string()))?;
            let chain_key = (effective_config.provider().to_owned(), key.clone());
            let prior = retry_chains.get(&chain_key).map_or(0, |(_, prior)| *prior);
            let next_retry = prior
                .checked_add(1)
                .ok_or(AgentLoopError::Invariant("retry number exhausted"))?;
            let initial_decision = decide(&retry_policy, &failure, next_retry, None);
            if matches!(initial_decision, RetryDecision::Stop) {
                return Ok(StepResolution::new(StepOutcome::Error(failure)));
            }
            if retries_in_step >= driver.config.limits.max_retries_per_step {
                return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                    "AGENT_MAX_RETRIES",
                    "the agent reached its retry limit",
                )?)));
            }
            if driver.counters.attempts >= driver.config.limits.max_attempts_per_turn {
                return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                    "AGENT_MAX_MODEL_ATTEMPTS",
                    "the agent reached its model-attempt limit",
                )?)));
            }
            let decision = match initial_decision {
                RetryDecision::NeedsSample => decide(
                    &retry_policy,
                    &failure,
                    next_retry,
                    Some(checked_sample(driver.runtime)?),
                ),
                decision => decision,
            };
            let RetryDecision::Retry { delay_ms } = decision else {
                return Ok(StepResolution::new(StepOutcome::Error(failure)));
            };
            let retry_id = match retry_chains.get(&chain_key) {
                Some((retry_id, _)) => retry_id.clone(),
                None => RetryId::new(next_id(driver.runtime, AgentIdKind::Retry)?),
            };
            let number = RetryNumber::new(next_retry as u64)
                .map_err(|_| AgentLoopError::Invariant("retry number exhausted"))?;
            let retry_event = match retry_policy.mode() {
                RetryMode::Normal => {
                    let maximum = retry_policy.max_retries().ok_or(AgentLoopError::Invariant(
                        "normal retry policy omitted maxRetries",
                    ))?;
                    let maximum = RetryNumber::new(maximum.get()).map_err(|_| {
                        AgentLoopError::Invariant("scheduled retry has zero maxRetries")
                    })?;
                    LlmRetryEvent::normal(
                        retry_id.clone(),
                        turn,
                        step,
                        effective_config.provider(),
                        key.clone(),
                        number,
                        maximum,
                        delay_ms,
                        failure,
                    )?
                }
                RetryMode::Always => LlmRetryEvent::always(
                    retry_id.clone(),
                    turn,
                    step,
                    effective_config.provider(),
                    key.clone(),
                    number,
                    delay_ms,
                    failure,
                )?,
            };
            let token = attempt_token.as_ref().ok_or(AgentLoopError::Invariant(
                "failed provider attempt lost its Session owner",
            ))?;
            match reservation
                .append_attempt_closure_settled(
                    token,
                    AttemptDisposition::Retry,
                    NewEvent::log(EventKind::llm_retry(retry_event)),
                )
                .await
            {
                Ok(_) => {}
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepResolution::new(StepOutcome::Error(
                        driver.failure_for_budget(&error, budget_failure),
                    )));
                }
                Err(error) => return Err(error.into()),
            }
            // The retry row now owns every fact needed after the failed
            // attempt. Drop every moved-out attempt payload while the Session
            // guard still covers it, before waiting for durable dispatch.
            drop(assembled);
            drop(source_seqs);
            let barrier = dispatch_barrier(reservation).await?;
            reservation.retire_attempt(token)?;
            attempt_token.take();
            if barrier == DispatchBarrier::ObserverUnavailable {
                driver.observer_unavailable = true;
                return Ok(StepResolution::new(StepOutcome::Error(
                    observer_unavailable_failure()?,
                )));
            }
            retry_chains.insert(chain_key, (retry_id.clone(), prior + 1));
            retries_in_step += 1;
            driver.counters.retries += 1;

            let delay = Duration::from_secs_f64(delay_ms.get() / 1_000.0);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => {
                    return Ok(StepResolution::new(StepOutcome::Cancelled));
                }
                _ = tokio::time::sleep_until(driver.deadline) => {
                    return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                        "AGENT_TURN_TIMEOUT",
                        "the agent turn timed out",
                    )?)));
                }
                _ = tokio::time::sleep(delay) => {}
            }
            tokio::task::yield_now().await;
            if cancellation.is_cancelled() {
                return Ok(StepResolution::new(StepOutcome::Cancelled));
            }
            if Instant::now() >= driver.deadline {
                return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                    "AGENT_TURN_TIMEOUT",
                    "the agent turn timed out",
                )?)));
            }
            let started = LlmRetryStartedEvent::new(retry_id, turn, step, number)?;
            match reservation
                .append_settled(NewEvent::log(EventKind::llm_retry_started(started)))
                .await
            {
                Ok(_) => {}
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepResolution::new(StepOutcome::Error(
                        driver.failure_for_budget(&error, budget_failure),
                    )));
                }
                Err(error) => return Err(error.into()),
            }
            continue;
        }

        return commit_successful_attempt(
            reservation,
            driver,
            turn,
            step,
            effective_config,
            assembled,
            source_seqs,
            attempt_token,
            cancellation,
            budget_failure,
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn consume_stream(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    _turn: TurnId,
    _step: StepId,
    attempt: &AttemptToken,
    mut stream: crate::provider::ProviderStream,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<StreamOutcome, AgentLoopError> {
    let mut ready_chunks = 0_usize;
    loop {
        let next = AssertUnwindSafe(stream.next()).catch_unwind();
        let item = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Ok(StreamOutcome::Cancelled),
            _ = tokio::time::sleep_until(driver.deadline) => {
                return Ok(StreamOutcome::Error(failure_reason(
                    "AGENT_TURN_TIMEOUT",
                    "the agent turn timed out",
                )?));
            }
            item = next => match item {
                Ok(item) => item,
                Err(_) => return Ok(StreamOutcome::Error(failure_reason(
                    "AGENT_PROVIDER_PANIC",
                    "the provider stream panicked",
                )?)),
            },
        };
        let Some(item) = item else {
            return match reservation.seal_attempt(attempt) {
                Ok(prepared) => Ok(StreamOutcome::Finished(prepared)),
                Err(AppendError::Validation(_)) => Ok(StreamOutcome::Error(failure_reason(
                    "AGENT_PROVIDER_PROTOCOL",
                    "the provider stream ended incorrectly",
                )?)),
                Err(error) if is_budget_error(&error) => Ok(StreamOutcome::Error(
                    driver.failure_for_budget(&error, budget_failure),
                )),
                Err(error) => Err(error.into()),
            };
        };
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(error) => {
                return Ok(StreamOutcome::Error(failure_from_display(
                    "AGENT_PROVIDER_STREAM",
                    "the provider stream failed",
                    &error,
                )?));
            }
        };
        let reported_output_tokens = match chunk.kind() {
            StreamChunkKind::Usage { usage } => Some(usage.output_tokens().get()),
            _ => None,
        };
        match reservation
            .append_attempt_chunk_settled(attempt, chunk)
            .await
        {
            Ok(_) => {}
            Err(error) if is_budget_error(&error) => {
                return Ok(StreamOutcome::Error(
                    driver.failure_for_budget(&error, budget_failure),
                ));
            }
            Err(AppendError::Validation(_)) => {
                return Ok(StreamOutcome::Error(failure_reason(
                    "AGENT_PROVIDER_PROTOCOL",
                    "the provider emitted an invalid stream",
                )?));
            }
            Err(error) => return Err(error.into()),
        }
        if let Some(output_tokens) = reported_output_tokens {
            driver.counters.reported_output_tokens = driver
                .counters
                .reported_output_tokens
                .checked_add(output_tokens)
                .unwrap_or(u64::MAX);
            if driver.counters.reported_output_tokens
                > driver.config.limits.max_reported_output_tokens_per_turn
            {
                return Ok(StreamOutcome::Error(failure_reason(
                    "AGENT_TOKEN_BUDGET",
                    "the agent reached its reported output-token limit",
                )?));
            }
        }
        ready_chunks += 1;
        if ready_chunks == AGENT_READY_WORK_BUDGET {
            ready_chunks = 0;
            tokio::task::yield_now().await;
            if cancellation.is_cancelled() {
                return Ok(StreamOutcome::Cancelled);
            }
            if Instant::now() >= driver.deadline {
                return Ok(StreamOutcome::Error(failure_reason(
                    "AGENT_TURN_TIMEOUT",
                    "the agent turn timed out",
                )?));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn commit_successful_attempt(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    turn: TurnId,
    step: StepId,
    config: LlmCallConfig,
    assembled: AssembledAssistant,
    source_seqs: Vec<EventSeq>,
    attempt_token: &mut Option<AttemptToken>,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<StepResolution, AgentLoopError> {
    let AssembledAssistant {
        mut content,
        usage,
        finish,
        replay_state,
        _resident_guard: resident_guard,
    } = assembled;
    let max_tokens = matches!(finish.kind(), FinishReasonKind::MaxTokens);
    // FinishReason is attempt-only on every successful path. Release this
    // alias while the Session attempt owner still covers the allocation, so
    // tool execution cannot outlive the credit that accounted for it.
    drop(finish);
    if max_tokens {
        content = without_tool_calls(content);
    }
    let message = Message::new(
        next_id(driver.runtime, AgentIdKind::Message)?,
        MessageRole::Assistant,
        content,
        MessageSource::model_with_replay_state(config.provider(), config.model(), replay_state)?,
    )?;
    let tool_calls = message
        .content()
        .iter()
        .filter_map(|block| match block.kind() {
            ContentBlockKind::ToolCall {
                id,
                name,
                arguments,
            } => Some(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: arguments.clone(),
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Err(code) = validate_tool_calls(driver, &tool_calls) {
        let message = if code == "AGENT_MAX_TOOL_CALLS" {
            "the agent reached its tool-call limit"
        } else {
            "the model produced invalid or duplicate tool calls"
        };
        return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
            code, message,
        )?)));
    }
    let mut claim_profiles = Vec::with_capacity(tool_calls.len());
    for call in &tool_calls {
        let profile = match catch_unwind(AssertUnwindSafe(|| {
            driver.tools.claim_profile(call.name.as_str())
        })) {
            Ok(profile) => profile,
            Err(_) => {
                return Ok(StepResolution::new(StepOutcome::Error(failure_reason(
                    "AGENT_TOOL_EXECUTOR",
                    "the tool executor failed while planning result capacity",
                )?)));
            }
        };
        claim_profiles.push(profile);
    }
    let assistant = NewEvent::surface(
        EventKind::AssistantMessage {
            turn,
            step,
            message,
            usage,
        },
        SurfaceIntent::append().with_sources(source_seqs),
    );
    if tool_calls.is_empty() {
        let token = attempt_token.as_ref().ok_or(AgentLoopError::Invariant(
            "successful provider attempt lost its Session owner",
        ))?;
        match reservation
            .append_attempt_closure_settled(token, AttemptDisposition::Committed, assistant)
            .await
        {
            Ok(receipt) => driver.observe_assistant_commit(&receipt),
            Err(error) if is_budget_error(&error) => {
                return Ok(StepResolution::new(StepOutcome::Error(
                    driver.failure_for_budget(&error, budget_failure),
                )));
            }
            Err(error) => return Err(error.into()),
        }
        let barrier = dispatch_barrier(reservation).await?;
        reservation.retire_attempt(token)?;
        attempt_token.take();
        drop(resident_guard);
        if barrier == DispatchBarrier::ObserverUnavailable {
            driver.observer_unavailable = true;
            return Ok(StepResolution::new(StepOutcome::Error(
                observer_unavailable_failure()?,
            )));
        }
        return Ok(StepResolution::new(if max_tokens {
            StepOutcome::MaxTokens
        } else {
            StepOutcome::Completed
        }));
    }

    commit_tool_round(
        reservation,
        driver,
        turn,
        step,
        assistant,
        tool_calls,
        claim_profiles,
        attempt_token,
        resident_guard,
        cancellation,
        budget_failure,
    )
    .await
}

#[derive(Clone)]
struct ToolCall {
    id: CallId,
    name: String,
    arguments: String,
}

fn validate_tool_calls(driver: &Driver<'_>, calls: &[ToolCall]) -> Result<(), &'static str> {
    if calls.len() > driver.config.limits.max_tool_calls_per_step
        || driver.counters.tool_calls.saturating_add(calls.len())
            > driver.config.limits.max_tool_calls_per_turn
    {
        return Err("AGENT_MAX_TOOL_CALLS");
    }
    let mut ids = BTreeSet::new();
    if calls.iter().any(|call| {
        call.id.is_empty()
            || call.id.as_str().len() > 1_024
            || call.id.as_str().chars().any(char::is_control)
            || call.name.is_empty()
            || call.name.len() > 256
            || call.name.chars().any(char::is_control)
            || !ids.insert(call.id.clone())
    }) {
        return Err("AGENT_INVALID_TOOL_CALL");
    }
    Ok(())
}

struct PlannedTool {
    call: ToolCall,
    claim_profile: ToolClaimProfile,
    dispatch: ToolDispatchBinding,
    call_seq: EventSeq,
    result_message_id: String,
    call_claim: EventClaim,
    result_claim: EventClaim,
}

#[allow(clippy::too_many_arguments)]
async fn commit_tool_round(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    turn: TurnId,
    step: StepId,
    assistant: NewEvent,
    calls: Vec<ToolCall>,
    claim_profiles: Vec<ToolClaimProfile>,
    attempt_token: &mut Option<AttemptToken>,
    resident_guard: Option<AttemptResidentGuard>,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<StepResolution, AgentLoopError> {
    if calls.len() != claim_profiles.len() {
        return Err(AgentLoopError::Invariant(
            "tool calls and claim profiles have different lengths",
        ));
    }
    let mut result_ids = Vec::with_capacity(calls.len());
    let mut fallbacks = Vec::with_capacity(1 + calls.len() * 2);
    fallbacks.push(assistant);
    let maximum_source_seq = EventSeq::new(crate::session::MAX_SAFE_INTEGER)
        .map_err(|_| AgentLoopError::Invariant("maximum event sequence is invalid"))?;
    for call in &calls {
        let result_id = next_id(driver.runtime, AgentIdKind::Message)?;
        fallbacks.push(NewEvent::log(EventKind::tool_call(
            turn,
            step,
            call.id.clone(),
            call.name.clone(),
            call.arguments.clone(),
        )));
        let profile = claim_profiles[result_ids.len()];
        fallbacks.push(if profile.is_shell_action() {
            shell_prestart_error_event(
                turn,
                step,
                &result_id,
                call,
                maximum_source_seq,
                "TOOL_OUTPUT_BUDGET_EXCEEDED",
                call.name.as_str(),
                "shell output could not fit safely in the session",
                None,
            )?
        } else {
            tool_error_event(
                turn,
                step,
                &result_id,
                call,
                maximum_source_seq,
                "TOOL_OUTPUT_BUDGET_EXCEEDED",
                call.name.as_str(),
                "tool output could not fit safely in the session",
            )?
        });
        result_ids.push(result_id);
    }
    let mut claims = match reservation.claim_batch(fallbacks) {
        Ok(claims) => claims,
        Err(error) if is_budget_error(&error) => {
            return Ok(StepResolution::new(StepOutcome::Error(
                driver.failure_for_budget(&error, budget_failure),
            )));
        }
        Err(error) => return Err(error.into()),
    };
    let mut assistant_claim = claims.remove(0);
    let mut planned = Vec::with_capacity(calls.len());
    for ((call, result_message_id), claim_profile) in
        calls.into_iter().zip(result_ids).zip(claim_profiles)
    {
        let call_claim = claims.remove(0);
        let result_claim = claims.remove(0);
        planned.push(PlannedTool {
            call,
            claim_profile,
            dispatch: ToolDispatchBinding::new(),
            call_seq: maximum_source_seq,
            result_message_id,
            call_claim,
            result_claim,
        });
    }
    for index in 0..planned.len() {
        if !planned[index].claim_profile.is_shell_action() {
            continue;
        }
        let ceiling = shell_prestart_claim_ceiling(
            turn,
            step,
            &planned[index].result_message_id,
            &planned[index].call,
            maximum_source_seq,
        )?;
        if let Err(error) =
            reservation.reserve_claim_retained_json_bytes(&mut planned[index].result_claim, ceiling)
        {
            release_uncommitted_tool_round(reservation, &mut assistant_claim, &mut planned)?;
            return if is_budget_error(&error) {
                Ok(StepResolution::new(StepOutcome::Error(
                    driver.failure_for_budget(&error, budget_failure),
                )))
            } else {
                Err(error.into())
            };
        }
    }
    let token = attempt_token.as_ref().ok_or(AgentLoopError::Invariant(
        "successful provider attempt lost its Session owner",
    ))?;
    let assistant_receipt = reservation
        .settle_attempt_closure_exact_settled(
            &mut assistant_claim,
            token,
            AttemptDisposition::Committed,
        )
        .await?;
    driver.observe_assistant_commit(&assistant_receipt);
    let barrier = dispatch_barrier(reservation).await?;
    reservation.retire_attempt(token)?;
    attempt_token.take();
    drop(resident_guard);
    if barrier == DispatchBarrier::ObserverUnavailable {
        driver.observer_unavailable = true;
    }
    driver.counters.tool_calls += planned.len();

    let mut cancelled = false;
    let mut concludes_turn = false;
    let mut infrastructure_failure = None;
    let mut latched_stop = ToolStop::None;
    for index in 0..planned.len() {
        let (completed, remaining) = planned.split_at_mut(index + 1);
        let plan = &mut completed[index];
        let actual_call_seq = reservation
            .settle_exact_settled(&mut plan.call_claim)
            .await?
            .seq();
        plan.call_seq = actual_call_seq;
        reservation.rebind_claim_fallback(
            &mut plan.result_claim,
            if plan.claim_profile.is_shell_action() {
                shell_prestart_error_event(
                    turn,
                    step,
                    &plan.result_message_id,
                    &plan.call,
                    actual_call_seq,
                    "TOOL_OUTPUT_BUDGET_EXCEEDED",
                    plan.call.name.as_str(),
                    "shell output could not fit safely in the session",
                    None,
                )?
            } else {
                tool_error_event(
                    turn,
                    step,
                    &plan.result_message_id,
                    &plan.call,
                    actual_call_seq,
                    "TOOL_OUTPUT_BUDGET_EXCEEDED",
                    plan.call.name.as_str(),
                    "tool output could not fit safely in the session",
                )?
            },
        )?;
        let result = if driver.durable_limit.is_some() {
            prestart_failure(
                plan,
                "SESSION_LIMIT",
                "tool was not started because the durable session reached its storage limit",
                ToolStop::None,
            )
        } else if infrastructure_failure.is_some()
            || driver.observer_unavailable
            || cancelled
            || cancellation.is_cancelled()
        {
            cancelled |= cancellation.is_cancelled();
            if plan.claim_profile.is_shell_action() {
                prestart_failure(
                    plan,
                    "ABORTED_BEFORE_DISPATCH",
                    "tool was not started because the turn was stopping",
                    if cancellation.is_cancelled() {
                        ToolStop::Cancelled
                    } else {
                        ToolStop::None
                    },
                )
            } else {
                ToolRun::ModelError {
                    code: "ABORTED_BEFORE_DISPATCH",
                    message: "tool was not started because the turn was stopping",
                }
            }
        } else if dispatch_barrier(reservation).await? == DispatchBarrier::ObserverUnavailable {
            driver.observer_unavailable = true;
            prestart_failure(
                plan,
                "ABORTED_BEFORE_DISPATCH",
                "tool was not started because the live session observer became unavailable",
                ToolStop::None,
            )
        } else {
            run_one_tool(driver, plan, cancellation).await
        };
        match result {
            ToolRun::Completed {
                result,
                settlement,
                stop,
            } => {
                let requested_conclusion = result.concludes_turn();
                let committed_preferred =
                    settle_tool_result(reservation, driver, plan, result, settlement).await?;
                concludes_turn |= requested_conclusion && committed_preferred;
                latched_stop = latch_tool_stop(latched_stop, stop);
                match stop {
                    ToolStop::None => {}
                    ToolStop::Cancelled => cancelled = true,
                    ToolStop::TurnTimeout => {
                        infrastructure_failure = Some(failure_reason(
                            "AGENT_TURN_TIMEOUT",
                            "the agent turn timed out",
                        )?);
                    }
                }
            }
            ToolRun::Mutation(mutation) => {
                let resolved =
                    resolve_mutation(reservation, driver, plan, mutation, cancellation).await?;
                match resolved {
                    ToolRun::Completed {
                        result,
                        settlement,
                        stop,
                    } => {
                        let requested_conclusion = result.concludes_turn();
                        let committed_preferred =
                            settle_tool_result(reservation, driver, plan, result, settlement)
                                .await?;
                        concludes_turn |= requested_conclusion && committed_preferred;
                        latched_stop = latch_tool_stop(latched_stop, stop);
                        match stop {
                            ToolStop::None => {}
                            ToolStop::Cancelled => cancelled = true,
                            ToolStop::TurnTimeout => {
                                infrastructure_failure = Some(failure_reason(
                                    "AGENT_TURN_TIMEOUT",
                                    "the agent turn timed out",
                                )?);
                            }
                        }
                    }
                    ToolRun::Infrastructure { stop } => {
                        if reservation.session().is_durable() {
                            settle_unknown_tool_outcome(reservation, plan, turn, step).await?;
                            infrastructure_failure = Some(failure_reason(
                                "AGENT_TOOL_EXECUTOR",
                                "the prepared file mutation failed without a definite outcome",
                            )?);
                            latched_stop = latch_tool_stop(latched_stop, stop);
                            continue;
                        }
                        reservation.release(&mut plan.result_claim)?;
                        for later in remaining {
                            reservation.release(&mut later.call_claim)?;
                            reservation.release(&mut later.result_claim)?;
                        }
                        return Ok(StepResolution::with_stop(
                            StepOutcome::Error(failure_reason(
                                "AGENT_TOOL_EXECUTOR",
                                "the prepared file mutation failed without a definite outcome",
                            )?),
                            latch_tool_stop(latched_stop, stop),
                        ));
                    }
                    ToolRun::ModelError { code, message } => {
                        settle_model_error(reservation, plan, turn, step, code, message).await?;
                    }
                    _ => {
                        return Err(AgentLoopError::Invariant(
                            "mutation resolution returned an invalid tool state",
                        ));
                    }
                }
            }
            ToolRun::Action(action) => {
                let resolved =
                    resolve_action(reservation, driver, plan, action, cancellation).await?;
                match resolved {
                    ToolRun::Completed {
                        result,
                        settlement,
                        stop,
                    } => {
                        let requested_conclusion = result.concludes_turn();
                        let committed_preferred =
                            settle_tool_result(reservation, driver, plan, result, settlement)
                                .await?;
                        concludes_turn |= requested_conclusion && committed_preferred;
                        latched_stop = latch_tool_stop(latched_stop, stop);
                        match stop {
                            ToolStop::None => {}
                            ToolStop::Cancelled => cancelled = true,
                            ToolStop::TurnTimeout => {
                                infrastructure_failure = Some(failure_reason(
                                    "AGENT_TURN_TIMEOUT",
                                    "the agent turn timed out",
                                )?);
                            }
                        }
                    }
                    ToolRun::ActionUnresolved { stop } => {
                        if reservation.session().is_durable() {
                            settle_unknown_tool_outcome(reservation, plan, turn, step).await?;
                            infrastructure_failure = Some(failure_reason(
                                "AGENT_TOOL_EXECUTOR",
                                "the foreground action lost a definite result",
                            )?);
                            latched_stop = latch_tool_stop(latched_stop, stop);
                            continue;
                        }
                        reservation.release(&mut plan.result_claim)?;
                        for later in remaining {
                            reservation.release(&mut later.call_claim)?;
                            reservation.release(&mut later.result_claim)?;
                        }
                        return Ok(StepResolution::with_stop(
                            StepOutcome::Error(failure_reason(
                                "AGENT_TOOL_EXECUTOR",
                                "the foreground action lost a definite result",
                            )?),
                            latch_tool_stop(latched_stop, stop),
                        ));
                    }
                    ToolRun::ModelError { code, message } => {
                        settle_model_error(reservation, plan, turn, step, code, message).await?;
                    }
                    _ => {
                        return Err(AgentLoopError::Invariant(
                            "action resolution returned an invalid tool state",
                        ));
                    }
                }
            }
            ToolRun::ModelError { code, message } => {
                if code == "ABORTED" {
                    cancelled = true;
                    latched_stop = latch_tool_stop(latched_stop, ToolStop::Cancelled);
                }
                settle_model_error(reservation, plan, turn, step, code, message).await?;
            }
            ToolRun::Infrastructure { stop } => {
                if reservation.session().is_durable() {
                    settle_unknown_tool_outcome(reservation, plan, turn, step).await?;
                    infrastructure_failure = Some(failure_reason(
                        "AGENT_TOOL_EXECUTOR",
                        "the tool executor failed before producing a result",
                    )?);
                    latched_stop = latch_tool_stop(latched_stop, stop);
                    continue;
                }
                reservation.release(&mut plan.result_claim)?;
                for later in remaining {
                    reservation.release(&mut later.call_claim)?;
                    reservation.release(&mut later.result_claim)?;
                }
                return Ok(StepResolution::with_stop(
                    StepOutcome::Error(failure_reason(
                        "AGENT_TOOL_EXECUTOR",
                        "the tool executor failed before producing a result",
                    )?),
                    latch_tool_stop(latched_stop, stop),
                ));
            }
            ToolRun::ActionUnresolved { stop } => {
                if reservation.session().is_durable() {
                    settle_unknown_tool_outcome(reservation, plan, turn, step).await?;
                    infrastructure_failure = Some(failure_reason(
                        "AGENT_TOOL_EXECUTOR",
                        "the foreground action lost a definite result",
                    )?);
                    latched_stop = latch_tool_stop(latched_stop, stop);
                    continue;
                }
                reservation.release(&mut plan.result_claim)?;
                for later in remaining {
                    reservation.release(&mut later.call_claim)?;
                    reservation.release(&mut later.result_claim)?;
                }
                return Ok(StepResolution::with_stop(
                    StepOutcome::Error(failure_reason(
                        "AGENT_TOOL_EXECUTOR",
                        "the foreground action lost a definite result",
                    )?),
                    latch_tool_stop(latched_stop, stop),
                ));
            }
            ToolRun::TurnTimeout => {
                latched_stop = latch_tool_stop(latched_stop, ToolStop::TurnTimeout);
                let preferred = tool_error_event(
                    turn,
                    step,
                    &plan.result_message_id,
                    &plan.call,
                    plan.call_seq,
                    "ABORTED",
                    "AbortError",
                    "tool was stopped because the agent turn timed out",
                )?;
                reservation
                    .settle_settled(&mut plan.result_claim, preferred)
                    .await?;
                infrastructure_failure = Some(failure_reason(
                    "AGENT_TURN_TIMEOUT",
                    "the agent turn timed out",
                )?);
            }
        }
        if (index + 1) % AGENT_READY_WORK_BUDGET == 0 {
            // A group of immediately-ready tool bodies must still give the
            // owning CLI a chance to deliver cancellation before the next one.
            tokio::task::yield_now().await;
        }
        // A stop may become ready while a truthful result is being encoded and
        // appended. Preserve the first such outer stop before any later tool is
        // dispatched; do not rewrite the result that was just committed.
        latched_stop = latch_tool_stop(
            latched_stop,
            sample_tool_stop(cancellation, driver.deadline),
        );
        match latched_stop {
            ToolStop::None => {}
            ToolStop::Cancelled => cancelled = true,
            ToolStop::TurnTimeout => {
                if infrastructure_failure.is_none() {
                    infrastructure_failure = Some(failure_reason(
                        "AGENT_TURN_TIMEOUT",
                        "the agent turn timed out",
                    )?);
                }
            }
        }
    }
    let outcome = if driver.durable_limit.is_some() {
        StepOutcome::Error(driver.session_limit_failure.clone())
    } else if driver.observer_unavailable {
        StepOutcome::Error(observer_unavailable_failure()?)
    } else if let Some(error) = infrastructure_failure {
        StepOutcome::Error(error)
    } else if cancelled {
        StepOutcome::Cancelled
    } else if concludes_turn {
        StepOutcome::Completed
    } else {
        StepOutcome::Continue
    };
    Ok(StepResolution::with_stop(outcome, latched_stop))
}

async fn settle_model_error(
    reservation: &mut SessionReservation<'_>,
    plan: &mut PlannedTool,
    turn: TurnId,
    step: StepId,
    code: &'static str,
    message: &'static str,
) -> Result<(), AgentLoopError> {
    let failure_name = match code {
        "ABORTED" | "ABORTED_BEFORE_DISPATCH" => "AbortError",
        TOOL_OUTCOME_UNKNOWN => "ToolOutcomeUnknownError",
        _ => plan.call.name.as_str(),
    };
    let preferred = tool_error_event(
        turn,
        step,
        &plan.result_message_id,
        &plan.call,
        plan.call_seq,
        code,
        failure_name,
        message,
    )?;
    reservation
        .settle_settled(&mut plan.result_claim, preferred)
        .await?;
    Ok(())
}

async fn settle_unknown_tool_outcome(
    reservation: &mut SessionReservation<'_>,
    plan: &mut PlannedTool,
    turn: TurnId,
    step: StepId,
) -> Result<(), AgentLoopError> {
    settle_model_error(
        reservation,
        plan,
        turn,
        step,
        TOOL_OUTCOME_UNKNOWN,
        "The tool call was interrupted after it was recorded, but no result was durably recorded. Its outcome is unknown. Decide whether to retry from the tool semantics: retry only if the operation is read-only or idempotent; if it may have side effects, first verify external state or ask the user. Do not retry blindly.",
    )
    .await
}

fn latch_tool_stop(current: ToolStop, observed: ToolStop) -> ToolStop {
    if current == ToolStop::None {
        observed
    } else {
        current
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolStop {
    None,
    Cancelled,
    TurnTimeout,
}

enum ToolRun {
    Completed {
        result: ToolExecutionResult,
        settlement: ResultSettlement,
        stop: ToolStop,
    },
    Mutation(PreparedToolMutation),
    Action(PreparedToolActionSetup),
    ModelError {
        code: &'static str,
        message: &'static str,
    },
    Infrastructure {
        stop: ToolStop,
    },
    ActionUnresolved {
        stop: ToolStop,
    },
    TurnTimeout,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResultSettlement {
    FallbackAllowed,
    PreferredRequired,
}

async fn run_one_tool(
    driver: &Driver<'_>,
    plan: &PlannedTool,
    cancellation: &CancellationToken,
) -> ToolRun {
    let call = &plan.call;
    if !driver
        .config
        .tools
        .iter()
        .any(|tool| tool.name() == call.name)
    {
        return prestart_failure(
            plan,
            "UNKNOWN_TOOL",
            "the requested tool was not declared for this model call",
            ToolStop::None,
        );
    }
    let raw = if call.arguments.is_empty() {
        "{}".to_owned()
    } else {
        call.arguments.clone()
    };
    if raw.len() > driver.config.limits.max_tool_argument_bytes {
        return prestart_failure(
            plan,
            "TOOL_ARGUMENTS_TOO_LARGE",
            "tool arguments exceed the configured size limit",
            ToolStop::None,
        );
    }
    let parsed = match serde_json::from_str(raw.as_str())
        .ok()
        .and_then(|value| JsonValue::new(value).ok())
    {
        Some(parsed) => parsed,
        None => {
            return prestart_failure(
                plan,
                "INVALID_TOOL_ARGUMENTS",
                "tool arguments are not valid bounded JSON",
                ToolStop::None,
            );
        }
    };
    let request = ToolExecutionRequest::new(
        call.id.clone(),
        call.name.clone(),
        raw,
        parsed,
        plan.dispatch.clone(),
    );
    let child = cancellation.child_token();
    if cancellation.is_cancelled() {
        child.cancel();
        return prestart_failure(
            plan,
            "ABORTED_BEFORE_DISPATCH",
            "tool was not started because the turn was stopping",
            ToolStop::Cancelled,
        );
    }
    if Instant::now() >= driver.deadline {
        child.cancel();
        return prestart_failure(
            plan,
            "ABORTED_BEFORE_DISPATCH",
            "tool was not started because the agent turn timed out",
            ToolStop::TurnTimeout,
        );
    }
    let future = match catch_unwind(AssertUnwindSafe(|| {
        driver.tools.prepare(request, child.clone())
    })) {
        Ok(future) => future,
        Err(_) => {
            child.cancel();
            return ToolRun::Infrastructure {
                stop: sample_tool_stop(cancellation, driver.deadline),
            };
        }
    };
    let guarded = AssertUnwindSafe(future).catch_unwind();
    tokio::pin!(guarded);
    let tool_deadline = Instant::now() + driver.config.limits.tool_duration;
    let interrupted = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            prestart_failure(
                plan,
                "ABORTED_BEFORE_DISPATCH",
                "tool was not started because the turn was stopping",
                ToolStop::Cancelled,
            )
        }
        _ = tokio::time::sleep_until(driver.deadline) => {
            prestart_failure(
                plan,
                "ABORTED_BEFORE_DISPATCH",
                "tool was not started because the agent turn timed out",
                ToolStop::TurnTimeout,
            )
        }
        _ = tokio::time::sleep_until(tool_deadline) => {
            prestart_failure(
                plan,
                "TOOL_TIMEOUT",
                "tool preparation exceeded its configured timeout",
                ToolStop::None,
            )
        }
        result = &mut guarded => return {
            if cancellation.is_cancelled() {
                child.cancel();
                return prestart_failure(
                    plan,
                    "ABORTED_BEFORE_DISPATCH",
                    "tool was not started because the turn was stopping",
                    ToolStop::Cancelled,
                );
            }
            if Instant::now() >= driver.deadline {
                child.cancel();
                return prestart_failure(
                    plan,
                    "ABORTED_BEFORE_DISPATCH",
                    "tool was not started because the agent turn timed out",
                    ToolStop::TurnTimeout,
                );
            }
            if Instant::now() >= tool_deadline {
                child.cancel();
                return prestart_failure(
                    plan,
                    "TOOL_TIMEOUT",
                    "tool preparation exceeded its configured timeout",
                    ToolStop::None,
                );
            }
            match result {
                Ok(Ok(ToolPreparation::Complete(result))) => {
                    if plan.claim_profile.is_shell_action()
                        && tool::validate_action_not_started_result(&result).is_err()
                    {
                        ToolRun::Infrastructure {
                            stop: ToolStop::None,
                        }
                    } else {
                        ToolRun::Completed {
                            result,
                            settlement: ResultSettlement::FallbackAllowed,
                            stop: ToolStop::None,
                        }
                    }
                }
                Ok(Ok(ToolPreparation::Mutation(mutation))) => {
                    if plan.claim_profile.is_shell_action() {
                        ToolRun::Infrastructure {
                            stop: ToolStop::None,
                        }
                    } else {
                        ToolRun::Mutation(mutation)
                    }
                }
                Ok(Ok(ToolPreparation::Action(action))) => {
                    if !plan.claim_profile.is_shell_action()
                        || !action.matches_dispatch(&plan.dispatch)
                    {
                        ToolRun::Infrastructure {
                            stop: ToolStop::None,
                        }
                    } else {
                        ToolRun::Action(action)
                    }
                }
                Ok(Err(_)) | Err(_) => {
                    child.cancel();
                    ToolRun::Infrastructure {
                        stop: sample_tool_stop(cancellation, driver.deadline),
                    }
                }
            }
        }
    };

    child.cancel();
    // Started tools get one bounded cleanup window. The same future is polled
    // again so cooperative implementations can observe their child token and
    // close files, sockets, or other resources before the durable result is
    // committed. A tool that ignores cancellation cannot hold the turn open.
    // Cancellation or timeout already won the linearization race. Cleanup
    // success, failure, panic, or grace expiry cannot rewrite that durable
    // outcome (and no extension detail is retained).
    let _ = tokio::time::timeout(MAX_AGENT_TOOL_SHUTDOWN_GRACE, &mut guarded).await;
    interrupted
}

fn prestart_failure(
    plan: &PlannedTool,
    code: &'static str,
    message: &'static str,
    stop: ToolStop,
) -> ToolRun {
    if !plan.claim_profile.is_shell_action() {
        return match stop {
            ToolStop::TurnTimeout => ToolRun::TurnTimeout,
            ToolStop::Cancelled => ToolRun::ModelError {
                code: "ABORTED",
                message: "tool was cancelled",
            },
            ToolStop::None => ToolRun::ModelError { code, message },
        };
    }
    let failure_name = if matches!(code, "ABORTED" | "ABORTED_BEFORE_DISPATCH") {
        "AbortError"
    } else {
        plan.call.name.as_str()
    };
    match shell_prestart_result(code, failure_name, message, None) {
        Ok(result) => ToolRun::Completed {
            result,
            settlement: ResultSettlement::FallbackAllowed,
            stop,
        },
        Err(_) => ToolRun::Infrastructure { stop },
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_action(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    plan: &mut PlannedTool,
    setup: PreparedToolActionSetup,
    cancellation: &CancellationToken,
) -> Result<ToolRun, AgentLoopError> {
    let setup_child = cancellation.child_token();
    let preparation_deadline = Instant::now() + MAX_AGENT_ACTION_PREPARATION_DURATION;
    let setup_control =
        ToolActionSetupControl::new(setup_child.clone(), driver.deadline, preparation_deadline);
    let setup_future = match catch_unwind(AssertUnwindSafe(|| setup.resolve(setup_control))) {
        Ok(future) => future,
        Err(_) => {
            setup_child.cancel();
            return Ok(ToolRun::ActionUnresolved {
                stop: sample_tool_stop(cancellation, driver.deadline),
            });
        }
    };
    let guarded_setup = AssertUnwindSafe(setup_future).catch_unwind();
    tokio::pin!(guarded_setup);
    let mut latched_stop = ToolStop::None;
    let setup_outcome = loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled(), if latched_stop == ToolStop::None => {
                latched_stop = ToolStop::Cancelled;
            }
            _ = tokio::time::sleep_until(driver.deadline), if latched_stop == ToolStop::None => {
                latched_stop = ToolStop::TurnTimeout;
            }
            outcome = &mut guarded_setup => break match outcome {
                Ok(outcome) => outcome,
                Err(_) => {
                    return Ok(ToolRun::ActionUnresolved {
                        stop: preserve_or_sample_tool_stop(
                            latched_stop,
                            cancellation,
                            driver.deadline,
                        ),
                    });
                }
            },
        }
    };
    let action = match setup_outcome {
        ToolActionSetupOutcome::Ready(action) => {
            latched_stop =
                preserve_or_sample_tool_stop(latched_stop, cancellation, driver.deadline);
            if !action.matches_dispatch(&plan.dispatch) {
                return Ok(ToolRun::ActionUnresolved { stop: latched_stop });
            }
            if latched_stop != ToolStop::None {
                return Ok(decline_action(
                    action,
                    ActionDeclineReason::AbortedBeforeDispatch,
                    latched_stop,
                ));
            }
            action
        }
        ToolActionSetupOutcome::NotStarted { turn_stop, result } => {
            let stop = preserve_or_sample_tool_stop(
                merge_action_stop(latched_stop, turn_stop),
                cancellation,
                driver.deadline,
            );
            if tool::validate_action_not_started_result(&result).is_err() {
                return Ok(ToolRun::ActionUnresolved { stop });
            }
            return Ok(ToolRun::Completed {
                result,
                settlement: ResultSettlement::FallbackAllowed,
                stop,
            });
        }
        ToolActionSetupOutcome::Infrastructure { turn_stop } => {
            return Ok(ToolRun::ActionUnresolved {
                stop: preserve_or_sample_tool_stop(
                    merge_action_stop(latched_stop, turn_stop),
                    cancellation,
                    driver.deadline,
                ),
            });
        }
    };

    if driver.config.shell_policy == ShellPolicy::Deny {
        return Ok(decline_action(
            action,
            ActionDeclineReason::PolicyDenied,
            ToolStop::None,
        ));
    }

    let maximum_result_bytes = action.maximum_result_event_bytes();
    let configured_result_fits = maximum_result_bytes <= driver.config.limits.max_tool_result_bytes
        && driver
            .counters
            .tool_result_bytes
            .checked_add(maximum_result_bytes)
            .is_some_and(|total| total <= driver.config.limits.max_tool_results_per_turn_bytes);
    if !configured_result_fits {
        return Ok(decline_action(
            action,
            ActionDeclineReason::OutputBudgetExceeded,
            ToolStop::None,
        ));
    }
    match reservation
        .reserve_claim_retained_json_bytes(&mut plan.result_claim, maximum_result_bytes)
    {
        Ok(()) => {}
        Err(error) if is_durable_session_limit(&error) => {
            driver.latch_durable_limit(&error);
            return Ok(prestart_failure(
                plan,
                "SESSION_LIMIT",
                "tool was not started because the durable session reached its storage limit",
                ToolStop::None,
            ));
        }
        Err(error) if is_memory_budget_error(&error) => {
            return Ok(decline_action(
                action,
                ActionDeclineReason::OutputBudgetExceeded,
                ToolStop::None,
            ));
        }
        Err(error) => return Err(error.into()),
    }

    let action = if driver.config.shell_policy == ShellPolicy::Ask {
        let approval_id = match next_id(driver.runtime, AgentIdKind::Approval) {
            Ok(id) => ApprovalRequestId::new(id),
            Err(_) => {
                return Ok(decline_action(
                    action,
                    ActionDeclineReason::ApprovalUnavailable,
                    ToolStop::None,
                ));
            }
        };
        let request = ApprovalRequest::new(
            approval_id.clone(),
            plan.call.name.clone(),
            plan.call.id.clone(),
            action.prompt(),
        );
        let asked = NewEvent::log(EventKind::approval_asked(ApprovalAskedEvent::new(
            approval_id.clone(),
            plan.call.name.clone(),
            Some(plan.call.id.clone()),
            action.prompt().reason().map(str::to_owned),
        )?));
        let decision_fallback = NewEvent::log(EventKind::approval_decided(
            ApprovalDecidedEvent::new(approval_id.clone(), ApprovalOutcome::AllowedOnce)?,
        ));
        let mut audit_claims = match reservation.claim_batch([asked, decision_fallback]) {
            Ok(claims) => claims,
            Err(error) if is_durable_session_limit(&error) => {
                driver.latch_durable_limit(&error);
                return Ok(prestart_failure(
                    plan,
                    "SESSION_LIMIT",
                    "tool was not started because the durable session reached its storage limit",
                    ToolStop::None,
                ));
            }
            Err(error) if is_memory_budget_error(&error) => {
                return Ok(decline_action(
                    action,
                    ActionDeclineReason::ApprovalUnavailable,
                    ToolStop::None,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let mut asked_claim = audit_claims.remove(0);
        let mut decided_claim = audit_claims.remove(0);
        reservation.settle_exact_settled(&mut asked_claim).await?;
        let asked_visible = dispatch_barrier(reservation).await? == DispatchBarrier::Ready;
        if !asked_visible {
            driver.observer_unavailable = true;
        }
        let (outcome, stop) = if asked_visible {
            request_approval(driver, request, cancellation).await
        } else {
            (ApprovalOutcome::Unavailable, ToolStop::None)
        };
        reservation.rebind_claim_fallback(
            &mut decided_claim,
            NewEvent::log(EventKind::approval_decided(ApprovalDecidedEvent::new(
                approval_id,
                outcome,
            )?)),
        )?;
        reservation.settle_exact_settled(&mut decided_claim).await?;
        let decision_visible = dispatch_barrier(reservation).await? == DispatchBarrier::Ready;
        if !decision_visible {
            driver.observer_unavailable = true;
        }
        match outcome {
            ApprovalOutcome::AllowedOnce if stop == ToolStop::None && decision_visible => action,
            ApprovalOutcome::AllowedOnce => {
                return Ok(decline_action(
                    action,
                    ActionDeclineReason::AbortedBeforeDispatch,
                    stop,
                ));
            }
            ApprovalOutcome::Cancelled => {
                return Ok(decline_action(
                    action,
                    ActionDeclineReason::ApprovalCancelled,
                    stop,
                ));
            }
            ApprovalOutcome::Rejected => {
                return Ok(decline_action(
                    action,
                    ActionDeclineReason::ApprovalRejected,
                    stop,
                ));
            }
            ApprovalOutcome::Unavailable => {
                return Ok(decline_action(
                    action,
                    ActionDeclineReason::ApprovalUnavailable,
                    stop,
                ));
            }
        }
    } else {
        action
    };

    let final_stop = sample_tool_stop(cancellation, driver.deadline);
    if final_stop != ToolStop::None {
        return Ok(decline_action(
            action,
            ActionDeclineReason::AbortedBeforeDispatch,
            final_stop,
        ));
    }
    if !action.matches_dispatch(&plan.dispatch) {
        return Ok(ToolRun::ActionUnresolved {
            stop: ToolStop::None,
        });
    }

    let action_child = cancellation.child_token();
    let action_deadline = Instant::now() + driver.config.limits.tool_duration;
    let action_control =
        ToolActionControl::new(action_child.clone(), driver.deadline, action_deadline);
    let action_future = match catch_unwind(AssertUnwindSafe(|| action.run(action_control))) {
        Ok(future) => future,
        Err(_) => {
            action_child.cancel();
            return Ok(ToolRun::ActionUnresolved {
                stop: sample_tool_stop(cancellation, driver.deadline),
            });
        }
    };
    let guarded_action = AssertUnwindSafe(action_future).catch_unwind();
    tokio::pin!(guarded_action);
    let mut latched_stop = ToolStop::None;
    let outcome = loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled(), if latched_stop == ToolStop::None => {
                latched_stop = ToolStop::Cancelled;
            }
            _ = tokio::time::sleep_until(driver.deadline), if latched_stop == ToolStop::None => {
                latched_stop = ToolStop::TurnTimeout;
            }
            outcome = &mut guarded_action => break match outcome {
                Ok(outcome) => outcome,
                Err(_) => {
                    return Ok(ToolRun::ActionUnresolved {
                        stop: preserve_or_sample_tool_stop(
                            latched_stop,
                            cancellation,
                            driver.deadline,
                        ),
                    });
                }
            },
        }
    };
    Ok(match outcome {
        ToolActionOutcome::NotStarted { turn_stop, result } => {
            let stop = preserve_or_sample_tool_stop(
                merge_action_stop(latched_stop, turn_stop),
                cancellation,
                driver.deadline,
            );
            if tool::validate_action_not_started_result(&result).is_err() {
                ToolRun::ActionUnresolved { stop }
            } else {
                ToolRun::Completed {
                    result,
                    settlement: ResultSettlement::FallbackAllowed,
                    stop,
                }
            }
        }
        ToolActionOutcome::Infrastructure { turn_stop }
        | ToolActionOutcome::StartedOwnershipLost { turn_stop } => ToolRun::ActionUnresolved {
            stop: preserve_or_sample_tool_stop(
                merge_action_stop(latched_stop, turn_stop),
                cancellation,
                driver.deadline,
            ),
        },
        ToolActionOutcome::StartedAndQuiescent { turn_stop, result } => {
            let stop = preserve_or_sample_tool_stop(
                merge_action_stop(latched_stop, turn_stop),
                cancellation,
                driver.deadline,
            );
            let result_fits_declared_bound = action_result_event_bytes(reservation, plan, &result)
                .is_ok_and(|size| size <= maximum_result_bytes);
            if tool::validate_action_started_result(&result).is_err() || !result_fits_declared_bound
            {
                ToolRun::ActionUnresolved { stop }
            } else {
                ToolRun::Completed {
                    result,
                    settlement: ResultSettlement::PreferredRequired,
                    stop,
                }
            }
        }
    })
}

fn decline_action(
    action: PreparedToolAction,
    reason: ActionDeclineReason,
    stop: ToolStop,
) -> ToolRun {
    match action.decline(reason) {
        Ok(result) => ToolRun::Completed {
            result,
            settlement: ResultSettlement::FallbackAllowed,
            stop,
        },
        Err(_) => ToolRun::ActionUnresolved { stop },
    }
}

fn sample_tool_stop(cancellation: &CancellationToken, deadline: Instant) -> ToolStop {
    if cancellation.is_cancelled() {
        ToolStop::Cancelled
    } else if Instant::now() >= deadline {
        ToolStop::TurnTimeout
    } else {
        ToolStop::None
    }
}

fn preserve_or_sample_tool_stop(
    current: ToolStop,
    cancellation: &CancellationToken,
    deadline: Instant,
) -> ToolStop {
    if current == ToolStop::None {
        sample_tool_stop(cancellation, deadline)
    } else {
        current
    }
}

fn merge_action_stop(current: ToolStop, reported: ToolActionTurnStop) -> ToolStop {
    if current != ToolStop::None {
        return current;
    }
    match reported {
        ToolActionTurnStop::None => ToolStop::None,
        ToolActionTurnStop::CallerCancelled => ToolStop::Cancelled,
        ToolActionTurnStop::TurnTimeout => ToolStop::TurnTimeout,
    }
}

#[allow(clippy::too_many_arguments)]
async fn resolve_mutation(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    plan: &mut PlannedTool,
    mutation: PreparedToolMutation,
    cancellation: &CancellationToken,
) -> Result<ToolRun, AgentLoopError> {
    if driver.config.file_change_policy == FileChangePolicy::Deny {
        return Ok(decline_mutation(
            mutation,
            MutationDeclineReason::PolicyDenied,
            ToolStop::None,
        ));
    }

    // An irreversible change may start only after the session has protected a
    // large enough slot for its truthful result. Near the session byte limit we
    // fail before asking the user and before invoking the commit capability.
    let maximum_result_bytes = mutation.maximum_result_event_bytes();
    let configured_result_fits = maximum_result_bytes <= driver.config.limits.max_tool_result_bytes
        && driver
            .counters
            .tool_result_bytes
            .checked_add(maximum_result_bytes)
            .is_some_and(|total| total <= driver.config.limits.max_tool_results_per_turn_bytes);
    if !configured_result_fits {
        return Ok(decline_mutation(
            mutation,
            MutationDeclineReason::OutputBudgetExceeded,
            ToolStop::None,
        ));
    }
    match reservation
        .reserve_claim_retained_json_bytes(&mut plan.result_claim, maximum_result_bytes)
    {
        Ok(()) => {}
        Err(error) if is_durable_session_limit(&error) => {
            driver.latch_durable_limit(&error);
            return Ok(prestart_failure(
                plan,
                "SESSION_LIMIT",
                "tool was not started because the durable session reached its storage limit",
                ToolStop::None,
            ));
        }
        Err(error) if is_memory_budget_error(&error) => {
            return Ok(decline_mutation(
                mutation,
                MutationDeclineReason::OutputBudgetExceeded,
                ToolStop::None,
            ));
        }
        Err(error) => return Err(error.into()),
    }

    let mutation = if driver.config.file_change_policy == FileChangePolicy::Ask {
        let approval_id = match next_id(driver.runtime, AgentIdKind::Approval) {
            Ok(id) => ApprovalRequestId::new(id),
            Err(_) => {
                return Ok(decline_mutation(
                    mutation,
                    MutationDeclineReason::ApprovalUnavailable,
                    ToolStop::None,
                ));
            }
        };
        let request = ApprovalRequest::new(
            approval_id.clone(),
            plan.call.name.clone(),
            plan.call.id.clone(),
            mutation.prompt(),
        );
        let asked = NewEvent::log(EventKind::approval_asked(ApprovalAskedEvent::new(
            approval_id.clone(),
            plan.call.name.clone(),
            Some(plan.call.id.clone()),
            mutation.prompt().reason().map(str::to_owned),
        )?));
        // `allowed-once` is the longest current wire spelling, so this fallback
        // protects enough bytes for every exact decision we may later rebind.
        let decision_fallback = NewEvent::log(EventKind::approval_decided(
            ApprovalDecidedEvent::new(approval_id.clone(), ApprovalOutcome::AllowedOnce)?,
        ));
        let mut audit_claims = match reservation.claim_batch([asked, decision_fallback]) {
            Ok(claims) => claims,
            Err(error) if is_durable_session_limit(&error) => {
                driver.latch_durable_limit(&error);
                return Ok(prestart_failure(
                    plan,
                    "SESSION_LIMIT",
                    "tool was not started because the durable session reached its storage limit",
                    ToolStop::None,
                ));
            }
            Err(error) if is_memory_budget_error(&error) => {
                return Ok(decline_mutation(
                    mutation,
                    MutationDeclineReason::ApprovalUnavailable,
                    ToolStop::None,
                ));
            }
            Err(error) => return Err(error.into()),
        };
        let mut asked_claim = audit_claims.remove(0);
        let mut decided_claim = audit_claims.remove(0);
        reservation.settle_exact_settled(&mut asked_claim).await?;
        let asked_visible = dispatch_barrier(reservation).await? == DispatchBarrier::Ready;
        if !asked_visible {
            driver.observer_unavailable = true;
        }
        let (outcome, stop) = if asked_visible {
            request_approval(driver, request, cancellation).await
        } else {
            (ApprovalOutcome::Unavailable, ToolStop::None)
        };
        reservation.rebind_claim_fallback(
            &mut decided_claim,
            NewEvent::log(EventKind::approval_decided(ApprovalDecidedEvent::new(
                approval_id,
                outcome,
            )?)),
        )?;
        reservation.settle_exact_settled(&mut decided_claim).await?;
        let decision_visible = dispatch_barrier(reservation).await? == DispatchBarrier::Ready;
        if !decision_visible {
            driver.observer_unavailable = true;
        }

        match outcome {
            ApprovalOutcome::AllowedOnce if stop == ToolStop::None && decision_visible => mutation,
            ApprovalOutcome::AllowedOnce => {
                return Ok(decline_mutation(
                    mutation,
                    MutationDeclineReason::AbortedBeforeDispatch,
                    stop,
                ));
            }
            ApprovalOutcome::Cancelled => {
                return Ok(decline_mutation(
                    mutation,
                    MutationDeclineReason::ApprovalCancelled,
                    stop,
                ));
            }
            ApprovalOutcome::Rejected => {
                return Ok(decline_mutation(
                    mutation,
                    MutationDeclineReason::ApprovalRejected,
                    stop,
                ));
            }
            ApprovalOutcome::Unavailable => {
                return Ok(decline_mutation(
                    mutation,
                    MutationDeclineReason::ApprovalUnavailable,
                    stop,
                ));
            }
        }
    } else {
        mutation
    };

    commit_mutation(driver, mutation, cancellation).await
}

fn decline_mutation(
    mutation: PreparedToolMutation,
    reason: MutationDeclineReason,
    stop: ToolStop,
) -> ToolRun {
    match mutation.decline(reason) {
        Ok(result) => ToolRun::Completed {
            result,
            settlement: ResultSettlement::FallbackAllowed,
            stop,
        },
        Err(_) => ToolRun::Infrastructure { stop },
    }
}

async fn request_approval(
    driver: &Driver<'_>,
    request: ApprovalRequest,
    cancellation: &CancellationToken,
) -> (ApprovalOutcome, ToolStop) {
    if cancellation.is_cancelled() {
        return (ApprovalOutcome::Cancelled, ToolStop::Cancelled);
    }
    if Instant::now() >= driver.deadline {
        return (ApprovalOutcome::Cancelled, ToolStop::TurnTimeout);
    }
    let child = cancellation.child_token();
    let future = match catch_unwind(AssertUnwindSafe(|| {
        driver
            .config
            .approval_provider
            .request(request, child.clone())
    })) {
        Ok(future) => future,
        Err(_) => {
            child.cancel();
            return (ApprovalOutcome::Unavailable, ToolStop::None);
        }
    };
    let guarded = AssertUnwindSafe(future).catch_unwind();
    tokio::pin!(guarded);
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            child.cancel();
            let _ = tokio::time::timeout(MAX_AGENT_TOOL_SHUTDOWN_GRACE, &mut guarded).await;
            (ApprovalOutcome::Cancelled, ToolStop::Cancelled)
        }
        _ = tokio::time::sleep_until(driver.deadline) => {
            child.cancel();
            let _ = tokio::time::timeout(MAX_AGENT_TOOL_SHUTDOWN_GRACE, &mut guarded).await;
            (ApprovalOutcome::Cancelled, ToolStop::TurnTimeout)
        }
        result = &mut guarded => {
            if cancellation.is_cancelled() {
                child.cancel();
                (ApprovalOutcome::Cancelled, ToolStop::Cancelled)
            } else if Instant::now() >= driver.deadline {
                child.cancel();
                (ApprovalOutcome::Cancelled, ToolStop::TurnTimeout)
            } else {
                match result {
                    Ok(Ok(outcome)) => (outcome, ToolStop::None),
                    Ok(Err(_)) | Err(_) => {
                        child.cancel();
                        (ApprovalOutcome::Unavailable, ToolStop::None)
                    }
                }
            }
        }
    }
}

async fn commit_mutation(
    driver: &Driver<'_>,
    mutation: PreparedToolMutation,
    cancellation: &CancellationToken,
) -> Result<ToolRun, AgentLoopError> {
    if cancellation.is_cancelled() {
        return Ok(decline_mutation(
            mutation,
            MutationDeclineReason::AbortedBeforeDispatch,
            ToolStop::Cancelled,
        ));
    }
    if Instant::now() >= driver.deadline {
        return Ok(decline_mutation(
            mutation,
            MutationDeclineReason::AbortedBeforeDispatch,
            ToolStop::TurnTimeout,
        ));
    }

    let child = cancellation.child_token();
    let job_child = child.clone();
    let mut job = tokio::task::spawn_blocking(move || {
        catch_unwind(AssertUnwindSafe(|| mutation.commit(job_child)))
    });
    let mut stop = ToolStop::None;
    let mut tool_timeout_seen = false;
    let tool_timeout = tokio::time::sleep(driver.config.limits.tool_duration);
    tokio::pin!(tool_timeout);
    let joined = loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                child.cancel();
                stop = ToolStop::Cancelled;
                break (&mut job).await;
            }
            _ = tokio::time::sleep_until(driver.deadline) => {
                child.cancel();
                stop = ToolStop::TurnTimeout;
                break (&mut job).await;
            }
            _ = &mut tool_timeout, if !tool_timeout_seen => {
                child.cancel();
                tool_timeout_seen = true;
            }
            result = &mut job => break result,
        }
    };
    let outcome = match joined {
        Ok(Ok(Ok(outcome))) => outcome,
        Ok(Ok(Err(_))) | Ok(Err(_)) | Err(_) => {
            child.cancel();
            return Ok(ToolRun::Infrastructure { stop });
        }
    };
    let (disposition, result) = outcome.into_parts();
    let result = if tool_timeout_seen
        && stop == ToolStop::None
        && disposition == ToolCommitDisposition::NotCommitted
    {
        let (_, _, _, meta, _) = result.into_parts();
        ToolExecutionResult::new(
            vec![ContentBlock::text(
                "Error: file mutation exceeded its configured timeout before publication",
            )?],
            true,
            Some(ToolFailure {
                name: "TimeoutError".to_owned(),
                code: "TOOL_TIMEOUT".to_owned(),
            }),
            meta,
            false,
        )?
    } else {
        result
    };
    Ok(ToolRun::Completed {
        result,
        settlement: if disposition == ToolCommitDisposition::Committed {
            ResultSettlement::PreferredRequired
        } else {
            ResultSettlement::FallbackAllowed
        },
        stop,
    })
}

fn session_has_unresolved_tool_calls(session: &Session) -> bool {
    session.has_unresolved_surface_tool_calls()
}

async fn settle_tool_result(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    plan: &mut PlannedTool,
    result: ToolExecutionResult,
    settlement: ResultSettlement,
) -> Result<bool, AgentLoopError> {
    let (content, is_error, error, meta, _concludes_turn) = result.into_parts();
    let component_bytes = content
        .iter()
        .try_fold(0_usize, |total, block| {
            total.checked_add(block.raw().encoded_len())
        })
        .and_then(|total| total.checked_add(meta.as_ref().map_or(0, JsonValue::encoded_len)))
        .and_then(|total| {
            error.as_ref().map_or(Some(total), |error| {
                total
                    .checked_add(error.name.len())
                    .and_then(|value| value.checked_add(error.code.len()))
            })
        });
    let component_fits = component_bytes.is_some_and(|size| {
        size <= driver.config.limits.max_tool_result_bytes
            && driver
                .counters
                .tool_result_bytes
                .checked_add(size)
                .is_some_and(|total| total <= driver.config.limits.max_tool_results_per_turn_bytes)
    });
    let preferred_required = settlement == ResultSettlement::PreferredRequired;
    if !preferred_required && !component_fits {
        reservation
            .settle_exact_settled(&mut plan.result_claim)
            .await?;
        return Ok(false);
    }
    let message = match Message::tool_result(
        plan.result_message_id.clone(),
        plan.call.id.clone(),
        content,
        is_error,
    ) {
        Ok(message) => message,
        Err(error) if preferred_required => return Err(error.into()),
        Err(_) => {
            reservation
                .settle_exact_settled(&mut plan.result_claim)
                .await?;
            return Ok(false);
        }
    };
    let preferred = NewEvent::surface(
        EventKind::ToolResult {
            turn: plan_turn(plan, reservation)?,
            step: plan_step(plan, reservation)?,
            message,
            error,
            meta,
        },
        SurfaceIntent::append().with_sources(vec![plan.call_seq]),
    );
    let size = match Session::event_retained_json_bytes(&preferred) {
        Ok(size) => size,
        Err(error) if preferred_required => return Err(error.into()),
        Err(_) => {
            reservation
                .settle_exact_settled(&mut plan.result_claim)
                .await?;
            return Ok(false);
        }
    };
    if preferred_required {
        match reservation
            .settle_preferred_only_settled(&mut plan.result_claim, preferred)
            .await
        {
            Ok(_) => {}
            Err(first @ AppendError::Clock(_)) if reservation.session().is_durable() => {
                match reservation
                    .resume_preferred_only_settled(&mut plan.result_claim)
                    .await
                {
                    Ok(_) => {}
                    Err(AppendError::Clock(_)) => return Err(first.into()),
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        }
        driver.counters.tool_result_bytes = driver.counters.tool_result_bytes.saturating_add(size);
        return Ok(true);
    }
    let inside_limits = size <= driver.config.limits.max_tool_result_bytes
        && driver
            .counters
            .tool_result_bytes
            .checked_add(size)
            .is_some_and(|total| total <= driver.config.limits.max_tool_results_per_turn_bytes);
    if !inside_limits {
        reservation
            .settle_exact_settled(&mut plan.result_claim)
            .await?;
        return Ok(false);
    }
    let settlement = reservation
        .settle_settled(&mut plan.result_claim, preferred)
        .await?;
    let preferred = matches!(settlement, ClaimedAppend::Preferred(_));
    if preferred {
        driver.counters.tool_result_bytes += size;
    }
    Ok(preferred)
}

fn release_uncommitted_tool_round(
    reservation: &mut SessionReservation<'_>,
    assistant_claim: &mut EventClaim,
    planned: &mut [PlannedTool],
) -> Result<(), AgentLoopError> {
    reservation.release(assistant_claim)?;
    for plan in planned {
        reservation.release(&mut plan.call_claim)?;
        reservation.release(&mut plan.result_claim)?;
    }
    Ok(())
}

fn action_result_event_bytes(
    reservation: &SessionReservation<'_>,
    plan: &PlannedTool,
    result: &ToolExecutionResult,
) -> Result<usize, AgentLoopError> {
    let message = Message::tool_result(
        plan.result_message_id.clone(),
        plan.call.id.clone(),
        result.content().to_vec(),
        result.is_error(),
    )?;
    let event = NewEvent::surface(
        EventKind::ToolResult {
            turn: plan_turn(plan, reservation)?,
            step: plan_step(plan, reservation)?,
            message,
            error: result.error().cloned(),
            meta: result.meta().cloned(),
        },
        SurfaceIntent::append().with_sources(vec![plan.call_seq]),
    );
    Session::event_retained_json_bytes(&event).map_err(Into::into)
}

fn shell_prestart_result(
    code: &'static str,
    failure_name: &str,
    message: impl Into<String>,
    parsed: Option<(&str, u64)>,
) -> Result<ToolExecutionResult, AgentLoopError> {
    let mut meta = serde_json::Map::from_iter([
        (
            "kind".to_owned(),
            serde_json::Value::String("foreground".to_owned()),
        ),
        ("started".to_owned(), serde_json::Value::Bool(false)),
        ("exitCode".to_owned(), serde_json::Value::Null),
        ("signal".to_owned(), serde_json::Value::Null),
    ]);
    if let Some((workdir, timeout_ms)) = parsed {
        meta.insert(
            "workdir".to_owned(),
            serde_json::Value::String(workdir.to_owned()),
        );
        meta.insert(
            "timeoutMs".to_owned(),
            serde_json::Value::Number(timeout_ms.into()),
        );
    }
    Ok(ToolExecutionResult::new(
        vec![ContentBlock::text(message.into())?],
        true,
        Some(ToolFailure {
            name: failure_name.to_owned(),
            code: code.to_owned(),
        }),
        Some(
            JsonValue::new(serde_json::Value::Object(meta)).map_err(|_| {
                AgentLoopError::Invariant("internal shell result metadata exceeded model bounds")
            })?,
        ),
        false,
    )?)
}

#[allow(clippy::too_many_arguments)]
fn shell_prestart_error_event(
    turn: TurnId,
    step: StepId,
    message_id: &str,
    call: &ToolCall,
    call_seq: EventSeq,
    code: &'static str,
    failure_name: &str,
    message: impl Into<String>,
    parsed: Option<(&str, u64)>,
) -> Result<NewEvent, AgentLoopError> {
    let result = shell_prestart_result(code, failure_name, message, parsed)?;
    let (content, is_error, error, meta, _) = result.into_parts();
    let message = Message::tool_result(message_id, call.id.clone(), content, is_error)?;
    Ok(NewEvent::surface(
        EventKind::ToolResult {
            turn,
            step,
            message,
            error,
            meta,
        },
        SurfaceIntent::append().with_sources(vec![call_seq]),
    ))
}

fn shell_prestart_claim_ceiling(
    turn: TurnId,
    step: StepId,
    message_id: &str,
    call: &ToolCall,
    call_seq: EventSeq,
) -> Result<usize, AgentLoopError> {
    // This probe is never appended. Backslashes exercise JSON expansion for a
    // maximum-length legal relative workdir; the remaining text covers every
    // fixed pre-start diagnostic without inventing those fields in a fallback.
    let workdir = "\\".repeat(4_096);
    let probe = shell_prestart_error_event(
        turn,
        step,
        message_id,
        call,
        call_seq,
        "TOOL_OUTPUT_BUDGET_EXCEEDED",
        &"x".repeat(256),
        "x".repeat(4_096),
        Some((workdir.as_str(), 295_000)),
    )?;
    Session::event_retained_json_bytes(&probe).map_err(Into::into)
}

fn plan_turn(
    _plan: &PlannedTool,
    reservation: &SessionReservation<'_>,
) -> Result<TurnId, AgentLoopError> {
    reservation
        .session()
        .state()
        .open_turn()
        .ok_or(AgentLoopError::Invariant("tool result has no open turn"))
}

fn plan_step(
    _plan: &PlannedTool,
    reservation: &SessionReservation<'_>,
) -> Result<StepId, AgentLoopError> {
    reservation
        .session()
        .state()
        .open_step()
        .ok_or(AgentLoopError::Invariant("tool result has no open step"))
}

#[allow(clippy::too_many_arguments)]
fn tool_error_event(
    turn: TurnId,
    step: StepId,
    message_id: &str,
    call: &ToolCall,
    call_seq: EventSeq,
    code: &'static str,
    failure_name: &str,
    message: &'static str,
) -> Result<NewEvent, AgentLoopError> {
    let content = vec![ContentBlock::text(message)?];
    let result = Message::tool_result(message_id, call.id.clone(), content, true)?;
    Ok(NewEvent::surface(
        EventKind::ToolResult {
            turn,
            step,
            message: result,
            error: Some(ToolFailure {
                name: failure_name.to_owned(),
                code: code.to_owned(),
            }),
            meta: None,
        },
        SurfaceIntent::append().with_sources(vec![call_seq]),
    ))
}

fn proposed_config(
    config: &AgentLoopConfig,
    previous: Option<&EpochHeader>,
    header_logged: bool,
) -> Result<LlmCallConfig, AgentLoopError> {
    if header_logged {
        if let Some(previous) = previous {
            let defaults = previous.adapter_defaults.clone().unwrap_or_default();
            return Ok(previous.config.without_adapter_defaults(&defaults)?);
        }
        return Ok(config.call.clone());
    }
    let Some(previous) = previous else {
        return Ok(config.call.clone());
    };
    let same_route = previous.config.provider() == config.call.provider()
        && previous.config.model() == config.call.model();
    let explicit_effort = same_route
        .then_some(previous)
        .filter(|header| {
            header
                .adapter_defaults
                .as_ref()
                .is_none_or(|defaults| defaults.reasoning_effort.is_none())
        })
        .and_then(|header| header.config.reasoning_effort());
    Ok(config
        .call
        .with_reasoning_effort_if_absent(explicit_effort)?)
}

fn next_id(runtime: &dyn AgentRuntime, kind: AgentIdKind) -> Result<String, AgentRuntimeError> {
    let id = runtime.next_id(kind)?;
    if id.is_empty() || id.len() > 1_024 || id.chars().any(char::is_control) {
        return Err(AgentRuntimeError::EmptyId {
            kind: kind.prefix(),
        });
    }
    Ok(id)
}

fn checked_sample(runtime: &dyn AgentRuntime) -> Result<f64, AgentRuntimeError> {
    let sample = runtime.sample_unit()?;
    if !sample.is_finite() || !(0.0..=1.0).contains(&sample) {
        return Err(AgentRuntimeError::InvalidSample);
    }
    Ok(sample)
}

fn failure_reason(code: &str, message: &str) -> Result<LlmFailure, AgentLoopError> {
    Ok(LlmFailure::new(message, code)?)
}

fn failure_from_display(
    code: &str,
    prefix: &str,
    _error: &impl std::fmt::Display,
) -> Result<LlmFailure, AgentLoopError> {
    // Provider implementations are extension boundaries. Their Display text
    // may contain prompts, credentials, or server payloads, so durable session
    // facts use only this stable agent-owned summary.
    failure_reason(code, prefix)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchBarrier {
    Ready,
    ObserverUnavailable,
}

async fn dispatch_barrier(
    reservation: &mut SessionReservation<'_>,
) -> Result<DispatchBarrier, AgentLoopError> {
    match reservation.flush_barrier().await {
        Ok(()) => Ok(DispatchBarrier::Ready),
        Err(BarrierError::ObserverUnavailable) => Ok(DispatchBarrier::ObserverUnavailable),
        Err(error) => Err(error.into()),
    }
}

fn observer_unavailable_failure() -> Result<LlmFailure, AgentLoopError> {
    failure_reason(
        "AGENT_OBSERVER_UNAVAILABLE",
        "the live session observer became unavailable",
    )
}

fn is_budget_error(error: &AppendError) -> bool {
    is_memory_budget_error(error) || is_durable_session_limit(error)
}

fn is_memory_budget_error(error: &AppendError) -> bool {
    matches!(
        error,
        AppendError::EventLimit { .. }
            | AppendError::RetainedJsonLimit { .. }
            | AppendError::ReservedEventLimit { .. }
            | AppendError::ReservedRetainedJsonLimit { .. }
    )
}

fn is_durable_session_limit(error: &AppendError) -> bool {
    matches!(
        error,
        AppendError::DurableRecord
            | AppendError::DurableEventLimit { .. }
            | AppendError::DurableByteLimit { .. }
            | AppendError::DurableResidentLimit { .. }
    )
}

fn is_fatal_loop_error(error: &AgentLoopError) -> bool {
    match error {
        AgentLoopError::Barrier(_) | AgentLoopError::Store(_) => true,
        AgentLoopError::Session(error) => matches!(
            error,
            AppendError::NeedsMaterialization
                | AppendError::DurableAsyncRequired
                | AppendError::DurablePoisoned
                | AppendError::DurableWriter
                | AppendError::NeedsAppendSettle
        ),
        _ => false,
    }
}
