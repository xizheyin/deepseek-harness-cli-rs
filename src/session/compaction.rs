//! Durable, provider-neutral compaction event vocabulary.

use std::{collections::BTreeSet, fmt};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{
    json_value::deserialize_present_option,
    model::{
        ContentBlock, ContentBlockKind, FiniteNumber, LlmCallConfig, LlmCallConfigAdapterDefaults,
        LlmFailure, MAX_MESSAGE_CONTENT_BLOCKS, Message, MessageRole, MessageSourceKind,
        NonNegativeSafeInteger, PositiveFiniteNumber, RequestPurpose, TokenUsage, ToolSchema,
        TrueMarker,
    },
};

use super::{EventSeq, SessionId, TurnId, error::EventValidationError};

pub(crate) const MAX_COMPACTION_SHADOWED_SEQS: usize = super::MAX_SOURCE_EVENT_SEQS - 2;
const MAX_COMPACTION_ID_BYTES: usize = 1_024;
const MAX_COMPACTION_SOURCE_COMMAND_ID_BYTES: usize = 1_024;
const MAX_COMPACTION_ROUTE_BYTES: usize = 1_024;
const MAX_COMPACTION_INSTRUCTION_ID_BYTES: usize = 1_024;
const MAX_COMPACTION_SYSTEM_BYTES: usize = 4 * 1024 * 1024;
const MAX_COMPACTION_TOOLS: usize = 256;
const MAX_COMPACTION_RETRY_CODES: usize = 256;
const MAX_COMPACTION_RETRY_CODE_BYTES: usize = 256;
const MAX_COMPACTION_RETRY_DELAY_MILLIS: f64 = 2_147_483_647.0;
const MAX_COMPACTION_FAILURE_CODE_BYTES: usize = 256;
const MAX_COMPACTION_FAILURE_MESSAGE_BYTES: usize = 32 * 1024;
const MAX_COMPACTION_FAILURE_REQUEST_ID_BYTES: usize = 1_024;
const MAX_COMPACTION_FAILURE_JSON_BYTES: usize = 48 * 1024;
pub(crate) const COMPACTION_INSTRUCTION_FORMAT_VERSION: u64 = 1;
pub(crate) const COMPACTION_INSTRUCTION_SOURCE: &str = "dsh.compaction";
pub(crate) const COMPACTION_CHECKPOINT_SOURCE: &str = "compact";
pub(crate) const COMPACTION_CHECKPOINT_PREFIX: &str = "This is an automatically generated checkpoint condensing an earlier span of the conversation to free up context. Treat the captured context as established background and build on it without restating it. Continue the task directly from the messages that follow, without acknowledging this checkpoint.\n\n<compacted-summary>";
pub(crate) const COMPACTION_CHECKPOINT_SUFFIX: &str = "</compacted-summary>";

/// Stable identity shared by one compaction bracket.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CompactionId(String);

impl CompactionId {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        validate_bounded_identity(
            &self.0,
            MAX_COMPACTION_ID_BYTES,
            "compactionId must be 1 to 1024 non-control UTF-8 bytes",
        )
    }
}

impl fmt::Display for CompactionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Why the Session started one automatic compaction.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompactionTrigger {
    Pressure,
    ContextOverflow,
    HardLimit,
}

/// Inclusive endpoints in current-surface order.
///
/// `start` may be numerically greater than `end` after an earlier replacement;
/// Projection validates their positions instead of comparing the integers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompactionRange {
    start: EventSeq,
    end: EventSeq,
}

impl CompactionRange {
    #[must_use]
    pub fn new(start: EventSeq, end: EventSeq) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub fn start(self) -> EventSeq {
        self.start
    }

    #[must_use]
    pub fn end(self) -> EventSeq {
        self.end
    }
}

/// Serializable retry mode frozen before a compaction request is dispatched.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PreparedRetryModeSnapshot {
    Normal,
    Always,
}

/// Serializable exponential-backoff facts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedRetryBackoffSnapshot {
    initial_delay_ms: PositiveFiniteNumber,
    max_delay_ms: PositiveFiniteNumber,
    jitter_ratio: FiniteNumber,
}

impl PreparedRetryBackoffSnapshot {
    pub fn new(
        initial_delay_ms: PositiveFiniteNumber,
        max_delay_ms: PositiveFiniteNumber,
        jitter_ratio: FiniteNumber,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            initial_delay_ms,
            max_delay_ms,
            jitter_ratio,
        };
        value.validate()?;
        Ok(value)
    }

    #[must_use]
    pub fn initial_delay_ms(&self) -> PositiveFiniteNumber {
        self.initial_delay_ms
    }

    #[must_use]
    pub fn max_delay_ms(&self) -> PositiveFiniteNumber {
        self.max_delay_ms
    }

    #[must_use]
    pub fn jitter_ratio(&self) -> FiniteNumber {
        self.jitter_ratio
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        if self.initial_delay_ms.get() > MAX_COMPACTION_RETRY_DELAY_MILLIS
            || self.max_delay_ms.get() > MAX_COMPACTION_RETRY_DELAY_MILLIS
            || self.initial_delay_ms > self.max_delay_ms
            || !(0.0..=1.0).contains(&self.jitter_ratio.get())
        {
            return Err(invalid_compaction("retry backoff is invalid"));
        }
        Ok(())
    }
}

/// Complete bounded retry facts retained without importing provider types.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedRetryPolicySnapshot {
    mode: PreparedRetryModeSnapshot,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    max_retries: Option<NonNegativeSafeInteger>,
    retryable_codes: Vec<String>,
    backoff: PreparedRetryBackoffSnapshot,
}

