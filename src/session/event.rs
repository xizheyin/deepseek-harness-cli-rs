//! Session identifiers, headers, event payloads, and surface metadata.

use std::{fmt, num::NonZeroU64, path::Path};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value, json};

pub use crate::json_value::MAX_SAFE_INTEGER;
use crate::json_value::{deserialize_present_option, deserialize_safe_i64, deserialize_safe_u64};
use crate::model::{
    CallId, FiniteNumber, JsonValue, LlmCallConfig, LlmCallConfigAdapterDefaults, LlmFailure,
    Message, NonNegativeSafeInteger, StreamChunk, TokenUsage, ToolSchema, TrueMarker,
};
use crate::workspace_authority::WorkspaceIdentity;

use super::error::{EventValidationError, HeaderError, NumberError};

/// Current upstream-compatible session format version.
pub const SESSION_FORMAT_VERSION: u64 = 0;
/// Recovery result emitted when a model call never reached tool dispatch.
pub const TOOL_NOT_STARTED: &str = "TOOL_NOT_STARTED";
/// Conservative result emitted when a durable intent may already have had an effect.
pub const TOOL_OUTCOME_UNKNOWN: &str = "TOOL_OUTCOME_UNKNOWN";
/// Reserved identity prefix for tool-result rows emitted only by recovery.
pub(crate) const RECOVERY_TOOL_RESULT_ID_PREFIX: &str = "dsh-recovery-tool-result-v1-";
/// Maximum provenance references retained on one surface event.
pub const MAX_SOURCE_EVENT_SEQS: usize = 4_096;
/// Maximum compact JSON bytes retained in one session header.
pub const MAX_SESSION_HEADER_BYTES: usize = 64 * 1024;

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            #[must_use]
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }
    };
}

string_id!(
    /// Identifies one session and its future persistence artifacts.
    SessionId
);
string_id!(
    /// Correlates all scheduled waits in one provider-policy retry chain.
    RetryId
);
string_id!(
    /// Correlates one approval question with its durable decision.
    ApprovalRequestId
);

/// Zero-based event position within one session.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EventSeq(u64);

impl EventSeq {
    /// Validate an event sequence against the upstream safe-integer domain.
    pub fn new(value: u64) -> Result<Self, NumberError> {
        if value > MAX_SAFE_INTEGER {
            return Err(NumberError::NonNegativeSafeInteger { field: "seq" });
        }
        Ok(Self(value))
    }

    /// Return the numeric sequence.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }

    pub(crate) fn from_index(index: usize) -> Option<Self> {
        u64::try_from(index)
            .ok()
            .filter(|value| *value <= MAX_SAFE_INTEGER)
            .map(Self)
    }
}

