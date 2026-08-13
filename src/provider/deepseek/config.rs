//! Validated DeepSeek connection facts.

use std::{net::IpAddr, time::Duration};

use thiserror::Error;

use crate::{model::NonNegativeSafeInteger, provider::RetryPolicy};

use super::credentials::CredentialRef;

/// Provider route owned by this adapter.
pub const DEEPSEEK_PROVIDER: &str = "deepseek-official";
/// Public DeepSeek API base.
pub const PUBLIC_BASE_URL: &str = "https://api.deepseek.com";
/// Default credential environment variable.
pub const DEFAULT_API_KEY_ENV: &str = "DEEPSEEK_API_KEY";
/// Optional process-level endpoint environment variable.
pub const BASE_URL_ENV: &str = "DEEPSEEK_BASE_URL";
/// Default maximum output token count.
pub const DEFAULT_MAX_TOKENS: u64 = 256_000;
/// Default combined context capacity.
pub const DEFAULT_CONTEXT_WINDOW: u64 = 1_000_000;
/// Default idle interval while waiting for one next provider item.
pub const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_millis(300_000);
/// Maximum advisory models retained by one provider configuration.
pub const MAX_DEEPSEEK_MODELS: usize = 256;
/// Maximum bytes in one provider-owned model identifier.
pub const MAX_DEEPSEEK_MODEL_ID_BYTES: usize = 256;
/// Maximum bytes in one model display name.
pub const MAX_DEEPSEEK_MODEL_NAME_BYTES: usize = 1_024;
/// Maximum bytes in one model description.
pub const MAX_DEEPSEEK_MODEL_DESCRIPTION_BYTES: usize = 8 * 1_024;
/// Node's maximum reliable timer delay, retained for upstream compatibility.
const MAX_STREAM_IDLE_TIMEOUT_MILLIS: u128 = 2_147_483_647;

/// Deployment-level DeepSeek thinking switch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeepSeekThinking {
    Enabled,
    Disabled,
}

/// DeepSeek-supported reasoning effort.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeepSeekReasoningEffort {
    Off,
    High,
    Max,
}

/// One advisory model plus exact per-model request limits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeepSeekModelConfig {
    id: String,
    name: Option<String>,
    description: Option<String>,
    context_window: Option<NonNegativeSafeInteger>,
    max_tokens: Option<NonNegativeSafeInteger>,
}

impl DeepSeekModelConfig {
    /// Construct an advisory model with adapter-wide limits.
    pub fn new(id: impl Into<String>) -> Result<Self, DeepSeekConfigError> {
        let id = id.into();
        if id.is_empty() || id.len() > MAX_DEEPSEEK_MODEL_ID_BYTES {
            return Err(DeepSeekConfigError::InvalidModelId);
        }
        Ok(Self {
            id,
            name: None,
            description: None,
            context_window: None,
            max_tokens: None,
        })
    }

    /// Set the human-readable discovery name.
    pub fn with_name(mut self, name: impl Into<String>) -> Result<Self, DeepSeekConfigError> {
        let name = name.into();
        if name.is_empty() || name.len() > MAX_DEEPSEEK_MODEL_NAME_BYTES {
            return Err(DeepSeekConfigError::InvalidModelName);
        }
        self.name = Some(name);
        Ok(self)
    }

    /// Set the optional discovery description.
    pub fn with_description(
        mut self,
        description: impl Into<String>,
    ) -> Result<Self, DeepSeekConfigError> {
        let description = description.into();
        if description.len() > MAX_DEEPSEEK_MODEL_DESCRIPTION_BYTES {
            return Err(DeepSeekConfigError::InvalidModelDescription);
        }
        self.description = Some(description);
        Ok(self)
    }

    /// Override the adapter-wide context capacity for this exact ID.
    pub fn with_context_window(mut self, context_window: u64) -> Result<Self, DeepSeekConfigError> {
        self.context_window = Some(positive_safe_integer(
            context_window,
            DeepSeekConfigError::InvalidModelContextWindow,
        )?);
        Ok(self)
    }

