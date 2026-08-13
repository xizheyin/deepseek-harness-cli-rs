//! Provider-neutral messages to DeepSeek chat-completions JSON.

use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::{
    model::{ContentBlock, ContentBlockKind, Message, MessageRole},
    provider::{MAX_PROVIDER_REQUEST_BYTES, ProviderRequest, RequestPurpose},
};

use super::config::{DEEPSEEK_PROVIDER, DeepSeekConfig, DeepSeekReasoningEffort, DeepSeekThinking};

pub(super) fn serialize_request(
    config: &DeepSeekConfig,
    request: &ProviderRequest,
) -> Result<Vec<u8>, RequestBuildError> {
    let value = request_value(config, request)?;
    let encoded =
        serde_json::to_vec(&value).map_err(|error| RequestBuildError::Encode(error.to_string()))?;
    if encoded.len() > MAX_PROVIDER_REQUEST_BYTES {
        return Err(RequestBuildError::TooLarge {
            maximum: MAX_PROVIDER_REQUEST_BYTES,
            actual: encoded.len(),
        });
    }
    Ok(encoded)
}

pub(super) fn request_value(
    config: &DeepSeekConfig,
    request: &ProviderRequest,
) -> Result<Value, RequestBuildError> {
    if request.config().provider() != DEEPSEEK_PROVIDER {
        return Err(RequestBuildError::WrongProvider);
    }
    for message in request.messages() {
        if content_has_image(message.content()) {
            return Err(RequestBuildError::UnsupportedContent);
        }
    }

    let mut messages = Vec::new();
    if let Some(system) = request.system() {
        messages.push(json!({ "role": "system", "content": system }));
    }
    for message in request.messages() {
        serialize_message(message, &mut messages);
    }

    let mut root = Map::new();
    root.insert(
        "model".to_owned(),
        Value::String(request.config().model().to_owned()),
    );
    root.insert("messages".to_owned(), Value::Array(messages));
    root.insert("stream".to_owned(), Value::Bool(true));
    root.insert(
        "stream_options".to_owned(),
        json!({ "include_usage": true }),
    );

    let (thinking, effort) = resolve_thinking(config, request)?;
    root.insert(
        "thinking".to_owned(),
        json!({
            "type": match thinking {
                DeepSeekThinking::Enabled => "enabled",
                DeepSeekThinking::Disabled => "disabled",
            }
        }),
    );
    if let Some(effort) = effort {
        root.insert(
            "reasoning_effort".to_owned(),
            Value::String(match effort {
                DeepSeekReasoningEffort::High => "high".to_owned(),
                DeepSeekReasoningEffort::Max => "max".to_owned(),
                DeepSeekReasoningEffort::Off => {
                    return Err(RequestBuildError::InvalidResolvedThinking);
                }
            }),
        );
    }

    if !request.tools().is_empty() {
        root.insert(
            "tools".to_owned(),
            Value::Array(
                request
                    .tools()
                    .iter()
                    .map(|tool| {
                        json!({
                            "type": "function",
                            "function": {
                                "name": tool.name(),
                                "description": tool.description(),
                                "parameters": tool.parameters().as_value(),
                            },
                        })
                    })
                    .collect(),
            ),
        );
    }
    if let Some(temperature) = request.config().temperature() {
        root.insert("temperature".to_owned(), json!(temperature));
    }
    let max_tokens = request
        .config()
        .max_tokens()
        .ok_or(RequestBuildError::UnpreparedConfig)?
        .get();
    if max_tokens == 0 {
        return Err(RequestBuildError::InvalidMaxTokens);
    }
    root.insert("max_tokens".to_owned(), Value::from(max_tokens));
    if let Some(stop) = request.config().stop() {
        root.insert("stop".to_owned(), json!(stop));
    }
    Ok(Value::Object(root))
}

fn serialize_message(message: &Message, output: &mut Vec<Value>) {
    match message.role() {
        MessageRole::System => output.push(json!({
            "role": "system",
            "content": flatten_text(message.content()),
        })),
        MessageRole::Assistant => serialize_assistant(message, output),
        MessageRole::User => serialize_user(message, output),
    }
}

