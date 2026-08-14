use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::stream;
use serde_json::json;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::{
    AgentIdKind, AgentLimits, AgentLoop, AgentLoopConfig, AgentLoopError, AgentRuntime,
    ApprovalFuture, ApprovalPrompt, ApprovalProvider, ApprovalRequest, NoApprovalProvider,
    ShellPolicy, ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
    ToolExecutorError, ToolPreparation, ToolPreparationFuture, TurnProposal,
    tool::{
        ActionDeclineReason, PreparedToolAction, PreparedToolActionSetup, ToolActionControl,
        ToolActionOutcome, ToolActionSetupOutcome, ToolActionTurnStop, ToolClaimProfile,
    },
};
use crate::{
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, JsonValue, LlmCallConfig,
        LlmCallConfigAdapterDefaults, MAX_JSON_VALUE_BYTES, Message, MessageSource, StreamChunk,
        ToolSchema,
    },
    provider::{
        ModelProvider, PreparedProviderCall, ProviderPrepareError, ProviderRequest, ProviderStream,
        RetryBackoff, RetryPolicy,
    },
    session::{
        ApprovalOutcome, Clock, ClockError, EventKind, EventSeq, MAX_SESSION_RETAINED_JSON_BYTES,
        NewEvent, Session, StepId, SurfaceIntent, ToolFailure, TurnEndReason, TurnId, UnixMillis,
    },
};

const NORMAL_RESULT_BOUND: usize = 128 * 1024;
const DELIBERATELY_FALSE_RESULT_BOUND: usize = 512;

#[derive(Default)]
struct FixedRuntime(Mutex<u64>);

impl AgentRuntime for FixedRuntime {
    fn next_id(&self, kind: AgentIdKind) -> Result<String, super::AgentRuntimeError> {
        let mut next = self.0.lock().unwrap();
        *next += 1;
        Ok(format!("{}-{next}", kind.prefix()))
    }

    fn sample_unit(&self) -> Result<f64, super::AgentRuntimeError> {
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
            Some(crate::model::NonNegativeSafeInteger::new(4_096).unwrap()),
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
                .expect("the action fixture must provide every model response")
                .into_iter()
                .map(Ok),
        ))
    }
}

enum ActionScript {
    SetupNotStarted,
    SlowSetup(SlowSetup),
    ActionNotStarted,
    Infrastructure,
    StartedAndQuiescent,
    StartedOwnershipLost,
    OversizedStartedResult,
    StopThenCleanup(StopThenCleanup),
}

struct StopThenCleanup {
    running: Arc<Semaphore>,
    cleanup_entered: Arc<Semaphore>,
    cleanup_release: Arc<Semaphore>,
}

#[derive(Clone, Copy)]
enum SlowSetupFinish {
    Ready,
    JoinPanic,
}

struct SlowSetup {
    finish: SlowSetupFinish,
    worker_started: Arc<Semaphore>,
    worker_release: Arc<BlockingGate>,
    join_observed: Arc<AtomicBool>,
    crossed_preparation_deadline: Arc<AtomicBool>,
    crossed_turn_deadline: Arc<AtomicBool>,
    cancellation_seen: Arc<AtomicBool>,
}

#[derive(Clone)]
struct SlowSetupProbe {
    worker_started: Arc<Semaphore>,
    worker_release: Arc<BlockingGate>,
    join_observed: Arc<AtomicBool>,
    crossed_preparation_deadline: Arc<AtomicBool>,
    crossed_turn_deadline: Arc<AtomicBool>,
    cancellation_seen: Arc<AtomicBool>,
}

impl SlowSetupProbe {
    fn new() -> Self {
        Self {
            worker_started: Arc::new(Semaphore::new(0)),
            worker_release: Arc::new(BlockingGate::default()),
            join_observed: Arc::new(AtomicBool::new(false)),
            crossed_preparation_deadline: Arc::new(AtomicBool::new(false)),
            crossed_turn_deadline: Arc::new(AtomicBool::new(false)),
            cancellation_seen: Arc::new(AtomicBool::new(false)),
        }
    }

    fn script(&self, finish: SlowSetupFinish) -> ActionScript {
        ActionScript::SlowSetup(SlowSetup {
            finish,
            worker_started: self.worker_started.clone(),
            worker_release: self.worker_release.clone(),
            join_observed: self.join_observed.clone(),
            crossed_preparation_deadline: self.crossed_preparation_deadline.clone(),
            crossed_turn_deadline: self.crossed_turn_deadline.clone(),
            cancellation_seen: self.cancellation_seen.clone(),
        })
    }

