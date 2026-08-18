#![cfg(unix)]

use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use deepseek_harness_cli::{
    agent::{
        AgentLoop, AgentLoopConfig, ApprovalFuture, ApprovalProvider, ApprovalProviderError,
        ApprovalRequest, FileChangePolicy, TurnProposal,
    },
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, JsonValue, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, NonNegativeSafeInteger, StreamChunk,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream, RetryBackoff,
        RetryPolicy,
    },
    session::{ApprovalOutcome, EventKind, Session, ToolFailure, TurnEndReason},
    tools::WorkspaceToolRegistry,
};
use diffy::{Line, create_patch};
use futures_util::stream;
use serde_json::{Value, json};
use tokio::sync::Barrier;
use tokio_util::sync::CancellationToken;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let ordinal = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dsh-file-changes-{label}-{}-{nanos}-{ordinal}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn new_short(label: &str) -> Self {
        let ordinal = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(format!("/tmp/dsh-{label}-{}-{ordinal}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn write(&self, relative: &str, content: &str) {
        fs::write(self.root.join(relative), content).unwrap();
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn mutation_file_with_size(size: usize) -> Vec<u8> {
    let mut bytes = b"old\none\ntwo\nthree\nfour\n".to_vec();
    while size.saturating_sub(bytes.len()) > 1024 * 1024 {
        bytes.extend(std::iter::repeat_n(b'x', 1024 * 1024 - 1));
        bytes.push(b'\n');
    }
    bytes.extend(std::iter::repeat_n(b'x', size - bytes.len()));
    bytes
}

struct ScriptedProvider {
    attempts: Mutex<VecDeque<Vec<StreamChunk>>>,
    dispatches: AtomicU64,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(patch: &str) -> Self {
        Self::with_response("call-1", patch, "done")
    }

    fn with_response(call_id: &str, patch: &str, final_text: &str) -> Self {
        Self {
            attempts: Mutex::new(
                vec![
                    tool_response_with_id(call_id, patch),
                    text_response_with(final_text),
                ]
                .into_iter()
                .collect(),
            ),
            dispatches: AtomicU64::new(0),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn dispatch_count(&self) -> u64 {
        self.dispatches.load(Ordering::SeqCst)
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelProvider for ScriptedProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        let mut raw = config.raw().as_value().clone();
        raw.as_object_mut()
            .unwrap()
            .insert("maxTokens".to_owned(), json!(1_024));
        let effective = serde_json::from_value(raw).unwrap();
        Ok(PreparedProviderCall::new(
            effective,
            LlmCallConfigAdapterDefaults::default(),
            Some(NonNegativeSafeInteger::new(10_000_000).unwrap()),
        )
        .with_retry_policy(
            RetryPolicy::normal(
                0,
                vec!["SERVER".to_owned()],
                RetryBackoff::new(1.0, 1.0, 0.0).unwrap(),
            )
            .unwrap(),
        ))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        let prepared = self.prepare_call(draft.config().clone())?;
        draft.finish(prepared, 1)
    }

    fn stream(&self, request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        self.requests
            .lock()
            .unwrap()
            .push(request.messages().to_vec());
        Box::pin(stream::iter(
            self.attempts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap()
                .into_iter()
                .map(Ok),
        ))
    }
}

struct FixedApproval {
    outcome: ApprovalOutcome,
    mutate_before_answer: Option<(PathBuf, String)>,
}

struct ActionApproval {
    action: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct RecordingApproval {
    outcome: ApprovalOutcome,
    requests: Arc<Mutex<Vec<ApprovalRequest>>>,
}

struct BarrierApproval {
    barrier: Arc<Barrier>,
}

impl ActionApproval {
    fn new(action: impl FnOnce() + Send + 'static) -> Self {
        Self {
            action: Mutex::new(Some(Box::new(action))),
        }
    }
}

impl ApprovalProvider for ActionApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        if let Some(action) = self.action.lock().unwrap().take() {
            action();
        }
        Box::pin(async { Ok(ApprovalOutcome::AllowedOnce) })
    }
}

impl ApprovalProvider for RecordingApproval {
    fn request(
        &self,
        request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        self.requests.lock().unwrap().push(request);
        let outcome = self.outcome;
        Box::pin(async move { Ok(outcome) })
    }
}

impl ApprovalProvider for BarrierApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        let barrier = self.barrier.clone();
        Box::pin(async move {
            barrier.wait().await;
            Ok(ApprovalOutcome::AllowedOnce)
        })
    }
}

impl ApprovalProvider for FixedApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        if let Some((path, content)) = &self.mutate_before_answer {
            fs::write(path, content).unwrap();
        }
        let outcome = self.outcome;
        Box::pin(async move { Ok::<_, ApprovalProviderError>(outcome) })
    }
}

fn tool_response_with_id(call_id: &str, patch: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(
            0,
            ContentBlock::tool_call(
                call_id,
                "apply_patch",
                json!({ "patch": patch }).to_string(),
            )
            .unwrap(),
        )
        .unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]
}

fn text_response_with(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn user() -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text("make the requested file change").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

async fn run_patch(
    workspace: &Path,
    patch: &str,
    policy: FileChangePolicy,
    approval: Arc<dyn ApprovalProvider>,
) -> Session {
    run_patch_with_provider(
        workspace,
        policy,
        approval,
        Arc::new(ScriptedProvider::new(patch)),
    )
    .await
}

async fn run_patch_with_provider(
    workspace: &Path,
    policy: FileChangePolicy,
    approval: Arc<dyn ApprovalProvider>,
    provider: Arc<ScriptedProvider>,
) -> Session {
    let registry = Arc::new(WorkspaceToolRegistry::open(workspace).unwrap());
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(registry.schemas().to_vec())
        .unwrap()
        .with_file_change_approval(policy, approval);
    let mut agent = AgentLoop::new(
        Session::new("file-change-test").unwrap(),
        provider,
        registry,
        config,
    )
    .unwrap();
    agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();
    agent.shutdown_into_session().await.unwrap()
}

fn patch_agent(
    id: &str,
    registry: Arc<WorkspaceToolRegistry>,
    patch: &str,
    policy: FileChangePolicy,
    approval: Arc<dyn ApprovalProvider>,
) -> (AgentLoop, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::new(patch));
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(registry.schemas().to_vec())
        .unwrap()
        .with_file_change_approval(policy, approval);
    let agent = AgentLoop::new(
        Session::new(id).unwrap(),
        provider.clone(),
        registry,
        config,
    )
    .unwrap();
    (agent, provider)
}

