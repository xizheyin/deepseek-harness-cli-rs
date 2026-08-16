//! Strict provider-neutral whole-stream grammar shared by Session and providers.

use std::collections::{HashMap, HashSet, TryReserveError};

use thiserror::Error;

use super::{ContentBlockKind, ContentBlockType, FinishReasonKind, StreamChunk, StreamChunkKind};

/// Maximum provider-neutral chunks emitted by one model call.
pub const MAX_PROVIDER_STREAM_CHUNKS: usize = 4_000;

/// Incremental validator for one live provider stream.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StreamValidator {
    open: HashMap<u64, ContentBlockType>,
    seen: HashSet<u64>,
    usage_seen: bool,
    finished: bool,
    chunk_count: usize,
}

/// One already-validated stream-state change.
///
/// Session persistence prepares this value before it assigns a timestamp or
/// journal sequence. Committing it later performs no validation and, when the
/// validator was created with [`StreamValidator::try_bounded`], needs no table
/// growth. That split keeps a failed clock or row encoding from half-consuming
/// one provider chunk.
pub(crate) struct PreparedStreamTransition {
    kind: PreparedStreamTransitionKind,
}

enum PreparedStreamTransitionKind {
    Continue,
    BlockStart {
        index: u64,
        block_type: ContentBlockType,
    },
    BlockEnd {
        index: u64,
    },
    Usage,
    Finish,
}

impl StreamValidator {
    /// Build the Session-owned validator with all per-attempt table capacity
    /// reserved up front. Provider adapters may continue to use `Default`;
    /// durable Session admission needs the fallible constructor so no hidden
    /// allocation remains after an event receives its timestamp.
    pub(crate) fn try_bounded() -> Result<Self, TryReserveError> {
        let mut validator = Self::default();
        validator.open.try_reserve(MAX_PROVIDER_STREAM_CHUNKS)?;
        validator.seen.try_reserve(MAX_PROVIDER_STREAM_CHUNKS)?;
        Ok(validator)
    }

    /// Validate and commit one next chunk.
    pub fn accept(&mut self, chunk: &StreamChunk) -> Result<(), StreamProtocolError> {
        let prepared = self.prepare(chunk)?;
        self.commit(prepared);
        Ok(())
    }

    pub(crate) fn prepare(
        &self,
        chunk: &StreamChunk,
    ) -> Result<PreparedStreamTransition, StreamProtocolError> {
        if self.finished {
            return Err(StreamProtocolError::ChunkAfterFinish {
                chunk_type: chunk_type(chunk).to_owned(),
            });
        }
        let is_finish = matches!(chunk.kind(), StreamChunkKind::Finish { .. });
        if self.chunk_count == MAX_PROVIDER_STREAM_CHUNKS
            || (self.chunk_count + 1 == MAX_PROVIDER_STREAM_CHUNKS && !is_finish)
        {
            return Err(StreamProtocolError::TooManyChunks {
                maximum: MAX_PROVIDER_STREAM_CHUNKS,
            });
        }

        let kind = match chunk.kind() {
            StreamChunkKind::BlockStart { index, block_type } => {
                let index = index.get();
                if self.seen.contains(&index) {
                    return Err(StreamProtocolError::ReusedBlockIndex { index });
                }
                PreparedStreamTransitionKind::BlockStart {
                    index,
                    block_type: block_type.clone(),
                }
            }
            StreamChunkKind::TextDelta { index, .. } => {
                self.require_open(index.get(), &ContentBlockType::Text)?;
                PreparedStreamTransitionKind::Continue
            }
            StreamChunkKind::ReasoningDelta { index, .. } => {
                self.require_open(index.get(), &ContentBlockType::Reasoning)?;
                PreparedStreamTransitionKind::Continue
            }
            StreamChunkKind::ToolCallDelta { index, .. } => {
                self.require_open(index.get(), &ContentBlockType::ToolCall)?;
                PreparedStreamTransitionKind::Continue
            }
            StreamChunkKind::BlockEnd { index, block } => {
                let index = index.get();
                let expected = self
                    .open
                    .get(&index)
                    .ok_or(StreamProtocolError::BlockEndWithoutStart { index })?;
                let actual = block_type(block.kind());
                if expected != &actual {
                    return Err(StreamProtocolError::BlockEndTypeMismatch {
                        index,
                        expected: type_name(expected),
                        actual: type_name(&actual),
                    });
                }
                PreparedStreamTransitionKind::BlockEnd { index }
            }
            StreamChunkKind::Usage { .. } => {
                if self.usage_seen {
                    return Err(StreamProtocolError::DuplicateUsage);
                }
                PreparedStreamTransitionKind::Usage
            }
            StreamChunkKind::Finish { reason, .. } => {
                let can_leave_open = matches!(
                    reason.kind(),
                    FinishReasonKind::Error { .. } | FinishReasonKind::Aborted { .. }
                );
                if !can_leave_open && !self.open.is_empty() {
                    return Err(StreamProtocolError::SuccessfulFinishWithOpenBlocks {
                        count: self.open.len(),
                    });
                }
                PreparedStreamTransitionKind::Finish
            }
            StreamChunkKind::Other { chunk_type } => {
                return Err(StreamProtocolError::UnknownLiveChunk {
                    chunk_type: chunk_type.clone(),
                });
            }
        };
        Ok(PreparedStreamTransition { kind })
    }

