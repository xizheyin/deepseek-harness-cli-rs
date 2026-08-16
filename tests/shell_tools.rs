#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    process::Command as ProcessCommand,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use deepseek_harness_cli::{
    agent::{
        AgentLimits, AgentLoop, AgentLoopConfig, ApprovalFuture, ApprovalProvider,
        ApprovalProviderError, ApprovalRequest, ShellPolicy, ToolExecutionFuture,
        ToolExecutionRequest, ToolExecutor, TurnProposal,
    },
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, NonNegativeSafeInteger, StreamChunk,
        ToolSchema,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream, RetryBackoff,
        RetryPolicy,
    },
    session::{ApprovalOutcome, EventKind, Session, TurnEndReason},
    tools::LocalToolRegistry,
};
use futures_util::stream;
use serde_json::{Value, json};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

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
            "dsh-shell-tools-{label}-{}-{nanos}-{ordinal}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ScriptedProvider {
    attempts: Mutex<VecDeque<Vec<StreamChunk>>>,
    requests: Mutex<Vec<Vec<Message>>>,
    dispatches: AtomicU64,
}

impl ScriptedProvider {
    fn for_bash(arguments: &Value) -> Self {
        Self::for_tool("bash", arguments)
    }

    fn for_tool(tool_name: &str, arguments: &Value) -> Self {
        Self {
            attempts: Mutex::new(
                vec![named_tool_response(tool_name, arguments), text_response()]
                    .into_iter()
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
            dispatches: AtomicU64::new(0),
        }
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }

    fn dispatch_count(&self) -> u64 {
        self.dispatches.load(Ordering::SeqCst)
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
            Some(NonNegativeSafeInteger::new(4_096).unwrap()),
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

    fn stream(&self, request: ProviderRequest, _cancellation: CancellationToken) -> ProviderStream {
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

struct RecordingApproval {
    outcome: ApprovalOutcome,
    requests: Mutex<Vec<ApprovalRequest>>,
}

impl RecordingApproval {
    fn new(outcome: ApprovalOutcome) -> Self {
        Self {
            outcome,
            requests: Mutex::new(Vec::new()),
        }
    }

    fn requests(&self) -> Vec<ApprovalRequest> {
        self.requests.lock().unwrap().clone()
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
        Box::pin(async move { Ok::<_, ApprovalProviderError>(outcome) })
    }
}

struct DirectOnlyRegistry {
    inner: Arc<LocalToolRegistry>,
}

impl ToolExecutor for DirectOnlyRegistry {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        self.inner.execute(request, cancellation)
    }
}

#[derive(Debug)]
struct ToolResultFacts {
    text: String,
    is_error: bool,
    error_code: Option<String>,
    meta: Value,
    inner_text_json_bytes: usize,
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn named_tool_response(tool_name: &str, arguments: &Value) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(
            0,
            ContentBlock::tool_call("shell-call-1", tool_name, arguments.to_string()).unwrap(),
        )
        .unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]
}

fn text_response() -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text("done").unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn user() -> Message {
    Message::user(
        "shell-user-1",
        vec![ContentBlock::text("run the requested foreground command").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

async fn run_with_executor(
    tools: Arc<dyn ToolExecutor>,
    schemas: Vec<ToolSchema>,
    arguments: Value,
    shell_policy: Option<ShellPolicy>,
    approval: Option<Arc<dyn ApprovalProvider>>,
) -> (Session, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::for_bash(&arguments));
    let mut config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(schemas)
        .unwrap();
    if let Some(policy) = shell_policy {
        config = config.with_shell_policy(policy);
    }
    if let Some(approval) = approval {
        config = config.with_approval_provider(approval);
    }
    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let mut agent = AgentLoop::new(
        Session::new(format!("shell-tools-{session_id}")).unwrap(),
        provider.clone(),
        tools,
        config,
    )
    .unwrap();
    agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();
    (agent.into_session(), provider)
}

async fn run_shell(
    registry: Arc<LocalToolRegistry>,
    arguments: Value,
    shell_policy: Option<ShellPolicy>,
    approval: Option<Arc<dyn ApprovalProvider>>,
) -> (Session, Arc<ScriptedProvider>) {
    let schemas = registry.schemas().to_vec();
    run_with_executor(registry, schemas, arguments, shell_policy, approval).await
}

fn shell_agent_with_limits(
    registry: Arc<LocalToolRegistry>,
    arguments: Value,
    limits: AgentLimits,
) -> (AgentLoop, Arc<ScriptedProvider>) {
    let provider = Arc::new(ScriptedProvider::for_bash(&arguments));
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(registry.schemas().to_vec())
        .unwrap()
        .with_limits(limits)
        .with_shell_policy(ShellPolicy::Allow);
    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let agent = AgentLoop::new(
        Session::new(format!("shell-tools-bounded-{session_id}")).unwrap(),
        provider.clone(),
        registry,
        config,
    )
    .unwrap();
    (agent, provider)
}

async fn run_named_tool(
    registry: Arc<LocalToolRegistry>,
    tool_name: &str,
    arguments: Value,
) -> Session {
    let provider = Arc::new(ScriptedProvider::for_tool(tool_name, &arguments));
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(registry.schemas().to_vec())
        .unwrap();
    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let mut agent = AgentLoop::new(
        Session::new(format!("shell-tools-{session_id}")).unwrap(),
        provider,
        registry,
        config,
    )
    .unwrap();
    agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();
    agent.into_session()
}

fn phase6_oracle() -> Value {
    serde_json::from_str(include_str!("fixtures/tools/upstream_phase6_oracle.json")).unwrap()
}

fn result_facts(session: &Session) -> ToolResultFacts {
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
                let result = message.content().iter().find_map(|block| {
                    let ContentBlockKind::ToolResult { is_error, .. } = block.kind() else {
                        return None;
                    };
                    let text_block = block
                        .tool_result_content()?
                        .iter()
                        .find(|inner| inner["type"] == "text")?;
                    Some((
                        text_block["text"].as_str().unwrap_or_default().to_owned(),
                        is_error.unwrap_or(false),
                        serde_json::to_vec(text_block).unwrap().len(),
                    ))
                })?;
                Some(ToolResultFacts {
                    text: result.0,
                    is_error: result.1,
                    error_code: error.as_ref().map(|failure| failure.code.clone()),
                    meta: meta
                        .as_ref()
                        .map_or(Value::Null, |value| value.as_value().clone()),
                    inner_text_json_bytes: result.2,
                })
            }
            _ => None,
        })
        .unwrap()
}

fn assert_result_cites_call(session: &Session) {
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
        panic!("the shell result must cite exactly one tool call")
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
        });
    assert_eq!(result_call_id, Some(call_id));
}

