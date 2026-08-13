//! DeepSeek streamed JSON to provider-neutral chunks.

use std::collections::BTreeMap;

use serde::Deserialize;
use thiserror::Error;

use crate::model::{
    CallId, ContentBlock, ContentBlockType, FinishReason, LlmFailure, ModelError,
    NonNegativeSafeInteger, StreamChunk, TokenUsage,
};

/// Terminal DeepSeek SSE data sentinel.
pub(super) const DONE: &str = "[DONE]";
/// Maximum content blocks opened by one DeepSeek call.
pub const MAX_DEEPSEEK_BLOCKS: usize = 128;
/// Maximum accumulated text, reasoning, and tool-argument bytes.
pub const MAX_DEEPSEEK_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// Maximum aggregate bytes in provider-neutral chunks emitted by one call.
pub const MAX_DEEPSEEK_EMITTED_BYTES: usize = 10 * 1024 * 1024;
const MAX_WIRE_CHOICES: usize = 128;
const MAX_TOOL_DELTAS_PER_CHOICE: usize = 128;
const MAX_UNKNOWN_FINISH_BYTES: usize = 128;

#[derive(Debug, Default)]
pub(super) struct DeepSeekTranslator {
    blocks: Vec<OpenBlock>,
    reasoning: Option<usize>,
    text: Option<usize>,
    tools: BTreeMap<u64, usize>,
    pending_finish: Option<String>,
    pending_usage: Option<TokenUsage>,
    retained_block_bytes: usize,
    emitted_bytes: usize,
    done: bool,
}

impl DeepSeekTranslator {
    pub(super) fn accept(&mut self, payload: &str) -> Result<Vec<StreamChunk>, TranslateError> {
        if self.done {
            return Err(TranslateError::AfterDone);
        }
        if payload == DONE {
            return self.finish();
        }
        let wire: WireChunk =
            serde_json::from_str(payload).map_err(|_| TranslateError::MalformedResponse)?;
        let choices = wire.choices.unwrap_or_default();
        if choices.len() > MAX_WIRE_CHOICES {
            return Err(TranslateError::TooManyChoices {
                maximum: MAX_WIRE_CHOICES,
            });
        }

        let mut output = Vec::new();
        for choice in choices {
            if let Some(delta) = choice.delta {
                if let Some(reasoning) = delta.reasoning_content.filter(|value| !value.is_empty()) {
                    let index = match self.reasoning {
                        Some(index) => index,
                        None => {
                            let index = self.open(OpenKind::Reasoning, &mut output)?;
                            self.reasoning = Some(index);
                            index
                        }
                    };
                    self.append(index, &reasoning)?;
                    let chunk =
                        StreamChunk::reasoning_delta(self.blocks[index].internal_index, reasoning)?;
                    self.emit(&mut output, chunk)?;
                }

                if let Some(text) = delta.content.filter(|value| !value.is_empty()) {
                    let index = match self.text {
                        Some(index) => index,
                        None => {
                            let index = self.open(OpenKind::Text, &mut output)?;
                            self.text = Some(index);
                            index
                        }
                    };
                    self.append(index, &text)?;
                    let chunk = StreamChunk::text_delta(self.blocks[index].internal_index, text)?;
                    self.emit(&mut output, chunk)?;
                }

                let tool_calls = delta.tool_calls.unwrap_or_default();
                if tool_calls.len() > MAX_TOOL_DELTAS_PER_CHOICE {
                    return Err(TranslateError::TooManyToolDeltas {
                        maximum: MAX_TOOL_DELTAS_PER_CHOICE,
                    });
                }
                for call in tool_calls {
                    let index = match self.tools.get(&call.index.get()) {
                        Some(index) => *index,
                        None => {
                            let index = self.open(OpenKind::ToolCall, &mut output)?;
                            self.tools.insert(call.index.get(), index);
                            index
                        }
                    };
                    if let Some(id) = call.id {
                        let previous = self.blocks[index].call_id.as_ref().map_or(0, String::len);
                        self.replace_retained(previous, id.len())?;
                        self.blocks[index].call_id = Some(id);
                    }
                    let fragment = match call.function {
                        Some(function) => {
                            if let Some(name) = function.name {
                                let previous =
                                    self.blocks[index].name.as_ref().map_or(0, String::len);
                                self.replace_retained(previous, name.len())?;
                                self.blocks[index].name = Some(name);
                            }
                            function.arguments.unwrap_or_default()
                        }
                        None => String::new(),
                    };
                    self.append(index, &fragment)?;
                    let (internal_index, call_id, name) = {
                        let block = &self.blocks[index];
                        (
                            block.internal_index,
                            block.call_id.clone().unwrap_or_default(),
                            block.name.clone(),
                        )
                    };
                    let chunk = StreamChunk::tool_call_delta(
                        internal_index,
                        CallId::new(call_id),
                        name,
                        fragment,
                    )?;
                    self.emit(&mut output, chunk)?;
                }
            }
            if let Some(reason) = choice.finish_reason {
                self.pending_finish = Some(reason);
            }
        }
        if let Some(usage) = wire.usage {
            self.pending_usage = Some(map_usage(usage)?);
        }
        Ok(output)
    }