    /// Override the adapter-wide output cap for this exact ID.
    pub fn with_max_tokens(mut self, max_tokens: u64) -> Result<Self, DeepSeekConfigError> {
        self.max_tokens = Some(positive_safe_integer(
            max_tokens,
            DeepSeekConfigError::InvalidModelMaxTokens,
        )?);
        Ok(self)
    }

    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    #[must_use]
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    #[must_use]
    pub fn context_window(&self) -> Option<NonNegativeSafeInteger> {
        self.context_window
    }

    #[must_use]
    pub fn max_tokens(&self) -> Option<NonNegativeSafeInteger> {
        self.max_tokens
    }
}

/// Immutable connection and request defaults for one provider instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeepSeekConfig {
    base_url: String,
    endpoint: String,
    credential_ref: CredentialRef,
    thinking: Option<DeepSeekThinking>,
    reasoning_effort: DeepSeekReasoningEffort,
    default_max_tokens: u64,
    default_context_window: u64,
    models: Vec<DeepSeekModelConfig>,
    stream_idle_timeout: Duration,
    retry_policy: RetryPolicy,
}

impl Default for DeepSeekConfig {
    fn default() -> Self {
        Self {
            base_url: PUBLIC_BASE_URL.to_owned(),
            endpoint: format!("{PUBLIC_BASE_URL}/chat/completions"),
            credential_ref: CredentialRef::default_deepseek(),
            thinking: None,
            reasoning_effort: DeepSeekReasoningEffort::High,
            default_max_tokens: DEFAULT_MAX_TOKENS,
            default_context_window: DEFAULT_CONTEXT_WINDOW,
            models: default_models(),
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
            retry_policy: RetryPolicy::default(),
        }
    }
}

impl DeepSeekConfig {
    /// Validate an explicit trusted endpoint and credential reference.
    pub fn new(
        base_url: impl Into<String>,
        credential_ref: CredentialRef,
    ) -> Result<Self, DeepSeekConfigError> {
        let (base_url, endpoint) = validate_base_url(&base_url.into())?;
        Ok(Self {
            base_url,
            endpoint,
            credential_ref,
            ..Self::default()
        })
    }

    /// Read only the process-level endpoint override; this never reads an API key.
    pub fn from_process_environment() -> Result<Self, DeepSeekConfigError> {
        let Some(value) = std::env::var_os(BASE_URL_ENV) else {
            return Ok(Self::default());
        };
        let value = value
            .into_string()
            .map_err(|_| DeepSeekConfigError::EndpointEnvironmentNotUnicode)?;
        Self::new(value, CredentialRef::default_deepseek())
    }

    /// Set deployment thinking policy and its default effort atomically.
    pub fn with_thinking_defaults(
        mut self,
        thinking: Option<DeepSeekThinking>,
        effort: DeepSeekReasoningEffort,
    ) -> Result<Self, DeepSeekConfigError> {
        if thinking == Some(DeepSeekThinking::Disabled) && effort != DeepSeekReasoningEffort::Off {
            return Err(DeepSeekConfigError::ThinkingDisabledWithEffort);
        }
        self.thinking = thinking;
        self.reasoning_effort = effort;
        Ok(self)
    }

    /// Set the adapter-wide output-token default.
    pub fn with_default_max_tokens(mut self, max_tokens: u64) -> Result<Self, DeepSeekConfigError> {
        if max_tokens == 0 || max_tokens > crate::json_value::MAX_SAFE_INTEGER {
            return Err(DeepSeekConfigError::InvalidMaxTokens);
        }
        self.default_max_tokens = max_tokens;
        Ok(self)
    }

    /// Set the fallback context capacity exposed to the Agent.
    pub fn with_default_context_window(
        mut self,
        context_window: u64,
    ) -> Result<Self, DeepSeekConfigError> {
        if context_window == 0 || context_window > crate::json_value::MAX_SAFE_INTEGER {
            return Err(DeepSeekConfigError::InvalidContextWindow);
        }
        self.default_context_window = context_window;
        Ok(self)
    }

