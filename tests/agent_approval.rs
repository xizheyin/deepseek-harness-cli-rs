use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use deepseek_harness_cli::{
    agent::{
        AgentBuildError, AgentIdKind, AgentLimits, AgentLoop, AgentLoopConfig, AgentRuntime,
        ApprovalFuture, ApprovalPrompt, ApprovalProvider, ApprovalProviderError, ApprovalRequest,
        FileChangePolicy, MAX_AGENT_COMMITTED_TOOL_RESULT_EVENT_BYTES, MAX_APPROVAL_PREVIEW_BYTES,
        MAX_APPROVAL_REASON_BYTES, MutationDeclineReason, PreparedToolMutation, ToolCommitOutcome,
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
        ApprovalAskedEvent, ApprovalOutcome, ApprovalRequestId, Clock, ClockError, EventKind,
        NewEvent, Session, ToolFailure, TurnEndReason, TurnId, UnixMillis,
    },
};
use futures_util::stream;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

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

struct CountingClock {
    calls: Arc<AtomicUsize>,
}

impl Clock for CountingClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        let offset = self.calls.fetch_add(1, Ordering::SeqCst);
        let value = i64::try_from(offset)
            .ok()
            .and_then(|offset| 1_000_i64.checked_add(offset))
            .ok_or_else(|| ClockError::new("test clock exhausted"))?;
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
                .unwrap()
                .into_iter()
                .map(Ok),
        ))
    }
}

#[derive(Default)]
struct RecordingApprovalProvider {
    outcomes: Mutex<VecDeque<Result<ApprovalOutcome, ApprovalProviderError>>>,
    requests: Mutex<Vec<ApprovalRequest>>,
}

impl RecordingApprovalProvider {
    fn with(outcomes: Vec<Result<ApprovalOutcome, ApprovalProviderError>>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into()),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl ApprovalProvider for RecordingApprovalProvider {
    fn request(
        &self,
        request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        self.requests.lock().unwrap().push(request);
        Box::pin(async move { self.outcomes.lock().unwrap().pop_front().unwrap() })
    }
}

struct LateAllowApproval {
    entered: Arc<Semaphore>,
    child_cancelled: Arc<AtomicBool>,
}

impl ApprovalProvider for LateAllowApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        self.entered.add_permits(1);
        let observed = self.child_cancelled.clone();
        Box::pin(async move {
            cancellation.cancelled().await;
            observed.store(true, Ordering::SeqCst);
            Ok(ApprovalOutcome::AllowedOnce)
        })
    }
}

struct PanicApproval {
    during_poll: bool,
}

impl ApprovalProvider for PanicApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        if !self.during_poll {
            panic!("synthetic approval factory panic");
        }
        Box::pin(async { panic!("synthetic approval future panic") })
    }
}

struct PreparedMutations {
    commits: Arc<Mutex<Vec<String>>>,
    before_commit: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl PreparedMutations {
    fn new(commits: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            commits,
            before_commit: None,
        }
    }

    fn with_before_commit(
        commits: Arc<Mutex<Vec<String>>>,
        before_commit: Arc<dyn Fn() + Send + Sync>,
    ) -> Self {
        Self {
            commits,
            before_commit: Some(before_commit),
        }
    }

    fn prepared(&self, call_id: String) -> PreparedToolMutation {
        let diff = format!("--- a/{call_id}.txt\n+++ b/{call_id}.txt\n@@ -1 +1 @@\n-old\n+new\n");
        let prompt =
            ApprovalPrompt::new(Some("change one workspace file".to_owned()), diff.clone())
                .unwrap();
        let decline_diff = diff.clone();
        let commit_diff = diff;
        let commits = self.commits.clone();
        let before_commit = self.before_commit.clone();
        PreparedToolMutation::new(
            prompt,
            64 * 1024,
            Box::new(move |reason| mutation_result(&decline_diff, Some(reason))),
            Box::new(move |_cancellation| {
                if let Some(before_commit) = before_commit {
                    before_commit();
                }
                commits.lock().unwrap().push(call_id);
                ToolCommitOutcome::committed(mutation_result(&commit_diff, None)?)
            }),
        )
        .unwrap()
    }
}

