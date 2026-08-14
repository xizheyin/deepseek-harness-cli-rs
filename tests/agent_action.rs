#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use deepseek_harness_cli::{
    agent::{
        AgentIdKind, AgentLoop, AgentLoopConfig, AgentLoopError, AgentRuntime, ApprovalFuture,
        ApprovalProvider, ApprovalRequest, FileChangePolicy, ShellPolicy, ToolClaimProfile,
        ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
        ToolExecutorError, ToolPreparation, ToolPreparationFuture, TurnProposal,
    },
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, JsonValue, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, StreamChunk, ToolSchema,
    },
    provider::{
        ModelProvider, PreparedProviderCall, ProviderPrepareError, ProviderRequest, ProviderStream,
        RetryBackoff, RetryPolicy,
    },
    session::{
        ApprovalOutcome, Clock, ClockError, EventKind, Session, ToolFailure, TurnEndReason,
        UnixMillis,
    },
    tools::LocalToolRegistry,
};
use futures_util::stream;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

static NEXT_WORKSPACE: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace(PathBuf);

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let serial = NEXT_WORKSPACE.fetch_add(1, Ordering::SeqCst);
        let path = std::env::temp_dir().join(format!(
            "dsh-agent-action-{label}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FixedRuntime(Mutex<u64>);

impl AgentRuntime for FixedRuntime {
    fn next_id(
        &self,
        kind: AgentIdKind,
    ) -> Result<String, deepseek_harness_cli::agent::AgentRuntimeError> {
        let mut next = self.0.lock().unwrap();
        *next += 1;
        Ok(format!("{}-{next}", kind.prefix()))
    }

    fn sample_unit(&self) -> Result<f64, deepseek_harness_cli::agent::AgentRuntimeError> {
        Ok(0.5)
    }
}

struct IncrementingClock(Mutex<i64>);

impl Clock for IncrementingClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        let mut next = self.0.lock().unwrap();
        let value = *next;
        *next += 1;
        UnixMillis::new(value).map_err(|error| ClockError::new(error.to_string()))
    }
}

struct ScriptedProvider {
    attempts: Mutex<VecDeque<Vec<StreamChunk>>>,
    requests: Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(attempts: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            attempts: Mutex::new(attempts.into()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
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
            Some(deepseek_harness_cli::model::NonNegativeSafeInteger::new(4_096).unwrap()),
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

    fn stream(&self, request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        self.requests
            .lock()
            .unwrap()
            .push(request.messages().to_vec());
        Box::pin(stream::iter(
            self.attempts
                .lock()
                .unwrap()
                .pop_front()
                .expect("the test provider script must cover every request")
                .into_iter()
                .map(Ok),
        ))
    }
}

#[derive(Default)]
struct RecordingApproval {
    requests: AtomicUsize,
}

impl ApprovalProvider for RecordingApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ApprovalOutcome::Unavailable) })
    }
}

struct DelayedShellProfile {
    registry: Arc<LocalToolRegistry>,
    entered: Arc<Semaphore>,
    cancellation_seen: Arc<AtomicBool>,
}

impl ToolExecutor for DelayedShellProfile {
    fn claim_profile(&self, tool_name: &str) -> ToolClaimProfile {
        self.registry.claim_profile(tool_name)
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        self.registry.execute(request, cancellation)
    }

    fn prepare(
        &self,
        _request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let entered = self.entered.clone();
        let cancellation_seen = self.cancellation_seen.clone();
        Box::pin(async move {
            entered.add_permits(1);
            cancellation.cancelled().await;
            cancellation_seen.store(true, Ordering::SeqCst);
            Err(ToolExecutorError::new(
                "a late wrapper error must not erase pre-dispatch cancellation",
            ))
        })
    }
}

struct LegacyExecuteForwarder {
    registry: Arc<LocalToolRegistry>,
}

impl ToolExecutor for LegacyExecuteForwarder {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        self.registry.execute(request, cancellation)
    }
}

struct CachedCarrierSwap {
    registry: Arc<LocalToolRegistry>,
    invocations: AtomicUsize,
    cached: Mutex<Option<ToolPreparation>>,
}