    /// Replace the advisory catalog used for exact model limit lookup.
    pub fn with_models(
        mut self,
        models: Vec<DeepSeekModelConfig>,
    ) -> Result<Self, DeepSeekConfigError> {
        if models.len() > MAX_DEEPSEEK_MODELS {
            return Err(DeepSeekConfigError::TooManyModels {
                maximum: MAX_DEEPSEEK_MODELS,
                actual: models.len(),
            });
        }
        let mut ids = std::collections::HashSet::with_capacity(models.len());
        for model in &models {
            if !ids.insert(model.id()) {
                return Err(DeepSeekConfigError::DuplicateModel {
                    id: model.id().to_owned(),
                });
            }
        }
        self.models = models;
        Ok(self)
    }

    /// Replace the provider-owned retry facts frozen by every prepared call.
    #[must_use]
    pub fn with_retry_policy(mut self, retry_policy: RetryPolicy) -> Self {
        self.retry_policy = retry_policy;
        self
    }

    /// Set the per-outstanding-read idle timeout.
    pub fn with_stream_idle_timeout(
        mut self,
        timeout: Duration,
    ) -> Result<Self, DeepSeekConfigError> {
        if timeout.is_zero() || timeout.as_millis() > MAX_STREAM_IDLE_TIMEOUT_MILLIS {
            return Err(DeepSeekConfigError::InvalidIdleTimeout);
        }
        self.stream_idle_timeout = timeout;
        Ok(self)
    }

    /// Trusted endpoint base without a trailing slash.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Complete chat-completions endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Name used to resolve the API key for each request.
    #[must_use]
    pub fn credential_ref(&self) -> &CredentialRef {
        &self.credential_ref
    }

    /// Deployment-level thinking policy.
    #[must_use]
    pub fn thinking(&self) -> Option<DeepSeekThinking> {
        self.thinking
    }

    /// Default per-request effort.
    #[must_use]
    pub fn reasoning_effort(&self) -> DeepSeekReasoningEffort {
        self.reasoning_effort
    }

    /// Default output cap used when the request omits one.
    #[must_use]
    pub fn default_max_tokens(&self) -> u64 {
        self.default_max_tokens
    }

    /// Fallback combined context capacity.
    #[must_use]
    pub fn default_context_window(&self) -> u64 {
        self.default_context_window
    }

    /// Advisory catalog and exact per-model limits.
    #[must_use]
    pub fn models(&self) -> &[DeepSeekModelConfig] {
        &self.models
    }

    /// Exact model entry, if this ID is present in the advisory catalog.
    #[must_use]
    pub fn model(&self, id: &str) -> Option<&DeepSeekModelConfig> {
        self.models.iter().find(|model| model.id() == id)
    }

    /// Maximum idle time for one outstanding provider item.
    #[must_use]
    pub fn stream_idle_timeout(&self) -> Duration {
        self.stream_idle_timeout
    }

    /// Provider-owned retry facts; execution belongs to the Agent Loop.
    #[must_use]
    pub fn retry_policy(&self) -> &RetryPolicy {
        &self.retry_policy
    }
}

fn default_models() -> Vec<DeepSeekModelConfig> {
    [
        ("deepseek-v4-flash", "DeepSeek-V4-Flash"),
        ("deepseek-v4-pro", "DeepSeek-V4-Pro"),
    ]
    .into_iter()
    .map(|(id, name)| {
        DeepSeekModelConfig::new(id)
            .and_then(|model| model.with_name(name))
            .and_then(|model| model.with_context_window(DEFAULT_CONTEXT_WINDOW))
            .expect("fixed DeepSeek model defaults are valid")
    })
    .collect()
}

fn positive_safe_integer(
    value: u64,
    error: DeepSeekConfigError,
) -> Result<NonNegativeSafeInteger, DeepSeekConfigError> {
    if value == 0 {
        return Err(error);
    }
    NonNegativeSafeInteger::new(value).map_err(|_| error)
}