impl ToolExecutor for PreparedMutations {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("prepare must own mutations")) })
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let call_id = request.call_id().to_string();
        Box::pin(async move { Ok(ToolPreparation::Mutation(self.prepared(call_id))) })
    }
}

#[derive(Default)]
struct CommitGate {
    released: Mutex<bool>,
    changed: Condvar,
}

impl CommitGate {
    fn wait(&self) {
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.changed.wait(released).unwrap();
        }
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}

#[derive(Clone, Copy)]
enum BlockingCommitDisposition {
    Committed,
    NotCommitted,
}

struct BlockingMutations {
    entered: Arc<Semaphore>,
    gate: Arc<CommitGate>,
    child_cancelled: Arc<AtomicBool>,
    commits: Arc<AtomicUsize>,
    disposition: BlockingCommitDisposition,
}

type BlockingMutationFixture = (
    Arc<BlockingMutations>,
    Arc<Semaphore>,
    Arc<CommitGate>,
    Arc<AtomicBool>,
    Arc<AtomicUsize>,
);

fn blocking_mutations(disposition: BlockingCommitDisposition) -> BlockingMutationFixture {
    let entered = Arc::new(Semaphore::new(0));
    let gate = Arc::new(CommitGate::default());
    let child_cancelled = Arc::new(AtomicBool::new(false));
    let commits = Arc::new(AtomicUsize::new(0));
    (
        Arc::new(BlockingMutations {
            entered: entered.clone(),
            gate: gate.clone(),
            child_cancelled: child_cancelled.clone(),
            commits: commits.clone(),
            disposition,
        }),
        entered,
        gate,
        child_cancelled,
        commits,
    )
}

impl ToolExecutor for BlockingMutations {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("prepare must own mutations")) })
    }

    fn prepare(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let entered = self.entered.clone();
        let gate = self.gate.clone();
        let child_cancelled = self.child_cancelled.clone();
        let commits = self.commits.clone();
        let disposition = self.disposition;
        Box::pin(async move {
            let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";
            let decline_diff = diff.to_owned();
            let commit_diff = diff.to_owned();
            let mutation = PreparedToolMutation::new(
                ApprovalPrompt::new(Some("change one workspace file".to_owned()), diff).unwrap(),
                64 * 1024,
                Box::new(move |reason| mutation_result(&decline_diff, Some(reason))),
                Box::new(move |cancellation| {
                    entered.add_permits(1);
                    gate.wait();
                    child_cancelled.store(cancellation.is_cancelled(), Ordering::SeqCst);
                    match disposition {
                        BlockingCommitDisposition::Committed => {
                            commits.fetch_add(1, Ordering::SeqCst);
                            ToolCommitOutcome::committed(mutation_result(&commit_diff, None)?)
                        }
                        BlockingCommitDisposition::NotCommitted => {
                            ToolCommitOutcome::not_committed(mutation_result(
                                &commit_diff,
                                Some(MutationDeclineReason::Aborted),
                            )?)
                        }
                    }
                }),
            )?;
            Ok(ToolPreparation::Mutation(mutation))
        })
    }
}

struct InvalidDeclineMutations {
    commits: Arc<AtomicUsize>,
}

impl ToolExecutor for InvalidDeclineMutations {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("prepare must own mutations")) })
    }

    fn prepare(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let commits = self.commits.clone();
        Box::pin(async move {
            let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";
            let commit_diff = diff.to_owned();
            Ok(ToolPreparation::Mutation(PreparedToolMutation::new(
                ApprovalPrompt::new(None, diff).unwrap(),
                64 * 1024,
                Box::new(|_| {
                    let content = ContentBlock::text("invalid successful decline")
                        .map_err(|error| ToolExecutorError::new(error.to_string()))?;
                    ToolExecutionResult::success(vec![content])
                        .map_err(|error| ToolExecutorError::new(error.to_string()))
                }),
                Box::new(move |_| {
                    commits.fetch_add(1, Ordering::SeqCst);
                    ToolCommitOutcome::committed(mutation_result(&commit_diff, None)?)
                }),
            )?))
        })
    }
}

