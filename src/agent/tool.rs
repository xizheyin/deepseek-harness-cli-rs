use std::{future::Future, pin::Pin, sync::Arc};

use thiserror::Error;
use tokio::time::Instant;
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

/// Future that settles every executor-owned worker or subprocess.
pub type ToolShutdownFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), ToolExecutorError>> + Send + 'a>>;

/// Bounded planning fact sampled before the Agent reserves durable tool events.
///
/// External executors can request the ordinary profile. The crate-controlled
/// action profile has no public constructor because it enables a stronger,
/// non-detachable cleanup contract.
#[derive(Clone, Eq, PartialEq)]
pub struct ToolClaimProfile {
    kind: ToolClaimKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolClaimKind {
    Standard,
    ShellAction,
    PluginAction(String),
}

impl std::fmt::Debug for ToolClaimProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("ToolClaimProfile")
            .field(&match self.kind {
                ToolClaimKind::Standard => "standard",
                ToolClaimKind::ShellAction => "crate-controlled-action",
                ToolClaimKind::PluginAction(_) => "crate-controlled-plugin-action",
            })
            .finish()
    }
}

impl ToolClaimProfile {
    #[must_use]
    pub fn standard() -> Self {
        Self {
            kind: ToolClaimKind::Standard,
        }
    }

    #[must_use]
    pub(crate) fn shell_action() -> Self {
        Self {
            kind: ToolClaimKind::ShellAction,
        }
    }

    pub(crate) fn plugin_action(plugin_id: String) -> Result<Self, ToolExecutorError> {
        let ActionContract::Plugin { plugin_id } = ActionContract::plugin(plugin_id)? else {
            return Err(ToolExecutorError::new(
                "plugin claim contract could not be validated",
            ));
        };
        Ok(Self {
            kind: ToolClaimKind::PluginAction(plugin_id),
        })
    }

    #[must_use]
    pub(crate) fn is_shell_action(&self) -> bool {
        self.kind == ToolClaimKind::ShellAction
    }

    #[must_use]
    pub(crate) fn is_plugin_action(&self) -> bool {
        matches!(self.kind, ToolClaimKind::PluginAction(_))
    }

    #[must_use]
    pub(crate) fn is_owned_action(&self) -> bool {
        matches!(
            self.kind,
            ToolClaimKind::ShellAction | ToolClaimKind::PluginAction(_)
        )
    }

    pub(crate) fn action_contract(&self) -> Option<ActionContract> {
        match &self.kind {
            ToolClaimKind::Standard => None,
            ToolClaimKind::ShellAction => Some(ActionContract::Shell),
            ToolClaimKind::PluginAction(plugin_id) => Some(ActionContract::Plugin {
                plugin_id: plugin_id.clone(),
            }),
        }
    }
}

impl Default for ToolClaimProfile {
    fn default() -> Self {
        Self::standard()
    }
}

/// Per-dispatch unforgeable identity retained by the Agent and sealed action.
#[derive(Clone)]
pub(crate) struct ToolDispatchBinding(Arc<()>);

impl ToolDispatchBinding {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }

    #[must_use]
    pub(crate) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for ToolDispatchBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ToolDispatchBinding(<redacted>)")
    }
}

/// Result of preparing one durable tool call.
pub enum ToolPreparation {
    Complete(ToolExecutionResult),
    Mutation(PreparedToolMutation),
    Action(PreparedToolActionSetup),
}

impl std::fmt::Debug for ToolPreparation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete(result) => formatter.debug_tuple("Complete").field(result).finish(),
            Self::Mutation(mutation) => formatter.debug_tuple("Mutation").field(mutation).finish(),
            Self::Action(action) => formatter.debug_tuple("Action").field(action).finish(),
        }
    }
}

/// Why a prepared foreground action was closed before process creation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ActionDeclineReason {
    PolicyDenied,
    ApprovalRejected,
    ApprovalCancelled,
    ApprovalUnavailable,
    AbortedBeforeDispatch,
    OutputBudgetExceeded,
}

/// First outer-turn stop observed while an owned action was settling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ToolActionTurnStop {
    #[default]
    None,
    CallerCancelled,
    TurnTimeout,
}

/// Control passed to the side-effect-free, owned setup stage.
pub(crate) struct ToolActionSetupControl {
    cancellation: CancellationToken,
    turn_deadline: Instant,
    preparation_deadline: Instant,
}