impl fmt::Display for EventSeq {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for EventSeq {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for EventSeq {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserialize_safe_u64(deserializer, false)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

macro_rules! nonzero_id {
    ($(#[$meta:meta])* $name:ident, $field:literal) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(NonZeroU64);

        impl $name {
            pub(crate) const fn first() -> Self {
                Self(NonZeroU64::MIN)
            }

            pub fn new(value: u64) -> Result<Self, NumberError> {
                if value == 0 || value > MAX_SAFE_INTEGER {
                    return Err(NumberError::PositiveSafeInteger { field: $field });
                }
                // `value == 0` was rejected above.
                let value = NonZeroU64::new(value)
                    .ok_or(NumberError::PositiveSafeInteger { field: $field })?;
                Ok(Self(value))
            }

            #[must_use]
            pub fn get(self) -> u64 {
                self.0.get()
            }

            pub(crate) fn successor(self) -> Option<Self> {
                self.get().checked_add(1).filter(|value| *value <= MAX_SAFE_INTEGER).and_then(NonZeroU64::new).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.get().fmt(formatter)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_u64(self.get())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = deserialize_safe_u64(deserializer, true)?;
                Self::new(value).map_err(de::Error::custom)
            }
        }
    };
}

nonzero_id!(
    /// One-based turn identity within a session.
    TurnId,
    "turn"
);
nonzero_id!(
    /// One-based step identity within a turn.
    StepId,
    "step"
);
nonzero_id!(
    /// One-based retry number inside one provider-policy chain.
    RetryNumber,
    "retry"
);

/// Signed Unix-epoch milliseconds used by imported and live events.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixMillis(i64);

impl UnixMillis {
    /// Validate a timestamp against JavaScript's exact integer range.
    pub fn new(value: i64) -> Result<Self, NumberError> {
        let maximum = i64::try_from(MAX_SAFE_INTEGER).unwrap_or(i64::MAX);
        if value < -maximum || value > maximum {
            return Err(NumberError::SignedSafeInteger { field: "time" });
        }
        Ok(Self(value))
    }

    /// Return the signed millisecond value.
    #[must_use]
    pub fn get(self) -> i64 {
        self.0
    }
}

impl fmt::Display for UnixMillis {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Serialize for UnixMillis {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_i64(self.0)
    }
}

impl<'de> Deserialize<'de> for UnixMillis {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = deserialize_safe_i64(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Immutable session metadata stored outside the conversation event log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionHeader {
    version: u64,
    id: SessionId,
    created_at: UnixMillis,
    cwd: Option<String>,
    parent_session: Option<SessionId>,
    seed_length: Option<NonNegativeSafeInteger>,
    origin: Option<SessionOrigin>,
    delegation_depth: Option<NonNegativeSafeInteger>,
    agent_preset: Option<String>,
    raw: JsonValue,
}

/// Durable coarse origin for a delegated session.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionOrigin {
    Subagent,
}

impl SessionHeader {
    /// Build the minimal current-version header.
    pub fn new(id: impl Into<SessionId>, created_at: UnixMillis) -> Result<Self, HeaderError> {
        if created_at.get() < 0 {
            return Err(HeaderError::NegativeCreatedAt);
        }
        let id = id.into();
        let raw = JsonValue::new(json!({
            "version": SESSION_FORMAT_VERSION,
            "id": id,
            "createdAt": created_at,
        }))
        .map_err(HeaderError::Json)?;
        validate_header_size(&raw)?;
        Ok(Self {
            version: SESSION_FORMAT_VERSION,
            id,
            created_at,
            cwd: None,
            parent_session: None,
            seed_length: None,
            origin: None,
            delegation_depth: None,
            agent_preset: None,
            raw,
        })
    }

    /// Build the complete top-level durable header in one validated step.
    pub(crate) fn new_durable(
        id: impl Into<SessionId>,
        created_at: UnixMillis,
        cwd: String,
        workspace: WorkspaceIdentity,
    ) -> Result<Self, HeaderError> {
        if !Path::new(&cwd).is_absolute() {
            return Err(HeaderError::RelativeWorkingDirectory(cwd));
        }
        Self::from_value(json!({
            "version": SESSION_FORMAT_VERSION,
            "id": id.into(),
            "createdAt": created_at,
            "cwd": cwd,
            "delegationDepth": 0,
            "rustWorkspaceIdentity": {
                "device": format!("{:x}", workspace.device()),
                "inode": format!("{:x}", workspace.inode()),
            },
        }))
    }

    /// Parse a header while retaining fields added by plugins or newer Harness versions.
    pub fn from_value(value: Value) -> Result<Self, HeaderError> {
        let raw = JsonValue::new(value).map_err(HeaderError::Json)?;
        validate_header_size(&raw)?;
        let fields = raw
            .as_value()
            .as_object()
            .ok_or_else(|| HeaderError::InvalidField {
                field: "header",
                detail: "must be a JSON object".to_owned(),
            })?;
        let version = header_required::<NonNegativeSafeInteger>(fields, "version")?.get();
        let id = SessionId::new(header_required_string(fields, "id")?);
        let created_at = header_required(fields, "createdAt")?;
        let cwd = header_optional_string(fields, "cwd")?;
        let parent_session = header_optional_string(fields, "parentSession")?.map(SessionId::new);
        let seed_length = header_optional(fields, "seedLength")?;
        let origin = header_optional(fields, "origin")?;
        let delegation_depth = header_optional(fields, "delegationDepth")?;
        let agent_preset = header_optional_string(fields, "agentPreset")?;
        let header = Self {
            version,
            id,
            created_at,
            cwd,
            parent_session,
            seed_length,
            origin,
            delegation_depth,
            agent_preset,
            raw,
        };
        header.validate_for(header.id())?;
        Ok(header)
    }

    /// Current durable format version.
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Stable session identity.
    #[must_use]
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Creation time in Unix milliseconds.
    #[must_use]
    pub fn created_at(&self) -> UnixMillis {
        self.created_at
    }

    /// Absolute working directory recorded at creation, when available.
    #[must_use]
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// Complete validated header JSON, including fields unknown to this build.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }

    /// Set and validate an absolute working directory.
    pub fn set_cwd(&mut self, cwd: impl Into<String>) -> Result<(), HeaderError> {
        let cwd = cwd.into();
        if !Path::new(&cwd).is_absolute() {
            return Err(HeaderError::RelativeWorkingDirectory(cwd));
        }
        let mut raw = self.raw.as_value().clone();
        raw.as_object_mut()
            .ok_or_else(|| HeaderError::InvalidField {
                field: "header",
                detail: "must be a JSON object".to_owned(),
            })?
            .insert("cwd".to_owned(), Value::String(cwd.clone()));
        let next_raw = JsonValue::new(raw).map_err(HeaderError::Json)?;
        validate_header_size(&next_raw)?;
        self.raw = next_raw;
        self.cwd = Some(cwd);
        Ok(())
    }

    pub(crate) fn validate_for(&self, requested_id: &SessionId) -> Result<(), HeaderError> {
        if self.version != SESSION_FORMAT_VERSION {
            return Err(HeaderError::UnsupportedVersion {
                expected: SESSION_FORMAT_VERSION,
                actual: self.version,
            });
        }
        if &self.id != requested_id {
            return Err(HeaderError::MismatchedId {
                expected: requested_id.as_str().to_owned(),
                actual: self.id.as_str().to_owned(),
            });
        }
        if self.created_at.get() < 0 {
            return Err(HeaderError::NegativeCreatedAt);
        }
        if let Some(cwd) = &self.cwd {
            if !Path::new(cwd).is_absolute() {
                return Err(HeaderError::RelativeWorkingDirectory(cwd.clone()));
            }
        }
        Ok(())
    }
}

fn validate_header_size(raw: &JsonValue) -> Result<(), HeaderError> {
    if raw.encoded_len() > MAX_SESSION_HEADER_BYTES {
        return Err(HeaderError::TooLarge {
            maximum: MAX_SESSION_HEADER_BYTES,
            actual: raw.encoded_len(),
        });
    }
    Ok(())
}

impl Serialize for SessionHeader {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SessionHeader {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

fn header_required<T: for<'de> Deserialize<'de>>(
    fields: &Map<String, Value>,
    field: &'static str,
) -> Result<T, HeaderError> {
    let value = fields
        .get(field)
        .cloned()
        .ok_or_else(|| HeaderError::InvalidField {
            field,
            detail: "missing required field".to_owned(),
        })?;
    serde_json::from_value(value).map_err(|error| HeaderError::InvalidField {
        field,
        detail: error.to_string(),
    })
}

fn header_optional<T: for<'de> Deserialize<'de>>(
    fields: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<T>, HeaderError> {
    fields
        .get(field)
        .cloned()
        .map(|value| {
            serde_json::from_value(value).map_err(|error| HeaderError::InvalidField {
                field,
                detail: error.to_string(),
            })
        })
        .transpose()
}

fn header_required_string(
    fields: &Map<String, Value>,
    field: &'static str,
) -> Result<String, HeaderError> {
    header_optional_string(fields, field)?.ok_or_else(|| HeaderError::InvalidField {
        field,
        detail: "missing required field".to_owned(),
    })
}

fn header_optional_string(
    fields: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, HeaderError> {
    match fields.get(field) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(HeaderError::InvalidField {
            field,
            detail: "must be a string when present".to_owned(),
        }),
    }
}

/// Why an active agent driver was cancelled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum TurnEndCancelCause {
    User,
    Parent,
    Hook { reason: String },
    Disposed,
    Legacy,
}

/// Why a complete agent turn ended.
///
/// Upstream deliberately lets plugins add new variants. Known variants have a
/// typed view; anything newer or structurally extended remains lossless in
/// `Other` instead of making an older Rust reader reject the session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TurnEndReason {
    Completed,
    Aborted {
        reason: TurnEndCancelCause,
    },
    Blocked,
    Error {
        error: LlmFailure,
    },
    MaxTokens,
    Interrupted,
    Other {
        kind: Option<String>,
        raw: JsonValue,
    },
}

impl TurnEndReason {
    /// Parse a plugin-extensible turn outcome without discarding its raw JSON.
    pub fn from_value(value: Value) -> Result<Self, crate::model::ModelError> {
        let raw = JsonValue::new(value)?;
        let Some(fields) = raw.as_value().as_object() else {
            return Ok(Self::Other { kind: None, raw });
        };
        let kind = fields
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let known = match kind.as_deref() {
            Some("completed") if fields.len() == 1 => Some(Self::Completed),
            Some("blocked") if fields.len() == 1 => Some(Self::Blocked),
            Some("max-tokens") if fields.len() == 1 => Some(Self::MaxTokens),
            Some("interrupted") if fields.len() == 1 => Some(Self::Interrupted),
            Some("aborted") if fields.len() == 2 => fields
                .get("reason")
                .cloned()
                .and_then(|reason| serde_json::from_value(reason).ok())
                .map(|reason| Self::Aborted { reason }),
            Some("error") if fields.len() == 2 => fields
                .get("error")
                .cloned()
                .and_then(|error| serde_json::from_value(error).ok())
                .map(|error| Self::Error { error }),
            _ => None,
        };
        Ok(known.unwrap_or(Self::Other { kind, raw }))
    }

    /// Wire kind when the retained value has a string tag.
    #[must_use]
    pub fn kind(&self) -> Option<&str> {
        match self {
            Self::Completed => Some("completed"),
            Self::Aborted { .. } => Some("aborted"),
            Self::Blocked => Some("blocked"),
            Self::Error { .. } => Some("error"),
            Self::MaxTokens => Some("max-tokens"),
            Self::Interrupted => Some("interrupted"),
            Self::Other { kind, .. } => kind.as_deref(),
        }
    }

    fn validate_canonical(&self) -> Result<(), EventValidationError> {
        let Self::Other { raw, .. } = self else {
            return Ok(());
        };
        let reparsed = Self::from_value(raw.as_value().clone())?;
        if &reparsed != self {
            return Err(EventValidationError::InconsistentTurnEndReason);
        }
        Ok(())
    }
}

impl Serialize for TurnEndReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Completed => json!({ "kind": "completed" }).serialize(serializer),
            Self::Aborted { reason } => {
                json!({ "kind": "aborted", "reason": reason }).serialize(serializer)
            }
            Self::Blocked => json!({ "kind": "blocked" }).serialize(serializer),
            Self::Error { error } => {
                json!({ "kind": "error", "error": error }).serialize(serializer)
            }
            Self::MaxTokens => json!({ "kind": "max-tokens" }).serialize(serializer),
            Self::Interrupted => json!({ "kind": "interrupted" }).serialize(serializer),
            Self::Other { raw, .. } => raw.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TurnEndReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// One whole-list todo snapshot entry.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Portable todo lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

/// Logged request state for the next model request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EpochHeader {
    pub config: LlmCallConfig,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub adapter_defaults: Option<LlmCallConfigAdapterDefaults>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub system: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub tools: Option<Vec<ToolSchema>>,
}

impl EpochHeader {
    /// Return the model-request canonical form used by upstream projection.
    #[must_use]
    pub fn canonicalized(&self) -> Self {
        Self {
            config: self.config.clone(),
            adapter_defaults: self.adapter_defaults.as_ref().and_then(|defaults| {
                (defaults.reasoning_effort.is_some() || defaults.max_tokens.is_some())
                    .then(|| defaults.clone())
            }),
            system: self
                .system
                .as_ref()
                .filter(|value| !value.is_empty())
                .cloned(),
            tools: self
                .tools
                .as_ref()
                .filter(|value| !value.is_empty())
                .cloned(),
        }
    }

    /// Field-wise request equality used to suppress redundant full snapshots.
    #[must_use]
    pub fn equivalent_to(&self, other: &Self) -> bool {
        let first = self.canonicalized();
        let second = other.canonicalized();
        first.config.equivalent_to(&second.config)
            && first.adapter_defaults == second.adapter_defaults
            && first.system == second.system
            && first.tools == second.tools
    }

    fn validate(&self) -> Result<(), EventValidationError> {
        self.config.validate()?;
        if let Some(defaults) = &self.adapter_defaults {
            if defaults.reasoning_effort.is_some() && self.config.reasoning_effort().is_none() {
                return Err(crate::model::ModelError::InvalidAdapterDefault {
                    field: "reasoningEffort",
                }
                .into());
            }
            if defaults.max_tokens.is_some() && self.config.max_tokens().is_none() {
                return Err(
                    crate::model::ModelError::InvalidAdapterDefault { field: "maxTokens" }.into(),
                );
            }
        }
        Ok(())
    }
}

/// Why a full request header snapshot was appended.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RequestHeaderReason {
    Initial,
    Resume,
    Change,
    Other(String),
}

impl Serialize for RequestHeaderReason {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(match self {
            Self::Initial => "initial",
            Self::Resume => "resume",
            Self::Change => "change",
            Self::Other(reason) => reason,
        })
    }
}

impl<'de> Deserialize<'de> for RequestHeaderReason {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let reason = String::deserialize(deserializer)?;
        match reason.as_str() {
            "initial" => Ok(Self::Initial),
            "resume" => Ok(Self::Resume),
            "change" => Ok(Self::Change),
            // Upstream rejects this removed legacy vocabulary while allowing
            // future strings to survive an older reader.
            "fallback" => Err(de::Error::custom(
                "legacy request/header reason \"fallback\" is unsupported",
            )),
            _ => Ok(Self::Other(reason)),
        }
    }
}

/// Resolved route metadata for the next request, including retained extensions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestContext {
    provider: Option<String>,
    model: Option<String>,
    context_window: Option<NonNegativeSafeInteger>,
    raw: JsonValue,
}

impl RequestContext {
    /// Construct route metadata with an optional exact context-window size.
    pub fn new(
        provider: impl Into<String>,
        model: impl Into<String>,
        context_window: Option<NonNegativeSafeInteger>,
    ) -> Result<Self, crate::model::ModelError> {
        let provider = provider.into();
        let model = model.into();
        if provider.is_empty() {
            return Err(crate::model::ModelError::EmptyProvider);
        }
        if model.is_empty() {
            return Err(crate::model::ModelError::EmptyModel);
        }
        let mut value = json!({ "provider": provider, "model": model });
        if let Some(context_window) = context_window {
            value["contextWindow"] = Value::from(context_window.get());
        }
        Self::from_value(value)
    }

