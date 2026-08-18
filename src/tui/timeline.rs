//! Truth-safe product views derived from committed tool and turn facts.

use std::{fmt, fmt::Write as _};

use thiserror::Error;

use crate::{
    agent::TurnOutcome,
    session::{TurnEndReason, TurnId},
};

use super::projector::{
    PatchActivityOperation, ToolActivity, ToolActivityState, UiProjectorStatus,
};

const MAX_VIEW_TEXT_BYTES: usize = 4 * 1024;
const MAX_CARD_TEXT_BYTES: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TimelineTone {
    Accent,
    Positive,
    Caution,
    Negative,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub(crate) enum TimelineViewError {
    #[error("CLI_OUTPUT_CAPACITY")]
    Capacity,
}

pub(crate) struct ToolCardView {
    tone: TimelineTone,
    headline: String,
    detail: Option<String>,
}

impl fmt::Debug for ToolCardView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolCardView")
            .field("tone", &self.tone)
            .field("headline_bytes", &self.headline.len())
            .field("detail_bytes", &self.detail.as_ref().map_or(0, String::len))
            .finish()
    }
}

impl ToolCardView {
    pub(crate) fn from_activity(tool: &ToolActivity) -> Result<Self, TimelineViewError> {
        if tool.state == ToolActivityState::OutcomeUnknown || plugin_outcome_unknown(tool) {
            return Self::new(
                TimelineTone::Negative,
                format!("Outcome unknown  {}", display_tool_name(tool)),
                Some("Side effects are not assumed safe to replay".to_owned()),
            );
        }

        if matches!(
            tool.state,
            ToolActivityState::Denied
                | ToolActivityState::Cancelled
                | ToolActivityState::Unavailable
        ) {
            if tool.conflicting_effect {
                let mut detail =
                    "Approval state conflicts with committed effect metadata".to_owned();
                if let Some(path) = tool.patch_path.as_deref() {
                    append_piece(&mut detail, path);
                }
                append_failure(&mut detail, tool);
                return Self::new(
                    TimelineTone::Negative,
                    format!("Conflicting tool facts  {}", display_tool_name(tool)),
                    Some(detail),
                );
            }
            let state = match tool.state {
                ToolActivityState::Denied => "Rejected",
                ToolActivityState::Cancelled => "Cancelled",
                ToolActivityState::Unavailable => "Unavailable",
                _ => unreachable!("the enclosing match fixes the approval state"),
            };
            let mut detail = tool.summary.clone().unwrap_or_default();
            append_failure(&mut detail, tool);
            let headline = if let Some(plugin_id) = tool.plugin_id.as_deref() {
                format!("Plugin {}  {plugin_id}", state.to_ascii_lowercase())
            } else {
                format!("{state}  {}", display_tool_name(tool))
            };
            return Self::new(
                TimelineTone::Caution,
                headline,
                (!detail.is_empty()).then_some(detail),
            );
        }

        if tool.name.as_str() == "apply_patch" {
            if let (Some(path), Some(operation), Some(committed)) = (
                tool.patch_path.as_deref(),
                tool.patch_operation,
                tool.committed_effect,
            ) {
                let action = match operation {
                    PatchActivityOperation::Create => "Created",
                    PatchActivityOperation::Update => "Updated",
                };
                let warning = tool.state == ToolActivityState::Failed
                    || tool.patch_cleanup_warning == Some(true);
                let (tone, headline) = match (committed, warning) {
                    (true, true) => (
                        TimelineTone::Caution,
                        format!("Changed with warning  {path}"),
                    ),
                    (true, false) => (TimelineTone::Positive, format!("{action}  {path}")),
                    (false, _) => (TimelineTone::Caution, format!("Not changed  {path}")),
                };
                let mut detail = format!(
                    "+{} -{}",
                    tool.patch_additions.unwrap_or(0),
                    tool.patch_removals.unwrap_or(0)
                );
                append_failure(&mut detail, tool);
                return Self::new(tone, headline, Some(detail));
            }
        }

        if tool.name.as_str() == "bash" {
            if tool.started_process == Some(true) {
                let (tone, status) = if tool.shell_timed_out == Some(true) {
                    (TimelineTone::Negative, "Timed out".to_owned())
                } else if let Some(signal) = tool.shell_signal.as_deref() {
                    (TimelineTone::Negative, format!("Signal {signal}"))
                } else if tool.state == ToolActivityState::Failed {
                    (TimelineTone::Negative, "Command failed".to_owned())
                } else if let Some(code) = tool.shell_exit_code {
                    (
                        if code == 0 {
                            TimelineTone::Positive
                        } else {
                            TimelineTone::Negative
                        },
                        format!("Exit {code}"),
                    )
                } else {
                    (TimelineTone::Caution, "Command settled".to_owned())
                };
                let mut detail = tool.summary.clone().unwrap_or_default();
                if tool.state == ToolActivityState::Failed {
                    if let Some(code) = tool.shell_exit_code {
                        append_piece(&mut detail, &format!("exit {code}"));
                    }
                    append_failure(&mut detail, tool);
                }
                return Self::new(tone, status, (!detail.is_empty()).then_some(detail));
            }
            if tool.started_process == Some(false) {
                let mut detail = tool.summary.clone().unwrap_or_default();
                append_failure(&mut detail, tool);
                return Self::new(
                    TimelineTone::Caution,
                    "Command not started".to_owned(),
                    (!detail.is_empty()).then_some(detail),
                );
            }
        }

        if let Some(plugin_id) = tool.plugin_id.as_deref() {
            let headline = match tool.state {
                ToolActivityState::Completed => format!("Plugin completed  {plugin_id}"),
                ToolActivityState::Denied => format!("Plugin rejected  {plugin_id}"),
                ToolActivityState::Cancelled => format!("Plugin cancelled  {plugin_id}"),
                ToolActivityState::Unavailable => format!("Plugin unavailable  {plugin_id}"),
                ToolActivityState::Failed => format!("Plugin failed  {plugin_id}"),
                _ => format!("Plugin settled  {plugin_id}"),
            };
            let tone = tone_for_state(tool.state);
            let mut detail = String::new();
            if tool.plugin_dispatched == Some(false) {
                detail.push_str("Not dispatched");
            }
            append_failure(&mut detail, tool);
            return Self::new(tone, headline, (!detail.is_empty()).then_some(detail));
        }

        let state = match tool.state {
            ToolActivityState::Completed => "Completed",
            ToolActivityState::Failed => "Failed",
            ToolActivityState::Denied => "Rejected",
            ToolActivityState::Cancelled => "Cancelled",
            ToolActivityState::Unavailable => "Unavailable",
            ToolActivityState::Preparing
            | ToolActivityState::AwaitingApproval
            | ToolActivityState::Allowed => "Result missing",
            ToolActivityState::OutcomeUnknown => "Outcome unknown",
        };
        let headline = format!("{state}  {}", display_tool_name(tool));
        let mut detail = tool.summary.clone().unwrap_or_default();
        append_failure(&mut detail, tool);
        if tool.payload_omitted {
            append_piece(&mut detail, "details omitted");
        }
        Self::new(
            tone_for_state(tool.state),
            headline,
            (!detail.is_empty()).then_some(detail),
        )
    }

