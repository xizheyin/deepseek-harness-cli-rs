//! Provider-neutral model call boundary.

mod retry;

pub mod deepseek;

use std::{pin::Pin, sync::Arc};

use futures_util::Stream;
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{
    model::{
        LlmCallConfig, LlmCallConfigAdapterDefaults, Message, ModelError, NonNegativeSafeInteger,
        StreamChunk, ToolSchema,
    },
    session::SessionId,
};

pub use crate::model::{
    MAX_PROVIDER_STREAM_CHUNKS, RequestPurpose, StreamProtocolError, StreamValidator,
};
pub use retry::{
    MAX_RETRY_DELAY_MILLIS, MAX_RETRYABLE_CODE_BYTES, MAX_RETRYABLE_CODES, RetryBackoff, RetryMode,
    RetryPolicy, RetryPolicyError,
};

/// Maximum messages admitted into one provider call.
pub const MAX_PROVIDER_MESSAGES: usize = 4_096;
/// Maximum tools admitted into one provider call.
pub const MAX_PROVIDER_TOOLS: usize = 256;
/// Maximum retained provider-neutral request data before wire serialization.
pub const MAX_PROVIDER_REQUEST_BYTES: usize = 8 * 1024 * 1024;
/// Maximum session identifier length carried in an HTTP header.
pub const MAX_PROVIDER_SESSION_ID_BYTES: usize = 1_024;

/// A bounded provider-neutral request.
#[derive(Eq, PartialEq)]
pub struct ProviderRequest {
    prepared: PreparedProviderCall,
    system: Option<String>,
    messages: Vec<Message>,
    tools: Vec<ToolSchema>,
    purpose: RequestPurpose,
    session_id: Option<SessionId>,
    retained_bytes: usize,
}

impl std::fmt::Debug for ProviderRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRequest")
            .field("provider", &self.config().provider())
            .field("model", &self.config().model())
            .field("system_present", &self.system.is_some())
            .field("message_count", &self.messages.len())
            .field("tool_count", &self.tools.len())
            .field("purpose", &self.purpose)
            .field("session_id_present", &self.session_id.is_some())
            .field("retained_bytes", &self.retained_bytes)
            .finish()
    }
}

impl ProviderRequest {
    /// Construct a request from a prepared call and model-visible history.
    pub fn new(
        prepared: PreparedProviderCall,
        messages: Vec<Message>,
    ) -> Result<Self, ProviderRequestError> {
        if messages.len() > MAX_PROVIDER_MESSAGES {
            return Err(ProviderRequestError::TooManyMessages {
                maximum: MAX_PROVIDER_MESSAGES,
                actual: messages.len(),
            });
        }
        let retained_bytes = add_sizes(
            prepared.config.raw().encoded_len(),
            messages.iter().map(|message| message.raw().encoded_len()),
        )?;
        ensure_request_size(retained_bytes)?;
        Ok(Self {
            prepared,
            system: None,
            messages,
            tools: Vec::new(),
            purpose: RequestPurpose::Conversation,
            session_id: None,
            retained_bytes,
        })
    }

    /// Add or replace the system prompt.
    pub fn with_system(mut self, system: impl Into<String>) -> Result<Self, ProviderRequestError> {
        let system = system.into();
        let previous = self.system.as_ref().map_or(0, String::len);
        self.retained_bytes = self
            .retained_bytes
            .checked_sub(previous)
            .and_then(|value| value.checked_add(system.len()))
            .ok_or(ProviderRequestError::SizeOverflow)?;
        ensure_request_size(self.retained_bytes)?;
        self.system = Some(system);
        Ok(self)
    }

