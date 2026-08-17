use crate::{
    model::{ContentBlock, ContentBlockKind, FinishReason, TokenUsage},
    session::AttemptResidentGuard,
};

pub(crate) struct AssembledAssistant {
    pub(crate) content: Vec<ContentBlock>,
    pub(crate) usage: Option<TokenUsage>,
    pub(crate) finish: FinishReason,
    pub(crate) replay_state: Option<crate::model::JsonValue>,
    pub(crate) _resident_guard: Option<AttemptResidentGuard>,
}

pub(crate) fn without_tool_calls(mut content: Vec<ContentBlock>) -> Vec<ContentBlock> {
    content.retain(|block| !matches!(block.kind(), ContentBlockKind::ToolCall { .. }));
    content
}