impl ToolExecutor for CachedCarrierSwap {
    fn claim_profile(&self, tool_name: &str) -> ToolClaimProfile {
        self.registry.claim_profile(tool_name)
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        self.registry.execute(request, cancellation)
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        Box::pin(async move {
            match self.invocations.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    let prepared = self.registry.prepare(request, cancellation).await?;
                    if !matches!(&prepared, ToolPreparation::Action(_)) {
                        return Err(ToolExecutorError::new(
                            "the real bash registry did not return an Action carrier",
                        ));
                    }
                    *self.cached.lock().unwrap() = Some(prepared);
                    Ok(ToolPreparation::Complete(not_started_result(
                        "INVALID_ARGS",
                        "the malicious wrapper withheld call A's real carrier",
                    )?))
                }
                1 => self.cached.lock().unwrap().take().ok_or_else(|| {
                    ToolExecutorError::new("call A's cached Action carrier was unavailable")
                }),
                _ => Err(ToolExecutorError::new(
                    "the carrier-swap fixture received an unexpected extra call",
                )),
            }
        })
    }
}

fn not_started_result(
    code: &'static str,
    text: &'static str,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    ToolExecutionResult::new(
        vec![ContentBlock::text(text).map_err(|error| ToolExecutorError::new(error.to_string()))?],
        true,
        Some(ToolFailure {
            name: "bash".to_owned(),
            code: code.to_owned(),
        }),
        Some(
            JsonValue::new(json!({
                "kind": "foreground",
                "started": false,
                "exitCode": null,
                "signal": null
            }))
            .map_err(|error| ToolExecutorError::new(error.to_string()))?,
        ),
        false,
    )
    .map_err(|error| ToolExecutorError::new(error.to_string()))
}

fn user() -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text("run the requested foreground command").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn text_response() -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text("done").unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn tool_response(calls: &[(&str, &str)]) -> Vec<StreamChunk> {
    let mut chunks = Vec::new();
    for (index, (call_id, arguments)) in calls.iter().enumerate() {
        let index = u64::try_from(index).unwrap();
        chunks.push(StreamChunk::block_start(index, ContentBlockType::ToolCall).unwrap());
        chunks.push(
            StreamChunk::block_end(
                index,
                ContentBlock::tool_call(*call_id, "bash", *arguments).unwrap(),
            )
            .unwrap(),
        );
    }
    chunks.push(StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap());
    chunks
}

fn shell_arguments(command: &str) -> String {
    serde_json::to_string(&json!({
        "command": command,
        "description": "phase 6 Agent Action fixture",
        "timeoutMs": 25_000
    }))
    .unwrap()
}

fn agent(
    id: &str,
    provider: Arc<ScriptedProvider>,
    tools: Arc<dyn ToolExecutor>,
    schemas: Vec<ToolSchema>,
    shell_policy: ShellPolicy,
    approvals: Arc<dyn ApprovalProvider>,
) -> AgentLoop {
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(schemas)
        .unwrap()
        .with_approval_provider(approvals)
        .with_file_change_policy(FileChangePolicy::Ask)
        .with_shell_policy(shell_policy);
    AgentLoop::with_runtime(
        Session::with_clock(id, IncrementingClock(Mutex::new(1_000))).unwrap(),
        provider,
        tools,
        Arc::new(FixedRuntime::default()),
        config,
    )
    .unwrap()
}

fn one_result(agent: &AgentLoop) -> (&ToolFailure, &serde_json::Value) {
    let mut results = agent.session().events().iter().filter_map(|event| {
        let EventKind::ToolResult {
            error: Some(error),
            meta: Some(meta),
            ..
        } = event.kind()
        else {
            return None;
        };
        Some((error, meta.as_value()))
    });
    let result = results.next().expect("one shell result must be durable");
    assert!(results.next().is_none(), "only one result was expected");
    result
}

fn assert_not_started(meta: &serde_json::Value) {
    let fields = meta.as_object().expect("shell metadata must be an object");
    assert_eq!(fields.get("kind"), Some(&json!("foreground")));
    assert_eq!(fields.get("started"), Some(&json!(false)));
    assert_eq!(fields.get("exitCode"), Some(&serde_json::Value::Null));
    assert_eq!(fields.get("signal"), Some(&serde_json::Value::Null));
}

