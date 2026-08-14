use std::collections::BTreeMap;

use crate::model::{
    ContentBlock, ContentBlockKind, FinishReason, StreamChunk, StreamChunkKind, TokenUsage,
};

#[derive(Default)]
pub(crate) struct AssistantAssembler {
    order: Vec<u64>,
    blocks: BTreeMap<u64, ContentBlock>,
    usage: Option<TokenUsage>,
    finish: Option<FinishReason>,
    replay_state: Option<crate::model::JsonValue>,
}

pub(crate) struct AssembledAssistant {
    pub(crate) content: Vec<ContentBlock>,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) finish: FinishReason,
    pub(crate) replay_state: Option<crate::model::JsonValue>,
}

impl AssistantAssembler {
    pub(crate) fn push(&mut self, chunk: &StreamChunk) {
        match chunk.kind() {
            StreamChunkKind::BlockStart { index, .. } => self.order.push(index.get()),
            StreamChunkKind::BlockEnd { index, block } => {
                self.blocks.insert(index.get(), block.clone());
            }
            StreamChunkKind::Usage { usage } => self.usage = Some(usage.clone()),
            StreamChunkKind::Finish {
                reason,
                replay_state,
            } => {
                self.finish = Some(reason.clone());
                self.replay_state = replay_state.clone();
            }
            StreamChunkKind::TextDelta { .. }
            | StreamChunkKind::ReasoningDelta { .. }
            | StreamChunkKind::ToolCallDelta { .. }
            | StreamChunkKind::Other { .. } => {}
        }
    }

    pub(crate) fn finish(self) -> Option<AssembledAssistant> {
        let finish = self.finish?;
        let content = self
            .order
            .into_iter()
            .filter_map(|index| self.blocks.get(&index).cloned())
            .collect();
        Some(AssembledAssistant {
            content,
            usage: self.usage,
            finish,
            replay_state: self.replay_state,
        })
    }
}

pub(crate) fn without_tool_calls(content: Vec<ContentBlock>) -> Vec<ContentBlock> {
    content
        .into_iter()
        .filter(|block| !matches!(block.kind(), ContentBlockKind::ToolCall { .. }))
        .collect()
}