impl ToolActionSetupControl {
    #[must_use]
    pub(crate) fn new(
        cancellation: CancellationToken,
        turn_deadline: Instant,
        preparation_deadline: Instant,
    ) -> Self {
        Self {
            cancellation,
            turn_deadline,
            preparation_deadline,
        }
    }

    #[must_use]
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    #[must_use]
    pub(crate) fn turn_deadline(&self) -> Instant {
        self.turn_deadline
    }

    #[must_use]
    pub(crate) fn preparation_deadline(&self) -> Instant {
        self.preparation_deadline
    }
}

/// Control passed to the single-use foreground action runner.
pub(crate) struct ToolActionControl {
    cancellation: CancellationToken,
    turn_deadline: Instant,
    action_deadline: Instant,
}

impl ToolActionControl {
    #[must_use]
    pub(crate) fn new(
        cancellation: CancellationToken,
        turn_deadline: Instant,
        action_deadline: Instant,
    ) -> Self {
        Self {
            cancellation,
            turn_deadline,
            action_deadline,
        }
    }

    #[must_use]
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    #[must_use]
    pub(crate) fn turn_deadline(&self) -> Instant {
        self.turn_deadline
    }

    #[must_use]
    pub(crate) fn action_deadline(&self) -> Instant {
        self.action_deadline
    }
}

pub(crate) type ToolActionSetupFuture =
    Pin<Box<dyn Future<Output = ToolActionSetupOutcome> + Send + 'static>>;
pub(crate) type ToolActionSetupFn =
    Box<dyn FnOnce(ToolActionSetupControl) -> ToolActionSetupFuture + Send + 'static>;
pub(crate) type ToolActionFuture =
    Pin<Box<dyn Future<Output = ToolActionOutcome> + Send + 'static>>;
pub(crate) type ToolActionDeclineFn = Box<
    dyn FnOnce(ActionDeclineReason) -> Result<ToolExecutionResult, ToolExecutorError>
        + Send
        + 'static,
>;
pub(crate) type ToolActionRunFn =
    Box<dyn FnOnce(ToolActionControl) -> ToolActionFuture + Send + 'static>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActionContract {
    Shell,
    Plugin { plugin_id: String },
}

impl ActionContract {
    fn plugin(plugin_id: String) -> Result<Self, ToolExecutorError> {
        let mut bytes = plugin_id.bytes();
        if !matches!(bytes.next(), Some(b'a'..=b'z'))
            || plugin_id.len() > 32
            || !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(ToolExecutorError::new("plugin action ID is invalid"));
        }
        Ok(Self::Plugin { plugin_id })
    }

    pub(crate) fn matches_profile(&self, profile: &ToolClaimProfile) -> bool {
        match (self, &profile.kind) {
            (Self::Shell, ToolClaimKind::ShellAction) => true,
            (
                Self::Plugin {
                    plugin_id: expected,
                },
                ToolClaimKind::PluginAction(actual),
            ) => expected == actual,
            _ => false,
        }
    }
}

/// Sealed carrier for the action's side-effect-free setup stage.
pub struct PreparedToolActionSetup {
    dispatch: ToolDispatchBinding,
    contract: ActionContract,
    resolve: ToolActionSetupFn,
}

impl std::fmt::Debug for PreparedToolActionSetup {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedToolActionSetup")
            .field("dispatch", &"<redacted>")
            .field("single_use_resolve", &true)
            .finish()
    }
}

impl PreparedToolActionSetup {
    pub(crate) fn new(
        dispatch: ToolDispatchBinding,
        resolve: ToolActionSetupFn,
    ) -> Result<Self, ToolExecutorError> {
        Ok(Self {
            dispatch,
            contract: ActionContract::Shell,
            resolve,
        })
    }

    pub(crate) fn new_plugin(
        dispatch: ToolDispatchBinding,
        plugin_id: String,
        resolve: ToolActionSetupFn,
    ) -> Result<Self, ToolExecutorError> {
        Ok(Self {
            dispatch,
            contract: ActionContract::plugin(plugin_id)?,
            resolve,
        })
    }

    #[must_use]
    pub(crate) fn contract(&self) -> &ActionContract {
        &self.contract
    }

    #[must_use]
    pub(crate) fn matches_dispatch(&self, expected: &ToolDispatchBinding) -> bool {
        self.dispatch.ptr_eq(expected)
    }