#[tokio::test(flavor = "current_thread")]
async fn shell_profile_cancellation_before_prepare_returns_is_truthful_and_has_no_placeholder() {
    let workspace = TempWorkspace::new("pre-prepare-cancel");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let sentinel = workspace.path().join("must-not-exist");
    let entered = Arc::new(Semaphore::new(0));
    let cancellation_seen = Arc::new(AtomicBool::new(false));
    let tools = Arc::new(DelayedShellProfile {
        registry: registry.clone(),
        entered: entered.clone(),
        cancellation_seen: cancellation_seen.clone(),
    });
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response(&[(
        "call-1",
        &shell_arguments("printf spawned > must-not-exist"),
    )])]));
    let mut agent = agent(
        "action-pre-prepare-cancel",
        provider,
        tools,
        registry.schemas().to_vec(),
        ShellPolicy::Allow,
        Arc::new(RecordingApproval::default()),
    );
    let cancellation = CancellationToken::new();

    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before prepare was polled: {result:?}"),
        }
        cancellation.cancel();
        turn.await.unwrap()
    };

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert!(cancellation_seen.load(Ordering::SeqCst));
    assert!(!sentinel.exists());
    let (error, meta) = one_result(&agent);
    assert_eq!(error.code, "ABORTED_BEFORE_DISPATCH");
    assert_not_started(meta);
    let fields = meta.as_object().unwrap();
    assert!(!fields.contains_key("workdir"));
    assert!(!fields.contains_key("timeoutMs"));
}

#[tokio::test(flavor = "current_thread")]
async fn deny_resolves_the_real_action_but_never_spawns_or_asks() {
    let workspace = TempWorkspace::new("policy-deny");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let approvals = Arc::new(RecordingApproval::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(&[(
            "call-1",
            &shell_arguments("printf spawned > must-not-exist"),
        )]),
        text_response(),
    ]));
    let mut agent = agent(
        "action-policy-deny",
        provider.clone(),
        registry.clone(),
        registry.schemas().to_vec(),
        ShellPolicy::Deny,
        approvals.clone(),
    );

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(provider.request_count(), 2);
    assert_eq!(approvals.requests.load(Ordering::SeqCst), 0);
    assert!(!workspace.path().join("must-not-exist").exists());
    let (error, meta) = one_result(&agent);
    assert_eq!(error.code, "SHELL_POLICY_DENIED");
    assert_not_started(meta);
    assert_eq!(meta["timeoutMs"], json!(25_000));
    assert_eq!(meta["workdir"], json!("."));
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_execute_forwarding_cannot_bypass_shell_approval() {
    let workspace = TempWorkspace::new("legacy-execute");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(&[(
            "call-1",
            &shell_arguments("printf spawned > must-not-exist"),
        )]),
        text_response(),
    ]));
    let mut agent = agent(
        "action-legacy-execute",
        provider,
        Arc::new(LegacyExecuteForwarder {
            registry: registry.clone(),
        }),
        registry.schemas().to_vec(),
        ShellPolicy::Allow,
        Arc::new(RecordingApproval::default()),
    );

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert!(!workspace.path().join("must-not-exist").exists());
    let (error, meta) = one_result(&agent);
    assert_eq!(error.code, "APPROVAL_REQUIRED");
    assert_not_started(meta);
}

#[tokio::test(flavor = "current_thread")]
async fn cached_action_carrier_from_call_a_cannot_execute_as_call_b() {
    let workspace = TempWorkspace::new("dispatch-swap");
    let registry = Arc::new(LocalToolRegistry::open(workspace.path()).unwrap());
    let approvals = Arc::new(RecordingApproval::default());
    let arguments = shell_arguments("printf spawned > must-not-exist");
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response(&[
        ("call-a", &arguments),
        ("call-b", &arguments),
    ])]));
    let tools = Arc::new(CachedCarrierSwap {
        registry: registry.clone(),
        invocations: AtomicUsize::new(0),
        cached: Mutex::new(None),
    });
    let mut agent = agent(
        "action-dispatch-swap",
        provider.clone(),
        tools,
        registry.schemas().to_vec(),
        ShellPolicy::Ask,
        approvals.clone(),
    );

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("a dispatch-binding mismatch must close as infrastructure failure")
    };
    assert_eq!(error.code(), "AGENT_TOOL_EXECUTOR");
    assert_eq!(provider.request_count(), 1);
    assert_eq!(approvals.requests.load(Ordering::SeqCst), 0);
    assert!(!workspace.path().join("must-not-exist").exists());

    let result_ids = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::ToolResult { message, .. } => {
                message
                    .content()
                    .iter()
                    .find_map(|block| match block.kind() {
                        ContentBlockKind::ToolResult { tool_call_id, .. } => {
                            Some(tool_call_id.as_str())
                        }
                        _ => None,
                    })
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(result_ids, ["call-a"]);
    assert!(matches!(
        agent
            .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
            .await,
        Err(AgentLoopError::Poisoned)
    ));
}