    fn assert_crossed_every_boundary(&self) {
        assert!(self.join_observed.load(Ordering::SeqCst));
        assert!(self.crossed_preparation_deadline.load(Ordering::SeqCst));
        assert!(self.crossed_turn_deadline.load(Ordering::SeqCst));
        assert!(self.cancellation_seen.load(Ordering::SeqCst));
    }
}

#[derive(Default)]
struct BlockingGate {
    released: Mutex<bool>,
    changed: Condvar,
}

impl BlockingGate {
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

struct ScriptedActions {
    scripts: Mutex<VecDeque<ActionScript>>,
    run_count: Arc<AtomicUsize>,
}

impl ScriptedActions {
    fn one(script: ActionScript) -> (Arc<Self>, Arc<AtomicUsize>) {
        let run_count = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Self {
                scripts: Mutex::new(VecDeque::from([script])),
                run_count: run_count.clone(),
            }),
            run_count,
        )
    }
}

#[derive(Default)]
struct CountingAllowApproval {
    requests: AtomicUsize,
}

impl ApprovalProvider for CountingAllowApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ApprovalOutcome::AllowedOnce) })
    }
}

struct LateAllowApproval {
    requests: AtomicUsize,
    entered: Arc<Semaphore>,
    returned_allow: Arc<AtomicBool>,
}

impl ApprovalProvider for LateAllowApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        let entered = self.entered.clone();
        let returned_allow = self.returned_allow.clone();
        Box::pin(async move {
            entered.add_permits(1);
            cancellation.cancelled().await;
            returned_allow.store(true, Ordering::SeqCst);
            Ok(ApprovalOutcome::AllowedOnce)
        })
    }
}

struct PanicClaimProfile;

impl ToolExecutor for PanicClaimProfile {
    fn claim_profile(&self, _tool_name: &str) -> ToolClaimProfile {
        panic!("SECRET_CLAIM_PROFILE_PANIC")
    }

    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { panic!("claim-profile panic must prevent execution") })
    }
}

impl ToolExecutor for ScriptedActions {
    fn claim_profile(&self, tool_name: &str) -> ToolClaimProfile {
        if tool_name == "bash" {
            ToolClaimProfile::shell_action()
        } else {
            ToolClaimProfile::standard()
        }
    }

    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("Action preparation is required")) })
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let script = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("one scripted Action outcome per call");
        let setup_dispatch = request.dispatch_binding().clone();
        let action_dispatch = setup_dispatch.clone();
        let run_count = self.run_count.clone();
        Box::pin(async move {
            let setup = PreparedToolActionSetup::new(
                setup_dispatch,
                Box::new(move |control| {
                    Box::pin(async move {
                        if matches!(&script, ActionScript::SetupNotStarted) {
                            return ToolActionSetupOutcome::NotStarted {
                                turn_stop: ToolActionTurnStop::None,
                                result: not_started_result("SHELL_WORKDIR_CHANGED"),
                            };
                        }

                        let script = match script {
                            ActionScript::SlowSetup(fixture) => {
                                finish_slow_setup(fixture, control).await
                            }
                            script => script,
                        };

                        let maximum_result_event_bytes =
                            if matches!(&script, ActionScript::OversizedStartedResult) {
                                DELIBERATELY_FALSE_RESULT_BOUND
                            } else {
                                NORMAL_RESULT_BOUND
                            };
                        let action = PreparedToolAction::new(
                            action_dispatch,
                            ApprovalPrompt::new(
                                Some("run one foreground command".to_owned()),
                                "bash: fixture",
                            )
                            .unwrap(),
                            maximum_result_event_bytes,
                            Box::new(|reason| Ok(declined_result(reason))),
                            Box::new(move |control| {
                                run_count.fetch_add(1, Ordering::SeqCst);
                                run_action_script(script, control)
                            }),
                        )
                        .unwrap();
                        ToolActionSetupOutcome::Ready(action)
                    })
                }),
            )?;
            Ok(ToolPreparation::Action(setup))
        })
    }
}

