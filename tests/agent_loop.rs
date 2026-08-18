use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use deepseek_harness_cli::{
    agent::{
        AgentIdKind, AgentLimits, AgentLoop, AgentLoopConfig, AgentRuntime, AgentShutdownError,
        MAX_AGENT_ATTEMPTS_PER_TURN, MAX_AGENT_FIXED_REQUEST_BYTES,
        MAX_AGENT_OUTPUT_TOKENS_PER_REQUEST, MAX_AGENT_REPORTED_OUTPUT_TOKENS,
        MAX_AGENT_RETRIES_PER_STEP, MAX_AGENT_STEPS_PER_TURN, MAX_AGENT_TOOL_ARGUMENT_BYTES,
        MAX_AGENT_TOOL_CALLS_PER_STEP, MAX_AGENT_TOOL_CALLS_PER_TURN, MAX_AGENT_TOOL_DURATION,
        MAX_AGENT_TOOL_RESULT_BYTES, MAX_AGENT_TOOL_RESULTS_PER_TURN_BYTES,
        MAX_AGENT_TURN_DURATION, ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult,
        ToolExecutor, ToolExecutorError, ToolShutdownFuture, TurnProposal,
    },
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, LlmCallConfig,
        LlmCallConfigAdapterDefaults, LlmFailure, Message, MessageSource, ModelError,
        PositiveFiniteNumber, StreamChunk, TokenUsage, ToolSchema,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        ProviderStreamError, RetryBackoff, RetryPolicy,
    },
    session::{
        Clock, ClockError, EventKind, MAX_SESSION_EVENTS, RequestHeaderReason, Session,
        TurnEndReason, UnixMillis,
    },
};
use futures_util::stream;
use serde_json::json;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

fn fake_preflight(
    draft: ProviderRequestDraft<'_>,
    prepare: impl FnOnce(LlmCallConfig) -> Result<PreparedProviderCall, ProviderPrepareError>,
) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
    let prepared = prepare(draft.config().clone())?;
    draft.finish(prepared, 1)
}

#[tokio::test]
async fn tool_shutdown_failure_is_reported_and_an_active_session_can_be_recovered() {
    let calls = Arc::new(AtomicUsize::new(0));
    let tools = Arc::new(FailingShutdownTools {
        calls: Arc::clone(&calls),
    });
    let mut agent = AgentLoop::with_runtime(
        session("tool-shutdown-failure"),
        Arc::new(FakeProvider::new(Vec::new())),
        tools.clone(),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    assert!(matches!(
        agent.shutdown().await,
        Err(AgentShutdownError::Tools(_))
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let agent = AgentLoop::with_runtime(
        session("tool-release-failure"),
        Arc::new(FakeProvider::new(Vec::new())),
        tools,
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let error = match agent.shutdown_into_session().await {
        Ok(_) => panic!("failing tools must not release the Session as a success"),
        Err(error) => error,
    };
    let (_tools, mut recovered) = error.into_parts();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    recovered.shutdown().await.unwrap();
}

#[tokio::test]
async fn tool_shutdown_factory_and_future_panics_return_the_still_owned_session() {
    for kind in [ShutdownPanic::Factory, ShutdownPanic::Future] {
        let agent = AgentLoop::with_runtime(
            session("tool-shutdown-panic"),
            Arc::new(FakeProvider::new(Vec::new())),
            Arc::new(PanickingShutdownTools(kind)),
            Arc::new(FixedRuntime::default()),
            config(),
        )
        .unwrap();
        let error = match agent.shutdown_into_session().await {
            Ok(_) => panic!("panicking tool shutdown must not report success"),
            Err(error) => error,
        };
        let (_tools, mut recovered) = error.into_parts();
        recovered.shutdown().await.unwrap();
    }
}

#[derive(Default)]
struct FixedRuntime(Mutex<u64>);

impl AgentRuntime for FixedRuntime {
    fn next_id(
        &self,
        kind: AgentIdKind,
    ) -> Result<String, deepseek_harness_cli::agent::AgentRuntimeError> {
        let mut value = self.0.lock().unwrap();
        *value += 1;
        Ok(format!("{}-{value}", kind.prefix()))
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

#[derive(Clone)]
struct ProbeClock {
    calls: Arc<AtomicUsize>,
}

impl Clock for ProbeClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        let tick = self.calls.fetch_add(1, Ordering::SeqCst);
        UnixMillis::new(1_700_000_000_000 + i64::try_from(tick).unwrap())
            .map_err(|error| ClockError::new(error.to_string()))
    }
}

struct FakeProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
    requests: Mutex<Vec<Vec<Message>>>,
    request_facts: Mutex<Vec<RecordedRequestFacts>>,
    retry_policy: RetryPolicy,
    clock_calls: Option<Arc<AtomicUsize>>,
    dispatch_event_counts: Mutex<Vec<usize>>,
}

#[derive(Clone)]
struct RecordedRequestFacts {
    provider: String,
    model: String,
    max_tokens: Option<u64>,
    system: Option<String>,
    tools: Vec<ToolSchema>,
    context_window: Option<u64>,
    config: serde_json::Value,
    session_id: Option<String>,
    messages: Vec<Message>,
}

impl FakeProvider {
    fn new(attempts: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            attempts: Mutex::new(
                attempts
                    .into_iter()
                    .map(|attempt| attempt.into_iter().map(Ok).collect())
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
            request_facts: Mutex::new(Vec::new()),
            retry_policy: RetryPolicy::normal(
                2,
                vec!["SERVER".to_owned()],
                RetryBackoff::new(1.0, 1.0, 0.0).unwrap(),
            )
            .unwrap(),
            clock_calls: None,
            dispatch_event_counts: Mutex::new(Vec::new()),
        }
    }

    fn with_results(attempts: Vec<Vec<Result<StreamChunk, ProviderStreamError>>>) -> Self {
        Self {
            attempts: Mutex::new(attempts.into()),
            requests: Mutex::new(Vec::new()),
            request_facts: Mutex::new(Vec::new()),
            retry_policy: RetryPolicy::normal(
                2,
                vec!["SERVER".to_owned()],
                RetryBackoff::new(1.0, 1.0, 0.0).unwrap(),
            )
            .unwrap(),
            clock_calls: None,
            dispatch_event_counts: Mutex::new(Vec::new()),
        }
    }

    fn with_retry_policy(attempts: Vec<Vec<StreamChunk>>, retry_policy: RetryPolicy) -> Self {
        Self {
            attempts: Mutex::new(
                attempts
                    .into_iter()
                    .map(|attempt| attempt.into_iter().map(Ok).collect())
                    .collect(),
            ),
            requests: Mutex::new(Vec::new()),
            request_facts: Mutex::new(Vec::new()),
            retry_policy,
            clock_calls: None,
            dispatch_event_counts: Mutex::new(Vec::new()),
        }
    }

    fn for_oracle(
        attempts: Vec<Vec<StreamChunk>>,
        retry_policy: RetryPolicy,
        clock_calls: Arc<AtomicUsize>,
    ) -> Self {
        let mut provider = Self::with_retry_policy(attempts, retry_policy);
        provider.clock_calls = Some(clock_calls);
        provider
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }

    fn request_facts(&self) -> Vec<RecordedRequestFacts> {
        self.request_facts.lock().unwrap().clone()
    }

    fn dispatch_event_counts(&self) -> Vec<usize> {
        self.dispatch_event_counts.lock().unwrap().clone()
    }
}

impl ModelProvider for FakeProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config).with_retry_policy(self.retry_policy.clone()))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        if let Some(calls) = &self.clock_calls {
            self.dispatch_event_counts
                .lock()
                .unwrap()
                .push(calls.load(Ordering::SeqCst) - 1);
        }
        let messages = request.messages().to_vec();
        assert_eq!(messages, request.messages());
        self.request_facts
            .lock()
            .unwrap()
            .push(RecordedRequestFacts {
                provider: request.config().provider().to_owned(),
                model: request.config().model().to_owned(),
                max_tokens: request.config().max_tokens().map(|value| value.get()),
                system: request.system().map(str::to_owned),
                tools: request.tools().to_vec(),
                context_window: request
                    .preparation()
                    .context_window()
                    .map(|value| value.get()),
                config: serde_json::to_value(request.config()).unwrap(),
                session_id: request.session_id().map(ToString::to_string),
                messages: messages.clone(),
            });
        self.requests.lock().unwrap().push(messages);
        let chunks = self.attempts.lock().unwrap().pop_front().unwrap();
        Box::pin(stream::iter(chunks))
    }
}

#[derive(Default)]
struct FakeTools {
    calls: Mutex<Vec<String>>,
}

struct PendingTools;

struct LargeConcludingTools;

struct SmallConcludingTools;

struct ModelErrorTools;

#[derive(Default)]
struct LargeResultTools(Mutex<usize>);

struct InfrastructureTools;

struct OracleEchoTools {
    clock_calls: Arc<AtomicUsize>,
    body_event_counts: Arc<Mutex<Vec<usize>>>,
}

struct PanicFactoryTools;

struct PanicPollTools;

#[derive(Default)]
struct NeverCalledTools(Mutex<usize>);

struct CancellingTools {
    turn: CancellationToken,
    calls: Mutex<Vec<String>>,
}

struct CleanupOnCancelTools {
    turn: CancellationToken,
    cleaned: Arc<AtomicBool>,
}

struct DropProbe(Arc<AtomicBool>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

struct IgnoresCancelTools {
    turn: CancellationToken,
    dropped: Arc<AtomicBool>,
}

struct NotifyingReadyTools {
    calls: AtomicUsize,
    entered: Arc<Notify>,
}

struct FailingShutdownTools {
    calls: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum ShutdownPanic {
    Factory,
    Future,
}

struct PanickingShutdownTools(ShutdownPanic);

impl ToolExecutor for FailingShutdownTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("not used")) })
    }

    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err(ToolExecutorError::new("shutdown failed")) })
    }
}

impl ToolExecutor for PanickingShutdownTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("not used")) })
    }

    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        match self.0 {
            ShutdownPanic::Factory => panic!("injected shutdown factory panic"),
            ShutdownPanic::Future => Box::pin(async {
                panic!("injected shutdown future panic");
            }),
        }
    }
}

impl ToolExecutor for PendingTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(std::future::pending())
    }
}

impl ToolExecutor for LargeConcludingTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async {
            ToolExecutionResult::new(
                vec![ContentBlock::text("x".repeat(2_048)).unwrap()],
                false,
                None,
                None,
                true,
            )
            .map_err(|error| ToolExecutorError::new(error.to_string()))
        })
    }
}

impl ToolExecutor for SmallConcludingTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async {
            ToolExecutionResult::new(
                vec![ContentBlock::text("finished by tool").unwrap()],
                false,
                None,
                None,
                true,
            )
            .map_err(|error| ToolExecutorError::new(error.to_string()))
        })
    }
}

impl ToolExecutor for ModelErrorTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async {
            ToolExecutionResult::model_error(
                vec![ContentBlock::text("permission denied").unwrap()],
                deepseek_harness_cli::session::ToolFailure {
                    name: "PolicyError".to_owned(),
                    code: "DENIED".to_owned(),
                },
            )
            .map_err(|error| ToolExecutorError::new(error.to_string()))
        })
    }
}

impl ToolExecutor for LargeResultTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let calls = &self.0;
        Box::pin(async move {
            *calls.lock().unwrap() += 1;
            ToolExecutionResult::success(vec![ContentBlock::text("x".repeat(6_000)).unwrap()])
                .map_err(|error| ToolExecutorError::new(error.to_string()))
        })
    }
}

impl ToolExecutor for InfrastructureTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { Err(ToolExecutorError::new("SECRET_EXECUTOR_DETAIL")) })
    }
}

impl ToolExecutor for PanicFactoryTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        panic!("SECRET_TOOL_FACTORY_PANIC")
    }
}

impl ToolExecutor for PanicPollTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async { panic!("SECRET_TOOL_POLL_PANIC") })
    }
}

impl ToolExecutor for OracleEchoTools {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        assert_eq!(request.name(), "echo");
        let clock_calls = self.clock_calls.clone();
        let body_event_counts = &self.body_event_counts;
        Box::pin(async move {
            body_event_counts
                .lock()
                .unwrap()
                .push(clock_calls.load(Ordering::SeqCst) - 1);
            ToolExecutionResult::success(vec![ContentBlock::text("echo: hello").unwrap()])
                .map_err(|error| ToolExecutorError::new(error.to_string()))
        })
    }
}

impl ToolExecutor for NeverCalledTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let calls = &self.0;
        Box::pin(async move {
            *calls.lock().unwrap() += 1;
            Err(ToolExecutorError::new("must not execute"))
        })
    }
}

impl ToolExecutor for CancellingTools {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let calls = &self.calls;
        let call_id = request.call_id().to_string();
        let turn = self.turn.clone();
        Box::pin(async move {
            calls.lock().unwrap().push(call_id);
            turn.cancel();
            std::future::pending().await
        })
    }
}

impl ToolExecutor for CleanupOnCancelTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        child: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let turn = self.turn.clone();
        let cleaned = self.cleaned.clone();
        Box::pin(async move {
            turn.cancel();
            child.cancelled().await;
            cleaned.store(true, Ordering::SeqCst);
            Err(ToolExecutorError::new("cleanup finished"))
        })
    }
}

impl ToolExecutor for IgnoresCancelTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _child: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let turn = self.turn.clone();
        let probe = DropProbe(self.dropped.clone());
        Box::pin(async move {
            let _probe = probe;
            turn.cancel();
            std::future::pending().await
        })
    }
}