impl PreparedRetryPolicySnapshot {
    pub fn normal(
        max_retries: NonNegativeSafeInteger,
        retryable_codes: Vec<String>,
        backoff: PreparedRetryBackoffSnapshot,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            mode: PreparedRetryModeSnapshot::Normal,
            max_retries: Some(max_retries),
            retryable_codes,
            backoff,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn always(backoff: PreparedRetryBackoffSnapshot) -> Result<Self, EventValidationError> {
        let value = Self {
            mode: PreparedRetryModeSnapshot::Always,
            max_retries: None,
            retryable_codes: Vec::new(),
            backoff,
        };
        value.validate()?;
        Ok(value)
    }

    #[must_use]
    pub fn mode(&self) -> PreparedRetryModeSnapshot {
        self.mode
    }

    #[must_use]
    pub fn max_retries(&self) -> Option<NonNegativeSafeInteger> {
        self.max_retries
    }

    #[must_use]
    pub fn retryable_codes(&self) -> &[String] {
        &self.retryable_codes
    }

    #[must_use]
    pub fn backoff(&self) -> &PreparedRetryBackoffSnapshot {
        &self.backoff
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        self.backoff.validate()?;
        if self.retryable_codes.len() > MAX_COMPACTION_RETRY_CODES {
            return Err(invalid_compaction("retry policy has too many codes"));
        }
        let mut seen = BTreeSet::new();
        for code in &self.retryable_codes {
            validate_bounded_identity(
                code,
                MAX_COMPACTION_RETRY_CODE_BYTES,
                "retry code must be 1 to 256 non-control UTF-8 bytes",
            )?;
            if !seen.insert(code.as_str()) {
                return Err(invalid_compaction("retry policy contains a duplicate code"));
            }
        }
        match self.mode {
            PreparedRetryModeSnapshot::Normal
                if self.max_retries.is_none() || self.retryable_codes.is_empty() =>
            {
                Err(invalid_compaction(
                    "normal retry policy requires maxRetries and retryableCodes",
                ))
            }
            PreparedRetryModeSnapshot::Always
                if self.max_retries.is_some() || !self.retryable_codes.is_empty() =>
            {
                Err(invalid_compaction(
                    "always retry policy must omit maxRetries and retryableCodes",
                ))
            }
            PreparedRetryModeSnapshot::Normal | PreparedRetryModeSnapshot::Always => Ok(()),
        }
    }
}

/// Effective provider preparation recorded without its process-local binding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PreparedCompactionCallSnapshot {
    config: LlmCallConfig,
    adapter_defaults: LlmCallConfigAdapterDefaults,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    context_window: Option<NonNegativeSafeInteger>,
    retry_policy: PreparedRetryPolicySnapshot,
}

impl PreparedCompactionCallSnapshot {
    pub fn new(
        config: LlmCallConfig,
        adapter_defaults: LlmCallConfigAdapterDefaults,
        context_window: Option<NonNegativeSafeInteger>,
        retry_policy: PreparedRetryPolicySnapshot,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            config,
            adapter_defaults,
            context_window,
            retry_policy,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        self.config.validate()?;
        validate_route(self.config.provider(), "prepared call provider is invalid")?;
        validate_route(self.config.model(), "prepared call model is invalid")?;
        if self.adapter_defaults.reasoning_effort.is_some()
            && self.config.reasoning_effort().is_none()
        {
            return Err(crate::model::ModelError::InvalidAdapterDefault {
                field: "reasoningEffort",
            }
            .into());
        }
        if self.adapter_defaults.max_tokens.is_some() && self.config.max_tokens().is_none() {
            return Err(
                crate::model::ModelError::InvalidAdapterDefault { field: "maxTokens" }.into(),
            );
        }
        if self
            .config
            .max_tokens()
            .is_some_and(|maximum| maximum.get() == 0)
        {
            return Err(invalid_compaction(
                "prepared compaction maxTokens must be positive when present",
            ));
        }
        self.retry_policy.validate()
    }

    #[must_use]
    pub fn config(&self) -> &LlmCallConfig {
        &self.config
    }

    #[must_use]
    pub fn adapter_defaults(&self) -> &LlmCallConfigAdapterDefaults {
        &self.adapter_defaults
    }

    #[must_use]
    pub fn context_window(&self) -> Option<NonNegativeSafeInteger> {
        self.context_window
    }

    #[must_use]
    pub fn retry_policy(&self) -> &PreparedRetryPolicySnapshot {
        &self.retry_policy
    }
}

/// Exact provider-visible request facts durably captured before dispatch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelVisibleDispatchSnapshot {
    trigger: CompactionTrigger,
    source_surface_generation: NonNegativeSafeInteger,
    shadowed_range: CompactionRange,
    shadowed_seqs: Vec<EventSeq>,
    prepared_call: PreparedCompactionCallSnapshot,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    request_header_seq: Option<EventSeq>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    request_context_seq: Option<EventSeq>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    system: Option<String>,
    tools: Vec<ToolSchema>,
    session_id: SessionId,
    purpose: RequestPurpose,
    instruction: Message,
    instruction_format_version: NonNegativeSafeInteger,
}

/// Named construction input for one complete pre-dispatch compaction recipe.
pub struct ModelVisibleDispatchInput {
    pub trigger: CompactionTrigger,
    pub source_surface_generation: NonNegativeSafeInteger,
    pub shadowed_range: CompactionRange,
    pub shadowed_seqs: Vec<EventSeq>,
    pub prepared_call: PreparedCompactionCallSnapshot,
    pub request_header_seq: Option<EventSeq>,
    pub request_context_seq: Option<EventSeq>,
    pub system: Option<String>,
    pub tools: Vec<ToolSchema>,
    pub session_id: SessionId,
    pub instruction: Message,
}