fn validate_base_url(input: &str) -> Result<(String, String), DeepSeekConfigError> {
    let parsed = reqwest::Url::parse(input).map_err(|_| DeepSeekConfigError::InvalidEndpoint)?;
    if parsed.cannot_be_a_base() || parsed.host_str().is_none() {
        return Err(DeepSeekConfigError::InvalidEndpoint);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(DeepSeekConfigError::EndpointUserInfo);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(DeepSeekConfigError::EndpointQueryOrFragment);
    }
    match parsed.scheme() {
        "https" => {}
        "http" if is_loopback(&parsed) => {}
        "http" => return Err(DeepSeekConfigError::RemotePlainHttp),
        _ => return Err(DeepSeekConfigError::UnsupportedEndpointScheme),
    }
    let base_url = parsed.as_str().trim_end_matches('/').to_owned();
    let endpoint = format!("{base_url}/chat/completions");
    Ok((base_url, endpoint))
}

fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let address = host
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || address
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Invalid DeepSeek connection configuration.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DeepSeekConfigError {
    /// The endpoint could not be parsed as a hierarchical URL with a host.
    #[error("DeepSeek endpoint must be an absolute HTTP(S) base URL")]
    InvalidEndpoint,
    /// Credentials embedded in a URL could leak through diagnostics or redirects.
    #[error("DeepSeek endpoint must not contain user information")]
    EndpointUserInfo,
    /// Query/fragment data is not part of a stable API base.
    #[error("DeepSeek endpoint must not contain a query string or fragment")]
    EndpointQueryOrFragment,
    /// Only HTTP transports are meaningful for this provider.
    #[error("DeepSeek endpoint must use HTTPS, or HTTP on loopback for offline testing")]
    UnsupportedEndpointScheme,
    /// A bearer credential must not travel over remote cleartext HTTP.
    #[error("remote DeepSeek endpoints must use HTTPS")]
    RemotePlainHttp,
    /// The process supplied a non-Unicode endpoint value.
    #[error("DEEPSEEK_BASE_URL is not valid Unicode")]
    EndpointEnvironmentNotUnicode,
    /// Disabled deployments can expose only the off effort.
    #[error("only reasoning effort off is valid when DeepSeek thinking is disabled")]
    ThinkingDisabledWithEffort,
    /// Output cap must be a positive JavaScript-safe integer.
    #[error("DeepSeek max tokens must be a positive safe integer")]
    InvalidMaxTokens,
    /// Context capacity must be a positive JavaScript-safe integer.
    #[error("DeepSeek context window must be a positive safe integer")]
    InvalidContextWindow,
    /// Advisory model identifiers are bounded and non-empty.
    #[error("DeepSeek model id must be non-empty and at most 256 bytes")]
    InvalidModelId,
    /// Advisory display names are bounded and non-empty when present.
    #[error("DeepSeek model name must be non-empty and at most 1024 bytes")]
    InvalidModelName,
    /// Advisory descriptions are bounded.
    #[error("DeepSeek model description must be at most 8192 bytes")]
    InvalidModelDescription,
    /// An exact model capacity must be a positive safe integer.
    #[error("DeepSeek model context window must be a positive safe integer")]
    InvalidModelContextWindow,
    /// An exact model output cap must be a positive safe integer.
    #[error("DeepSeek model max tokens must be a positive safe integer")]
    InvalidModelMaxTokens,
    /// Duplicate exact IDs would make preparation ambiguous.
    #[error("DeepSeek model id {id:?} appears more than once")]
    DuplicateModel { id: String },
    /// The advisory catalog is bounded.
    #[error("DeepSeek config has {actual} models; maximum is {maximum}")]
    TooManyModels { maximum: usize, actual: usize },
    /// Timer value is outside the reliable runtime range.
    #[error("DeepSeek stream idle timeout is outside the supported range")]
    InvalidIdleTimeout,
}
