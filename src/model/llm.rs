//! Provider call configuration, failures, usage, and stream chunks.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::json_value::deserialize_present_option;

use super::{
    CallId, ContentBlock, FiniteNumber, JsonValue, ModelError, NonNegativeSafeInteger,
    PositiveFiniteNumber, ProviderRequestId, ReasoningEffortId, TrueMarker, object, optional_json,
    optional_string, optional_typed, required_string, required_typed, shape,
};

/// Serializable provider or transport failure facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LlmFailure {
    message: String,
    code: String,
    status: Option<u16>,
    provider_retry_after_ms: Option<PositiveFiniteNumber>,
    request_id: Option<ProviderRequestId>,
    raw: JsonValue,
}

impl LlmFailure {
    /// Construct the required provider-neutral failure facts.
    pub fn new(message: impl Into<String>, code: impl Into<String>) -> Result<Self, ModelError> {
        Self::from_parts(message.into(), code.into(), None, None, None)
    }

    /// Construct complete validated provider failure facts.
    pub fn from_parts(
        message: String,
        code: String,
        status: Option<u16>,
        provider_retry_after_ms: Option<PositiveFiniteNumber>,
        request_id: Option<ProviderRequestId>,
    ) -> Result<Self, ModelError> {
        if message.is_empty() {
            return Err(ModelError::InvalidFailure("message must not be empty"));
        }
        if code.is_empty() {
            return Err(ModelError::InvalidFailure("code must not be empty"));
        }
        if status.is_some_and(|status| !(100..=599).contains(&status)) {
            return Err(ModelError::InvalidFailure(
                "status must be an integer from 100 through 599",
            ));
        }
        if request_id.as_ref().is_some_and(ProviderRequestId::is_empty) {
            return Err(ModelError::InvalidFailure("requestId must not be empty"));
        }
        let mut value = json!({ "message": message, "code": code });
        if let Some(status) = status {
            value["status"] = Value::from(status);
        }
        if let Some(delay) = provider_retry_after_ms {
            value["providerRetryAfterMs"] = serde_json::to_value(delay)
                .map_err(|error| shape("LLM failure", error.to_string()))?;
        }
        if let Some(request_id) = &request_id {
            value["requestId"] = Value::String(request_id.as_str().to_owned());
        }
        Ok(Self {
            message,
            code,
            status,
            provider_retry_after_ms,
            request_id,
            raw: JsonValue::new(value)?,
        })
    }

    /// Human-readable failure summary.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Stable provider-neutral failure code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Optional HTTP status.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        self.status
    }

    /// Positive provider-requested retry delay in milliseconds.
    #[must_use]
    pub fn provider_retry_after_ms(&self) -> Option<PositiveFiniteNumber> {
        self.provider_retry_after_ms
    }

    /// Opaque provider request identifier for diagnostics.
    #[must_use]
    pub fn request_id(&self) -> Option<&ProviderRequestId> {
        self.request_id.as_ref()
    }

    /// Complete validated failure JSON, including extension fields.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }

    fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let fields = object(raw.as_value(), "LLM failure")?;
        let message = required_string(fields, "message", "LLM failure")?;
        let code = required_string(fields, "code", "LLM failure")?;
        let status = optional_typed::<u16>(fields, "status", "LLM failure")?;
        let provider_retry_after_ms =
            optional_typed::<PositiveFiniteNumber>(fields, "providerRetryAfterMs", "LLM failure")?;
        let request_id =
            optional_string(fields, "requestId", "LLM failure")?.map(ProviderRequestId::new);
        let mut failure =
            Self::from_parts(message, code, status, provider_retry_after_ms, request_id)?;
        failure.raw = raw;
        Ok(failure)
    }
}

impl Serialize for LlmFailure {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LlmFailure {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Parsed stop reason facts, including plugin-defined reason kinds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinishReasonKind {
    Stop,
    ToolCalls,
    MaxTokens,
    Aborted { failure: LlmFailure },
    Error { failure: LlmFailure },
    Other { kind: String },
}

/// Why one provider stream stopped.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinishReason {
    kind: FinishReasonKind,
    raw: JsonValue,
}

impl FinishReason {
    pub fn stop() -> Result<Self, ModelError> {
        Self::from_value(json!({ "kind": "stop" }))
    }

    pub fn tool_calls() -> Result<Self, ModelError> {
        Self::from_value(json!({ "kind": "tool-calls" }))
    }