fn run_action_script(
    script: ActionScript,
    control: ToolActionControl,
) -> Pin<Box<dyn Future<Output = ToolActionOutcome> + Send + 'static>> {
    Box::pin(async move {
        match script {
            ActionScript::SetupNotStarted => unreachable!("setup consumes this script"),
            ActionScript::SlowSetup(_) => unreachable!("setup resolves this script"),
            ActionScript::ActionNotStarted => ToolActionOutcome::NotStarted {
                turn_stop: ToolActionTurnStop::None,
                result: not_started_result("SHELL_SPAWN_FAILED"),
            },
            ActionScript::Infrastructure => ToolActionOutcome::Infrastructure {
                turn_stop: ToolActionTurnStop::None,
            },
            ActionScript::StartedAndQuiescent => ToolActionOutcome::StartedAndQuiescent {
                turn_stop: ToolActionTurnStop::None,
                result: started_result("small started result"),
            },
            ActionScript::StartedOwnershipLost => ToolActionOutcome::StartedOwnershipLost {
                turn_stop: ToolActionTurnStop::None,
            },
            ActionScript::OversizedStartedResult => ToolActionOutcome::StartedAndQuiescent {
                turn_stop: ToolActionTurnStop::None,
                result: started_result(&"x".repeat(4 * 1024)),
            },
            ActionScript::StopThenCleanup(fixture) => {
                fixture.running.add_permits(1);
                let turn_stop = tokio::select! {
                    biased;
                    _ = control.cancellation().cancelled() => {
                        ToolActionTurnStop::CallerCancelled
                    }
                    _ = tokio::time::sleep_until(control.turn_deadline()) => {
                        ToolActionTurnStop::TurnTimeout
                    }
                };
                fixture.cleanup_entered.add_permits(1);
                drop(fixture.cleanup_release.acquire().await.unwrap());
                ToolActionOutcome::StartedAndQuiescent {
                    turn_stop,
                    result: started_abort_result(turn_stop),
                }
            }
        }
    })
}

async fn finish_slow_setup(
    fixture: SlowSetup,
    control: super::ToolActionSetupControl,
) -> ActionScript {
    let finish = fixture.finish;
    let worker_started = fixture.worker_started.clone();
    let worker_release = fixture.worker_release.clone();
    let job = tokio::task::spawn_blocking(move || {
        worker_started.add_permits(1);
        worker_release.wait();
        if matches!(finish, SlowSetupFinish::JoinPanic) {
            panic!("SECRET_SLOW_SETUP_WORKER_PANIC");
        }
    });

    let joined = job.await;
    fixture.join_observed.store(true, Ordering::SeqCst);
    fixture.crossed_preparation_deadline.store(
        tokio::time::Instant::now() >= control.preparation_deadline(),
        Ordering::SeqCst,
    );
    fixture.crossed_turn_deadline.store(
        tokio::time::Instant::now() >= control.turn_deadline(),
        Ordering::SeqCst,
    );
    fixture
        .cancellation_seen
        .store(control.cancellation().is_cancelled(), Ordering::SeqCst);
    joined.expect("the synthetic setup worker must join without panicking");
    ActionScript::StartedAndQuiescent
}

fn declined_result(reason: ActionDeclineReason) -> ToolExecutionResult {
    let code = match reason {
        ActionDeclineReason::PolicyDenied => "SHELL_POLICY_DENIED",
        ActionDeclineReason::ApprovalRejected => "APPROVAL_REJECTED",
        ActionDeclineReason::ApprovalCancelled => "APPROVAL_CANCELLED",
        ActionDeclineReason::ApprovalUnavailable => "APPROVAL_UNAVAILABLE",
        ActionDeclineReason::AbortedBeforeDispatch => "ABORTED_BEFORE_DISPATCH",
        ActionDeclineReason::OutputBudgetExceeded => "TOOL_OUTPUT_BUDGET_EXCEEDED",
    };
    not_started_result(code)
}

fn not_started_result(code: &'static str) -> ToolExecutionResult {
    shell_result(
        "not started",
        Some(code),
        json!({
            "kind": "foreground",
            "started": false,
            "exitCode": null,
            "signal": null
        }),
    )
}

fn started_result(text: &str) -> ToolExecutionResult {
    shell_result(text, None, started_meta(false))
}

fn started_abort_result(stop: ToolActionTurnStop) -> ToolExecutionResult {
    let message = match stop {
        ToolActionTurnStop::CallerCancelled => "cancelled command was cleaned up",
        ToolActionTurnStop::TurnTimeout => "turn-expired command was cleaned up",
        ToolActionTurnStop::None => unreachable!("the cleanup fixture requires a stop"),
    };
    shell_result(message, Some("ABORTED"), started_meta(true))
}

fn started_meta(aborted: bool) -> serde_json::Value {
    json!({
        "kind": "foreground",
        "started": true,
        "exitCode": 0,
        "signal": null,
        "timedOut": false,
        "aborted": aborted,
        "outputLimitExceeded": false,
        "pipeSetupFailed": false,
        "pipeReadFailed": false,
        "signalDeliveryFailed": false,
        "pipeDrainTimedOut": false,
        "timeoutMs": 25_000,
        "workdir": ".",
        "stdoutTruncated": false,
        "stderrTruncated": false
    })
}