#[test]
fn local_registry_exposes_a_closed_foreground_bash_schema() {
    let workspace = TempWorkspace::new("schema");
    let registry = LocalToolRegistry::open(workspace.path()).unwrap();
    let oracle = phase6_oracle();
    assert_eq!(
        registry
            .schemas()
            .iter()
            .map(|schema| schema.name())
            .collect::<Vec<_>>(),
        ["list", "glob", "grep", "read", "apply_patch", "bash"]
    );
    let parameters = registry.schemas().last().unwrap().parameters().as_value();
    assert_eq!(parameters["type"], "object");
    assert_eq!(parameters["required"], json!(["command", "description"]));
    assert_eq!(parameters["additionalProperties"], false);
    assert_eq!(
        parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["command", "description", "timeoutMs", "workdir"]
    );
    assert_eq!(
        parameters["properties"]["timeoutMs"],
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": 295000,
            "description": "Command-local timeout in milliseconds"
        })
    );
    let upstream = &oracle["schemaSurface"]["foregroundOnly"]["modelSchema"]["parameters"];
    assert_eq!(upstream["properties"]["timeoutMs"]["type"], "number");
    assert!(upstream.get("additionalProperties").is_none());
}

#[test]
fn runtime_argument_parser_is_closed_and_never_starts_an_invalid_call() {
    let workspace = TempWorkspace::new("closed-args");
    let sentinel = workspace.path().join("must-not-exist");
    let command = "printf started > must-not-exist";
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let runtime = runtime();
    let cases = [
        json!({"command": command}),
        json!({"command": command, "description": null}),
        json!({"command": command, "description": "invalid", "timeoutMs": null}),
        json!({"command": command, "description": "invalid", "timeoutMs": 1.5}),
        json!({"command": command, "description": "invalid", "run_in_background": true}),
        json!({"command": "   ", "description": "invalid"}),
    ];
    for arguments in cases {
        let (session, _) = runtime.block_on(run_shell(
            registry.clone(),
            arguments,
            Some(ShellPolicy::Allow),
            None,
        ));
        let result = result_facts(&session);
        assert!(result.is_error);
        assert_eq!(result.error_code.as_deref(), Some("INVALID_ARGS"));
        assert_eq!(result.meta["kind"], "foreground");
        assert_eq!(result.meta["started"], false);
        assert_eq!(result.meta["exitCode"], Value::Null);
        assert_eq!(result.meta["signal"], Value::Null);
        assert!(!sentinel.exists());
    }
}