    pub fn max_tokens() -> Result<Self, ModelError> {
        Self::from_value(json!({ "kind": "max-tokens" }))
    }

    pub fn aborted(failure: LlmFailure) -> Result<Self, ModelError> {
        Self::from_value(json!({ "kind": "aborted", "failure": failure }))
    }

    pub fn error(failure: LlmFailure) -> Result<Self, ModelError> {
        Self::from_value(json!({ "kind": "error", "failure": failure }))
    }

    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let fields = object(raw.as_value(), "finish reason")?;
        let tag = required_string(fields, "kind", "finish reason")?;
        let kind = match tag.as_str() {
            "stop" => FinishReasonKind::Stop,
            "tool-calls" => FinishReasonKind::ToolCalls,
            "max-tokens" => FinishReasonKind::MaxTokens,
            "aborted" => FinishReasonKind::Aborted {
                failure: required_typed(fields, "failure", "finish reason")?,
            },
            "error" => FinishReasonKind::Error {
                failure: required_typed(fields, "failure", "finish reason")?,
            },
            _ => FinishReasonKind::Other { kind: tag },
        };
        Ok(Self { kind, raw })
    }

    #[must_use]
    pub fn kind(&self) -> &FinishReasonKind {
        &self.kind
    }

    /// Complete validated reason JSON, including adapter-defined variants.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }
}

impl Serialize for FinishReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for FinishReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Disjoint token accounting for one model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TokenUsage {
    input_tokens: NonNegativeSafeInteger,
    output_tokens: NonNegativeSafeInteger,
    cache_read_tokens: Option<NonNegativeSafeInteger>,
    cache_write_tokens: Option<NonNegativeSafeInteger>,
    reasoning_tokens: Option<NonNegativeSafeInteger>,
    raw: JsonValue,
}

impl TokenUsage {
    /// Construct the two required disjoint token counts.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Result<Self, ModelError> {
        Self::from_parts(
            NonNegativeSafeInteger::new(input_tokens)?,
            NonNegativeSafeInteger::new(output_tokens)?,
            None,
            None,
            None,
        )
    }

    /// Construct all optional disjoint token counts.
    pub fn from_parts(
        input_tokens: NonNegativeSafeInteger,
        output_tokens: NonNegativeSafeInteger,
        cache_read_tokens: Option<NonNegativeSafeInteger>,
        cache_write_tokens: Option<NonNegativeSafeInteger>,
        reasoning_tokens: Option<NonNegativeSafeInteger>,
    ) -> Result<Self, ModelError> {
        let mut value = json!({
            "inputTokens": input_tokens,
            "outputTokens": output_tokens,
        });
        for (key, item) in [
            ("cacheReadTokens", cache_read_tokens),
            ("cacheWriteTokens", cache_write_tokens),
            ("reasoningTokens", reasoning_tokens),
        ] {
            if let Some(item) = item {
                value[key] = Value::from(item.get());
            }
        }
        Ok(Self {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_write_tokens,
            reasoning_tokens,
            raw: JsonValue::new(value)?,
        })
    }

    /// Uncached input tokens.
    #[must_use]
    pub fn input_tokens(&self) -> NonNegativeSafeInteger {
        self.input_tokens
    }

    /// Output tokens.
    #[must_use]
    pub fn output_tokens(&self) -> NonNegativeSafeInteger {
        self.output_tokens
    }

    /// Tokens served from a cache, when reported separately.
    #[must_use]
    pub fn cache_read_tokens(&self) -> Option<NonNegativeSafeInteger> {
        self.cache_read_tokens
    }

    /// Tokens written to a cache, when reported separately.
    #[must_use]
    pub fn cache_write_tokens(&self) -> Option<NonNegativeSafeInteger> {
        self.cache_write_tokens
    }

    /// Provider-reported reasoning tokens, when available.
    #[must_use]
    pub fn reasoning_tokens(&self) -> Option<NonNegativeSafeInteger> {
        self.reasoning_tokens
    }

    /// Complete validated usage JSON, including extension fields.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }

    fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let fields = object(raw.as_value(), "token usage")?;
        let mut usage = Self::from_parts(
            required_typed(fields, "inputTokens", "token usage")?,
            required_typed(fields, "outputTokens", "token usage")?,
            optional_typed(fields, "cacheReadTokens", "token usage")?,
            optional_typed(fields, "cacheWriteTokens", "token usage")?,
            optional_typed(fields, "reasoningTokens", "token usage")?,
        )?;
        usage.raw = raw;
        Ok(usage)
    }
}

