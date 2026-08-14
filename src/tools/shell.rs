use std::{
    ffi::{OsStr, OsString},
    sync::Arc,
    time::Duration,
};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    agent::{
        ActionDeclineReason, ApprovalPrompt, PreparedToolAction, PreparedToolActionSetup,
        ToolActionSetupControl, ToolActionSetupOutcome, ToolActionTurnStop, ToolDispatchBinding,
        ToolExecutionRequest, ToolExecutionResult, ToolExecutorError, ToolPreparation,
    },
    model::{ContentBlock, JsonValue, ToolSchema},
    session::ToolFailure,
};

use super::{
    MAX_TOOL_CONTENT_BYTES,
    error::{ToolCallError, ToolCallResult, ToolRegistryBuildError},
    json_string_content_bytes,
    process::{
        ProcessControl, ProcessLaunchPermit, ProcessOutcome, ProcessPrecheckError,
        ProcessPrimaryCause, ProcessReport, ProcessRequest, ProcessRunner, ProcessStartFailure,
        ProcessTermination, ProcessTurnStop,
    },
    text_block_encoded_bytes,
    workspace::{PreparedShellWorkdir, Workspace},
};

pub(crate) const MAX_SHELL_COMMAND_BYTES: usize = 32 * 1024;
pub(crate) const MAX_SHELL_DESCRIPTION_BYTES: usize = 1024;
pub(crate) const MAX_SHELL_WORKDIR_BYTES: usize = 4096;
pub(crate) const DEFAULT_SHELL_TIMEOUT_MS: u64 = 25_000;
pub(crate) const MAX_SHELL_TIMEOUT_MS: u64 = 295_000;
pub(crate) const MAX_SHELL_RESULT_EVENT_BYTES: usize = 128 * 1024;

const MAX_CHILD_ENVIRONMENT_ENTRIES: usize = 24;
const MAX_CHILD_ENVIRONMENT_BYTES: usize = 32 * 1024;

const COPIED_ENVIRONMENT_NAMES: [&str; 19] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LANGUAGE",
    "TZ",
    "LC_ALL",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_COLLATE",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "CARGO_HOME",
    "RUSTUP_HOME",
];

const FIXED_ENVIRONMENT_OVERRIDES: [(&str, &str); 5] = [
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("GIT_TERMINAL_PROMPT", "0"),
];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ShellArgumentsWire {
    command: String,
    description: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    timeout_ms: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    workdir: Option<String>,
}

/// Fully validated model-supplied shell arguments.
#[derive(Clone)]
pub(crate) struct ShellArguments {
    command: String,
    description: String,
    timeout_ms: u64,
    workdir: String,
}

impl std::fmt::Debug for ShellArguments {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellArguments")
            .field("command_bytes", &self.command.len())
            .field("description_bytes", &self.description.len())
            .field("timeout_ms", &self.timeout_ms)
            .field("workdir_bytes", &self.workdir.len())
            .finish()
    }
}

impl ShellArguments {
    pub(crate) fn command(&self) -> &str {
        &self.command
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    pub(crate) fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    pub(crate) fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    pub(crate) fn workdir(&self) -> &str {
        &self.workdir
    }

    pub(crate) fn into_command(self) -> String {
        self.command
    }
}

pub(crate) fn parse_arguments(value: &Value) -> ToolCallResult<ShellArguments> {
    let wire: ShellArgumentsWire = serde_json::from_value(value.clone()).map_err(|_| {
        ToolCallError::invalid_args("bash arguments must match the advertised object schema")
    })?;

    validate_nonblank(&wire.command, "bash.command")?;
    if wire.command.len() > MAX_SHELL_COMMAND_BYTES
        || wire
            .command
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(ToolCallError::invalid_args(format!(
            "bash.command must be at most {MAX_SHELL_COMMAND_BYTES} bytes and contain no control characters other than newline or tab"
        )));
    }

    validate_nonblank(&wire.description, "bash.description")?;
    if wire.description.len() > MAX_SHELL_DESCRIPTION_BYTES
        || wire.description.chars().any(char::is_control)
    {
        return Err(ToolCallError::invalid_args(format!(
            "bash.description must be at most {MAX_SHELL_DESCRIPTION_BYTES} bytes and contain no control characters"
        )));
    }

    let workdir = wire.workdir.unwrap_or_else(|| ".".to_owned());
    validate_nonblank(&workdir, "bash.workdir")?;
    if workdir.len() > MAX_SHELL_WORKDIR_BYTES || workdir.chars().any(char::is_control) {
        return Err(ToolCallError::invalid_args(format!(
            "bash.workdir must be at most {MAX_SHELL_WORKDIR_BYTES} bytes and contain no control characters"
        )));
    }

    let timeout_ms = wire.timeout_ms.unwrap_or(DEFAULT_SHELL_TIMEOUT_MS);
    if !(1..=MAX_SHELL_TIMEOUT_MS).contains(&timeout_ms) {
        return Err(ToolCallError::invalid_args(format!(
            "bash.timeoutMs must be between 1 and {MAX_SHELL_TIMEOUT_MS}"
        )));
    }

    Ok(ShellArguments {
        command: wire.command,
        description: wire.description,
        timeout_ms,
        workdir,
    })
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn validate_nonblank(value: &str, field: &str) -> ToolCallResult<()> {
    if value.trim().is_empty() {
        return Err(ToolCallError::invalid_args(format!(
            "{field} must not be blank"
        )));
    }
    Ok(())
}

/// Immutable child environment captured once at registry construction.
#[derive(Clone)]
pub(crate) struct ShellEnvironment {
    entries: Arc<[(OsString, OsString)]>,
    retained_bytes: usize,
}

impl std::fmt::Debug for ShellEnvironment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ShellEnvironment")
            .field("entry_count", &self.entries.len())
            .field("retained_bytes", &self.retained_bytes)
            .finish()
    }
}

impl ShellEnvironment {
    pub(crate) fn capture() -> Result<Self, ToolRegistryBuildError> {
        build_environment(|name| std::env::var_os(name))
    }

    pub(crate) fn entries(&self) -> Arc<[(OsString, OsString)]> {
        Arc::clone(&self.entries)
    }
}

fn build_environment(
    mut get: impl FnMut(&str) -> Option<OsString>,
) -> Result<ShellEnvironment, ToolRegistryBuildError> {
    let mut entries = Vec::with_capacity(MAX_CHILD_ENVIRONMENT_ENTRIES);
    let mut retained_bytes = 0_usize;
    for name in COPIED_ENVIRONMENT_NAMES {
        let value = match get(name) {
            Some(value) => value,
            None if name == "PATH" => OsString::from("/usr/bin:/bin"),
            None => continue,
        };
        if value.to_str().is_none() {
            return Err(ToolRegistryBuildError::InvalidEnvironment);
        }
        append_environment_entry(&mut entries, &mut retained_bytes, name, value)?;
    }
    for (name, value) in FIXED_ENVIRONMENT_OVERRIDES {
        append_environment_entry(
            &mut entries,
            &mut retained_bytes,
            name,
            OsString::from(value),
        )?;
    }
    if entries.len() > MAX_CHILD_ENVIRONMENT_ENTRIES {
        return Err(ToolRegistryBuildError::EnvironmentTooLarge);
    }
    Ok(ShellEnvironment {
        entries: entries.into(),
        retained_bytes,
    })
}

fn append_environment_entry(
    entries: &mut Vec<(OsString, OsString)>,
    retained_bytes: &mut usize,
    name: &str,
    value: OsString,
) -> Result<(), ToolRegistryBuildError> {
    let value_bytes = os_str_bytes(value.as_os_str());
    *retained_bytes = retained_bytes
        .checked_add(name.len())
        .and_then(|total| total.checked_add(value_bytes))
        .ok_or(ToolRegistryBuildError::EnvironmentTooLarge)?;
    if *retained_bytes > MAX_CHILD_ENVIRONMENT_BYTES {
        return Err(ToolRegistryBuildError::EnvironmentTooLarge);
    }
    entries.push((OsString::from(name), value));
    Ok(())
}