impl ToolExecutor for FakeTools {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let calls = &self.calls;
        let name = request.name().to_owned();
        Box::pin(async move {
            calls.lock().unwrap().push(name);
            let block = ContentBlock::text("tool says 4")
                .map_err(|error| ToolExecutorError::new(error.to_string()))?;
            ToolExecutionResult::success(vec![block])
                .map_err(|error| ToolExecutorError::new(error.to_string()))
        })
    }
}

impl ToolExecutor for NotifyingReadyTools {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancel: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        Box::pin(async {
            ToolExecutionResult::success(vec![ContentBlock::text("ready").unwrap()])
                .map_err(|error| ToolExecutorError::new(error.to_string()))
        })
    }
}

fn user(text: &str) -> Message {
    user_with_id("user-1", text)
}

fn user_with_id(id: &str, text: &str) -> Message {
    Message::user(
        id,
        vec![ContentBlock::text(text).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn session(id: &str) -> Session {
    Session::with_clock(id, IncrementingClock(Mutex::new(1_000))).unwrap()
}

fn text_response(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::text_delta(0, text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn tool_response(arguments: &str, finish: FinishReason) -> Vec<StreamChunk> {
    let block = ContentBlock::tool_call("call-1", "calculator", arguments).unwrap();
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::tool_call_delta(0, "call-1", Some("calculator".to_owned()), arguments)
            .unwrap(),
        StreamChunk::block_end(0, block).unwrap(),
        StreamChunk::finish(finish, None).unwrap(),
    ]
}

fn tool_response_with_id(call_id: &str) -> Vec<StreamChunk> {
    let block = ContentBlock::tool_call(call_id, "calculator", "{}").unwrap();
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(0, block).unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]
}

fn two_tool_response() -> Vec<StreamChunk> {
    let first = ContentBlock::tool_call("call-1", "calculator", "{}").unwrap();
    let second = ContentBlock::tool_call("call-2", "calculator", "{}").unwrap();
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(0, first).unwrap(),
        StreamChunk::block_start(1, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(1, second).unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]
}

fn many_tool_response(count: usize) -> Vec<StreamChunk> {
    let mut chunks = Vec::with_capacity(count * 2 + 1);
    for index in 0..count {
        let call_id = format!("call-{index}");
        let block = ContentBlock::tool_call(call_id.as_str(), "calculator", "{}").unwrap();
        chunks.push(StreamChunk::block_start(index as u64, ContentBlockType::ToolCall).unwrap());
        chunks.push(StreamChunk::block_end(index as u64, block).unwrap());
    }
    chunks.push(StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap());
    chunks
}

fn maximum_ready_text_response() -> Vec<StreamChunk> {
    let delta_count = deepseek_harness_cli::provider::MAX_PROVIDER_STREAM_CHUNKS - 3;
    let text = "x".repeat(delta_count);
    let mut chunks = Vec::with_capacity(deepseek_harness_cli::provider::MAX_PROVIDER_STREAM_CHUNKS);
    chunks.push(StreamChunk::block_start(0, ContentBlockType::Text).unwrap());
    chunks.extend((0..delta_count).map(|_| StreamChunk::text_delta(0, "x").unwrap()));
    chunks.push(StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap());
    chunks.push(StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap());
    chunks
}

fn named_tool_response(name: &str) -> Vec<StreamChunk> {
    let block = ContentBlock::tool_call("call-unknown", name, "{}").unwrap();
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(0, block).unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]
}

struct NoMaxTokensProvider {
    streams: Mutex<usize>,
}

impl ModelProvider for NoMaxTokensProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(PreparedProviderCall::new(
            config,
            LlmCallConfigAdapterDefaults::default(),
            None,
        ))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        *self.streams.lock().unwrap() += 1;
        Box::pin(stream::pending())
    }
}

fn prepared(config: LlmCallConfig) -> PreparedProviderCall {
    let mut raw = config.raw().as_value().clone();
    raw.as_object_mut()
        .unwrap()
        .insert("maxTokens".to_owned(), json!(1_024));
    let config = serde_json::from_value(raw).unwrap();
    PreparedProviderCall::new(
        config,
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
    )
}

struct PendingProvider;

struct PanicPrepareProvider;

#[derive(Default)]
struct InvalidPreflightProvider {
    streams: AtomicUsize,
}

#[derive(Default)]
struct WireTooLargeProvider {
    preparations: AtomicUsize,
    preflights: AtomicUsize,
    streams: AtomicUsize,
    cancel_during_preflight: Option<CancellationToken>,
}

struct PanicStreamProvider {
    cancellation: Mutex<Option<CancellationToken>>,
}

struct PanicStreamPollProvider {
    cancellation: Mutex<Option<CancellationToken>>,
}

struct TokenObservingProvider {
    chunks: Mutex<Option<Vec<StreamChunk>>>,
    cancellation: Mutex<Option<CancellationToken>>,
}

struct PolicyProvider {
    attempts: Mutex<VecDeque<Vec<StreamChunk>>>,
    policy: RetryPolicy,
    requests: Mutex<usize>,
}

struct SequencedPreparedProvider {
    preparations: Mutex<VecDeque<(String, u64, u64)>>,
    attempts: Mutex<VecDeque<Vec<StreamChunk>>>,
}

impl PolicyProvider {
    fn new(attempts: Vec<Vec<StreamChunk>>, policy: RetryPolicy) -> Self {
        Self {
            attempts: Mutex::new(attempts.into()),
            policy,
            requests: Mutex::new(0),
        }
    }
}

impl SequencedPreparedProvider {
    fn new(preparations: Vec<(&str, u64, u64)>, attempts: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            preparations: Mutex::new(
                preparations
                    .into_iter()
                    .map(|(model, context, max_tokens)| (model.to_owned(), context, max_tokens))
                    .collect(),
            ),
            attempts: Mutex::new(attempts.into()),
        }
    }
}

impl ModelProvider for SequencedPreparedProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        let (model, context_window, max_tokens) =
            self.preparations.lock().unwrap().pop_front().unwrap();
        let mut raw = config.raw().as_value().clone();
        raw["model"] = model.into();
        raw["maxTokens"] = max_tokens.into();
        let effective = serde_json::from_value(raw).unwrap();
        Ok(PreparedProviderCall::new(
            effective,
            LlmCallConfigAdapterDefaults::default(),
            Some(deepseek_harness_cli::model::NonNegativeSafeInteger::new(context_window).unwrap()),
        ))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
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

impl ModelProvider for PolicyProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config).with_retry_policy(self.policy.clone()))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        *self.requests.lock().unwrap() += 1;
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

impl ModelProvider for PendingProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        Box::pin(stream::pending())
    }
}

impl ModelProvider for PanicPrepareProvider {
    fn prepare_call(
        &self,
        _config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        panic!("SECRET_PROVIDER_PREPARE_PANIC")
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        unreachable!("a failed preparation must not open a stream")
    }
}

impl ModelProvider for InvalidPreflightProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        let prepared = self.prepare_call(draft.config().clone())?;
        Err(ProviderPreflightError::InvalidRequest {
            failure: LlmFailure::new(
                "the provider rejected the encoded request",
                "INVALID_REQUEST",
            )
            .unwrap(),
            prepared,
        })
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        self.streams.fetch_add(1, Ordering::SeqCst);
        Box::pin(stream::pending())
    }
}

impl ModelProvider for WireTooLargeProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        self.preflights.fetch_add(1, Ordering::SeqCst);
        let prepared = self.prepare_call(draft.config().clone())?;
        if let Some(cancellation) = &self.cancel_during_preflight {
            cancellation.cancel();
        }
        Err(ProviderPreflightError::WireTooLarge {
            maximum: 1,
            prepared,
        })
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        self.streams.fetch_add(1, Ordering::SeqCst);
        Box::pin(stream::pending())
    }
}

impl ModelProvider for PanicStreamProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, cancel: CancellationToken) -> ProviderStream {
        *self.cancellation.lock().unwrap() = Some(cancel);
        panic!("SECRET_PROVIDER_STREAM_PANIC")
    }
}

impl ModelProvider for PanicStreamPollProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, cancel: CancellationToken) -> ProviderStream {
        *self.cancellation.lock().unwrap() = Some(cancel);
        Box::pin(stream::poll_fn(|_| panic!("SECRET_PROVIDER_POLL_PANIC")))
    }
}

impl ModelProvider for TokenObservingProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, cancel: CancellationToken) -> ProviderStream {
        *self.cancellation.lock().unwrap() = Some(cancel);
        let chunks = self.chunks.lock().unwrap().take().unwrap();
        Box::pin(stream::iter(chunks.into_iter().map(Ok)))
    }
}

struct CancelAtEofProvider {
    cancellation: CancellationToken,
}

struct Phase7CancelThenContinueProvider {
    first_chunks: Mutex<Option<VecDeque<StreamChunk>>>,
    partial_committed: Arc<Notify>,
    requests: Mutex<Vec<Vec<Message>>>,
    calls: AtomicUsize,
}

impl Phase7CancelThenContinueProvider {
    fn new(first_chunks: Vec<StreamChunk>, partial_committed: Arc<Notify>) -> Self {
        Self {
            first_chunks: Mutex::new(Some(first_chunks.into())),
            partial_committed,
            requests: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        }
    }

    fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

impl ModelProvider for CancelAtEofProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        let chunks = VecDeque::from(text_response("must not commit"));
        let cancellation = self.cancellation.clone();
        Box::pin(stream::unfold(
            (chunks, cancellation),
            |(mut chunks, cancellation)| async move {
                match chunks.pop_front() {
                    Some(chunk) => Some((Ok(chunk), (chunks, cancellation))),
                    None => {
                        cancellation.cancel();
                        None
                    }
                }
            },
        ))
    }
}

impl ModelProvider for Phase7CancelThenContinueProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        Ok(prepared(config))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        fake_preflight(draft, |config| self.prepare_call(config))
    }

    fn stream(&self, request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        self.requests
            .lock()
            .unwrap()
            .push(request.messages().to_vec());
        match self.calls.fetch_add(1, Ordering::SeqCst) {
            0 => {
                let mut chunks = self.first_chunks.lock().unwrap().take().unwrap();
                let partial_committed = self.partial_committed.clone();
                let mut notified = false;
                Box::pin(stream::poll_fn(move |_| {
                    if let Some(chunk) = chunks.pop_front() {
                        return std::task::Poll::Ready(Some(Ok(chunk)));
                    }
                    if !notified {
                        notified = true;
                        partial_committed.notify_one();
                    }
                    std::task::Poll::Pending
                }))
            }
            1 => Box::pin(stream::iter(
                text_response("continued after cancellation")
                    .into_iter()
                    .map(Ok),
            )),
            call => panic!("unexpected Phase 7 provider call {call}"),
        }
    }
}

struct EmptyMessageIdRuntime;

impl AgentRuntime for EmptyMessageIdRuntime {
    fn next_id(
        &self,
        kind: AgentIdKind,
    ) -> Result<String, deepseek_harness_cli::agent::AgentRuntimeError> {
        Ok(match kind {
            AgentIdKind::Message => String::new(),
            AgentIdKind::Retry => "retry-ok".to_owned(),
            AgentIdKind::Approval => "approval-ok".to_owned(),
        })
    }

    fn sample_unit(&self) -> Result<f64, deepseek_harness_cli::agent::AgentRuntimeError> {
        Ok(0.5)
    }
}

fn config() -> AgentLoopConfig {
    let schema = ToolSchema::new(
        "calculator",
        "adds numbers",
        deepseek_harness_cli::model::JsonValue::new(json!({"type":"object"})).unwrap(),
    )
    .unwrap();
    AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(vec![schema])
        .unwrap()
}

fn oracle_config(system: &str, with_tool: bool) -> AgentLoopConfig {
    let mut config = AgentLoopConfig::new(LlmCallConfig::new("mock", "oracle-model").unwrap())
        .with_system(system)
        .unwrap();
    if with_tool {
        let schema = ToolSchema::new(
            "echo",
            "echo text",
            deepseek_harness_cli::model::JsonValue::new(json!({
                "type": "object",
                "properties": {"text": {"type": "string"}}
            }))
            .unwrap(),
        )
        .unwrap();
        config = config.with_tools(vec![schema]).unwrap();
    }
    config
}

fn agent(
    id: &str,
    provider: Arc<FakeProvider>,
    tools: Arc<FakeTools>,
    config: AgentLoopConfig,
) -> AgentLoop {
    AgentLoop::with_runtime(
        session(id),
        provider,
        tools,
        Arc::new(FixedRuntime::default()),
        config,
    )
    .unwrap()
}

