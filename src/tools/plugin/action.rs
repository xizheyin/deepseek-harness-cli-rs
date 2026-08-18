use std::sync::Arc;

use serde_json::json;

use crate::{
    agent::{
        ActionDeclineReason, ApprovalPrompt, MAX_APPROVAL_PREVIEW_BYTES, PreparedToolAction,
        PreparedToolActionSetup, ToolActionControl, ToolActionOutcome, ToolActionSetupControl,
        ToolActionSetupOutcome, ToolActionTurnStop, ToolExecutionRequest, ToolExecutionResult,
        ToolExecutorError, ToolPreparation,
    },
    model::{ContentBlock, JsonValue},
    session::ToolFailure,
};

use super::{PluginCallControl, PluginCallOutcome, PluginHost, PluginResultPayload, PluginStop};

const MAX_PLUGIN_CONTENT_BYTES: usize = 64 * 1024;
const MAX_PLUGIN_RESULT_EVENT_BYTES: usize = 128 * 1024;

pub(crate) fn approval_required_result(
    plugin_id: &str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    not_started_error(
        plugin_id,
        "ApprovalError",
        "APPROVAL_REQUIRED",
        "plugin calls must use the Agent approval Action stage",
    )
}

pub(crate) fn prepare_action(
    request: ToolExecutionRequest,
    host: Arc<PluginHost>,
) -> Result<ToolPreparation, ToolExecutorError> {
    let plugin_id = host
        .plugin_id(request.name())
        .ok_or_else(|| ToolExecutorError::new("plugin tool binding is unavailable"))?
        .to_owned();
    if host
        .validate_arguments(request.name(), request.arguments())
        .is_err()
    {
        return not_started_error(
            &plugin_id,
            "ToolArgsError",
            "INVALID_ARGS",
            "plugin arguments do not match the declared schema",
        )
        .map(ToolPreparation::Complete);
    }
    if !host.is_available(request.name()) {
        return not_started_error(
            &plugin_id,
            "PluginUnavailableError",
            "PLUGIN_UNAVAILABLE",
            "plugin is unavailable",
        )
        .map(ToolPreparation::Complete);
    }
    let rendered_arguments = serde_json::to_string(request.arguments().as_value())
        .map_err(|_| ToolExecutorError::new("plugin arguments could not be rendered"))?;
    let preview =
        bounded_approval_preview(&plugin_id, request.name(), rendered_arguments.as_str())?;
    let reason = host.description(request.name()).map(str::to_owned);
    let prompt = ApprovalPrompt::new(reason, preview)
        .map_err(|_| ToolExecutorError::new("plugin approval prompt is invalid"))?;
    let dispatch = request.dispatch_binding().clone();
    let setup_dispatch = dispatch.clone();
    let tool_name = request.name().to_owned();
    let arguments = request.arguments().clone();
    let setup_plugin_id = plugin_id.clone();
    let setup = PreparedToolActionSetup::new_plugin(
        dispatch,
        plugin_id,
        Box::new(move |control| {
            Box::pin(async move {
                if let Some((turn_stop, code, message)) = setup_stop(&control) {
                    return match not_started_error(&setup_plugin_id, "AbortError", code, message) {
                        Ok(result) => ToolActionSetupOutcome::NotStarted { turn_stop, result },
                        Err(_) => ToolActionSetupOutcome::Infrastructure { turn_stop },
                    };
                }
                let decline_plugin_id = setup_plugin_id.clone();
                let run_plugin_id = setup_plugin_id.clone();
                let run_host = Arc::clone(&host);
                let action = PreparedToolAction::new_plugin(
                    setup_dispatch,
                    setup_plugin_id,
                    prompt,
                    MAX_PLUGIN_RESULT_EVENT_BYTES,
                    Box::new(move |reason| declined_result(&decline_plugin_id, reason)),
                    Box::new(move |control| {
                        Box::pin(run_plugin_action(
                            run_host,
                            run_plugin_id,
                            tool_name,
                            arguments,
                            control,
                        ))
                    }),
                );
                match action {
                    Ok(action) => ToolActionSetupOutcome::Ready(action),
                    Err(_) => ToolActionSetupOutcome::Infrastructure {
                        turn_stop: ToolActionTurnStop::None,
                    },
                }
            })
        }),
    )?;
    Ok(ToolPreparation::Action(setup))
}