#[cfg(unix)]
fn os_str_bytes(value: &OsStr) -> usize {
    use std::os::unix::ffi::OsStrExt;
    value.as_bytes().len()
}

#[cfg(not(unix))]
fn os_str_bytes(value: &OsStr) -> usize {
    value.to_string_lossy().len()
}

pub(crate) fn schema() -> Result<ToolSchema, ToolRegistryBuildError> {
    let parameters = JsonValue::new(json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "minLength": 1,
                "maxLength": 32768,
                "description": "Exact bash command; runtime maximum is 32768 UTF-8 bytes"
            },
            "description": {
                "type": "string",
                "minLength": 1,
                "maxLength": 1024,
                "description": "Short display description; runtime maximum is 1024 UTF-8 bytes"
            },
            "timeoutMs": {
                "type": "integer",
                "minimum": 1,
                "maximum": 295000,
                "description": "Command-local timeout in milliseconds"
            },
            "workdir": {
                "type": "string",
                "minLength": 1,
                "maxLength": 4096,
                "description": "Workspace-contained directory; runtime maximum is 4096 UTF-8 bytes"
            }
        },
        "required": ["command", "description"],
        "additionalProperties": false
    }))
    .map_err(|source| ToolRegistryBuildError::InvalidSchema {
        tool: "bash",
        source: source.into(),
    })?;
    ToolSchema::new(
        "bash",
        "Run one bounded foreground Bash command in the retained workspace.",
        parameters,
    )
    .map_err(|source| ToolRegistryBuildError::InvalidSchema {
        tool: "bash",
        source,
    })
}

pub(crate) fn approval_prompt(
    arguments: &ShellArguments,
    workdir: &str,
) -> Result<ApprovalPrompt, ToolExecutorError> {
    let preview = format!(
        "Command:\n{}\n\nWorking directory: {workdir}\nTimeout: {}ms\nEnvironment: cleared; copied when present: {}; fixed overrides: {}",
        arguments.command(),
        arguments.timeout_ms(),
        COPIED_ENVIRONMENT_NAMES.join(", "),
        FIXED_ENVIRONMENT_OVERRIDES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    );
    ApprovalPrompt::new(Some(arguments.description().to_owned()), preview)
        .map_err(|_| ToolExecutorError::new("shell approval prompt normalization failed"))
}

pub(crate) fn approval_required_result() -> Result<ToolExecutionResult, ToolExecutorError> {
    shell_error_result(
        "ApprovalError",
        "APPROVAL_REQUIRED",
        "bash must use the Agent approval Action stage",
        None,
        None,
    )
}

pub(crate) fn invalid_arguments_result(
    error: ToolCallError,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let (name, code, message) = error.into_model_parts()?;
    shell_error_result(name, code, &message, None, None)
}

pub(crate) fn shell_error_result(
    name: &'static str,
    code: &'static str,
    message: &str,
    timeout_ms: Option<u64>,
    workdir: Option<&str>,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let content = ContentBlock::text(format!("Error: {message}"))
        .map_err(|_| ToolExecutorError::new("shell error normalization failed"))?;
    let mut meta = json!({
        "kind": "foreground",
        "started": false,
        "exitCode": null,
        "signal": null
    });
    if let Some(timeout_ms) = timeout_ms {
        meta["timeoutMs"] = json!(timeout_ms);
    }
    if let Some(workdir) = workdir {
        meta["workdir"] = json!(workdir);
    }
    let meta = JsonValue::new(meta)
        .map_err(|_| ToolExecutorError::new("shell error metadata normalization failed"))?;
    ToolExecutionResult::new(
        vec![content],
        true,
        Some(ToolFailure {
            name: name.to_owned(),
            code: code.to_owned(),
        }),
        Some(meta),
        false,
    )
    .map_err(|_| ToolExecutorError::new("shell error normalization failed"))
}

pub(crate) fn declined_result(
    reason: ActionDeclineReason,
    timeout_ms: u64,
    workdir: &str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let (name, code, message) = match reason {
        ActionDeclineReason::PolicyDenied => (
            "ShellPolicyError",
            "SHELL_POLICY_DENIED",
            "shell execution is denied by policy",
        ),
        ActionDeclineReason::ApprovalRejected => (
            "ApprovalError",
            "APPROVAL_REJECTED",
            "shell execution approval was rejected",
        ),
        ActionDeclineReason::ApprovalCancelled => (
            "ApprovalError",
            "APPROVAL_CANCELLED",
            "shell execution approval was cancelled",
        ),
        ActionDeclineReason::ApprovalUnavailable => (
            "ApprovalError",
            "APPROVAL_UNAVAILABLE",
            "shell execution approval is unavailable",
        ),
        ActionDeclineReason::AbortedBeforeDispatch => (
            "AbortError",
            "ABORTED_BEFORE_DISPATCH",
            "shell execution was cancelled before process creation",
        ),
        ActionDeclineReason::OutputBudgetExceeded => (
            "ToolOutputError",
            "TOOL_OUTPUT_BUDGET_EXCEEDED",
            "the shell result cannot fit in the remaining session output budget",
        ),
    };
    shell_error_result(name, code, message, Some(timeout_ms), Some(workdir))
}

fn pre_dispatch_stop_result(
    preparation_timed_out: bool,
    timeout_ms: u64,
    workdir: Option<&str>,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    if preparation_timed_out {
        shell_error_result(
            "ToolTimeoutError",
            "TOOL_TIMEOUT",
            "shell working-directory preparation exceeded its time limit",
            Some(timeout_ms),
            workdir,
        )
    } else {
        shell_error_result(
            "AbortError",
            "ABORTED_BEFORE_DISPATCH",
            "shell execution stopped before process creation",
            Some(timeout_ms),
            workdir,
        )
    }
}

pub(crate) struct PreparedShellInvocation {
    arguments: ShellArguments,
    workdir: PreparedShellWorkdir,
}

impl std::fmt::Debug for PreparedShellInvocation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedShellInvocation")
            .field("arguments", &self.arguments)
            .field("workdir", &self.workdir)
            .finish()
    }
}

impl PreparedShellInvocation {
    pub(crate) fn arguments(&self) -> &ShellArguments {
        &self.arguments
    }

    pub(crate) fn workdir(&self) -> &PreparedShellWorkdir {
        &self.workdir
    }

    pub(crate) fn into_parts(self) -> (ShellArguments, PreparedShellWorkdir) {
        (self.arguments, self.workdir)
    }
}

pub(crate) type FinishShellSetup = Box<
    dyn FnOnce(
            ToolDispatchBinding,
            PreparedShellInvocation,
        ) -> Result<PreparedToolAction, ToolExecutorError>
        + Send
        + 'static,
>;

/// Parse promptly and seal the first stage. Filesystem work begins only when
/// the Agent resolves the returned action setup.
pub(crate) fn prepare_action_setup(
    request: ToolExecutionRequest,
    workspace: Arc<Workspace>,
    finish: FinishShellSetup,
) -> Result<ToolPreparation, ToolExecutorError> {
    let arguments = match parse_arguments(request.arguments().as_value()) {
        Ok(arguments) => arguments,
        Err(error) => {
            return invalid_arguments_result(error).map(ToolPreparation::Complete);
        }
    };
    let setup_dispatch = request.dispatch_binding().clone();
    let action_dispatch = setup_dispatch.clone();
    let setup = PreparedToolActionSetup::new(
        setup_dispatch,
        Box::new(move |control| {
            Box::pin(resolve_action_setup(
                control,
                workspace,
                arguments,
                action_dispatch,
                finish,
            ))
        }),
    )?;
    Ok(ToolPreparation::Action(setup))
}