#[tokio::test]
async fn text_completion_is_logged_before_a_balanced_turn_closes() {
    let provider = Arc::new(FakeProvider::new(vec![text_response("hello")]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("text", provider.clone(), tools, config());

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("hi")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(outcome.steps(), 1);
    assert_eq!(outcome.attempts(), 1);
    assert_eq!(
        outcome.turn_end_seq(),
        agent.session().events().last().unwrap().seq()
    );
    let final_message = outcome
        .final_message()
        .expect("the committed text answer is carried by the outcome");
    assert!(matches!(
        final_message.content(),
        [block] if matches!(block.kind(), ContentBlockKind::Text { text } if text == "hello")
    ));
    assert_eq!(agent.session().state().open_turn(), None);
    assert_eq!(provider.requests()[0], vec![user("hi")]);
    assert_eq!(agent.session().messages().len(), 2);
    assert!(matches!(
        agent.session().events().last().unwrap().kind(),
        EventKind::TurnEnd {
            reason: TurnEndReason::Completed,
            ..
        }
    ));
}

#[tokio::test]
async fn request_headers_are_suppressed_when_stable_and_marked_on_resume_or_change() {
    const ORACLE_SYSTEM: &str = concat!(
        "You are an AI agent powered by DeepSeek Harness.\n\n",
        "Phase 3 oracle persona."
    );
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/agent/upstream_phase3_oracle.json")).unwrap();
    let lifecycle = &oracle["scenarios"]["requestHeaderLifecycle"];
    assert_eq!(lifecycle["checks"]["stableSuppressed"], true);
    assert_eq!(lifecycle["checks"]["changedSnapshot"], true);
    assert_eq!(lifecycle["checks"]["resumedSnapshot"], true);

    let provider = Arc::new(FakeProvider::new(vec![
        text_response("first"),
        text_response("second"),
        text_response("resumed"),
    ]));
    let tools = Arc::new(FakeTools::default());
    let mut original = agent(
        "header-lifecycle",
        provider.clone(),
        tools.clone(),
        oracle_config(ORACLE_SYSTEM, false),
    );
    original
        .run_turn(
            TurnProposal::Enter(vec![user_with_id("user-one", "one")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(request_header_reasons(original.session())).unwrap(),
        lifecycle["stableAndChange"]["afterInitial"]
    );
    original
        .run_turn(
            TurnProposal::Enter(vec![user_with_id("user-two", "two")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(request_header_reasons(original.session())).unwrap(),
        lifecycle["stableAndChange"]["afterStable"]
    );

    let mut reconstructed = AgentLoop::with_runtime(
        original.shutdown_into_session().await.unwrap(),
        provider,
        tools,
        Arc::new(FixedRuntime::default()),
        oracle_config(ORACLE_SYSTEM, false),
    )
    .unwrap();
    reconstructed
        .run_turn(
            TurnProposal::Enter(vec![user_with_id("user-three", "three")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(request_header_reasons(reconstructed.session())).unwrap(),
        lifecycle["resume"]["afterResume"]
    );
    assert_eq!(
        request_header_payloads(reconstructed.session()),
        lifecycle["resume"]["payloads"]
    );

    let provider = Arc::new(SequencedPreparedProvider::new(
        vec![
            ("oracle-model", 4_096, 1_024),
            ("oracle-model", 4_096, 1_024),
            ("oracle-model", 4_096, 2_048),
        ],
        vec![
            text_response("first"),
            text_response("stable"),
            text_response("changed"),
        ],
    ));
    let mut changed = AgentLoop::with_runtime(
        session("header-change"),
        provider,
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        oracle_config(ORACLE_SYSTEM, false),
    )
    .unwrap();
    changed
        .run_turn(
            TurnProposal::Enter(vec![user_with_id("user-a", "a")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(request_header_reasons(changed.session())).unwrap(),
        lifecycle["stableAndChange"]["afterInitial"]
    );
    changed
        .run_turn(
            TurnProposal::Enter(vec![user_with_id("user-b", "b")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(request_header_reasons(changed.session())).unwrap(),
        lifecycle["stableAndChange"]["afterStable"]
    );
    changed
        .run_turn(
            TurnProposal::Enter(vec![user_with_id("user-c", "c")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(request_header_reasons(changed.session())).unwrap(),
        lifecycle["stableAndChange"]["afterChange"]
    );
    assert_eq!(
        request_header_payloads(changed.session()),
        lifecycle["stableAndChange"]["payloads"]
    );
    assert_eq!(
        changed
            .session()
            .request_header()
            .unwrap()
            .config
            .max_tokens()
            .unwrap()
            .get(),
        2_048
    );
}

#[tokio::test]
async fn one_tool_result_becomes_the_next_steps_model_context() {
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response("{\"a\":2}", FinishReason::tool_calls().unwrap()),
        text_response("four"),
    ]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("tool", provider.clone(), tools.clone(), config());

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("calculate")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.steps(), 2);
    assert!(outcome.final_message().is_some_and(|message| {
        message
            .content()
            .iter()
            .any(|block| matches!(block.kind(), ContentBlockKind::Text { text } if text == "four"))
    }));
    assert_eq!(tools.calls.lock().unwrap().as_slice(), ["calculator"]);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].len(), 1);
    assert_eq!(requests[1].len(), 3);
    assert_eq!(agent.session().messages().len(), 4);
}

#[tokio::test(start_paused = true)]
async fn provider_failure_retries_in_the_same_step_with_durable_events() {
    let failure = LlmFailure::new("temporary", "SERVER").unwrap();
    let provider = Arc::new(FakeProvider::new(vec![
        vec![StreamChunk::finish(FinishReason::error(failure).unwrap(), None).unwrap()],
        text_response("recovered"),
    ]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("retry", provider.clone(), tools, config());

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("try")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.steps(), 1);
    assert_eq!(outcome.attempts(), 2);
    let types = agent
        .session()
        .events()
        .iter()
        .map(|event| event.kind().event_type())
        .collect::<Vec<_>>();
    let scheduled = types.iter().position(|kind| *kind == "llm/retry").unwrap();
    let started = types
        .iter()
        .position(|kind| *kind == "llm/retry-started")
        .unwrap();
    assert!(scheduled < started);
    assert_eq!(provider.requests().len(), 2);
}

#[tokio::test]
async fn rejected_proposal_has_no_step_or_provider_request() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("reject", provider.clone(), tools, config());
    let outcome = agent
        .run_turn(TurnProposal::Reject, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Blocked);
    assert_eq!(outcome.steps(), 0);
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn max_tokens_drops_tool_calls_and_never_executes_them() {
    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::max_tokens().unwrap(),
    )]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("max-tokens", provider, tools.clone(), config());
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("go")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::MaxTokens);
    assert!(tools.calls.lock().unwrap().is_empty());
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
    );
}

#[tokio::test]
async fn bad_tool_arguments_are_model_facing_errors_without_side_effects() {
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response("{broken", FinishReason::tool_calls().unwrap()),
        text_response("corrected"),
    ]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("bad-args", provider, tools.clone(), config());
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("go")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert!(tools.calls.lock().unwrap().is_empty());
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. } if error.code == "INVALID_TOOL_ARGUMENTS"
    )));
}

#[tokio::test]
async fn oversized_tool_arguments_and_tool_call_limits_have_no_side_effects() {
    let oversized = serde_json::to_string(&json!({"value": "x".repeat(64)})).unwrap();
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response(&oversized, FinishReason::tool_calls().unwrap()),
        text_response("recovered"),
    ]));
    let tools = Arc::new(FakeTools::default());
    let limits = AgentLimits::default()
        .with_max_tool_argument_bytes(64)
        .unwrap();
    let mut argument_agent = agent(
        "argument-limit",
        provider,
        tools.clone(),
        config().with_limits(limits),
    );
    let outcome = argument_agent
        .run_turn(
            TurnProposal::Enter(vec![user("large args")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert!(tools.calls.lock().unwrap().is_empty());
    assert!(
        argument_agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(
                event.kind(),
                EventKind::ToolResult { error: Some(error), .. }
                    if error.code == "TOOL_ARGUMENTS_TOO_LARGE"
            ))
    );

    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::tool_calls().unwrap(),
    )]));
    let tools = Arc::new(FakeTools::default());
    let limits = AgentLimits::default()
        .with_max_tool_calls_per_step(0)
        .unwrap();
    let mut agent = agent(
        "tool-call-limit",
        provider,
        tools.clone(),
        config().with_limits(limits),
    );
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("no tools")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected tool-call limit")
    };
    assert_eq!(error.code(), "AGENT_MAX_TOOL_CALLS");
    assert_eq!(outcome.tool_calls(), 0);
    assert!(tools.calls.lock().unwrap().is_empty());
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn per_step_and_per_turn_tool_call_limits_stop_before_extra_side_effects() {
    let provider = Arc::new(FakeProvider::new(vec![two_tool_response()]));
    let tools = Arc::new(FakeTools::default());
    let limits = AgentLimits::default()
        .with_max_tool_calls_per_step(1)
        .unwrap();
    let mut step_agent = agent(
        "step-tool-count",
        provider,
        tools.clone(),
        config().with_limits(limits),
    );
    let outcome = step_agent
        .run_turn(
            TurnProposal::Enter(vec![user("two at once")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_MAX_TOOL_CALLS"
    ));
    assert_eq!(outcome.tool_calls(), 0);
    assert!(tools.calls.lock().unwrap().is_empty());
    assert!(
        !step_agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );

    let provider = Arc::new(FakeProvider::new(vec![
        tool_response_with_id("call-first"),
        tool_response_with_id("call-second"),
    ]));
    let tools = Arc::new(FakeTools::default());
    let limits = AgentLimits::default()
        .with_max_tool_calls_per_turn(1)
        .unwrap();
    let mut turn_agent = agent(
        "turn-tool-count",
        provider,
        tools.clone(),
        config().with_limits(limits),
    );
    let outcome = turn_agent
        .run_turn(
            TurnProposal::Enter(vec![user("one per step")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_MAX_TOOL_CALLS"
    ));
    assert_eq!(outcome.tool_calls(), 1);
    assert_eq!(tools.calls.lock().unwrap().len(), 1);
    assert_eq!(
        turn_agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
            .count(),
        1
    );
    assert_eq!(
        turn_agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
            .count(),
        1
    );
    assert_eq!(turn_agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn configured_step_limit_closes_with_a_truthful_error() {
    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::tool_calls().unwrap(),
    )]));
    let tools = Arc::new(FakeTools::default());
    let limits = AgentLimits::default().with_max_steps_per_turn(1).unwrap();
    let mut agent = agent("step-limit", provider, tools, config().with_limits(limits));
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("loop")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected a structured limit error")
    };
    assert_eq!(error.code(), "AGENT_MAX_STEPS");
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn duplicate_call_ids_close_with_error_before_any_side_effect() {
    let first = ContentBlock::tool_call("same", "calculator", "{}").unwrap();
    let second = ContentBlock::tool_call("same", "calculator", "{}").unwrap();
    let provider = Arc::new(FakeProvider::new(vec![vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(0, first).unwrap(),
        StreamChunk::block_start(1, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(1, second).unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("duplicate", provider, tools.clone(), config());
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("duplicate")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected duplicate-call error")
    };
    assert_eq!(error.code(), "AGENT_INVALID_TOOL_CALL");
    assert!(tools.calls.lock().unwrap().is_empty());
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
}

#[tokio::test]
async fn cancellation_before_a_turn_starts_commits_only_balanced_boundaries() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("cancel-before", provider.clone(), tools, config());
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user("stop")]), cancellation)
        .await
        .unwrap();
    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(outcome.steps(), 0);
    assert!(provider.requests().is_empty());
    assert_eq!(agent.session().events().len(), 2);
}

#[tokio::test]
async fn invalid_non_user_turn_input_is_rejected_before_any_event_is_written() {
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut agent = agent(
        "invalid-input",
        provider,
        Arc::new(FakeTools::default()),
        config(),
    );
    let assistant = Message::assistant(
        "assistant-input",
        vec![ContentBlock::text("not a user").unwrap()],
        "mock",
        "model",
    )
    .unwrap();

    assert!(matches!(
        agent
            .run_turn(
                TurnProposal::Enter(vec![assistant]),
                CancellationToken::new(),
            )
            .await,
        Err(deepseek_harness_cli::agent::AgentLoopError::InvalidTurnMessages)
    ));
    assert!(agent.session().events().is_empty());
    assert_eq!(agent.session().state().open_turn(), None);

    let outcome = agent
        .run_turn(TurnProposal::Reject, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Blocked);
}

#[tokio::test]
async fn cancelled_zero_step_proposals_record_aborted_not_success_or_blocked() {
    for (id, proposal) in [
        ("cancel-reject", TurnProposal::Reject),
        ("cancel-empty", TurnProposal::Enter(Vec::new())),
    ] {
        let provider = Arc::new(FakeProvider::new(vec![]));
        let mut agent = agent(
            id,
            provider.clone(),
            Arc::new(FakeTools::default()),
            config(),
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let outcome = agent.run_turn(proposal, cancellation).await.unwrap();
        assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
        assert_eq!((outcome.steps(), outcome.attempts()), (0, 0));
        assert_eq!(agent.session().events().len(), 2);
        assert!(provider.requests().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn a_hung_tool_times_out_and_the_model_can_continue() {
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response("{}", FinishReason::tool_calls().unwrap()),
        text_response("after timeout"),
    ]));
    let limits = AgentLimits::default()
        .with_tool_duration(std::time::Duration::from_millis(10))
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("tool-timeout"),
        provider,
        Arc::new(PendingTools),
        Arc::new(FixedRuntime::default()),
        config().with_limits(limits),
    )
    .unwrap();
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("timeout")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. } if error.code == "TOOL_TIMEOUT"
    )));
}

#[tokio::test(start_paused = true)]
async fn retry_wait_cancellation_logs_schedule_but_not_started() {
    let failure = LlmFailure::new("temporary", "SERVER").unwrap();
    let provider = Arc::new(FakeProvider::new(vec![vec![
        StreamChunk::finish(FinishReason::error(failure).unwrap(), None).unwrap(),
    ]]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("retry-cancel", provider, tools, config());
    let cancellation = CancellationToken::new();
    let watcher = cancellation.clone();
    let handle = tokio::spawn(async move {
        tokio::task::yield_now().await;
        watcher.cancel();
    });
    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user("retry")]), cancellation)
        .await
        .unwrap();
    handle.await.unwrap();
    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    let types = agent
        .session()
        .events()
        .iter()
        .map(|event| event.kind().event_type())
        .collect::<Vec<_>>();
    assert!(types.contains(&"llm/retry"));
    assert!(!types.contains(&"llm/retry-started"));
}