fn serialize_assistant(message: &Message, output: &mut Vec<Value>) {
    let text = flatten_text(message.content());
    let reasoning = message
        .content()
        .iter()
        .filter_map(|block| match block.kind() {
            ContentBlockKind::Reasoning { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let tool_calls = message
        .content()
        .iter()
        .filter_map(|block| match block.kind() {
            ContentBlockKind::ToolCall {
                id,
                name,
                arguments,
            } => Some(json!({
                "id": id.as_str(),
                "type": "function",
                "function": { "name": name, "arguments": arguments },
            })),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut wire = Map::new();
    wire.insert("role".to_owned(), Value::String("assistant".to_owned()));
    wire.insert("content".to_owned(), Value::String(text));
    if !tool_calls.is_empty() && !reasoning.is_empty() {
        wire.insert("reasoning_content".to_owned(), Value::String(reasoning));
    }
    if !tool_calls.is_empty() {
        wire.insert("tool_calls".to_owned(), Value::Array(tool_calls));
    }
    output.push(Value::Object(wire));
}

fn serialize_user(message: &Message, output: &mut Vec<Value>) {
    let tool_results = message
        .content()
        .iter()
        .filter(|block| matches!(block.kind(), ContentBlockKind::ToolResult { .. }))
        .collect::<Vec<_>>();
    let text = flatten_text(message.content());
    if !text.is_empty() || tool_results.is_empty() {
        output.push(json!({ "role": "user", "content": text }));
    }
    for result in tool_results {
        let ContentBlockKind::ToolResult { tool_call_id, .. } = result.kind() else {
            continue;
        };
        let content = flatten_raw_text(result.tool_result_content().unwrap_or_default());
        output.push(json!({
            "role": "tool",
            "tool_call_id": tool_call_id.as_str(),
            "content": if content.is_empty() { "(no output)" } else { &content },
        }));
    }
}

fn flatten_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block.kind() {
            ContentBlockKind::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn flatten_raw_text(blocks: &[Value]) -> String {
    blocks
        .iter()
        .filter_map(|block| {
            let fields = block.as_object()?;
            (fields.get("type")?.as_str()? == "text").then(|| fields.get("text")?.as_str())?
        })
        .collect()
}

fn content_has_image(blocks: &[ContentBlock]) -> bool {
    let mut pending = blocks
        .iter()
        .map(|block| block.raw().as_value())
        .collect::<Vec<_>>();
    while let Some(value) = pending.pop() {
        let Some(fields) = value.as_object() else {
            continue;
        };
        match fields.get("type").and_then(Value::as_str) {
            Some("image") => return true,
            Some("tool-result") => {
                if let Some(content) = fields.get("content").and_then(Value::as_array) {
                    pending.extend(content);
                }
            }
            _ => {}
        }
    }
    false
}

fn resolve_thinking(
    config: &DeepSeekConfig,
    request: &ProviderRequest,
) -> Result<(DeepSeekThinking, Option<DeepSeekReasoningEffort>), RequestBuildError> {
    if request.purpose() == RequestPurpose::SessionTitle {
        return Ok((DeepSeekThinking::Disabled, None));
    }
    let effort = match request.config().reasoning_effort() {
        None => return Err(RequestBuildError::UnpreparedConfig),
        Some(value) => match value.as_str() {
            "off" => DeepSeekReasoningEffort::Off,
            "high" => DeepSeekReasoningEffort::High,
            "max" => DeepSeekReasoningEffort::Max,
            other => {
                return Err(RequestBuildError::UnsupportedReasoningEffort {
                    value: other.chars().take(64).collect(),
                });
            }
        },
    };
    if config.thinking() == Some(DeepSeekThinking::Disabled)
        && effort != DeepSeekReasoningEffort::Off
    {
        return Err(RequestBuildError::ThinkingDisabledWithEffort);
    }
    match effort {
        DeepSeekReasoningEffort::Off => Ok((DeepSeekThinking::Disabled, None)),
        DeepSeekReasoningEffort::High | DeepSeekReasoningEffort::Max => {
            Ok((DeepSeekThinking::Enabled, Some(effort)))
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum RequestBuildError {
    #[error("request was routed to a provider other than DeepSeek")]
    WrongProvider,
    #[error("DeepSeek chat completions do not support image content")]
    UnsupportedContent,
    #[error("DeepSeek does not support reasoning effort {value:?}")]
    UnsupportedReasoningEffort { value: String },
    #[error("DeepSeek thinking is disabled for this deployment")]
    ThinkingDisabledWithEffort,
    #[error("DeepSeek max tokens must be positive")]
    InvalidMaxTokens,
    #[error("DeepSeek call config was not prepared before dispatch")]
    UnpreparedConfig,
    #[error("resolved DeepSeek thinking state is inconsistent")]
    InvalidResolvedThinking,
    #[error("DeepSeek request is {actual} bytes; maximum is {maximum}")]
    TooLarge { maximum: usize, actual: usize },
    #[error("failed to serialize DeepSeek request: {0}")]
    Encode(String),
}

impl RequestBuildError {
    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedContent => "UNSUPPORTED_CONTENT",
            Self::UnsupportedReasoningEffort { .. } | Self::ThinkingDisabledWithEffort => {
                "UNSUPPORTED_REASONING_EFFORT"
            }
            Self::TooLarge { .. } => "REQUEST_TOO_LARGE",
            Self::WrongProvider
            | Self::InvalidMaxTokens
            | Self::UnpreparedConfig
            | Self::InvalidResolvedThinking
            | Self::Encode(_) => "INVALID_REQUEST",
        }
    }
}