#[test]
fn direct_execute_cannot_bypass_the_agent_shell_action_gate() {
    let workspace = TempWorkspace::new("direct-fail-closed");
    let sentinel = workspace.path().join("must-not-exist");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let schemas = registry.schemas().to_vec();
    let direct_only: Arc<dyn ToolExecutor> = Arc::new(DirectOnlyRegistry { inner: registry });
    let arguments = json!({
        "command": "printf started > must-not-exist",
        "description": "prove the direct compatibility seam is fail-closed"
    });
    let (session, _) = runtime().block_on(run_with_executor(
        direct_only,
        schemas,
        arguments,
        Some(ShellPolicy::Allow),
        None,
    ));
    let result = result_facts(&session);
    assert!(result.is_error);
    assert_eq!(result.error_code.as_deref(), Some("APPROVAL_REQUIRED"));
    assert_eq!(result.meta["started"], false);
    assert!(!sentinel.exists());
}

#[test]
fn shell_policy_allow_deny_ask_and_default_no_ui_are_fail_closed() {
    let runtime = runtime();
    let oracle = phase6_oracle();
    assert_eq!(
        oracle["configuration"]["libraryExecutorDefaults"]["timeoutMs"],
        120_000
    );
    assert_eq!(
        oracle["configuration"]["shippedBaseComposition"]["ordinaryCallApproval"],
        "no prompt unless another pre-rule asks or escalation is requested"
    );

    let allow_workspace = TempWorkspace::new("allow");
    let allow_target = allow_workspace.path().join("allowed");
    let allow_registry = Arc::new(LocalToolRegistry::open(allow_workspace.path()).unwrap());
    let (allow_session, _) = runtime.block_on(run_shell(
        allow_registry,
        json!({
            "command": "printf allowed > allowed",
            "description": "allow one foreground command"
        }),
        Some(ShellPolicy::Allow),
        None,
    ));
    assert_eq!(fs::read_to_string(allow_target).unwrap(), "allowed");
    let allow_result = result_facts(&allow_session);
    assert!(!allow_result.is_error);
    assert_eq!(allow_result.meta["timeoutMs"], 25_000);

    let deny_workspace = TempWorkspace::new("deny");
    let deny_target = deny_workspace.path().join("denied");
    let deny_registry = Arc::new(LocalToolRegistry::open(deny_workspace.path()).unwrap());
    let deny_approval = Arc::new(RecordingApproval::new(ApprovalOutcome::AllowedOnce));
    let (deny_session, _) = runtime.block_on(run_shell(
        deny_registry,
        json!({
            "command": "printf denied > denied",
            "description": "deny one foreground command"
        }),
        Some(ShellPolicy::Deny),
        Some(deny_approval.clone()),
    ));
    assert_eq!(
        result_facts(&deny_session).error_code.as_deref(),
        Some("SHELL_POLICY_DENIED")
    );
    assert!(deny_approval.requests().is_empty());
    assert!(!deny_target.exists());

    for (label, answer, expected_code, should_exist) in [
        ("ask-allow", ApprovalOutcome::AllowedOnce, None, true),
        (
            "ask-reject",
            ApprovalOutcome::Rejected,
            Some("APPROVAL_REJECTED"),
            false,
        ),
    ] {
        let workspace = TempWorkspace::new(label);
        let target = workspace.path().join("answer");
        let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
        let approval = Arc::new(RecordingApproval::new(answer));
        let (session, _) = runtime.block_on(run_shell(
            registry,
            json!({
                "command": "printf answer > answer",
                "description": "ask before one foreground command"
            }),
            Some(ShellPolicy::Ask),
            Some(approval.clone()),
        ));
        assert_eq!(result_facts(&session).error_code.as_deref(), expected_code);
        assert_eq!(approval.requests().len(), 1);
        assert_eq!(target.exists(), should_exist);
    }

    let default_workspace = TempWorkspace::new("default-ask");
    let default_target = default_workspace.path().join("unavailable");
    let default_registry = Arc::new(LocalToolRegistry::open(default_workspace.path()).unwrap());
    let (default_session, provider) = runtime.block_on(run_shell(
        default_registry,
        json!({
            "command": "printf unavailable > unavailable",
            "description": "use the safe default without an approval UI"
        }),
        None,
        None,
    ));
    assert_eq!(
        result_facts(&default_session).error_code.as_deref(),
        Some("APPROVAL_UNAVAILABLE")
    );
    assert!(default_session.events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ApprovalDecided { decided }
            if decided.outcome() == ApprovalOutcome::Unavailable
    )));
    assert_eq!(provider.dispatch_count(), 2);
    assert!(!default_target.exists());
}