fn shell_result(
    text: &str,
    failure_code: Option<&'static str>,
    meta: serde_json::Value,
) -> ToolExecutionResult {
    let error = failure_code.map(|code| ToolFailure {
        name: "bash".to_owned(),
        code: code.to_owned(),
    });
    ToolExecutionResult::new(
        vec![ContentBlock::text(text).unwrap()],
        error.is_some(),
        error,
        Some(JsonValue::new(meta).unwrap()),
        false,
    )
    .unwrap()
}

fn padded_session_with_remaining(id: &str, target_remaining: usize) -> Session {
    assert!(target_remaining > 1_024);
    let initial_each = (MAX_SESSION_RETAINED_JSON_BYTES - target_remaining) / 2 - 512;
    let mut first = initial_each;
    let mut second = initial_each;
    let maximum_padding = MAX_JSON_VALUE_BYTES - r#"{"padding":""}"#.len();

    for _ in 0..4 {
        assert!(first <= maximum_padding && second <= maximum_padding);
        let snapshot = json!({
            "header": { "version": 0, "id": id, "createdAt": 1 },
            "events": [
                {
                    "type": "test/padding-a",
                    "seq": 0,
                    "time": 1,
                    "data": { "padding": "x".repeat(first) },
                    "ignorable": true
                },
                {
                    "type": "test/padding-b",
                    "seq": 1,
                    "time": 2,
                    "data": { "padding": "x".repeat(second) },
                    "ignorable": true
                },
                {
                    "type": "session/end-seed",
                    "seq": 2,
                    "time": 3,
                    "data": {}
                }
            ]
        })
        .to_string();
        let session = Session::from_json(&snapshot, IncrementingClock(Mutex::new(10))).unwrap();
        let remaining = session.remaining_budget().remaining_retained_json_bytes;
        if remaining == target_remaining {
            return session;
        }
        if remaining > target_remaining {
            let grow = remaining - target_remaining;
            let second_room = maximum_padding - second;
            let second_grow = grow.min(second_room);
            second += second_grow;
            first += grow - second_grow;
        } else {
            let shrink = target_remaining - remaining;
            let second_shrink = shrink.min(second);
            second -= second_shrink;
            first -= shrink - second_shrink;
        }
    }
    panic!("the padding fixture did not reach its exact retained-byte target")
}

fn schema() -> ToolSchema {
    ToolSchema::new(
        "bash",
        "Run one bounded foreground command.",
        JsonValue::new(json!({
            "type": "object",
            "properties": {
                "command": { "type": "string" },
                "description": { "type": "string" }
            },
            "required": ["command", "description"],
            "additionalProperties": false
        }))
        .unwrap(),
    )
    .unwrap()
}

fn user() -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text("run it").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn tool_response() -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(
            0,
            ContentBlock::tool_call(
                "call-1",
                "bash",
                r#"{"command":"printf fixture","description":"fixture"}"#,
            )
            .unwrap(),
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

fn agent(
    id: &str,
    provider: Arc<ScriptedProvider>,
    tools: Arc<dyn ToolExecutor>,
    limits: Option<AgentLimits>,
) -> AgentLoop {
    agent_with_policy(
        id,
        provider,
        tools,
        limits,
        ShellPolicy::Allow,
        Arc::new(NoApprovalProvider),
    )
}

fn agent_with_policy(
    id: &str,
    provider: Arc<ScriptedProvider>,
    tools: Arc<dyn ToolExecutor>,
    limits: Option<AgentLimits>,
    shell_policy: ShellPolicy,
    approval_provider: Arc<dyn ApprovalProvider>,
) -> AgentLoop {
    let mut config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(vec![schema()])
        .unwrap()
        .with_approval_provider(approval_provider)
        .with_shell_policy(shell_policy);
    if let Some(limits) = limits {
        config = config.with_limits(limits);
    }
    AgentLoop::with_runtime(
        Session::with_clock(id, IncrementingClock(Mutex::new(1_000))).unwrap(),
        provider,
        tools,
        Arc::new(FixedRuntime::default()),
        config,
    )
    .unwrap()
}

fn tool_results(agent: &AgentLoop) -> Vec<(&ToolFailure, &serde_json::Value)> {
    agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::ToolResult {
                error: Some(error),
                meta: Some(meta),
                ..
            } => Some((error, meta.as_value())),
            _ => None,
        })
        .collect()
}

fn all_result_meta(agent: &AgentLoop) -> Vec<&serde_json::Value> {
    agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::ToolResult {
                meta: Some(meta), ..
            } => Some(meta.as_value()),
            _ => None,
        })
        .collect()
}