struct SetupFailure {
    error: ToolCallError,
    timeout_ms: u64,
    workdir: Option<String>,
}

impl std::fmt::Debug for SetupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SetupFailure")
            .field("error", &self.error)
            .field("timeout_ms", &self.timeout_ms)
            .field(
                "workdir_bytes",
                &self.workdir.as_ref().map_or(0, String::len),
            )
            .finish()
    }
}

impl SetupFailure {
    fn settings(&self) -> (u64, Option<&str>) {
        (self.timeout_ms, self.workdir.as_deref())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SetupStop {
    Caller,
    Turn,
    PreparationTimeout,
}

async fn resolve_action_setup(
    control: ToolActionSetupControl,
    workspace: Arc<Workspace>,
    arguments: ShellArguments,
    action_dispatch: ToolDispatchBinding,
    finish: FinishShellSetup,
) -> ToolActionSetupOutcome {
    let outer_cancellation = control.cancellation().clone();
    let worker_cancellation = outer_cancellation.child_token();
    let worker_token = worker_cancellation.clone();
    let turn_deadline = control.turn_deadline();
    let preparation_deadline = control.preparation_deadline();
    let timeout_ms = arguments.timeout_ms();
    let mut worker = tokio::task::spawn_blocking(move || {
        let resolved = workspace
            .resolve_shell_workdir(arguments.workdir())
            .map_err(|error| SetupFailure {
                error,
                timeout_ms,
                workdir: None,
            })?;
        let display = resolved.display.clone();
        let workdir = workspace
            .prepare_shell_workdir(resolved, &worker_token)
            .map_err(|error| SetupFailure {
                error,
                timeout_ms,
                workdir: Some(display),
            })?;
        Ok::<PreparedShellInvocation, SetupFailure>(PreparedShellInvocation { arguments, workdir })
    });

    let mut stop = None;
    let mut turn_stop = ToolActionTurnStop::None;
    let mut caller_seen = false;
    let mut turn_seen = false;
    let mut preparation_seen = false;
    let joined = loop {
        tokio::select! {
            biased;
            _ = outer_cancellation.cancelled(), if !caller_seen => {
                caller_seen = true;
                if stop.is_none() {
                    stop = Some(SetupStop::Caller);
                }
                if turn_stop == ToolActionTurnStop::None {
                    turn_stop = ToolActionTurnStop::CallerCancelled;
                }
                worker_cancellation.cancel();
            }
            _ = tokio::time::sleep_until(turn_deadline), if !turn_seen => {
                turn_seen = true;
                if stop.is_none() {
                    stop = Some(SetupStop::Turn);
                }
                if turn_stop == ToolActionTurnStop::None {
                    turn_stop = ToolActionTurnStop::TurnTimeout;
                }
                worker_cancellation.cancel();
            }
            _ = tokio::time::sleep_until(preparation_deadline), if !preparation_seen => {
                preparation_seen = true;
                if stop.is_none() {
                    stop = Some(SetupStop::PreparationTimeout);
                }
                worker_cancellation.cancel();
            }
            joined = &mut worker => break joined,
        }
    };

    // A ready JoinHandle does not freeze the stop snapshot. Sample all sources
    // once more in the documented priority before classifying its payload.
    let now = tokio::time::Instant::now();
    if outer_cancellation.is_cancelled() && !caller_seen {
        if stop.is_none() {
            stop = Some(SetupStop::Caller);
        }
        if turn_stop == ToolActionTurnStop::None {
            turn_stop = ToolActionTurnStop::CallerCancelled;
        }
    }
    if now >= turn_deadline && !turn_seen {
        if stop.is_none() {
            stop = Some(SetupStop::Turn);
        }
        if turn_stop == ToolActionTurnStop::None {
            turn_stop = ToolActionTurnStop::TurnTimeout;
        }
    }
    if now >= preparation_deadline && !preparation_seen && stop.is_none() {
        stop = Some(SetupStop::PreparationTimeout);
    }

    let joined = match joined {
        Ok(joined) => joined,
        Err(_) => return ToolActionSetupOutcome::Infrastructure { turn_stop },
    };
    if let Some(stop) = stop {
        let (timeout_ms, workdir) = match &joined {
            Ok(prepared) => (
                prepared.arguments().timeout_ms(),
                Some(prepared.workdir().display()),
            ),
            Err(failure) => failure.settings(),
        };
        let result = match pre_dispatch_stop_result(
            stop == SetupStop::PreparationTimeout,
            timeout_ms,
            workdir,
        ) {
            Ok(result) => result,
            Err(_) => return ToolActionSetupOutcome::Infrastructure { turn_stop },
        };
        return ToolActionSetupOutcome::NotStarted { turn_stop, result };
    }

    let prepared = match joined {
        Ok(prepared) => prepared,
        Err(failure) => {
            let timeout_ms = failure.timeout_ms;
            let workdir = failure.workdir.clone();
            let (name, code, message) = match failure.error.into_model_parts() {
                Ok(parts) => parts,
                Err(_) => return ToolActionSetupOutcome::Infrastructure { turn_stop },
            };
            let result = match shell_error_result(
                name,
                code,
                &message,
                Some(timeout_ms),
                workdir.as_deref(),
            ) {
                Ok(result) => result,
                Err(_) => return ToolActionSetupOutcome::Infrastructure { turn_stop },
            };
            return ToolActionSetupOutcome::NotStarted { turn_stop, result };
        }
    };
    match finish(action_dispatch, prepared) {
        Ok(action) => ToolActionSetupOutcome::Ready(action),
        Err(_) => ToolActionSetupOutcome::Infrastructure { turn_stop },
    }
}

pub(crate) fn finish_action(
    dispatch: ToolDispatchBinding,
    invocation: PreparedShellInvocation,
    environment: Arc<[(OsString, OsString)]>,
    runner: Arc<ProcessRunner>,
) -> Result<PreparedToolAction, ToolExecutorError> {
    let prompt = approval_prompt(invocation.arguments(), invocation.workdir().display())?;
    let timeout_ms = invocation.arguments().timeout_ms();
    let decline_workdir = invocation.workdir().display().to_owned();
    let run_workdir = decline_workdir.clone();
    PreparedToolAction::new(
        dispatch,
        prompt,
        MAX_SHELL_RESULT_EVENT_BYTES,
        Box::new(move |reason| declined_result(reason, timeout_ms, &decline_workdir)),
        Box::new(move |control| {
            Box::pin(run_prepared_action(
                invocation,
                environment,
                runner,
                run_workdir,
                control,
            ))
        }),
    )
}

struct LaunchReady {
    workdir: std::os::fd::OwnedFd,
    permit: ProcessLaunchPermit,
}

enum LaunchCheckFailure {
    Workdir(ToolCallError),
    Process(ProcessPrecheckError),
}

impl std::fmt::Debug for LaunchCheckFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Workdir(error) => formatter.debug_tuple("Workdir").field(error).finish(),
            Self::Process(error) => formatter.debug_tuple("Process").field(error).finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ActionStop {
    Caller,
    Turn,
    ActionTimeout,
}

async fn run_prepared_action(
    invocation: PreparedShellInvocation,
    environment: Arc<[(OsString, OsString)]>,
    runner: Arc<ProcessRunner>,
    workdir_display: String,
    control: crate::agent::ToolActionControl,
) -> crate::agent::ToolActionOutcome {
    let (arguments, workdir) = invocation.into_parts();
    let timeout_ms = arguments.timeout_ms();
    let command_timeout = arguments.timeout();
    let outer_cancellation = control.cancellation().clone();
    let worker_cancellation = outer_cancellation.child_token();
    let worker_token = worker_cancellation.clone();
    let check_runner = Arc::clone(&runner);
    let turn_deadline = control.turn_deadline();
    let action_deadline = control.action_deadline();
    let mut worker = tokio::task::spawn_blocking(move || {
        // Both checks share this one owned job, so neither can remain detached.
        // Reopen the directory first and make the host prerequisite the final
        // blocking check, minimizing its check-to-spawn window.
        let workdir = workdir
            .revalidate(&worker_token)
            .map_err(LaunchCheckFailure::Workdir)?;
        let permit = check_runner
            .pre_spawn_check(&worker_token)
            .map_err(LaunchCheckFailure::Process)?;
        Ok::<LaunchReady, LaunchCheckFailure>(LaunchReady { workdir, permit })
    });

    let mut stop = None;
    let mut turn_stop = ToolActionTurnStop::None;
    let mut caller_seen = false;
    let mut turn_seen = false;
    let mut action_seen = false;
    let joined = loop {
        tokio::select! {
            biased;
            _ = outer_cancellation.cancelled(), if !caller_seen => {
                caller_seen = true;
                if stop.is_none() {
                    stop = Some(ActionStop::Caller);
                }
                if turn_stop == ToolActionTurnStop::None {
                    turn_stop = ToolActionTurnStop::CallerCancelled;
                }
                worker_cancellation.cancel();
            }
            _ = tokio::time::sleep_until(turn_deadline), if !turn_seen => {
                turn_seen = true;
                if stop.is_none() {
                    stop = Some(ActionStop::Turn);
                }
                if turn_stop == ToolActionTurnStop::None {
                    turn_stop = ToolActionTurnStop::TurnTimeout;
                }
                worker_cancellation.cancel();
            }
            _ = tokio::time::sleep_until(action_deadline), if !action_seen => {
                action_seen = true;
                if stop.is_none() {
                    stop = Some(ActionStop::ActionTimeout);
                }
                worker_cancellation.cancel();
            }
            joined = &mut worker => break joined,
        }
    };

    let now = tokio::time::Instant::now();
    if outer_cancellation.is_cancelled() && !caller_seen {
        if stop.is_none() {
            stop = Some(ActionStop::Caller);
        }
        if turn_stop == ToolActionTurnStop::None {
            turn_stop = ToolActionTurnStop::CallerCancelled;
        }
    }
    if now >= turn_deadline && !turn_seen {
        if stop.is_none() {
            stop = Some(ActionStop::Turn);
        }
        if turn_stop == ToolActionTurnStop::None {
            turn_stop = ToolActionTurnStop::TurnTimeout;
        }
    }
    if now >= action_deadline && !action_seen && stop.is_none() {
        stop = Some(ActionStop::ActionTimeout);
    }

    let ready = match joined {
        Err(_) => return crate::agent::ToolActionOutcome::Infrastructure { turn_stop },
        Ok(ready) => ready,
    };
    if let Some(stop) = stop {
        let result = match pre_spawn_stop_result(stop, timeout_ms, &workdir_display) {
            Ok(result) => result,
            Err(_) => return crate::agent::ToolActionOutcome::Infrastructure { turn_stop },
        };
        return crate::agent::ToolActionOutcome::NotStarted { turn_stop, result };
    }
    let ready = match ready {
        Ok(ready) => ready,
        Err(failure) => {
            let result = match launch_check_failure_result(failure, timeout_ms, &workdir_display) {
                Ok(result) => result,
                Err(_) => return crate::agent::ToolActionOutcome::Infrastructure { turn_stop },
            };
            return crate::agent::ToolActionOutcome::NotStarted { turn_stop, result };
        }
    };
    let request = match ProcessRequest::new(
        arguments.into_command(),
        ready.workdir,
        environment,
        command_timeout,
        ready.permit,
    ) {
        Ok(request) => request,
        Err(_) => return crate::agent::ToolActionOutcome::Infrastructure { turn_stop },
    };
    let process_control = ProcessControl::new(outer_cancellation, turn_deadline, action_deadline);
    map_process_outcome(
        runner.run(request, process_control).await,
        timeout_ms,
        workdir_display,
    )
}

fn pre_spawn_stop_result(
    stop: ActionStop,
    timeout_ms: u64,
    workdir: &str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    if stop == ActionStop::ActionTimeout {
        shell_error_result(
            "ToolTimeoutError",
            "TOOL_TIMEOUT",
            "shell execution exceeded the Agent action time limit before process creation",
            Some(timeout_ms),
            Some(workdir),
        )
    } else {
        shell_error_result(
            "AbortError",
            "ABORTED_BEFORE_DISPATCH",
            "shell execution stopped before process creation",
            Some(timeout_ms),
            Some(workdir),
        )
    }
}

fn launch_check_failure_result(
    failure: LaunchCheckFailure,
    timeout_ms: u64,
    workdir: &str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    match failure {
        LaunchCheckFailure::Workdir(error) => {
            let (name, code, message) = error.into_model_parts()?;
            shell_error_result(name, code, &message, Some(timeout_ms), Some(workdir))
        }
        LaunchCheckFailure::Process(ProcessPrecheckError::Cancelled) => shell_error_result(
            "AbortError",
            "ABORTED_BEFORE_DISPATCH",
            "shell execution stopped before process creation",
            Some(timeout_ms),
            Some(workdir),
        ),
        LaunchCheckFailure::Process(ProcessPrecheckError::ObserverUnavailable) => {
            shell_error_result(
                "ShellProcessError",
                "SHELL_PROCESS_OBSERVER_UNAVAILABLE",
                "the foreground process observer became unavailable before spawn",
                Some(timeout_ms),
                Some(workdir),
            )
        }
    }
}

fn map_process_outcome(
    outcome: ProcessOutcome,
    timeout_ms: u64,
    workdir: String,
) -> crate::agent::ToolActionOutcome {
    match outcome {
        ProcessOutcome::NotStarted { turn_stop, cause } => {
            let turn_stop = map_turn_stop(turn_stop);
            let result = match process_start_failure_result(cause, timeout_ms, &workdir) {
                Ok(result) => result,
                Err(_) => return crate::agent::ToolActionOutcome::Infrastructure { turn_stop },
            };
            crate::agent::ToolActionOutcome::NotStarted { turn_stop, result }
        }
        ProcessOutcome::StartedAndQuiescent(report) => {
            let turn_stop = map_turn_stop(report.turn_stop());
            // ProcessReport is crate-private and validates its termination fact;
            // the renderer separately proves the 64 KiB ContentBlock bound.
            let result = process_report_result(report, timeout_ms, workdir)
                .expect("bounded process facts always form a valid shell result");
            crate::agent::ToolActionOutcome::StartedAndQuiescent { turn_stop, result }
        }
        ProcessOutcome::StartedOwnershipLost { turn_stop } => {
            crate::agent::ToolActionOutcome::StartedOwnershipLost {
                turn_stop: map_turn_stop(turn_stop),
            }
        }
    }
}

fn process_start_failure_result(
    cause: ProcessStartFailure,
    timeout_ms: u64,
    workdir: &str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let (name, code, message) = match cause {
        ProcessStartFailure::CallerCancelled | ProcessStartFailure::TurnTimeout => (
            "AbortError",
            "ABORTED_BEFORE_DISPATCH",
            "shell execution stopped before process creation",
        ),
        ProcessStartFailure::ActionTimeout => (
            "ToolTimeoutError",
            "TOOL_TIMEOUT",
            "shell execution exceeded the Agent action time limit before process creation",
        ),
        ProcessStartFailure::ObserverUnavailable => (
            "ShellProcessError",
            "SHELL_PROCESS_OBSERVER_UNAVAILABLE",
            "the foreground process observer became unavailable before spawn",
        ),
        ProcessStartFailure::AsyncRuntimeUnavailable => (
            "ShellProcessError",
            "SHELL_ASYNC_RUNTIME_UNAVAILABLE",
            "the foreground shell action requires a Tokio I/O runtime",
        ),
        ProcessStartFailure::PipePreflightFailed => (
            "ShellProcessError",
            "SHELL_PIPE_PREFLIGHT_FAILED",
            "the foreground output-pipe preflight failed",
        ),
        ProcessStartFailure::SpawnFailed => (
            "ShellProcessError",
            "SHELL_SPAWN_FAILED",
            "the Bash process could not be started",
        ),
    };
    shell_error_result(name, code, message, Some(timeout_ms), Some(workdir))
}

fn process_report_result(
    report: ProcessReport,
    timeout_ms: u64,
    workdir: String,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let termination = match report.termination() {
        ProcessTermination::ExitCode(code) => ShellTermination::ExitCode(code),
        termination @ ProcessTermination::Signal(_) => ShellTermination::Signal(
            termination
                .canonical_signal_name()
                .ok_or_else(|| ToolExecutorError::new("invalid process signal"))?,
        ),
    };
    let flags = report.flags();
    started_result(StartedShellResult {
        stdout: report.stdout().bytes().to_vec(),
        stderr: report.stderr().bytes().to_vec(),
        termination,
        primary: map_primary(report.primary()),
        output_limit_exceeded: flags.output_limit_exceeded(),
        pipe_setup_failed: flags.pipe_setup_failed(),
        pipe_read_failed: flags.pipe_read_failed(),
        signal_delivery_failed: flags.signal_delivery_failed(),
        pipe_drain_timed_out: flags.pipe_drain_timed_out(),
        timeout_ms,
        workdir,
        stdout_truncated: report.stdout().truncated(),
        stderr_truncated: report.stderr().truncated(),
    })
}

fn map_primary(primary: ProcessPrimaryCause) -> ShellPrimary {
    match primary {
        ProcessPrimaryCause::Natural => ShellPrimary::Natural,
        ProcessPrimaryCause::CallerCancelled => ShellPrimary::CallerCancelled,
        ProcessPrimaryCause::TurnTimeout => ShellPrimary::TurnTimeout,
        ProcessPrimaryCause::ActionTimeout => ShellPrimary::ActionTimeout,
        ProcessPrimaryCause::CommandTimeout => ShellPrimary::CommandTimeout,
        ProcessPrimaryCause::PipeSetupFailed => ShellPrimary::PipeSetupFailed,
        ProcessPrimaryCause::PipeReadFailed => ShellPrimary::PipeReadFailed,
        ProcessPrimaryCause::OutputLimit => ShellPrimary::OutputLimit,
        ProcessPrimaryCause::PipeDrainTimeout => ShellPrimary::PipeDrainTimeout,
        ProcessPrimaryCause::BackgroundNotSupported => ShellPrimary::BackgroundNotSupported,
    }
}

fn map_turn_stop(stop: ProcessTurnStop) -> ToolActionTurnStop {
    match stop {
        ProcessTurnStop::None => ToolActionTurnStop::None,
        ProcessTurnStop::CallerCancelled => ToolActionTurnStop::CallerCancelled,
        ProcessTurnStop::TurnTimeout => ToolActionTurnStop::TurnTimeout,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ShellPrimary {
    Natural,
    CallerCancelled,
    TurnTimeout,
    ActionTimeout,
    CommandTimeout,
    PipeSetupFailed,
    PipeReadFailed,
    OutputLimit,
    PipeDrainTimeout,
    BackgroundNotSupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ShellTermination {
    ExitCode(i32),
    Signal(String),
}

pub(crate) struct StartedShellResult {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) termination: ShellTermination,
    pub(crate) primary: ShellPrimary,
    pub(crate) output_limit_exceeded: bool,
    pub(crate) pipe_setup_failed: bool,
    pub(crate) pipe_read_failed: bool,
    pub(crate) signal_delivery_failed: bool,
    pub(crate) pipe_drain_timed_out: bool,
    pub(crate) timeout_ms: u64,
    pub(crate) workdir: String,
    pub(crate) stdout_truncated: bool,
    pub(crate) stderr_truncated: bool,
}

impl std::fmt::Debug for StartedShellResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StartedShellResult")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr.len())
            .field("termination", &self.termination)
            .field("primary", &self.primary)
            .field("timeout_ms", &self.timeout_ms)
            .field("workdir_bytes", &self.workdir.len())
            .field("stdout_truncated", &self.stdout_truncated)
            .field("stderr_truncated", &self.stderr_truncated)
            .finish_non_exhaustive()
    }
}

pub(crate) fn started_result(
    mut facts: StartedShellResult,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    if facts.pipe_setup_failed {
        // No pipe was reliably monitored after process creation.
        facts.stdout_truncated = true;
        facts.stderr_truncated = true;
    }
    let stdout = String::from_utf8_lossy(&facts.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&facts.stderr).into_owned();
    let primary_marker = primary_marker(facts.primary);
    let (text, stdout_truncated, stderr_truncated) = render_bounded_text(
        &stdout,
        &stderr,
        facts.stdout_truncated,
        facts.stderr_truncated,
        facts.pipe_setup_failed,
        facts.pipe_read_failed,
        facts.signal_delivery_failed,
        facts.pipe_drain_timed_out,
        facts.output_limit_exceeded && facts.primary != ShellPrimary::OutputLimit,
        facts.primary == ShellPrimary::CommandTimeout,
        facts.timeout_ms,
        primary_marker,
        &facts.termination,
    )?;
    let (exit_code, signal) = match &facts.termination {
        ShellTermination::ExitCode(code) => (Some(*code), None),
        ShellTermination::Signal(signal) => (None, Some(signal.as_str())),
    };
    let timed_out = facts.primary == ShellPrimary::CommandTimeout;
    let aborted = matches!(
        facts.primary,
        ShellPrimary::CallerCancelled | ShellPrimary::TurnTimeout | ShellPrimary::ActionTimeout
    );
    let meta = JsonValue::new(json!({
        "kind": "foreground",
        "started": true,
        "exitCode": exit_code,
        "signal": signal,
        "timedOut": timed_out,
        "aborted": aborted,
        "outputLimitExceeded": facts.output_limit_exceeded,
        "pipeSetupFailed": facts.pipe_setup_failed,
        "pipeReadFailed": facts.pipe_read_failed,
        "signalDeliveryFailed": facts.signal_delivery_failed,
        "pipeDrainTimedOut": facts.pipe_drain_timed_out,
        "timeoutMs": facts.timeout_ms,
        "workdir": facts.workdir,
        "stdoutTruncated": stdout_truncated,
        "stderrTruncated": stderr_truncated
    }))
    .map_err(|_| ToolExecutorError::new("shell result metadata normalization failed"))?;
    let content = ContentBlock::text(text)
        .map_err(|_| ToolExecutorError::new("shell output normalization failed"))?;
    if content.raw().encoded_len() > MAX_TOOL_CONTENT_BYTES {
        return Err(ToolExecutorError::new(
            "shell output exceeded its normalized bound",
        ));
    }
    let failure = primary_failure(facts.primary).map(|(name, code)| ToolFailure {
        name: name.to_owned(),
        code: code.to_owned(),
    });
    ToolExecutionResult::new(vec![content], failure.is_some(), failure, Some(meta), false)
        .map_err(|_| ToolExecutorError::new("shell result normalization failed"))
}

fn primary_failure(primary: ShellPrimary) -> Option<(&'static str, &'static str)> {
    match primary {
        ShellPrimary::Natural | ShellPrimary::CommandTimeout => None,
        ShellPrimary::CallerCancelled => Some(("AbortError", "ABORTED")),
        ShellPrimary::TurnTimeout => Some(("AgentTimeoutError", "AGENT_TURN_TIMEOUT")),
        ShellPrimary::ActionTimeout => Some(("ToolTimeoutError", "TOOL_TIMEOUT")),
        ShellPrimary::PipeSetupFailed => Some(("ShellError", "SHELL_PIPE_SETUP_FAILED")),
        ShellPrimary::PipeReadFailed => Some(("ShellError", "SHELL_PIPE_READ_FAILED")),
        ShellPrimary::OutputLimit => Some(("ShellError", "SHELL_OUTPUT_LIMIT")),
        ShellPrimary::PipeDrainTimeout => Some(("ShellError", "SHELL_PIPE_DRAIN_TIMEOUT")),
        ShellPrimary::BackgroundNotSupported => {
            Some(("ShellError", "BACKGROUND_PROCESS_NOT_SUPPORTED"))
        }
    }
}

fn primary_marker(primary: ShellPrimary) -> Option<&'static str> {
    match primary {
        ShellPrimary::PipeSetupFailed => Some("[output pipe setup failed; process group stopped]"),
        ShellPrimary::PipeReadFailed => Some("[pipe read failed; output is incomplete]"),
        ShellPrimary::OutputLimit => Some("[output limit exceeded; process group stopped]"),
        ShellPrimary::BackgroundNotSupported => {
            Some("[background process is not supported; process group stopped]")
        }
        ShellPrimary::Natural
        | ShellPrimary::CallerCancelled
        | ShellPrimary::TurnTimeout
        | ShellPrimary::ActionTimeout
        | ShellPrimary::CommandTimeout
        | ShellPrimary::PipeDrainTimeout => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_bounded_text(
    stdout: &str,
    stderr: &str,
    raw_stdout_truncated: bool,
    raw_stderr_truncated: bool,
    pipe_setup_failed: bool,
    pipe_read_failed: bool,
    signal_delivery_failed: bool,
    pipe_drain_timed_out: bool,
    secondary_output_limit: bool,
    timed_out: bool,
    timeout_ms: u64,
    primary_marker: Option<&str>,
    termination: &ShellTermination,
) -> Result<(String, bool, bool), ToolExecutorError> {
    let full = assemble_rendered_text(
        stdout,
        stderr,
        raw_stdout_truncated,
        raw_stderr_truncated,
        pipe_setup_failed,
        pipe_read_failed,
        signal_delivery_failed,
        pipe_drain_timed_out,
        secondary_output_limit,
        timed_out,
        timeout_ms,
        primary_marker,
        termination,
        !stdout.is_empty() || !stderr.is_empty(),
    );
    if text_block_encoded_bytes(json_string_content_bytes(&full)) <= MAX_TOOL_CONTENT_BYTES {
        return Ok((full, raw_stdout_truncated, raw_stderr_truncated));
    }

    let assumed_stdout_truncated = raw_stdout_truncated || !stdout.is_empty();
    let assumed_stderr_truncated = raw_stderr_truncated || !stderr.is_empty();
    let fixed = assemble_rendered_text(
        "",
        "",
        assumed_stdout_truncated,
        assumed_stderr_truncated,
        pipe_setup_failed,
        pipe_read_failed,
        signal_delivery_failed,
        pipe_drain_timed_out,
        secondary_output_limit,
        timed_out,
        timeout_ms,
        primary_marker,
        termination,
        !stdout.is_empty() || !stderr.is_empty(),
    );
    let fixed_bytes = text_block_encoded_bytes(json_string_content_bytes(&fixed));
    if fixed_bytes > MAX_TOOL_CONTENT_BYTES {
        return Err(ToolExecutorError::new(
            "fixed shell result markers exceed the output bound",
        ));
    }

    // Reserve four encoded newlines for stream/notice boundaries. Selection is
    // by compact-JSON cost, not visible UTF-8 bytes, so escapes cannot overflow.
    let mut available = MAX_TOOL_CONTENT_BYTES
        .saturating_sub(fixed_bytes)
        .saturating_sub(8);
    loop {
        let (selected_stdout, stdout_omitted, selected_stderr, stderr_omitted) =
            select_stream_suffixes(stdout, stderr, available);
        let stdout_truncated = raw_stdout_truncated || stdout_omitted;
        let stderr_truncated = raw_stderr_truncated || stderr_omitted;
        let rendered = assemble_rendered_text(
            &selected_stdout,
            &selected_stderr,
            stdout_truncated,
            stderr_truncated,
            pipe_setup_failed,
            pipe_read_failed,
            signal_delivery_failed,
            pipe_drain_timed_out,
            secondary_output_limit,
            timed_out,
            timeout_ms,
            primary_marker,
            termination,
            !stdout.is_empty() || !stderr.is_empty(),
        );
        let encoded = text_block_encoded_bytes(json_string_content_bytes(&rendered));
        if encoded <= MAX_TOOL_CONTENT_BYTES {
            return Ok((rendered, stdout_truncated, stderr_truncated));
        }
        let over = encoded.saturating_sub(MAX_TOOL_CONTENT_BYTES).max(1);
        let next = available.saturating_sub(over);
        if next == available {
            return Err(ToolExecutorError::new(
                "shell output renderer did not converge",
            ));
        }
        available = next;
    }
}

fn select_stream_suffixes(
    stdout: &str,
    stderr: &str,
    available: usize,
) -> (String, bool, String, bool) {
    if stdout.is_empty() {
        let (selected, omitted, _) = suffix_with_encoded_budget(stderr, available);
        return (String::new(), false, selected, omitted);
    }
    if stderr.is_empty() {
        let (selected, omitted, _) = suffix_with_encoded_budget(stdout, available);
        return (selected, omitted, String::new(), false);
    }

    let stdout_budget = available / 2;
    let stderr_budget = available.saturating_sub(stdout_budget);
    let (mut selected_stdout, mut stdout_omitted, stdout_used) =
        suffix_with_encoded_budget(stdout, stdout_budget);
    let (mut selected_stderr, mut stderr_omitted, stderr_used) =
        suffix_with_encoded_budget(stderr, stderr_budget);

    let stdout_unused = stdout_budget.saturating_sub(stdout_used);
    if stdout_unused > 0 {
        let selected = suffix_with_encoded_budget(stderr, stderr_budget + stdout_unused);
        selected_stderr = selected.0;
        stderr_omitted = selected.1;
    }
    let stderr_unused = stderr_budget.saturating_sub(stderr_used);
    if stderr_unused > 0 {
        let selected = suffix_with_encoded_budget(stdout, stdout_budget + stderr_unused);
        selected_stdout = selected.0;
        stdout_omitted = selected.1;
    }
    (
        selected_stdout,
        stdout_omitted,
        selected_stderr,
        stderr_omitted,
    )
}

fn suffix_with_encoded_budget(value: &str, budget: usize) -> (String, bool, usize) {
    let mut used = 0_usize;
    let mut start = value.len();
    for (index, character) in value.char_indices().rev() {
        let cost = json_character_content_bytes(character);
        let Some(next) = used.checked_add(cost) else {
            break;
        };
        if next > budget {
            break;
        }
        used = next;
        start = index;
    }
    (value[start..].to_owned(), start != 0, used)
}

fn json_character_content_bytes(character: char) -> usize {
    match character {
        '"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
        '\u{0000}'..='\u{001f}' => 6,
        other => other.len_utf8(),
    }
}

#[allow(clippy::too_many_arguments)]
fn assemble_rendered_text(
    stdout: &str,
    stderr: &str,
    stdout_truncated: bool,
    stderr_truncated: bool,
    pipe_setup_failed: bool,
    pipe_read_failed: bool,
    signal_delivery_failed: bool,
    pipe_drain_timed_out: bool,
    secondary_output_limit: bool,
    timed_out: bool,
    timeout_ms: u64,
    primary_marker: Option<&str>,
    termination: &ShellTermination,
    had_stream_output: bool,
) -> String {
    let mut output = String::new();
    output.push_str(stdout);
    if stdout_truncated {
        append_line(&mut output, "[stdout truncated; tail only]");
    }
    if !stderr.is_empty() || stderr_truncated {
        append_line(&mut output, "[stderr]");
        output.push('\n');
        output.push_str(stderr);
        if stderr_truncated {
            append_line(&mut output, "[stderr truncated; tail only]");
        }
    }
    if output.is_empty() && !had_stream_output {
        output.push_str("(no output)");
    }
    if pipe_setup_failed {
        append_line(
            &mut output,
            "[warning: output pipes could not be monitored; output is incomplete]",
        );
    }
    if pipe_read_failed {
        append_line(
            &mut output,
            "[warning: a pipe read failed; output is incomplete]",
        );
    }
    if signal_delivery_failed {
        append_line(&mut output, "[warning: a process-group signal failed]");
    }
    if pipe_drain_timed_out {
        append_line(
            &mut output,
            "[warning: output pipe remained open; an escaped process may still be running]",
        );
    }
    if secondary_output_limit {
        append_line(
            &mut output,
            "[output limit exceeded; process group stopped]",
        );
    }
    if let Some(marker) = primary_marker {
        append_line(&mut output, marker);
    }
    if timed_out {
        append_line(&mut output, &format!("[timed out after {timeout_ms}ms]"));
    }
    match termination {
        ShellTermination::ExitCode(0) => {}
        ShellTermination::ExitCode(code) => {
            append_line(&mut output, &format!("[exit code: {code}]"));
        }
        ShellTermination::Signal(signal) => {
            append_line(&mut output, &format!("[killed by signal: {signal}]"));
        }
    }
    output
}

fn append_line(output: &mut String, line: &str) {
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(line);
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, ffi::OsString, os::unix::ffi::OsStringExt};

    use serde_json::json;

    use super::{
        DEFAULT_SHELL_TIMEOUT_MS, MAX_CHILD_ENVIRONMENT_BYTES, MAX_SHELL_COMMAND_BYTES,
        MAX_SHELL_DESCRIPTION_BYTES, MAX_SHELL_TIMEOUT_MS, MAX_SHELL_WORKDIR_BYTES,
        ShellEnvironment, ShellPrimary, ShellTermination, StartedShellResult, approval_prompt,
        build_environment, parse_arguments, schema, started_result,
    };

    fn result_with(
        stdout: impl Into<Vec<u8>>,
        stderr: impl Into<Vec<u8>>,
        termination: ShellTermination,
        primary: ShellPrimary,
    ) -> StartedShellResult {
        StartedShellResult {
            stdout: stdout.into(),
            stderr: stderr.into(),
            termination,
            primary,
            output_limit_exceeded: false,
            pipe_setup_failed: false,
            pipe_read_failed: false,
            signal_delivery_failed: false,
            pipe_drain_timed_out: false,
            timeout_ms: 25_000,
            workdir: ".".to_owned(),
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }

    fn result_text(result: &crate::agent::ToolExecutionResult) -> &str {
        result.content()[0].raw().as_value()["text"]
            .as_str()
            .unwrap()
    }

    #[test]
    fn shell_schema_and_runtime_parser_are_closed() {
        let parameters = schema().unwrap().parameters().as_value().clone();
        assert_eq!(parameters["required"], json!(["command", "description"]));
        assert_eq!(parameters["additionalProperties"], false);

        let parsed = parse_arguments(&json!({
            "command": "printf ok",
            "description": "print one value"
        }))
        .unwrap();
        assert_eq!(parsed.timeout_ms(), DEFAULT_SHELL_TIMEOUT_MS);
        assert_eq!(parsed.workdir(), ".");
        let prompt = approval_prompt(&parsed, ".").unwrap();
        assert!(prompt.preview().contains("PATH, HOME"));
        assert!(prompt.preview().contains("NO_COLOR, TERM, PAGER"));

        for invalid in [
            json!({"command": "printf bad"}),
            json!({"command": "printf bad", "description": null}),
            json!({"command": "printf bad", "description": "bad", "timeoutMs": null}),
            json!({"command": "printf bad", "description": "bad", "timeoutMs": 1.5}),
            json!({"command": "printf bad", "description": "bad", "background": true}),
            json!({"command": "  ", "description": "bad"}),
            json!({"command": "printf bad", "description": "bad\nline"}),
        ] {
            assert!(parse_arguments(&invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn shell_argument_byte_and_timeout_boundaries_are_exact() {
        let valid = json!({
            "command": "x".repeat(MAX_SHELL_COMMAND_BYTES),
            "description": "d".repeat(MAX_SHELL_DESCRIPTION_BYTES),
            "timeoutMs": MAX_SHELL_TIMEOUT_MS,
            "workdir": "w".repeat(MAX_SHELL_WORKDIR_BYTES)
        });
        assert!(parse_arguments(&valid).is_ok());
        assert!(
            parse_arguments(&json!({
                "command": "x".repeat(MAX_SHELL_COMMAND_BYTES + 1),
                "description": "d"
            }))
            .is_err()
        );
        assert!(
            parse_arguments(&json!({
                "command": "x",
                "description": "界".repeat(342)
            }))
            .is_err()
        );
        for timeout in [0, MAX_SHELL_TIMEOUT_MS + 1] {
            assert!(
                parse_arguments(&json!({
                    "command": "x",
                    "description": "d",
                    "timeoutMs": timeout
                }))
                .is_err()
            );
        }
    }

    #[test]
    fn child_environment_is_fixed_bounded_and_debug_redacted() {
        let source = BTreeMap::from([
            ("PATH", OsString::from("/conspicuous/path")),
            ("HOME", OsString::from("/conspicuous/home")),
            ("BASH_ENV", OsString::from("/must/not/copy")),
        ]);
        let environment = build_environment(|name| source.get(name).cloned()).unwrap();
        let values = environment
            .entries
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        assert_eq!(values["PATH"], "/conspicuous/path");
        assert_eq!(values["NO_COLOR"], "1");
        assert_eq!(values["TERM"], "dumb");
        assert!(!values.contains_key("BASH_ENV"));
        let debug = format!("{environment:?}");
        assert!(!debug.contains("conspicuous"));
    }

    #[test]
    fn missing_path_has_a_fixed_fallback_and_oversize_is_redacted() {
        let fallback = build_environment(|_| None).unwrap();
        assert!(
            fallback
                .entries
                .iter()
                .any(|(name, value)| { name == "PATH" && value == "/usr/bin:/bin" })
        );

        let error = build_environment(|name| {
            (name == "HOME").then(|| OsString::from("x".repeat(MAX_CHILD_ENVIRONMENT_BYTES)))
        })
        .unwrap_err();
        assert!(matches!(
            error,
            super::ToolRegistryBuildError::EnvironmentTooLarge
        ));
        assert!(!format!("{error:?}").contains(&"x".repeat(128)));
    }

    #[test]
    fn environment_byte_and_unicode_boundaries_are_exact_and_redacted() {
        let baseline = build_environment(|_| None).unwrap().retained_bytes;
        let exact_value_bytes = MAX_CHILD_ENVIRONMENT_BYTES - baseline - "HOME".len();
        let exact = build_environment(|name| {
            (name == "HOME").then(|| OsString::from("x".repeat(exact_value_bytes)))
        })
        .unwrap();
        assert_eq!(exact.retained_bytes, MAX_CHILD_ENVIRONMENT_BYTES);

        let one_over = build_environment(|name| {
            (name == "HOME").then(|| OsString::from("x".repeat(exact_value_bytes + 1)))
        })
        .unwrap_err();
        assert!(matches!(
            one_over,
            super::ToolRegistryBuildError::EnvironmentTooLarge
        ));

        let invalid = build_environment(|name| {
            (name == "HOME").then(|| OsString::from_vec(vec![b'h', 0xff, b'i']))
        })
        .unwrap_err();
        assert!(matches!(
            invalid,
            super::ToolRegistryBuildError::InvalidEnvironment
        ));
        assert!(!format!("{invalid:?}").contains("hi"));

        let all_names = build_environment(|_| Some(OsString::from("x"))).unwrap();
        assert_eq!(all_names.entries.len(), 24);
    }

    #[test]
    fn shell_environment_clone_shares_one_immutable_snapshot() {
        let environment = ShellEnvironment::capture().unwrap();
        let first = environment.entries();
        let second = environment.entries();
        assert!(std::sync::Arc::ptr_eq(&first, &second));
        assert!(first.len() <= 24);
    }

    #[test]
    fn canonical_small_shell_results_keep_upstream_text_shape() {
        let success = started_result(result_with(
            b"hello\n".to_vec(),
            Vec::new(),
            ShellTermination::ExitCode(0),
            ShellPrimary::Natural,
        ))
        .unwrap();
        assert_eq!(result_text(&success), "hello\n");
        assert!(!success.is_error());

        let mixed = started_result(result_with(
            b"out\n".to_vec(),
            b"err\n".to_vec(),
            ShellTermination::ExitCode(0),
            ShellPrimary::Natural,
        ))
        .unwrap();
        assert_eq!(result_text(&mixed), "out\n[stderr]\nerr\n");

        let nonzero = started_result(result_with(
            b"failing\n".to_vec(),
            Vec::new(),
            ShellTermination::ExitCode(3),
            ShellPrimary::Natural,
        ))
        .unwrap();
        assert_eq!(result_text(&nonzero), "failing\n[exit code: 3]");
        assert!(!nonzero.is_error());

        let signalled = started_result(result_with(
            Vec::new(),
            Vec::new(),
            ShellTermination::Signal("SIGTERM".to_owned()),
            ShellPrimary::Natural,
        ))
        .unwrap();
        assert_eq!(
            result_text(&signalled),
            "(no output)\n[killed by signal: SIGTERM]"
        );
    }

    #[test]
    fn renderer_counts_compact_json_and_marks_each_omitted_stream() {
        let rendered = started_result(result_with(
            vec![b'a'; 40 * 1024],
            vec![b'b'; 40 * 1024],
            ShellTermination::ExitCode(0),
            ShellPrimary::Natural,
        ))
        .unwrap();
        assert!(rendered.content()[0].raw().encoded_len() <= 64 * 1024);
        assert_eq!(rendered.meta().unwrap().as_value()["stdoutTruncated"], true);
        assert_eq!(rendered.meta().unwrap().as_value()["stderrTruncated"], true);
        assert!(result_text(&rendered).contains("[stdout truncated; tail only]"));
        assert!(result_text(&rendered).contains("[stderr truncated; tail only]"));

        let mut raw_capped = result_with(
            vec![b' '; 64_000],
            Vec::new(),
            ShellTermination::ExitCode(0),
            ShellPrimary::Natural,
        );
        raw_capped.stdout_truncated = true;
        let raw_capped = started_result(raw_capped).unwrap();
        assert!(raw_capped.content()[0].raw().encoded_len() > 60 * 1024);
        assert!(raw_capped.content()[0].raw().encoded_len() <= 64 * 1024);
    }

    #[test]
    fn renderer_bounds_invalid_utf8_and_json_escape_expansion() {
        let invalid = started_result(result_with(
            vec![b'a', 0xff, b'b'],
            Vec::new(),
            ShellTermination::ExitCode(0),
            ShellPrimary::Natural,
        ))
        .unwrap();
        assert_eq!(result_text(&invalid), "a\u{fffd}b");
        assert_eq!(invalid.meta().unwrap().as_value()["stdoutTruncated"], false);

        let hostile = [b'"', b'\\', b'\n', 0x01]
            .into_iter()
            .cycle()
            .take(64_000)
            .collect::<Vec<_>>();
        let expanded = started_result(result_with(
            hostile,
            Vec::new(),
            ShellTermination::ExitCode(0),
            ShellPrimary::Natural,
        ))
        .unwrap();
        assert!(expanded.content()[0].raw().encoded_len() <= 64 * 1024);
        assert_eq!(expanded.meta().unwrap().as_value()["stdoutTruncated"], true);
        assert!(result_text(&expanded).contains("[stdout truncated; tail only]"));
    }

    #[test]
    fn secondary_process_degradations_are_machine_and_model_visible() {
        fn assert_visible(result: crate::agent::ToolExecutionResult, field: &str, marker: &str) {
            assert_eq!(result.meta().unwrap().as_value()[field], true);
            assert!(result_text(&result).contains(marker));
            assert_eq!(
                result.error().map(|failure| failure.code.as_str()),
                Some("ABORTED")
            );
        }

        let mut pipe_setup = result_with(
            Vec::new(),
            Vec::new(),
            ShellTermination::Signal("SIGTERM".to_owned()),
            ShellPrimary::CallerCancelled,
        );
        pipe_setup.pipe_setup_failed = true;
        let pipe_setup = started_result(pipe_setup).unwrap();
        assert_eq!(
            pipe_setup.meta().unwrap().as_value()["stdoutTruncated"],
            true
        );
        assert_eq!(
            pipe_setup.meta().unwrap().as_value()["stderrTruncated"],
            true
        );
        assert_visible(
            pipe_setup,
            "pipeSetupFailed",
            "[warning: output pipes could not be monitored; output is incomplete]",
        );

        let mut pipe_read = result_with(
            Vec::new(),
            b"diagnostic\n".to_vec(),
            ShellTermination::Signal("SIGTERM".to_owned()),
            ShellPrimary::CallerCancelled,
        );
        pipe_read.pipe_read_failed = true;
        pipe_read.stderr_truncated = true;
        assert_visible(
            started_result(pipe_read).unwrap(),
            "pipeReadFailed",
            "[warning: a pipe read failed; output is incomplete]",
        );

        let mut signal = result_with(
            Vec::new(),
            Vec::new(),
            ShellTermination::Signal("SIGTERM".to_owned()),
            ShellPrimary::CallerCancelled,
        );
        signal.signal_delivery_failed = true;
        assert_visible(
            started_result(signal).unwrap(),
            "signalDeliveryFailed",
            "[warning: a process-group signal failed]",
        );

        let mut drain = result_with(
            Vec::new(),
            Vec::new(),
            ShellTermination::Signal("SIGTERM".to_owned()),
            ShellPrimary::CallerCancelled,
        );
        drain.pipe_drain_timed_out = true;
        assert_visible(
            started_result(drain).unwrap(),
            "pipeDrainTimedOut",
            "[warning: output pipe remained open; an escaped process may still be running]",
        );
    }

    #[test]
    fn command_timeout_is_an_ordinary_distinct_process_fact() {
        let mut timeout = result_with(
            Vec::new(),
            Vec::new(),
            ShellTermination::ExitCode(0),
            ShellPrimary::CommandTimeout,
        );
        timeout.timeout_ms = 100;
        let timeout = started_result(timeout).unwrap();
        assert_eq!(
            result_text(&timeout),
            "(no output)\n[timed out after 100ms]"
        );
        assert_eq!(timeout.meta().unwrap().as_value()["timedOut"], true);
        assert!(!timeout.is_error());
    }

    #[test]
    fn a_secondary_output_limit_remains_visible_when_an_abort_won_first() {
        let mut aborted = result_with(
            b"tail\n".to_vec(),
            Vec::new(),
            ShellTermination::Signal("SIGTERM".to_owned()),
            ShellPrimary::CallerCancelled,
        );
        aborted.output_limit_exceeded = true;
        let aborted = started_result(aborted).unwrap();
        assert_eq!(
            result_text(&aborted),
            "tail\n[output limit exceeded; process group stopped]\n[killed by signal: SIGTERM]"
        );
        assert_eq!(
            aborted.error().map(|failure| failure.code.as_str()),
            Some("ABORTED")
        );
        assert_eq!(
            aborted.meta().unwrap().as_value()["outputLimitExceeded"],
            true
        );
    }
}