    fn new(
        tone: TimelineTone,
        headline: String,
        detail: Option<String>,
    ) -> Result<Self, TimelineViewError> {
        Ok(Self {
            tone,
            headline: bounded_to(headline, MAX_CARD_TEXT_BYTES)?,
            detail: detail
                .map(single_line_summary)
                .transpose()?
                .map(|detail| bounded_to(detail, MAX_CARD_TEXT_BYTES))
                .transpose()?,
        })
    }

    pub(crate) const fn tone(&self) -> TimelineTone {
        self.tone
    }

    pub(crate) fn headline(&self) -> &str {
        &self.headline
    }

    pub(crate) fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

pub(crate) struct WorkReceiptView {
    tone: TimelineTone,
    headline: String,
    counters: Option<String>,
    effects: Option<String>,
}

struct OutcomeFacts<'a> {
    turn: TurnId,
    reason: &'a TurnEndReason,
    steps: usize,
    retries: usize,
    tool_calls: usize,
    reported_output_tokens: u64,
}

impl<'a> From<&'a TurnOutcome> for OutcomeFacts<'a> {
    fn from(outcome: &'a TurnOutcome) -> Self {
        Self {
            turn: outcome.turn(),
            reason: outcome.reason(),
            steps: outcome.steps(),
            retries: outcome.retries(),
            tool_calls: outcome.tool_calls(),
            reported_output_tokens: outcome.reported_output_tokens(),
        }
    }
}