fn mutation_result(
    diff: &str,
    decline: Option<MutationDeclineReason>,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let (text, failure, committed) = match decline {
        None => ("file updated", None, true),
        Some(MutationDeclineReason::PolicyDenied) => (
            "file change denied by policy",
            Some(("PolicyError", "POLICY_DENIED")),
            false,
        ),
        Some(MutationDeclineReason::ApprovalRejected) => (
            "file change rejected",
            Some(("ApprovalError", "APPROVAL_REJECTED")),
            false,
        ),
        Some(MutationDeclineReason::ApprovalCancelled) => (
            "file change approval cancelled",
            Some(("AbortError", "APPROVAL_CANCELLED")),
            false,
        ),
        Some(MutationDeclineReason::ApprovalUnavailable) => (
            "file change approval unavailable",
            Some(("ApprovalError", "APPROVAL_UNAVAILABLE")),
            false,
        ),
        Some(MutationDeclineReason::AbortedBeforeDispatch) => (
            "file change was cancelled before commit",
            Some(("AbortError", "ABORTED_BEFORE_DISPATCH")),
            false,
        ),
        Some(MutationDeclineReason::Aborted) => (
            "file change aborted",
            Some(("AbortError", "ABORTED")),
            false,
        ),
        Some(MutationDeclineReason::OutputBudgetExceeded) => (
            "file result did not fit",
            Some(("ToolError", "TOOL_OUTPUT_BUDGET_EXCEEDED")),
            false,
        ),
    };
    let content = vec![ContentBlock::text(text).unwrap()];
    let error = failure.map(|(name, code)| ToolFailure {
        name: name.to_owned(),
        code: code.to_owned(),
    });
    ToolExecutionResult::new(
        content,
        error.is_some(),
        error,
        Some(JsonValue::new(json!({ "diff": diff, "committed": committed })).unwrap()),
        false,
    )
    .map_err(|error| ToolExecutorError::new(error.to_string()))
}

fn prepared_with_result_bound(
    maximum_result_event_bytes: usize,
) -> Result<PreparedToolMutation, ToolExecutorError> {
    let diff = "--- a/file.txt\n+++ b/file.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let decline_diff = diff.to_owned();
    let commit_diff = diff.to_owned();
    PreparedToolMutation::new(
        ApprovalPrompt::new(None, diff)
            .map_err(|error| ToolExecutorError::new(error.to_string()))?,
        maximum_result_event_bytes,
        Box::new(move |reason| mutation_result(&decline_diff, Some(reason))),
        Box::new(move |_| ToolCommitOutcome::committed(mutation_result(&commit_diff, None)?)),
    )
}

#[test]
fn approval_and_commit_constructors_reject_ambiguous_public_states() {
    assert!(ApprovalPrompt::new(None, "").is_err());
    assert!(ApprovalPrompt::new(None, "x".repeat(MAX_APPROVAL_PREVIEW_BYTES)).is_ok());
    assert!(ApprovalPrompt::new(None, "x".repeat(MAX_APPROVAL_PREVIEW_BYTES + 1)).is_err());
    assert!(
        ApprovalPrompt::new(Some("x".repeat(MAX_APPROVAL_REASON_BYTES)), "safe preview").is_ok()
    );
    assert!(
        ApprovalPrompt::new(
            Some("x".repeat(MAX_APPROVAL_REASON_BYTES + 1)),
            "safe preview"
        )
        .is_err()
    );
    assert!(ApprovalPrompt::new(Some("unsafe\0reason".to_owned()), "safe preview").is_err());
    assert!(ApprovalPrompt::new(Some("line one\n\tline two".to_owned()), "safe preview").is_ok());

    let prompt = ApprovalPrompt::new(
        Some("SECRET_APPROVAL_REASON".to_owned()),
        "SECRET_APPROVAL_PREVIEW",
    )
    .unwrap();
    let debug = format!("{prompt:?}");
    assert!(!debug.contains("SECRET_APPROVAL_REASON"));
    assert!(!debug.contains("SECRET_APPROVAL_PREVIEW"));

    assert!(prepared_with_result_bound(0).is_err());
    assert!(prepared_with_result_bound(MAX_AGENT_COMMITTED_TOOL_RESULT_EVENT_BYTES).is_ok());
    assert!(prepared_with_result_bound(MAX_AGENT_COMMITTED_TOOL_RESULT_EVENT_BYTES + 1).is_err());

    let committed_false =
        mutation_result("fixture", Some(MutationDeclineReason::PolicyDenied)).unwrap();
    assert!(ToolCommitOutcome::committed(committed_false).is_err());

    let committed_true = mutation_result("fixture", None).unwrap();
    assert!(ToolCommitOutcome::not_committed(committed_true).is_err());

    let missing_marker = ToolExecutionResult::model_error(
        vec![ContentBlock::text("error").unwrap()],
        ToolFailure {
            name: "ToolError".to_owned(),
            code: "FAILED".to_owned(),
        },
    )
    .unwrap();
    assert!(ToolCommitOutcome::not_committed(missing_marker).is_err());
    let non_boolean_marker = ToolExecutionResult::new(
        vec![ContentBlock::text("error").unwrap()],
        true,
        Some(ToolFailure {
            name: "ToolError".to_owned(),
            code: "FAILED".to_owned(),
        }),
        Some(JsonValue::new(json!({ "committed": "false" })).unwrap()),
        false,
    )
    .unwrap();
    assert!(ToolCommitOutcome::not_committed(non_boolean_marker).is_err());
}