    fn open(
        &mut self,
        kind: OpenKind,
        output: &mut Vec<StreamChunk>,
    ) -> Result<usize, TranslateError> {
        if self.blocks.len() == MAX_DEEPSEEK_BLOCKS {
            return Err(TranslateError::TooManyBlocks {
                maximum: MAX_DEEPSEEK_BLOCKS,
            });
        }
        let internal_index = self.blocks.len() as u64;
        let chunk = StreamChunk::block_start(internal_index, kind.block_type())?;
        self.emit(output, chunk)?;
        self.blocks.push(OpenBlock {
            internal_index,
            kind,
            text: String::new(),
            call_id: None,
            name: None,
        });
        Ok(self.blocks.len() - 1)
    }

    fn append(&mut self, block: usize, fragment: &str) -> Result<(), TranslateError> {
        let next = self
            .retained_block_bytes
            .checked_add(fragment.len())
            .ok_or(TranslateError::OutputTooLarge {
                maximum: MAX_DEEPSEEK_OUTPUT_BYTES,
            })?;
        if next > MAX_DEEPSEEK_OUTPUT_BYTES {
            return Err(TranslateError::OutputTooLarge {
                maximum: MAX_DEEPSEEK_OUTPUT_BYTES,
            });
        }
        self.retained_block_bytes = next;
        self.blocks[block].text.push_str(fragment);
        Ok(())
    }

    fn replace_retained(&mut self, previous: usize, next: usize) -> Result<(), TranslateError> {
        let retained = self
            .retained_block_bytes
            .checked_sub(previous)
            .and_then(|value| value.checked_add(next))
            .ok_or(TranslateError::OutputTooLarge {
                maximum: MAX_DEEPSEEK_OUTPUT_BYTES,
            })?;
        if retained > MAX_DEEPSEEK_OUTPUT_BYTES {
            return Err(TranslateError::OutputTooLarge {
                maximum: MAX_DEEPSEEK_OUTPUT_BYTES,
            });
        }
        self.retained_block_bytes = retained;
        Ok(())
    }

    fn emit(
        &mut self,
        output: &mut Vec<StreamChunk>,
        chunk: StreamChunk,
    ) -> Result<(), TranslateError> {
        let next = self
            .emitted_bytes
            .checked_add(chunk.raw().encoded_len())
            .ok_or(TranslateError::EmittedTooLarge {
                maximum: MAX_DEEPSEEK_EMITTED_BYTES,
            })?;
        if next > MAX_DEEPSEEK_EMITTED_BYTES {
            return Err(TranslateError::EmittedTooLarge {
                maximum: MAX_DEEPSEEK_EMITTED_BYTES,
            });
        }
        self.emitted_bytes = next;
        output.push(chunk);
        Ok(())
    }

    fn finish(&mut self) -> Result<Vec<StreamChunk>, TranslateError> {
        self.done = true;
        let mut candidates = Vec::with_capacity(self.blocks.len() + 2);
        for block in &self.blocks {
            let content = match block.kind {
                OpenKind::Text => ContentBlock::text(&block.text)?,
                OpenKind::Reasoning => ContentBlock::reasoning(&block.text)?,
                OpenKind::ToolCall => ContentBlock::tool_call(
                    CallId::new(block.call_id.clone().unwrap_or_default()),
                    block.name.clone().unwrap_or_default(),
                    &block.text,
                )?,
            };
            candidates.push(StreamChunk::block_end(block.internal_index, content)?);
        }
        if let Some(usage) = self.pending_usage.take() {
            candidates.push(StreamChunk::usage(usage)?);
        }
        let reason = map_finish_reason(
            self.pending_finish.as_deref().unwrap_or("stop"),
            self.blocks.is_empty(),
        )?;
        candidates.push(StreamChunk::finish(reason, None)?);
        let mut output = Vec::with_capacity(candidates.len());
        for chunk in candidates {
            self.emit(&mut output, chunk)?;
        }
        Ok(output)
    }
}

#[derive(Clone, Copy, Debug)]
enum OpenKind {
    Text,
    Reasoning,
    ToolCall,
}