impl fmt::Debug for WorkReceiptView {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkReceiptView")
            .field("tone", &self.tone)
            .field("headline_bytes", &self.headline.len())
            .field(
                "counter_bytes",
                &self.counters.as_ref().map_or(0, String::len),
            )
            .field(
                "effect_bytes",
                &self.effects.as_ref().map_or(0, String::len),
            )
            .finish()
    }
}

impl WorkReceiptView {
    pub(crate) fn from_outcome(
        outcome: &TurnOutcome,
        tools: &[ToolActivity],
        status: UiProjectorStatus,
    ) -> Result<Self, TimelineViewError> {
        Self::from_facts(OutcomeFacts::from(outcome), tools, status)
    }

    fn from_facts(
        outcome: OutcomeFacts<'_>,
        tools: &[ToolActivity],
        status: UiProjectorStatus,
    ) -> Result<Self, TimelineViewError> {
        let (tone, headline) = match outcome.reason {
            TurnEndReason::Completed => (TimelineTone::Accent, "Turn complete".to_owned()),
            TurnEndReason::Aborted { .. } => (TimelineTone::Caution, "Turn stopped".to_owned()),
            TurnEndReason::Blocked => (TimelineTone::Caution, "Turn blocked".to_owned()),
            TurnEndReason::MaxTokens => (TimelineTone::Caution, "Token limit reached".to_owned()),
            TurnEndReason::Interrupted => (TimelineTone::Caution, "Turn interrupted".to_owned()),
            TurnEndReason::Error { error } => (
                TimelineTone::Negative,
                format!("Turn failed  {}", error.code()),
            ),
            TurnEndReason::Other { kind, .. } => (
                TimelineTone::Caution,
                kind.as_deref().map_or_else(
                    || "Turn ended".to_owned(),
                    |kind| format!("Turn ended  {kind}"),
                ),
            ),
        };

        let mut counters = format!(
            "{} {} | {} tool {}",
            outcome.steps,
            plural(outcome.steps, "step", "steps"),
            outcome.tool_calls,
            plural(outcome.tool_calls, "request", "requests"),
        );
        if outcome.retries != 0 {
            write!(
                &mut counters,
                " | {} {}",
                outcome.retries,
                plural(outcome.retries, "retry", "retries")
            )
            .map_err(|_| TimelineViewError::Capacity)?;
        }
        if outcome.reported_output_tokens != 0 {
            write!(
                &mut counters,
                " | {} reported output tokens",
                outcome.reported_output_tokens
            )
            .map_err(|_| TimelineViewError::Capacity)?;
        }

        let mut changed_paths = Vec::new();
        changed_paths
            .try_reserve_exact(tools.len())
            .map_err(|_| TimelineViewError::Capacity)?;
        let mut additions = 0_usize;
        let mut removals = 0_usize;
        let mut commands = 0_usize;
        let mut issues = 0_usize;
        for tool in tools.iter().filter(|tool| tool.turn == outcome.turn) {
            if tool.committed_effect == Some(true) {
                if let Some(path) = tool.patch_path.as_deref() {
                    if !changed_paths.contains(&path) {
                        changed_paths.push(path);
                    }
                }
                additions = additions.saturating_add(tool.patch_additions.unwrap_or(0));
                removals = removals.saturating_add(tool.patch_removals.unwrap_or(0));
            }
            if tool.started_process == Some(true) {
                commands = commands.saturating_add(1);
            }
            if matches!(
                tool.state,
                ToolActivityState::Failed
                    | ToolActivityState::Denied
                    | ToolActivityState::Cancelled
                    | ToolActivityState::Unavailable
                    | ToolActivityState::OutcomeUnknown
            ) || tool.shell_exit_code.is_some_and(|code| code != 0)
                || tool.shell_timed_out == Some(true)
                || tool.shell_signal.is_some()
                || plugin_outcome_unknown(tool)
                || tool.conflicting_effect
            {
                issues = issues.saturating_add(1);
            }
        }
        let mut effect_parts = Vec::new();
        effect_parts
            .try_reserve_exact(4)
            .map_err(|_| TimelineViewError::Capacity)?;
        if !changed_paths.is_empty() {
            effect_parts.push(format!(
                "{} {} changed (+{additions} -{removals})",
                changed_paths.len(),
                plural(changed_paths.len(), "file", "files")
            ));
        }
        if commands != 0 {
            effect_parts.push(format!(
                "{commands} {} run",
                plural(commands, "command", "commands")
            ));
        }
        if issues != 0 {
            effect_parts.push(format!("{issues} {}", plural(issues, "issue", "issues")));
        }
        if status.degraded
            || status.omitted_tool_facts != 0
            || status.omitted_approval_facts != 0
            || status.orphan_prune_markers != 0
            || status.conflicting_facts != 0
        {
            effect_parts.push("details incomplete".to_owned());
        }
        let effects = (!effect_parts.is_empty()).then(|| effect_parts.join(" | "));
        Ok(Self {
            tone,
            headline: bounded(headline)?,
            counters: Some(bounded(counters)?),
            effects: effects.map(bounded).transpose()?,
        })
    }