impl Serialize for TokenUsage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for TokenUsage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// The content vocabulary announced by a stream block boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentBlockType {
    Text,
    Reasoning,
    Image,
    ToolCall,
    ToolResult,
    Other(String),
}

impl Serialize for ContentBlockType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match self {
            Self::Text => "text",
            Self::Reasoning => "reasoning",
            Self::Image => "image",
            Self::ToolCall => "tool-call",
            Self::ToolResult => "tool-result",
            Self::Other(value) => value,
        })
    }
}

impl<'de> Deserialize<'de> for ContentBlockType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(match value.as_str() {
            "text" => Self::Text,
            "reasoning" => Self::Reasoning,
            "image" => Self::Image,
            "tool-call" => Self::ToolCall,
            "tool-result" => Self::ToolResult,
            _ => Self::Other(value),
        })
    }
}

/// Parsed stream facts. Unknown chunk kinds remain replayable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StreamChunkKind {
    BlockStart {
        index: NonNegativeSafeInteger,
        block_type: ContentBlockType,
    },
    TextDelta {
        index: NonNegativeSafeInteger,
        text: String,
    },
    ReasoningDelta {
        index: NonNegativeSafeInteger,
        text: String,
    },
    ToolCallDelta {
        index: NonNegativeSafeInteger,
        id: CallId,
        name: Option<String>,
        arguments_delta: String,
    },
    BlockEnd {
        index: NonNegativeSafeInteger,
        block: ContentBlock,
    },
    Usage {
        usage: TokenUsage,
    },
    Finish {
        reason: FinishReason,
        replay_state: Option<JsonValue>,
    },
    Other {
        chunk_type: String,
    },
}

/// Provider-neutral streaming event emitted by an adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamChunk {
    kind: StreamChunkKind,
    raw: JsonValue,
}

impl StreamChunk {
    pub fn block_start(index: u64, block_type: ContentBlockType) -> Result<Self, ModelError> {
        Self::from_value(json!({
            "type": "block-start",
            "index": NonNegativeSafeInteger::new(index)?,
            "blockType": block_type,
        }))
    }

    pub fn text_delta(index: u64, text: impl Into<String>) -> Result<Self, ModelError> {
        Self::from_value(json!({
            "type": "text-delta",
            "index": NonNegativeSafeInteger::new(index)?,
            "text": text.into(),
        }))
    }

    pub fn reasoning_delta(index: u64, text: impl Into<String>) -> Result<Self, ModelError> {
        Self::from_value(json!({
            "type": "reasoning-delta",
            "index": NonNegativeSafeInteger::new(index)?,
            "text": text.into(),
        }))
    }

    pub fn tool_call_delta(
        index: u64,
        id: impl Into<CallId>,
        name: Option<String>,
        arguments_delta: impl Into<String>,
    ) -> Result<Self, ModelError> {
        let mut value = json!({
            "type": "tool-call-delta",
            "index": NonNegativeSafeInteger::new(index)?,
            "id": id.into(),
            "argumentsDelta": arguments_delta.into(),
        });
        if let Some(name) = name {
            value["name"] = Value::String(name);
        }
        Self::from_value(value)
    }

    pub fn block_end(index: u64, block: ContentBlock) -> Result<Self, ModelError> {
        Self::from_value(json!({
            "type": "block-end",
            "index": NonNegativeSafeInteger::new(index)?,
            "block": block,
        }))
    }

    pub fn usage(usage: TokenUsage) -> Result<Self, ModelError> {
        Self::from_value(json!({ "type": "usage", "usage": usage }))
    }