#[tokio::test(start_paused = true)]
async fn retry_boundary_yields_before_the_second_attempt_and_resamples_cancellation() {
    let failure = LlmFailure::new("temporary", "SERVER").unwrap();
    let provider = Arc::new(FakeProvider::new(vec![
        vec![StreamChunk::finish(FinishReason::error(failure).unwrap(), None).unwrap()],
        text_response("must not be requested"),
    ]));
    let cancellation = CancellationToken::new();
    let cancel_from_sibling = cancellation.clone();
    let mut agent = AgentLoop::with_runtime(
        session("retry-boundary-yield"),
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let (outcome, ()) = tokio::join! {
        biased;
        agent.run_turn(
            TurnProposal::Enter(vec![user("retry fairly")]),
            cancellation,
        ),
        async move {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            cancel_from_sibling.cancel();
        },
    };
    let outcome = outcome.unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(provider.requests().len(), 1);
    let types = agent
        .session()
        .events()
        .iter()
        .map(|event| event.kind().event_type())
        .collect::<Vec<_>>();
    assert!(types.contains(&"llm/retry"));
    assert!(!types.contains(&"llm/retry-started"));
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test(start_paused = true)]
async fn retry_after_uses_provider_delay_or_policy_fallback_exactly() {
    let failure_with_delay = |delay_ms: f64| {
        LlmFailure::from_parts(
            "temporary".to_owned(),
            "SERVER".to_owned(),
            None,
            Some(PositiveFiniteNumber::new(delay_ms).unwrap()),
            None,
        )
        .unwrap()
    };
    let finish = |failure: LlmFailure| {
        vec![StreamChunk::finish(FinishReason::error(failure).unwrap(), None).unwrap()]
    };

    let normal = RetryPolicy::normal(
        2,
        vec!["SERVER".to_owned()],
        RetryBackoff::new(1.0, 10.0, 0.0).unwrap(),
    )
    .unwrap();
    let provider = Arc::new(PolicyProvider::new(
        vec![finish(failure_with_delay(2.5)), text_response("ok")],
        normal,
    ));
    let mut agent = AgentLoop::with_runtime(
        session("provider-retry-after"),
        provider,
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    agent
        .run_turn(
            TurnProposal::Enter(vec![user("retry")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let delay = agent
        .session()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::LlmRetry { retry } => Some(retry.delay_ms().get()),
            _ => None,
        })
        .unwrap();
    assert_eq!(delay, 2.5);

    let normal = RetryPolicy::normal(
        2,
        vec!["SERVER".to_owned()],
        RetryBackoff::new(1.0, 1.0, 0.0).unwrap(),
    )
    .unwrap();
    let provider = Arc::new(PolicyProvider::new(
        vec![finish(failure_with_delay(2.0))],
        normal,
    ));
    let mut agent = AgentLoop::with_runtime(
        session("normal-retry-after-too-long"),
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("stop")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "SERVER"
    ));
    assert_eq!(*provider.requests.lock().unwrap(), 1);
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::LlmRetry { .. }))
    );

    let always = RetryPolicy::always(RetryBackoff::new(1.0, 1.0, 0.0).unwrap());
    let provider = Arc::new(PolicyProvider::new(
        vec![finish(failure_with_delay(2.0)), text_response("ok")],
        always,
    ));
    let mut agent = AgentLoop::with_runtime(
        session("always-retry-after-fallback"),
        provider,
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    agent
        .run_turn(
            TurnProposal::Enter(vec![user("fallback")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let delay = agent
        .session()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::LlmRetry { retry } => Some(retry.delay_ms().get()),
            _ => None,
        })
        .unwrap();
    assert_eq!(delay, 1.0);
}

#[tokio::test]
async fn non_retryable_provider_failure_is_a_closed_turn_error() {
    let failure = LlmFailure::new("bad request", "INVALID_REQUEST").unwrap();
    let provider = Arc::new(FakeProvider::new(vec![vec![
        StreamChunk::finish(FinishReason::error(failure).unwrap(), None).unwrap(),
    ]]));
    let tools = Arc::new(FakeTools::default());
    let mut agent = agent("provider-error", provider, tools, config());
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("fail")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected provider error")
    };
    assert_eq!(error.code(), "INVALID_REQUEST");
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test(start_paused = true)]
async fn retry_and_attempt_safety_limits_close_with_stable_errors() {
    let failure = || {
        vec![
            StreamChunk::finish(
                FinishReason::error(LlmFailure::new("temporary", "SERVER").unwrap()).unwrap(),
                None,
            )
            .unwrap(),
        ]
    };
    let always = RetryPolicy::always(RetryBackoff::new(1.0, 1.0, 0.0).unwrap());

    let retry_provider = Arc::new(PolicyProvider::new(
        vec![failure(), failure()],
        always.clone(),
    ));
    let retry_limits = AgentLimits::default().with_max_retries_per_step(1).unwrap();
    let mut retry_agent = AgentLoop::with_runtime(
        session("retry-limit"),
        retry_provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config().with_limits(retry_limits),
    )
    .unwrap();
    let outcome = retry_agent
        .run_turn(
            TurnProposal::Enter(vec![user("retry")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected retry limit")
    };
    assert_eq!(error.code(), "AGENT_MAX_RETRIES");
    assert_eq!((outcome.attempts(), outcome.retries()), (2, 1));
    assert_eq!(*retry_provider.requests.lock().unwrap(), 2);
    assert_eq!(
        retry_agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::LlmRetry { .. }))
            .count(),
        1
    );
    assert_eq!(
        retry_agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::LlmRetryStarted { .. }))
            .count(),
        1
    );
    assert_eq!(retry_agent.session().state().open_turn(), None);

    let attempt_provider = Arc::new(PolicyProvider::new(vec![failure()], always));
    let attempt_limits = AgentLimits::default()
        .with_max_attempts_per_turn(1)
        .unwrap();
    let mut attempt_agent = AgentLoop::with_runtime(
        session("attempt-limit"),
        attempt_provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config().with_limits(attempt_limits),
    )
    .unwrap();
    let outcome = attempt_agent
        .run_turn(
            TurnProposal::Enter(vec![user("attempt")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected attempt limit")
    };
    assert_eq!(error.code(), "AGENT_MAX_MODEL_ATTEMPTS");
    assert_eq!((outcome.attempts(), outcome.retries()), (1, 0));
    assert_eq!(*attempt_provider.requests.lock().unwrap(), 1);
    assert_eq!(attempt_agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn provider_reported_token_budget_closes_before_assistant_publication() {
    let provider = Arc::new(FakeProvider::new(vec![vec![
        StreamChunk::usage(TokenUsage::new(1, 2).unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]]));
    let limits = AgentLimits::default()
        .with_max_reported_output_tokens_per_turn(1)
        .unwrap();
    let mut agent = agent(
        "token-budget",
        provider,
        Arc::new(FakeTools::default()),
        config().with_limits(limits),
    );
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("tokens")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected token budget")
    };
    assert_eq!(error.code(), "AGENT_TOKEN_BUDGET");
    assert_eq!(outcome.reported_output_tokens(), 2);
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test(start_paused = true)]
async fn turn_deadline_cancels_a_pending_provider_and_closes() {
    let limits = AgentLimits::default()
        .with_turn_duration(std::time::Duration::from_millis(1))
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("turn-timeout"),
        Arc::new(PendingProvider),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config().with_limits(limits),
    )
    .unwrap();
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("wait")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected turn timeout")
    };
    assert_eq!(error.code(), "AGENT_TURN_TIMEOUT");
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn cancellation_while_waiting_for_provider_closes_step_and_turn() {
    let cancellation = CancellationToken::new();
    let watcher = cancellation.clone();
    let mut agent = AgentLoop::with_runtime(
        session("provider-cancel"),
        Arc::new(PendingProvider),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let cancel_task = tokio::spawn(async move {
        tokio::task::yield_now().await;
        watcher.cancel();
    });

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user("wait")]), cancellation)
        .await
        .unwrap();
    cancel_task.await.unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(agent.session().state().open_turn(), None);
    assert!(
        agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::StepEnd { .. }))
    );
}

#[tokio::test]
async fn cancellation_that_races_with_stream_eof_prevents_assistant_commit() {
    let cancellation = CancellationToken::new();
    let provider = Arc::new(CancelAtEofProvider {
        cancellation: cancellation.clone(),
    });
    let mut agent = AgentLoop::with_runtime(
        session("eof-cancel"),
        provider,
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user("race")]), cancellation)
        .await
        .unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn always_ready_provider_yields_before_the_stream_chunk_ceiling() {
    let cancellation = CancellationToken::new();
    let cancel_from_sibling = cancellation.clone();
    let provider = Arc::new(FakeProvider::new(vec![maximum_ready_text_response()]));
    let mut agent = AgentLoop::with_runtime(
        session("ready-stream-yield"),
        provider,
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let (outcome, ()) = tokio::join! {
        biased;
        agent.run_turn(
            TurnProposal::Enter(vec![user("stream fairly")]),
            cancellation,
        ),
        async move { cancel_from_sibling.cancel(); },
    };
    let outcome = outcome.unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    let chunks = agent
        .session()
        .events()
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::AssistantChunk { .. }))
        .count();
    assert!((1..=32).contains(&chunks), "observed {chunks} chunks");
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test(start_paused = true)]
async fn completed_step_yield_resamples_the_turn_deadline_before_turn_end() {
    let limits = AgentLimits::default()
        .with_turn_duration(std::time::Duration::from_millis(1))
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("completed-step-yield"),
        Arc::new(FakeProvider::new(vec![text_response("already committed")])),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config().with_limits(limits),
    )
    .unwrap();

    let (outcome, ()) = tokio::join! {
        biased;
        agent.run_turn(
            TurnProposal::Enter(vec![user("finish fairly")]),
            CancellationToken::new(),
        ),
        async { tokio::time::advance(std::time::Duration::from_millis(2)).await; },
    };
    let outcome = outcome.unwrap();

    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_TURN_TIMEOUT"
    ));
    assert!(
        agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn immediate_step_continuations_yield_before_the_step_limit() {
    let cancellation = CancellationToken::new();
    let cancel_from_sibling = cancellation.clone();
    let entered = Arc::new(Notify::new());
    let tools = Arc::new(NotifyingReadyTools {
        calls: AtomicUsize::new(0),
        entered: Arc::clone(&entered),
    });
    let provider = Arc::new(FakeProvider::new(
        (0..MAX_AGENT_STEPS_PER_TURN)
            .map(|index| tool_response_with_id(&format!("step-call-{index}")))
            .collect(),
    ));
    let limits = AgentLimits::default()
        .with_max_steps_per_turn(MAX_AGENT_STEPS_PER_TURN)
        .unwrap()
        .with_max_attempts_per_turn(MAX_AGENT_ATTEMPTS_PER_TURN)
        .unwrap()
        .with_max_tool_calls_per_turn(MAX_AGENT_TOOL_CALLS_PER_TURN)
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("ready-step-yield"),
        provider,
        tools.clone(),
        Arc::new(FixedRuntime::default()),
        config().with_limits(limits),
    )
    .unwrap();

    let (outcome, ()) = tokio::join! {
        biased;
        agent.run_turn(
            TurnProposal::Enter(vec![user("continue fairly")]),
            cancellation,
        ),
        async move {
            entered.notified().await;
            cancel_from_sibling.cancel();
        },
    };
    let outcome = outcome.unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(tools.calls.load(Ordering::SeqCst), 1);
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn immediate_tool_group_yields_before_all_bodies_run() {
    let cancellation = CancellationToken::new();
    let cancel_from_sibling = cancellation.clone();
    let entered = Arc::new(Notify::new());
    let tools = Arc::new(NotifyingReadyTools {
        calls: AtomicUsize::new(0),
        entered: Arc::clone(&entered),
    });
    let provider = Arc::new(FakeProvider::new(vec![many_tool_response(
        MAX_AGENT_TOOL_CALLS_PER_STEP,
    )]));
    let limits = AgentLimits::default()
        .with_max_tool_calls_per_step(MAX_AGENT_TOOL_CALLS_PER_STEP)
        .unwrap()
        .with_max_tool_calls_per_turn(MAX_AGENT_TOOL_CALLS_PER_TURN)
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("ready-tools-yield"),
        provider,
        tools.clone(),
        Arc::new(FixedRuntime::default()),
        config().with_limits(limits),
    )
    .unwrap();

    let (outcome, ()) = tokio::join! {
        biased;
        agent.run_turn(
            TurnProposal::Enter(vec![user("tools fairly")]),
            cancellation,
        ),
        async move {
            entered.notified().await;
            cancel_from_sibling.cancel();
        },
    };
    let outcome = outcome.unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    let executed = tools.calls.load(Ordering::SeqCst);
    assert!((1..=32).contains(&executed), "executed {executed} bodies");
    assert_eq!(agent.session().state().open_turn(), None);
    let calls = agent
        .session()
        .events()
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
        .count();
    let results = agent
        .session()
        .events()
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
        .count();
    assert_eq!(
        (calls, results),
        (MAX_AGENT_TOOL_CALLS_PER_STEP, MAX_AGENT_TOOL_CALLS_PER_STEP)
    );
}