    pub(crate) fn resolve(self, control: ToolActionSetupControl) -> ToolActionSetupFuture {
        (self.resolve)(control)
    }
}

/// Definite outcome of resolving the pre-spawn action setup.
pub(crate) enum ToolActionSetupOutcome {
    Ready(PreparedToolAction),
    NotStarted {
        turn_stop: ToolActionTurnStop,
        result: ToolExecutionResult,
    },
    Infrastructure {
        turn_stop: ToolActionTurnStop,
    },
}

/// Sealed, fully owned, single-use foreground action.
pub struct PreparedToolAction {
    dispatch: ToolDispatchBinding,
    contract: ActionContract,
    prompt: ApprovalPrompt,
    maximum_result_event_bytes: usize,
    decline: ToolActionDeclineFn,
    run: ToolActionRunFn,
}

impl std::fmt::Debug for PreparedToolAction {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedToolAction")
            .field("dispatch", &"<redacted>")
            .field("prompt", &self.prompt)
            .field(
                "maximum_result_event_bytes",
                &self.maximum_result_event_bytes,
            )
            .field("single_use_decline", &true)
            .field("single_use_run", &true)
            .finish()
    }
}

impl PreparedToolAction {
    pub(crate) fn new(
        dispatch: ToolDispatchBinding,
        prompt: ApprovalPrompt,
        maximum_result_event_bytes: usize,
        decline: ToolActionDeclineFn,
        run: ToolActionRunFn,
    ) -> Result<Self, ToolExecutorError> {
        if maximum_result_event_bytes == 0
            || maximum_result_event_bytes > super::MAX_AGENT_ACTION_RESULT_EVENT_BYTES
        {
            return Err(ToolExecutorError::new(
                "prepared action result bound is outside the supported range",
            ));
        }
        Ok(Self {
            dispatch,
            contract: ActionContract::Shell,
            prompt,
            maximum_result_event_bytes,
            decline,
            run,
        })
    }

    pub(crate) fn new_plugin(
        dispatch: ToolDispatchBinding,
        plugin_id: String,
        prompt: ApprovalPrompt,
        maximum_result_event_bytes: usize,
        decline: ToolActionDeclineFn,
        run: ToolActionRunFn,
    ) -> Result<Self, ToolExecutorError> {
        let mut action = Self::new(dispatch, prompt, maximum_result_event_bytes, decline, run)?;
        action.contract = ActionContract::plugin(plugin_id)?;
        Ok(action)
    }

    #[must_use]
    pub(crate) fn contract(&self) -> &ActionContract {
        &self.contract
    }

    #[must_use]
    pub(crate) fn matches_dispatch(&self, expected: &ToolDispatchBinding) -> bool {
        self.dispatch.ptr_eq(expected)
    }

    #[must_use]
    pub(crate) fn prompt(&self) -> &ApprovalPrompt {
        &self.prompt
    }

    #[must_use]
    pub(crate) fn maximum_result_event_bytes(&self) -> usize {
        self.maximum_result_event_bytes
    }

    pub(crate) fn decline(
        self,
        reason: ActionDeclineReason,
    ) -> Result<ToolExecutionResult, ToolExecutorError> {
        let contract = self.contract.clone();
        let result = (self.decline)(reason)?;
        validate_action_result(&result, &contract, false, true)?;
        Ok(result)
    }

    pub(crate) fn run(self, control: ToolActionControl) -> ToolActionFuture {
        (self.run)(control)
    }
}

/// Definite lifecycle outcome of a sealed foreground action.
pub(crate) enum ToolActionOutcome {
    NotStarted {
        turn_stop: ToolActionTurnStop,
        result: ToolExecutionResult,
    },
    Infrastructure {
        turn_stop: ToolActionTurnStop,
    },
    StartedAndQuiescent {
        turn_stop: ToolActionTurnStop,
        result: ToolExecutionResult,
    },
    StartedOwnershipLost {
        turn_stop: ToolActionTurnStop,
    },
}

pub(crate) fn validate_action_not_started_result(
    result: &ToolExecutionResult,
    contract: &ActionContract,
) -> Result<(), ToolExecutorError> {
    validate_action_result(result, contract, false, true)
}

pub(crate) fn validate_action_started_result(
    result: &ToolExecutionResult,
    contract: &ActionContract,
) -> Result<(), ToolExecutorError> {
    validate_action_result(result, contract, true, false)
}

