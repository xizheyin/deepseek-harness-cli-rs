//! Messages, content blocks, and merge-extensible provenance.

use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use serde_json::{Value, json};

use crate::json_value::deserialize_present_option;

use super::{
    AttachmentId, CallId, JsonValue, MAX_MESSAGE_CONTENT_BLOCKS, MessageId, ModelError,
    NonNegativeSafeInteger, object, optional_json, optional_string, optional_typed,
    required_string, shape,
};

/// Raster image formats accepted by the canonical attachment reference.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ImageMediaType {
    #[serde(rename = "image/png")]
    Png,
    #[serde(rename = "image/jpeg")]
    Jpeg,
    #[serde(rename = "image/webp")]
    Webp,
    #[serde(rename = "image/gif")]
    Gif,
}

/// Durable metadata for one immutable raster image.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachmentRef {
    attachment_id: AttachmentId,
    media_type: ImageMediaType,
    bytes: NonNegativeSafeInteger,
    width: NonNegativeSafeInteger,
    height: NonNegativeSafeInteger,
    #[serde(
        default,
        deserialize_with = "deserialize_present_option",
        skip_serializing_if = "Option::is_none"
    )]
    name: Option<String>,
}

impl ImageAttachmentRef {
    /// Build validated immutable image metadata.
    pub fn new(
        attachment_id: impl Into<AttachmentId>,
        media_type: ImageMediaType,
        bytes: u64,
        width: u64,
        height: u64,
        name: Option<String>,
    ) -> Result<Self, ModelError> {
        Ok(Self {
            attachment_id: attachment_id.into(),
            media_type,
            bytes: NonNegativeSafeInteger::new(bytes)?,
            width: NonNegativeSafeInteger::new(width)?,
            height: NonNegativeSafeInteger::new(height)?,
            name,
        })
    }

    /// Stable attachment identity.
    #[must_use]
    pub fn attachment_id(&self) -> &AttachmentId {
        &self.attachment_id
    }

    /// Verified raster media type.
    #[must_use]
    pub fn media_type(&self) -> ImageMediaType {
        self.media_type
    }

    /// Exact encoded byte length.
    #[must_use]
    pub fn bytes(&self) -> NonNegativeSafeInteger {
        self.bytes
    }

    /// Intrinsic width in pixels.
    #[must_use]
    pub fn width(&self) -> NonNegativeSafeInteger {
        self.width
    }

    /// Intrinsic height in pixels.
    #[must_use]
    pub fn height(&self) -> NonNegativeSafeInteger {
        self.height
    }

    /// Optional display name, never a filesystem path.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

/// Parsed facts for a content block. The original bounded JSON remains authoritative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentBlockKind {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Image {
        attachment: ImageAttachmentRef,
    },
    ToolCall {
        id: CallId,
        name: String,
        arguments: String,
    },
    ToolResult {
        tool_call_id: CallId,
        is_error: Option<bool>,
    },
    Other {
        block_type: Option<String>,
    },
}

/// One provider-neutral block with lossless support for plugin-added shapes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentBlock {
    kind: ContentBlockKind,
    raw: JsonValue,
}

impl ContentBlock {
    /// Construct a plain-text content block.
    pub fn text(text: impl Into<String>) -> Result<Self, ModelError> {
        Self::from_value(json!({ "type": "text", "text": text.into() }))
    }

    /// Construct a reasoning content block.
    pub fn reasoning(text: impl Into<String>) -> Result<Self, ModelError> {
        Self::from_value(json!({ "type": "reasoning", "text": text.into() }))
    }

    /// Construct an immutable image reference block.
    pub fn image(attachment: ImageAttachmentRef) -> Result<Self, ModelError> {
        Self::from_value(json!({ "type": "image", "attachment": attachment }))
    }

    /// Construct a model-requested tool call while retaining raw JSON arguments.
    pub fn tool_call(
        id: impl Into<CallId>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Result<Self, ModelError> {
        Self::from_value(json!({
            "type": "tool-call",
            "id": id.into(),
            "name": name.into(),
            "arguments": arguments.into(),
        }))
    }

    /// Construct a correlated tool-result block.
    pub fn tool_result(
        call_id: impl Into<CallId>,
        content: Vec<ContentBlock>,
        is_error: Option<bool>,
    ) -> Result<Self, ModelError> {
        let content = content
            .into_iter()
            .map(|block| block.raw.into_value())
            .collect::<Vec<_>>();
        let mut value = json!({
            "type": "tool-result",
            "toolCallId": call_id.into(),
            "content": content,
        });
        if let Some(is_error) = is_error {
            value["isError"] = Value::Bool(is_error);
        }
        Self::from_value(value)
    }

    /// Parse a plugin-extensible block without discarding unknown fields.
    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let kind = parse_content_block(raw.as_value());
        Ok(Self { kind, raw })
    }