    pub(crate) const fn tone(&self) -> TimelineTone {
        self.tone
    }

    pub(crate) fn headline(&self) -> &str {
        &self.headline
    }

    pub(crate) fn counters(&self) -> Option<&str> {
        self.counters.as_deref()
    }

    pub(crate) fn effects(&self) -> Option<&str> {
        self.effects.as_deref()
    }
}

fn display_tool_name(tool: &ToolActivity) -> &str {
    match tool.name.as_str() {
        "list" => "List",
        "glob" => "Glob",
        "grep" => "Search",
        "read" => "Read",
        "apply_patch" => "Patch",
        "bash" => "Command",
        name => name,
    }
}

fn tone_for_state(state: ToolActivityState) -> TimelineTone {
    match state {
        ToolActivityState::Completed => TimelineTone::Positive,
        ToolActivityState::Preparing | ToolActivityState::Allowed => TimelineTone::Accent,
        ToolActivityState::AwaitingApproval
        | ToolActivityState::Denied
        | ToolActivityState::Cancelled => TimelineTone::Caution,
        ToolActivityState::Failed
        | ToolActivityState::Unavailable
        | ToolActivityState::OutcomeUnknown => TimelineTone::Negative,
    }
}

fn append_failure(output: &mut String, tool: &ToolActivity) {
    if let Some(code) = tool.failure_code.as_deref() {
        append_piece(output, code);
    }
}

fn append_piece(output: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    if !output.is_empty() {
        output.push_str(" | ");
    }
    output.push_str(piece);
}

fn plugin_outcome_unknown(tool: &ToolActivity) -> bool {
    match (
        tool.plugin_id.as_ref(),
        tool.plugin_dispatched,
        tool.plugin_peer_settled,
        tool.plugin_quiescent,
    ) {
        (Some(_), Some(dispatched), Some(peer_settled), Some(quiescent)) => {
            !quiescent
                || dispatched != peer_settled
                || (tool.state == ToolActivityState::Completed && !dispatched)
        }
        _ => false,
    }
}

fn single_line_summary(value: String) -> Result<String, TimelineViewError> {
    let mut output = String::new();
    output
        .try_reserve_exact(MAX_CARD_TEXT_BYTES)
        .map_err(|_| TimelineViewError::Capacity)?;
    let content_limit = MAX_CARD_TEXT_BYTES.saturating_sub("...".len());
    let mut truncated = false;
    for character in value.chars() {
        let needed = if character == '\n' {
            "\\n".len()
        } else {
            character.len_utf8()
        };
        if output.len().saturating_add(needed) > content_limit {
            truncated = true;
            break;
        }
        if character == '\n' {
            output.push_str("\\n");
        } else {
            output.push(character);
        }
    }
    if truncated {
        output.push_str("...");
    }
    Ok(output)
}

