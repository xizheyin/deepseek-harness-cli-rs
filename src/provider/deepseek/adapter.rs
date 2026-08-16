//! Lazy DeepSeek provider orchestration.

use std::{collections::VecDeque, future::Future, sync::Arc, time::SystemTime};

use futures_util::{StreamExt, stream};
use tokio::time::Instant;
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::{
    model::{
        LlmCallConfig, LlmCallConfigAdapterDefaults, LlmFailure, NonNegativeSafeInteger,
        ReasoningEffortId, StreamChunk, TrueMarker,
    },
    provider::{
        MAX_PROVIDER_STREAM_CHUNKS, ModelProvider, PreparedProviderCall, PreparedRequestPreflight,
        ProviderBinding, ProviderPreflightError, ProviderPrepareError, ProviderRequest,
        ProviderRequestDraft, ProviderStream, ProviderStreamError, RequestPurpose, StreamValidator,
    },
};

use super::{
    config::{DEEPSEEK_PROVIDER, DeepSeekConfig, DeepSeekReasoningEffort, DeepSeekThinking},
    credentials::{ApiKey, CredentialLookup, CredentialSource, EnvironmentCredentials},
    error::DeepSeekFailure,
    request::{RequestBuildError, preflight_request_len, serialize_request},
    response::{DONE, DeepSeekTranslator, TranslateError},
    sse::{SseDecoder, SseError, SseItem},
    transport::{
        ByteStream, DeepSeekProviderBuildError, HttpRequest, HttpResponse, HttpTransport,
        ReqwestTransport,
    },
};

/// A reusable DeepSeek chat-completions provider.
#[derive(Clone)]
pub struct DeepSeekProvider {
    inner: Arc<DeepSeekInner>,
}

impl std::fmt::Debug for DeepSeekProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeepSeekProvider")
            .field("config", &self.inner.config)
            .field("credentials", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl DeepSeekProvider {
    /// Build the real HTTPS provider with an explicit credential source.
    pub fn new(
        config: DeepSeekConfig,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, DeepSeekProviderBuildError> {
        let transport = Arc::new(ReqwestTransport::new()?);
        Ok(Self::with_transport(config, credentials, transport))
    }

    /// Build the real provider that resolves the configured process variable per request.
    pub fn from_environment(config: DeepSeekConfig) -> Result<Self, DeepSeekProviderBuildError> {
        Self::new(config, Arc::new(EnvironmentCredentials))
    }

    /// Start one lazy model call without requiring the trait to be imported.
    #[must_use]
    pub fn stream(
        &self,
        request: ProviderRequest,
        cancellation: CancellationToken,
    ) -> ProviderStream {
        <Self as ModelProvider>::stream(self, request, cancellation)
    }

    /// Resolve DeepSeek model defaults before the caller logs a request header.
    pub fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        <Self as ModelProvider>::prepare_call(self, config)
    }

    /// Prepare and count one exact DeepSeek wire without credentials or I/O.
    pub fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        <Self as ModelProvider>::preflight_request(self, draft)
    }

    /// Immutable request/connection facts owned by this instance.
    #[must_use]
    pub fn config(&self) -> &DeepSeekConfig {
        &self.inner.config
    }

    pub(super) fn with_transport(
        config: DeepSeekConfig,
        credentials: Arc<dyn CredentialSource>,
        transport: Arc<dyn HttpTransport>,
    ) -> Self {
        Self {
            inner: Arc::new(DeepSeekInner {
                config,
                credentials,
                transport,
                binding: ProviderBinding::new(),
            }),
        }
    }
}