fn bounded_approval_preview(
    plugin_id: &str,
    tool_name: &str,
    arguments: &str,
) -> Result<String, ToolExecutorError> {
    let prefix = format!(
        "Native plugin `{plugin_id}` is a trusted local process and is not sandboxed.\n\
         It requests `{tool_name}` with arguments:\n"
    );
    if prefix.len().saturating_add(arguments.len()) <= MAX_APPROVAL_PREVIEW_BYTES {
        return Ok(format!("{prefix}{arguments}"));
    }
    let suffix = format!(
        "\n… [arguments truncated; original {} UTF-8 bytes]",
        arguments.len()
    );
    let available = MAX_APPROVAL_PREVIEW_BYTES
        .checked_sub(prefix.len().saturating_add(suffix.len()))
        .ok_or_else(|| ToolExecutorError::new("plugin approval preview prefix is too large"))?;
    let mut end = available.min(arguments.len());
    while !arguments.is_char_boundary(end) {
        end -= 1;
    }
    let mut preview = String::new();
    preview
        .try_reserve_exact(prefix.len() + end + suffix.len())
        .map_err(|_| ToolExecutorError::new("plugin approval preview allocation failed"))?;
    preview.push_str(&prefix);
    preview.push_str(&arguments[..end]);
    preview.push_str(&suffix);
    Ok(preview)
}

fn setup_stop(
    control: &ToolActionSetupControl,
) -> Option<(ToolActionTurnStop, &'static str, &'static str)> {
    if control.cancellation().is_cancelled() {
        Some((
            ToolActionTurnStop::CallerCancelled,
            "ABORTED_BEFORE_DISPATCH",
            "plugin call was cancelled before dispatch",
        ))
    } else if tokio::time::Instant::now() >= control.turn_deadline() {
        Some((
            ToolActionTurnStop::TurnTimeout,
            "ABORTED_BEFORE_DISPATCH",
            "plugin call stopped because the turn timed out",
        ))
    } else if tokio::time::Instant::now() >= control.preparation_deadline() {
        Some((
            ToolActionTurnStop::None,
            "PLUGIN_TIMEOUT",
            "plugin call preparation timed out",
        ))
    } else {
        None
    }
}

async fn run_plugin_action(
    host: Arc<PluginHost>,
    plugin_id: String,
    tool_name: String,
    arguments: JsonValue,
    control: ToolActionControl,
) -> ToolActionOutcome {
    let outcome = host
        .invoke(
            &tool_name,
            arguments,
            PluginCallControl::new(
                control.cancellation().clone(),
                control.turn_deadline().into_std(),
                control.action_deadline().into_std(),
            ),
        )
        .await;
    normalize_outcome(&plugin_id, outcome)
}