fn result_facts(session: &Session) -> (Option<ToolFailure>, Option<&JsonValue>, String) {
    session
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::ToolResult {
                message,
                error,
                meta,
                ..
            } => {
                let text = message
                    .content()
                    .iter()
                    .filter_map(ContentBlock::tool_result_content)
                    .flatten()
                    .find_map(|block| block.get("text").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned();
                Some((error.clone(), meta.as_ref(), text))
            }
            _ => None,
        })
        .unwrap()
}

fn result_code(session: &Session) -> Option<&str> {
    session
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::ToolResult {
                error: Some(error), ..
            } => Some(error.code.as_str()),
            _ => None,
        })
}

fn patch_from_before_after(path: &str, before: &str, after: &str, create: bool) -> String {
    let generated = create_patch(before, after).to_string();
    let body = &generated[generated.find("@@ ").unwrap()..];
    format!(
        "{}+++ b/{path}\n{body}",
        if create {
            "--- /dev/null\n".to_owned()
        } else {
            format!("--- a/{path}\n")
        }
    )
}

fn normalized_diff_facts(path: &str, diff: &str) -> Vec<Value> {
    let patch = diffy::Patch::from_str(diff).unwrap();
    patch
        .hunks()
        .iter()
        .map(|hunk| {
            let mut old_text = String::new();
            let mut new_text = String::new();
            for line in hunk.lines() {
                match line {
                    Line::Context(value) => {
                        old_text.push_str(value);
                        new_text.push_str(value);
                    }
                    Line::Delete(value) => old_text.push_str(value),
                    Line::Insert(value) => new_text.push_str(value),
                }
            }
            if old_text.ends_with('\n') {
                old_text.pop();
            }
            if new_text.ends_with('\n') {
                new_text.pop();
            }
            json!({ "path": path, "oldText": old_text, "newText": new_text })
        })
        .collect()
}

fn is_mutation_step_fact(event_type: &str) -> bool {
    matches!(
        event_type,
        "assistant/message"
            | "tool/call"
            | "approval/asked"
            | "approval/decided"
            | "tool/result"
            | "step/end"
    )
}

fn oracle_first_mutation_step_types(events: &[Value]) -> Vec<String> {
    let mut started = false;
    let mut types = Vec::new();
    for event in events {
        let event_type = event["type"].as_str().unwrap();
        started |= event_type == "assistant/message";
        if started && is_mutation_step_fact(event_type) {
            types.push(event_type.to_owned());
        }
        if started && event_type == "step/end" {
            break;
        }
    }
    types
}

fn rust_first_mutation_step_types(session: &Session) -> Vec<String> {
    let mut started = false;
    let mut types = Vec::new();
    for event in session.events() {
        let event_type = event.kind().event_type();
        started |= event_type == "assistant/message";
        if started && is_mutation_step_fact(event_type) {
            types.push(event_type.to_owned());
        }
        if started && event_type == "step/end" {
            break;
        }
    }
    types
}

#[test]
fn workspace_registry_exposes_four_read_tools_then_one_closed_apply_patch_schema() {
    let workspace = TempWorkspace::new("schema");
    let registry = WorkspaceToolRegistry::open(workspace.path()).unwrap();
    assert_eq!(
        registry
            .schemas()
            .iter()
            .map(|schema| schema.name())
            .collect::<Vec<_>>(),
        ["list", "glob", "grep", "read", "apply_patch"]
    );
    let parameters = registry.schemas()[4].parameters().as_value();
    assert_eq!(parameters["required"], json!(["patch"]));
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(
        parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["patch"]
    );
}

#[tokio::test]
async fn canonical_file_changes_match_the_committed_phase5_oracle_scope() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/tools/upstream_phase5_oracle.json")).unwrap();
    assert_eq!(oracle["schemaVersion"], 1);
    assert_eq!(
        oracle["upstream"]["commit"],
        "47f943859bef60e4160492346772ded9b24f765a"
    );
    assert_eq!(
        oracle["toolSurface"]["registeredNames"],
        json!(["edit", "read", "write"])
    );
    assert_eq!(oracle["toolSurface"]["applyPatchPresent"], false);
    assert_eq!(
        oracle["observationFailures"]["unobservedWrite"]["result"]["error"]["info"]["code"],
        "FS_NOT_OBSERVED"
    );
    assert_eq!(
        oracle["observationFailures"]["staleWrite"]["result"]["error"]["info"]["code"],
        "FS_STALE_VERSION"
    );
    for pointer in [
        "/windowedObservation/checks/oneLineReadSucceeded",
        "/windowedObservation/checks/editOutsideWindowAuthorized",
        "/windowedObservation/checks/wholeFileVersionNotWindowCoverage",
        "/lastWindowOverwrite/checks/competitorInjected",
        "/lastWindowOverwrite/checks/guardedCallStillSucceeded",
        "/lastWindowOverwrite/checks/finalRenameOverwroteLastWindowCompetitor",
    ] {
        assert_eq!(
            oracle.pointer(pointer),
            Some(&Value::Bool(true)),
            "{pointer}"
        );
    }

    for (name, create) in [
        ("writeCreate", true),
        ("readThenWriteUpdate", false),
        ("uniqueEdit", false),
        ("replaceAllDiff", false),
    ] {
        let scenario = &oracle["canonicalMutations"][name];
        let path = scenario["input"]["file_path"].as_str().unwrap();
        let before = scenario["initial"].as_str().unwrap_or_default();
        let after = scenario["diskAfter"].as_str().unwrap();
        let patch = patch_from_before_after(path, before, after, create);
        let workspace = TempWorkspace::new(name);
        if !create {
            workspace.write(path, before);
        }
        let requests = Arc::new(Mutex::new(Vec::new()));
        let session = run_patch(
            workspace.path(),
            &patch,
            FileChangePolicy::Ask,
            Arc::new(RecordingApproval {
                outcome: ApprovalOutcome::AllowedOnce,
                requests: requests.clone(),
            }),
        )
        .await;

        assert_eq!(scenario["result"]["isError"], false, "scenario={name}");
        assert_eq!(
            fs::read_to_string(workspace.path().join(path)).unwrap(),
            after
        );
        let (error, meta, _) = result_facts(&session);
        assert!(error.is_none(), "scenario={name}");
        let meta = meta.unwrap().as_value();
        assert_eq!(meta["path"], path, "scenario={name}");
        assert_eq!(
            meta["operation"],
            if create { "create" } else { "update" },
            "scenario={name}"
        );
        assert_eq!(meta["committed"], true, "scenario={name}");
        let approved = requests.lock().unwrap();
        assert_eq!(approved.len(), 1, "scenario={name}");
        assert_eq!(approved[0].preview(), meta["diff"].as_str().unwrap());

        let rust_diffs = normalized_diff_facts(path, meta["diff"].as_str().unwrap());
        let upstream_diffs = scenario["result"]["meta"]["diffs"].as_array().unwrap();
        if create {
            assert!(upstream_diffs.is_empty());
            assert_eq!(rust_diffs.len(), 1);
        } else {
            assert_eq!(&rust_diffs, upstream_diffs, "scenario={name}");
        }
    }
}