    pub fn finish(
        reason: FinishReason,
        replay_state: Option<JsonValue>,
    ) -> Result<Self, ModelError> {
        let mut value = json!({ "type": "finish", "reason": reason });
        if let Some(replay_state) = replay_state {
            value["replayState"] = replay_state.into_value();
        }
        Self::from_value(value)
    }

    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let fields = object(raw.as_value(), "stream chunk")?;
        let chunk_type = required_string(fields, "type", "stream chunk")?;
        let kind = match chunk_type.as_str() {
            "block-start" => StreamChunkKind::BlockStart {
                index: required_typed(fields, "index", "stream chunk")?,
                block_type: required_typed(fields, "blockType", "stream chunk")?,
            },
            "text-delta" => StreamChunkKind::TextDelta {
                index: required_typed(fields, "index", "stream chunk")?,
                text: required_string(fields, "text", "stream chunk")?,
            },
            "reasoning-delta" => StreamChunkKind::ReasoningDelta {
                index: required_typed(fields, "index", "stream chunk")?,
                text: required_string(fields, "text", "stream chunk")?,
            },
            "tool-call-delta" => StreamChunkKind::ToolCallDelta {
                index: required_typed(fields, "index", "stream chunk")?,
                id: CallId::new(required_string(fields, "id", "stream chunk")?),
                name: optional_string(fields, "name", "stream chunk")?,
                arguments_delta: required_string(fields, "argumentsDelta", "stream chunk")?,
            },
            "block-end" => StreamChunkKind::BlockEnd {
                index: required_typed(fields, "index", "stream chunk")?,
                block: required_typed(fields, "block", "stream chunk")?,
            },
            "usage" => StreamChunkKind::Usage {
                usage: required_typed(fields, "usage", "stream chunk")?,
            },
            "finish" => StreamChunkKind::Finish {
                reason: required_typed(fields, "reason", "stream chunk")?,
                replay_state: optional_json(fields, "replayState")?,
            },
            _ => StreamChunkKind::Other { chunk_type },
        };
        Ok(Self { kind, raw })
    }

    #[must_use]
    pub fn kind(&self) -> &StreamChunkKind {
        &self.kind
    }

    /// Complete validated chunk JSON, including adapter-defined fields.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }

    /// Whether this chunk contains non-empty model output.
    #[must_use]
    pub fn is_token_delta(&self) -> bool {
        match &self.kind {
            StreamChunkKind::TextDelta { text, .. }
            | StreamChunkKind::ReasoningDelta { text, .. } => !text.is_empty(),
            StreamChunkKind::ToolCallDelta {
                name,
                arguments_delta,
                ..
            } => name.is_some() || !arguments_delta.is_empty(),
            _ => false,
        }
    }
}

impl Serialize for StreamChunk {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for StreamChunk {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// JSON-schema description of a tool sent to a model.
#[derive(Clone)]
pub struct ToolSchema {
    inner: Arc<ToolSchemaInner>,
}

#[derive(Debug, Eq, PartialEq)]
struct ToolSchemaInner {
    name: String,
    description: String,
    parameters: JsonValue,
    raw: JsonValue,
}

impl std::fmt::Debug for ToolSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl PartialEq for ToolSchema {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for ToolSchema {}

impl ToolSchema {
    /// Construct one bounded JSON-schema tool declaration.
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: JsonValue,
    ) -> Result<Self, ModelError> {
        let name = name.into();
        let description = description.into();
        if !parameters.as_value().is_object() {
            return Err(shape("tool schema", "parameters must be an object"));
        }
        let raw = JsonValue::new(json!({
            "name": name,
            "description": description,
            "parameters": parameters,
        }))?;
        Ok(Self::from_parts(name, description, parameters, raw))
    }

    /// Tool name sent to the model.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Human-readable tool description sent to the model.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.inner.description
    }

    /// JSON Schema object for the tool arguments.
    #[must_use]
    pub fn parameters(&self) -> &JsonValue {
        &self.inner.parameters
    }

    /// Complete validated schema JSON, including extension fields.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.inner.raw
    }

    fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let fields = object(raw.as_value(), "tool schema")?;
        let name = required_string(fields, "name", "tool schema")?;
        let description = required_string(fields, "description", "tool schema")?;
        let parameters = JsonValue::new(
            fields
                .get("parameters")
                .cloned()
                .ok_or_else(|| shape("tool schema", "missing parameters"))?,
        )?;
        if !parameters.as_value().is_object() {
            return Err(shape("tool schema", "parameters must be an object"));
        }
        Ok(Self::from_parts(name, description, parameters, raw))
    }

    fn from_parts(
        name: String,
        description: String,
        parameters: JsonValue,
        raw: JsonValue,
    ) -> Self {
        Self {
            inner: Arc::new(ToolSchemaInner {
                name,
                description,
                parameters,
                raw,
            }),
        }
    }
}