#[tokio::test]
async fn an_invalid_decline_result_never_claims_commit_or_publishes_a_false_result() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response(&["call-1"])]));
    let commits = Arc::new(AtomicUsize::new(0));
    let mut agent = agent(
        "invalid-decline",
        provider.clone(),
        Arc::new(InvalidDeclineMutations {
            commits: commits.clone(),
        }),
        FileChangePolicy::Deny,
        Arc::new(RecordingApprovalProvider::default()),
    );
    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("invalid decline must close as an infrastructure error")
    };
    assert_eq!(error.code(), "AGENT_TOOL_EXECUTOR");
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
    );
    assert!(matches!(
        agent
            .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
            .await,
        Err(deepseek_harness_cli::agent::AgentLoopError::Poisoned)
    ));
    assert_eq!(provider.requests().len(), 1);
}

fn schema() -> ToolSchema {
    ToolSchema::new(
        "apply_patch",
        "Apply one approved patch.",
        JsonValue::new(json!({
            "type": "object",
            "properties": { "patch": { "type": "string" } },
            "required": ["patch"],
            "additionalProperties": false
        }))
        .unwrap(),
    )
    .unwrap()
}

fn user() -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text("change it").unwrap()],
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

fn tool_response(call_ids: &[&str]) -> Vec<StreamChunk> {
    let mut chunks = Vec::new();
    for (index, call_id) in call_ids.iter().enumerate() {
        let index = u64::try_from(index).unwrap();
        chunks.push(StreamChunk::block_start(index, ContentBlockType::ToolCall).unwrap());
        chunks.push(
            StreamChunk::block_end(
                index,
                ContentBlock::tool_call(*call_id, "apply_patch", r#"{"patch":"fixture"}"#).unwrap(),
            )
            .unwrap(),
        );
    }
    chunks.push(StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap());
    chunks
}

fn agent(
    id: &str,
    provider: Arc<ScriptedProvider>,
    tools: Arc<dyn ToolExecutor>,
    policy: FileChangePolicy,
    approvals: Arc<dyn ApprovalProvider>,
) -> AgentLoop {
    let session = Session::with_clock(id, IncrementingClock(Mutex::new(1_000))).unwrap();
    agent_with_session(session, provider, tools, policy, approvals, None)
}

fn agent_with_session(
    session: Session,
    provider: Arc<ScriptedProvider>,
    tools: Arc<dyn ToolExecutor>,
    policy: FileChangePolicy,
    approvals: Arc<dyn ApprovalProvider>,
    limits: Option<AgentLimits>,
) -> AgentLoop {
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(vec![schema()])
        .unwrap()
        .with_file_change_approval(policy, approvals);
    let config = if let Some(limits) = limits {
        config.with_limits(limits)
    } else {
        config
    };
    AgentLoop::with_runtime(
        session,
        provider,
        tools,
        Arc::new(FixedRuntime::default()),
        config,
    )
    .unwrap()
}

#[tokio::test]
async fn allow_and_deny_never_call_the_approval_provider() {
    for (policy, expected_commits, expected_code) in [
        (FileChangePolicy::Allow, 1, None),
        (FileChangePolicy::Deny, 0, Some("POLICY_DENIED")),
    ] {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response(&["call-1"]),
            text_response(),
        ]));
        let commits = Arc::new(Mutex::new(Vec::new()));
        let tools = Arc::new(PreparedMutations::new(commits.clone()));
        let approvals = Arc::new(RecordingApprovalProvider::default());
        let mut agent = agent(
            "approval-policy",
            provider,
            tools,
            policy,
            approvals.clone(),
        );
        agent
            .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(commits.lock().unwrap().len(), expected_commits);
        assert!(approvals.requests.lock().unwrap().is_empty());
        assert!(!agent.session().events().iter().any(|event| {
            matches!(
                event.kind(),
                EventKind::ApprovalAsked { .. } | EventKind::ApprovalDecided { .. }
            )
        }));
        if let Some(code) = expected_code {
            assert!(agent.session().events().iter().any(|event| matches!(
                event.kind(),
                EventKind::ToolResult { error: Some(error), .. } if error.code == code
            )));
        }
    }
}