    /// Parse route metadata without discarding fields added by another producer.
    pub fn from_value(value: Value) -> Result<Self, crate::model::ModelError> {
        let raw = JsonValue::new(value)?;
        let fields =
            raw.as_value()
                .as_object()
                .ok_or_else(|| crate::model::ModelError::InvalidShape {
                    subject: "request context",
                    detail: "must be a JSON object".to_owned(),
                })?;
        // The typed live constructor above is strict. Imported upstream logs are
        // intentionally wider: the TS session core spreads and preserves this
        // record without validating provider/model/contextWindow.
        let provider = fields
            .get("provider")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let model = fields
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let context_window = fields
            .get("contextWindow")
            .cloned()
            .and_then(|value| serde_json::from_value(value).ok());
        Ok(Self {
            provider,
            model,
            context_window,
            raw,
        })
    }

    #[must_use]
    pub fn provider(&self) -> Option<&str> {
        self.provider.as_deref()
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub fn context_window(&self) -> Option<NonNegativeSafeInteger> {
        self.context_window
    }

    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }

    /// Typed route facts used to decide whether another full snapshot is needed.
    #[must_use]
    pub fn equivalent_to(&self, other: &Self) -> bool {
        self.provider == other.provider
            && self.model == other.model
            && self.context_window == other.context_window
    }
}

impl Serialize for RequestContext {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RequestContext {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::from_value(Value::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

/// Internal tool failure identity stored beside the model-facing result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ToolFailure {
    pub name: String,
    pub code: String,
}

/// Retry policy mode retained in one durable schedule event.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmRetryMode {
    Normal,
    Always,
}

/// One provider-routed retry scheduled after a failed model attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmRetryEvent {
    retry_id: RetryId,
    turn: TurnId,
    step: StepId,
    provider: String,
    mode: LlmRetryMode,
    policy_key: String,
    retry: RetryNumber,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    max_retries: Option<RetryNumber>,
    delay_ms: FiniteNumber,
    failure: LlmFailure,
}

impl LlmRetryEvent {
    #[allow(clippy::too_many_arguments)]
    pub fn normal(
        retry_id: RetryId,
        turn: TurnId,
        step: StepId,
        provider: impl Into<String>,
        policy_key: impl Into<String>,
        retry: RetryNumber,
        max_retries: RetryNumber,
        delay_ms: FiniteNumber,
        failure: LlmFailure,
    ) -> Result<Self, EventValidationError> {
        Self::new(
            retry_id,
            turn,
            step,
            provider.into(),
            LlmRetryMode::Normal,
            policy_key.into(),
            retry,
            Some(max_retries),
            delay_ms,
            failure,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn always(
        retry_id: RetryId,
        turn: TurnId,
        step: StepId,
        provider: impl Into<String>,
        policy_key: impl Into<String>,
        retry: RetryNumber,
        delay_ms: FiniteNumber,
        failure: LlmFailure,
    ) -> Result<Self, EventValidationError> {
        Self::new(
            retry_id,
            turn,
            step,
            provider.into(),
            LlmRetryMode::Always,
            policy_key.into(),
            retry,
            None,
            delay_ms,
            failure,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        retry_id: RetryId,
        turn: TurnId,
        step: StepId,
        provider: String,
        mode: LlmRetryMode,
        policy_key: String,
        retry: RetryNumber,
        max_retries: Option<RetryNumber>,
        delay_ms: FiniteNumber,
        failure: LlmFailure,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            retry_id,
            turn,
            step,
            provider,
            mode,
            policy_key,
            retry,
            max_retries,
            delay_ms,
            failure,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn retry_id(&self) -> &RetryId {
        &self.retry_id
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn step(&self) -> StepId {
        self.step
    }

    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn mode(&self) -> LlmRetryMode {
        self.mode
    }

    pub fn policy_key(&self) -> &str {
        &self.policy_key
    }

    pub fn retry(&self) -> RetryNumber {
        self.retry
    }

    pub fn max_retries(&self) -> Option<RetryNumber> {
        self.max_retries
    }

    pub fn delay_ms(&self) -> FiniteNumber {
        self.delay_ms
    }

    pub fn failure(&self) -> &LlmFailure {
        &self.failure
    }

    fn validate(&self) -> Result<(), EventValidationError> {
        if self.retry_id.is_empty() {
            return Err(EventValidationError::InvalidRetryEvent(
                "retryId must not be empty",
            ));
        }
        if self.provider.is_empty() {
            return Err(EventValidationError::InvalidRetryEvent(
                "provider must not be empty",
            ));
        }
        if self.policy_key.is_empty() {
            return Err(EventValidationError::InvalidRetryEvent(
                "policyKey must not be empty",
            ));
        }
        if !(0.0..=2_147_483_647.0).contains(&self.delay_ms.get()) {
            return Err(EventValidationError::InvalidRetryEvent(
                "delayMs must be inside the runtime timer range",
            ));
        }
        match (self.mode, self.max_retries) {
            (LlmRetryMode::Normal, Some(maximum)) if self.retry <= maximum => Ok(()),
            (LlmRetryMode::Normal, _) => Err(EventValidationError::InvalidRetryEvent(
                "normal retry must not exceed a positive maxRetries",
            )),
            (LlmRetryMode::Always, None) => Ok(()),
            (LlmRetryMode::Always, Some(_)) => Err(EventValidationError::InvalidRetryEvent(
                "always retry must omit maxRetries",
            )),
        }
    }
}

/// Wait-complete marker written immediately before the next provider attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmRetryStartedEvent {
    retry_id: RetryId,
    turn: TurnId,
    step: StepId,
    retry: RetryNumber,
}

impl LlmRetryStartedEvent {
    pub fn new(
        retry_id: RetryId,
        turn: TurnId,
        step: StepId,
        retry: RetryNumber,
    ) -> Result<Self, EventValidationError> {
        if retry_id.is_empty() {
            return Err(EventValidationError::InvalidRetryEvent(
                "retryId must not be empty",
            ));
        }
        Ok(Self {
            retry_id,
            turn,
            step,
            retry,
        })
    }

    pub fn retry_id(&self) -> &RetryId {
        &self.retry_id
    }

    pub fn turn(&self) -> TurnId {
        self.turn
    }

    pub fn step(&self) -> StepId {
        self.step
    }

    pub fn retry(&self) -> RetryNumber {
        self.retry
    }
}

/// One-shot answer to a durable approval question.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

/// Durable fact that the Agent asked one bounded approval question.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalAskedEvent {
    id: ApprovalRequestId,
    tool_name: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    call_id: Option<CallId>,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    reason: Option<String>,
}

impl ApprovalAskedEvent {
    pub fn new(
        id: ApprovalRequestId,
        tool_name: impl Into<String>,
        call_id: Option<CallId>,
        reason: Option<String>,
    ) -> Result<Self, EventValidationError> {
        let value = Self {
            id,
            tool_name: tool_name.into(),
            call_id,
            reason,
        };
        value.validate()?;
        Ok(value)
    }

    #[must_use]
    pub fn id(&self) -> &ApprovalRequestId {
        &self.id
    }

    #[must_use]
    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    #[must_use]
    pub fn call_id(&self) -> Option<&CallId> {
        self.call_id.as_ref()
    }

    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.reason.as_deref()
    }

    fn validate(&self) -> Result<(), EventValidationError> {
        validate_approval_id(&self.id)?;
        if self.tool_name.is_empty()
            || self.tool_name.len() > 256
            || self.tool_name.chars().any(char::is_control)
        {
            return Err(EventValidationError::InvalidApprovalEvent(
                "toolName must be 1 to 256 non-control UTF-8 bytes",
            ));
        }
        if self.call_id.as_ref().is_some_and(|call_id| {
            call_id.is_empty()
                || call_id.as_str().len() > 1_024
                || call_id.as_str().chars().any(char::is_control)
        }) {
            return Err(EventValidationError::InvalidApprovalEvent(
                "callId must be 1 to 1024 non-control UTF-8 bytes when present",
            ));
        }
        if self.reason.as_ref().is_some_and(|reason| {
            reason.len() > 4 * 1_024
                || reason
                    .chars()
                    .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        }) {
            return Err(EventValidationError::InvalidApprovalEvent(
                "reason must be at most 4096 UTF-8 bytes without unsafe controls",
            ));
        }
        Ok(())
    }
}

/// Durable answer paired with one preceding approval request.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecidedEvent {
    id: ApprovalRequestId,
    outcome: ApprovalOutcome,
}

impl ApprovalDecidedEvent {
    pub fn new(
        id: ApprovalRequestId,
        outcome: ApprovalOutcome,
    ) -> Result<Self, EventValidationError> {
        validate_approval_id(&id)?;
        Ok(Self { id, outcome })
    }