impl Serialize for ToolSchema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.inner.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ToolSchema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Provider-neutral call configuration recorded in a request header.
#[derive(Clone)]
pub struct LlmCallConfig {
    inner: Arc<LlmCallConfigInner>,
}

#[derive(Debug, Eq, PartialEq)]
struct LlmCallConfigInner {
    provider: String,
    model: String,
    reasoning_effort: Option<ReasoningEffortId>,
    temperature: Option<FiniteNumber>,
    max_tokens: Option<NonNegativeSafeInteger>,
    stop: Option<Vec<String>>,
    raw: JsonValue,
}

impl std::fmt::Debug for LlmCallConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl PartialEq for LlmCallConfig {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for LlmCallConfig {}

impl LlmCallConfig {
    /// Construct the required provider/model route.
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Result<Self, ModelError> {
        Self::from_parts(provider.into(), model.into(), None, None, None, None)
    }

    /// Construct all currently known call fields.
    pub fn from_parts(
        provider: String,
        model: String,
        reasoning_effort: Option<ReasoningEffortId>,
        temperature: Option<FiniteNumber>,
        max_tokens: Option<NonNegativeSafeInteger>,
        stop: Option<Vec<String>>,
    ) -> Result<Self, ModelError> {
        if provider.is_empty() {
            return Err(ModelError::EmptyProvider);
        }
        if model.is_empty() {
            return Err(ModelError::EmptyModel);
        }
        if reasoning_effort
            .as_ref()
            .is_some_and(ReasoningEffortId::is_empty)
        {
            return Err(ModelError::EmptyOptionalId {
                field: "reasoningEffort",
            });
        }
        let mut value = json!({ "provider": provider, "model": model });
        if let Some(reasoning_effort) = &reasoning_effort {
            value["reasoningEffort"] = Value::String(reasoning_effort.as_str().to_owned());
        }
        if let Some(temperature) = temperature {
            value["temperature"] = serde_json::to_value(temperature)
                .map_err(|error| shape("call config", error.to_string()))?;
        }
        if let Some(max_tokens) = max_tokens {
            value["maxTokens"] = Value::from(max_tokens.get());
        }
        if let Some(stop) = &stop {
            value["stop"] = serde_json::to_value(stop)
                .map_err(|error| shape("call config", error.to_string()))?;
        }
        Ok(Self::from_parsed_parts(
            provider,
            model,
            reasoning_effort,
            temperature,
            max_tokens,
            stop,
            JsonValue::new(value)?,
        ))
    }

    /// Registered provider route.
    #[must_use]
    pub fn provider(&self) -> &str {
        &self.inner.provider
    }

    /// Provider-owned model identifier.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.inner.model
    }