    /// Add or replace the tool schemas exposed to the model.
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Result<Self, ProviderRequestError> {
        if tools.len() > MAX_PROVIDER_TOOLS {
            return Err(ProviderRequestError::TooManyTools {
                maximum: MAX_PROVIDER_TOOLS,
                actual: tools.len(),
            });
        }
        let previous = self
            .tools
            .iter()
            .try_fold(0_usize, |total, tool| {
                total.checked_add(tool.raw().encoded_len())
            })
            .ok_or(ProviderRequestError::SizeOverflow)?;
        let next = tools.iter().try_fold(0_usize, |total, tool| {
            total.checked_add(tool.raw().encoded_len())
        });
        self.retained_bytes = self
            .retained_bytes
            .checked_sub(previous)
            .and_then(|value| value.checked_add(next?))
            .ok_or(ProviderRequestError::SizeOverflow)?;
        ensure_request_size(self.retained_bytes)?;
        self.tools = tools;
        Ok(self)
    }

    /// Mark the purpose used by provider-specific request controls.
    #[must_use]
    pub fn with_purpose(mut self, purpose: RequestPurpose) -> Self {
        self.purpose = purpose;
        self
    }

    /// Add the session routing identifier sent to the provider host.
    pub fn with_session_id(
        mut self,
        session_id: impl Into<SessionId>,
    ) -> Result<Self, ProviderRequestError> {
        let session_id = session_id.into();
        let value = session_id.as_str();
        if value.is_empty()
            || value.len() > MAX_PROVIDER_SESSION_ID_BYTES
            || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(ProviderRequestError::InvalidSessionId);
        }
        self.session_id = Some(session_id);
        Ok(self)
    }

    /// Provider/model and sampling configuration.
    #[must_use]
    pub fn config(&self) -> &LlmCallConfig {
        &self.prepared.config
    }

    /// Adapter-owned defaults and model context frozen before logging.
    #[must_use]
    pub fn preparation(&self) -> &PreparedProviderCall {
        &self.prepared
    }

    /// Optional system prompt prepended by the provider adapter.
    #[must_use]
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    /// Ordered model-visible history.
    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// Ordered tool declarations.
    #[must_use]
    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    /// Purpose-specific provider behavior.
    #[must_use]
    pub fn purpose(&self) -> RequestPurpose {
        self.purpose
    }

    /// Optional host-side routing identity.
    #[must_use]
    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    /// Compact provider-neutral bytes retained by this request.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }
}

/// Effective model facts resolved before a request header is logged.
///
/// Phase 3 will inspect these values, write them to the append-only session,
/// and then move this one-shot value into [`ProviderRequest`].
#[derive(Debug, Eq, PartialEq)]
pub struct PreparedProviderCall {
    config: LlmCallConfig,
    retry_policy: RetryPolicy,
    adapter_defaults: LlmCallConfigAdapterDefaults,
    context_window: Option<NonNegativeSafeInteger>,
    binding: Option<ProviderBinding>,
}

impl PreparedProviderCall {
    /// Build a preparation result for a provider implementation or test fake.
    pub fn new(
        config: LlmCallConfig,
        adapter_defaults: LlmCallConfigAdapterDefaults,
        context_window: Option<NonNegativeSafeInteger>,
    ) -> Self {
        Self {
            config,
            retry_policy: RetryPolicy::default(),
            adapter_defaults,
            context_window,
            binding: None,
        }
    }

    /// Fully materialized configuration that must be logged and dispatched.
    #[must_use]
    pub fn config(&self) -> &LlmCallConfig {
        &self.config
    }

    /// Provider-owned retry facts frozen with this exact call.
    #[must_use]
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }

    /// Which fields were supplied by this adapter rather than the caller.
    #[must_use]
    pub fn adapter_defaults(&self) -> &LlmCallConfigAdapterDefaults {
        &self.adapter_defaults
    }

    /// Provider/model context capacity, when known.
    #[must_use]
    pub fn context_window(&self) -> Option<NonNegativeSafeInteger> {
        self.context_window
    }

    pub(crate) fn bind_to(mut self, binding: &ProviderBinding) -> Self {
        self.binding = Some(binding.clone());
        self
    }

    /// Replace the default policy with the facts owned by this provider.
    ///
    /// This is public so deterministic fake providers and future provider
    /// implementations can satisfy the same preparation contract. It does
    /// not grant the private DeepSeek instance binding required by the real
    /// DeepSeek transport.
    #[must_use]
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    pub(crate) fn is_bound_to(&self, binding: &ProviderBinding) -> bool {
        self.binding
            .as_ref()
            .is_some_and(|candidate| candidate.matches(binding))
    }
}