fn validate_action_result(
    result: &ToolExecutionResult,
    contract: &ActionContract,
    expected_started: bool,
    require_error: bool,
) -> Result<(), ToolExecutorError> {
    let fields = result
        .meta()
        .and_then(|meta| meta.as_value().as_object())
        .ok_or_else(|| ToolExecutorError::new("an action result requires object metadata"))?;
    if require_error && !result.is_error() {
        return Err(ToolExecutorError::new(
            "a not-started action result must be an error",
        ));
    }
    if fields.contains_key("committed") {
        return Err(ToolExecutorError::new(
            "an action result must not use file-mutation committed metadata",
        ));
    }
    if let ActionContract::Plugin { plugin_id } = contract {
        return validate_plugin_action_result(result, fields, plugin_id, expected_started);
    }
    if fields.get("started").and_then(serde_json::Value::as_bool) != Some(expected_started)
        || fields.get("kind").and_then(serde_json::Value::as_str) != Some("foreground")
    {
        return Err(ToolExecutorError::new(
            "a shell action result has inconsistent contract metadata",
        ));
    }
    let exit_code = fields.get("exitCode");
    let signal = fields.get("signal");
    if !expected_started {
        if !exit_code.is_some_and(serde_json::Value::is_null)
            || !signal.is_some_and(serde_json::Value::is_null)
        {
            return Err(ToolExecutorError::new(
                "a not-started action result must retain null status fields",
            ));
        }
        return Ok(());
    }
    let exited = exit_code.and_then(serde_json::Value::as_i64).is_some()
        && signal.is_some_and(serde_json::Value::is_null);
    let signalled = exit_code.is_some_and(serde_json::Value::is_null)
        && signal
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty());
    if exited == signalled {
        return Err(ToolExecutorError::new(
            "a quiescent action result requires exactly one termination status",
        ));
    }
    let boolean_fields = [
        "timedOut",
        "aborted",
        "outputLimitExceeded",
        "pipeSetupFailed",
        "pipeReadFailed",
        "signalDeliveryFailed",
        "pipeDrainTimedOut",
        "stdoutTruncated",
        "stderrTruncated",
    ];
    if boolean_fields.iter().any(|name| {
        fields
            .get(*name)
            .and_then(serde_json::Value::as_bool)
            .is_none()
    }) || fields
        .get("timeoutMs")
        .and_then(serde_json::Value::as_u64)
        .is_none()
        || fields
            .get("workdir")
            .and_then(serde_json::Value::as_str)
            .is_none()
    {
        return Err(ToolExecutorError::new(
            "a started action result is missing complete lifecycle metadata",
        ));
    }
    Ok(())
}