    /// Adapter-owned reasoning effort, when selected.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&ReasoningEffortId> {
        self.inner.reasoning_effort.as_ref()
    }

    /// Maximum output tokens, when selected.
    #[must_use]
    pub fn max_tokens(&self) -> Option<NonNegativeSafeInteger> {
        self.inner.max_tokens
    }

    /// Sampling temperature, when explicitly configured.
    #[must_use]
    pub fn temperature(&self) -> Option<FiniteNumber> {
        self.inner.temperature
    }

    /// Ordered stop strings, when explicitly configured.
    #[must_use]
    pub fn stop(&self) -> Option<&[String]> {
        self.inner.stop.as_deref()
    }

    /// Complete validated config JSON, including extension fields.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.inner.raw
    }

    /// Upstream field-wise equality used to decide whether a new request
    /// header represents a real config change. Extension JSON is ignored.
    #[must_use]
    pub fn equivalent_to(&self, other: &Self) -> bool {
        self.inner.provider == other.inner.provider
            && self.inner.model == other.inner.model
            && self.inner.reasoning_effort == other.inner.reasoning_effort
            && self.inner.temperature == other.inner.temperature
            && self.inner.max_tokens == other.inner.max_tokens
            && self.inner.stop == other.inner.stop
    }

    /// Preserve extension fields while materializing provider-owned defaults.
    pub(crate) fn with_materialized_defaults(
        &self,
        reasoning_effort: ReasoningEffortId,
        max_tokens: NonNegativeSafeInteger,
    ) -> Result<Self, ModelError> {
        let mut value = self.inner.raw.as_value().clone();
        let fields = value
            .as_object_mut()
            .ok_or_else(|| shape("call config", "must be a JSON object"))?;
        fields.insert(
            "reasoningEffort".to_owned(),
            Value::String(reasoning_effort.as_str().to_owned()),
        );
        fields.insert("maxTokens".to_owned(), Value::from(max_tokens.get()));
        Self::from_value(value)
    }

    /// Remove fields previously supplied by an adapter while preserving extensions.
    pub(crate) fn without_adapter_defaults(
        &self,
        defaults: &LlmCallConfigAdapterDefaults,
    ) -> Result<Self, ModelError> {
        let mut value = self.inner.raw.as_value().clone();
        let fields = value
            .as_object_mut()
            .ok_or_else(|| shape("call config", "must be a JSON object"))?;
        if defaults.reasoning_effort.is_some() {
            fields.remove("reasoningEffort");
        }
        if defaults.max_tokens.is_some() {
            fields.remove("maxTokens");
        }
        Self::from_value(value)
    }

    /// Restore an explicit prior effort only when the new proposal omitted it.
    pub(crate) fn with_reasoning_effort_if_absent(
        &self,
        effort: Option<&ReasoningEffortId>,
    ) -> Result<Self, ModelError> {
        let Some(effort) = effort else {
            return Ok(self.clone());
        };
        if self.inner.reasoning_effort.is_some() {
            return Ok(self.clone());
        }
        let mut value = self.inner.raw.as_value().clone();
        value
            .as_object_mut()
            .ok_or_else(|| shape("call config", "must be a JSON object"))?
            .insert(
                "reasoningEffort".to_owned(),
                Value::String(effort.as_str().to_owned()),
            );
        Self::from_value(value)
    }

    pub(crate) fn validate(&self) -> Result<(), ModelError> {
        if self.inner.provider.is_empty() {
            return Err(ModelError::EmptyProvider);
        }
        if self.inner.model.is_empty() {
            return Err(ModelError::EmptyModel);
        }
        if self
            .inner
            .reasoning_effort
            .as_ref()
            .is_some_and(ReasoningEffortId::is_empty)
        {
            return Err(ModelError::EmptyOptionalId {
                field: "reasoningEffort",
            });
        }
        Ok(())
    }

    fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let fields = object(raw.as_value(), "call config")?;
        let provider = required_string(fields, "provider", "call config")?;
        let model = required_string(fields, "model", "call config")?;
        let reasoning_effort =
            optional_string(fields, "reasoningEffort", "call config")?.map(ReasoningEffortId::new);
        let temperature = optional_typed(fields, "temperature", "call config")?;
        let max_tokens = optional_typed(fields, "maxTokens", "call config")?;
        let stop = optional_typed(fields, "stop", "call config")?;
        Ok(Self::from_parsed_parts(
            provider,
            model,
            reasoning_effort,
            temperature,
            max_tokens,
            stop,
            raw,
        ))
    }

    fn from_parsed_parts(
        provider: String,
        model: String,
        reasoning_effort: Option<ReasoningEffortId>,
        temperature: Option<FiniteNumber>,
        max_tokens: Option<NonNegativeSafeInteger>,
        stop: Option<Vec<String>>,
        raw: JsonValue,
    ) -> Self {
        Self {
            inner: Arc::new(LlmCallConfigInner {
                provider,
                model,
                reasoning_effort,
                temperature,
                max_tokens,
                stop,
                raw,
            }),
        }
    }
}

impl Serialize for LlmCallConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.inner.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for LlmCallConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Which call-config values were supplied by adapter defaults.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmCallConfigAdapterDefaults {
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_effort: Option<TrueMarker>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_tokens: Option<TrueMarker>,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::{JsonValue, LlmCallConfig, ToolSchema};

    #[test]
    fn provider_request_prefix_values_clone_shallowly() {
        let schema = ToolSchema::new(
            "read",
            "read one file",
            JsonValue::new(json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }))
            .unwrap(),
        )
        .unwrap();
        let schema_clone = schema.clone();
        assert!(Arc::ptr_eq(&schema.inner, &schema_clone.inner));
        assert_eq!(schema, schema_clone);

        let config = LlmCallConfig::new("mock", "mock-model").unwrap();
        let config_clone = config.clone();
        assert!(Arc::ptr_eq(&config.inner, &config_clone.inner));
        assert_eq!(config, config_clone);
        assert_eq!(
            serde_json::to_value(&config).unwrap(),
            serde_json::to_value(&config_clone).unwrap()
        );
    }
}