#[test]
fn canonical_foreground_results_match_the_committed_phase6_oracle() {
    let oracle = phase6_oracle();
    assert_eq!(oracle["schemaVersion"], 1);
    assert_eq!(
        oracle["upstream"]["commit"],
        "47f943859bef60e4160492346772ded9b24f765a"
    );
    for pointer in [
        "/lifecycleBoundaries/checks/callerAbort/bodyStartedBeforeCallerAbort",
        "/lifecycleBoundaries/checks/callerAbort/callerAbortCleanupReapedLeaderBeforeResult",
        "/lifecycleBoundaries/checks/callerAbort/toolBoundaryReturnsGenericAbort",
        "/lifecycleBoundaries/checks/callerAbort/internalShellResultNotExposed",
        "/lifecycleBoundaries/checks/callerAbort/callerAbortReasonNotExposed",
        "/lifecycleBoundaries/checks/directCompletion/foregroundResultSettlesNormally",
        "/lifecycleBoundaries/checks/directCompletion/sameGroupDescendantOutlivesForegroundResult",
        "/lifecycleBoundaries/checks/directCompletion/subprocessServiceDisposeAwaitsWholeGroup",
    ] {
        assert_eq!(
            oracle.pointer(pointer),
            Some(&Value::Bool(true)),
            "{pointer}"
        );
    }
    let workspace = TempWorkspace::new("oracle");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let runtime = runtime();

    for name in [
        "success",
        "silent",
        "stdoutAndStderr",
        "nonzero",
        "selfSignal",
    ] {
        let scenario = &oracle["processScenarios"][name];
        let (session, _) = runtime.block_on(run_shell(
            registry.clone(),
            scenario["input"].clone(),
            Some(ShellPolicy::Allow),
            None,
        ));
        let result = result_facts(&session);
        let expected = &scenario["result"];
        assert_eq!(result.is_error, expected["isError"], "scenario={name}");
        assert_eq!(
            result.text, expected["content"][0]["text"],
            "scenario={name}"
        );
        for field in ["exitCode", "signal", "timedOut", "timeoutMs"] {
            assert_eq!(
                result.meta[field], expected["value"][field],
                "scenario={name}, field={field}"
            );
        }
        assert_eq!(result.meta["started"], true, "scenario={name}");
        assert_eq!(result.meta["kind"], "foreground", "scenario={name}");
        assert!(result.meta.get("stdout").is_none(), "scenario={name}");
        assert!(result.meta.get("stderr").is_none(), "scenario={name}");
        assert_result_cites_call(&session);
    }

    let timeout = &oracle["processScenarios"]["timeout"];
    let (session, _) = runtime.block_on(run_shell(
        registry,
        timeout["input"].clone(),
        Some(ShellPolicy::Allow),
        None,
    ));
    let result = result_facts(&session);
    assert!(!result.is_error);
    assert_eq!(result.meta["started"], true);
    assert_eq!(result.meta["timedOut"], true);
    assert_eq!(result.meta["timeoutMs"], 100);
    assert!(result.text.contains("[timed out after 100ms]"));
    assert_ne!(
        result.meta["exitCode"].is_null(),
        result.meta["signal"].is_null()
    );
}