#[tokio::test]
async fn ask_allowed_audits_before_commit_and_replays_the_result() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(&["call-1"]),
        text_response(),
    ]));
    let commits = Arc::new(Mutex::new(Vec::new()));
    let clock_calls = Arc::new(AtomicUsize::new(0));
    let session = Session::with_clock(
        "approval-allowed",
        CountingClock {
            calls: clock_calls.clone(),
        },
    )
    .unwrap();
    let clock_origin = clock_calls.load(Ordering::SeqCst);
    let events_seen_at_commit = Arc::new(AtomicUsize::new(usize::MAX));
    let observed_clock = clock_calls.clone();
    let observed_events = events_seen_at_commit.clone();
    let tools = Arc::new(PreparedMutations::with_before_commit(
        commits.clone(),
        Arc::new(move || {
            observed_events.store(
                observed_clock.load(Ordering::SeqCst) - clock_origin,
                Ordering::SeqCst,
            );
        }),
    ));
    let approvals = Arc::new(RecordingApprovalProvider::with(vec![Ok(
        ApprovalOutcome::AllowedOnce,
    )]));
    let mut agent = agent_with_session(
        session,
        provider.clone(),
        tools,
        FileChangePolicy::Ask,
        approvals.clone(),
        None,
    );
    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(commits.lock().unwrap().as_slice(), ["call-1"]);
    let requests = approvals.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].tool_name(), "apply_patch");
    assert_eq!(requests[0].call_id().as_str(), "call-1");
    assert!(requests[0].preview().contains("+++ b/call-1.txt"));
    let approval_preview = requests[0].preview().to_owned();
    drop(requests);

    let result_diff = agent
        .session()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::ToolResult {
                meta: Some(meta), ..
            } => meta
                .as_value()
                .get("diff")
                .and_then(serde_json::Value::as_str),
            _ => None,
        })
        .unwrap();
    assert_eq!(result_diff, approval_preview);

    let relevant = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::AssistantMessage { .. }
            | EventKind::ToolCall { .. }
            | EventKind::ApprovalAsked { .. }
            | EventKind::ApprovalDecided { .. }
            | EventKind::ToolResult { .. }
            | EventKind::StepEnd { .. } => Some(event.kind().event_type()),
            _ => None,
        })
        .take(6)
        .collect::<Vec<_>>();
    assert_eq!(
        relevant,
        [
            "assistant/message",
            "tool/call",
            "approval/asked",
            "approval/decided",
            "tool/result",
            "step/end",
        ]
    );
    let decided_index = agent
        .session()
        .events()
        .iter()
        .position(|event| matches!(event.kind(), EventKind::ApprovalDecided { .. }))
        .unwrap();
    let result_index = agent
        .session()
        .events()
        .iter()
        .position(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
        .unwrap();
    assert!(decided_index < events_seen_at_commit.load(Ordering::SeqCst));
    assert_eq!(events_seen_at_commit.load(Ordering::SeqCst), result_index);
    assert_eq!(provider.requests().len(), 2);
    assert!(provider.requests()[1].iter().any(|message| {
        message.content().iter().any(|block| {
            matches!(
                block.kind(),
                deepseek_harness_cli::model::ContentBlockKind::ToolResult { .. }
            )
        })
    }));
}

