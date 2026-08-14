//! Bounded, cancellable Agent Loop joining sessions, providers, and tools.

mod assembler;
mod error;
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
    model::{
        CallId, ContentBlock, ContentBlockKind, FinishReasonKind, JsonValue, LlmCallConfig,
        LlmFailure, Message, MessageRole, MessageSource, StreamChunkKind, ToolSchema,
    },
    provider::{ModelProvider, ProviderRequest, RetryMode, StreamValidator},
    session::{
        AppendError, ClaimedAppend, EpochHeader, EventClaim, EventKind, EventSeq, LlmRetryEvent,
        LlmRetryStartedEvent, NewEvent, RequestContext, RequestHeaderReason, RetryId, RetryNumber,
        Session, SessionReservation, StepId, SurfaceIntent, ToolFailure, TurnEndCancelCause,
        TurnEndReason, TurnId,
    },
};

pub use error::{AgentBuildError, AgentLoopError, AgentRuntimeError};
pub use tool::{
    NoTools, ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
    ToolExecutorError,
};

use assembler::{AssembledAssistant, AssistantAssembler, without_tool_calls};
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
/// Extra time given to an already-started tool to observe cancellation and
/// release its own resources. This is separate from the normal tool timeout.
pub const MAX_AGENT_TOOL_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
pub const MAX_AGENT_TOOL_ARGUMENT_BYTES: usize = 256 * 1024;
pub const MAX_AGENT_TOOL_RESULT_BYTES: usize = 256 * 1024;
pub const MAX_AGENT_TOOL_RESULTS_PER_TURN_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_AGENT_FIXED_REQUEST_BYTES: usize = 4 * 1024 * 1024;

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
}

impl AgentIdKind {
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Retry => "retry",
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

#[derive(Clone, Copy, Debug, Default)]
pub struct SystemAgentRuntime;

impl AgentRuntime for SystemAgentRuntime {
    fn next_id(&self, kind: AgentIdKind) -> Result<String, AgentRuntimeError> {
        Ok(format!("{}-{}", kind.prefix(), uuid::Uuid::new_v4()))
    }

    fn sample_unit(&self) -> Result<f64, AgentRuntimeError> {
        let value = uuid::Uuid::new_v4().as_u128();
        Ok(value as f64 / u128::MAX as f64)
    }
}

/// Counters and the exact reason committed by a finished turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TurnOutcome {
    turn: TurnId,
    reason: TurnEndReason,
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