#[test]
fn workdir_preview_result_and_next_request_keep_one_call_provenance() {
    let oracle = phase6_oracle();
    assert_eq!(
        oracle["processScenarios"]["checks"]["defaultWorkdirUsesSession"],
        true
    );
    assert_eq!(
        oracle["processScenarios"]["checks"]["relativeWorkdirUsesSessionBase"],
        true
    );
    assert_eq!(
        oracle["processScenarios"]["workdir"]["relative"]["input"]["workdir"],
        "nested"
    );
    let workspace = TempWorkspace::new("workdir-preview");
    fs::create_dir(workspace.path().join("nested")).unwrap();
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let approval = Arc::new(RecordingApproval::new(ApprovalOutcome::AllowedOnce));
    let arguments = json!({
        "command": "pwd",
        "description": "print the approved nested working directory",
        "timeoutMs": 25000,
        "workdir": "nested"
    });
    let (session, provider) = runtime().block_on(run_shell(
        registry,
        arguments,
        Some(ShellPolicy::Ask),
        Some(approval.clone()),
    ));
    let request = approval.requests().into_iter().next().unwrap();
    assert_eq!(request.tool_name(), "bash");
    assert_eq!(request.call_id().as_str(), "shell-call-1");
    assert_eq!(
        request.reason(),
        Some("print the approved nested working directory")
    );
    assert!(request.preview().contains("pwd"));
    assert!(request.preview().contains("nested"));
    assert!(request.preview().contains("25000"));

    let result = result_facts(&session);
    let expected_cwd = fs::canonicalize(workspace.path().join("nested")).unwrap();
    assert_eq!(result.text, format!("{}\n", expected_cwd.display()));
    assert_eq!(result.meta["workdir"], "nested");
    assert_eq!(result.meta["timeoutMs"], 25000);
    assert_result_cites_call(&session);

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].iter().any(|message| {
        message.content().iter().any(|block| {
            matches!(
                block.kind(),
                ContentBlockKind::ToolResult { tool_call_id, .. }
                    if tool_call_id.as_str() == "shell-call-1"
            )
        })
    }));
}

#[test]
fn workdir_failures_are_specific_redacted_and_never_reach_approval_or_spawn() {
    let oracle = phase6_oracle();
    assert_eq!(
        oracle["processScenarios"]["checks"]["absoluteWorkdirAccepted"],
        true
    );
    let workspace = TempWorkspace::new("workdir-errors");
    let outside = TempWorkspace::new("workdir-outside");
    fs::write(workspace.path().join("ordinary-file"), "not a directory\n").unwrap();
    let sentinel = workspace.path().join("must-not-exist");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let runtime = runtime();
    let cases = [
        (
            "missing".to_owned(),
            "SHELL_WORKDIR_NOT_FOUND",
            Some("missing"),
        ),
        (
            "ordinary-file".to_owned(),
            "SHELL_WORKDIR_NOT_DIRECTORY",
            Some("ordinary-file"),
        ),
        (
            outside.path().to_string_lossy().into_owned(),
            "SHELL_WORKDIR_OUTSIDE_WORKSPACE",
            None,
        ),
    ];

    for (workdir, expected_code, expected_display) in cases {
        let approval = Arc::new(RecordingApproval::new(ApprovalOutcome::AllowedOnce));
        let (session, _) = runtime.block_on(run_shell(
            registry.clone(),
            json!({
                "command": "printf spawned > must-not-exist",
                "description": "prove an invalid workdir cannot start",
                "workdir": workdir.clone()
            }),
            Some(ShellPolicy::Ask),
            Some(approval.clone()),
        ));
        let result = result_facts(&session);
        assert!(result.is_error);
        assert_eq!(result.error_code.as_deref(), Some(expected_code));
        assert_eq!(result.meta["started"], false);
        assert_eq!(result.meta["timeoutMs"], 25_000);
        match expected_display {
            Some(display) => assert_eq!(result.meta["workdir"], display),
            None => assert!(result.meta.get("workdir").is_none()),
        }
        assert!(approval.requests().is_empty());
        assert!(!result.text.contains(&workdir));
        assert!(!sentinel.exists());
    }
}

