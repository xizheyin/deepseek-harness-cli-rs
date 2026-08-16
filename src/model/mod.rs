//! Provider-neutral messages and model-stream vocabulary.

mod stream;

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::json_value::serialize_js_number;
pub use crate::json_value::{
    JsonValue, JsonValueError, MAX_JSON_DEPTH, MAX_JSON_NODES, MAX_JSON_VALUE_BYTES,
    NonNegativeSafeInteger, PositiveFiniteNumber,
};
pub(crate) use stream::PreparedStreamTransition;
pub use stream::{MAX_PROVIDER_STREAM_CHUNKS, StreamProtocolError, StreamValidator};

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Brand an opaque string without changing it.
            #[must_use]
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the original opaque value.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Return whether this identifier is empty.
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

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::new(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::new(value)
            }
        }
    };
}

string_id!(
    /// Stable identity carried by one message across log and model boundaries.
    MessageId
);
string_id!(
    /// Correlates one model-issued tool call with its result.
    CallId
);
string_id!(
    /// Provider-issued request identity retained for diagnostics.
    ProviderRequestId
);
string_id!(
    /// Adapter-owned reasoning-effort identity.
    ReasoningEffortId
);
string_id!(
    /// Opaque durable identity for an image attachment.
    AttachmentId
);

/// Why one provider-neutral model call is being made.
///
/// This value lives in the model layer because a durable compaction dispatch
/// records it without depending on a concrete Provider implementation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestPurpose {
    /// Normal conversation or tool continuation.
    #[default]
    Conversation,
    /// Short auxiliary title generation.
    SessionTitle,
    /// Context-compaction generation.
    Compaction,
}

/// A finite JSON number other than negative zero.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct FiniteNumber(f64);

impl FiniteNumber {
    /// Validate one number against the lossless JSON domain used by the session log.
    pub fn new(value: f64) -> Result<Self, ModelError> {
        if !value.is_finite() || (value == 0.0 && value.is_sign_negative()) {
            return Err(ModelError::InvalidJsonNumber);
        }
        Ok(Self(value))
    }

    /// Return the validated numeric value.
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

// Every constructor and deserializer excludes NaN, so equality is reflexive.
impl Eq for FiniteNumber {}

impl Serialize for FiniteNumber {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_js_number(self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for FiniteNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// The only legal value for an optional marker whose presence means `true`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TrueMarker;

impl Serialize for TrueMarker {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bool(true)
    }
}

impl<'de> Deserialize<'de> for TrueMarker {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(de::Error::custom("marker must be true when present"))
        }
    }
}

/// Errors raised while constructing or validating provider-neutral values.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ModelError {
    /// A persisted message must have a stable non-empty identity.
    #[error("message id must not be empty")]
    EmptyMessageId,
    /// An assistant message must identify its provider route.
    #[error("model provider must not be empty")]
    EmptyProvider,
    /// An assistant message must identify its provider-owned model.
    #[error("model id must not be empty")]
    EmptyModel,
    /// A durable optional identifier cannot be present but empty.
    #[error("{field} must not be empty when present")]
    EmptyOptionalId { field: &'static str },
    /// A logged adapter-default marker has no corresponding config value.
    #[error("adapter default {field} requires the corresponding config value")]
    InvalidAdapterDefault { field: &'static str },
    /// Plugin context fields do not match the selected form.
    #[error("plugin context form and detail fields do not match")]
    InvalidContextForm,
    /// A tool-result message does not have the required role/source/block relationship.
    #[error("tool-result message must contain exactly one matching tool-result block")]
    InvalidToolResult,
    /// An event expected one specific message role/source combination.
    #[error("message does not satisfy the {expected} event shape")]
    WrongMessageShape { expected: &'static str },
    /// JSON persistence would change this numeric value.
    #[error("JSON numbers must be finite and must not be negative zero")]
    InvalidJsonNumber,
    /// A structured value has the wrong minimum wire shape.
    #[error("invalid {subject}: {detail}")]
    InvalidShape {
        subject: &'static str,
        detail: String,
    },
    /// LLM failure facts violate the provider-neutral contract.
    #[error("invalid LLM failure: {0}")]
    InvalidFailure(&'static str),
    /// A message contains more blocks than the bounded core accepts.
    #[error("message contains {actual} blocks; maximum is {maximum}")]
    TooManyContentBlocks { maximum: usize, actual: usize },
    /// A bounded opaque JSON value was invalid.
    #[error(transparent)]
    Json(#[from] JsonValueError),
}

/// Maximum number of top-level blocks retained in one message.
pub const MAX_MESSAGE_CONTENT_BLOCKS: usize = 4_096;
mod llm;
mod message;

pub use llm::*;
pub use message::*;

fn object<'a>(
    value: &'a Value,
    subject: &'static str,
) -> Result<&'a Map<String, Value>, ModelError> {
    value
        .as_object()
        .ok_or_else(|| shape(subject, "must be a JSON object"))
}

fn required_string(
    fields: &Map<String, Value>,
    key: &'static str,
    subject: &'static str,
) -> Result<String, ModelError> {
    fields
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| shape(subject, format!("{key} must be a string")))
}

fn optional_string(
    fields: &Map<String, Value>,
    key: &'static str,
    subject: &'static str,
) -> Result<Option<String>, ModelError> {
    match fields.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(shape(
            subject,
            format!("{key} must be a string when present"),
        )),
    }
}

fn required_typed<T: for<'de> Deserialize<'de>>(
    fields: &Map<String, Value>,
    key: &'static str,
    subject: &'static str,
) -> Result<T, ModelError> {
    let value = fields
        .get(key)
        .cloned()
        .ok_or_else(|| shape(subject, format!("missing {key}")))?;
    serde_json::from_value(value).map_err(|error| shape(subject, format!("invalid {key}: {error}")))
}

fn optional_typed<T: for<'de> Deserialize<'de>>(
    fields: &Map<String, Value>,
    key: &'static str,
    subject: &'static str,
) -> Result<Option<T>, ModelError> {
    fields
        .get(key)
        .cloned()
        .map(|value| {
            serde_json::from_value(value)
                .map_err(|error| shape(subject, format!("invalid {key}: {error}")))
        })
        .transpose()
}

fn optional_json(
    fields: &Map<String, Value>,
    key: &'static str,
) -> Result<Option<JsonValue>, ModelError> {
    fields
        .get(key)
        .cloned()
        .map(JsonValue::new)
        .transpose()
        .map_err(ModelError::from)
}

fn shape(subject: &'static str, detail: impl Into<String>) -> ModelError {
    ModelError::InvalidShape {
        subject,
        detail: detail.into(),
    }
}