    #[must_use]
    pub fn id(&self) -> &ApprovalRequestId {
        &self.id
    }

    #[must_use]
    pub fn outcome(&self) -> ApprovalOutcome {
        self.outcome
    }
}

fn validate_approval_id(id: &ApprovalRequestId) -> Result<(), EventValidationError> {
    if id.is_empty() || id.as_str().len() > 1_024 || id.as_str().chars().any(char::is_control) {
        return Err(EventValidationError::InvalidApprovalEvent(
            "approval id must be 1 to 1024 non-control UTF-8 bytes",
        ));
    }
    Ok(())
}

/// A known core event payload or a retained unknown ignorable payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventKind {
    TurnStart {
        turn: TurnId,
    },
    TurnEnd {
        turn: TurnId,
        reason: TurnEndReason,
    },
    StepStart {
        turn: TurnId,
        step: StepId,
    },
    StepEnd {
        turn: TurnId,
        step: StepId,
    },
    UserMessage {
        message: Message,
    },
    AssistantChunk {
        turn: TurnId,
        step: StepId,
        chunk: StreamChunk,
    },
    AssistantMessage {
        turn: TurnId,
        step: StepId,
        message: Message,
        usage: Option<TokenUsage>,
    },
    ToolCall {
        turn: TurnId,
        step: StepId,
        call_id: CallId,
        name: String,
        arguments: String,
    },
    ToolResult {
        turn: TurnId,
        step: StepId,
        message: Message,
        error: Option<ToolFailure>,
        meta: Option<JsonValue>,
    },
    TodoWrite {
        todos: Vec<TodoItem>,
    },
    RequestHeader {
        header: EpochHeader,
        reason: RequestHeaderReason,
    },
    RequestContext {
        context: RequestContext,
    },
    LlmRetry {
        retry: LlmRetryEvent,
    },
    LlmRetryStarted {
        started: LlmRetryStartedEvent,
    },
    ApprovalAsked {
        asked: ApprovalAskedEvent,
    },
    ApprovalDecided {
        decided: ApprovalDecidedEvent,
    },
    CompactionStart {
        start: super::CompactionStartEvent,
    },
    CompactionSummary {
        summary: super::CompactionSummaryEvent,
    },
    CompactionEnd {
        end: super::CompactionEndEvent,
    },
    CompactionPrune {
        prune: super::CompactionPruneEvent,
    },
    EndSeed,
    /// An unknown envelope retained only because `ignorable: true` made it safe to skip.
    Unknown {
        event_type: String,
        data: JsonValue,
    },
}

impl EventKind {
    /// Construct a turn boundary.
    #[must_use]
    pub fn turn_start(turn: TurnId) -> Self {
        Self::TurnStart { turn }
    }