fn normalize_outcome(plugin_id: &str, outcome: PluginCallOutcome) -> ToolActionOutcome {
    match outcome {
        PluginCallOutcome::Settled(payload) => {
            let result = match payload {
                PluginResultPayload::Success(value) => settled_success(plugin_id, &value),
                PluginResultPayload::Failure(error) => settled_error(
                    plugin_id,
                    "PluginError",
                    error.code(),
                    &visible_controls(error.message()),
                    true,
                ),
            };
            normalized_started(result, ToolActionTurnStop::None)
        }
        PluginCallOutcome::InvalidArguments => normalized_not_started(
            not_started_error(
                plugin_id,
                "ToolArgsError",
                "INVALID_ARGS",
                "plugin arguments do not match the declared schema",
            ),
            ToolActionTurnStop::None,
        ),
        PluginCallOutcome::InvalidOutput => normalized_started(
            settled_error(
                plugin_id,
                "PluginOutputError",
                "PLUGIN_OUTPUT_INVALID",
                "plugin result does not match the declared output schema",
                true,
            ),
            ToolActionTurnStop::None,
        ),
        PluginCallOutcome::Busy => normalized_not_started(
            not_started_error(
                plugin_id,
                "PluginBusyError",
                "PLUGIN_BUSY",
                "plugin call queue is full",
            ),
            ToolActionTurnStop::None,
        ),
        PluginCallOutcome::Unavailable => normalized_not_started(
            not_started_error(
                plugin_id,
                "PluginUnavailableError",
                "PLUGIN_UNAVAILABLE",
                "plugin is unavailable",
            ),
            ToolActionTurnStop::None,
        ),
        PluginCallOutcome::StoppedBeforeDispatch { stop } => {
            let (turn_stop, code, message) = stop_parts(stop);
            let result =
                plugin_error_result(plugin_id, "AbortError", code, message, false, false, true);
            normalized_not_started(result, turn_stop)
        }
        PluginCallOutcome::StoppedAfterSettlement { stop } => {
            let (turn_stop, code, message) = stop_parts(stop);
            normalized_started(
                plugin_error_result(plugin_id, "AbortError", code, message, true, true, true),
                turn_stop,
            )
        }
        PluginCallOutcome::OutcomeUnknown { stop } => {
            let turn_stop = stop.map_or(ToolActionTurnStop::None, |stop| stop_parts(stop).0);
            normalized_started(
                plugin_error_result(
                    plugin_id,
                    "PluginOutcomeError",
                    "TOOL_OUTCOME_UNKNOWN",
                    "plugin call may have started, but no trustworthy matching result was received",
                    true,
                    false,
                    true,
                ),
                turn_stop,
            )
        }
        PluginCallOutcome::OwnershipLost { stop, dispatched } => {
            let turn_stop = stop.map_or(ToolActionTurnStop::None, |stop| stop_parts(stop).0);
            if dispatched {
                ToolActionOutcome::StartedOwnershipLost { turn_stop }
            } else {
                ToolActionOutcome::Infrastructure { turn_stop }
            }
        }
    }
}

fn settled_success(
    plugin_id: &str,
    value: &JsonValue,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let text = value.as_value().as_str().map_or_else(
        || {
            serde_json::to_string(value.as_value())
                .map_err(|_| ToolExecutorError::new("plugin result could not be rendered"))
        },
        |value| Ok(value.to_owned()),
    )?;
    let block = ContentBlock::text(text)
        .map_err(|_| ToolExecutorError::new("plugin result could not be normalized"))?;
    if block.raw().encoded_len() > MAX_PLUGIN_CONTENT_BYTES {
        return settled_error(
            plugin_id,
            "PluginOutputError",
            "PLUGIN_OUTPUT_INVALID",
            "plugin result expands beyond the rendered output limit",
            true,
        );
    }
    ToolExecutionResult::new(
        vec![block],
        false,
        None,
        Some(plugin_meta(plugin_id, true, true, true)?),
        false,
    )
    .map_err(|_| ToolExecutorError::new("plugin success result is invalid"))
}

fn settled_error(
    plugin_id: &str,
    name: &str,
    code: &str,
    message: &str,
    peer_settled: bool,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    plugin_error_result(plugin_id, name, code, message, true, peer_settled, true)
}

fn not_started_error(
    plugin_id: &str,
    name: &str,
    code: &str,
    message: &str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    plugin_error_result(plugin_id, name, code, message, false, false, true)
}

#[allow(clippy::too_many_arguments)]
fn plugin_error_result(
    plugin_id: &str,
    name: &str,
    code: &str,
    message: &str,
    dispatched: bool,
    peer_settled: bool,
    quiescent: bool,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let content = ContentBlock::text(format!("Error: {message}"))
        .map_err(|_| ToolExecutorError::new("plugin error content is invalid"))?;
    ToolExecutionResult::new(
        vec![content],
        true,
        Some(ToolFailure {
            name: name.to_owned(),
            code: code.to_owned(),
        }),
        Some(plugin_meta(plugin_id, dispatched, peer_settled, quiescent)?),
        false,
    )
    .map_err(|_| ToolExecutorError::new("plugin error result is invalid"))
}