impl ModelProvider for DeepSeekProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        if config.provider() != DEEPSEEK_PROVIDER {
            return Err(ProviderPrepareError::WrongProvider {
                expected: DEEPSEEK_PROVIDER.to_owned(),
                actual: config.provider().to_owned(),
            });
        }

        let requested_effort = config.reasoning_effort().map(|value| value.as_str());
        let effective_effort = match self.inner.config.thinking() {
            Some(DeepSeekThinking::Disabled) => match requested_effort {
                None | Some("off") => DeepSeekReasoningEffort::Off,
                Some(value) => {
                    return Err(ProviderPrepareError::UnsupportedReasoningEffort {
                        value: value.to_owned(),
                    });
                }
            },
            None | Some(DeepSeekThinking::Enabled) => match requested_effort {
                None => self.inner.config.reasoning_effort(),
                Some("off") => DeepSeekReasoningEffort::Off,
                Some("high") => DeepSeekReasoningEffort::High,
                Some("max") => DeepSeekReasoningEffort::Max,
                Some(value) => {
                    return Err(ProviderPrepareError::UnsupportedReasoningEffort {
                        value: value.to_owned(),
                    });
                }
            },
        };
        let model = self.inner.config.model(config.model());
        let max_tokens = match config.max_tokens() {
            Some(value) if value.get() > 0 => value,
            Some(_) => {
                return Err(ProviderPrepareError::Model(
                    crate::model::ModelError::InvalidShape {
                        subject: "call config",
                        detail: "maxTokens must be positive".to_owned(),
                    },
                ));
            }
            None => model
                .and_then(super::config::DeepSeekModelConfig::max_tokens)
                .unwrap_or(
                    NonNegativeSafeInteger::new(self.inner.config.default_max_tokens())
                        .map_err(crate::model::ModelError::from)?,
                ),
        };
        let effective_effort = ReasoningEffortId::new(match effective_effort {
            DeepSeekReasoningEffort::Off => "off",
            DeepSeekReasoningEffort::High => "high",
            DeepSeekReasoningEffort::Max => "max",
        });
        let effective = config.with_materialized_defaults(effective_effort, max_tokens)?;
        let adapter_defaults = LlmCallConfigAdapterDefaults {
            reasoning_effort: config.reasoning_effort().is_none().then_some(TrueMarker),
            max_tokens: config.max_tokens().is_none().then_some(TrueMarker),
        };
        let context_window = Some(
            model
                .and_then(super::config::DeepSeekModelConfig::context_window)
                .unwrap_or(
                    NonNegativeSafeInteger::new(self.inner.config.default_context_window())
                        .map_err(crate::model::ModelError::from)?,
                ),
        );
        Ok(
            PreparedProviderCall::new(effective, adapter_defaults, context_window)
                .with_retry_policy(self.inner.config.retry_policy().clone())
                .bind_to(&self.inner.binding),
        )
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        let prepared = self.prepare_call(draft.config().clone())?;
        let encoded_bytes =
            match preflight_request_len(&self.inner.config, prepared.config(), draft) {
                Ok(encoded_bytes) => encoded_bytes,
                Err(error) => return Err(provider_preflight_error(error, prepared)),
            };
        draft.finish(prepared, encoded_bytes)
    }

    fn stream(&self, request: ProviderRequest, cancellation: CancellationToken) -> ProviderStream {
        let child = cancellation.child_token();
        let cancel_on_drop = child.clone().drop_guard();
        let state = ProviderState {
            inner: Arc::clone(&self.inner),
            phase: Phase::Start(Some(request)),
            pending: VecDeque::new(),
            validator: StreamValidator::default(),
            cancellation: child,
            _cancel_on_drop: cancel_on_drop,
        };
        stream::unfold(state, |mut state| async move {
            state.next_item().await.map(|item| (item, state))
        })
        .boxed()
    }
}

fn provider_preflight_error(
    error: RequestBuildError,
    prepared: PreparedProviderCall,
) -> ProviderPreflightError {
    if matches!(&error, RequestBuildError::TooLarge { .. }) {
        return ProviderPreflightError::WireTooLarge {
            maximum: crate::provider::MAX_PROVIDER_REQUEST_BYTES,
            prepared,
        };
    }
    let code = error.code();
    let failure = LlmFailure::new(error.to_string(), code)
        .expect("bounded DeepSeek encoder errors are valid fixed LLM failures");
    ProviderPreflightError::InvalidRequest { failure, prepared }
}

struct DeepSeekInner {
    config: DeepSeekConfig,
    credentials: Arc<dyn CredentialSource>,
    transport: Arc<dyn HttpTransport>,
    binding: ProviderBinding,
}

struct ProviderState {
    inner: Arc<DeepSeekInner>,
    phase: Phase,
    pending: VecDeque<Result<StreamChunk, ProviderStreamError>>,
    validator: StreamValidator,
    cancellation: CancellationToken,
    _cancel_on_drop: DropGuard,
}

enum Phase {
    Start(Option<ProviderRequest>),
    Reading(ResponseReader),
    Busy,
    Done,
}