/// Process-local identity binding preparation and dispatch to one provider instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProviderBinding(Arc<()>);

impl ProviderBinding {
    pub(crate) fn new() -> Self {
        Self(Arc::new(()))
    }

    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

fn add_sizes(
    initial: usize,
    values: impl IntoIterator<Item = usize>,
) -> Result<usize, ProviderRequestError> {
    values
        .into_iter()
        .try_fold(initial, usize::checked_add)
        .ok_or(ProviderRequestError::SizeOverflow)
}

fn ensure_request_size(actual: usize) -> Result<(), ProviderRequestError> {
    if actual > MAX_PROVIDER_REQUEST_BYTES {
        return Err(ProviderRequestError::TooLarge {
            maximum: MAX_PROVIDER_REQUEST_BYTES,
            actual,
        });
    }
    Ok(())
}

/// Invalid provider-neutral request construction.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProviderRequestError {
    /// Too many messages would make later logging and serialization unbounded.
    #[error("provider request has {actual} messages; maximum is {maximum}")]
    TooManyMessages { maximum: usize, actual: usize },
    /// Too many tool schemas were supplied.
    #[error("provider request has {actual} tools; maximum is {maximum}")]
    TooManyTools { maximum: usize, actual: usize },
    /// Aggregate retained data exceeds the request budget.
    #[error("provider request retains {actual} bytes; maximum is {maximum}")]
    TooLarge { maximum: usize, actual: usize },
    /// Aggregate size arithmetic exceeded the platform range.
    #[error("provider request size overflowed")]
    SizeOverflow,
    /// A session ID cannot be put in one safe HTTP header value.
    #[error("session id must be 1 to 1024 printable non-space ASCII bytes")]
    InvalidSessionId,
}

/// A lazy, backpressured stream of provider-neutral model events.
pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<StreamChunk, ProviderStreamError>> + Send + 'static>>;

/// Stable boundary implemented by a real or fake model provider.
///
/// This is a trusted persistence boundary: prepared configuration, defaults,
/// retry facts, and accepted chunks are recorded in the append-only session
/// log. Implementations must never put credentials or private transport
/// diagnostics in those values and must redact failures before publication.
/// The built-in DeepSeek provider enforces this; custom providers are
/// responsible for the same contract.
pub trait ModelProvider: Send + Sync {
    /// Resolve model capabilities and materialize defaults before session
    /// logging. This synchronous method must return promptly; remote discovery
    /// belongs in the lazy stream or a future async preparation API.
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError>;

    /// Begin one one-shot model request. Work starts only when the stream is
    /// polled. Polling must remain non-blocking and dropping/cancelling the
    /// stream must reclaim provider-owned work; detached background requests
    /// are outside this contract.
    fn stream(&self, request: ProviderRequest, cancellation: CancellationToken) -> ProviderStream;
}

/// A provider could not turn a proposed route into a loggable effective call.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProviderPrepareError {
    /// The route belongs to another provider implementation.
    #[error("request was routed to provider {actual:?}; expected {expected:?}")]
    WrongProvider { expected: String, actual: String },
    /// The selected model/deployment cannot honor this effort.
    #[error("provider does not support reasoning effort {value:?}")]
    UnsupportedReasoningEffort { value: String },
    /// A resolved typed model fact could not be represented safely.
    #[error(transparent)]
    Model(#[from] ModelError),
}

/// Failures in the provider-neutral live-stream contract itself.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ProviderStreamError {
    /// A provider emitted an illegal stream sequence.
    #[error(transparent)]
    Protocol(#[from] StreamProtocolError),
    /// A provider could not construct a valid provider-neutral event.
    #[error(transparent)]
    Model(#[from] ModelError),
}