    /// Known facts, or `Other` for a plugin/forward-compatible block.
    #[must_use]
    pub fn kind(&self) -> &ContentBlockKind {
        &self.kind
    }

    /// Exact bounded JSON supplied by the producer.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }

    /// Nested model-facing blocks for a tool result, when this is one.
    #[must_use]
    pub fn tool_result_content(&self) -> Option<&[Value]> {
        if !matches!(self.kind, ContentBlockKind::ToolResult { .. }) {
            return None;
        }
        self.raw
            .as_value()
            .get("content")?
            .as_array()
            .map(Vec::as_slice)
    }
}

impl Serialize for ContentBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ContentBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(de::Error::custom)
    }
}

/// Provider-neutral conversation role.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MessageRole {
    System,
    User,
    Assistant,
}

/// Semantic form of plugin-supplied context.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextForm {
    Instructions,
    Catalog,
    Snapshot,
    Notice,
    Relay,
    Recall,
}

/// One named contribution to snapshot-form context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContextSnapshotSection {
    pub name: String,
    pub text: String,
}

/// Parsed source facts. Unknown kinds remain model-visible and round-trip unchanged.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageSourceKind {
    User,
    Plugin {
        plugin: String,
        form: Option<ContextForm>,
        sections: Option<Vec<ContextSnapshotSection>>,
        summary: Option<String>,
    },
    Model {
        provider: String,
        model: String,
        replay_state: Option<JsonValue>,
    },
    Tool {
        call_id: CallId,
    },
    Other {
        kind: String,
    },
}

/// Where a message came from, including lossless plugin extensions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageSource {
    kind: MessageSourceKind,
    raw: JsonValue,
}

impl MessageSource {
    /// Source for a direct human prompt.
    pub fn user() -> Result<Self, ModelError> {
        Self::from_value(json!({ "kind": "user" }))
    }

    /// Source for opaque context contributed by a named subsystem.
    pub fn plugin(plugin: impl Into<String>) -> Result<Self, ModelError> {
        Self::from_value(json!({ "kind": "plugin", "plugin": plugin.into() }))
    }

    /// Source for snapshot-form plugin context.
    pub fn plugin_snapshot(
        plugin: impl Into<String>,
        sections: Vec<ContextSnapshotSection>,
    ) -> Result<Self, ModelError> {
        Self::from_value(json!({
            "kind": "plugin",
            "plugin": plugin.into(),
            "form": "snapshot",
            "sections": sections,
        }))
    }

    /// Source for one notice-form plugin message.
    pub fn plugin_notice(
        plugin: impl Into<String>,
        summary: impl Into<String>,
    ) -> Result<Self, ModelError> {
        Self::from_value(json!({
            "kind": "plugin",
            "plugin": plugin.into(),
            "form": "notice",
            "summary": summary.into(),
        }))
    }

    /// Source for a routed model response.
    pub fn model(
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ModelError> {
        Self::model_with_replay_state(provider, model, None)
    }

    /// Source for a routed model response with optional adapter replay state.
    pub fn model_with_replay_state(
        provider: impl Into<String>,
        model: impl Into<String>,
        replay_state: Option<JsonValue>,
    ) -> Result<Self, ModelError> {
        let provider = provider.into();
        let model = model.into();
        if provider.is_empty() {
            return Err(ModelError::EmptyProvider);
        }
        if model.is_empty() {
            return Err(ModelError::EmptyModel);
        }
        let mut value = json!({
            "kind": "model",
            "provider": provider,
            "model": model,
        });
        if let Some(replay_state) = replay_state {
            value["replayState"] = replay_state.into_value();
        }
        Self::from_value(value)
    }

    /// Source for a tool result.
    pub fn tool(call_id: impl Into<CallId>) -> Result<Self, ModelError> {
        let call_id = call_id.into();
        if call_id.is_empty() {
            return Err(ModelError::InvalidToolResult);
        }
        Self::from_value(json!({ "kind": "tool", "callId": call_id }))
    }

    /// Parse a merge-extensible source record without discarding unknown fields.
    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let kind = parse_message_source(raw.as_value())?;
        Ok(Self { kind, raw })
    }