struct ResponseReader {
    body: ByteStream,
    decoder: SseDecoder,
    items: VecDeque<SseItem>,
    decoder_failure: Option<SseError>,
    translator: DeepSeekTranslator,
}

impl ProviderState {
    async fn next_item(&mut self) -> Option<Result<StreamChunk, ProviderStreamError>> {
        let mut deadline = None;
        loop {
            if self.cancellation.is_cancelled() && !self.validator.is_finished() {
                self.pending.clear();
                return self.finish_with(DeepSeekFailure::cancelled());
            }
            if let Some(item) = self.pending.pop_front() {
                return Some(match item {
                    Ok(chunk) => match self.validator.accept(&chunk) {
                        Ok(()) => Ok(chunk),
                        Err(error) => {
                            self.phase = Phase::Done;
                            Err(error.into())
                        }
                    },
                    Err(error) => {
                        self.phase = Phase::Done;
                        Err(error)
                    }
                });
            }
            let phase = std::mem::replace(&mut self.phase, Phase::Busy);
            match phase {
                Phase::Start(mut request) => {
                    let request = request.take()?;
                    if !request.preparation().is_bound_to(&self.inner.binding) {
                        return self.finish_with(DeepSeekFailure::new(
                            "prepared DeepSeek call belongs to another provider instance",
                            "INVALID_PREPARED_CALL",
                        ));
                    }
                    if self.cancellation.is_cancelled() {
                        return self.finish_with(DeepSeekFailure::cancelled());
                    }
                    let body = match serialize_request(&self.inner.config, &request) {
                        Ok(body) => body,
                        Err(error) => {
                            if self.cancellation.is_cancelled() {
                                return self.finish_with(DeepSeekFailure::cancelled());
                            }
                            return self.finish_with(DeepSeekFailure::from_request(&error));
                        }
                    };
                    if self.cancellation.is_cancelled() {
                        return self.finish_with(DeepSeekFailure::cancelled());
                    }
                    let lookup = self
                        .inner
                        .credentials
                        .resolve(self.inner.config.credential_ref());
                    if self.cancellation.is_cancelled() {
                        return self.finish_with(DeepSeekFailure::cancelled());
                    }
                    let key = match lookup {
                        CredentialLookup::Missing => {
                            let message = format!(
                                "no DeepSeek API key is available; set {}",
                                self.inner.config.credential_ref().as_str()
                            );
                            return self
                                .finish_with(DeepSeekFailure::new(message, "MISSING_CREDENTIAL"));
                        }
                        CredentialLookup::InvalidEncoding => {
                            return self.finish_with(DeepSeekFailure::new(
                                "the configured DeepSeek API key is not valid Unicode",
                                "INVALID_CREDENTIAL",
                            ));
                        }
                        CredentialLookup::Present(value) => match ApiKey::normalize(value) {
                            Ok(key) => key,
                            Err(_) => {
                                return self.finish_with(DeepSeekFailure::new(
                                    format!(
                                        "{} contains an unusable DeepSeek API key",
                                        self.inner.config.credential_ref().as_str()
                                    ),
                                    "INVALID_CREDENTIAL",
                                ));
                            }
                        },
                    };
                    let first_deadline = Instant::now() + self.inner.config.stream_idle_timeout();
                    deadline = Some(first_deadline);
                    if self.cancellation.is_cancelled() {
                        return self.finish_with(DeepSeekFailure::cancelled());
                    }
                    let mut http = HttpRequest::new(self.inner.config.endpoint().to_owned(), body);
                    http.insert_header("authorization", format!("Bearer {}", key.expose()), true);
                    http.insert_header("content-type", "application/json", false);
                    http.insert_header("accept", "text/event-stream", false);
                    http.insert_header("user-agent", user_agent(), false);
                    if let Some(session_id) = request.session_id() {
                        http.insert_header(
                            "x-deepseek-harness-session-id",
                            session_id.as_str(),
                            true,
                        );
                    }
                    if request.purpose() == RequestPurpose::Compaction {
                        http.insert_header("x-deepseek-harness-compact", "1", false);
                    }
                    let transport = Arc::clone(&self.inner.transport);
                    let send = transport.send(http, self.cancellation.clone());
                    let response = match wait_until(first_deadline, &self.cancellation, send).await
                    {
                        Wait::Ready(Ok(response)) => response,
                        Wait::Ready(Err(_)) => {
                            return self.finish_with(DeepSeekFailure::transport());
                        }
                        Wait::Cancelled => {
                            return self.finish_with(DeepSeekFailure::cancelled());
                        }
                        Wait::TimedOut => return self.finish_with(DeepSeekFailure::timeout()),
                    };
                    if !(200..300).contains(&response.status()) {
                        return self.handle_http_error(response, &key, first_deadline).await;
                    }
                    let mut response = response;
                    let Some(body) = response.take_body() else {
                        return self.finish_with(DeepSeekFailure::new(
                            "DeepSeek API returned no response body",
                            "EMPTY_RESPONSE",
                        ));
                    };
                    self.phase = Phase::Reading(ResponseReader {
                        body,
                        decoder: SseDecoder::default(),
                        items: VecDeque::new(),
                        decoder_failure: None,
                        translator: DeepSeekTranslator::default(),
                    });
                }
                Phase::Reading(mut reader) => {
                    let current_deadline = *deadline.get_or_insert_with(|| {
                        Instant::now() + self.inner.config.stream_idle_timeout()
                    });
                    if let Some(item) = reader.items.pop_front() {
                        match item {
                            SseItem::Comment => {
                                deadline =
                                    Some(Instant::now() + self.inner.config.stream_idle_timeout());
                                self.phase = Phase::Reading(reader);
                                continue;
                            }
                            SseItem::Data(payload) => {
                                let done = payload == DONE;
                                match reader.translator.accept(&payload) {
                                    Ok(chunks) => {
                                        self.phase = if done {
                                            Phase::Done
                                        } else {
                                            Phase::Reading(reader)
                                        };
                                        if let Err(error) = self.enqueue_chunks(chunks) {
                                            self.phase = Phase::Done;
                                            self.pending.push_back(Err(error));
                                        }
                                        continue;
                                    }
                                    Err(error) => {
                                        let failure = failure_from_translate(&error);
                                        self.phase = Phase::Done;
                                        return self.finish_with(failure);
                                    }
                                }
                            }
                        }
                    }
                    if let Some(error) = reader.decoder_failure.take() {
                        self.phase = Phase::Done;
                        return self.finish_with(failure_from_sse(&error));
                    }
                    match wait_until(current_deadline, &self.cancellation, reader.body.next()).await
                    {
                        Wait::Ready(Some(Ok(bytes))) => match reader.decoder.push(&bytes) {
                            Ok(items) => {
                                reader.items.extend(items);
                                self.phase = Phase::Reading(reader);
                            }
                            Err(error) => {
                                reader.items.extend(error.items);
                                reader.decoder_failure = Some(error.error);
                                self.phase = Phase::Reading(reader);
                            }
                        },
                        Wait::Ready(Some(Err(_))) => {
                            self.phase = Phase::Done;
                            return self.finish_with(DeepSeekFailure::transport());
                        }
                        Wait::Ready(None) => {
                            let _ = reader.decoder.finish();
                            self.phase = Phase::Done;
                            return self.finish_with(DeepSeekFailure::new(
                                "SSE stream ended without [DONE]",
                                "STREAM_CLOSED",
                            ));
                        }
                        Wait::Cancelled => {
                            self.phase = Phase::Done;
                            return self.finish_with(DeepSeekFailure::cancelled());
                        }
                        Wait::TimedOut => {
                            self.phase = Phase::Done;
                            return self.finish_with(DeepSeekFailure::timeout());
                        }
                    }
                }
                Phase::Done => return None,
                Phase::Busy => {
                    self.phase = Phase::Done;
                    self.pending.push_back(Err(ProviderStreamError::Protocol(
                        crate::provider::StreamProtocolError::MissingFinish,
                    )));
                }
            }
        }
    }