async fn assert_no_result_and_poisoned(agent: &mut AgentLoop) {
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
    );
    let second = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await;
    assert!(matches!(second, Err(AgentLoopError::Poisoned)));
}

#[tokio::test]
async fn declared_action_capacity_is_rejected_before_approval_or_run() {
    let approvals = Arc::new(CountingAllowApproval::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(),
        text_response(),
    ]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::StartedAndQuiescent);
    let limits = AgentLimits::default()
        .with_max_tool_result_bytes(NORMAL_RESULT_BOUND - 1)
        .unwrap();
    let mut agent = agent_with_policy(
        "action-configured-output-budget",
        provider,
        tools,
        Some(limits),
        ShellPolicy::Ask,
        approvals.clone(),
    );

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(approvals.requests.load(Ordering::SeqCst), 0);
    assert_eq!(run_count.load(Ordering::SeqCst), 0);
    let results = tool_results(&agent);
    let [(error, meta)] = results.as_slice() else {
        panic!("the pre-dispatch output rejection must publish one result")
    };
    assert_eq!(error.code, "TOOL_OUTPUT_BUDGET_EXCEEDED");
    assert_eq!(meta["started"], json!(false));
    assert_eq!(meta["exitCode"], serde_json::Value::Null);
    assert_eq!(meta["signal"], serde_json::Value::Null);
}

#[tokio::test]
async fn caller_cancellation_wins_over_a_late_allow_without_running_the_action() {
    let approval_entered = Arc::new(Semaphore::new(0));
    let returned_allow = Arc::new(AtomicBool::new(false));
    let approvals = Arc::new(LateAllowApproval {
        requests: AtomicUsize::new(0),
        entered: approval_entered.clone(),
        returned_allow: returned_allow.clone(),
    });
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::StartedAndQuiescent);
    let mut agent = agent_with_policy(
        "action-late-allow-cancel",
        provider,
        tools,
        None,
        ShellPolicy::Ask,
        approvals.clone(),
    );
    let cancellation = CancellationToken::new();

    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = approval_entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before approval was requested: {result:?}"),
        }
        cancellation.cancel();
        turn.await.unwrap()
    };

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(approvals.requests.load(Ordering::SeqCst), 1);
    assert!(returned_allow.load(Ordering::SeqCst));
    assert_eq!(run_count.load(Ordering::SeqCst), 0);
    let decisions = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::ApprovalDecided { decided } => Some(decided.outcome()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(decisions, [ApprovalOutcome::Cancelled]);
    let results = tool_results(&agent);
    let [(error, meta)] = results.as_slice() else {
        panic!("cancelled approval must publish one pre-dispatch result")
    };
    assert_eq!(error.code, "ABORTED_BEFORE_DISPATCH");
    assert_eq!(meta["started"], json!(false));
}

#[tokio::test]
async fn claim_profile_panic_precedes_tool_round_admission_and_redacts_its_payload() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let mut agent = agent(
        "action-claim-profile-panic",
        provider,
        Arc::new(PanicClaimProfile),
        None,
    );

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("claim-profile panic must close the step as an infrastructure error")
    };
    assert_eq!(error.code(), "AGENT_TOOL_EXECUTOR");
    assert!(!agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::AssistantMessage { .. }
            | EventKind::ToolCall { .. }
            | EventKind::ToolResult { .. }
    )));
    assert!(
        agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantChunk { .. }))
    );
    assert!(
        !agent
            .session()
            .to_json()
            .unwrap()
            .contains("SECRET_CLAIM_PROFILE_PANIC")
    );
}