#[tokio::test]
async fn approval_order_and_side_effects_match_the_committed_phase5_oracle_scope() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/tools/upstream_phase5_oracle.json")).unwrap();

    for name in ["defaultAllow", "deny", "askAllowed", "askRejected"] {
        let scenario = &oracle["approvalPipeline"][name];
        let events = scenario["events"].as_array().unwrap();
        let upstream_call = events
            .iter()
            .find(|event| event["type"] == "tool/call")
            .unwrap();
        let arguments: Value =
            serde_json::from_str(upstream_call["data"]["arguments"].as_str().unwrap()).unwrap();
        let path = arguments["file_path"].as_str().unwrap();
        let requested = arguments["content"].as_str().unwrap();
        let patch = patch_from_before_after(path, "", requested, true);
        let (policy, answer) = match name {
            "defaultAllow" => (FileChangePolicy::Allow, ApprovalOutcome::Unavailable),
            "deny" => (FileChangePolicy::Deny, ApprovalOutcome::Unavailable),
            "askAllowed" => (FileChangePolicy::Ask, ApprovalOutcome::AllowedOnce),
            "askRejected" => (FileChangePolicy::Ask, ApprovalOutcome::Rejected),
            _ => unreachable!(),
        };
        let workspace = TempWorkspace::new(name);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let approvals = Arc::new(RecordingApproval {
            outcome: answer,
            requests: requests.clone(),
        });
        let provider = Arc::new(ScriptedProvider::new(&patch));
        let session =
            run_patch_with_provider(workspace.path(), policy, approvals, provider.clone()).await;

        assert_eq!(
            rust_first_mutation_step_types(&session),
            oracle_first_mutation_step_types(events),
            "scenario={name}"
        );
        assert_eq!(
            u64::try_from(requests.lock().unwrap().len()).unwrap(),
            scenario["answererCalls"].as_u64().unwrap(),
            "scenario={name}"
        );
        assert_eq!(
            provider.dispatch_count(),
            scenario["dispatchCount"].as_u64().unwrap(),
            "scenario={name}"
        );
        assert_eq!(
            result_facts(&session).0.is_some(),
            scenario["resultIsError"].as_bool().unwrap(),
            "scenario={name}"
        );
        match scenario["diskAfter"].as_str() {
            Some(expected) => {
                assert_eq!(
                    fs::read_to_string(workspace.path().join(path)).unwrap(),
                    expected
                )
            }
            None => assert!(!workspace.path().join(path).exists()),
        }

        let call_event = session
            .events()
            .iter()
            .find(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
            .unwrap();
        let EventKind::ToolCall { call_id, .. } = call_event.kind() else {
            unreachable!()
        };
        let result_event = session
            .events()
            .iter()
            .find(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
            .unwrap();
        let [source] = result_event.source_event_seqs().unwrap() else {
            panic!("scenario {name} result must cite exactly one call")
        };
        assert!(std::ptr::eq(
            call_event,
            &session.events()[usize::try_from(source.get()).unwrap()]
        ));
        let EventKind::ToolResult { message, .. } = result_event.kind() else {
            unreachable!()
        };
        let result_call_id = message
            .content()
            .iter()
            .find_map(|block| match block.kind() {
                ContentBlockKind::ToolResult { tool_call_id, .. } => Some(tool_call_id),
                _ => None,
            })
            .unwrap();
        assert_eq!(result_call_id, call_id);

        let asked = session
            .events()
            .iter()
            .find_map(|event| match event.kind() {
                EventKind::ApprovalAsked { asked } => Some(asked),
                _ => None,
            });
        let decided = session
            .events()
            .iter()
            .find_map(|event| match event.kind() {
                EventKind::ApprovalDecided { decided } => Some(decided),
                _ => None,
            });
        if let Some(upstream_decided) = events
            .iter()
            .find(|event| event["type"] == "approval/decided")
        {
            let asked = asked.unwrap();
            let decided = decided.unwrap();
            assert_eq!(asked.id(), decided.id());
            assert_eq!(asked.call_id(), Some(call_id));
            assert_eq!(
                decided.outcome(),
                match upstream_decided["data"]["outcome"].as_str().unwrap() {
                    "allowed-once" => ApprovalOutcome::AllowedOnce,
                    "rejected" => ApprovalOutcome::Rejected,
                    other => panic!("unexpected upstream approval outcome {other}"),
                }
            );
        } else {
            assert!(asked.is_none());
            assert!(decided.is_none());
        }
    }
}

#[tokio::test]
async fn approval_outcomes_match_the_committed_phase7_oracle_scope() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/cli/upstream_phase7_oracle.json")).unwrap();
    assert_eq!(oracle["schemaVersion"], 1);
    assert_eq!(
        oracle["upstream"]["commit"],
        "47f943859bef60e4160492346772ded9b24f765a"
    );

    for name in ["allow", "reject", "cancel"] {
        let scenario = &oracle["scenarios"]["approval"][name];
        assert!(
            scenario["checks"]
                .as_object()
                .unwrap()
                .values()
                .all(|check| check.as_bool() == Some(true))
        );
        let expected_events = scenario["relevantDurableEvents"].as_array().unwrap();
        let call_id = expected_events[0]["data"]["callId"].as_str().unwrap();
        assert_eq!(call_id, format!("call-{name}"));
        assert_eq!(expected_events[0]["data"]["name"], "sentinel");
        assert_eq!(expected_events[0]["data"]["arguments"], "{}");
        assert_eq!(
            expected_events[1]["data"]["reason"],
            format!("Phase 7 {name} oracle")
        );
        assert_eq!(expected_events[1]["data"]["toolName"], "sentinel");
        assert_eq!(expected_events[1]["data"]["callId"], call_id);
        assert_eq!(
            expected_events[3]["sourceEventSeqs"],
            json!([expected_events[0]["seq"].as_u64().unwrap()])
        );
        assert_eq!(expected_events[3]["surfaceOp"], "append");
        let final_text = scenario["wireUpdates"][0]["content"]["text"]
            .as_str()
            .unwrap();
        let relative_path = scenario["sideEffect"]["path"]
            .as_str()
            .unwrap()
            .rsplit('/')
            .next()
            .unwrap();
        assert_eq!(
            scenario["sideEffect"]["path"],
            format!("<workspace>/approval-{name}.txt")
        );
        let requested = format!("{name}\n");
        let patch = patch_from_before_after(relative_path, "", &requested, true);
        let expected_outcome = match expected_events[2]["data"]["outcome"].as_str().unwrap() {
            "allowed-once" => ApprovalOutcome::AllowedOnce,
            "rejected" => ApprovalOutcome::Rejected,
            "cancelled" => ApprovalOutcome::Cancelled,
            other => panic!("unexpected Phase 7 approval outcome {other}"),
        };
        let expected_is_error = expected_events[3]["data"]["message"]["content"][0]["isError"]
            .as_bool()
            .unwrap();

        let workspace = TempWorkspace::new(&format!("phase7-{name}"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let approval = Arc::new(RecordingApproval {
            outcome: expected_outcome,
            requests: requests.clone(),
        });
        let provider = Arc::new(ScriptedProvider::with_response(call_id, &patch, final_text));
        let session = run_patch_with_provider(
            workspace.path(),
            FileChangePolicy::Ask,
            approval,
            provider.clone(),
        )
        .await;

        let relevant = session
            .events()
            .iter()
            .filter(|event| {
                matches!(
                    event.kind(),
                    EventKind::ToolCall { .. }
                        | EventKind::ApprovalAsked { .. }
                        | EventKind::ApprovalDecided { .. }
                        | EventKind::ToolResult { .. }
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            relevant
                .iter()
                .map(|event| event.kind().event_type())
                .collect::<Vec<_>>(),
            expected_events
                .iter()
                .map(|event| event["type"].as_str().unwrap())
                .collect::<Vec<_>>(),
            "scenario={name}: approval audit order"
        );

        let EventKind::ToolCall {
            turn,
            step,
            call_id: actual_call_id,
            name: actual_tool_name,
            arguments,
        } = relevant[0].kind()
        else {
            unreachable!()
        };
        assert_eq!(actual_call_id.as_str(), call_id, "scenario={name}");
        assert_eq!(actual_tool_name, "apply_patch", "scenario={name}");
        assert_eq!(
            turn.get(),
            expected_events[0]["data"]["turn"].as_u64().unwrap()
        );
        assert_eq!(
            step.get(),
            expected_events[0]["data"]["step"].as_u64().unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Value>(arguments).unwrap(),
            json!({ "patch": patch }),
            "scenario={name}"
        );

        let EventKind::ApprovalAsked { asked } = relevant[1].kind() else {
            unreachable!()
        };
        let EventKind::ApprovalDecided { decided } = relevant[2].kind() else {
            unreachable!()
        };
        assert_eq!(asked.id(), decided.id(), "scenario={name}");
        assert_eq!(asked.call_id(), Some(actual_call_id), "scenario={name}");
        assert_eq!(asked.tool_name(), actual_tool_name, "scenario={name}");
        assert_eq!(
            asked.reason(),
            Some(format!("Create workspace file `{relative_path}`").as_str()),
            "scenario={name}"
        );
        assert_eq!(decided.outcome(), expected_outcome, "scenario={name}");

        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 1, "scenario={name}");
        assert_eq!(recorded[0].id(), asked.id(), "scenario={name}");
        assert_eq!(recorded[0].call_id(), actual_call_id, "scenario={name}");
        assert_eq!(recorded[0].tool_name(), actual_tool_name, "scenario={name}");
        assert_eq!(recorded[0].reason(), asked.reason(), "scenario={name}");

        let result = relevant[3];
        assert_eq!(
            result.source_event_seqs(),
            Some([relevant[0].seq()].as_slice()),
            "scenario={name}: result must cite its durable call"
        );
        let EventKind::ToolResult { message, error, .. } = result.kind() else {
            unreachable!()
        };
        assert_eq!(error.is_some(), expected_is_error, "scenario={name}");
        assert!(matches!(
            message.source().kind(),
            deepseek_harness_cli::model::MessageSourceKind::Tool { call_id: result_call }
                if result_call == actual_call_id
        ));

        let (failure, meta, result_text) = result_facts(&session);
        let expected_rust_text = match name {
            "allow" => format!("Created workspace file `{relative_path}`."),
            "reject" => "Error: the file change was rejected".to_owned(),
            "cancel" => "Error: the approval request was cancelled".to_owned(),
            _ => unreachable!(),
        };
        assert_eq!(result_text, expected_rust_text, "scenario={name}");
        let expected_upstream_text = match name {
            "allow" => "sentinel allow".to_owned(),
            "reject" => "Error: the user rejected tool \"sentinel\"".to_owned(),
            "cancel" => "Error: approval for tool \"sentinel\" was cancelled".to_owned(),
            _ => unreachable!(),
        };
        assert_eq!(
            expected_events[3]["data"]["message"]["content"][0]["content"][0]["text"],
            expected_upstream_text,
            "scenario={name}: pinned upstream sentinel result"
        );
        assert_eq!(
            failure.as_ref().map(|failure| failure.code.as_str()),
            match name {
                "allow" => None,
                "reject" => Some("APPROVAL_REJECTED"),
                "cancel" => Some("APPROVAL_CANCELLED"),
                _ => unreachable!(),
            },
            "scenario={name}"
        );
        let meta = meta.unwrap().as_value();
        assert_eq!(meta["path"], relative_path, "scenario={name}");
        assert_eq!(meta["operation"], "create", "scenario={name}");
        assert_eq!(meta["committed"], name == "allow", "scenario={name}");
        assert_eq!(recorded[0].preview(), meta["diff"].as_str().unwrap());
        drop(recorded);
        let [result_block] = message.content() else {
            panic!("scenario={name}: tool result must contain one block")
        };
        assert!(matches!(
            result_block.kind(),
            ContentBlockKind::ToolResult { tool_call_id, is_error }
                if tool_call_id == actual_call_id
                    && is_error.unwrap_or(false) == expected_is_error
        ));

        match scenario["sideEffect"]["diskAfter"].as_str() {
            Some(expected) => assert_eq!(
                fs::read_to_string(workspace.path().join(relative_path)).unwrap(),
                expected,
                "scenario={name}"
            ),
            None => assert!(
                !workspace.path().join(relative_path).exists(),
                "scenario={name}: denied approval must not create the file"
            ),
        }
        assert_eq!(provider.dispatch_count(), 2, "scenario={name}");
        let provider_requests = provider.requests();
        assert_eq!(provider_requests.len(), 2, "scenario={name}");
        let tool_results_per_request = provider_requests
            .iter()
            .map(|messages| {
                messages
                    .iter()
                    .flat_map(Message::content)
                    .filter(|block| matches!(block.kind(), ContentBlockKind::ToolResult { .. }))
                    .count()
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_results_per_request, [0, 1], "scenario={name}");
        assert!(provider_requests[1].iter().any(|message| {
            matches!(
                message.source().kind(),
                deepseek_harness_cli::model::MessageSourceKind::Tool { call_id: result_call }
                    if result_call.as_str() == call_id
            ) && message.content().iter().any(|block| {
                matches!(
                    block.kind(),
                    ContentBlockKind::ToolResult { tool_call_id, is_error }
                        if tool_call_id.as_str() == call_id
                            && is_error.unwrap_or(false) == expected_is_error
                )
            })
        }));
        let assistant_texts = session
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                EventKind::AssistantMessage { message, .. } => {
                    let text = message
                        .content()
                        .iter()
                        .filter_map(|block| match block.kind() {
                            ContentBlockKind::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>();
                    (!text.is_empty()).then_some(text)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(assistant_texts, [final_text.to_owned()]);
        assert!(matches!(
            session
                .events()
                .iter()
                .find_map(|event| match event.kind() {
                    EventKind::TurnEnd { reason, .. } => Some(reason),
                    _ => None,
                }),
            Some(TurnEndReason::Completed)
        ));
        assert_eq!(scenario["promptResponse"]["stopReason"], "end_turn");
    }
}

#[tokio::test]
async fn default_rust_file_policy_asks_and_fails_closed_without_a_ui() {
    let workspace = TempWorkspace::new("default-ask");
    let patch = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+new\n";
    let registry = Arc::new(WorkspaceToolRegistry::open(workspace.path()).unwrap());
    let provider = Arc::new(ScriptedProvider::new(patch));
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(registry.schemas().to_vec())
        .unwrap();
    let mut agent = AgentLoop::new(
        Session::new("default-ask").unwrap(),
        provider.clone(),
        registry,
        config,
    )
    .unwrap();
    agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert!(!workspace.path().join("new.txt").exists());
    assert_eq!(result_code(agent.session()), Some("APPROVAL_UNAVAILABLE"));
    assert_eq!(provider.dispatch_count(), 2);
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ApprovalDecided { decided }
            if decided.outcome() == ApprovalOutcome::Unavailable
    )));
}

#[tokio::test]
async fn create_commit_matches_the_exact_preview_metadata_and_final_bytes() {
    let workspace = TempWorkspace::new("create");
    let patch = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1,2 @@\n+hello\n+world\n";
    let session = run_patch(
        workspace.path(),
        patch,
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;

    assert_eq!(
        fs::read(workspace.path().join("new.txt")).unwrap(),
        b"hello\nworld\n"
    );
    assert_eq!(
        fs::metadata(workspace.path().join("new.txt"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let (error, meta, text) = result_facts(&session);
    assert!(error.is_none());
    assert!(text.contains("Created"));
    let meta = meta.unwrap().as_value();
    assert_eq!(meta["path"], "new.txt");
    assert_eq!(meta["operation"], "create");
    assert_eq!(meta["diff"], patch);
    assert_eq!(meta["committed"], true);
}

#[tokio::test]
async fn update_preview_is_regenerated_with_three_lines_of_context() {
    let workspace = TempWorkspace::new("canonical-context");
    workspace.write(
        "lines.txt",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
    );
    let patch = "--- a/lines.txt\n+++ b/lines.txt\n@@ -5 +5 @@\n-five\n+FIVE\n";
    let session = run_patch(
        workspace.path(),
        patch,
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;

    assert_eq!(
        fs::read_to_string(workspace.path().join("lines.txt")).unwrap(),
        "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\n"
    );
    let expected = "--- a/lines.txt\n+++ b/lines.txt\n@@ -2,7 +2,7 @@\n two\n three\n four\n-five\n+FIVE\n six\n seven\n eight\n";
    assert_eq!(
        result_facts(&session).1.unwrap().as_value()["diff"],
        expected
    );
}

#[tokio::test]
async fn real_registry_asks_with_the_same_canonical_diff_it_persists() {
    let workspace = TempWorkspace::new("approval-preview");
    workspace.write(
        "lines.txt",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
    );
    let patch = "--- a/lines.txt\n+++ b/lines.txt\n@@ -5 +5 @@\n-five\n+FIVE\n";
    let requests = Arc::new(Mutex::new(Vec::new()));
    let session = run_patch(
        workspace.path(),
        patch,
        FileChangePolicy::Ask,
        Arc::new(RecordingApproval {
            outcome: ApprovalOutcome::AllowedOnce,
            requests: requests.clone(),
        }),
    )
    .await;

    let expected = "--- a/lines.txt\n+++ b/lines.txt\n@@ -2,7 +2,7 @@\n two\n three\n four\n-five\n+FIVE\n six\n seven\n eight\n";
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].preview(), expected);
    assert_eq!(
        result_facts(&session).1.unwrap().as_value()["diff"],
        expected
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("lines.txt")).unwrap(),
        "one\ntwo\nthree\nfour\nFIVE\nsix\nseven\neight\nnine\n"
    );
}

#[tokio::test]
async fn a_no_op_patch_never_asks_stages_or_changes_file_metadata() {
    let workspace = TempWorkspace::new("no-op");
    workspace.write("file.txt", "same\n");
    let target = workspace.path().join("file.txt");
    let before = fs::metadata(&target).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));

    let session = run_patch(
        workspace.path(),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n same\n",
        FileChangePolicy::Ask,
        Arc::new(RecordingApproval {
            outcome: ApprovalOutcome::AllowedOnce,
            requests: requests.clone(),
        }),
    )
    .await;

    let after = fs::metadata(&target).unwrap();
    assert_eq!(result_code(&session), Some("NO_CHANGES"));
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(fs::read(&target).unwrap(), b"same\n");
    assert_eq!(after.ino(), before.ino());
    assert_eq!(after.len(), before.len());
    assert_eq!(after.mode(), before.mode());
    assert_eq!(after.mtime(), before.mtime());
    assert_eq!(after.mtime_nsec(), before.mtime_nsec());
    assert!(!fs::read_dir(workspace.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".dsh-stage-")
    }));
}

#[tokio::test]
async fn rejection_and_late_update_conflict_never_modify_the_winning_file() {
    let update = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";

    let rejected = TempWorkspace::new("rejected");
    rejected.write("file.txt", "old\n");
    let rejected_session = run_patch(
        rejected.path(),
        update,
        FileChangePolicy::Ask,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Rejected,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert_eq!(
        fs::read_to_string(rejected.path().join("file.txt")).unwrap(),
        "old\n"
    );
    assert_eq!(
        result_facts(&rejected_session).0.unwrap().code,
        "APPROVAL_REJECTED"
    );

    let conflict = TempWorkspace::new("conflict");
    conflict.write("file.txt", "old\n");
    let conflict_path = conflict.path().join("file.txt");
    let conflict_session = run_patch(
        conflict.path(),
        update,
        FileChangePolicy::Ask,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::AllowedOnce,
            mutate_before_answer: Some((conflict_path.clone(), "external winner\n".to_owned())),
        }),
    )
    .await;
    assert_eq!(
        fs::read_to_string(conflict_path).unwrap(),
        "external winner\n"
    );
    assert_eq!(
        result_facts(&conflict_session).0.unwrap().code,
        "FILE_CONFLICT"
    );

    let create_race = TempWorkspace::new("create-race");
    let create_path = create_race.path().join("new.txt");
    let create = "--- /dev/null\n+++ b/new.txt\n@@ -0,0 +1 @@\n+agent\n";
    let race_session = run_patch(
        create_race.path(),
        create,
        FileChangePolicy::Ask,
        Arc::new(ActionApproval::new({
            let create_path = create_path.clone();
            move || fs::write(create_path, "external winner\n").unwrap()
        })),
    )
    .await;
    assert_eq!(
        fs::read_to_string(create_path).unwrap(),
        "external winner\n"
    );
    let (error, meta, _) = result_facts(&race_session);
    assert_eq!(error.unwrap().code, "FILE_ALREADY_EXISTS");
    assert_eq!(meta.unwrap().as_value()["committed"], false);
}

#[tokio::test]
async fn malformed_multi_file_and_traversal_patches_leave_disk_unchanged() {
    let workspace = TempWorkspace::new("invalid");
    workspace.write("file.txt", "old\n");
    for patch in [
        "not a unified diff",
        "diff --git a/file.txt b/file.txt\nGIT binary patch\nliteral 1\nA\n",
        "diff --git a/file.txt b/file.txt\nold mode 100644\nnew mode 100755\n",
        "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-wrong\n+new\n",
        "--- /dev/null\n+++ b/../outside.txt\n@@ -0,0 +1 @@\n+secret\n",
        "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n--- /dev/null\n+++ b/second.txt\n@@ -0,0 +1 @@\n+second\n",
    ] {
        let session = run_patch(
            workspace.path(),
            patch,
            FileChangePolicy::Allow,
            Arc::new(FixedApproval {
                outcome: ApprovalOutcome::Unavailable,
                mutate_before_answer: None,
            }),
        )
        .await;
        assert_eq!(
            fs::read_to_string(workspace.path().join("file.txt")).unwrap(),
            "old\n"
        );
        assert!(result_facts(&session).0.is_some());
    }
    assert!(
        !workspace
            .path()
            .parent()
            .unwrap()
            .join("outside.txt")
            .exists()
    );
}

#[tokio::test]
async fn mutation_paths_reject_symlinks_hardlinks_and_lexical_aliases() {
    let workspace = TempWorkspace::new("path-security");
    let outside = TempWorkspace::new("path-security-outside");
    fs::create_dir(workspace.path().join("real")).unwrap();
    workspace.write("real/inside.txt", "inside\n");
    workspace.write("final-target.txt", "final\n");
    workspace.write("hard-target.txt", "hard\n");
    outside.write("secret.txt", "outside\n");
    symlink("real", workspace.path().join("dir-link")).unwrap();
    symlink("final-target.txt", workspace.path().join("final-link.txt")).unwrap();
    symlink("missing.txt", workspace.path().join("broken-link.txt")).unwrap();
    symlink("cycle-b.txt", workspace.path().join("cycle-a.txt")).unwrap();
    symlink("cycle-a.txt", workspace.path().join("cycle-b.txt")).unwrap();
    symlink(outside.path(), workspace.path().join("outside-dir-link")).unwrap();
    symlink(
        outside.path().join("secret.txt"),
        workspace.path().join("outside-file-link.txt"),
    )
    .unwrap();
    fs::hard_link(
        workspace.path().join("hard-target.txt"),
        workspace.path().join("hard-alias.txt"),
    )
    .unwrap();

    for (path, old, expected_code) in [
        ("dir-link/inside.txt", "inside", "WORKSPACE_PATH_DENIED"),
        ("final-link.txt", "final", "WORKSPACE_PATH_DENIED"),
        ("broken-link.txt", "missing", "WORKSPACE_PATH_DENIED"),
        ("cycle-a.txt", "cycle", "WORKSPACE_PATH_DENIED"),
        (
            "outside-dir-link/secret.txt",
            "outside",
            "WORKSPACE_PATH_DENIED",
        ),
        ("outside-file-link.txt", "outside", "WORKSPACE_PATH_DENIED"),
        ("hard-alias.txt", "hard", "FILE_HARDLINK_DENIED"),
        ("real/./inside.txt", "inside", "WORKSPACE_PATH_DENIED"),
        ("real//inside.txt", "inside", "WORKSPACE_PATH_DENIED"),
    ] {
        let patch = format!("--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-{old}\n+changed\n");
        let session = run_patch(
            workspace.path(),
            &patch,
            FileChangePolicy::Allow,
            Arc::new(FixedApproval {
                outcome: ApprovalOutcome::Unavailable,
                mutate_before_answer: None,
            }),
        )
        .await;
        assert_eq!(result_code(&session), Some(expected_code), "path={path}");
    }

    assert_eq!(
        fs::read_to_string(workspace.path().join("real/inside.txt")).unwrap(),
        "inside\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("final-target.txt")).unwrap(),
        "final\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("hard-target.txt")).unwrap(),
        "hard\n"
    );
    assert_eq!(
        fs::read_to_string(workspace.path().join("hard-alias.txt")).unwrap(),
        "hard\n"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "outside\n"
    );
}

#[tokio::test]
async fn late_mode_hardlink_and_parent_identity_changes_are_conflicts() {
    let update = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";

    let mode = TempWorkspace::new("mode-conflict");
    mode.write("file.txt", "old\n");
    let mode_path = mode.path().join("file.txt");
    let session = run_patch(
        mode.path(),
        update,
        FileChangePolicy::Ask,
        Arc::new(ActionApproval::new({
            let mode_path = mode_path.clone();
            move || fs::set_permissions(&mode_path, fs::Permissions::from_mode(0o600)).unwrap()
        })),
    )
    .await;
    assert_eq!(result_code(&session), Some("FILE_CONFLICT"));
    assert_eq!(fs::read_to_string(&mode_path).unwrap(), "old\n");
    assert_eq!(
        fs::metadata(&mode_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let hardlink = TempWorkspace::new("late-hardlink");
    hardlink.write("file.txt", "old\n");
    let hardlink_path = hardlink.path().join("file.txt");
    let alias_path = hardlink.path().join("alias.txt");
    let session = run_patch(
        hardlink.path(),
        update,
        FileChangePolicy::Ask,
        Arc::new(ActionApproval::new({
            let hardlink_path = hardlink_path.clone();
            let alias_path = alias_path.clone();
            move || fs::hard_link(hardlink_path, alias_path).unwrap()
        })),
    )
    .await;
    assert_eq!(result_code(&session), Some("FILE_CONFLICT"));
    assert_eq!(fs::read_to_string(&hardlink_path).unwrap(), "old\n");
    assert_eq!(fs::read_to_string(&alias_path).unwrap(), "old\n");

    let parent = TempWorkspace::new("parent-conflict");
    fs::create_dir(parent.path().join("dir")).unwrap();
    parent.write("dir/file.txt", "old\n");
    let moved = parent.path().join("moved");
    let replacement = parent.path().join("dir");
    let parent_patch = "--- a/dir/file.txt\n+++ b/dir/file.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let session = run_patch(
        parent.path(),
        parent_patch,
        FileChangePolicy::Ask,
        Arc::new(ActionApproval::new({
            let moved = moved.clone();
            let replacement = replacement.clone();
            move || {
                fs::rename(&replacement, &moved).unwrap();
                fs::create_dir(&replacement).unwrap();
                fs::write(replacement.join("file.txt"), "replacement\n").unwrap();
            }
        })),
    )
    .await;
    assert_eq!(result_code(&session), Some("FILE_CONFLICT"));
    assert_eq!(fs::read_to_string(moved.join("file.txt")).unwrap(), "old\n");
    assert_eq!(
        fs::read_to_string(replacement.join("file.txt")).unwrap(),
        "replacement\n"
    );
}

#[tokio::test]
async fn updates_preserve_homogeneous_crlf_and_reject_mixed_line_endings() {
    let crlf = TempWorkspace::new("crlf");
    fs::write(crlf.path().join("file.txt"), b"one\r\ntwo\r\n").unwrap();
    let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -2 +2 @@\n-two\n+TWO\n";
    let session = run_patch(
        crlf.path(),
        patch,
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert!(result_code(&session).is_none());
    assert_eq!(
        fs::read(crlf.path().join("file.txt")).unwrap(),
        b"one\r\nTWO\r\n"
    );

    let mixed = TempWorkspace::new("mixed-lines");
    fs::write(mixed.path().join("file.txt"), b"one\r\ntwo\n").unwrap();
    let session = run_patch(
        mixed.path(),
        patch,
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert_eq!(result_code(&session), Some("FILE_NOT_TEXT"));
    assert_eq!(
        fs::read(mixed.path().join("file.txt")).unwrap(),
        b"one\r\ntwo\n"
    );

    let no_final_newline = TempWorkspace::new("no-final-newline");
    fs::write(no_final_newline.path().join("file.txt"), b"old").unwrap();
    let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\\ No newline at end of file\n";
    let session = run_patch(
        no_final_newline.path(),
        patch,
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert!(result_code(&session).is_none());
    assert_eq!(
        fs::read(no_final_newline.path().join("file.txt")).unwrap(),
        b"new"
    );
}

#[tokio::test]
async fn updates_preserve_ordinary_modes_and_strip_special_bits() {
    for (initial_mode, expected_mode) in [(0o640, 0o640), (0o755, 0o755), (0o6755, 0o755)] {
        let workspace = TempWorkspace::new("update-mode");
        let path = workspace.path().join("file.txt");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(initial_mode)).unwrap();
        let session = run_patch(
            workspace.path(),
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            FileChangePolicy::Allow,
            Arc::new(FixedApproval {
                outcome: ApprovalOutcome::Unavailable,
                mutate_before_answer: None,
            }),
        )
        .await;

        assert!(result_code(&session).is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            expected_mode
        );
    }
}

#[tokio::test]
async fn invalid_text_non_regular_and_missing_targets_fail_without_writes() {
    for (label, bytes, expected_code) in [
        ("invalid-utf8", vec![0xff], "FILE_NOT_TEXT"),
        ("nul", b"old\0\n".to_vec(), "FILE_NOT_TEXT"),
    ] {
        let workspace = TempWorkspace::new(label);
        let path = workspace.path().join("file.txt");
        fs::write(&path, &bytes).unwrap();
        let session = run_patch(
            workspace.path(),
            "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n",
            FileChangePolicy::Allow,
            Arc::new(FixedApproval {
                outcome: ApprovalOutcome::Unavailable,
                mutate_before_answer: None,
            }),
        )
        .await;
        assert_eq!(result_code(&session), Some(expected_code));
        assert_eq!(fs::read(path).unwrap(), bytes);
    }

    let directory = TempWorkspace::new("directory-target");
    fs::create_dir(directory.path().join("target")).unwrap();
    let session = run_patch(
        directory.path(),
        "--- a/target\n+++ b/target\n@@ -1 +1 @@\n-old\n+new\n",
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert_eq!(result_code(&session), Some("FILE_NOT_REGULAR"));
    assert!(directory.path().join("target").is_dir());

    let socket = TempWorkspace::new_short("sock");
    let socket_path = socket.path().join("target");
    let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
    let session = run_patch(
        socket.path(),
        "--- a/target\n+++ b/target\n@@ -1 +1 @@\n-old\n+new\n",
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert_eq!(result_code(&session), Some("FILE_NOT_REGULAR"));
    assert!(socket_path.exists());
    drop(listener);

    let missing = TempWorkspace::new("missing-targets");
    let update = run_patch(
        missing.path(),
        "--- a/missing.txt\n+++ b/missing.txt\n@@ -1 +1 @@\n-old\n+new\n",
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert_eq!(result_code(&update), Some("FILE_NOT_FOUND"));
    let create = run_patch(
        missing.path(),
        "--- /dev/null\n+++ b/missing/new.txt\n@@ -0,0 +1 @@\n+new\n",
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert_eq!(result_code(&create), Some("FS_NOT_FOUND"));
    assert!(!missing.path().join("missing").exists());
}

#[tokio::test]
async fn mutation_file_size_accepts_sixteen_mib_and_rejects_one_byte_more() {
    const LIMIT: usize = 16 * 1024 * 1024;
    let patch = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";

    let exact = TempWorkspace::new("file-size-exact");
    let exact_path = exact.path().join("file.txt");
    fs::write(&exact_path, mutation_file_with_size(LIMIT)).unwrap();
    let session = run_patch(
        exact.path(),
        patch,
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert!(result_code(&session).is_none());
    let exact_after = fs::read(&exact_path).unwrap();
    assert_eq!(exact_after.len(), LIMIT);
    assert!(exact_after.starts_with(b"new\n"));

    let over = TempWorkspace::new("file-size-over");
    let over_path = over.path().join("file.txt");
    let original = mutation_file_with_size(LIMIT + 1);
    fs::write(&over_path, &original).unwrap();
    let session = run_patch(
        over.path(),
        patch,
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    assert_eq!(result_code(&session), Some("FILE_TOO_LARGE"));
    assert_eq!(fs::read(over_path).unwrap(), original);
}

#[tokio::test]
async fn two_prepared_plans_from_one_baseline_allow_only_one_publication() {
    let updates = TempWorkspace::new("two-update-plans");
    updates.write("file.txt", "old\n");
    let registry = Arc::new(WorkspaceToolRegistry::open(updates.path()).unwrap());
    let barrier = Arc::new(Barrier::new(2));
    let (mut first, first_provider) = patch_agent(
        "two-update-first",
        registry.clone(),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+first\n",
        FileChangePolicy::Ask,
        Arc::new(BarrierApproval {
            barrier: barrier.clone(),
        }),
    );
    let (mut second, second_provider) = patch_agent(
        "two-update-second",
        registry,
        "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+second\n",
        FileChangePolicy::Ask,
        Arc::new(BarrierApproval { barrier }),
    );
    let (first_outcome, second_outcome) = tokio::join!(
        first.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new()),
        second.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new()),
    );
    first_outcome.unwrap();
    second_outcome.unwrap();

    let update_codes = [result_code(first.session()), result_code(second.session())];
    assert_eq!(update_codes.iter().filter(|code| code.is_none()).count(), 1);
    assert_eq!(
        update_codes
            .iter()
            .filter(|code| **code == Some("FILE_CONFLICT"))
            .count(),
        1
    );
    assert!(matches!(
        fs::read_to_string(updates.path().join("file.txt"))
            .unwrap()
            .as_str(),
        "first\n" | "second\n"
    ));
    assert_eq!(first_provider.dispatch_count(), 2);
    assert_eq!(second_provider.dispatch_count(), 2);

    let creates = TempWorkspace::new("two-create-plans");
    let registry = Arc::new(WorkspaceToolRegistry::open(creates.path()).unwrap());
    let barrier = Arc::new(Barrier::new(2));
    let (mut first, _) = patch_agent(
        "two-create-first",
        registry.clone(),
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1 @@\n+first\n",
        FileChangePolicy::Ask,
        Arc::new(BarrierApproval {
            barrier: barrier.clone(),
        }),
    );
    let (mut second, _) = patch_agent(
        "two-create-second",
        registry,
        "--- /dev/null\n+++ b/file.txt\n@@ -0,0 +1 @@\n+second\n",
        FileChangePolicy::Ask,
        Arc::new(BarrierApproval { barrier }),
    );
    let (first_outcome, second_outcome) = tokio::join!(
        first.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new()),
        second.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new()),
    );
    first_outcome.unwrap();
    second_outcome.unwrap();

    let create_codes = [result_code(first.session()), result_code(second.session())];
    assert_eq!(create_codes.iter().filter(|code| code.is_none()).count(), 1);
    assert_eq!(
        create_codes
            .iter()
            .filter(|code| **code == Some("FILE_ALREADY_EXISTS"))
            .count(),
        1
    );
    assert!(matches!(
        fs::read_to_string(creates.path().join("file.txt"))
            .unwrap()
            .as_str(),
        "first\n" | "second\n"
    ));
}

#[tokio::test]
async fn concurrent_readers_observe_only_the_complete_old_or_new_file() {
    const SIZE: usize = 4 * 1024 * 1024;
    let workspace = TempWorkspace::new("old-or-new");
    let path = workspace.path().join("file.txt");
    let old = Arc::new(mutation_file_with_size(SIZE));
    let mut new_bytes = old.as_ref().clone();
    new_bytes[..3].copy_from_slice(b"new");
    let new = Arc::new(new_bytes);
    fs::write(&path, old.as_slice()).unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let invalid = Arc::new(Mutex::new(None::<String>));
    let reader = std::thread::spawn({
        let path = path.clone();
        let old = old.clone();
        let new = new.clone();
        let stop = stop.clone();
        let reads = reads.clone();
        let invalid = invalid.clone();
        move || {
            while !stop.load(Ordering::SeqCst) {
                match fs::read(&path) {
                    Ok(bytes)
                        if bytes.as_slice() == old.as_slice()
                            || bytes.as_slice() == new.as_slice() => {}
                    Ok(bytes) => {
                        *invalid.lock().unwrap() =
                            Some(format!("observed partial file of {} bytes", bytes.len()));
                        break;
                    }
                    Err(error) => {
                        *invalid.lock().unwrap() = Some(format!("read failed: {error}"));
                        break;
                    }
                }
                reads.fetch_add(1, Ordering::SeqCst);
                std::thread::yield_now();
            }
        }
    });
    while reads.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }

    let session = run_patch(
        workspace.path(),
        "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n",
        FileChangePolicy::Allow,
        Arc::new(FixedApproval {
            outcome: ApprovalOutcome::Unavailable,
            mutate_before_answer: None,
        }),
    )
    .await;
    stop.store(true, Ordering::SeqCst);
    reader.join().unwrap();

    assert!(result_code(&session).is_none());
    assert!(reads.load(Ordering::SeqCst) > 0);
    assert_eq!(*invalid.lock().unwrap(), None);
    assert_eq!(fs::read(path).unwrap().as_slice(), new.as_slice());
}