fn bounded(value: String) -> Result<String, TimelineViewError> {
    bounded_to(value, MAX_VIEW_TEXT_BYTES)
}

fn bounded_to(mut value: String, maximum: usize) -> Result<String, TimelineViewError> {
    if value.len() <= maximum {
        return Ok(value);
    }
    let mut end = maximum.saturating_sub("...".len());
    while end != 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
        .try_reserve("...".len())
        .map_err(|_| TimelineViewError::Capacity)?;
    value.push_str("...");
    Ok(value)
}

fn plural<'a>(count: usize, singular: &'a str, plural: &'a str) -> &'a str {
    if count == 1 { singular } else { plural }
}

#[cfg(test)]
mod tests {
    use crate::{
        model::LlmFailure,
        session::{
            ApprovalOutcome, CommittedUiKind, StepId, TurnEndReason, TurnId, UiIdentity,
            UiOpaquePayload, UiToolFailure,
        },
        tui::projector::{ToolActivityState, UiProjector},
    };

    use super::{OutcomeFacts, TimelineTone, ToolCardView, WorkReceiptView};

    fn turn() -> TurnId {
        TurnId::new(1).unwrap()
    }

    fn step() -> StepId {
        StepId::new(1).unwrap()
    }

    fn id(value: &str) -> UiIdentity {
        UiIdentity::from_text_for_test(value)
    }

    fn request(projector: &mut UiProjector, name: &str, arguments: &str) {
        projector
            .observe(&CommittedUiKind::ToolRequested {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                name: id(name),
                arguments: UiOpaquePayload::from_text_for_test(arguments),
            })
            .unwrap();
    }

    fn decide(projector: &mut UiProjector, tool_name: &str, outcome: ApprovalOutcome) {
        projector
            .observe(&CommittedUiKind::ApprovalAsked {
                id: id("approval"),
                tool_name: id(tool_name),
                call_id: Some(id("call")),
                reason: None,
            })
            .unwrap();
        projector
            .observe(&CommittedUiKind::ApprovalDecided {
                id: id("approval"),
                outcome,
            })
            .unwrap();
    }

    fn reject(projector: &mut UiProjector, tool_name: &str) {
        decide(projector, tool_name, ApprovalOutcome::Rejected);
    }