#[tokio::test]
async fn shell_prestart_claim_growth_failure_releases_the_whole_round_atomically() {
    let turn = TurnId::new(1).unwrap();
    let step = StepId::new(1).unwrap();
    let arguments = r#"{"command":"printf fixture","description":"fixture"}"#.to_owned();
    let call = super::ToolCall {
        id: "call-1".into(),
        name: "bash".to_owned(),
        arguments: arguments.clone(),
    };
    let assistant_message = Message::assistant(
        "message-1",
        vec![ContentBlock::tool_call("call-1", "bash", arguments).unwrap()],
        "mock",
        "model",
    )
    .unwrap();
    let assistant_without_sources = NewEvent::surface(
        EventKind::AssistantMessage {
            turn,
            step,
            message: assistant_message.clone(),
            usage: None,
        },
        SurfaceIntent::append(),
    );
    let maximum_source_seq = EventSeq::new(crate::session::MAX_SAFE_INTEGER).unwrap();
    let result_message_id = "message-2";
    let call_event = NewEvent::log(EventKind::tool_call(
        turn,
        step,
        call.id.clone(),
        call.name.clone(),
        call.arguments.clone(),
    ));
    let fallback = super::shell_prestart_error_event(
        turn,
        step,
        result_message_id,
        &call,
        maximum_source_seq,
        "TOOL_OUTPUT_BUDGET_EXCEEDED",
        "bash",
        "shell output could not fit safely in the session",
        None,
    )
    .unwrap();
    let ceiling = super::shell_prestart_claim_ceiling(
        turn,
        step,
        result_message_id,
        &call,
        maximum_source_seq,
    )
    .unwrap();
    let assistant_bytes = Session::event_retained_json_bytes(&assistant_without_sources).unwrap();
    let call_bytes = Session::event_retained_json_bytes(&call_event).unwrap();
    let fallback_bytes = Session::event_retained_json_bytes(&fallback).unwrap();
    assert!(ceiling > fallback_bytes + 1);
    let capacity_after_preamble = assistant_bytes + call_bytes + ceiling - 1;
    assert!(assistant_bytes + call_bytes + fallback_bytes <= capacity_after_preamble);

    let chunks = tool_response();
    let turn_start = NewEvent::log(EventKind::turn_start(turn));
    let step_start = NewEvent::log(EventKind::step_start(turn, step));
    let chunk_events = chunks
        .into_iter()
        .map(|chunk| NewEvent::log(EventKind::assistant_chunk(turn, step, chunk)))
        .collect::<Vec<_>>();
    let preamble_bytes = std::iter::once(&turn_start)
        .chain(std::iter::once(&step_start))
        .chain(chunk_events.iter())
        .map(|event| Session::event_retained_json_bytes(event).unwrap())
        .sum::<usize>();
    let mut session = padded_session_with_remaining(
        "action-shell-claim-grow",
        preamble_bytes + capacity_after_preamble,
    );
    let mut source_seqs = Vec::new();
    session.append(turn_start).unwrap();
    session.append(step_start).unwrap();
    for event in chunk_events {
        source_seqs.push(session.append(event).unwrap().seq());
    }
    assert_eq!(
        session.remaining_budget().remaining_retained_json_bytes,
        capacity_after_preamble
    );
    let assistant = NewEvent::surface(
        EventKind::AssistantMessage {
            turn,
            step,
            message: assistant_message,
            usage: None,
        },
        SurfaceIntent::append().with_sources(source_seqs),
    );

    let provider = ScriptedProvider::new(vec![]);
    let (tools, run_count) = ScriptedActions::one(ActionScript::StartedAndQuiescent);
    let runtime = FixedRuntime(Mutex::new(1));
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(vec![schema()])
        .unwrap()
        .with_shell_policy(ShellPolicy::Allow);
    let mut request_header_logged = true;
    let mut driver = super::Driver {
        provider: &provider,
        tools: tools.as_ref(),
        runtime: &runtime,
        config: &config,
        request_header_logged: &mut request_header_logged,
        counters: super::Counters::default(),
        deadline: tokio::time::Instant::now() + Duration::from_secs(30),
    };
    let budget_failure = super::failure_reason(
        "AGENT_EVENT_BUDGET",
        "the session has no safe room for another agent event",
    )
    .unwrap();
    let mut reservation = session.reservation();
    let resolution = super::commit_tool_round(
        &mut reservation,
        &mut driver,
        turn,
        step,
        assistant,
        vec![call],
        vec![ToolClaimProfile::shell_action()],
        &CancellationToken::new(),
        &budget_failure,
    )
    .await
    .unwrap();

    let super::StepOutcome::Error(error) = resolution.outcome else {
        panic!("claim growth failure must return the Agent event-budget error")
    };
    assert_eq!(error.code(), "AGENT_EVENT_BUDGET");
    assert_eq!(driver.counters.tool_calls, 0);
    assert_eq!(run_count.load(Ordering::SeqCst), 0);
    assert!(!reservation.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::AssistantMessage { .. }
            | EventKind::ToolCall { .. }
            | EventKind::ToolResult { .. }
    )));
    assert!(
        reservation
            .session()
            .events()
            .iter()
            .all(|event| { !event.data().as_value().to_string().contains("workdir") })
    );
}