    async fn handle_http_error(
        &mut self,
        mut response: HttpResponse,
        key: &ApiKey,
        deadline: Instant,
    ) -> Option<Result<StreamChunk, ProviderStreamError>> {
        let status = response.status();
        let retry_after = response.header("retry-after").map(str::to_owned);
        let request_id = response
            .header("x-request-id")
            .or_else(|| response.header("x-deepseek-request-id"))
            .map(str::to_owned);
        let body = match response.take_body() {
            None => Vec::new(),
            Some(body) => match read_error_body(body, deadline, &self.cancellation).await {
                ErrorBody::Complete(body) => body,
                ErrorBody::Cancelled => return self.finish_with(DeepSeekFailure::cancelled()),
                ErrorBody::TimedOut => return self.finish_with(DeepSeekFailure::timeout()),
                ErrorBody::Unreadable | ErrorBody::TooLarge => Vec::new(),
            },
        };
        self.phase = Phase::Done;
        self.finish_with(DeepSeekFailure::http(
            status,
            &body,
            retry_after.as_deref(),
            request_id.as_deref(),
            key,
            SystemTime::now(),
        ))
    }

    fn enqueue_chunks(&mut self, chunks: Vec<StreamChunk>) -> Result<(), ProviderStreamError> {
        let contains_finish = chunks
            .iter()
            .any(|chunk| matches!(chunk.kind(), crate::model::StreamChunkKind::Finish { .. }));
        let exceeds_budget = self
            .validator
            .chunk_count()
            .checked_add(chunks.len())
            .is_none_or(|total| {
                total > MAX_PROVIDER_STREAM_CHUNKS
                    || (total == MAX_PROVIDER_STREAM_CHUNKS && !contains_finish)
            });
        if exceeds_budget {
            let failure = DeepSeekFailure::new(
                "DeepSeek stream produced too many events",
                "RESPONSE_TOO_LARGE",
            );
            let terminal = failure.into_chunk()?;
            let mut planned = self.validator.clone();
            planned.accept(&terminal)?;
            self.pending.push_back(Ok(terminal));
            self.phase = Phase::Done;
            return Ok(());
        }
        let mut planned = self.validator.clone();
        for chunk in &chunks {
            if let Err(error) = planned.accept(chunk) {
                self.pending.push_back(Err(error.into()));
                self.phase = Phase::Done;
                return Ok(());
            }
        }
        for chunk in chunks {
            self.pending.push_back(Ok(chunk));
        }
        Ok(())
    }