    #[must_use]
    pub fn reason(&self) -> &TurnEndReason {
        &self.reason
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
            Arc::new(SystemAgentRuntime),
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
        if session.state().open_turn().is_some() {
            return Err(AgentBuildError::SessionNotIdle);
        }
        if session_has_unresolved_tool_calls(&session) {
            return Err(AgentBuildError::UnresolvedToolCall);
        }
        config.validate_fixed_request_size()?;
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
        if (result.is_err() && self.session.state().open_turn().is_some())
            || session_has_unresolved_tool_calls(&self.session)
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

enum StreamOutcome {
    Finished(AssembledAssistant, Vec<EventSeq>),
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
    deadline: Instant,
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
    let turn_fallback = TurnEndReason::Error {
        error: budget_reason.clone(),
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
    reservation.settle_exact(&mut turn_start)?;

    let mut driver = Driver {
        provider,
        tools,
        runtime,
        config,
        request_header_logged,
        counters: Counters::default(),
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

    let settlement = reservation.settle(
        &mut turn_end,
        NewEvent::log(EventKind::turn_end(turn, reason.clone())),
    )?;
    if matches!(settlement, ClaimedAppend::Fallback(_)) {
        reason = turn_fallback;
    }
    Ok(TurnOutcome {
        turn,
        reason,
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
        let mut claims = match reservation.claim_batch(exact) {
            Ok(claims) => claims,
            Err(error) if is_budget_error(&error) => {
                return Ok(TurnEndReason::Error {
                    error: budget_failure.clone(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut step_start = claims.remove(0);
        reservation.settle_exact(&mut step_start)?;
        for _ in 0..messages.len() {
            let mut message = claims.remove(0);
            reservation.settle_exact(&mut message)?;
        }
        let mut step_end = claims.remove(0);
        messages.clear();
        driver.counters.steps += 1;

        let outcome = match AssertUnwindSafe(run_step(
            reservation,
            driver,
            turn,
            step,
            cancellation,
            budget_failure,
        ))
        .catch_unwind()
        .await
        {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) | Err(_) => StepOutcome::Error(failure_reason(
                "AGENT_INTERNAL",
                "the agent stopped after an internal failure",
            )?),
        };
        reservation.settle_exact(&mut step_end)?;
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
        match outcome {
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

async fn run_step(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    turn: TurnId,
    step: StepId,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<StepOutcome, AgentLoopError> {
    let mut retry_chains: BTreeMap<(String, String), (RetryId, usize)> = BTreeMap::new();
    let mut retries_in_step = 0_usize;
    loop {
        if cancellation.is_cancelled() {
            return Ok(StepOutcome::Cancelled);
        }
        if Instant::now() >= driver.deadline {
            return Ok(StepOutcome::Error(failure_reason(
                "AGENT_TURN_TIMEOUT",
                "the agent turn timed out",
            )?));
        }
        if driver.counters.attempts >= driver.config.limits.max_attempts_per_turn {
            return Ok(StepOutcome::Error(failure_reason(
                "AGENT_MAX_MODEL_ATTEMPTS",
                "the agent reached its model-attempt limit",
            )?));
        }
        driver.counters.attempts += 1;

        let proposed = proposed_config(
            driver.config,
            reservation.session().request_header(),
            *driver.request_header_logged,
        )?;
        let prepared =
            match catch_unwind(AssertUnwindSafe(|| driver.provider.prepare_call(proposed))) {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(error)) => {
                    return Ok(StepOutcome::Error(failure_from_display(
                        "AGENT_PROVIDER_PREPARE",
                        "provider preparation failed",
                        &error,
                    )?));
                }
                Err(_) => {
                    return Ok(StepOutcome::Error(failure_reason(
                        "AGENT_PROVIDER_PANIC",
                        "the provider panicked while preparing a request",
                    )?));
                }
            };
        let effective_config = prepared.config().clone();
        if !effective_config.max_tokens().is_some_and(|maximum| {
            maximum.get() > 0 && maximum.get() <= driver.config.limits.max_output_tokens_per_request
        }) {
            return Ok(StepOutcome::Error(failure_reason(
                "AGENT_MAX_OUTPUT_TOKENS",
                "the prepared model request exceeds the agent output-token limit",
            )?));
        }
        let retry_policy = prepared.retry_policy().clone();
        let adapter_defaults = prepared.adapter_defaults().clone();
        let context_window = prepared.context_window();
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
            match reservation.append(NewEvent::log(EventKind::RequestHeader {
                header: header.clone(),
                reason,
            })) {
                Ok(_) => *driver.request_header_logged = true,
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepOutcome::Error(budget_failure.clone()));
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
            match reservation.append(NewEvent::log(EventKind::RequestContext {
                context: context.clone(),
            })) {
                Ok(_) => {}
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepOutcome::Error(budget_failure.clone()));
                }
                Err(error) => return Err(error.into()),
            }
        }

        let request_result = (|| {
            let mut request = ProviderRequest::new(prepared, reservation.session().messages())?;
            if let Some(system) = &driver.config.system {
                request = request.with_system(system.clone())?;
            }
            if !driver.config.tools.is_empty() {
                request = request.with_tools(driver.config.tools.clone())?;
            }
            request.with_session_id(reservation.session().id().clone())
        })();
        let request = match request_result {
            Ok(request) => request,
            Err(error) => {
                return Ok(StepOutcome::Error(failure_from_display(
                    "AGENT_REQUEST",
                    "model request construction failed",
                    &error,
                )?));
            }
        };

        let attempt_cancellation = cancellation.child_token();
        let stream = match catch_unwind(AssertUnwindSafe(|| {
            driver
                .provider
                .stream(request, attempt_cancellation.clone())
        })) {
            Ok(stream) => stream,
            Err(_) => {
                attempt_cancellation.cancel();
                return Ok(StepOutcome::Error(failure_reason(
                    "AGENT_PROVIDER_PANIC",
                    "the provider panicked while opening a stream",
                )?));
            }
        };
        let streamed = consume_stream(
            reservation,
            driver,
            turn,
            step,
            stream,
            cancellation,
            budget_failure,
        )
        .await;
        if !matches!(streamed, Ok(StreamOutcome::Finished(_, _))) {
            attempt_cancellation.cancel();
        }
        let streamed = streamed?;
        let (assembled, source_seqs) = match streamed {
            StreamOutcome::Cancelled => return Ok(StepOutcome::Cancelled),
            StreamOutcome::Error(error) => return Ok(StepOutcome::Error(error)),
            StreamOutcome::Finished(assembled, sources) => (assembled, sources),
        };

        // Cancellation can race with the provider's final item. Re-check it
        // before publishing an assistant message or starting any tool work.
        if cancellation.is_cancelled() {
            attempt_cancellation.cancel();
            return Ok(StepOutcome::Cancelled);
        }
        if Instant::now() >= driver.deadline {
            attempt_cancellation.cancel();
            return Ok(StepOutcome::Error(failure_reason(
                "AGENT_TURN_TIMEOUT",
                "the agent turn timed out",
            )?));
        }

        let provider_failure = match assembled.finish.kind() {
            FinishReasonKind::Error { failure } | FinishReasonKind::Aborted { failure } => {
                Some(failure.clone())
            }
            _ => None,
        };
        if let Some(failure) = provider_failure {
            if cancellation.is_cancelled() {
                return Ok(StepOutcome::Cancelled);
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
                return Ok(StepOutcome::Error(failure));
            }
            if retries_in_step >= driver.config.limits.max_retries_per_step {
                return Ok(StepOutcome::Error(failure_reason(
                    "AGENT_MAX_RETRIES",
                    "the agent reached its retry limit",
                )?));
            }
            if driver.counters.attempts >= driver.config.limits.max_attempts_per_turn {
                return Ok(StepOutcome::Error(failure_reason(
                    "AGENT_MAX_MODEL_ATTEMPTS",
                    "the agent reached its model-attempt limit",
                )?));
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
                return Ok(StepOutcome::Error(failure));
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
            match reservation.append(NewEvent::log(EventKind::llm_retry(retry_event))) {
                Ok(_) => {}
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepOutcome::Error(budget_failure.clone()));
                }
                Err(error) => return Err(error.into()),
            }
            retry_chains.insert(chain_key, (retry_id.clone(), prior + 1));
            retries_in_step += 1;
            driver.counters.retries += 1;

            let delay = Duration::from_secs_f64(delay_ms.get() / 1_000.0);
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Ok(StepOutcome::Cancelled),
                _ = tokio::time::sleep_until(driver.deadline) => {
                    return Ok(StepOutcome::Error(failure_reason(
                        "AGENT_TURN_TIMEOUT",
                        "the agent turn timed out",
                    )?));
                }
                _ = tokio::time::sleep(delay) => {}
            }
            if cancellation.is_cancelled() {
                return Ok(StepOutcome::Cancelled);
            }
            if Instant::now() >= driver.deadline {
                return Ok(StepOutcome::Error(failure_reason(
                    "AGENT_TURN_TIMEOUT",
                    "the agent turn timed out",
                )?));
            }
            let started = LlmRetryStartedEvent::new(retry_id, turn, step, number)?;
            match reservation.append(NewEvent::log(EventKind::llm_retry_started(started))) {
                Ok(_) => {}
                Err(error) if is_budget_error(&error) => {
                    return Ok(StepOutcome::Error(budget_failure.clone()));
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
    turn: TurnId,
    step: StepId,
    mut stream: crate::provider::ProviderStream,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<StreamOutcome, AgentLoopError> {
    let mut validator = StreamValidator::default();
    let mut assembler = AssistantAssembler::default();
    let mut sources = Vec::new();
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
            if let Err(error) = validator.complete() {
                return Ok(StreamOutcome::Error(failure_from_display(
                    "AGENT_PROVIDER_PROTOCOL",
                    "the provider stream ended incorrectly",
                    &error,
                )?));
            }
            let assembled = assembler.finish().ok_or(AgentLoopError::Invariant(
                "validated stream had no terminal finish",
            ))?;
            return Ok(StreamOutcome::Finished(assembled, sources));
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
        if let Err(error) = validator.accept(&chunk) {
            return Ok(StreamOutcome::Error(failure_from_display(
                "AGENT_PROVIDER_PROTOCOL",
                "the provider emitted an invalid stream",
                &error,
            )?));
        }
        let seq = match reservation.append(NewEvent::log(EventKind::assistant_chunk(
            turn,
            step,
            chunk.clone(),
        ))) {
            Ok(event) => event.seq(),
            Err(error) if is_budget_error(&error) => {
                return Ok(StreamOutcome::Error(budget_failure.clone()));
            }
            Err(error) => return Err(error.into()),
        };
        if let StreamChunkKind::Usage { usage } = chunk.kind() {
            driver.counters.reported_output_tokens = driver
                .counters
                .reported_output_tokens
                .checked_add(usage.output_tokens().get())
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
        sources.push(seq);
        assembler.push(&chunk);
    }
}

#[allow(clippy::too_many_arguments)]
async fn commit_successful_attempt(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    turn: TurnId,
    step: StepId,
    config: LlmCallConfig,
    mut assembled: AssembledAssistant,
    source_seqs: Vec<EventSeq>,
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<StepOutcome, AgentLoopError> {
    let max_tokens = matches!(assembled.finish.kind(), FinishReasonKind::MaxTokens);
    if max_tokens {
        assembled.content = without_tool_calls(assembled.content);
    }
    let message = Message::new(
        next_id(driver.runtime, AgentIdKind::Message)?,
        MessageRole::Assistant,
        assembled.content,
        MessageSource::model_with_replay_state(
            config.provider(),
            config.model(),
            assembled.replay_state,
        )?,
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
        return Ok(StepOutcome::Error(failure_reason(code, message)?));
    }
    let assistant = NewEvent::surface(
        EventKind::AssistantMessage {
            turn,
            step,
            message,
            usage: assembled.usage,
        },
        SurfaceIntent::append().with_sources(source_seqs),
    );
    if tool_calls.is_empty() {
        match reservation.append(assistant) {
            Ok(_) => {}
            Err(error) if is_budget_error(&error) => {
                return Ok(StepOutcome::Error(budget_failure.clone()));
            }
            Err(error) => return Err(error.into()),
        }
        return Ok(if max_tokens {
            StepOutcome::MaxTokens
        } else {
            StepOutcome::Completed
        });
    }

    commit_tool_round(
        reservation,
        driver,
        turn,
        step,
        assistant,
        tool_calls,
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
    cancellation: &CancellationToken,
    budget_failure: &LlmFailure,
) -> Result<StepOutcome, AgentLoopError> {
    let first_seq = reservation
        .session()
        .next_seq()
        .ok_or(AgentLoopError::Invariant("session sequence exhausted"))?;
    let mut result_ids = Vec::with_capacity(calls.len());
    let mut fallbacks = Vec::with_capacity(1 + calls.len() * 2);
    fallbacks.push(assistant);
    for (index, call) in calls.iter().enumerate() {
        let call_offset = 1_u64 + (index as u64) * 2;
        let call_seq = EventSeq::new(first_seq.get() + call_offset)
            .map_err(|_| AgentLoopError::Invariant("tool call sequence exhausted"))?;
        let result_id = next_id(driver.runtime, AgentIdKind::Message)?;
        fallbacks.push(NewEvent::log(EventKind::tool_call(
            turn,
            step,
            call.id.clone(),
            call.name.clone(),
            call.arguments.clone(),
        )));
        fallbacks.push(tool_error_event(
            turn,
            step,
            &result_id,
            call,
            call_seq,
            "TOOL_OUTPUT_BUDGET_EXCEEDED",
            call.name.as_str(),
            "tool output could not fit safely in the session",
        )?);
        result_ids.push((call_seq, result_id));
    }
    let mut claims = match reservation.claim_batch(fallbacks) {
        Ok(claims) => claims,
        Err(error) if is_budget_error(&error) => {
            return Ok(StepOutcome::Error(budget_failure.clone()));
        }
        Err(error) => return Err(error.into()),
    };
    let mut assistant_claim = claims.remove(0);
    reservation.settle_exact(&mut assistant_claim)?;
    let mut planned = Vec::with_capacity(calls.len());
    for (call, (call_seq, result_message_id)) in calls.into_iter().zip(result_ids) {
        let call_claim = claims.remove(0);
        let result_claim = claims.remove(0);
        planned.push(PlannedTool {
            call,
            call_seq,
            result_message_id,
            call_claim,
            result_claim,
        });
    }
    driver.counters.tool_calls += planned.len();

    let mut cancelled = false;
    let mut concludes_turn = false;
    let mut infrastructure_failure = None;
    for index in 0..planned.len() {
        let (completed, remaining) = planned.split_at_mut(index + 1);
        let plan = &mut completed[index];
        reservation.settle_exact(&mut plan.call_claim)?;
        let result = if infrastructure_failure.is_some() || cancelled || cancellation.is_cancelled()
        {
            cancelled |= cancellation.is_cancelled();
            ToolRun::ModelError {
                code: "ABORTED_BEFORE_DISPATCH",
                message: "tool was not started because the turn was stopping",
            }
        } else {
            run_one_tool(driver, &plan.call, cancellation).await
        };
        match result {
            ToolRun::Completed(result) => {
                let requested_conclusion = result.concludes_turn();
                let committed_preferred = settle_tool_result(reservation, driver, plan, result)?;
                concludes_turn |= requested_conclusion && committed_preferred;
            }
            ToolRun::ModelError { code, message } => {
                if code == "ABORTED" {
                    cancelled = true;
                }
                let failure_name = if matches!(code, "ABORTED" | "ABORTED_BEFORE_DISPATCH") {
                    "AbortError"
                } else {
                    plan.call.name.as_str()
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
                reservation.settle(&mut plan.result_claim, preferred)?;
            }
            ToolRun::Infrastructure => {
                reservation.release(&mut plan.result_claim)?;
                for later in remaining {
                    reservation.release(&mut later.call_claim)?;
                    reservation.release(&mut later.result_claim)?;
                }
                return Ok(StepOutcome::Error(failure_reason(
                    "AGENT_TOOL_EXECUTOR",
                    "the tool executor failed before producing a result",
                )?));
            }
            ToolRun::TurnTimeout => {
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
                reservation.settle(&mut plan.result_claim, preferred)?;
                infrastructure_failure = Some(failure_reason(
                    "AGENT_TURN_TIMEOUT",
                    "the agent turn timed out",
                )?);
            }
        }
    }
    if let Some(error) = infrastructure_failure {
        return Ok(StepOutcome::Error(error));
    }
    if cancelled {
        return Ok(StepOutcome::Cancelled);
    }
    if concludes_turn {
        return Ok(StepOutcome::Completed);
    }
    Ok(StepOutcome::Continue)
}

enum ToolRun {
    Completed(ToolExecutionResult),
    ModelError {
        code: &'static str,
        message: &'static str,
    },
    Infrastructure,
    TurnTimeout,
}

async fn run_one_tool(
    driver: &Driver<'_>,
    call: &ToolCall,
    cancellation: &CancellationToken,
) -> ToolRun {
    if !driver
        .config
        .tools
        .iter()
        .any(|tool| tool.name() == call.name)
    {
        return ToolRun::ModelError {
            code: "UNKNOWN_TOOL",
            message: "the requested tool was not declared for this model call",
        };
    }
    let raw = if call.arguments.is_empty() {
        "{}".to_owned()
    } else {
        call.arguments.clone()
    };
    if raw.len() > driver.config.limits.max_tool_argument_bytes {
        return ToolRun::ModelError {
            code: "TOOL_ARGUMENTS_TOO_LARGE",
            message: "tool arguments exceed the configured size limit",
        };
    }
    let parsed = match serde_json::from_str(raw.as_str())
        .ok()
        .and_then(|value| JsonValue::new(value).ok())
    {
        Some(parsed) => parsed,
        None => {
            return ToolRun::ModelError {
                code: "INVALID_TOOL_ARGUMENTS",
                message: "tool arguments are not valid bounded JSON",
            };
        }
    };
    let request = ToolExecutionRequest::new(call.id.clone(), call.name.clone(), raw, parsed);
    let child = cancellation.child_token();
    if cancellation.is_cancelled() {
        child.cancel();
        return ToolRun::ModelError {
            code: "ABORTED_BEFORE_DISPATCH",
            message: "tool was not started because the turn was stopping",
        };
    }
    let future = match catch_unwind(AssertUnwindSafe(|| {
        driver.tools.execute(request, child.clone())
    })) {
        Ok(future) => future,
        Err(_) => {
            child.cancel();
            return ToolRun::Infrastructure;
        }
    };
    let guarded = AssertUnwindSafe(future).catch_unwind();
    tokio::pin!(guarded);
    let interrupted = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            ToolRun::ModelError { code: "ABORTED", message: "tool was cancelled" }
        }
        _ = tokio::time::sleep_until(driver.deadline) => {
            ToolRun::TurnTimeout
        }
        _ = tokio::time::sleep(driver.config.limits.tool_duration) => {
            ToolRun::ModelError { code: "TOOL_TIMEOUT", message: "tool exceeded its configured timeout" }
        }
        result = &mut guarded => return if cancellation.is_cancelled() {
            child.cancel();
            ToolRun::ModelError { code: "ABORTED", message: "tool was cancelled" }
        } else {
            match result {
                Ok(Ok(result)) => ToolRun::Completed(result),
                Ok(Err(_)) | Err(_) => {
                    child.cancel();
                    ToolRun::Infrastructure
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

fn session_has_unresolved_tool_calls(session: &Session) -> bool {
    let mut unresolved: BTreeMap<CallId, usize> = BTreeMap::new();
    for seq in session.state().surface_nodes() {
        let Ok(index) = usize::try_from(seq.get()) else {
            return true;
        };
        let Some(event) = session.events().get(index) else {
            return true;
        };
        match event.kind() {
            EventKind::AssistantMessage { message, .. } => {
                for block in message.content() {
                    if let ContentBlockKind::ToolCall { id, .. } = block.kind() {
                        *unresolved.entry(id.clone()).or_default() += 1;
                    }
                }
            }
            EventKind::ToolResult { message, .. } => {
                let Ok(tool_call_id) = message.validate_tool_result() else {
                    return true;
                };
                let remove = unresolved.get_mut(tool_call_id).is_some_and(|count| {
                    *count -= 1;
                    *count == 0
                });
                if remove {
                    unresolved.remove(tool_call_id);
                }
            }
            _ => {}
        }
    }
    !unresolved.is_empty()
}

fn settle_tool_result(
    reservation: &mut SessionReservation<'_>,
    driver: &mut Driver<'_>,
    plan: &mut PlannedTool,
    result: ToolExecutionResult,
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
    if !component_fits {
        reservation.settle_exact(&mut plan.result_claim)?;
        return Ok(false);
    }
    let message = match Message::tool_result(
        plan.result_message_id.clone(),
        plan.call.id.clone(),
        content,
        is_error,
    ) {
        Ok(message) => message,
        Err(_) => {
            reservation.settle_exact(&mut plan.result_claim)?;
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
        Err(_) => {
            reservation.settle_exact(&mut plan.result_claim)?;
            return Ok(false);
        }
    };
    let inside_limits = size <= driver.config.limits.max_tool_result_bytes
        && driver
            .counters
            .tool_result_bytes
            .checked_add(size)
            .is_some_and(|total| total <= driver.config.limits.max_tool_results_per_turn_bytes);
    if !inside_limits {
        reservation.settle_exact(&mut plan.result_claim)?;
        return Ok(false);
    }
    let settlement = reservation.settle(&mut plan.result_claim, preferred)?;
    let preferred = matches!(settlement, ClaimedAppend::Preferred(_));
    if preferred {
        driver.counters.tool_result_bytes += size;
    }
    Ok(preferred)
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

fn is_budget_error(error: &AppendError) -> bool {
    matches!(
        error,
        AppendError::EventLimit { .. }
            | AppendError::RetainedJsonLimit { .. }
            | AppendError::ReservedEventLimit { .. }
            | AppendError::ReservedRetainedJsonLimit { .. }
    )
}