#[tokio::test]
async fn ask_non_grants_never_commit() {
    for (outcome, expected_code, expected_outcome) in [
        (
            Ok(ApprovalOutcome::Rejected),
            "APPROVAL_REJECTED",
            ApprovalOutcome::Rejected,
        ),
        (
            Ok(ApprovalOutcome::Unavailable),
            "APPROVAL_UNAVAILABLE",
            ApprovalOutcome::Unavailable,
        ),
        (
            Ok(ApprovalOutcome::Cancelled),
            "APPROVAL_CANCELLED",
            ApprovalOutcome::Cancelled,
        ),
        (
            Err(ApprovalProviderError::new("SECRET_APPROVAL_PROVIDER")),
            "APPROVAL_UNAVAILABLE",
            ApprovalOutcome::Unavailable,
        ),
    ] {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response(&["call-1"]),
            text_response(),
        ]));
        let commits = Arc::new(Mutex::new(Vec::new()));
        let approvals = Arc::new(RecordingApprovalProvider::with(vec![outcome]));
        let mut agent = agent(
            "approval-no-grant",
            provider,
            Arc::new(PreparedMutations::new(commits.clone())),
            FileChangePolicy::Ask,
            approvals,
        );
        agent
            .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
            .await
            .unwrap();
        assert!(commits.lock().unwrap().is_empty());
        assert!(agent.session().events().iter().any(|event| matches!(
            event.kind(),
            EventKind::ToolResult { error: Some(error), .. } if error.code == expected_code
        )));
        assert!(agent.session().events().iter().any(|event| matches!(
            event.kind(),
            EventKind::ApprovalDecided { decided } if decided.outcome() == expected_outcome
        )));
        assert!(
            !agent
                .session()
                .to_json()
                .unwrap()
                .contains("SECRET_APPROVAL_PROVIDER")
        );
    }
}

#[tokio::test]
async fn approval_provider_panics_become_unavailable_without_committing() {
    for during_poll in [false, true] {
        let provider = Arc::new(ScriptedProvider::new(vec![
            tool_response(&["call-1"]),
            text_response(),
        ]));
        let commits = Arc::new(Mutex::new(Vec::new()));
        let mut agent = agent(
            "approval-panic",
            provider,
            Arc::new(PreparedMutations::new(commits.clone())),
            FileChangePolicy::Ask,
            Arc::new(PanicApproval { during_poll }),
        );

        agent
            .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
            .await
            .unwrap();

        assert!(commits.lock().unwrap().is_empty());
        assert!(agent.session().events().iter().any(|event| matches!(
            event.kind(),
            EventKind::ApprovalDecided { decided }
                if decided.outcome() == ApprovalOutcome::Unavailable
        )));
        assert!(agent.session().events().iter().any(|event| matches!(
            event.kind(),
            EventKind::ToolResult { error: Some(error), .. }
                if error.code == "APPROVAL_UNAVAILABLE"
        )));
        let durable = agent.session().to_json().unwrap();
        assert!(!durable.contains("synthetic approval factory panic"));
        assert!(!durable.contains("synthetic approval future panic"));
    }
}