impl OpenKind {
    fn block_type(self) -> ContentBlockType {
        match self {
            Self::Text => ContentBlockType::Text,
            Self::Reasoning => ContentBlockType::Reasoning,
            Self::ToolCall => ContentBlockType::ToolCall,
        }
    }
}

#[derive(Debug)]
struct OpenBlock {
    internal_index: u64,
    kind: OpenKind,
    text: String,
    call_id: Option<String>,
    name: Option<String>,
}

fn map_finish_reason(reason: &str, empty: bool) -> Result<FinishReason, TranslateError> {
    match reason {
        "stop" if empty => Ok(FinishReason::error(LlmFailure::new(
            "model returned a completed response with no content",
            "EMPTY_RESPONSE",
        )?)?),
        "stop" => Ok(FinishReason::stop()?),
        "tool_calls" => Ok(FinishReason::tool_calls()?),
        "length" => Ok(FinishReason::max_tokens()?),
        other
            if !other.is_empty()
                && other.len() <= MAX_UNKNOWN_FINISH_BYTES
                && other.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) =>
        {
            Ok(FinishReason::error(LlmFailure::new(
                format!("model stopped: {other}"),
                other.to_ascii_uppercase(),
            )?)?)
        }
        _ => Err(TranslateError::MalformedResponse),
    }
}

fn map_usage(usage: WireUsage) -> Result<TokenUsage, TranslateError> {
    let cache_read = usage
        .prompt_tokens_details
        .and_then(|details| details.cached_tokens)
        .or(usage.prompt_cache_hit_tokens);
    let input = usage
        .prompt_tokens
        .get()
        .checked_sub(cache_read.map_or(0, NonNegativeSafeInteger::get))
        .ok_or(TranslateError::InvalidUsage)?;
    let reasoning = usage
        .completion_tokens_details
        .and_then(|details| details.reasoning_tokens);
    Ok(TokenUsage::from_parts(
        NonNegativeSafeInteger::new(input)?,
        usage.completion_tokens,
        cache_read,
        None,
        reasoning,
    )?)
}

#[derive(Debug, Deserialize)]
struct WireChunk {
    #[serde(default)]
    choices: Option<Vec<WireChoice>>,
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireChoice {
    #[serde(default)]
    delta: Option<WireDelta>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct WireToolCallDelta {
    index: NonNegativeSafeInteger,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<WireFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct WireFunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    prompt_tokens: NonNegativeSafeInteger,
    completion_tokens: NonNegativeSafeInteger,
    #[serde(default)]
    prompt_cache_hit_tokens: Option<NonNegativeSafeInteger>,
    #[serde(default)]
    prompt_tokens_details: Option<WirePromptDetails>,
    #[serde(default)]
    completion_tokens_details: Option<WireCompletionDetails>,
}

#[derive(Debug, Deserialize)]
struct WirePromptDetails {
    #[serde(default)]
    cached_tokens: Option<NonNegativeSafeInteger>,
}

#[derive(Debug, Deserialize)]
struct WireCompletionDetails {
    #[serde(default)]
    reasoning_tokens: Option<NonNegativeSafeInteger>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(super) enum TranslateError {
    #[error("DeepSeek returned malformed streamed JSON")]
    MalformedResponse,
    #[error("DeepSeek response contains too many choices; maximum is {maximum}")]
    TooManyChoices { maximum: usize },
    #[error("DeepSeek response contains too many tool deltas; maximum is {maximum}")]
    TooManyToolDeltas { maximum: usize },
    #[error("DeepSeek response opens too many blocks; maximum is {maximum}")]
    TooManyBlocks { maximum: usize },
    #[error("DeepSeek accumulated output exceeds {maximum} bytes")]
    OutputTooLarge { maximum: usize },
    #[error("DeepSeek emitted chunks exceed {maximum} bytes")]
    EmittedTooLarge { maximum: usize },
    #[error("DeepSeek usage counters are inconsistent")]
    InvalidUsage,
    #[error("DeepSeek emitted data after [DONE]")]
    AfterDone,
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error(transparent)]
    Json(#[from] crate::model::JsonValueError),
}

impl TranslateError {
    pub(super) fn code(&self) -> &'static str {
        match self {
            Self::TooManyChoices { .. }
            | Self::TooManyToolDeltas { .. }
            | Self::TooManyBlocks { .. }
            | Self::OutputTooLarge { .. }
            | Self::EmittedTooLarge { .. } => "RESPONSE_TOO_LARGE",
            Self::MalformedResponse
            | Self::InvalidUsage
            | Self::AfterDone
            | Self::Model(_)
            | Self::Json(_) => "MALFORMED_RESPONSE",
        }
    }
}