    /// Construct a completed turn boundary with its true reason.
    #[must_use]
    pub fn turn_end(turn: TurnId, reason: TurnEndReason) -> Self {
        Self::TurnEnd { turn, reason }
    }

    /// Construct a step boundary.
    #[must_use]
    pub fn step_start(turn: TurnId, step: StepId) -> Self {
        Self::StepStart { turn, step }
    }

    /// Construct a step close boundary.
    #[must_use]
    pub fn step_end(turn: TurnId, step: StepId) -> Self {
        Self::StepEnd { turn, step }
    }

    /// Construct a model-visible user event.
    #[must_use]
    pub fn user_message(message: Message) -> Self {
        Self::UserMessage { message }
    }

    /// Construct one raw assistant stream chunk.
    #[must_use]
    pub fn assistant_chunk(turn: TurnId, step: StepId, chunk: StreamChunk) -> Self {
        Self::AssistantChunk { turn, step, chunk }
    }

    /// Construct the assembled assistant message for a step.
    #[must_use]
    pub fn assistant_message(turn: TurnId, step: StepId, message: Message) -> Self {
        Self::AssistantMessage {
            turn,
            step,
            message,
            usage: None,
        }
    }

    /// Construct a durable tool-call intention with raw model arguments.
    #[must_use]
    pub fn tool_call(
        turn: TurnId,
        step: StepId,
        call_id: impl Into<CallId>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self::ToolCall {
            turn,
            step,
            call_id: call_id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Construct a completed model-visible tool result.
    #[must_use]
    pub fn tool_result(turn: TurnId, step: StepId, message: Message) -> Self {
        Self::ToolResult {
            turn,
            step,
            message,
            error: None,
            meta: None,
        }
    }

    #[must_use]
    pub fn llm_retry(retry: LlmRetryEvent) -> Self {
        Self::LlmRetry { retry }
    }

    #[must_use]
    pub fn llm_retry_started(started: LlmRetryStartedEvent) -> Self {
        Self::LlmRetryStarted { started }
    }

    #[must_use]
    pub fn approval_asked(asked: ApprovalAskedEvent) -> Self {
        Self::ApprovalAsked { asked }
    }

    #[must_use]
    pub fn approval_decided(decided: ApprovalDecidedEvent) -> Self {
        Self::ApprovalDecided { decided }
    }

    #[must_use]
    pub fn compaction_start(start: super::CompactionStartEvent) -> Self {
        Self::CompactionStart { start }
    }

    #[must_use]
    pub fn compaction_summary(summary: super::CompactionSummaryEvent) -> Self {
        Self::CompactionSummary { summary }
    }

    #[must_use]
    pub fn compaction_end(end: super::CompactionEndEvent) -> Self {
        Self::CompactionEnd { end }
    }

    #[must_use]
    pub fn compaction_prune(prune: super::CompactionPruneEvent) -> Self {
        Self::CompactionPrune { prune }
    }

    /// Stable wire tag for this event.
    #[must_use]
    pub fn event_type(&self) -> &str {
        match self {
            Self::TurnStart { .. } => "turn/start",
            Self::TurnEnd { .. } => "turn/end",
            Self::StepStart { .. } => "step/start",
            Self::StepEnd { .. } => "step/end",
            Self::UserMessage { .. } => "user/message",
            Self::AssistantChunk { .. } => "assistant/chunk",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
            Self::TodoWrite { .. } => "todo/write",
            Self::RequestHeader { .. } => "request/header",
            Self::RequestContext { .. } => "request/context",
            Self::LlmRetry { .. } => "llm/retry",
            Self::LlmRetryStarted { .. } => "llm/retry-started",
            Self::ApprovalAsked { .. } => "approval/asked",
            Self::ApprovalDecided { .. } => "approval/decided",
            Self::CompactionStart { .. } => "compaction/start",
            Self::CompactionSummary { .. } => "compaction/summary",
            Self::CompactionEnd { .. } => "compaction/end",
            Self::CompactionPrune { .. } => "compaction/prune",
            Self::EndSeed => "session/end-seed",
            Self::Unknown { event_type, .. } => event_type,
        }
    }

    pub(crate) fn live_event_type(&self) -> Option<&'static str> {
        Some(match self {
            Self::TurnStart { .. } => "turn/start",
            Self::TurnEnd { .. } => "turn/end",
            Self::StepStart { .. } => "step/start",
            Self::StepEnd { .. } => "step/end",
            Self::UserMessage { .. } => "user/message",
            Self::AssistantChunk { .. } => "assistant/chunk",
            Self::AssistantMessage { .. } => "assistant/message",
            Self::ToolCall { .. } => "tool/call",
            Self::ToolResult { .. } => "tool/result",
            Self::TodoWrite { .. } => "todo/write",
            Self::RequestHeader { .. } => "request/header",
            Self::RequestContext { .. } => "request/context",
            Self::LlmRetry { .. } => "llm/retry",
            Self::LlmRetryStarted { .. } => "llm/retry-started",
            Self::ApprovalAsked { .. } => "approval/asked",
            Self::ApprovalDecided { .. } => "approval/decided",
            Self::CompactionStart { .. } => "compaction/start",
            Self::CompactionSummary { .. } => "compaction/summary",
            Self::CompactionEnd { .. } => "compaction/end",
            Self::CompactionPrune { .. } => "compaction/prune",
            Self::EndSeed => "session/end-seed",
            Self::Unknown { .. } => return None,
        })
    }

    #[must_use]
    pub(crate) fn is_surface_eligible(&self) -> bool {
        matches!(
            self,
            Self::UserMessage { .. } | Self::AssistantMessage { .. } | Self::ToolResult { .. }
        )
    }

    pub(crate) fn validate(&self) -> Result<(), EventValidationError> {
        match self {
            Self::TurnEnd { reason, .. } => reason.validate_canonical()?,
            Self::UserMessage { message } => message.validate_user_event()?,
            Self::AssistantMessage { message, .. } => message.validate_assistant_event()?,
            Self::ToolResult { message, .. } => {
                message.validate_tool_result()?;
            }
            Self::RequestHeader { header, reason } => {
                header.validate()?;
                if let RequestHeaderReason::Other(value) = reason {
                    match value.as_str() {
                        "fallback" => {
                            return Err(EventValidationError::LegacyRequestHeaderReason);
                        }
                        "initial" | "resume" | "change" => {
                            return Err(EventValidationError::NonCanonicalRequestHeaderReason {
                                reason: value.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Self::RequestContext { .. } => {}
            Self::LlmRetry { retry } => retry.validate()?,
            Self::LlmRetryStarted { started } if started.retry_id.is_empty() => {
                return Err(EventValidationError::InvalidRetryEvent(
                    "retryId must not be empty",
                ));
            }
            Self::ApprovalAsked { asked } => asked.validate()?,
            Self::ApprovalDecided { decided } => validate_approval_id(&decided.id)?,
            Self::CompactionStart { start } => start.validate()?,
            Self::CompactionSummary { summary } => summary.validate()?,
            Self::CompactionEnd { end } => end.validate()?,
            Self::CompactionPrune { prune } => prune.validate()?,
            // These values can only be created through their bounded, validated
            // constructors, so append has no second placeholder validation pass.
            Self::AssistantChunk { .. } | Self::Unknown { .. } => {}
            _ => {}
        }
        Ok(())
    }
}

/// How a message-producing event changes the model-visible surface.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SurfaceOp {
    Append(SurfaceAppend),
    Replace(SurfaceReplace),
}

/// Wire value for the canonical string `"append"`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SurfaceAppend;

impl Serialize for SurfaceAppend {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str("append")
    }
}

impl<'de> Deserialize<'de> for SurfaceAppend {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        if value == "append" {
            Ok(Self)
        } else {
            Err(de::Error::custom(
                "surface append marker must be \"append\"",
            ))
        }
    }
}

/// Inclusive current-surface range replaced by one new node.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceReplace {
    op: ReplaceMarker,
    pub start: EventSeq,
    pub end: EventSeq,
}

/// Wire value for the canonical string `"replace"`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename = "replace")]
enum ReplaceMarker {
    #[serde(rename = "replace")]
    Replace,
}

impl SurfaceOp {
    /// Append one message event to the surface tail.
    #[must_use]
    pub fn append() -> Self {
        Self::Append(SurfaceAppend)
    }

    /// Replace an inclusive range of current surface event sequences.
    #[must_use]
    pub fn replace(start: EventSeq, end: EventSeq) -> Self {
        Self::Replace(SurfaceReplace {
            op: ReplaceMarker::Replace,
            start,
            end,
        })
    }
}

/// Surface placement and provenance supplied with a new event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceIntent {
    pub(super) operation: SurfaceOp,
    pub(super) source_event_seqs: Option<Vec<EventSeq>>,
}

impl SurfaceIntent {
    /// Mark an ordinary message-producing append.
    #[must_use]
    pub fn append() -> Self {
        Self {
            operation: SurfaceOp::append(),
            source_event_seqs: None,
        }
    }

    /// Mark one inclusive positional replacement.
    #[must_use]
    pub fn replace(start: EventSeq, end: EventSeq, sources: Vec<EventSeq>) -> Self {
        Self {
            operation: SurfaceOp::replace(start, end),
            source_event_seqs: Some(sources),
        }
    }

    /// Record the complete known provenance for an append.
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<EventSeq>) -> Self {
        self.source_event_seqs = Some(sources);
        self
    }
}

/// Event input before the session assigns sequence and time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewEvent {
    pub(super) kind: EventKind,
    pub(super) surface: Option<SurfaceIntent>,
}

impl NewEvent {
    /// Construct a log-only event with no surface metadata.
    #[must_use]
    pub fn log(kind: EventKind) -> Self {
        Self {
            kind,
            surface: None,
        }
    }