#[tokio::test]
async fn mutation_result_budget_fails_before_asking_or_committing() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(&["call-1"]),
        text_response(),
    ]));
    let commits = Arc::new(Mutex::new(Vec::new()));
    let approvals = Arc::new(RecordingApprovalProvider::with(vec![Ok(
        ApprovalOutcome::AllowedOnce,
    )]));
    let limits = AgentLimits::default()
        .with_max_tool_result_bytes(1_024)
        .unwrap();
    let session =
        Session::with_clock("approval-budget", IncrementingClock(Mutex::new(1_000))).unwrap();
    let mut agent = agent_with_session(
        session,
        provider,
        Arc::new(PreparedMutations::new(commits.clone())),
        FileChangePolicy::Ask,
        approvals.clone(),
        Some(limits),
    );

    agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert!(commits.lock().unwrap().is_empty());
    assert!(approvals.requests.lock().unwrap().is_empty());
    assert!(!agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ApprovalAsked { .. } | EventKind::ApprovalDecided { .. }
    )));
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. }
            if error.code == "TOOL_OUTPUT_BUDGET_EXCEEDED"
    )));
}

#[tokio::test]
async fn cancellation_while_asking_discards_a_late_allow_and_never_commits() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response(&["call-1"])]));
    let commits = Arc::new(Mutex::new(Vec::new()));
    let entered = Arc::new(Semaphore::new(0));
    let child_cancelled = Arc::new(AtomicBool::new(false));
    let approvals = Arc::new(LateAllowApproval {
        entered: entered.clone(),
        child_cancelled: child_cancelled.clone(),
    });
    let mut agent = agent(
        "approval-cancelled",
        provider,
        Arc::new(PreparedMutations::new(commits.clone())),
        FileChangePolicy::Ask,
        approvals,
    );
    let cancellation = CancellationToken::new();
    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before approval was requested: {result:?}"),
        }
        cancellation.cancel();
        turn.await.unwrap()
    };

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert!(commits.lock().unwrap().is_empty());
    assert!(child_cancelled.load(Ordering::SeqCst));
    assert_eq!(
        agent
            .session()
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                EventKind::ApprovalDecided { decided } => Some(decided.outcome()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [ApprovalOutcome::Cancelled]
    );
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. }
            if error.code == "ABORTED_BEFORE_DISPATCH"
    )));
}

#[tokio::test]
async fn cancellation_during_commit_waits_for_and_persists_the_committed_fact() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response(&["call-1"])]));
    let (tools, entered, gate, child_cancelled, commits) =
        blocking_mutations(BlockingCommitDisposition::Committed);
    let mut agent = agent(
        "commit-cancelled",
        provider,
        tools,
        FileChangePolicy::Allow,
        Arc::new(RecordingApprovalProvider::default()),
    );
    let cancellation = CancellationToken::new();
    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before mutation commit started: {result:?}"),
        }
        cancellation.cancel();
        gate.release();
        turn.await.unwrap()
    };

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(child_cancelled.load(Ordering::SeqCst));
    let (error, committed) = agent
        .session()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::ToolResult { error, meta, .. } => Some((
                error.as_ref(),
                meta.as_ref().and_then(|meta| {
                    meta.as_value()
                        .get("committed")
                        .and_then(serde_json::Value::as_bool)
                }),
            )),
            _ => None,
        })
        .unwrap();
    assert!(error.is_none());
    assert_eq!(committed, Some(true));
}

#[tokio::test(start_paused = true)]
async fn tool_timeout_waits_for_a_definite_not_committed_fact() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(&["call-1"]),
        text_response(),
    ]));
    let (tools, entered, gate, child_cancelled, commits) =
        blocking_mutations(BlockingCommitDisposition::NotCommitted);
    let limits = AgentLimits::default()
        .with_tool_duration(Duration::from_millis(1))
        .unwrap();
    let mut agent = agent_with_session(
        Session::with_clock(
            "commit-timeout-not-committed",
            IncrementingClock(Mutex::new(1_000)),
        )
        .unwrap(),
        provider.clone(),
        tools,
        FileChangePolicy::Allow,
        Arc::new(RecordingApprovalProvider::default()),
        Some(limits),
    );
    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before mutation commit started: {result:?}"),
        }
        tokio::time::advance(Duration::from_millis(2)).await;
        std::future::poll_fn(|context| {
            if std::future::Future::poll(turn.as_mut(), context).is_ready() {
                panic!("turn ended before the blocked mutation was released");
            }
            std::task::Poll::Ready(())
        })
        .await;
        gate.release();
        turn.await.unwrap()
    };

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(child_cancelled.load(Ordering::SeqCst));
    assert_eq!(provider.requests().len(), 2);
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult {
            error: Some(error),
            meta: Some(meta),
            ..
        } if error.code == "TOOL_TIMEOUT"
            && meta.as_value()["committed"] == serde_json::Value::Bool(false)
    )));
}