#[tokio::test]
async fn cancelling_one_of_multiple_tools_pairs_every_call_without_later_side_effects() {
    let cancellation = CancellationToken::new();
    let tools = Arc::new(CancellingTools {
        turn: cancellation.clone(),
        calls: Mutex::new(Vec::new()),
    });
    let provider = Arc::new(FakeProvider::new(vec![two_tool_response()]));
    let mut agent = AgentLoop::with_runtime(
        session("tool-cancel"),
        provider,
        tools.clone(),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("cancel tools")]),
            cancellation,
        )
        .await
        .unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(tools.calls.lock().unwrap().len(), 1);
    let calls = agent
        .session()
        .events()
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
        .count();
    let results = agent
        .session()
        .events()
        .iter()
        .filter(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
        .count();
    assert_eq!((calls, results), (2, 2));
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. }
            if error.code == "ABORTED_BEFORE_DISPATCH"
                && error.name == "AbortError"
    )));
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. }
            if error.code == "ABORTED" && error.name == "AbortError"
    )));
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test(start_paused = true)]
async fn a_started_tool_gets_a_bounded_cooperative_cleanup_poll() {
    let cancellation = CancellationToken::new();
    let cleaned = Arc::new(AtomicBool::new(false));
    let tools = Arc::new(CleanupOnCancelTools {
        turn: cancellation.clone(),
        cleaned: cleaned.clone(),
    });
    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::tool_calls().unwrap(),
    )]));
    let mut agent = AgentLoop::with_runtime(
        session("tool-cleanup"),
        provider,
        tools,
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user("cleanup")]), cancellation)
        .await
        .unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert!(cleaned.load(Ordering::SeqCst));
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. }
            if error.code == "ABORTED" && error.name == "AbortError"
    )));
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test(start_paused = true)]
async fn a_tool_that_ignores_cancellation_is_dropped_after_the_cleanup_grace() {
    let cancellation = CancellationToken::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let tools = Arc::new(IgnoresCancelTools {
        turn: cancellation.clone(),
        dropped: dropped.clone(),
    });
    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::tool_calls().unwrap(),
    )]));
    let mut agent = AgentLoop::with_runtime(
        session("tool-cleanup-grace"),
        provider,
        tools,
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(TurnProposal::Enter(vec![user("cleanup")]), cancellation)
        .await
        .unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert!(dropped.load(Ordering::SeqCst));
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn oversized_concluding_tool_result_falls_back_and_does_not_end_the_turn() {
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response("{}", FinishReason::tool_calls().unwrap()),
        text_response("continued"),
    ]));
    let limits = AgentLimits::default()
        .with_max_tool_result_bytes(128)
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("tool-result-budget"),
        provider.clone(),
        Arc::new(LargeConcludingTools),
        Arc::new(FixedRuntime::default()),
        config().with_limits(limits),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("large result")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(provider.requests().len(), 2);
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. }
            if error.code == "TOOL_OUTPUT_BUDGET_EXCEEDED"
    )));
}