    pub(crate) fn commit(&mut self, prepared: PreparedStreamTransition) {
        match prepared.kind {
            PreparedStreamTransitionKind::Continue => {}
            PreparedStreamTransitionKind::BlockStart { index, block_type } => {
                debug_assert!(!self.seen.contains(&index));
                self.seen.insert(index);
                self.open.insert(index, block_type);
            }
            PreparedStreamTransitionKind::BlockEnd { index } => {
                debug_assert!(self.open.contains_key(&index));
                self.open.remove(&index);
            }
            PreparedStreamTransitionKind::Usage => self.usage_seen = true,
            PreparedStreamTransitionKind::Finish => self.finished = true,
        }
        self.chunk_count += 1;
    }

    /// Validate that the producer ended only after a terminal finish.
    pub fn complete(&self) -> Result<(), StreamProtocolError> {
        if !self.finished {
            return Err(StreamProtocolError::MissingFinish);
        }
        Ok(())
    }

    /// Number of already accepted chunks.
    #[must_use]
    pub fn chunk_count(&self) -> usize {
        self.chunk_count
    }

    /// Whether a terminal finish has been accepted.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    fn require_open(
        &self,
        index: u64,
        expected: &ContentBlockType,
    ) -> Result<(), StreamProtocolError> {
        match self.open.get(&index) {
            Some(actual) if actual == expected => Ok(()),
            actual => Err(StreamProtocolError::DeltaWithoutMatchingBlock {
                index,
                expected: type_name(expected),
                actual: actual.map_or_else(|| "none".to_owned(), type_name),
            }),
        }
    }
}

fn block_type(kind: &ContentBlockKind) -> ContentBlockType {
    match kind {
        ContentBlockKind::Text { .. } => ContentBlockType::Text,
        ContentBlockKind::Reasoning { .. } => ContentBlockType::Reasoning,
        ContentBlockKind::Image { .. } => ContentBlockType::Image,
        ContentBlockKind::ToolCall { .. } => ContentBlockType::ToolCall,
        ContentBlockKind::ToolResult { .. } => ContentBlockType::ToolResult,
        ContentBlockKind::Other { block_type } => {
            ContentBlockType::Other(block_type.clone().unwrap_or_default())
        }
    }
}

fn type_name(block_type: &ContentBlockType) -> String {
    match block_type {
        ContentBlockType::Text => "text".to_owned(),
        ContentBlockType::Reasoning => "reasoning".to_owned(),
        ContentBlockType::Image => "image".to_owned(),
        ContentBlockType::ToolCall => "tool-call".to_owned(),
        ContentBlockType::ToolResult => "tool-result".to_owned(),
        ContentBlockType::Other(value) => value.clone(),
    }
}

fn chunk_type(chunk: &StreamChunk) -> &str {
    match chunk.kind() {
        StreamChunkKind::BlockStart { .. } => "block-start",
        StreamChunkKind::TextDelta { .. } => "text-delta",
        StreamChunkKind::ReasoningDelta { .. } => "reasoning-delta",
        StreamChunkKind::ToolCallDelta { .. } => "tool-call-delta",
        StreamChunkKind::BlockEnd { .. } => "block-end",
        StreamChunkKind::Usage { .. } => "usage",
        StreamChunkKind::Finish { .. } => "finish",
        StreamChunkKind::Other { chunk_type } => chunk_type,
    }
}

/// One broken whole-stream invariant.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum StreamProtocolError {
    /// A provider reused a block identity that already closed.
    #[error("provider stream reused block index {index}")]
    ReusedBlockIndex { index: u64 },
    /// A delta did not address a matching open block.
    #[error("{expected} delta at index {index} requires an open {expected} block, got {actual}")]
    DeltaWithoutMatchingBlock {
        index: u64,
        expected: String,
        actual: String,
    },
    /// A block ended without a corresponding start.
    #[error("block-end index {index} has no open block")]
    BlockEndWithoutStart { index: u64 },
    /// A block ended with a different type.
    #[error("block-end index {index} closes {actual}, expected {expected}")]
    BlockEndTypeMismatch {
        index: u64,
        expected: String,
        actual: String,
    },
    /// Usage appeared more than once.
    #[error("provider stream emitted usage more than once")]
    DuplicateUsage,
    /// A successful terminal reason cannot abandon partial blocks.
    #[error("provider stream finished successfully with {count} open block(s)")]
    SuccessfulFinishWithOpenBlocks { count: usize },
    /// A live provider emitted an unrecognized chunk vocabulary.
    #[error("provider stream emitted unknown live chunk {chunk_type:?}")]
    UnknownLiveChunk { chunk_type: String },
    /// Nothing may follow terminal finish.
    #[error("provider stream emitted {chunk_type} after terminal finish")]
    ChunkAfterFinish { chunk_type: String },
    /// The iterator ended without terminal finish.
    #[error("provider stream ended without a terminal finish chunk")]
    MissingFinish,
    /// One call exceeded the bounded event count.
    #[error("provider stream exceeds the maximum of {maximum} chunks")]
    TooManyChunks { maximum: usize },
}