    /// Parsed source facts.
    #[must_use]
    pub fn kind(&self) -> &MessageSourceKind {
        &self.kind
    }

    /// Exact bounded JSON supplied by the producer.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.raw
    }
}

impl Serialize for MessageSource {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MessageSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(de::Error::custom)
    }
}

/// One immutable message shared by delivery, durable history, and model requests.
///
/// Cloning a message is intentionally shallow. Phase 8 needs the same bounded
/// payload to be owned by the active Session surface, a provider request, and a
/// turn outcome without allocating another complete JSON tree for every owner.
#[derive(Clone)]
pub struct Message {
    inner: Arc<MessageInner>,
}

#[derive(Debug, Eq, PartialEq)]
struct MessageInner {
    id: MessageId,
    role: MessageRole,
    content: Vec<ContentBlock>,
    source: MessageSource,
    raw: JsonValue,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl PartialEq for Message {
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for Message {}

impl Message {
    /// Construct and validate a message with an explicit role and source.
    pub fn new(
        id: impl Into<MessageId>,
        role: MessageRole,
        content: Vec<ContentBlock>,
        source: MessageSource,
    ) -> Result<Self, ModelError> {
        let id = id.into();
        if id.is_empty() {
            return Err(ModelError::EmptyMessageId);
        }
        if content.len() > MAX_MESSAGE_CONTENT_BLOCKS {
            return Err(ModelError::TooManyContentBlocks {
                maximum: MAX_MESSAGE_CONTENT_BLOCKS,
                actual: content.len(),
            });
        }
        let value = json!({
            "id": id,
            "role": role,
            "content": content,
            "source": source,
        });
        let raw = JsonValue::new(value)?;
        Ok(Self::from_parts(id, role, content, source, raw))
    }

    /// Construct a direct or injected user-role message.
    pub fn user(
        id: impl Into<MessageId>,
        content: Vec<ContentBlock>,
        source: MessageSource,
    ) -> Result<Self, ModelError> {
        Self::new(id, MessageRole::User, content, source)
    }

    /// Construct a model-produced assistant message.
    pub fn assistant(
        id: impl Into<MessageId>,
        content: Vec<ContentBlock>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Result<Self, ModelError> {
        Self::new(
            id,
            MessageRole::Assistant,
            content,
            MessageSource::model(provider, model)?,
        )
    }

    /// Construct a user-role message containing one correlated tool result.
    pub fn tool_result(
        id: impl Into<MessageId>,
        call_id: impl Into<CallId>,
        content: Vec<ContentBlock>,
        is_error: bool,
    ) -> Result<Self, ModelError> {
        let call_id = call_id.into();
        Self::new(
            id,
            MessageRole::User,
            vec![ContentBlock::tool_result(
                call_id.clone(),
                content,
                Some(is_error),
            )?],
            MessageSource::tool(call_id)?,
        )
    }

    /// Parse an identified message while retaining plugin-added fields and blocks.
    pub fn from_value(value: Value) -> Result<Self, ModelError> {
        let raw = JsonValue::new(value)?;
        let fields = object(raw.as_value(), "message")?;
        let id = required_string(fields, "id", "message")?;
        if id.is_empty() {
            return Err(ModelError::EmptyMessageId);
        }
        let role = serde_json::from_value(
            fields
                .get("role")
                .cloned()
                .ok_or_else(|| shape("message", "missing role"))?,
        )
        .map_err(|error| shape("message", error.to_string()))?;
        let content_values = fields
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| shape("message", "content must be an array"))?;
        if content_values.len() > MAX_MESSAGE_CONTENT_BLOCKS {
            return Err(ModelError::TooManyContentBlocks {
                maximum: MAX_MESSAGE_CONTENT_BLOCKS,
                actual: content_values.len(),
            });
        }
        let content = content_values
            .iter()
            .cloned()
            .map(ContentBlock::from_value)
            .collect::<Result<Vec<_>, _>>()?;
        let source = MessageSource::from_value(
            fields
                .get("source")
                .cloned()
                .ok_or_else(|| shape("message", "missing source"))?,
        )?;
        Ok(Self::from_parts(
            MessageId::new(id),
            role,
            content,
            source,
            raw,
        ))
    }

    fn from_parts(
        id: MessageId,
        role: MessageRole,
        content: Vec<ContentBlock>,
        source: MessageSource,
        raw: JsonValue,
    ) -> Self {
        Self {
            inner: Arc::new(MessageInner {
                id,
                role,
                content,
                source,
                raw,
            }),
        }
    }

    /// Stable message identity.
    #[must_use]
    pub fn id(&self) -> &MessageId {
        &self.inner.id
    }

    /// Provider-neutral role.
    #[must_use]
    pub fn role(&self) -> MessageRole {
        self.inner.role
    }

    /// Ordered model-facing content.
    #[must_use]
    pub fn content(&self) -> &[ContentBlock] {
        &self.inner.content
    }

    /// Producer identity and provenance.
    #[must_use]
    pub fn source(&self) -> &MessageSource {
        &self.inner.source
    }

    /// Exact bounded JSON, including plugin-added fields.
    #[must_use]
    pub fn raw(&self) -> &JsonValue {
        &self.inner.raw
    }

    pub(crate) fn validate_tool_result(&self) -> Result<&CallId, ModelError> {
        if self.inner.role != MessageRole::User {
            return Err(ModelError::InvalidToolResult);
        }
        let MessageSourceKind::Tool { call_id } = self.inner.source.kind() else {
            return Err(ModelError::InvalidToolResult);
        };
        let [block] = self.inner.content.as_slice() else {
            return Err(ModelError::InvalidToolResult);
        };
        let ContentBlockKind::ToolResult { tool_call_id, .. } = block.kind() else {
            return Err(ModelError::InvalidToolResult);
        };
        if call_id.is_empty() || tool_call_id != call_id {
            return Err(ModelError::InvalidToolResult);
        }
        Ok(call_id)
    }

    pub(crate) fn tool_result_is_error(&self) -> bool {
        matches!(
            self.inner.content.as_slice(),
            [block]
                if matches!(
                    block.kind(),
                    ContentBlockKind::ToolResult { is_error: Some(true), .. }
                )
        )
    }

    pub(crate) fn validate_user_event(&self) -> Result<(), ModelError> {
        if self.inner.role != MessageRole::User {
            return Err(ModelError::WrongMessageShape { expected: "user" });
        }
        Ok(())
    }

    pub(crate) fn validate_assistant_event(&self) -> Result<(), ModelError> {
        let MessageSourceKind::Model {
            provider, model, ..
        } = self.inner.source.kind()
        else {
            return Err(ModelError::WrongMessageShape {
                expected: "assistant/model",
            });
        };
        if self.inner.role != MessageRole::Assistant {
            return Err(ModelError::WrongMessageShape {
                expected: "assistant/model",
            });
        }
        if provider.is_empty() {
            return Err(ModelError::EmptyProvider);
        }
        if model.is_empty() {
            return Err(ModelError::EmptyModel);
        }
        Ok(())
    }
}

impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.inner.raw.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Message {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(de::Error::custom)
    }
}

fn parse_content_block(value: &Value) -> ContentBlockKind {
    let Some(fields) = value.as_object() else {
        return ContentBlockKind::Other { block_type: None };
    };
    let block_type = fields.get("type").and_then(Value::as_str);
    match block_type {
        Some("text") => fields
            .get("text")
            .and_then(Value::as_str)
            .map(|text| ContentBlockKind::Text {
                text: text.to_owned(),
            })
            .unwrap_or_else(|| ContentBlockKind::Other {
                block_type: Some("text".to_owned()),
            }),
        Some("reasoning") => fields
            .get("text")
            .and_then(Value::as_str)
            .map(|text| ContentBlockKind::Reasoning {
                text: text.to_owned(),
            })
            .unwrap_or_else(|| ContentBlockKind::Other {
                block_type: Some("reasoning".to_owned()),
            }),
        Some("image") => fields
            .get("attachment")
            .cloned()
            .and_then(|attachment| serde_json::from_value(attachment).ok())
            .map(|attachment| ContentBlockKind::Image { attachment })
            .unwrap_or_else(|| ContentBlockKind::Other {
                block_type: Some("image".to_owned()),
            }),
        Some("tool-call") => match (
            fields.get("id").and_then(Value::as_str),
            fields.get("name").and_then(Value::as_str),
            fields.get("arguments").and_then(Value::as_str),
        ) {
            (Some(id), Some(name), Some(arguments)) => ContentBlockKind::ToolCall {
                id: CallId::new(id),
                name: name.to_owned(),
                arguments: arguments.to_owned(),
            },
            _ => ContentBlockKind::Other {
                block_type: Some("tool-call".to_owned()),
            },
        },
        Some("tool-result") => match (
            fields.get("toolCallId").and_then(Value::as_str),
            fields.get("content").and_then(Value::as_array),
        ) {
            (Some(call_id), Some(_)) => ContentBlockKind::ToolResult {
                tool_call_id: CallId::new(call_id),
                // Upstream deliberately validates only the model-visible shell. A
                // plugin may retain `null` or a future value here; expose a bool
                // only when one is actually present and keep the raw value either way.
                is_error: fields.get("isError").and_then(Value::as_bool),
            },
            _ => ContentBlockKind::Other {
                block_type: Some("tool-result".to_owned()),
            },
        },
        other => ContentBlockKind::Other {
            block_type: other.map(str::to_owned),
        },
    }
}

fn parse_message_source(value: &Value) -> Result<MessageSourceKind, ModelError> {
    let fields = object(value, "message source")?;
    let tag = required_string(fields, "kind", "message source")?;
    if tag.is_empty() {
        return Err(shape("message source", "kind must not be empty"));
    }
    Ok(match tag.as_str() {
        "user" => MessageSourceKind::User,
        "plugin" => {
            let parsed = (|| {
                let plugin = required_string(fields, "plugin", "message source")?;
                let form = optional_typed(fields, "form", "message source")?;
                let sections = optional_typed(fields, "sections", "message source")?;
                let summary = optional_string(fields, "summary", "message source")?;
                let valid = match form {
                    None => sections.is_none() && summary.is_none(),
                    Some(ContextForm::Snapshot) => sections.is_some() && summary.is_none(),
                    Some(ContextForm::Notice) => sections.is_none() && summary.is_some(),
                    Some(
                        ContextForm::Instructions
                        | ContextForm::Catalog
                        | ContextForm::Relay
                        | ContextForm::Recall,
                    ) => sections.is_none() && summary.is_none(),
                };
                valid
                    .then_some(MessageSourceKind::Plugin {
                        plugin,
                        form,
                        sections,
                        summary,
                    })
                    .ok_or(ModelError::InvalidContextForm)
            })();
            parsed.unwrap_or_else(|_| MessageSourceKind::Other { kind: tag.clone() })
        }
        "model" => {
            let parsed = (|| {
                let provider = required_string(fields, "provider", "message source")?;
                let model = required_string(fields, "model", "message source")?;
                if provider.is_empty() {
                    return Err(ModelError::EmptyProvider);
                }
                if model.is_empty() {
                    return Err(ModelError::EmptyModel);
                }
                Ok(MessageSourceKind::Model {
                    provider,
                    model,
                    replay_state: optional_json(fields, "replayState")?,
                })
            })();
            parsed.unwrap_or_else(|_| MessageSourceKind::Other { kind: tag.clone() })
        }
        "tool" => {
            let parsed = required_string(fields, "callId", "message source")
                .map(CallId::new)
                .and_then(|call_id| {
                    (!call_id.is_empty())
                        .then_some(MessageSourceKind::Tool { call_id })
                        .ok_or(ModelError::InvalidToolResult)
                });
            parsed.unwrap_or_else(|_| MessageSourceKind::Other { kind: tag.clone() })
        }
        _ => MessageSourceKind::Other { kind: tag },
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{ContentBlock, Message, MessageSource};

    #[test]
    fn message_clone_shares_one_immutable_payload() {
        let message = Message::user(
            "message-1",
            vec![ContentBlock::text("x".repeat(1024)).unwrap()],
            MessageSource::user().unwrap(),
        )
        .unwrap();
        let cloned = message.clone();

        assert!(Arc::ptr_eq(&message.inner, &cloned.inner));
        assert_eq!(message, cloned);
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            serde_json::to_value(&cloned).unwrap()
        );
    }
}