fn plugin_meta(
    plugin_id: &str,
    dispatched: bool,
    peer_settled: bool,
    quiescent: bool,
) -> Result<JsonValue, ToolExecutorError> {
    JsonValue::new(json!({
        "kind":"plugin",
        "pluginId":plugin_id,
        "dispatched":dispatched,
        "peerSettled":peer_settled,
        "quiescent":quiescent
    }))
    .map_err(|_| ToolExecutorError::new("plugin result metadata is invalid"))
}

fn declined_result(
    plugin_id: &str,
    reason: ActionDeclineReason,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let (name, code, message) = match reason {
        ActionDeclineReason::PolicyDenied => (
            "PluginPolicyError",
            "PLUGIN_POLICY_DENIED",
            "plugin execution is denied by policy",
        ),
        ActionDeclineReason::ApprovalRejected => (
            "ApprovalError",
            "APPROVAL_REJECTED",
            "plugin execution approval was rejected",
        ),
        ActionDeclineReason::ApprovalCancelled => (
            "ApprovalError",
            "APPROVAL_CANCELLED",
            "plugin execution approval was cancelled",
        ),
        ActionDeclineReason::ApprovalUnavailable => (
            "ApprovalError",
            "APPROVAL_UNAVAILABLE",
            "plugin execution approval is unavailable",
        ),
        ActionDeclineReason::AbortedBeforeDispatch => (
            "AbortError",
            "ABORTED_BEFORE_DISPATCH",
            "plugin execution stopped before dispatch",
        ),
        ActionDeclineReason::OutputBudgetExceeded => (
            "ToolOutputError",
            "TOOL_OUTPUT_BUDGET_EXCEEDED",
            "plugin result cannot fit in the remaining session output budget",
        ),
    };
    not_started_error(plugin_id, name, code, message)
}

fn stop_parts(stop: PluginStop) -> (ToolActionTurnStop, &'static str, &'static str) {
    match stop {
        PluginStop::CallerCancelled => (
            ToolActionTurnStop::CallerCancelled,
            "ABORTED",
            "plugin call was cancelled",
        ),
        PluginStop::TurnTimeout => (
            ToolActionTurnStop::TurnTimeout,
            "ABORTED",
            "plugin call stopped because the turn timed out",
        ),
        PluginStop::ActionTimeout => (
            ToolActionTurnStop::None,
            "PLUGIN_TIMEOUT",
            "plugin call exceeded its time limit",
        ),
    }
}

fn normalized_started(
    result: Result<ToolExecutionResult, ToolExecutorError>,
    turn_stop: ToolActionTurnStop,
) -> ToolActionOutcome {
    match result {
        Ok(result) => ToolActionOutcome::StartedAndQuiescent { turn_stop, result },
        Err(_) => ToolActionOutcome::StartedOwnershipLost { turn_stop },
    }
}

fn normalized_not_started(
    result: Result<ToolExecutionResult, ToolExecutorError>,
    turn_stop: ToolActionTurnStop,
) -> ToolActionOutcome {
    match result {
        Ok(result) => ToolActionOutcome::NotStarted { turn_stop, result },
        Err(_) => ToolActionOutcome::Infrastructure { turn_stop },
    }
}

fn visible_controls(value: &str) -> String {
    let mut visible = String::new();
    for character in value.chars() {
        if character.is_control() {
            visible.extend(character.escape_default());
        } else {
            visible.push(character);
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use crate::agent::MAX_APPROVAL_PREVIEW_BYTES;

    use super::bounded_approval_preview;

    #[test]
    fn maximum_arguments_still_produce_a_bounded_explicitly_unsandboxed_preview() {
        let arguments = format!("{{\"text\":\"{}\"}}", "界".repeat(24_000));
        let preview = bounded_approval_preview("text-tools", "text_stats", &arguments).unwrap();
        assert!(preview.len() <= MAX_APPROVAL_PREVIEW_BYTES);
        assert!(preview.contains("not sandboxed"));
        assert!(preview.contains("arguments truncated"));
        assert!(std::str::from_utf8(preview.as_bytes()).is_ok());
    }
}