#[test]
fn local_registry_file_and_shell_tools_keep_one_retained_workspace_authority() {
    let workspace = TempWorkspace::new("one-authority");
    fs::write(
        workspace.path().join("authority.txt"),
        "retained-original\n",
    )
    .unwrap();
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());

    let retained_name = workspace.path().with_extension("retained-root");
    fs::rename(workspace.path(), &retained_name).unwrap();
    fs::create_dir(workspace.path()).unwrap();
    fs::write(
        workspace.path().join("authority.txt"),
        "ambient-replacement\n",
    )
    .unwrap();

    let runtime = runtime();
    let read_session = runtime.block_on(run_named_tool(
        registry.clone(),
        "read",
        json!({"file_path": "authority.txt"}),
    ));
    let read = result_facts(&read_session);
    assert!(read.text.contains("retained-original"));
    assert!(!read.text.contains("ambient-replacement"));

    let (shell_session, _) = runtime.block_on(run_shell(
        registry.clone(),
        json!({
            "command": "/bin/cat authority.txt",
            "description": "read through the retained shell directory"
        }),
        Some(ShellPolicy::Allow),
        None,
    ));
    let shell = result_facts(&shell_session);
    assert_eq!(shell.text, "retained-original\n");
    assert_eq!(shell.meta["workdir"], ".");

    drop(registry);
    fs::remove_dir_all(retained_name).unwrap();
}

#[test]
fn real_agent_cancellation_after_shell_spawn_waits_for_group_cleanup() {
    let workspace = TempWorkspace::new("agent-cancel-started-shell");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let (mut agent, provider) = shell_agent_with_limits(
        registry,
        json!({
            "command": "trap 'printf cleaned > cleaned; exit 0' TERM; printf started > started; while :; do /bin/sleep 1; done",
            "description": "prove caller cancellation waits for the started shell",
            "timeoutMs": 25000
        }),
        AgentLimits::default(),
    );
    let cancellation = CancellationToken::new();

    let outcome = runtime().block_on(async {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        let started_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            tokio::select! {
                result = &mut turn => panic!("shell turn ended before its started sentinel: {result:?}"),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if workspace.path().join("started").exists() {
                        break;
                    }
                    assert!(
                        tokio::time::Instant::now() < started_deadline,
                        "shell did not reach its started boundary"
                    );
                }
            }
        }
        cancellation.cancel();
        tokio::time::timeout(Duration::from_secs(6), turn)
            .await
            .expect("started cancellation must finish after bounded process cleanup")
            .unwrap()
    });

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(provider.dispatch_count(), 1);
    assert!(workspace.path().join("cleaned").exists());
    let result = result_facts(agent.session());
    assert!(result.is_error);
    assert_eq!(result.error_code.as_deref(), Some("ABORTED"));
    assert_eq!(result.meta["started"], true);
    assert_eq!(result.meta["aborted"], true);
}