#[tokio::test(start_paused = true)]
async fn tool_timeout_cannot_rewrite_a_late_committed_fact() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(&["call-1"]),
        text_response(),
    ]));
    let (tools, entered, gate, child_cancelled, commits) =
        blocking_mutations(BlockingCommitDisposition::Committed);
    let limits = AgentLimits::default()
        .with_tool_duration(Duration::from_millis(1))
        .unwrap();
    let mut agent = agent_with_session(
        Session::with_clock(
            "commit-timeout-committed",
            IncrementingClock(Mutex::new(1_000)),
        )
        .unwrap(),
        provider.clone(),
        tools,
        FileChangePolicy::Allow,
        Arc::new(RecordingApprovalProvider::default()),
        Some(limits),
    );
    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before mutation commit started: {result:?}"),
        }
        tokio::time::advance(Duration::from_millis(2)).await;
        std::future::poll_fn(|context| {
            if std::future::Future::poll(turn.as_mut(), context).is_ready() {
                panic!("turn ended before the blocked mutation was released");
            }
            std::task::Poll::Ready(())
        })
        .await;
        gate.release();
        turn.await.unwrap()
    };

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(child_cancelled.load(Ordering::SeqCst));
    assert_eq!(provider.requests().len(), 2);
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult {
            error: None,
            meta: Some(meta),
            ..
        } if meta.as_value()["committed"] == serde_json::Value::Bool(true)
    )));
}

#[test]
fn agent_rebuild_refuses_an_unmatched_approval_tail() {
    let mut session =
        Session::with_clock("approval-crash-tail", IncrementingClock(Mutex::new(1_000))).unwrap();
    session
        .append(NewEvent::log(EventKind::turn_start(
            TurnId::new(1).unwrap(),
        )))
        .unwrap();
    session
        .append(NewEvent::log(EventKind::approval_asked(
            ApprovalAskedEvent::new(
                ApprovalRequestId::new("approval-pending"),
                "apply_patch",
                None,
                Some("pending change".to_owned()),
            )
            .unwrap(),
        )))
        .unwrap();

    let result = AgentLoop::with_runtime(
        session,
        Arc::new(ScriptedProvider::new(Vec::new())),
        Arc::new(PreparedMutations::new(Arc::new(Mutex::new(Vec::new())))),
        Arc::new(FixedRuntime::default()),
        AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
            .with_tools(vec![schema()])
            .unwrap(),
    );
    assert!(matches!(result, Err(AgentBuildError::UnresolvedApproval)));
}

#[tokio::test]
async fn two_asked_mutations_keep_real_call_sequences_and_distinct_ids() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(&["call-1", "call-2"]),
        text_response(),
    ]));
    let commits = Arc::new(Mutex::new(Vec::new()));
    let approvals = Arc::new(RecordingApprovalProvider::with(vec![
        Ok(ApprovalOutcome::AllowedOnce),
        Ok(ApprovalOutcome::AllowedOnce),
    ]));
    let mut agent = agent(
        "approval-two",
        provider,
        Arc::new(PreparedMutations::new(commits)),
        FileChangePolicy::Ask,
        approvals,
    );
    agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    let asked_ids = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::ApprovalAsked { asked } => Some(asked.id().as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(asked_ids.len(), 2);
    assert_ne!(asked_ids[0], asked_ids[1]);
    for result in agent
        .session()
        .events()
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
    {
        let [source] = result.source_event_seqs().unwrap() else {
            panic!("tool result must cite exactly one call")
        };
        let EventKind::ToolResult { message, .. } = result.kind() else {
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
        let EventKind::ToolCall { call_id, .. } =
            agent.session().events()[usize::try_from(source.get()).unwrap()].kind()
        else {
            panic!("tool result source must be a tool call")
        };
        assert_eq!(result_call_id, call_id);
    }
}