#[tokio::test]
async fn setup_not_started_settles_a_truthful_result_without_polling_the_action() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(),
        text_response(),
    ]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::SetupNotStarted);
    let mut agent = agent("action-setup-not-started", provider, tools, None);

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(run_count.load(Ordering::SeqCst), 0);
    let results = tool_results(&agent);
    let [(error, meta)] = results.as_slice() else {
        panic!("setup NotStarted must publish exactly one result")
    };
    assert_eq!(error.code, "SHELL_WORKDIR_CHANGED");
    assert_eq!(meta["started"], json!(false));
    assert_eq!(meta["exitCode"], serde_json::Value::Null);
    assert_eq!(meta["signal"], serde_json::Value::Null);
}

#[tokio::test]
async fn action_not_started_is_a_result_but_infrastructure_is_an_unresolved_call() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(),
        text_response(),
    ]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::ActionNotStarted);
    let mut not_started = agent("action-not-started", provider, tools, None);
    let outcome = not_started
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(run_count.load(Ordering::SeqCst), 1);
    assert_eq!(tool_results(&not_started)[0].0.code, "SHELL_SPAWN_FAILED");
    assert_eq!(tool_results(&not_started)[0].1["started"], json!(false));

    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::Infrastructure);
    let mut infrastructure = agent("action-infrastructure", provider, tools, None);
    let outcome = infrastructure
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("Action Infrastructure must close the step as an error")
    };
    assert_eq!(error.code(), "AGENT_TOOL_EXECUTOR");
    assert_eq!(run_count.load(Ordering::SeqCst), 1);
    assert_no_result_and_poisoned(&mut infrastructure).await;
}

#[tokio::test]
async fn started_and_quiescent_is_preferred_and_replayed_but_ownership_loss_is_unresolved() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_response(),
        text_response(),
    ]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::StartedAndQuiescent);
    let mut settled = agent("action-started-settled", provider.clone(), tools, None);
    let outcome = settled
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(run_count.load(Ordering::SeqCst), 1);
    let settled_meta = all_result_meta(&settled);
    assert_eq!(settled_meta.len(), 1);
    assert_eq!(settled_meta[0], &started_meta(false));
    assert!(provider.requests()[1].iter().any(|message| {
        message.content().iter().any(|block| {
            matches!(block.kind(), ContentBlockKind::ToolResult { tool_call_id, .. }
                if tool_call_id.as_str() == "call-1")
        })
    }));

    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::StartedOwnershipLost);
    let mut lost = agent("action-started-lost", provider, tools, None);
    let outcome = lost
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(outcome.reason(), TurnEndReason::Error { .. }));
    assert_eq!(run_count.load(Ordering::SeqCst), 1);
    assert_no_result_and_poisoned(&mut lost).await;
}

#[tokio::test]
async fn a_started_result_that_breaks_its_claim_never_falls_back_to_placeholder_success() {
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let (tools, run_count) = ScriptedActions::one(ActionScript::OversizedStartedResult);
    let mut agent = agent("action-preferred-only", provider, tools, None);

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user()]), CancellationToken::new())
        .await
        .unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Error { .. }));
    assert_eq!(run_count.load(Ordering::SeqCst), 1);
    assert!(
        !agent
            .session()
            .to_json()
            .unwrap()
            .contains("TOOL_OUTPUT_BUDGET_EXCEEDED")
    );
    assert_no_result_and_poisoned(&mut agent).await;
}

#[tokio::test(start_paused = true)]
async fn caller_first_survives_cleanup_even_when_the_turn_deadline_arrives_later() {
    let running = Arc::new(Semaphore::new(0));
    let cleanup_entered = Arc::new(Semaphore::new(0));
    let cleanup_release = Arc::new(Semaphore::new(0));
    let (tools, _) = ScriptedActions::one(ActionScript::StopThenCleanup(StopThenCleanup {
        running: running.clone(),
        cleanup_entered: cleanup_entered.clone(),
        cleanup_release: cleanup_release.clone(),
    }));
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let limits = AgentLimits::default()
        .with_turn_duration(Duration::from_millis(10))
        .unwrap();
    let mut agent = agent("action-caller-first", provider, tools, Some(limits));
    let cancellation = CancellationToken::new();

    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = running.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before Action started: {result:?}"),
        }
        cancellation.cancel();
        tokio::select! {
            biased;
            permit = cleanup_entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before cleanup was released: {result:?}"),
        }
        tokio::time::advance(Duration::from_millis(20)).await;
        cleanup_release.add_permits(1);
        turn.await.unwrap()
    };

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(all_result_meta(&agent)[0]["started"], json!(true));
    assert_eq!(all_result_meta(&agent)[0]["aborted"], json!(true));
}