#[test]
fn real_agent_tool_timeout_keeps_a_started_result_and_can_continue() {
    let workspace = TempWorkspace::new("agent-tool-timeout");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let limits = AgentLimits::default()
        .with_tool_duration(Duration::from_secs(2))
        .unwrap();
    let (mut agent, provider) = shell_agent_with_limits(
        registry,
        json!({
            "command": "trap 'printf cleaned > cleaned; exit 0' TERM; printf started > started; while :; do /bin/sleep 1; done",
            "description": "prove the Agent tool deadline owns a started result",
            "timeoutMs": 25000
        }),
        limits,
    );

    let outcome = runtime().block_on(async {
        tokio::time::timeout(
            Duration::from_secs(8),
            agent.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new()),
        )
        .await
        .expect("tool timeout cleanup must remain bounded")
        .unwrap()
    });

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(provider.dispatch_count(), 2);
    assert!(workspace.path().join("started").exists());
    assert!(workspace.path().join("cleaned").exists());
    let result = result_facts(agent.session());
    assert!(result.is_error);
    assert_eq!(result.error_code.as_deref(), Some("TOOL_TIMEOUT"));
    assert_eq!(result.meta["started"], true);
    assert_eq!(result.meta["aborted"], true);
}

#[test]
fn real_agent_turn_timeout_keeps_the_shell_fact_but_closes_the_turn() {
    let workspace = TempWorkspace::new("agent-turn-timeout");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let limits = AgentLimits::default()
        .with_turn_duration(Duration::from_secs(3))
        .unwrap()
        .with_tool_duration(Duration::from_secs(6))
        .unwrap();
    let (mut agent, provider) = shell_agent_with_limits(
        registry,
        json!({
            "command": "trap 'printf cleaned > cleaned; exit 0' TERM; printf started > started; while :; do /bin/sleep 1; done",
            "description": "prove the turn deadline closes only after shell cleanup",
            "timeoutMs": 25000
        }),
        limits,
    );

    let outcome = runtime().block_on(async {
        tokio::time::timeout(
            Duration::from_secs(10),
            agent.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new()),
        )
        .await
        .expect("turn timeout cleanup must remain bounded")
        .unwrap()
    });

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("a turn deadline must close the turn as an error")
    };
    assert_eq!(error.code(), "AGENT_TURN_TIMEOUT");
    assert_eq!(provider.dispatch_count(), 1);
    assert!(workspace.path().join("started").exists());
    assert!(workspace.path().join("cleaned").exists());
    let result = result_facts(agent.session());
    assert!(result.is_error);
    assert_eq!(result.error_code.as_deref(), Some("AGENT_TURN_TIMEOUT"));
    assert_eq!(result.meta["started"], true);
    assert_eq!(result.meta["aborted"], true);
}

#[test]
fn real_agent_detects_a_silent_background_process_without_waiting_for_pipe_eof() {
    let workspace = TempWorkspace::new("agent-silent-background");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let started = std::time::Instant::now();
    let (session, provider) = runtime().block_on(run_shell(
        registry,
        json!({
            "command": "/bin/sleep 60 &",
            "description": "prove a silent background process cannot outlive the foreground call",
            "timeoutMs": 25000
        }),
        Some(ShellPolicy::Allow),
        None,
    ));

    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the runner appears to have waited for inherited pipe EOF instead of its process observer"
    );
    assert_eq!(provider.dispatch_count(), 2);
    let result = result_facts(&session);
    assert!(result.is_error);
    assert_eq!(
        result.error_code.as_deref(),
        Some("BACKGROUND_PROCESS_NOT_SUPPORTED")
    );
    assert_eq!(result.meta["started"], true);
}

#[test]
fn rendered_shell_output_obeys_the_64_kib_compact_json_limit() {
    let workspace = TempWorkspace::new("encoded-output");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let (session, _) = runtime().block_on(run_shell(
        registry,
        json!({
            "command": "printf '%*s' 100000 ''",
            "description": "produce more output than one durable text block can retain",
            "timeoutMs": 25000
        }),
        Some(ShellPolicy::Allow),
        None,
    ));
    let result = result_facts(&session);
    assert!(!result.is_error);
    assert!(result.inner_text_json_bytes <= 64 * 1024);
    assert!(result.inner_text_json_bytes > 60 * 1024);
    assert_eq!(result.meta["stdoutTruncated"], true);
    assert_eq!(result.meta["stderrTruncated"], false);
    assert!(result.text.contains("[stdout truncated; tail only]"));
}