fn validate_plugin_action_result(
    result: &ToolExecutionResult,
    fields: &serde_json::Map<String, serde_json::Value>,
    plugin_id: &str,
    expected_dispatched: bool,
) -> Result<(), ToolExecutorError> {
    const KEYS: [&str; 5] = ["kind", "pluginId", "dispatched", "peerSettled", "quiescent"];
    if fields.len() != KEYS.len()
        || KEYS.iter().any(|key| !fields.contains_key(*key))
        || fields.get("kind").and_then(serde_json::Value::as_str) != Some("plugin")
        || fields.get("pluginId").and_then(serde_json::Value::as_str) != Some(plugin_id)
        || fields
            .get("dispatched")
            .and_then(serde_json::Value::as_bool)
            != Some(expected_dispatched)
        || fields
            .get("peerSettled")
            .and_then(serde_json::Value::as_bool)
            .is_none()
        || fields.get("quiescent").and_then(serde_json::Value::as_bool) != Some(true)
    {
        return Err(ToolExecutorError::new(
            "a plugin action result has inconsistent contract metadata",
        ));
    }
    let peer_settled = fields.get("peerSettled") == Some(&serde_json::Value::Bool(true));
    if !expected_dispatched && peer_settled {
        return Err(ToolExecutorError::new(
            "a non-dispatched plugin action cannot have a matching peer result",
        ));
    }
    if expected_dispatched && !peer_settled {
        let is_unknown = result.is_error()
            && result
                .error()
                .is_some_and(|failure| failure.code == crate::session::TOOL_OUTCOME_UNKNOWN);
        if !is_unknown {
            return Err(ToolExecutorError::new(
                "a dispatched plugin action without a peer result must report an unknown outcome",
            ));
        }
    }
    Ok(())
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
    dispatch: ToolDispatchBinding,
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
        dispatch: ToolDispatchBinding,
    ) -> Self {
        Self {
            call_id,
            name,
            raw_arguments,
            arguments,
            dispatch,
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

    #[must_use]
    pub(crate) fn dispatch_binding(&self) -> &ToolDispatchBinding {
        &self.dispatch
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
    /// Return the bounded claim profile for a declared tool name.
    ///
    /// This method must be prompt, pure, and stable for the executor's schema
    /// snapshot. The Agent calls it once before admitting any events for the
    /// tool round.
    fn claim_profile(&self, _tool_name: &str) -> ToolClaimProfile {
        ToolClaimProfile::standard()
    }

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

    /// Stop and join every executor-owned background resource.
    ///
    /// Ordinary in-process tools own no persistent resources, so their
    /// default is already settled. Executors that own subprocesses must make
    /// this operation idempotent and cancellation-safe.
    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        Box::pin(async { Ok(()) })
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

#[cfg(test)]
mod tests {
    use crate::{
        model::{ContentBlock, JsonValue},
        session::{TOOL_OUTCOME_UNKNOWN, ToolFailure},
    };

    use super::{
        ActionContract, ToolExecutionResult, validate_action_not_started_result,
        validate_action_started_result,
    };

    fn plugin_result(
        dispatched: bool,
        peer_settled: bool,
        quiescent: bool,
        code: Option<&str>,
        extra: bool,
    ) -> ToolExecutionResult {
        let mut meta = serde_json::json!({
            "kind":"plugin",
            "pluginId":"text-tools",
            "dispatched":dispatched,
            "peerSettled":peer_settled,
            "quiescent":quiescent,
        });
        if extra {
            meta.as_object_mut()
                .unwrap()
                .insert("internalId".to_owned(), 7_u64.into());
        }
        let error = code.map(|code| ToolFailure {
            name: "PluginError".to_owned(),
            code: code.to_owned(),
        });
        ToolExecutionResult::new(
            vec![ContentBlock::text(if error.is_some() { "error" } else { "ok" }).unwrap()],
            error.is_some(),
            error,
            Some(JsonValue::new(meta).unwrap()),
            false,
        )
        .unwrap()
    }

    #[test]
    fn plugin_action_metadata_is_closed_and_matches_dispatch_truth() {
        let contract = ActionContract::Plugin {
            plugin_id: "text-tools".to_owned(),
        };
        assert!(
            validate_action_not_started_result(
                &plugin_result(false, false, true, Some("PLUGIN_POLICY_DENIED"), false),
                &contract,
            )
            .is_ok()
        );
        assert!(
            validate_action_started_result(
                &plugin_result(true, true, true, None, false),
                &contract,
            )
            .is_ok()
        );
        assert!(
            validate_action_started_result(
                &plugin_result(true, false, true, Some(TOOL_OUTCOME_UNKNOWN), false),
                &contract,
            )
            .is_ok()
        );

        for invalid in [
            plugin_result(false, false, true, Some("PLUGIN_POLICY_DENIED"), true),
            plugin_result(false, false, false, Some("PLUGIN_POLICY_DENIED"), false),
            plugin_result(false, true, true, Some("PLUGIN_POLICY_DENIED"), false),
        ] {
            assert!(validate_action_not_started_result(&invalid, &contract).is_err());
        }
        for invalid in [
            plugin_result(true, false, true, None, false),
            plugin_result(true, false, true, Some("PLUGIN_TIMEOUT"), false),
            plugin_result(true, true, false, Some("PLUGIN_TIMEOUT"), false),
        ] {
            assert!(validate_action_started_result(&invalid, &contract).is_err());
        }
    }

    #[test]
    fn plugin_claim_contract_rejects_shell_and_cross_plugin_carriers() {
        let claim = super::ToolClaimProfile::plugin_action("plugin-a".to_owned()).unwrap();
        assert!(
            ActionContract::Plugin {
                plugin_id: "plugin-a".to_owned()
            }
            .matches_profile(&claim)
        );
        assert!(
            !ActionContract::Plugin {
                plugin_id: "plugin-b".to_owned()
            }
            .matches_profile(&claim)
        );
        assert!(!ActionContract::Shell.matches_profile(&claim));
        assert!(super::ToolClaimProfile::plugin_action("INVALID".to_owned()).is_err());
    }
}