impl ModelVisibleDispatchSnapshot {
    pub fn new(input: ModelVisibleDispatchInput) -> Result<Self, EventValidationError> {
        let value = Self {
            trigger: input.trigger,
            source_surface_generation: input.source_surface_generation,
            shadowed_range: input.shadowed_range,
            shadowed_seqs: input.shadowed_seqs,
            prepared_call: input.prepared_call,
            request_header_seq: input.request_header_seq,
            request_context_seq: input.request_context_seq,
            system: input.system,
            tools: input.tools,
            session_id: input.session_id,
            purpose: RequestPurpose::Compaction,
            instruction: input.instruction,
            instruction_format_version: NonNegativeSafeInteger::new(
                COMPACTION_INSTRUCTION_FORMAT_VERSION,
            )
            .map_err(crate::model::ModelError::from)?,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        validate_shadowed(
            self.shadowed_range,
            &self.shadowed_seqs,
            MAX_COMPACTION_SHADOWED_SEQS,
        )?;
        self.prepared_call.validate()?;
        if self
            .system
            .as_ref()
            .is_some_and(|system| system.len() > MAX_COMPACTION_SYSTEM_BYTES)
        {
            return Err(invalid_compaction("compaction system text is too large"));
        }
        if self.tools.len() > MAX_COMPACTION_TOOLS {
            return Err(invalid_compaction("compaction has too many tool schemas"));
        }
        validate_session_id(&self.session_id)?;
        if self.purpose != RequestPurpose::Compaction {
            return Err(invalid_compaction(
                "compaction dispatch purpose must be compaction",
            ));
        }
        if self.instruction_format_version.get() != COMPACTION_INSTRUCTION_FORMAT_VERSION {
            return Err(invalid_compaction(
                "unsupported compaction instruction format version",
            ));
        }
        if self.instruction.role() != MessageRole::User
            || !matches!(
                self.instruction.source().kind(),
                MessageSourceKind::Plugin { plugin, .. }
                    if plugin == COMPACTION_INSTRUCTION_SOURCE
            )
            || !is_exact_instruction_source(self.instruction.source())
        {
            return Err(invalid_compaction(
                "compaction instruction must be a dsh.compaction user message",
            ));
        }
        let instruction_id = self.instruction.id().as_str();
        if instruction_id.is_empty()
            || instruction_id.len() > MAX_COMPACTION_INSTRUCTION_ID_BYTES
            || !instruction_id
                .bytes()
                .all(|byte| (0x21..=0x7e).contains(&byte))
        {
            return Err(invalid_compaction(
                "compaction instruction ID must be 1 to 1024 visible ASCII bytes",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn trigger(&self) -> CompactionTrigger {
        self.trigger
    }

    #[must_use]
    pub fn source_surface_generation(&self) -> NonNegativeSafeInteger {
        self.source_surface_generation
    }

    #[must_use]
    pub fn shadowed_range(&self) -> CompactionRange {
        self.shadowed_range
    }

    #[must_use]
    pub fn shadowed_seqs(&self) -> &[EventSeq] {
        &self.shadowed_seqs
    }

    #[must_use]
    pub fn prepared_call(&self) -> &PreparedCompactionCallSnapshot {
        &self.prepared_call
    }

    #[must_use]
    pub fn request_header_seq(&self) -> Option<EventSeq> {
        self.request_header_seq
    }

    #[must_use]
    pub fn request_context_seq(&self) -> Option<EventSeq> {
        self.request_context_seq
    }

    #[must_use]
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolSchema] {
        &self.tools
    }

    #[must_use]
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    #[must_use]
    pub fn instruction(&self) -> &Message {
        &self.instruction
    }

    #[must_use]
    pub fn purpose(&self) -> RequestPurpose {
        self.purpose
    }

    #[must_use]
    pub fn instruction_format_version(&self) -> NonNegativeSafeInteger {
        self.instruction_format_version
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CompactionOwner(Option<TurnId>);

impl Serialize for CompactionOwner {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CompactionOwner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Option::<TurnId>::deserialize(deserializer).map(Self)
    }
}

/// `compaction/start` payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionStartEvent {
    compaction_id: CompactionId,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    source_command_id: Option<String>,
    turn: CompactionOwner,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    dispatch: Option<ModelVisibleDispatchSnapshot>,
}

impl CompactionStartEvent {
    pub fn new(
        compaction_id: CompactionId,
        source_command_id: Option<String>,
        turn: TurnId,
        dispatch: ModelVisibleDispatchSnapshot,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            compaction_id,
            source_command_id,
            turn: CompactionOwner(Some(turn)),
            dispatch: Some(dispatch),
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        self.compaction_id.validate()?;
        validate_source_command_id(self.source_command_id.as_deref())?;
        if let Some(dispatch) = &self.dispatch {
            dispatch.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn compaction_id(&self) -> &CompactionId {
        &self.compaction_id
    }

    #[must_use]
    pub fn source_command_id(&self) -> Option<&str> {
        self.source_command_id.as_deref()
    }

    #[must_use]
    pub fn turn(&self) -> Option<TurnId> {
        self.turn.0
    }

    #[must_use]
    pub fn dispatch(&self) -> Option<&ModelVisibleDispatchSnapshot> {
        self.dispatch.as_ref()
    }
}

/// `compaction/summary` payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionSummaryEvent {
    compaction_id: CompactionId,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    source_command_id: Option<String>,
    summary: Vec<ContentBlock>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    raw_output: Option<Vec<ContentBlock>>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    llm_stream_call: Option<TrueMarker>,
    shadowed_range: CompactionRange,
    shadowed_seqs: Vec<EventSeq>,
    shadowed_token_count: NonNegativeSafeInteger,
    provider: String,
    model: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    max_tokens: Option<NonNegativeSafeInteger>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    usage: Option<TokenUsage>,
}

/// Named construction input for one successfully aggregated summary call.
pub struct CompactionSummaryInput {
    pub compaction_id: CompactionId,
    pub source_command_id: Option<String>,
    pub summary: Vec<ContentBlock>,
    pub raw_output: Vec<ContentBlock>,
    pub shadowed_range: CompactionRange,
    pub shadowed_seqs: Vec<EventSeq>,
    pub shadowed_token_count: NonNegativeSafeInteger,
    pub provider: String,
    pub model: String,
    pub max_tokens: Option<NonNegativeSafeInteger>,
    pub usage: Option<TokenUsage>,
}

impl CompactionSummaryEvent {
    pub fn new(input: CompactionSummaryInput) -> Result<Self, EventValidationError> {
        let value = Self {
            compaction_id: input.compaction_id,
            source_command_id: input.source_command_id,
            summary: input.summary,
            raw_output: Some(input.raw_output),
            llm_stream_call: Some(TrueMarker),
            shadowed_range: input.shadowed_range,
            shadowed_seqs: input.shadowed_seqs,
            shadowed_token_count: input.shadowed_token_count,
            provider: input.provider,
            model: input.model,
            max_tokens: input.max_tokens,
            usage: input.usage,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        self.compaction_id.validate()?;
        validate_source_command_id(self.source_command_id.as_deref())?;
        if self.summary.is_empty() || self.summary.len() > MAX_MESSAGE_CONTENT_BLOCKS {
            return Err(invalid_compaction(
                "compaction summary block count is invalid",
            ));
        }
        if !self.summary.iter().all(|block| {
            matches!(block.kind(), ContentBlockKind::Text { .. })
        }) || !self.summary.iter().any(|block| {
            matches!(block.kind(), ContentBlockKind::Text { text } if !text.trim().is_empty())
        }) {
            return Err(invalid_compaction(
                "compaction summary must contain nonempty text blocks only",
            ));
        }
        if self
            .raw_output
            .as_ref()
            .is_some_and(|output| output.len() > MAX_MESSAGE_CONTENT_BLOCKS)
        {
            return Err(invalid_compaction(
                "compaction raw output has too many blocks",
            ));
        }
        if self.llm_stream_call.is_some() {
            let Some(raw_output) = &self.raw_output else {
                return Err(invalid_compaction(
                    "llmStreamCall requires complete rawOutput",
                ));
            };
            if raw_output.iter().any(|block| {
                !matches!(
                    block.kind(),
                    ContentBlockKind::Text { .. } | ContentBlockKind::Reasoning { .. }
                )
            }) {
                return Err(invalid_compaction(
                    "compaction rawOutput may contain only reasoning and text blocks",
                ));
            }
            let mut text = raw_output
                .iter()
                .filter(|block| matches!(block.kind(), ContentBlockKind::Text { .. }));
            if !self
                .summary
                .iter()
                .all(|summary| text.next() == Some(summary))
                || text.next().is_some()
            {
                return Err(invalid_compaction(
                    "compaction summary must exactly preserve rawOutput text blocks",
                ));
            }
        }
        validate_shadowed(
            self.shadowed_range,
            &self.shadowed_seqs,
            MAX_COMPACTION_SHADOWED_SEQS,
        )?;
        validate_route(&self.provider, "compaction summary provider is invalid")?;
        validate_route(&self.model, "compaction summary model is invalid")?;
        if self.max_tokens.is_some_and(|maximum| maximum.get() == 0) {
            return Err(invalid_compaction(
                "compaction summary maxTokens must be positive when present",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn compaction_id(&self) -> &CompactionId {
        &self.compaction_id
    }

    #[must_use]
    pub fn source_command_id(&self) -> Option<&str> {
        self.source_command_id.as_deref()
    }

    #[must_use]
    pub fn summary(&self) -> &[ContentBlock] {
        &self.summary
    }

    #[must_use]
    pub fn raw_output(&self) -> Option<&[ContentBlock]> {
        self.raw_output.as_deref()
    }

    #[must_use]
    pub fn is_llm_stream_call(&self) -> bool {
        self.llm_stream_call.is_some()
    }

    #[must_use]
    pub fn shadowed_range(&self) -> CompactionRange {
        self.shadowed_range
    }

    #[must_use]
    pub fn shadowed_seqs(&self) -> &[EventSeq] {
        &self.shadowed_seqs
    }

    #[must_use]
    pub fn shadowed_token_count(&self) -> NonNegativeSafeInteger {
        self.shadowed_token_count
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn max_tokens(&self) -> Option<NonNegativeSafeInteger> {
        self.max_tokens
    }

    #[must_use]
    pub fn usage(&self) -> Option<&TokenUsage> {
        self.usage.as_ref()
    }
}

/// Read-compatible error form for `compaction/end`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CompactionEndError {
    Failure(LlmFailure),
    LegacyString(String),
}

impl CompactionEndError {
    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        match self {
            Self::LegacyString(message)
                if message.len() <= MAX_COMPACTION_FAILURE_MESSAGE_BYTES =>
            {
                Ok(())
            }
            Self::LegacyString(_) => Err(invalid_compaction(
                "legacy compaction error exceeds the compatibility byte limit",
            )),
            Self::Failure(failure) => {
                validate_bounded_identity(
                    failure.code(),
                    MAX_COMPACTION_FAILURE_CODE_BYTES,
                    "compaction failure code is invalid",
                )?;
                if failure.message().is_empty()
                    || failure.message().len() > MAX_COMPACTION_FAILURE_MESSAGE_BYTES
                {
                    return Err(invalid_compaction("compaction failure message is invalid"));
                }
                if failure.request_id().is_some_and(|request_id| {
                    request_id.is_empty()
                        || request_id.as_str().len() > MAX_COMPACTION_FAILURE_REQUEST_ID_BYTES
                        || request_id.as_str().chars().any(char::is_control)
                }) {
                    return Err(invalid_compaction(
                        "compaction failure requestId is invalid",
                    ));
                }
                if failure.raw().encoded_len() > MAX_COMPACTION_FAILURE_JSON_BYTES {
                    return Err(invalid_compaction("compaction failure JSON is too large"));
                }
                Ok(())
            }
        }
    }

    pub(crate) fn is_legacy(&self) -> bool {
        matches!(self, Self::LegacyString(_))
    }
}

/// `compaction/end` payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionEndEvent {
    compaction_id: CompactionId,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    source_command_id: Option<String>,
    turn: CompactionOwner,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    error: Option<CompactionEndError>,
}

impl CompactionEndEvent {
    pub fn new(
        compaction_id: CompactionId,
        source_command_id: Option<String>,
        turn: TurnId,
        error: Option<LlmFailure>,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            compaction_id,
            source_command_id,
            turn: CompactionOwner(Some(turn)),
            error: error.map(CompactionEndError::Failure),
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        self.compaction_id.validate()?;
        validate_source_command_id(self.source_command_id.as_deref())?;
        if let Some(error) = &self.error {
            error.validate()?;
        }
        Ok(())
    }

    #[must_use]
    pub fn compaction_id(&self) -> &CompactionId {
        &self.compaction_id
    }

    #[must_use]
    pub fn source_command_id(&self) -> Option<&str> {
        self.source_command_id.as_deref()
    }

    #[must_use]
    pub fn turn(&self) -> Option<TurnId> {
        self.turn.0
    }

    #[must_use]
    pub fn error(&self) -> Option<&CompactionEndError> {
        self.error.as_ref()
    }
}

/// `compaction/prune` shadow-price marker.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionPruneEvent {
    shadowed_range: CompactionRange,
    shadowed_seqs: Vec<EventSeq>,
    shadowed_token_count: NonNegativeSafeInteger,
}

impl CompactionPruneEvent {
    pub fn new(
        shadowed_range: CompactionRange,
        shadowed_seqs: Vec<EventSeq>,
        shadowed_token_count: NonNegativeSafeInteger,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            shadowed_range,
            shadowed_seqs,
            shadowed_token_count,
        };
        value.validate()?;
        Ok(value)
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        validate_shadowed(self.shadowed_range, &self.shadowed_seqs, 1)?;
        if self.shadowed_seqs.len() != 1 || self.shadowed_range.start != self.shadowed_range.end {
            return Err(invalid_compaction(
                "compaction/prune must name one singleton surface node",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn shadowed_range(&self) -> CompactionRange {
        self.shadowed_range
    }

    #[must_use]
    pub fn shadowed_seqs(&self) -> &[EventSeq] {
        &self.shadowed_seqs
    }

    #[must_use]
    pub fn shadowed_token_count(&self) -> NonNegativeSafeInteger {
        self.shadowed_token_count
    }
}

fn validate_shadowed(
    range: CompactionRange,
    shadowed: &[EventSeq],
    maximum: usize,
) -> Result<(), EventValidationError> {
    if shadowed.is_empty() || shadowed.len() > maximum {
        return Err(invalid_compaction(
            "compaction shadowedSeqs length is invalid",
        ));
    }
    if shadowed.first() != Some(&range.start) || shadowed.last() != Some(&range.end) {
        return Err(invalid_compaction(
            "compaction range must match the first and last shadowed seq",
        ));
    }
    let mut seen = BTreeSet::new();
    if shadowed.iter().any(|seq| !seen.insert(*seq)) {
        return Err(invalid_compaction(
            "compaction shadowedSeqs contains a duplicate",
        ));
    }
    Ok(())
}

fn validate_source_command_id(value: Option<&str>) -> Result<(), EventValidationError> {
    value.map_or(Ok(()), |value| {
        validate_bounded_identity(
            value,
            MAX_COMPACTION_SOURCE_COMMAND_ID_BYTES,
            "sourceCommandId must be 1 to 1024 non-control UTF-8 bytes",
        )
    })
}

fn validate_route(value: &str, detail: &'static str) -> Result<(), EventValidationError> {
    validate_bounded_identity(value, MAX_COMPACTION_ROUTE_BYTES, detail)
}

fn validate_session_id(session_id: &SessionId) -> Result<(), EventValidationError> {
    let value = session_id.as_str();
    if value.is_empty()
        || value.len() > 1_024
        || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(invalid_compaction("compaction sessionId is invalid"));
    }
    Ok(())
}

fn validate_bounded_identity(
    value: &str,
    maximum: usize,
    detail: &'static str,
) -> Result<(), EventValidationError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(invalid_compaction(detail));
    }
    Ok(())
}

fn is_exact_instruction_source(source: &crate::model::MessageSource) -> bool {
    let Some(fields) = source.raw().as_value().as_object() else {
        return false;
    };
    fields.len() == 2
        && fields.get("kind").and_then(serde_json::Value::as_str) == Some("plugin")
        && fields.get("plugin").and_then(serde_json::Value::as_str)
            == Some(COMPACTION_INSTRUCTION_SOURCE)
}

fn invalid_compaction(detail: &'static str) -> EventValidationError {
    EventValidationError::InvalidCompactionEvent(detail)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        model::{JsonValue, MessageSource, ProviderRequestId, ReasoningEffortId},
        session::{EventKind, codec::kind_data_value},
    };

    fn integer(value: u64) -> NonNegativeSafeInteger {
        NonNegativeSafeInteger::new(value).unwrap()
    }

    fn retry_policy() -> PreparedRetryPolicySnapshot {
        PreparedRetryPolicySnapshot::normal(
            integer(2),
            vec!["RATE_LIMIT".into()],
            PreparedRetryBackoffSnapshot::new(
                PositiveFiniteNumber::new(10.0).unwrap(),
                PositiveFiniteNumber::new(100.0).unwrap(),
                FiniteNumber::new(0.25).unwrap(),
            )
            .unwrap(),
        )
        .unwrap()
    }

    fn prepared_call(max_tokens: u64) -> PreparedCompactionCallSnapshot {
        PreparedCompactionCallSnapshot::new(
            LlmCallConfig::from_parts(
                "deepseek".into(),
                "deepseek-chat".into(),
                Some(ReasoningEffortId::new("high")),
                None,
                Some(integer(max_tokens)),
                None,
            )
            .unwrap(),
            LlmCallConfigAdapterDefaults::default(),
            Some(integer(131_072)),
            retry_policy(),
        )
        .unwrap()
    }

    fn dispatch() -> ModelVisibleDispatchSnapshot {
        ModelVisibleDispatchSnapshot::new(ModelVisibleDispatchInput {
            trigger: CompactionTrigger::Pressure,
            source_surface_generation: integer(7),
            shadowed_range: CompactionRange::new(
                EventSeq::new(2).unwrap(),
                EventSeq::new(3).unwrap(),
            ),
            shadowed_seqs: vec![EventSeq::new(2).unwrap(), EventSeq::new(3).unwrap()],
            prepared_call: prepared_call(4_096),
            request_header_seq: Some(EventSeq::new(0).unwrap()),
            request_context_seq: Some(EventSeq::new(1).unwrap()),
            system: Some("system prompt".into()),
            tools: Vec::new(),
            session_id: SessionId::new("session-compaction"),
            instruction: Message::user(
                "compaction-instruction",
                vec![ContentBlock::text("Summarize the selected range.").unwrap()],
                MessageSource::plugin(COMPACTION_INSTRUCTION_SOURCE).unwrap(),
            )
            .unwrap(),
        })
        .unwrap()
    }

    fn summary_input(
        summary: Vec<ContentBlock>,
        raw_output: Vec<ContentBlock>,
        max_tokens: u64,
    ) -> CompactionSummaryInput {
        CompactionSummaryInput {
            compaction_id: CompactionId::new("compact-1"),
            source_command_id: Some("command-1".into()),
            summary,
            raw_output,
            shadowed_range: CompactionRange::new(
                EventSeq::new(2).unwrap(),
                EventSeq::new(3).unwrap(),
            ),
            shadowed_seqs: vec![EventSeq::new(2).unwrap(), EventSeq::new(3).unwrap()],
            shadowed_token_count: integer(500),
            provider: "deepseek".into(),
            model: "deepseek-chat".into(),
            max_tokens: Some(integer(max_tokens)),
            usage: None,
        }
    }

    #[test]
    fn constructors_emit_the_frozen_camel_case_wire_shape() {
        let start = CompactionStartEvent::new(
            CompactionId::new("compact-1"),
            Some("command-1".into()),
            TurnId::new(4).unwrap(),
            dispatch(),
        )
        .unwrap();
        let text = ContentBlock::from_value(json!({
            "type": "text",
            "text": "stable summary",
            "extension": { "kept": true }
        }))
        .unwrap();
        let summary = CompactionSummaryEvent::new(summary_input(
            vec![text.clone()],
            vec![ContentBlock::reasoning("private").unwrap(), text],
            4_096,
        ))
        .unwrap();
        let end = CompactionEndEvent::new(
            CompactionId::new("compact-1"),
            Some("command-1".into()),
            TurnId::new(4).unwrap(),
            None,
        )
        .unwrap();
        let prune = CompactionPruneEvent::new(
            CompactionRange::new(EventSeq::new(9).unwrap(), EventSeq::new(9).unwrap()),
            vec![EventSeq::new(9).unwrap()],
            integer(42),
        )
        .unwrap();

        let start_data = kind_data_value(&EventKind::compaction_start(start.clone())).unwrap();
        assert_eq!(start_data["compactionId"], "compact-1");
        assert_eq!(start_data["sourceCommandId"], "command-1");
        assert_eq!(start_data["dispatch"]["purpose"], "compaction");
        assert_eq!(
            start_data["dispatch"]["preparedCall"]["retryPolicy"],
            json!({
                "mode": "normal",
                "maxRetries": 2,
                "retryableCodes": ["RATE_LIMIT"],
                "backoff": {
                    "initialDelayMs": 10,
                    "maxDelayMs": 100,
                    "jitterRatio": 0.25
                }
            })
        );
        assert_eq!(
            start_data["dispatch"]["instructionFormatVersion"],
            COMPACTION_INSTRUCTION_FORMAT_VERSION
        );
        assert_eq!(
            start_data["dispatch"]["instruction"]["source"],
            json!({ "kind": "plugin", "plugin": "dsh.compaction" })
        );
        assert_eq!(
            serde_json::from_value::<CompactionStartEvent>(start_data).unwrap(),
            start
        );

        let summary_data =
            kind_data_value(&EventKind::compaction_summary(summary.clone())).unwrap();
        assert_eq!(summary_data["llmStreamCall"], true);
        assert_eq!(summary_data["shadowedTokenCount"], 500);
        assert_eq!(summary_data["summary"][0]["extension"]["kept"], true);
        assert_eq!(
            serde_json::from_value::<CompactionSummaryEvent>(summary_data).unwrap(),
            summary
        );

        let end_data = kind_data_value(&EventKind::compaction_end(end.clone())).unwrap();
        assert!(end_data.get("error").is_none());
        assert_eq!(
            serde_json::from_value::<CompactionEndEvent>(end_data).unwrap(),
            end
        );
        let failed_end = CompactionEndEvent::new(
            CompactionId::new("compact-1"),
            Some("command-1".into()),
            TurnId::new(4).unwrap(),
            Some(
                LlmFailure::from_parts(
                    "try later".into(),
                    "RATE_LIMIT".into(),
                    Some(429),
                    Some(PositiveFiniteNumber::new(1_250.0).unwrap()),
                    Some(ProviderRequestId::new("request-1")),
                )
                .unwrap(),
            ),
        )
        .unwrap();
        assert_eq!(
            kind_data_value(&EventKind::compaction_end(failed_end)).unwrap(),
            json!({
                "compactionId": "compact-1",
                "sourceCommandId": "command-1",
                "turn": 4,
                "error": {
                    "message": "try later",
                    "code": "RATE_LIMIT",
                    "status": 429,
                    "providerRetryAfterMs": 1250,
                    "requestId": "request-1"
                }
            })
        );

        let prune_data = kind_data_value(&EventKind::compaction_prune(prune.clone())).unwrap();
        assert_eq!(prune_data["shadowedSeqs"], json!([9]));
        assert_eq!(
            serde_json::from_value::<CompactionPruneEvent>(prune_data).unwrap(),
            prune
        );
    }

    #[test]
    fn llm_summary_must_exactly_filter_reasoning_from_raw_output() {
        let text = ContentBlock::from_value(json!({
            "type": "text",
            "text": "kept",
            "extension": 1
        }))
        .unwrap();
        CompactionSummaryEvent::new(summary_input(
            vec![text.clone()],
            vec![ContentBlock::reasoning("private").unwrap(), text],
            128,
        ))
        .unwrap();

        let empty = CompactionSummaryEvent::new(summary_input(
            vec![ContentBlock::text("kept").unwrap()],
            Vec::new(),
            128,
        ));
        assert!(matches!(
            empty,
            Err(EventValidationError::InvalidCompactionEvent(_))
        ));

        let mismatch = CompactionSummaryEvent::new(summary_input(
            vec![ContentBlock::text("summary").unwrap()],
            vec![ContentBlock::text("different").unwrap()],
            128,
        ));
        assert!(matches!(
            mismatch,
            Err(EventValidationError::InvalidCompactionEvent(_))
        ));

        let tool_call = CompactionSummaryEvent::new(summary_input(
            vec![ContentBlock::text("summary").unwrap()],
            vec![ContentBlock::tool_call("call", "read", "{}").unwrap()],
            128,
        ));
        assert!(matches!(
            tool_call,
            Err(EventValidationError::InvalidCompactionEvent(_))
        ));
    }

    #[test]
    fn compaction_bounds_accept_exact_values_and_reject_one_over() {
        CompactionId::new("x".repeat(MAX_COMPACTION_ID_BYTES))
            .validate()
            .unwrap();
        assert!(
            CompactionId::new("x".repeat(MAX_COMPACTION_ID_BYTES + 1))
                .validate()
                .is_err()
        );

        let instruction = |id: String| {
            Message::user(
                id,
                vec![ContentBlock::text("Summarize.").unwrap()],
                MessageSource::plugin(COMPACTION_INSTRUCTION_SOURCE).unwrap(),
            )
            .unwrap()
        };
        let mut exact_dispatch = dispatch();
        exact_dispatch.instruction = instruction("x".repeat(MAX_COMPACTION_INSTRUCTION_ID_BYTES));
        exact_dispatch.validate().unwrap();
        assert_eq!(exact_dispatch.purpose(), RequestPurpose::Compaction);
        assert_eq!(
            exact_dispatch.instruction_format_version().get(),
            COMPACTION_INSTRUCTION_FORMAT_VERSION
        );
        let mut long_dispatch = dispatch();
        long_dispatch.instruction =
            instruction("x".repeat(MAX_COMPACTION_INSTRUCTION_ID_BYTES + 1));
        assert!(long_dispatch.validate().is_err());
        let mut control_dispatch = dispatch();
        control_dispatch.instruction = instruction("bad\nid".into());
        assert!(control_dispatch.validate().is_err());

        let zero_call = PreparedCompactionCallSnapshot::new(
            LlmCallConfig::from_parts(
                "deepseek".into(),
                "deepseek-chat".into(),
                None,
                None,
                Some(integer(0)),
                None,
            )
            .unwrap(),
            LlmCallConfigAdapterDefaults::default(),
            None,
            retry_policy(),
        );
        assert!(matches!(
            zero_call,
            Err(EventValidationError::InvalidCompactionEvent(_))
        ));

        let zero_summary = CompactionSummaryEvent::new(summary_input(
            vec![ContentBlock::text("summary").unwrap()],
            vec![ContentBlock::text("summary").unwrap()],
            0,
        ));
        assert!(matches!(
            zero_summary,
            Err(EventValidationError::InvalidCompactionEvent(_))
        ));

        let exact_seqs = (1..=u64::try_from(MAX_COMPACTION_SHADOWED_SEQS).unwrap())
            .map(|seq| EventSeq::new(seq).unwrap())
            .collect::<Vec<_>>();
        let exact_range =
            CompactionRange::new(*exact_seqs.first().unwrap(), *exact_seqs.last().unwrap());
        let mut exact = summary_input(
            vec![ContentBlock::text("summary").unwrap()],
            vec![ContentBlock::text("summary").unwrap()],
            128,
        );
        exact.shadowed_range = exact_range;
        exact.shadowed_seqs = exact_seqs.clone();
        CompactionSummaryEvent::new(exact).unwrap();

        let mut one_over_seqs = exact_seqs;
        one_over_seqs
            .push(EventSeq::new(u64::try_from(MAX_COMPACTION_SHADOWED_SEQS + 1).unwrap()).unwrap());
        let mut one_over = summary_input(
            vec![ContentBlock::text("summary").unwrap()],
            vec![ContentBlock::text("summary").unwrap()],
            128,
        );
        one_over.shadowed_range = CompactionRange::new(
            *one_over_seqs.first().unwrap(),
            *one_over_seqs.last().unwrap(),
        );
        one_over.shadowed_seqs = one_over_seqs;
        assert!(matches!(
            CompactionSummaryEvent::new(one_over),
            Err(EventValidationError::InvalidCompactionEvent(_))
        ));
    }

    #[test]
    fn retry_and_failure_bounds_are_exact_and_fail_closed() {
        let backoff = || {
            PreparedRetryBackoffSnapshot::new(
                PositiveFiniteNumber::new(1.0).unwrap(),
                PositiveFiniteNumber::new(2.0).unwrap(),
                FiniteNumber::new(0.5).unwrap(),
            )
            .unwrap()
        };
        let exact_codes = (0..MAX_COMPACTION_RETRY_CODES)
            .map(|index| format!("ERROR_{index}"))
            .collect::<Vec<_>>();
        PreparedRetryPolicySnapshot::normal(integer(2), exact_codes.clone(), backoff()).unwrap();
        let mut too_many_codes = exact_codes;
        too_many_codes.push("ONE_OVER".into());
        assert!(
            PreparedRetryPolicySnapshot::normal(integer(2), too_many_codes, backoff()).is_err()
        );
        PreparedRetryPolicySnapshot::normal(
            integer(2),
            vec!["x".repeat(MAX_COMPACTION_RETRY_CODE_BYTES)],
            backoff(),
        )
        .unwrap();
        assert!(
            PreparedRetryPolicySnapshot::normal(
                integer(2),
                vec!["x".repeat(MAX_COMPACTION_RETRY_CODE_BYTES + 1)],
                backoff(),
            )
            .is_err()
        );
        assert!(
            PreparedRetryPolicySnapshot::normal(
                integer(2),
                vec!["DUPLICATE".into(), "DUPLICATE".into()],
                backoff(),
            )
            .is_err()
        );
        assert!(
            PreparedRetryBackoffSnapshot::new(
                PositiveFiniteNumber::new(2.0).unwrap(),
                PositiveFiniteNumber::new(1.0).unwrap(),
                FiniteNumber::new(0.5).unwrap(),
            )
            .is_err()
        );
        assert!(
            PreparedRetryBackoffSnapshot::new(
                PositiveFiniteNumber::new(1.0).unwrap(),
                PositiveFiniteNumber::new(2.0).unwrap(),
                FiniteNumber::new(1.01).unwrap(),
            )
            .is_err()
        );

        CompactionEndError::Failure(
            LlmFailure::new("m".repeat(MAX_COMPACTION_FAILURE_MESSAGE_BYTES), "CODE").unwrap(),
        )
        .validate()
        .unwrap();
        assert!(
            CompactionEndError::Failure(
                LlmFailure::new("m".repeat(MAX_COMPACTION_FAILURE_MESSAGE_BYTES + 1), "CODE",)
                    .unwrap(),
            )
            .validate()
            .is_err()
        );
        CompactionEndError::Failure(
            LlmFailure::new("message", "C".repeat(MAX_COMPACTION_FAILURE_CODE_BYTES)).unwrap(),
        )
        .validate()
        .unwrap();
        assert!(
            CompactionEndError::Failure(
                LlmFailure::new("message", "C".repeat(MAX_COMPACTION_FAILURE_CODE_BYTES + 1),)
                    .unwrap(),
            )
            .validate()
            .is_err()
        );
        let request_failure = |length| {
            LlmFailure::from_parts(
                "message".into(),
                "CODE".into(),
                None,
                None,
                Some(ProviderRequestId::new("r".repeat(length))),
            )
            .unwrap()
        };
        CompactionEndError::Failure(request_failure(MAX_COMPACTION_FAILURE_REQUEST_ID_BYTES))
            .validate()
            .unwrap();
        assert!(
            CompactionEndError::Failure(request_failure(
                MAX_COMPACTION_FAILURE_REQUEST_ID_BYTES + 1
            ))
            .validate()
            .is_err()
        );
        CompactionEndError::LegacyString("x".repeat(MAX_COMPACTION_FAILURE_MESSAGE_BYTES))
            .validate()
            .unwrap();
        assert!(
            CompactionEndError::LegacyString("x".repeat(MAX_COMPACTION_FAILURE_MESSAGE_BYTES + 1))
                .validate()
                .is_err()
        );
        let oversized_raw: LlmFailure = serde_json::from_value(json!({
            "message": "message",
            "code": "CODE",
            "extension": "x".repeat(MAX_COMPACTION_FAILURE_JSON_BYTES)
        }))
        .unwrap();
        assert!(
            CompactionEndError::Failure(oversized_raw)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn dispatch_and_block_collection_bounds_are_exact_and_fail_closed() {
        let mut exact_system = dispatch();
        exact_system.system = Some("s".repeat(MAX_COMPACTION_SYSTEM_BYTES));
        exact_system.validate().unwrap();
        let mut long_system = dispatch();
        long_system.system = Some("s".repeat(MAX_COMPACTION_SYSTEM_BYTES + 1));
        assert!(long_system.validate().is_err());

        let schema = ToolSchema::new(
            "read",
            "read one file",
            JsonValue::new(json!({ "type": "object" })).unwrap(),
        )
        .unwrap();
        let mut exact_tools = dispatch();
        exact_tools.tools = vec![schema.clone(); MAX_COMPACTION_TOOLS];
        exact_tools.validate().unwrap();
        let mut too_many_tools = dispatch();
        too_many_tools.tools = vec![schema; MAX_COMPACTION_TOOLS + 1];
        assert!(too_many_tools.validate().is_err());

        let block = ContentBlock::text("summary").unwrap();
        CompactionSummaryEvent::new(summary_input(
            vec![block.clone(); MAX_MESSAGE_CONTENT_BLOCKS],
            vec![block.clone(); MAX_MESSAGE_CONTENT_BLOCKS],
            128,
        ))
        .unwrap();
        assert!(
            CompactionSummaryEvent::new(summary_input(
                vec![block.clone(); MAX_MESSAGE_CONTENT_BLOCKS + 1],
                vec![block; MAX_MESSAGE_CONTENT_BLOCKS + 1],
                128,
            ))
            .is_err()
        );

        let route = |provider: String, model: String| {
            PreparedCompactionCallSnapshot::new(
                LlmCallConfig::new(provider, model).unwrap(),
                LlmCallConfigAdapterDefaults::default(),
                None,
                retry_policy(),
            )
        };
        route("p".repeat(MAX_COMPACTION_ROUTE_BYTES), "model".into()).unwrap();
        assert!(route("p".repeat(MAX_COMPACTION_ROUTE_BYTES + 1), "model".into()).is_err());
        route("provider".into(), "m".repeat(MAX_COMPACTION_ROUTE_BYTES)).unwrap();
        assert!(
            route(
                "provider".into(),
                "m".repeat(MAX_COMPACTION_ROUTE_BYTES + 1)
            )
            .is_err()
        );

        CompactionStartEvent::new(
            CompactionId::new("compact"),
            Some("c".repeat(MAX_COMPACTION_SOURCE_COMMAND_ID_BYTES)),
            TurnId::new(1).unwrap(),
            dispatch(),
        )
        .unwrap();
        assert!(
            CompactionStartEvent::new(
                CompactionId::new("compact"),
                Some("c".repeat(MAX_COMPACTION_SOURCE_COMMAND_ID_BYTES + 1)),
                TurnId::new(1).unwrap(),
                dispatch(),
            )
            .is_err()
        );
        let mut exact_session = dispatch();
        exact_session.session_id = SessionId::new("s".repeat(1_024));
        exact_session.validate().unwrap();
        let mut long_session = dispatch();
        long_session.session_id = SessionId::new("s".repeat(1_025));
        assert!(long_session.validate().is_err());
    }
}