    #[test]
    fn committed_patch_warning_keeps_effect_and_counts_only_hunk_lines() {
        let mut projector = UiProjector::default();
        request(&mut projector, "apply_patch", r#"{"patch":"secret-input"}"#);
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: Some(UiToolFailure {
                    name: "DurabilityWarning".to_owned(),
                    code: "PATCH_WARNING_SECRET".to_owned(),
                }),
                content: UiOpaquePayload::from_text_for_test("secret result body"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"path":"src/secret.rs","operation":"update","diff":"--- a/src/secret.rs\n+++ b/src/secret.rs\n@@ -1,2 +1,2 @@\n-old\n---content\n+new\n+++content\n","committed":true,"cleanupWarning":true}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let tool = &projector.tools()[0];
        assert_eq!(tool.patch_additions, Some(2));
        assert_eq!(tool.patch_removals, Some(2));
        let card = ToolCardView::from_activity(tool).unwrap();
        assert_eq!(card.tone(), TimelineTone::Caution);
        assert!(card.headline().starts_with("Changed with warning"));
        assert!(card.detail().unwrap().contains("+2 -2"));
        let debug = format!("{card:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains("PATCH_WARNING_SECRET"));
    }

    #[test]
    fn natural_nonzero_shell_exit_is_not_presented_as_success() {
        let mut projector = UiProjector::default();
        request(
            &mut projector,
            "bash",
            r#"{"command":"cargo test secret-suite"}"#,
        );
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("42 tests passed"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"foreground","started":true,"exitCode":1,"signal":null,"timedOut":false,"aborted":false,"outputLimitExceeded":false,"pipeSetupFailed":false,"pipeReadFailed":false,"signalDeliveryFailed":false,"pipeDrainTimedOut":false,"timeoutMs":1000,"workdir":".","stdoutTruncated":false,"stderrTruncated":false}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let card = ToolCardView::from_activity(&projector.tools()[0]).unwrap();
        assert_eq!(card.tone(), TimelineTone::Negative);
        assert_eq!(card.headline(), "Exit 1");
        assert!(!card.headline().contains("success"));
        assert!(!card.detail().unwrap().contains("42 tests passed"));
    }

    #[test]
    fn shell_failure_cannot_be_hidden_by_a_zero_exit_fact() {
        let mut projector = UiProjector::default();
        request(&mut projector, "bash", r#"{"command":"produce too much"}"#);
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: true,
                failure: Some(UiToolFailure {
                    name: "OutputLimit".to_owned(),
                    code: "SHELL_OUTPUT_LIMIT".to_owned(),
                }),
                content: UiOpaquePayload::from_text_for_test("omitted"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"foreground","started":true,"exitCode":0,"signal":null,"timedOut":false,"aborted":false,"outputLimitExceeded":true,"pipeSetupFailed":false,"pipeReadFailed":false,"signalDeliveryFailed":false,"pipeDrainTimedOut":false,"timeoutMs":1000,"workdir":".","stdoutTruncated":true,"stderrTruncated":false}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let card = ToolCardView::from_activity(&projector.tools()[0]).unwrap();
        assert_eq!(card.tone(), TimelineTone::Negative);
        assert_eq!(card.headline(), "Command failed");
        assert!(card.detail().unwrap().contains("SHELL_OUTPUT_LIMIT"));
    }

    #[test]
    fn focus_command_summary_is_single_line_and_compact() {
        let mut projector = UiProjector::default();
        let command = format!("echo first\n{}", "x".repeat(1_000));
        request(
            &mut projector,
            "bash",
            &serde_json::json!({ "command": command }).to_string(),
        );
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("ignored"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"foreground","started":true,"exitCode":0,"signal":null,"timedOut":false,"aborted":false,"outputLimitExceeded":false,"pipeSetupFailed":false,"pipeReadFailed":false,"signalDeliveryFailed":false,"pipeDrainTimedOut":false,"timeoutMs":1000,"workdir":".","stdoutTruncated":false,"stderrTruncated":false}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let card = ToolCardView::from_activity(&projector.tools()[0]).unwrap();
        let detail = card.detail().unwrap();
        assert!(detail.len() <= 256);
        assert!(!detail.contains('\n'));
        assert!(detail.contains("\\n"));
        assert!(detail.ends_with("..."));
    }

    #[test]
    fn contradictory_approval_and_effect_facts_never_render_as_success() {
        let mut patch = UiProjector::default();
        request(&mut patch, "apply_patch", r#"{"patch":"diff"}"#);
        reject(&mut patch, "apply_patch");
        patch
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("ignored"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"path":"src/lib.rs","operation":"update","diff":"--- a\n+++ b\n@@ -1 +1 @@\n-old\n+new\n","committed":true}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        assert!(patch.tools()[0].conflicting_effect);
        let patch_card = ToolCardView::from_activity(&patch.tools()[0]).unwrap();
        assert_eq!(patch_card.tone(), TimelineTone::Negative);
        assert!(patch_card.headline().starts_with("Conflicting tool facts"));
        assert!(!patch_card.headline().starts_with("Updated"));

        let mut shell = UiProjector::default();
        request(&mut shell, "bash", r#"{"command":"echo unsafe"}"#);
        reject(&mut shell, "bash");
        shell
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("ignored"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"foreground","started":true,"exitCode":0,"signal":null,"timedOut":false,"aborted":false,"outputLimitExceeded":false,"pipeSetupFailed":false,"pipeReadFailed":false,"signalDeliveryFailed":false,"pipeDrainTimedOut":false,"timeoutMs":1000,"workdir":".","stdoutTruncated":false,"stderrTruncated":false}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let shell_card = ToolCardView::from_activity(&shell.tools()[0]).unwrap();
        assert_eq!(shell_card.tone(), TimelineTone::Negative);
        assert!(shell_card.headline().starts_with("Conflicting tool facts"));
        assert_ne!(shell_card.headline(), "Exit 0");
    }

    #[test]
    fn receipt_uses_tool_requests_and_never_invents_test_results() {
        let mut projector = UiProjector::default();
        request(&mut projector, "read", r#"{"file_path":"src/lib.rs"}"#);
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("42 tests passed"),
                meta: UiOpaquePayload::from_text_for_test("{}"),
                surface_replacement_target: None,
            })
            .unwrap();
        let reason = TurnEndReason::Completed;
        let receipt = WorkReceiptView::from_facts(
            OutcomeFacts {
                turn: turn(),
                reason: &reason,
                steps: 2,
                retries: 1,
                tool_calls: 3,
                reported_output_tokens: 88,
            },
            projector.tools(),
            projector.status(),
        )
        .unwrap();
        assert_eq!(receipt.headline(), "Turn complete");
        assert!(receipt.counters().unwrap().contains("3 tool requests"));
        let rendered = format!(
            "{} {} {}",
            receipt.headline(),
            receipt.counters().unwrap(),
            receipt.effects().unwrap_or_default()
        );
        assert!(!rendered.contains("tests passed"));
        assert!(!rendered.contains("All tests"));
    }

    #[test]
    fn unknown_outcome_is_never_flattened_to_an_ordinary_failure() {
        let mut projector = UiProjector::default();
        request(&mut projector, "read", r#"{"file_path":"src/lib.rs"}"#);
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: true,
                failure: Some(UiToolFailure {
                    name: "ToolOutcomeUnknown".to_owned(),
                    code: crate::session::TOOL_OUTCOME_UNKNOWN.to_owned(),
                }),
                content: UiOpaquePayload::from_text_for_test("secret"),
                meta: UiOpaquePayload::from_text_for_test("{}"),
                surface_replacement_target: None,
            })
            .unwrap();
        assert_eq!(
            projector.tools()[0].state,
            ToolActivityState::OutcomeUnknown
        );
        let card = ToolCardView::from_activity(&projector.tools()[0]).unwrap();
        assert!(card.headline().starts_with("Outcome unknown"));
        assert!(
            card.detail()
                .unwrap()
                .contains("not assumed safe to replay")
        );
    }

    #[test]
    fn plugin_cards_accept_only_closed_public_metadata() {
        let mut projector = UiProjector::default();
        request(
            &mut projector,
            "text_stats",
            r#"{"text":"SECRET_ARGUMENT"}"#,
        );
        projector
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("SECRET_RESULT"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"plugin","pluginId":"text-tools","dispatched":true,"peerSettled":true,"quiescent":true}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let card = ToolCardView::from_activity(&projector.tools()[0]).unwrap();
        assert_eq!(card.headline(), "Plugin completed  text-tools");
        let rendered = format!(
            "{} {} {card:?}",
            card.headline(),
            card.detail().unwrap_or("")
        );
        assert!(!rendered.contains("SECRET_ARGUMENT"));
        assert!(!rendered.contains("SECRET_RESULT"));

        let mut malformed = UiProjector::default();
        request(&mut malformed, "text_stats", r#"{"text":"safe"}"#);
        malformed
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("ignored"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"plugin","pluginId":"text-tools","dispatched":true,"peerSettled":true,"quiescent":true,"program":"/SECRET/PATH"}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        assert!(malformed.tools()[0].plugin_id.is_none());
        let fallback = ToolCardView::from_activity(&malformed.tools()[0]).unwrap();
        assert!(!fallback.headline().contains("SECRET"));

        let mut impossible = UiProjector::default();
        request(&mut impossible, "text_stats", r#"{"text":"safe"}"#);
        impossible
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("ignored"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"plugin","pluginId":"text-tools","dispatched":false,"peerSettled":true,"quiescent":true}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let impossible_card = ToolCardView::from_activity(&impossible.tools()[0]).unwrap();
        assert!(impossible_card.headline().starts_with("Outcome unknown"));
        let reason = TurnEndReason::Completed;
        let receipt = WorkReceiptView::from_facts(
            OutcomeFacts {
                turn: turn(),
                reason: &reason,
                steps: 1,
                retries: 0,
                tool_calls: 1,
                reported_output_tokens: 0,
            },
            impossible.tools(),
            impossible.status(),
        )
        .unwrap();
        assert!(receipt.effects().unwrap().contains("1 issue"));

        let mut completed_without_dispatch = UiProjector::default();
        request(
            &mut completed_without_dispatch,
            "text_stats",
            r#"{"text":"safe"}"#,
        );
        completed_without_dispatch
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: false,
                failure: None,
                content: UiOpaquePayload::from_text_for_test("ignored"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"plugin","pluginId":"text-tools","dispatched":false,"peerSettled":false,"quiescent":true}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let completed_without_dispatch_card =
            ToolCardView::from_activity(&completed_without_dispatch.tools()[0]).unwrap();
        assert!(
            completed_without_dispatch_card
                .headline()
                .starts_with("Outcome unknown")
        );
        let completed_without_dispatch_receipt = WorkReceiptView::from_facts(
            OutcomeFacts {
                turn: turn(),
                reason: &reason,
                steps: 1,
                retries: 0,
                tool_calls: 1,
                reported_output_tokens: 0,
            },
            completed_without_dispatch.tools(),
            completed_without_dispatch.status(),
        )
        .unwrap();
        assert!(
            completed_without_dispatch_receipt
                .effects()
                .unwrap()
                .contains("1 issue")
        );

        let mut rejected = UiProjector::default();
        request(&mut rejected, "text_stats", r#"{"text":"safe"}"#);
        reject(&mut rejected, "text_stats");
        rejected
            .observe(&CommittedUiKind::ToolResult {
                turn: turn(),
                step: step(),
                call_id: id("call"),
                is_error: true,
                failure: Some(UiToolFailure {
                    name: "PluginRejected".to_owned(),
                    code: "PLUGIN_REJECTED".to_owned(),
                }),
                content: UiOpaquePayload::from_text_for_test("ignored"),
                meta: UiOpaquePayload::from_text_for_test(
                    r#"{"kind":"plugin","pluginId":"text-tools","dispatched":false,"peerSettled":false,"quiescent":true}"#,
                ),
                surface_replacement_target: None,
            })
            .unwrap();
        let rejected_card = ToolCardView::from_activity(&rejected.tools()[0]).unwrap();
        assert_eq!(rejected_card.headline(), "Plugin rejected  text-tools");
    }

    #[test]
    fn plugin_predispatch_outcomes_keep_the_public_plugin_id() {
        for (outcome, expected) in [
            (ApprovalOutcome::Rejected, "Plugin rejected  text-tools"),
            (ApprovalOutcome::Cancelled, "Plugin cancelled  text-tools"),
            (
                ApprovalOutcome::Unavailable,
                "Plugin unavailable  text-tools",
            ),
        ] {
            let mut projector = UiProjector::default();
            request(&mut projector, "text_stats", r#"{"text":"safe"}"#);
            decide(&mut projector, "text_stats", outcome);
            projector
                .observe(&CommittedUiKind::ToolResult {
                    turn: turn(),
                    step: step(),
                    call_id: id("call"),
                    is_error: true,
                    failure: Some(UiToolFailure {
                        name: "PluginStopped".to_owned(),
                        code: "PLUGIN_STOPPED".to_owned(),
                    }),
                    content: UiOpaquePayload::from_text_for_test("ignored"),
                    meta: UiOpaquePayload::from_text_for_test(
                        r#"{"kind":"plugin","pluginId":"text-tools","dispatched":false,"peerSettled":false,"quiescent":true}"#,
                    ),
                    surface_replacement_target: None,
                })
                .unwrap();
            let card = ToolCardView::from_activity(&projector.tools()[0]).unwrap();
            assert_eq!(card.headline(), expected);
        }
    }

    #[test]
    fn error_receipt_debug_is_bounded_to_lengths() {
        let projector = UiProjector::default();
        let reason = TurnEndReason::Error {
            error: LlmFailure::new("SECRET_MESSAGE", "SECRET_CODE").unwrap(),
        };
        let receipt = WorkReceiptView::from_facts(
            OutcomeFacts {
                turn: turn(),
                reason: &reason,
                steps: 0,
                retries: 0,
                tool_calls: 0,
                reported_output_tokens: 0,
            },
            projector.tools(),
            projector.status(),
        )
        .unwrap();
        let debug = format!("{receipt:?}");
        assert!(!debug.contains("SECRET_CODE"));
        assert!(!debug.contains("SECRET_MESSAGE"));
    }
}