    fn finish_with(
        &mut self,
        failure: DeepSeekFailure,
    ) -> Option<Result<StreamChunk, ProviderStreamError>> {
        self.phase = Phase::Done;
        let chunk = match failure.into_chunk() {
            Ok(chunk) => chunk,
            Err(error) => return Some(Err(error.into())),
        };
        match self.validator.accept(&chunk) {
            Ok(()) => Some(Ok(chunk)),
            Err(error) => Some(Err(error.into())),
        }
    }
}

fn failure_from_translate(error: &TranslateError) -> DeepSeekFailure {
    DeepSeekFailure::new(error.to_string(), error.code())
}

fn failure_from_sse(error: &SseError) -> DeepSeekFailure {
    DeepSeekFailure::new(error.to_string(), "RESPONSE_TOO_LARGE")
}

fn user_agent() -> String {
    format!(
        "{}/{} (+{})",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    )
}

const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

enum ErrorBody {
    Complete(Vec<u8>),
    TooLarge,
    Unreadable,
    Cancelled,
    TimedOut,
}

async fn read_error_body(
    mut body: ByteStream,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> ErrorBody {
    let mut output = Vec::new();
    loop {
        match wait_until(deadline, cancellation, body.next()).await {
            Wait::Ready(Some(Ok(bytes))) => {
                let Some(next) = output.len().checked_add(bytes.len()) else {
                    return ErrorBody::TooLarge;
                };
                if next > MAX_ERROR_BODY_BYTES {
                    return ErrorBody::TooLarge;
                }
                output.extend_from_slice(&bytes);
            }
            Wait::Ready(Some(Err(_))) => return ErrorBody::Unreadable,
            Wait::Ready(None) => return ErrorBody::Complete(output),
            Wait::Cancelled => return ErrorBody::Cancelled,
            Wait::TimedOut => return ErrorBody::TimedOut,
        }
    }
}

enum Wait<T> {
    Ready(T),
    Cancelled,
    TimedOut,
}

async fn wait_until<F, T>(deadline: Instant, cancellation: &CancellationToken, future: F) -> Wait<T>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(deadline) => Wait::TimedOut,
        _ = cancellation.cancelled() => Wait::Cancelled,
        value = &mut future => Wait::Ready(value),
    }
}