    /// Construct a message-producing event with explicit surface placement.
    #[must_use]
    pub fn surface(kind: EventKind, intent: SurfaceIntent) -> Self {
        Self {
            kind,
            surface: Some(intent),
        }
    }
}

/// One immutable entry in the session event log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionEvent {
    pub(crate) seq: EventSeq,
    pub(crate) time: UnixMillis,
    pub(crate) kind: EventKind,
    pub(crate) surface_op: Option<SurfaceOp>,
    pub(crate) source_event_seqs: Option<Vec<EventSeq>>,
    pub(crate) ignorable: Option<TrueMarker>,
    /// Exact validated payload admitted at an import boundary, including extensions.
    pub(crate) original_data: JsonValue,
}

impl SessionEvent {
    /// Zero-based contiguous position in the session log.
    #[must_use]
    pub fn seq(&self) -> EventSeq {
        self.seq
    }

    /// Unix-epoch millisecond timestamp.
    #[must_use]
    pub fn time(&self) -> UnixMillis {
        self.time
    }

    /// Typed event payload and wire tag.
    #[must_use]
    pub fn kind(&self) -> &EventKind {
        &self.kind
    }

    /// Model-visible surface operation, when this is a surface event.
    #[must_use]
    pub fn surface_op(&self) -> Option<&SurfaceOp> {
        self.surface_op.as_ref()
    }

    /// Earlier source events cited by this event.
    #[must_use]
    pub fn source_event_seqs(&self) -> Option<&[EventSeq]> {
        self.source_event_seqs.as_deref()
    }

    /// Whether an older reader may safely skip an unknown type.
    #[must_use]
    pub fn is_ignorable(&self) -> bool {
        self.ignorable.is_some()
    }

    /// Complete validated event payload, including fields unknown to this build.
    #[must_use]
    pub fn data(&self) -> &JsonValue {
        &self.original_data
    }

    pub(crate) fn from_new(
        seq: EventSeq,
        time: UnixMillis,
        event: NewEvent,
        original_data: JsonValue,
    ) -> Self {
        let (surface_op, source_event_seqs) = match event.surface {
            Some(intent) => (Some(intent.operation), intent.source_event_seqs),
            None => (None, None),
        };
        Self {
            seq,
            time,
            kind: event.kind,
            surface_op,
            source_event_seqs,
            ignorable: None,
            original_data,
        }
    }

    pub(crate) fn set_time_for_commit(&mut self, time: UnixMillis) {
        self.time = time;
    }
}