#[tokio::test(start_paused = true)]
async fn turn_deadline_first_survives_cleanup_even_when_caller_cancels_later() {
    let running = Arc::new(Semaphore::new(0));
    let cleanup_entered = Arc::new(Semaphore::new(0));
    let cleanup_release = Arc::new(Semaphore::new(0));
    let (tools, _) = ScriptedActions::one(ActionScript::StopThenCleanup(StopThenCleanup {
        running: running.clone(),
        cleanup_entered: cleanup_entered.clone(),
        cleanup_release: cleanup_release.clone(),
    }));
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let limits = AgentLimits::default()
        .with_turn_duration(Duration::from_millis(10))
        .unwrap();
    let mut agent = agent("action-turn-first", provider, tools, Some(limits));
    let cancellation = CancellationToken::new();

    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = running.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before Action started: {result:?}"),
        }
        tokio::time::advance(Duration::from_millis(11)).await;
        tokio::select! {
            biased;
            permit = cleanup_entered.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before cleanup was released: {result:?}"),
        }
        cancellation.cancel();
        cleanup_release.add_permits(1);
        turn.await.unwrap()
    };

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("the first observed turn deadline must remain the outer reason")
    };
    assert_eq!(error.code(), "AGENT_TURN_TIMEOUT");
    assert_eq!(all_result_meta(&agent)[0]["started"], json!(true));
    assert_eq!(all_result_meta(&agent)[0]["aborted"], json!(true));
}

#[tokio::test(start_paused = true)]
async fn setup_awaits_its_owned_blocking_job_across_all_deadlines_without_running() {
    let probe = SlowSetupProbe::new();
    let (tools, run_count) = ScriptedActions::one(probe.script(SlowSetupFinish::Ready));
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let limits = AgentLimits::default()
        .with_turn_duration(Duration::from_millis(10))
        .unwrap();
    let mut agent = agent("action-slow-setup-ready", provider, tools, Some(limits));
    let cancellation = CancellationToken::new();

    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = probe.worker_started.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before the blocking setup job started: {result:?}"),
        }
        cancellation.cancel();
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert!(!probe.join_observed.load(Ordering::SeqCst));
        tokio::select! {
            biased;
            result = &mut turn => panic!("turn detached its still-owned setup job: {result:?}"),
            () = tokio::task::yield_now() => {}
        }
        probe.worker_release.release();
        turn.await.unwrap()
    };

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    probe.assert_crossed_every_boundary();
    assert_eq!(run_count.load(Ordering::SeqCst), 0);
    let results = tool_results(&agent);
    let [(error, meta)] = results.as_slice() else {
        panic!("the ready Action must be declined after setup observes cancellation")
    };
    assert_eq!(error.code, "ABORTED_BEFORE_DISPATCH");
    assert_eq!(meta["started"], json!(false));
}

#[tokio::test(start_paused = true)]
async fn setup_join_panic_is_unresolved_and_keeps_the_first_turn_timeout() {
    let probe = SlowSetupProbe::new();
    let (tools, run_count) = ScriptedActions::one(probe.script(SlowSetupFinish::JoinPanic));
    let provider = Arc::new(ScriptedProvider::new(vec![tool_response()]));
    let limits = AgentLimits::default()
        .with_turn_duration(Duration::from_millis(10))
        .unwrap();
    let mut agent = agent(
        "action-slow-setup-join-panic",
        provider,
        tools,
        Some(limits),
    );
    let cancellation = CancellationToken::new();

    let outcome = {
        let turn = agent.run_turn(TurnProposal::Enter(vec![user()]), cancellation.clone());
        tokio::pin!(turn);
        tokio::select! {
            biased;
            permit = probe.worker_started.acquire() => drop(permit.unwrap()),
            result = &mut turn => panic!("turn ended before the blocking setup job started: {result:?}"),
        }
        tokio::time::advance(Duration::from_millis(11)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert!(!probe.join_observed.load(Ordering::SeqCst));
        probe.worker_release.release();
        turn.await.unwrap()
    };

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("the first observed turn timeout must remain the outer reason")
    };
    assert_eq!(error.code(), "AGENT_TURN_TIMEOUT");
    assert!(probe.join_observed.load(Ordering::SeqCst));
    assert!(probe.crossed_preparation_deadline.load(Ordering::SeqCst));
    assert!(probe.crossed_turn_deadline.load(Ordering::SeqCst));
    assert!(!probe.cancellation_seen.load(Ordering::SeqCst));
    assert_eq!(run_count.load(Ordering::SeqCst), 0);
    assert!(
        !agent
            .session()
            .to_json()
            .unwrap()
            .contains("SECRET_SLOW_SETUP_WORKER_PANIC")
    );
    assert_no_result_and_poisoned(&mut agent).await;
}
