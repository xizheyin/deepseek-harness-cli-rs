//! Strict whole-stream grammar shared by real and fake providers.

use std::collections::{BTreeMap, BTreeSet};

use thiserror::Error;

use crate::model::{
    ContentBlockKind, ContentBlockType, FinishReasonKind, StreamChunk, StreamChunkKind,
};

/// Maximum provider-neutral chunks emitted by one model call.
pub const MAX_PROVIDER_STREAM_CHUNKS: usize = 4_000;

/// Incremental validator for one live provider stream.
#[derive(Clone, Debug, Default)]
pub struct StreamValidator {
    open: BTreeMap<u64, ContentBlockType>,
    seen: BTreeSet<u64>,
    usage_seen: bool,
    finished: bool,
    chunk_count: usize,
}

impl StreamValidator {
    /// Validate and commit one next chunk.
    pub fn accept(&mut self, chunk: &StreamChunk) -> Result<(), StreamProtocolError> {
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

        match chunk.kind() {
            StreamChunkKind::BlockStart { index, block_type } => {
                let index = index.get();
                if self.seen.contains(&index) {
                    return Err(StreamProtocolError::ReusedBlockIndex { index });
                }
                self.seen.insert(index);
                self.open.insert(index, block_type.clone());
            }
            StreamChunkKind::TextDelta { index, .. } => {
                self.require_open(index.get(), &ContentBlockType::Text)?;
            }
            StreamChunkKind::ReasoningDelta { index, .. } => {
                self.require_open(index.get(), &ContentBlockType::Reasoning)?;
            }
            StreamChunkKind::ToolCallDelta { index, .. } => {
                self.require_open(index.get(), &ContentBlockType::ToolCall)?;
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
                self.open.remove(&index);
            }
            StreamChunkKind::Usage { .. } => {
                if self.usage_seen {
                    return Err(StreamProtocolError::DuplicateUsage);
                }
                self.usage_seen = true;
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
                self.finished = true;
            }
            StreamChunkKind::Other { chunk_type } => {
                return Err(StreamProtocolError::UnknownLiveChunk {
                    chunk_type: chunk_type.clone(),
                });
            }
        }
        self.chunk_count += 1;
        Ok(())
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