#[tokio::test]
async fn model_facing_tool_failure_continues_but_small_concluding_success_ends_the_turn() {
    let provider = Arc::new(FakeProvider::new(vec![
        tool_response("{}", FinishReason::tool_calls().unwrap()),
        text_response("recovered"),
    ]));
    let mut failure_agent = AgentLoop::with_runtime(
        session("model-facing-tool-failure"),
        provider.clone(),
        Arc::new(ModelErrorTools),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let outcome = failure_agent
        .run_turn(
            TurnProposal::Enter(vec![user("denied tool")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(provider.requests().len(), 2);
    assert!(
        failure_agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(
                event.kind(),
                EventKind::ToolResult { error: Some(error), .. }
                    if error.code == "DENIED" && error.name == "PolicyError"
            ))
    );

    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::tool_calls().unwrap(),
    )]));
    let mut concluding_agent = AgentLoop::with_runtime(
        session("concluding-tool-success"),
        provider.clone(),
        Arc::new(SmallConcludingTools),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let outcome = concluding_agent
        .run_turn(
            TurnProposal::Enter(vec![user("finish in tool")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(outcome.steps(), 1);
    assert_eq!(concluding_agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn executor_failure_is_closed_without_persisting_extension_details() {
    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::tool_calls().unwrap(),
    )]));
    let mut agent = AgentLoop::with_runtime(
        session("tool-infrastructure"),
        provider.clone(),
        Arc::new(InfrastructureTools),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("fail tool")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected infrastructure error")
    };
    assert_eq!(error.code(), "AGENT_TOOL_EXECUTOR");
    assert_eq!(agent.session().state().open_turn(), None);
    assert_eq!(
        agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
            .count(),
        1
    );
    assert_eq!(
        agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
            .count(),
        0
    );
    assert!(matches!(
        agent
            .run_turn(
                TurnProposal::Enter(vec![user("must not dispatch")]),
                CancellationToken::new(),
            )
            .await,
        Err(deepseek_harness_cli::agent::AgentLoopError::Poisoned)
    ));
    assert_eq!(provider.requests().len(), 1);
    let json = agent.session().to_json().unwrap();
    let recovered = agent.shutdown_into_session().await.unwrap();
    assert!(matches!(
        AgentLoop::with_runtime(
            recovered,
            Arc::new(FakeProvider::new(vec![])),
            Arc::new(FakeTools::default()),
            Arc::new(FixedRuntime::default()),
            config(),
        ),
        Err(deepseek_harness_cli::agent::AgentBuildError::UnresolvedToolCall)
    ));
    assert!(!json.contains("SECRET_EXECUTOR_DETAIL"));
    assert_eq!(
        ToolExecutorError::new("SECRET_EXECUTOR_DETAIL").to_string(),
        "tool executor failed"
    );
}

#[tokio::test]
async fn unresolved_guard_uses_only_model_calls_and_canonical_tool_results() {
    let user_tool_call = Message::user(
        "user-tool-call",
        vec![ContentBlock::tool_call("not-model-owned", "calculator", "{}").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap();
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("first"),
        text_response("second"),
    ]));
    let mut user_call_agent = agent(
        "user-tool-call-is-not-pending",
        provider.clone(),
        Arc::new(FakeTools::default()),
        config(),
    );
    user_call_agent
        .run_turn(
            TurnProposal::Enter(vec![user_tool_call]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    user_call_agent
        .run_turn(
            TurnProposal::Enter(vec![user("continue")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(provider.requests().len(), 2);

    let provider = Arc::new(FakeProvider::new(vec![tool_response(
        "{}",
        FinishReason::tool_calls().unwrap(),
    )]));
    let mut unresolved_agent = AgentLoop::with_runtime(
        session("spoofed-tool-result"),
        provider,
        Arc::new(InfrastructureTools),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    unresolved_agent
        .run_turn(
            TurnProposal::Enter(vec![user("leave an unresolved call")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut unresolved = unresolved_agent.shutdown_into_session().await.unwrap();
    let spoofed = Message::tool_result(
        "spoofed-result",
        "call-1",
        vec![ContentBlock::text("not a canonical result event").unwrap()],
        false,
    )
    .unwrap();
    unresolved
        .append(deepseek_harness_cli::session::NewEvent::surface(
            EventKind::user_message(spoofed),
            deepseek_harness_cli::session::SurfaceIntent::append(),
        ))
        .unwrap();
    assert!(matches!(
        AgentLoop::with_runtime(
            unresolved,
            Arc::new(FakeProvider::new(vec![])),
            Arc::new(FakeTools::default()),
            Arc::new(FixedRuntime::default()),
            config(),
        ),
        Err(deepseek_harness_cli::agent::AgentBuildError::UnresolvedToolCall)
    ));
}

#[tokio::test]
async fn infrastructure_failure_on_first_of_two_tools_stops_before_the_second_call() {
    let provider = Arc::new(FakeProvider::new(vec![two_tool_response()]));
    let mut agent = AgentLoop::with_runtime(
        session("two-tool-infrastructure"),
        provider,
        Arc::new(InfrastructureTools),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("two tools")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected infrastructure error")
    };
    assert_eq!(error.code(), "AGENT_TOOL_EXECUTOR");
    assert_eq!(agent.session().state().open_turn(), None);
    assert_eq!(
        agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
            .count(),
        1
    );
    assert_eq!(
        agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
            .count(),
        0
    );
}

#[tokio::test]
async fn undeclared_tool_becomes_model_facing_error_without_executor_side_effect() {
    let provider = Arc::new(FakeProvider::new(vec![
        named_tool_response("not-declared"),
        text_response("recovered"),
    ]));
    let tools = Arc::new(NeverCalledTools::default());
    let mut agent = AgentLoop::with_runtime(
        session("unknown-tool"),
        provider,
        tools.clone(),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("hallucinate")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(*tools.0.lock().unwrap(), 0);
    assert!(agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolResult { error: Some(error), .. } if error.code == "UNKNOWN_TOOL"
    )));
}

#[tokio::test]
async fn provider_must_materialize_a_bounded_output_limit_before_dispatch() {
    let provider = Arc::new(NoMaxTokensProvider {
        streams: Mutex::new(0),
    });
    let mut missing_agent = AgentLoop::with_runtime(
        session("missing-max-output"),
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = missing_agent
        .run_turn(
            TurnProposal::Enter(vec![user("bounded")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected max-output error")
    };
    assert_eq!(error.code(), "AGENT_MAX_OUTPUT_TOKENS");
    assert_eq!(*provider.streams.lock().unwrap(), 0);
    assert_eq!(missing_agent.session().state().open_turn(), None);

    let provider = Arc::new(FakeProvider::new(vec![]));
    let limits = AgentLimits::default()
        .with_max_output_tokens_per_request(1_023)
        .unwrap();
    let mut capped = agent(
        "oversized-prepared-max",
        provider.clone(),
        Arc::new(FakeTools::default()),
        config().with_limits(limits),
    );
    let outcome = capped
        .run_turn(
            TurnProposal::Enter(vec![user("bounded")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_MAX_OUTPUT_TOKENS"
    ));
    assert!(provider.requests().is_empty());
    assert_eq!(capped.session().state().open_turn(), None);
}

#[tokio::test]
async fn hard_wire_limit_closes_only_the_open_turn_before_dispatch() {
    let provider = Arc::new(WireTooLargeProvider::default());
    let mut agent = AgentLoop::with_runtime(
        session("wire-hard-limit"),
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("must fit before entering a step")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_CONTEXT_LIMIT"
    ));
    assert_eq!(outcome.steps(), 0);
    assert_eq!(outcome.attempts(), 0);
    assert_eq!(
        rust_event_types(agent.session()),
        vec!["turn/start", "turn/end"]
    );
    assert_eq!(provider.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(provider.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 0);
    assert_eq!(agent.session().state().open_turn(), None);
    assert_eq!(agent.session().state().open_step(), None);
}

#[tokio::test]
async fn cancellation_latched_during_preflight_overrides_the_hard_wire_limit() {
    let cancellation = CancellationToken::new();
    let provider = Arc::new(WireTooLargeProvider {
        cancel_during_preflight: Some(cancellation.clone()),
        ..WireTooLargeProvider::default()
    });
    let mut agent = AgentLoop::with_runtime(
        session("wire-hard-limit-cancelled"),
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("cancellation wins")]),
            cancellation,
        )
        .await
        .unwrap();

    assert_eq!(
        outcome.reason(),
        &TurnEndReason::Aborted {
            reason: deepseek_harness_cli::session::TurnEndCancelCause::User,
        }
    );
    assert_eq!(outcome.steps(), 0);
    assert_eq!(outcome.attempts(), 0);
    assert_eq!(
        rust_event_types(agent.session()),
        vec!["turn/start", "turn/end"]
    );
    assert_eq!(provider.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(provider.preflights.load(Ordering::SeqCst), 1);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalid_preflight_keeps_the_prepared_header_and_context_without_streaming() {
    let provider = Arc::new(InvalidPreflightProvider::default());
    let mut agent = AgentLoop::with_runtime(
        session("invalid-preflight"),
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("invalid encoded request")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "INVALID_REQUEST"
    ));
    assert_eq!(outcome.steps(), 1);
    assert_eq!(outcome.attempts(), 1);
    assert_eq!(provider.streams.load(Ordering::SeqCst), 0);
    assert_eq!(
        rust_event_types(agent.session()),
        vec![
            "turn/start",
            "step/start",
            "user/message",
            "request/header",
            "request/context",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(agent.session().state().open_step(), None);
    assert_eq!(agent.session().state().open_turn(), None);
}

#[tokio::test]
async fn provider_extension_error_text_never_enters_the_session() {
    let provider = Arc::new(FakeProvider::with_results(vec![vec![Err(
        ProviderStreamError::Model(ModelError::InvalidShape {
            subject: "fake provider",
            detail: "SECRET_PROVIDER_DETAIL".to_owned(),
        }),
    )]]));
    let mut agent = agent(
        "provider-secret",
        provider,
        Arc::new(FakeTools::default()),
        config(),
    );

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("secret")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected provider stream error")
    };
    assert_eq!(error.code(), "AGENT_PROVIDER_STREAM");
    assert!(
        !agent
            .session()
            .to_json()
            .unwrap()
            .contains("SECRET_PROVIDER_DETAIL")
    );
}

#[tokio::test]
async fn internal_runtime_failure_after_stream_still_closes_the_step_and_turn() {
    let provider = Arc::new(FakeProvider::new(vec![text_response("hello")]));
    let mut agent = AgentLoop::with_runtime(
        session("internal-close"),
        provider,
        Arc::new(FakeTools::default()),
        Arc::new(EmptyMessageIdRuntime),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("trigger")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected internal error")
    };
    assert_eq!(error.code(), "AGENT_INTERNAL");
    assert_eq!(agent.session().state().open_turn(), None);
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
}

#[tokio::test]
async fn session_event_floor_rejects_a_step_without_leaking_partial_user_input() {
    let mut nearly_full = session("agent-event-floor");
    while nearly_full.events().len() < MAX_SESSION_EVENTS - 4 {
        nearly_full
            .append(deepseek_harness_cli::session::NewEvent::log(
                EventKind::EndSeed,
            ))
            .unwrap();
    }
    let provider = Arc::new(FakeProvider::new(vec![]));
    let mut agent = AgentLoop::with_runtime(
        nearly_full,
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("must be atomic")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let TurnEndReason::Error { error } = outcome.reason() else {
        panic!("expected event-budget error")
    };
    assert_eq!(error.code(), "AGENT_EVENT_BUDGET");
    assert_eq!(outcome.steps(), 0);
    assert!(provider.requests().is_empty());
    assert!(!agent.session().messages().iter().any(|message| {
        message.content().iter().any(|block| {
            matches!(block.kind(), deepseek_harness_cli::model::ContentBlockKind::Text { text } if text == "must be atomic")
        })
    }));
    assert_eq!(agent.session().state().open_turn(), None);
}

#[test]
fn agent_limit_builders_enforce_every_public_hard_ceiling() {
    macro_rules! check_usize_limit {
        ($method:ident, $minimum:expr, $maximum:expr) => {{
            let minimum = $minimum;
            let maximum = $maximum;
            assert!(AgentLimits::default().$method(minimum).is_ok());
            assert!(AgentLimits::default().$method(maximum).is_ok());
            assert!(AgentLimits::default().$method(maximum + 1).is_err());
            if minimum > 0 {
                assert!(AgentLimits::default().$method(0).is_err());
            }
        }};
    }
    macro_rules! check_u64_limit {
        ($method:ident, $maximum:expr) => {{
            let maximum = $maximum;
            assert!(AgentLimits::default().$method(1).is_ok());
            assert!(AgentLimits::default().$method(maximum).is_ok());
            assert!(AgentLimits::default().$method(maximum + 1).is_err());
            assert!(AgentLimits::default().$method(0).is_err());
        }};
    }

    check_usize_limit!(with_max_steps_per_turn, 1, MAX_AGENT_STEPS_PER_TURN);
    check_usize_limit!(with_max_attempts_per_turn, 1, MAX_AGENT_ATTEMPTS_PER_TURN);
    check_usize_limit!(with_max_retries_per_step, 0, MAX_AGENT_RETRIES_PER_STEP);
    check_usize_limit!(
        with_max_tool_calls_per_step,
        0,
        MAX_AGENT_TOOL_CALLS_PER_STEP
    );
    check_usize_limit!(
        with_max_tool_calls_per_turn,
        0,
        MAX_AGENT_TOOL_CALLS_PER_TURN
    );
    check_usize_limit!(
        with_max_tool_argument_bytes,
        1,
        MAX_AGENT_TOOL_ARGUMENT_BYTES
    );
    check_usize_limit!(with_max_tool_result_bytes, 1, MAX_AGENT_TOOL_RESULT_BYTES);
    check_usize_limit!(
        with_max_tool_results_per_turn_bytes,
        1,
        MAX_AGENT_TOOL_RESULTS_PER_TURN_BYTES
    );
    check_u64_limit!(
        with_max_output_tokens_per_request,
        MAX_AGENT_OUTPUT_TOKENS_PER_REQUEST
    );
    check_u64_limit!(
        with_max_reported_output_tokens_per_turn,
        MAX_AGENT_REPORTED_OUTPUT_TOKENS
    );

    assert!(
        AgentLimits::default()
            .with_turn_duration(std::time::Duration::from_nanos(1))
            .is_ok()
    );
    assert!(
        AgentLimits::default()
            .with_turn_duration(MAX_AGENT_TURN_DURATION)
            .is_ok()
    );
    assert!(
        AgentLimits::default()
            .with_turn_duration(std::time::Duration::ZERO)
            .is_err()
    );
    assert!(
        AgentLimits::default()
            .with_turn_duration(MAX_AGENT_TURN_DURATION + std::time::Duration::from_nanos(1))
            .is_err()
    );
    assert!(
        AgentLimits::default()
            .with_tool_duration(std::time::Duration::from_nanos(1))
            .is_ok()
    );
    assert!(
        AgentLimits::default()
            .with_tool_duration(MAX_AGENT_TOOL_DURATION)
            .is_ok()
    );
    assert!(
        AgentLimits::default()
            .with_tool_duration(std::time::Duration::ZERO)
            .is_err()
    );
    assert!(
        AgentLimits::default()
            .with_tool_duration(MAX_AGENT_TOOL_DURATION + std::time::Duration::from_nanos(1))
            .is_err()
    );
}

#[test]
fn fixed_request_tool_schema_and_tool_result_boundaries_fail_closed() {
    let call = LlmCallConfig::new("mock", "model").unwrap();
    let base = AgentLoopConfig::new(call);
    let remaining = MAX_AGENT_FIXED_REQUEST_BYTES - base.call().raw().encoded_len();
    assert!(base.clone().with_system("x".repeat(remaining)).is_ok());
    assert!(base.with_system("x".repeat(remaining + 1)).is_err());

    let parameters =
        || deepseek_harness_cli::model::JsonValue::new(json!({"type":"object"})).unwrap();
    let maximum_name = ToolSchema::new("x".repeat(256), "ok", parameters()).unwrap();
    assert!(
        AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
            .with_tools(vec![maximum_name])
            .is_ok()
    );
    for bad_name in ["x".repeat(257), "bad\nname".to_owned()] {
        let schema = ToolSchema::new(bad_name, "bad", parameters()).unwrap();
        assert!(
            AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
                .with_tools(vec![schema])
                .is_err()
        );
    }
    let empty = ToolSchema::new("", "bad", parameters()).unwrap();
    assert!(
        AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
            .with_tools(vec![empty])
            .is_err()
    );
    let duplicate = ToolSchema::new("duplicate", "one", parameters()).unwrap();
    assert!(
        AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
            .with_tools(vec![
                duplicate.clone(),
                ToolSchema::new("duplicate", "two", parameters()).unwrap(),
            ])
            .is_err()
    );

    assert!(
        ToolExecutionResult::success(vec![
            ContentBlock::text("x".repeat(140 * 1024)).unwrap(),
            ContentBlock::text("y".repeat(140 * 1024)).unwrap(),
        ])
        .is_err()
    );
    let failure = deepseek_harness_cli::session::ToolFailure {
        name: "Error".to_owned(),
        code: "FAILED".to_owned(),
    };
    assert!(
        ToolExecutionResult::new(Vec::new(), false, Some(failure.clone()), None, false).is_err()
    );
    assert!(ToolExecutionResult::new(Vec::new(), true, None, None, false).is_err());
    assert!(ToolExecutionResult::new(Vec::new(), true, Some(failure), None, true).is_err());
}

#[test]
fn public_agent_debug_summaries_omit_prompt_tool_and_result_payloads() {
    let schema = ToolSchema::new(
        "safe-name",
        "SECRET_SCHEMA_DESCRIPTION",
        deepseek_harness_cli::model::JsonValue::new(json!({
            "type": "object",
            "SECRET_SCHEMA_PARAMETER": true
        }))
        .unwrap(),
    )
    .unwrap();
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_system("SECRET_SYSTEM_PROMPT")
        .unwrap()
        .with_tools(vec![schema])
        .unwrap();
    let config_debug = format!("{config:?}");
    for secret in [
        "SECRET_SYSTEM_PROMPT",
        "SECRET_SCHEMA_DESCRIPTION",
        "SECRET_SCHEMA_PARAMETER",
    ] {
        assert!(!config_debug.contains(secret));
    }

    let result = ToolExecutionResult::new(
        vec![ContentBlock::text("SECRET_TOOL_CONTENT").unwrap()],
        true,
        Some(deepseek_harness_cli::session::ToolFailure {
            name: "SECRET_FAILURE_NAME".to_owned(),
            code: "SECRET_FAILURE_CODE".to_owned(),
        }),
        Some(
            deepseek_harness_cli::model::JsonValue::new(json!({
                "SECRET_TOOL_META": true
            }))
            .unwrap(),
        ),
        false,
    )
    .unwrap();
    let result_debug = format!("{result:?}");
    for secret in [
        "SECRET_TOOL_CONTENT",
        "SECRET_FAILURE_NAME",
        "SECRET_FAILURE_CODE",
        "SECRET_TOOL_META",
    ] {
        assert!(!result_debug.contains(secret));
    }
}

#[tokio::test]
async fn turn_input_count_and_aggregate_bytes_are_rejected_before_logging() {
    let mut too_many =
        vec![user("small"); deepseek_harness_cli::provider::MAX_PROVIDER_MESSAGES + 1];
    too_many[deepseek_harness_cli::provider::MAX_PROVIDER_MESSAGES] = Message::assistant(
        "unexamined-tail",
        vec![ContentBlock::text("not user").unwrap()],
        "mock",
        "model",
    )
    .unwrap();
    let mut count_agent = agent(
        "turn-count",
        Arc::new(FakeProvider::new(vec![])),
        Arc::new(FakeTools::default()),
        config(),
    );
    assert!(matches!(
        count_agent
            .run_turn(TurnProposal::Enter(too_many), CancellationToken::new())
            .await,
        Err(deepseek_harness_cli::agent::AgentLoopError::TooManyTurnMessages { .. })
    ));
    assert!(count_agent.session().events().is_empty());

    let half = deepseek_harness_cli::provider::MAX_PROVIDER_REQUEST_BYTES / 2 + 1_024;
    let large = vec![
        user_with_id("large-1", &"x".repeat(half)),
        user_with_id("large-2", &"y".repeat(half)),
    ];
    let mut bytes_agent = agent(
        "turn-bytes",
        Arc::new(FakeProvider::new(vec![])),
        Arc::new(FakeTools::default()),
        config(),
    );
    assert!(matches!(
        bytes_agent
            .run_turn(TurnProposal::Enter(large), CancellationToken::new())
            .await,
        Err(deepseek_harness_cli::agent::AgentLoopError::TurnInputTooLarge { .. })
    ));
    assert!(bytes_agent.session().events().is_empty());
}

#[tokio::test]
async fn aggregate_tool_result_budget_keeps_pairs_and_falls_back_on_the_second_result() {
    let provider = Arc::new(FakeProvider::new(vec![
        two_tool_response(),
        text_response("continued"),
    ]));
    let tools = Arc::new(LargeResultTools::default());
    let limits = AgentLimits::default()
        .with_max_tool_result_bytes(8 * 1024)
        .unwrap()
        .with_max_tool_results_per_turn_bytes(8 * 1024)
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("aggregate-tool-results"),
        provider.clone(),
        tools.clone(),
        Arc::new(FixedRuntime::default()),
        config().with_limits(limits),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("two results")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(*tools.0.lock().unwrap(), 2);
    assert_eq!(provider.requests().len(), 2);
    let results = agent
        .session()
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::ToolResult { error, .. } => Some(error.as_ref().map(|error| &error.code)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    assert_eq!(results.iter().filter(|error| error.is_none()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|error| error.is_some_and(|code| code == "TOOL_OUTPUT_BUDGET_EXCEEDED"))
            .count(),
        1
    );
}

#[tokio::test]
async fn provider_and_tool_panics_close_without_persisting_the_panic_payload() {
    let mut prepare_agent = AgentLoop::with_runtime(
        session("prepare-panic"),
        Arc::new(PanicPrepareProvider),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let outcome = prepare_agent
        .run_turn(
            TurnProposal::Enter(vec![user("prepare")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_PROVIDER_PANIC"
    ));
    assert_eq!(outcome.steps(), 1);
    assert_eq!(outcome.attempts(), 1);
    assert_eq!(
        rust_event_types(prepare_agent.session()),
        vec![
            "turn/start",
            "step/start",
            "user/message",
            "step/end",
            "turn/end",
        ]
    );
    assert_eq!(prepare_agent.session().state().open_turn(), None);
    assert!(
        !prepare_agent
            .session()
            .to_json()
            .unwrap()
            .contains("SECRET_")
    );

    let stream_provider = Arc::new(PanicStreamProvider {
        cancellation: Mutex::new(None),
    });
    let mut stream_agent = AgentLoop::with_runtime(
        session("stream-panic"),
        stream_provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let outcome = stream_agent
        .run_turn(
            TurnProposal::Enter(vec![user("stream")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_PROVIDER_PANIC"
    ));
    assert!(
        stream_provider
            .cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert_eq!(stream_agent.session().state().open_turn(), None);
    assert!(
        !stream_agent
            .session()
            .to_json()
            .unwrap()
            .contains("SECRET_")
    );

    let poll_provider = Arc::new(PanicStreamPollProvider {
        cancellation: Mutex::new(None),
    });
    let mut poll_agent = AgentLoop::with_runtime(
        session("stream-poll-panic"),
        poll_provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();
    let outcome = poll_agent
        .run_turn(
            TurnProposal::Enter(vec![user("stream poll")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_PROVIDER_PANIC"
    ));
    assert!(
        poll_provider
            .cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    assert_eq!(poll_agent.session().state().open_turn(), None);
    assert!(!poll_agent.session().to_json().unwrap().contains("SECRET_"));

    for (id, tools) in [
        (
            "tool-factory-panic",
            Arc::new(PanicFactoryTools) as Arc<dyn ToolExecutor>,
        ),
        (
            "tool-poll-panic",
            Arc::new(PanicPollTools) as Arc<dyn ToolExecutor>,
        ),
    ] {
        let mut tool_agent = AgentLoop::with_runtime(
            session(id),
            Arc::new(FakeProvider::new(vec![tool_response(
                "{}",
                FinishReason::tool_calls().unwrap(),
            )])),
            tools,
            Arc::new(FixedRuntime::default()),
            config(),
        )
        .unwrap();
        let outcome = tool_agent
            .run_turn(
                TurnProposal::Enter(vec![user("tool panic")]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(matches!(
            outcome.reason(),
            TurnEndReason::Error { error } if error.code() == "AGENT_TOOL_EXECUTOR"
        ));
        assert_eq!(tool_agent.session().state().open_turn(), None);
        assert_eq!(
            tool_agent
                .session()
                .events()
                .iter()
                .filter(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
                .count(),
            1
        );
        assert_eq!(
            tool_agent
                .session()
                .events()
                .iter()
                .filter(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
                .count(),
            0
        );
        assert!(!tool_agent.session().to_json().unwrap().contains("SECRET_"));
    }
}

#[tokio::test]
async fn partial_stream_that_hits_the_event_floor_still_cancels_and_closes() {
    let mut nearly_full = session("partial-event-floor");
    while nearly_full.events().len() < MAX_SESSION_EVENTS - 10 {
        nearly_full
            .append(deepseek_harness_cli::session::NewEvent::log(
                EventKind::EndSeed,
            ))
            .unwrap();
    }
    let provider = Arc::new(TokenObservingProvider {
        chunks: Mutex::new(Some(text_response("partially logged"))),
        cancellation: Mutex::new(None),
    });
    let mut agent = AgentLoop::with_runtime(
        nearly_full,
        provider.clone(),
        Arc::new(FakeTools::default()),
        Arc::new(FixedRuntime::default()),
        config(),
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("fill the log")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(matches!(
        outcome.reason(),
        TurnEndReason::Error { error } if error.code() == "AGENT_EVENT_BUDGET"
    ));
    assert_eq!(agent.session().events().len(), MAX_SESSION_EVENTS);
    assert_eq!(agent.session().state().open_turn(), None);
    assert_eq!(agent.session().state().open_step(), None);
    assert_eq!(
        agent
            .session()
            .events()
            .iter()
            .filter(|event| matches!(event.kind(), EventKind::AssistantChunk { .. }))
            .count(),
        3
    );
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
    assert!(
        provider
            .cancellation
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
}

#[tokio::test]
async fn two_turns_match_the_committed_phase7_acp_oracle() {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/cli/upstream_phase7_oracle.json")).unwrap();
    assert_eq!(oracle["schemaVersion"], 1);
    assert_eq!(
        oracle["upstream"]["commit"],
        "47f943859bef60e4160492346772ded9b24f765a"
    );
    let expected = &oracle["scenarios"]["acpTwoTurns"];
    assert!(all_boolean_checks_are_true(&expected["checks"]));

    let attempts = [1_u64, 2]
        .into_iter()
        .map(|turn| phase7_chunks_for_turn(expected, turn))
        .collect::<Vec<_>>();
    let provider = Arc::new(FakeProvider::new(attempts));
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "mock").unwrap())
        .with_system("You are an AI agent powered by DeepSeek Harness.\n\nPhase 7 oracle.")
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("phase7-two-turns"),
        provider.clone(),
        Arc::new(NeverCalledTools::default()),
        Arc::new(FixedRuntime::default()),
        config,
    )
    .unwrap();

    for (index, prompt) in expected["prompts"].as_array().unwrap().iter().enumerate() {
        let text = prompt["text"].as_str().unwrap();
        let outcome = agent
            .run_turn(
                TurnProposal::Enter(vec![user_with_id(
                    &format!("phase7-user-{}", index + 1),
                    text,
                )]),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(turn_reason_kind(outcome.reason()), "completed");
    }

    assert_eq!(
        rust_event_types(agent.session()),
        upstream_phase7_core_event_types(expected),
        "Phase 7 two-turn durable event order"
    );
    assert_eq!(
        rust_assistant_chunks(agent.session()),
        upstream_phase7_assistant_chunks(expected),
        "Phase 7 two-turn raw chunks"
    );
    assert_eq!(
        phase7_turn_ends(agent.session()),
        expected["durableTurnEnds"],
        "Phase 7 two-turn reasons"
    );
    assert_eq!(
        phase7_wire_updates(agent.session()),
        expected["wireUpdates"],
        "Phase 7 ACP committed assistant updates"
    );

    let requests = provider.requests();
    let expected_requests = expected["providerRequestTranscripts"].as_array().unwrap();
    assert_eq!(requests.len(), expected_requests.len());
    for (actual, expected) in requests.iter().zip(expected_requests) {
        assert_eq!(
            messages_without_ids(actual),
            expected["messages"],
            "Phase 7 request history must retain the prior committed turn"
        );
    }
}

#[tokio::test]
async fn cancellation_then_continuation_matches_the_committed_phase7_acp_oracle() {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/cli/upstream_phase7_oracle.json")).unwrap();
    assert_eq!(
        oracle["upstream"]["commit"],
        "47f943859bef60e4160492346772ded9b24f765a"
    );
    let expected = &oracle["scenarios"]["acpCancelThenContinue"];
    assert!(all_boolean_checks_are_true(&expected["checks"]));

    let first_chunks = expected["cancelledIntervalEvents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["type"] == "assistant/chunk")
        .map(|event| serde_json::from_value(event["data"]["chunk"].clone()).unwrap())
        .collect::<Vec<_>>();
    let partial_committed = Arc::new(Notify::new());
    let provider = Arc::new(Phase7CancelThenContinueProvider::new(
        first_chunks,
        partial_committed.clone(),
    ));
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "mock").unwrap())
        .with_system("You are an AI agent powered by DeepSeek Harness.\n\nPhase 7 oracle.")
        .unwrap();
    let mut agent = AgentLoop::with_runtime(
        session("phase7-cancel-continue"),
        provider.clone(),
        Arc::new(NeverCalledTools::default()),
        Arc::new(FixedRuntime::default()),
        config,
    )
    .unwrap();

    let cancellation = CancellationToken::new();
    let first_outcome = {
        let first_turn = agent.run_turn(
            TurnProposal::Enter(vec![user_with_id(
                "phase7-cancel-user-1",
                expected["firstPrompt"]["text"]
                    .as_str()
                    .unwrap_or("start cancellable turn"),
            )]),
            cancellation.clone(),
        );
        tokio::pin!(first_turn);
        tokio::select! {
            biased;
            result = &mut first_turn => panic!("turn finished before the durable partial barrier: {result:?}"),
            () = partial_committed.notified() => {
                cancellation.cancel();
                (&mut first_turn).await.unwrap()
            }
        }
    };
    assert!(matches!(
        first_outcome.reason(),
        TurnEndReason::Aborted { reason }
            if matches!(reason, deepseek_harness_cli::session::TurnEndCancelCause::User)
    ));

    let first_interval_len = agent.session().events().len();
    assert_eq!(
        rust_event_types(agent.session()),
        expected["cancelledIntervalEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|event| event["type"].as_str())
            .filter(|event_type| *event_type != "agent/inbox/spliced")
            .collect::<Vec<_>>()
    );
    assert_eq!(
        rust_assistant_chunks(agent.session()),
        expected["cancelledIntervalEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["type"] == "assistant/chunk")
            .map(|event| event["data"]["chunk"].clone())
            .collect::<Vec<_>>()
    );
    assert!(
        !agent
            .session()
            .events()
            .iter()
            .any(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );
    assert!(
        phase7_wire_updates(agent.session())
            .as_array()
            .unwrap()
            .is_empty()
    );

    // The fixed upstream generator supplies this prompt, but the current fixture records only
    // its response. Keep the literal tied to the audited generator until the fixture grows it.
    let second_outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user_with_id(
                "phase7-cancel-user-2",
                "continue in same session",
            )]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(turn_reason_kind(second_outcome.reason()), "completed");
    assert_eq!(
        phase7_turn_ends(agent.session()),
        expected["finalDurableTurnEnds"]
    );
    assert_eq!(
        phase7_wire_updates(agent.session()),
        expected["finalWireUpdates"]
    );
    assert!(
        agent.session().events()[..first_interval_len]
            .iter()
            .all(|event| !matches!(event.kind(), EventKind::AssistantMessage { .. }))
    );

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    let second_request = messages_without_ids(&requests[1]);
    let second_request_json = serde_json::to_string(&second_request).unwrap();
    assert!(second_request_json.contains("continue in same session"));
    assert!(!second_request_json.contains("partial-before-cancel"));
}

#[tokio::test(start_paused = true)]
async fn rust_core_traces_match_the_committed_upstream_phase3_oracle() {
    let oracle: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/agent/upstream_phase3_oracle.json")).unwrap();
    assert_eq!(
        oracle["upstream"]["commit"],
        "47f943859bef60e4160492346772ded9b24f765a"
    );
    for scenario in [
        "textCompletion",
        "singleToolRoundTrip",
        "retrySameStep",
        "maxTokens",
        "preStepReject",
    ] {
        let value = &oracle["scenarios"][scenario];
        assert!(value.is_object(), "missing oracle scenario {scenario}");
        assert!(all_boolean_checks_are_true(&value["checks"]));
        for request in value["requests"].as_array().into_iter().flatten() {
            assert!(
                request["checks"]["messagesEqualDerivation"]
                    .as_bool()
                    .unwrap()
            );
            assert!(request["checks"]["completeHeaderEqual"].as_bool().unwrap());
            assert!(
                request["checks"]["headerLoggedBeforeDispatch"]
                    .as_bool()
                    .unwrap()
            );
            assert!(
                request["checks"]["contextLoggedBeforeDispatch"]
                    .as_bool()
                    .unwrap()
            );
        }
    }

    const BASE_SYSTEM: &str =
        "You are an AI agent powered by DeepSeek Harness.\n\nPhase 3 oracle persona.";
    let text_system = format!("{BASE_SYSTEM}\n\nOnly report observed facts.");
    let cases = [
        (
            "textCompletion",
            "phase3-text",
            TurnProposal::Enter(vec![user_with_id("user-text", "complete once")]),
            text_system.as_str(),
            false,
            "completed",
            1,
            1,
            0,
            0,
            2,
        ),
        (
            "singleToolRoundTrip",
            "phase3-tool",
            TurnProposal::Enter(vec![user_with_id("user-tool", "call echo")]),
            BASE_SYSTEM,
            true,
            "completed",
            2,
            2,
            0,
            1,
            5,
        ),
        (
            "retrySameStep",
            "phase3-retry",
            TurnProposal::Enter(vec![user_with_id("user-retry", "retry once")]),
            BASE_SYSTEM,
            false,
            "completed",
            1,
            2,
            1,
            0,
            2,
        ),
        (
            "maxTokens",
            "phase3-max-tokens",
            TurnProposal::Enter(vec![user_with_id("user-max", "hit output limit")]),
            BASE_SYSTEM,
            true,
            "max-tokens",
            1,
            1,
            0,
            0,
            9,
        ),
        (
            "preStepReject",
            "phase3-reject",
            TurnProposal::Reject,
            BASE_SYSTEM,
            false,
            "blocked",
            0,
            0,
            0,
            0,
            0,
        ),
    ];

    for (
        name,
        session_id,
        proposal,
        system,
        with_tool,
        reason,
        steps,
        request_count,
        retries,
        tool_calls,
        output_tokens,
    ) in cases
    {
        let expected = &oracle["scenarios"][name];
        let attempts = attempts_from_oracle(expected);
        let retry_policy = if name == "retrySameStep" {
            RetryPolicy::normal(
                2,
                vec!["RATE_LIMIT".to_owned(), "SERVER".to_owned()],
                RetryBackoff::new(1.0, 1.0, 0.0).unwrap(),
            )
            .unwrap()
        } else {
            RetryPolicy::normal(
                2,
                vec!["SERVER".to_owned()],
                RetryBackoff::new(1.0, 1.0, 0.0).unwrap(),
            )
            .unwrap()
        };
        let clock_calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FakeProvider::for_oracle(
            attempts,
            retry_policy,
            clock_calls.clone(),
        ));
        let body_event_counts = Arc::new(Mutex::new(Vec::new()));
        let tools: Arc<dyn ToolExecutor> = if with_tool {
            Arc::new(OracleEchoTools {
                clock_calls: clock_calls.clone(),
                body_event_counts: body_event_counts.clone(),
            })
        } else {
            Arc::new(NeverCalledTools::default())
        };
        let mut agent = AgentLoop::with_runtime(
            Session::with_clock(
                session_id,
                ProbeClock {
                    calls: clock_calls.clone(),
                },
            )
            .unwrap(),
            provider.clone(),
            tools,
            Arc::new(FixedRuntime::default()),
            oracle_config(system, with_tool),
        )
        .unwrap();
        let outcome = agent
            .run_turn(proposal, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(
            rust_event_types(agent.session()),
            upstream_core_event_types(expected),
            "{name}: core event sequence"
        );
        assert_eq!(turn_reason_kind(outcome.reason()), reason, "{name}: reason");
        assert_eq!(outcome.steps(), steps, "{name}: steps");
        assert_eq!(outcome.attempts(), request_count, "{name}: attempts");
        assert_eq!(outcome.retries(), retries, "{name}: retries");
        assert_eq!(outcome.tool_calls(), tool_calls, "{name}: tool calls");
        assert_eq!(
            outcome.reported_output_tokens(),
            output_tokens,
            "{name}: output tokens"
        );
        assert_eq!(provider.requests().len(), request_count, "{name}: requests");
        assert_eq!(agent.session().state().open_turn(), None, "{name}: turn");
        assert_eq!(agent.session().state().open_step(), None, "{name}: step");
        assert!(agent.session().state().pending_calls().is_empty());
        assert_eq!(
            normalize_message_values(serde_json::to_value(agent.session().messages()).unwrap()),
            normalize_message_values(expected["derivedMessages"].clone()),
            "{name}: final model-visible history"
        );
        let request_facts = provider.request_facts();
        let dispatch_event_counts = provider.dispatch_event_counts();
        let expected_requests = expected["requests"].as_array().unwrap();
        assert_eq!(request_facts.len(), expected_requests.len());
        assert_eq!(dispatch_event_counts.len(), expected_requests.len());
        for ((actual_facts, actual_event_count), expected_request) in request_facts
            .iter()
            .zip(&dispatch_event_counts)
            .zip(expected_requests)
        {
            let replay = Session::replay(&agent.session().events()[..*actual_event_count]).unwrap();
            assert_eq!(
                actual_facts.messages,
                replay.messages(),
                "{name}: dispatch messages must already be reconstructible from the log"
            );
            assert_eq!(
                rust_event_types_from_events(&agent.session().events()[..*actual_event_count]),
                upstream_request_core_event_types(expected_request),
                "{name}: dispatch-time committed prefix"
            );
            assert_eq!(
                normalize_message_values(serde_json::to_value(&actual_facts.messages).unwrap()),
                normalize_message_values(expected_request["request"]["messages"].clone()),
                "{name}: request messages"
            );
            assert_eq!(
                expected_request["request"]["provider"],
                actual_facts.provider
            );
            assert_eq!(expected_request["request"]["model"], actual_facts.model);
            assert_eq!(
                expected_request["request"]["maxTokens"].as_u64(),
                actual_facts.max_tokens
            );
            assert_eq!(
                expected_request["request"]["system"].as_str(),
                actual_facts.system.as_deref()
            );
            assert_eq!(
                expected_request["request"]["tools"]
                    .as_array()
                    .map_or(0, Vec::len),
                actual_facts.tools.len()
            );
            assert_eq!(
                expected_request["request"]["tools"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default(),
                serde_json::to_value(&actual_facts.tools)
                    .unwrap()
                    .as_array()
                    .cloned()
                    .unwrap()
            );
            assert_eq!(
                expected_request["requestContext"]["contextWindow"].as_u64(),
                actual_facts.context_window
            );
            assert_eq!(
                expected_request["request"]["sessionId"].as_str(),
                actual_facts.session_id.as_deref()
            );
            assert_eq!(
                expected_request["foldedHeader"]["config"],
                actual_facts.config
            );
            assert_eq!(
                expected_request["foldedHeader"],
                serde_json::to_value(replay.request_header().unwrap()).unwrap(),
                "{name}: replayed request header at dispatch"
            );
            assert_eq!(
                expected_request["requestContext"],
                serde_json::to_value(replay.request_context().unwrap()).unwrap(),
                "{name}: replayed request context at dispatch"
            );
        }

        assert_eq!(
            rust_assistant_chunks(agent.session()),
            upstream_assistant_chunks(expected),
            "{name}: provider chunks are retained without translation drift"
        );
        assert_surface_provenance_matches(name, agent.session(), expected);

        if name == "singleToolRoundTrip" {
            let tool_snapshots = expected["toolBodySnapshots"].as_array().unwrap();
            assert_eq!(
                body_event_counts.lock().unwrap().len(),
                tool_snapshots.len()
            );
            for (actual_event_count, expected_snapshot) in
                body_event_counts.lock().unwrap().iter().zip(tool_snapshots)
            {
                assert_eq!(
                    rust_event_types_from_events(&agent.session().events()[..*actual_event_count]),
                    upstream_core_types(&expected_snapshot["eventTypes"]),
                    "{name}: tool body starts only after tool/call is committed"
                );
            }
            let call = agent
                .session()
                .events()
                .iter()
                .find(|event| matches!(event.kind(), EventKind::ToolCall { .. }))
                .unwrap();
            let result = agent
                .session()
                .events()
                .iter()
                .find(|event| matches!(event.kind(), EventKind::ToolResult { .. }))
                .unwrap();
            let EventKind::ToolCall {
                call_id,
                name,
                arguments,
                ..
            } = call.kind()
            else {
                unreachable!()
            };
            assert_eq!(call_id.as_str(), "call-echo-1");
            assert_eq!(name, "echo");
            assert_eq!(arguments, "{\"text\":\"hello\"}");
            assert_eq!(result.source_event_seqs(), Some([call.seq()].as_slice()));
            let EventKind::ToolResult { message, .. } = result.kind() else {
                unreachable!()
            };
            assert!(matches!(
                message.source().kind(),
                deepseek_harness_cli::model::MessageSourceKind::Tool { call_id }
                    if call_id.as_str() == "call-echo-1"
            ));
        }
        if name == "retrySameStep" {
            let expected_retry = expected["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["type"] == "llm/retry")
                .unwrap();
            let expected_started = expected["events"]
                .as_array()
                .unwrap()
                .iter()
                .find(|event| event["type"] == "llm/retry-started")
                .unwrap();
            let retry = agent
                .session()
                .events()
                .iter()
                .find_map(|event| match event.kind() {
                    EventKind::LlmRetry { retry } => Some(retry),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                normalized_retry_value(serde_json::to_value(retry).unwrap()),
                normalized_retry_value(expected_retry["data"].clone()),
                "{name}: durable retry payload"
            );
            let started = agent
                .session()
                .events()
                .iter()
                .find_map(|event| match event.kind() {
                    EventKind::LlmRetryStarted { started } => Some(started),
                    _ => None,
                })
                .unwrap();
            assert_eq!(
                normalized_retry_value(serde_json::to_value(started).unwrap()),
                normalized_retry_value(expected_started["data"].clone()),
                "{name}: durable retry-started payload"
            );
            assert_eq!(started.retry_id(), retry.retry_id());
            assert_eq!(started.turn(), retry.turn());
            assert_eq!(started.step(), retry.step());
            assert_eq!(started.retry(), retry.retry());
        }
    }
}

fn phase7_chunks_for_turn(scenario: &serde_json::Value, turn: u64) -> Vec<StreamChunk> {
    scenario["durableEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| {
            event["type"] == "assistant/chunk" && event["data"]["turn"].as_u64() == Some(turn)
        })
        .map(|event| serde_json::from_value(event["data"]["chunk"].clone()).unwrap())
        .collect()
}

fn upstream_phase7_core_event_types(scenario: &serde_json::Value) -> Vec<&str> {
    scenario["durableEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|event| event["type"].as_str())
        .filter(|event_type| *event_type != "agent/inbox/spliced")
        .collect()
}

fn upstream_phase7_assistant_chunks(scenario: &serde_json::Value) -> Vec<serde_json::Value> {
    scenario["durableEvents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["type"] == "assistant/chunk")
        .map(|event| event["data"]["chunk"].clone())
        .collect()
}

fn messages_without_ids(messages: &[Message]) -> serde_json::Value {
    let mut value = serde_json::to_value(messages).unwrap();
    for message in value.as_array_mut().into_iter().flatten() {
        message.as_object_mut().unwrap().remove("id");
    }
    value
}

fn phase7_turn_ends(session: &Session) -> serde_json::Value {
    serde_json::Value::Array(
        session
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                EventKind::TurnEnd { turn, reason } => Some(json!({
                    "turn": turn,
                    "reason": reason,
                })),
                _ => None,
            })
            .collect(),
    )
}

fn phase7_wire_updates(session: &Session) -> serde_json::Value {
    serde_json::Value::Array(
        session
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                EventKind::AssistantMessage { message, .. } => {
                    let text = message
                        .content()
                        .iter()
                        .filter_map(|block| match block.kind() {
                            deepseek_harness_cli::model::ContentBlockKind::Text { text } => {
                                Some(text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<String>();
                    (!text.is_empty()).then(|| {
                        json!({
                            "content": { "text": text, "type": "text" },
                            "sessionUpdate": "agent_message_chunk",
                        })
                    })
                }
                _ => None,
            })
            .collect(),
    )
}

fn attempts_from_oracle(scenario: &serde_json::Value) -> Vec<Vec<StreamChunk>> {
    let events = scenario["events"].as_array().unwrap();
    let requests = scenario["requests"].as_array().unwrap();
    requests
        .iter()
        .enumerate()
        .map(|(index, request)| {
            let start = usize::try_from(request["eventCount"].as_u64().unwrap()).unwrap();
            let end = requests
                .get(index + 1)
                .map(|next| usize::try_from(next["eventCount"].as_u64().unwrap()).unwrap())
                .unwrap_or(events.len());
            events[start..end]
                .iter()
                .filter(|event| event["type"] == "assistant/chunk")
                .map(|event| serde_json::from_value(event["data"]["chunk"].clone()).unwrap())
                .collect()
        })
        .collect()
}

fn rust_event_types(session: &Session) -> Vec<&str> {
    rust_event_types_from_events(session.events())
}

fn request_header_reasons(session: &Session) -> Vec<RequestHeaderReason> {
    session
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::RequestHeader { reason, .. } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

fn request_header_payloads(session: &Session) -> serde_json::Value {
    serde_json::Value::Array(
        session
            .events()
            .iter()
            .filter_map(|event| match event.kind() {
                EventKind::RequestHeader { header, reason } => Some(json!({
                    "header": header,
                    "reason": reason,
                })),
                _ => None,
            })
            .collect(),
    )
}

fn rust_event_types_from_events(
    events: &[deepseek_harness_cli::session::SessionEvent],
) -> Vec<&str> {
    events
        .iter()
        .map(|event| event.kind().event_type())
        .collect()
}

fn upstream_core_event_types(scenario: &serde_json::Value) -> Vec<&str> {
    scenario["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|event| event["type"].as_str())
        .filter(|event_type| *event_type != "agent/inbox/spliced")
        .collect()
}

fn upstream_request_core_event_types(request: &serde_json::Value) -> Vec<&str> {
    upstream_core_types(&request["eventTypes"])
}

fn upstream_core_types(types: &serde_json::Value) -> Vec<&str> {
    types
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .filter(|event_type| *event_type != "agent/inbox/spliced")
        .collect()
}

fn rust_assistant_chunks(session: &Session) -> Vec<serde_json::Value> {
    session
        .events()
        .iter()
        .filter_map(|event| match event.kind() {
            EventKind::AssistantChunk { chunk, .. } => Some(serde_json::to_value(chunk).unwrap()),
            _ => None,
        })
        .collect()
}

fn upstream_assistant_chunks(scenario: &serde_json::Value) -> Vec<serde_json::Value> {
    scenario["events"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|event| event["type"] == "assistant/chunk")
        .map(|event| event["data"]["chunk"].clone())
        .collect()
}

fn assert_surface_provenance_matches(name: &str, session: &Session, scenario: &serde_json::Value) {
    let upstream_events = scenario["events"].as_array().unwrap();
    let upstream_core_events = upstream_events
        .iter()
        .filter(|event| event["type"] != "agent/inbox/spliced")
        .collect::<Vec<_>>();
    assert_eq!(session.events().len(), upstream_core_events.len());
    for (actual, expected) in session.events().iter().zip(upstream_core_events) {
        let expected_sources = expected["sourceEventSeqs"].as_array().map(|sources| {
            sources
                .iter()
                .map(|source| {
                    let source = source.as_u64().unwrap();
                    u64::try_from(
                        upstream_events
                            .iter()
                            .filter(|event| event["type"] != "agent/inbox/spliced")
                            .position(|event| event["seq"].as_u64() == Some(source))
                            .unwrap(),
                    )
                    .unwrap()
                })
                .collect::<Vec<_>>()
        });
        let actual_sources = actual.source_event_seqs().map(|sources| {
            sources
                .iter()
                .map(|source| source.get())
                .collect::<Vec<_>>()
        });
        assert_eq!(
            actual_sources, expected_sources,
            "{name}: surface provenance"
        );
    }
}

fn normalize_message_values(mut value: serde_json::Value) -> serde_json::Value {
    for (index, message) in value.as_array_mut().into_iter().flatten().enumerate() {
        if message["role"] != "user" || message["source"]["kind"] != "user" {
            message["id"] = format!("generated-message-{index}").into();
        }
    }
    value
}

fn normalized_retry_value(mut value: serde_json::Value) -> serde_json::Value {
    if value.get("retryId").is_some() {
        value["retryId"] = "normalized-retry-id".into();
    }
    value
}

fn turn_reason_kind(reason: &TurnEndReason) -> &'static str {
    match reason {
        TurnEndReason::Completed => "completed",
        TurnEndReason::Blocked => "blocked",
        TurnEndReason::MaxTokens => "max-tokens",
        TurnEndReason::Aborted { .. } => "aborted",
        TurnEndReason::Error { .. } => "error",
        TurnEndReason::Interrupted => "interrupted",
        TurnEndReason::Other { .. } => "other",
    }
}

fn all_boolean_checks_are_true(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::Array(values) => values.iter().all(all_boolean_checks_are_true),
        serde_json::Value::Object(values) => values.values().all(all_boolean_checks_are_true),
        _ => true,
    }
}