#[test]
fn isolated_environment_and_fixed_startup_child() {
    if std::env::var_os("DSH_SHELL_TEST_CHILD").is_none() {
        return;
    }
    let workspace = PathBuf::from(std::env::var_os("DSH_SHELL_TEST_WORKSPACE").unwrap());
    let expected_path = std::env::var("DSH_SHELL_TEST_EXPECTED_PATH").unwrap();
    let hook_path = std::env::var("BASH_ENV").unwrap();
    let registry = Arc::new(LocalToolRegistry::open(&workspace).unwrap());
    let registry_debug = format!("{registry:?}");
    let (session, _) = runtime().block_on(run_shell(
        registry,
        json!({
            "command": "printf 'argv0=%s\\npath=%s\\nbash_env=%s\\nsafe=%s\\nsecret=%s\\nambient_dsh=%s\\nproxy=%s\\nterminal=%s,%s,%s,%s,%s\\n' \"$0\" \"$PATH\" \"${BASH_ENV+present}\" \"${PHASE6_TEST_SAFE+present}\" \"${PHASE6_TEST_SECRET+present}\" \"${DSH_PHASE6_AMBIENT+present}\" \"${HTTPS_PROXY+present}\" \"$NO_COLOR\" \"$TERM\" \"$PAGER\" \"$GIT_PAGER\" \"$GIT_TERMINAL_PROMPT\"",
            "description": "inspect the fixed executable and scrubbed child environment",
            "timeoutMs": 25000
        }),
        Some(ShellPolicy::Allow),
        None,
    ));
    let result = result_facts(&session);
    assert_eq!(
        result.text,
        format!(
            "argv0=bash\npath={expected_path}\nbash_env=\nsafe=\nsecret=\nambient_dsh=\nproxy=\nterminal=1,dumb,cat,cat,0\n"
        )
    );
    assert!(!workspace.join("bash-env-hook-ran").exists());
    let encoded = session.to_json().unwrap();
    for hidden in [
        "conspicuous-fake-secret",
        "ordinary-safe-value",
        "conspicuous-fake-managed-value",
        "http://proxy.invalid",
        hook_path.as_str(),
    ] {
        assert!(!encoded.contains(hidden), "Session leaked {hidden}");
        assert!(!registry_debug.contains(hidden), "Debug leaked {hidden}");
    }
}

#[test]
fn child_environment_drops_startup_hooks_secrets_proxies_and_ambient_dsh() {
    let oracle = phase6_oracle();
    for check in [
        "ordinaryAmbientRetained",
        "bareBashUsesEffectivePath",
        "bashEnvHookExecuted",
        "argvZeroIsBareBash",
    ] {
        assert_eq!(
            oracle["processScenarios"]["checks"][check], true,
            "missing upstream oracle fact {check}"
        );
    }
    assert_eq!(
        oracle["processScenarios"]["executableAndStartup"]["pathResolution"]["result"]["isError"],
        true
    );
    let workspace = TempWorkspace::new("isolated-environment");
    let empty_path = workspace.path().join("empty-path");
    fs::create_dir(&empty_path).unwrap();
    let hook = workspace.path().join("bash-env-hook.sh");
    fs::write(&hook, "printf hook-ran > bash-env-hook-ran\n").unwrap();

    let output = ProcessCommand::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("isolated_environment_and_fixed_startup_child")
        .arg("--nocapture")
        .env("DSH_SHELL_TEST_CHILD", "1")
        .env("DSH_SHELL_TEST_WORKSPACE", workspace.path())
        .env("DSH_SHELL_TEST_EXPECTED_PATH", &empty_path)
        .env("PATH", &empty_path)
        .env("BASH_ENV", &hook)
        .env("PHASE6_TEST_SAFE", "ordinary-safe-value")
        .env("PHASE6_TEST_SECRET", "conspicuous-fake-secret")
        .env("DSH_PHASE6_AMBIENT", "conspicuous-fake-managed-value")
        .env("HTTPS_PROXY", "http://proxy.invalid")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "isolated child failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!workspace.path().join("bash-env-hook-ran").exists());
}
