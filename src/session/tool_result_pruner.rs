//! Model-free tool-result pruning and compact raw-data identity facts.

use std::{
    io::{self, Write},
    sync::Arc,
};

use aws_lc_rs::digest::{Context, SHA256};
use serde::{Serialize, Serializer, ser::SerializeMap};
use serde_json::{Map, Value};

use crate::model::Message;

use super::{EventSeq, journal_row::JournalRowLocator};

const DEFAULT_THRESHOLD_CODE_POINTS: usize = 8_192;
const DEFAULT_HEAD_CODE_POINTS: usize = 4_096;
const DEFAULT_TAIL_CODE_POINTS: usize = 1_024;
const PRUNED_MIDDLE_MARKER: &str = "\n\n[... tool result middle pruned ...]\n\n";
const MASKED_DATA_DIGEST_DOMAIN: &[u8] = b"dsh.tool-result.masked-data.v1\0";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MaskedToolResultDigest([u8; 32]);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ToolResultSnapshot {
    seq: EventSeq,
    message: Message,
    estimated_tokens: u64,
    row: JournalRowLocator,
    masked: MaskedToolResultDigest,
}

impl ToolResultSnapshot {
    pub(super) fn new(
        seq: EventSeq,
        message: Message,
        estimated_tokens: u64,
        row: JournalRowLocator,
        masked: MaskedToolResultDigest,
    ) -> Self {
        Self {
            seq,
            message,
            estimated_tokens,
            row,
            masked,
        }
    }

    pub(super) fn seq(&self) -> EventSeq {
        self.seq
    }

    pub(super) fn message(&self) -> &Message {
        &self.message
    }

    pub(super) fn estimated_tokens(&self) -> u64 {
        self.estimated_tokens
    }

    pub(super) fn row(&self) -> JournalRowLocator {
        self.row
    }

    pub(super) fn masked(&self) -> MaskedToolResultDigest {
        self.masked
    }
}

pub(crate) struct ValidatedRawRow {
    owner: Arc<()>,
    snapshot: ToolResultSnapshot,
    data: Value,
}

impl ValidatedRawRow {
    pub(super) fn new(owner: Arc<()>, snapshot: ToolResultSnapshot, data: Value) -> Self {
        Self {
            owner,
            snapshot,
            data,
        }
    }

    pub(crate) fn prune(
        mut self,
        config: ToolResultPruneConfig,
    ) -> Result<Option<ValidatedRawReplacement>, ToolResultPruneError> {
        let Some(outcome) = prune_raw_tool_result_data(&mut self.data, config)? else {
            return Ok(None);
        };
        if masked_data_sha256(&self.data).map_err(|_| ToolResultPruneError::InvalidShape)?
            != self.snapshot.masked
        {
            return Err(ToolResultPruneError::InvalidShape);
        }
        Ok(Some(ValidatedRawReplacement {
            owner: self.owner,
            snapshot: self.snapshot,
            data: self.data,
            outcome,
        }))
    }
}

pub(crate) struct ValidatedRawReplacement {
    owner: Arc<()>,
    snapshot: ToolResultSnapshot,
    data: Value,
    outcome: ToolResultPruneOutcome,
}

impl ValidatedRawReplacement {
    pub(super) fn into_parts(self) -> (Arc<()>, ToolResultSnapshot, Value, ToolResultPruneOutcome) {
        (self.owner, self.snapshot, self.data, self.outcome)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolResultPruneConfig {
    threshold_code_points: usize,
    head_code_points: usize,
    tail_code_points: usize,
}

impl ToolResultPruneConfig {
    pub(crate) fn new(
        threshold_code_points: usize,
        head_code_points: usize,
        tail_code_points: usize,
    ) -> Result<Self, ToolResultPruneError> {
        let retained = head_code_points
            .checked_add(PRUNED_MIDDLE_MARKER.chars().count())
            .and_then(|value| value.checked_add(tail_code_points))
            .ok_or(ToolResultPruneError::InvalidConfig)?;
        if threshold_code_points == 0 || retained > threshold_code_points {
            return Err(ToolResultPruneError::InvalidConfig);
        }
        Ok(Self {
            threshold_code_points,
            head_code_points,
            tail_code_points,
        })
    }
}

impl Default for ToolResultPruneConfig {
    fn default() -> Self {
        Self {
            threshold_code_points: DEFAULT_THRESHOLD_CODE_POINTS,
            head_code_points: DEFAULT_HEAD_CODE_POINTS,
            tail_code_points: DEFAULT_TAIL_CODE_POINTS,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ToolResultPruneOutcome {
    pub(crate) original_code_points: usize,
    pub(crate) pruned_code_points: usize,
}

#[derive(Clone, Copy, Debug, Eq, thiserror::Error, PartialEq)]
pub(crate) enum ToolResultPruneError {
    #[error("the tool-result pruning configuration is invalid")]
    InvalidConfig,
    #[error("the tool-result payload does not have the expected shape")]
    InvalidShape,
    #[error("the tool-result pruning transform exceeded its bounded capacity")]
    Capacity,
}

enum BlockEdit {
    Keep,
    Replace(String),
    Drop,
}

/// Prune the text stream inside one raw tool-result `data` value.
///
/// All replacement strings and the edit plan are allocated before `data` is
/// changed. A failure therefore leaves the complete raw value untouched.
pub(crate) fn prune_raw_tool_result_data(
    data: &mut Value,
    config: ToolResultPruneConfig,
) -> Result<Option<ToolResultPruneOutcome>, ToolResultPruneError> {
    // Re-run construction validation even for `Default`: this keeps a later
    // config source from creating a partially meaningful invalid state.
    let config = ToolResultPruneConfig::new(
        config.threshold_code_points,
        config.head_code_points,
        config.tail_code_points,
    )?;
    let content = tool_result_content(data).ok_or(ToolResultPruneError::InvalidShape)?;
    let original_code_points = content.iter().try_fold(0_usize, |total, block| {
        let Some(text) = text_block(block)? else {
            return Ok(total);
        };
        total
            .checked_add(text.chars().count())
            .ok_or(ToolResultPruneError::Capacity)
    })?;
    if original_code_points <= config.threshold_code_points {
        return Ok(None);
    }

    let removed_start = config.head_code_points;
    let removed_end = original_code_points
        .checked_sub(config.tail_code_points)
        .ok_or(ToolResultPruneError::InvalidConfig)?;
    let mut edits = Vec::new();
    edits
        .try_reserve_exact(content.len())
        .map_err(|_| ToolResultPruneError::Capacity)?;
    let mut cursor = 0_usize;
    let mut marker_written = false;
    for block in content {
        let Some(text) = text_block(block)? else {
            edits.push(BlockEdit::Keep);
            continue;
        };
        let block_points = text.chars().count();
        let block_end = cursor
            .checked_add(block_points)
            .ok_or(ToolResultPruneError::Capacity)?;
        let intersects = cursor < removed_end && block_end > removed_start;
        if !intersects {
            // Fixed upstream rebuilds every text block during a triggered
            // prune and drops any resulting empty block, including one that
            // was already empty outside the removed middle span.
            edits.push(if block_points == 0 {
                BlockEdit::Drop
            } else {
                BlockEdit::Keep
            });
            cursor = block_end;
            continue;
        }

        let prefix_points = removed_start.saturating_sub(cursor).min(block_points);
        let suffix_start_points = removed_end.saturating_sub(cursor).min(block_points);
        let prefix_end = byte_index_at_char(text, prefix_points);
        let suffix_start = byte_index_at_char(text, suffix_start_points);
        let marker = if marker_written {
            ""
        } else {
            marker_written = true;
            PRUNED_MIDDLE_MARKER
        };
        let capacity = prefix_end
            .checked_add(marker.len())
            .and_then(|value| value.checked_add(text.len().saturating_sub(suffix_start)))
            .ok_or(ToolResultPruneError::Capacity)?;
        if capacity == 0 {
            edits.push(BlockEdit::Drop);
        } else {
            let mut replacement = String::new();
            replacement
                .try_reserve_exact(capacity)
                .map_err(|_| ToolResultPruneError::Capacity)?;
            replacement.push_str(&text[..prefix_end]);
            replacement.push_str(marker);
            replacement.push_str(&text[suffix_start..]);
            edits.push(BlockEdit::Replace(replacement));
        }
        cursor = block_end;
    }
    if !marker_written {
        return Err(ToolResultPruneError::InvalidShape);
    }

    let content = tool_result_content_mut(data).ok_or(ToolResultPruneError::InvalidShape)?;
    let mut edits = edits.into_iter();
    content.retain_mut(|block| match edits.next() {
        Some(BlockEdit::Keep) => true,
        Some(BlockEdit::Replace(text)) => {
            let Some(object) = block.as_object_mut() else {
                return false;
            };
            let Some(value) = object.get_mut("text") else {
                return false;
            };
            *value = Value::String(text);
            true
        }
        Some(BlockEdit::Drop) => false,
        None => false,
    });

    let pruned_code_points = config
        .head_code_points
        .checked_add(PRUNED_MIDDLE_MARKER.chars().count())
        .and_then(|value| value.checked_add(config.tail_code_points))
        .ok_or(ToolResultPruneError::Capacity)?;
    Ok(Some(ToolResultPruneOutcome {
        original_code_points,
        pruned_code_points,
    }))
}

fn tool_result_content(data: &Value) -> Option<&Vec<Value>> {
    data.as_object()?
        .get("message")?
        .as_object()?
        .get("content")?
        .as_array()?
        .first()?
        .as_object()?
        .get("content")?
        .as_array()
}

fn tool_result_content_mut(data: &mut Value) -> Option<&mut Vec<Value>> {
    data.as_object_mut()?
        .get_mut("message")?
        .as_object_mut()?
        .get_mut("content")?
        .as_array_mut()?
        .first_mut()?
        .as_object_mut()?
        .get_mut("content")?
        .as_array_mut()
}

fn text_block(block: &Value) -> Result<Option<&str>, ToolResultPruneError> {
    let Some(object) = block.as_object() else {
        return Ok(None);
    };
    if object.get("type").and_then(Value::as_str) != Some("text") {
        return Ok(None);
    }
    object
        .get("text")
        .and_then(Value::as_str)
        .map(Some)
        .ok_or(ToolResultPruneError::InvalidShape)
}

fn byte_index_at_char(text: &str, character: usize) -> usize {
    text.char_indices()
        .nth(character)
        .map_or(text.len(), |(index, _)| index)
}

/// Hash one complete tool-result `data` object while masking only the nested
/// result-content array that pruning is allowed to replace.
///
/// The hash is computed directly from the bounded JSON tree. It therefore does
/// not retain or allocate a second copy of a potentially multi-megabyte tool
/// result merely to prove that every immutable field stayed equal.
pub(crate) fn masked_data_sha256(data: &Value) -> Result<MaskedToolResultDigest, ()> {
    let data = data.as_object().ok_or(())?;
    let message = data.get("message").and_then(Value::as_object).ok_or(())?;
    let message_content = message
        .get("content")
        .and_then(Value::as_array)
        .filter(|content| content.len() == 1)
        .ok_or(())?;
    let result = message_content[0].as_object().ok_or(())?;
    if !result.contains_key("content") {
        return Err(());
    }

    let masked = MaskedData {
        data,
        message,
        result,
    };
    let mut context = Context::new(&SHA256);
    context.update(MASKED_DATA_DIGEST_DOMAIN);
    let mut writer = DigestWriter(context);
    serde_json::to_writer(&mut writer, &masked).map_err(|_| ())?;
    let digest = writer.0.finish();
    let mut bytes = [0_u8; 32];
    bytes.copy_from_slice(digest.as_ref());
    Ok(MaskedToolResultDigest(bytes))
}

struct MaskedData<'a> {
    data: &'a Map<String, Value>,
    message: &'a Map<String, Value>,
    result: &'a Map<String, Value>,
}

impl Serialize for MaskedData<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.data.len()))?;
        for (key, value) in self.data {
            if key == "message" {
                map.serialize_entry(
                    key,
                    &MaskedMessage {
                        message: self.message,
                        result: self.result,
                    },
                )?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

struct MaskedMessage<'a> {
    message: &'a Map<String, Value>,
    result: &'a Map<String, Value>,
}

impl Serialize for MaskedMessage<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.message.len()))?;
        for (key, value) in self.message {
            if key == "content" {
                map.serialize_entry(
                    key,
                    &[MaskedResult {
                        result: self.result,
                    }],
                )?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

struct MaskedResult<'a> {
    result: &'a Map<String, Value>,
}

impl Serialize for MaskedResult<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.result.len()))?;
        for (key, value) in self.result {
            if key == "content" {
                map.serialize_entry(key, &Value::Null)?;
            } else {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

struct DigestWriter(Context);

impl Write for DigestWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        PRUNED_MIDDLE_MARKER, ToolResultPruneConfig, masked_data_sha256, prune_raw_tool_result_data,
    };

    fn data_with_content(content: serde_json::Value) -> serde_json::Value {
        json!({
            "turn": 1,
            "step": 1,
            "message": {
                "id": "result-1",
                "role": "user",
                "content": [{
                    "type": "tool-result",
                    "toolCallId": "call-1",
                    "content": content,
                    "isError": false,
                    "futureResultField": { "kept": true }
                }],
                "source": { "kind": "tool", "callId": "call-1" },
                "futureMessageField": [1, 2, 3]
            },
            "meta": { "future": true }
        })
    }

    #[test]
    fn masked_identity_changes_only_for_immutable_tool_result_data() {
        let original = data_with_content(json!([{ "type": "text", "text": "large output" }]));
        let mut changed_text = original.clone();
        changed_text["message"]["content"][0]["content"] =
            json!([{ "type": "text", "text": "pruned" }]);
        assert_eq!(
            masked_data_sha256(&original).unwrap(),
            masked_data_sha256(&changed_text).unwrap()
        );

        let mut changed_extension = changed_text;
        changed_extension["meta"]["future"] = json!(false);
        assert_ne!(
            masked_data_sha256(&original).unwrap(),
            masked_data_sha256(&changed_extension).unwrap()
        );
    }

    #[test]
    fn default_pruner_obeys_exact_threshold_and_unicode_code_points() {
        let mut exact = data_with_content(json!([{
            "type": "text",
            "text": "x".repeat(8_192),
            "extension": "preserved"
        }]));
        assert_eq!(
            prune_raw_tool_result_data(&mut exact, ToolResultPruneConfig::default()).unwrap(),
            None
        );

        let mut one_over = data_with_content(json!([{
            "type": "text",
            "text": format!(
                "{}{}{}",
                "H".repeat(4_096),
                "😀".repeat(3_073),
                "T".repeat(1_024)
            ),
            "extension": "preserved"
        }]));
        let outcome = prune_raw_tool_result_data(&mut one_over, ToolResultPruneConfig::default())
            .unwrap()
            .unwrap();
        assert_eq!(outcome.original_code_points, 8_193);
        assert_eq!(outcome.pruned_code_points, 5_159);
        let text = one_over["message"]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text.chars().count(), 5_159);
        assert_eq!(text.matches(PRUNED_MIDDLE_MARKER).count(), 1);
        assert!(text.starts_with(&"H".repeat(4_096)));
        assert!(text.ends_with(&"T".repeat(1_024)));
        assert_eq!(
            one_over["message"]["content"][0]["content"][0]["extension"],
            json!("preserved")
        );
    }

    #[test]
    fn pruner_treats_text_blocks_as_one_stream_and_preserves_rich_blocks() {
        let config = ToolResultPruneConfig::new(50, 4, 3).unwrap();
        let rich = json!({ "type": "future-rich", "payload": [1, 2, 3] });
        let mut data = data_with_content(json!([
            { "type": "text", "text": "HEAD", "extension": "first" },
            rich.clone(),
            { "type": "text", "text": "m".repeat(44), "extension": "middle" },
            { "type": "text", "text": "END", "extension": "last" }
        ]));
        let before_identity = masked_data_sha256(&data).unwrap();
        let outcome = prune_raw_tool_result_data(&mut data, config)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.original_code_points, 51);
        assert_eq!(outcome.pruned_code_points, 46);
        assert_eq!(masked_data_sha256(&data).unwrap(), before_identity);
        assert_eq!(data["message"]["content"][0]["content"][0]["text"], "HEAD");
        assert_eq!(data["message"]["content"][0]["content"][1], rich);
        assert_eq!(
            data["message"]["content"][0]["content"][2]["text"],
            PRUNED_MIDDLE_MARKER
        );
        assert_eq!(data["message"]["content"][0]["content"][3]["text"], "END");
        assert_eq!(
            data["message"]["content"][0]["content"][2]["extension"],
            "middle"
        );

        assert_eq!(prune_raw_tool_result_data(&mut data, config).unwrap(), None);
    }

    #[test]
    fn zero_head_and_tail_can_make_a_token_neutral_marker_only_result() {
        let config = ToolResultPruneConfig::new(39, 0, 0).unwrap();
        let mut data = data_with_content(json!([
            { "type": "text", "text": "x".repeat(20) },
            { "type": "image", "url": "preserved" },
            { "type": "text", "text": "x".repeat(20) }
        ]));
        let outcome = prune_raw_tool_result_data(&mut data, config)
            .unwrap()
            .unwrap();
        assert_eq!(outcome.original_code_points, 40);
        assert_eq!(outcome.pruned_code_points, 39);
        assert_eq!(
            data["message"]["content"][0]["content"],
            json!([
                { "type": "text", "text": PRUNED_MIDDLE_MARKER },
                { "type": "image", "url": "preserved" }
            ])
        );
    }

    #[test]
    fn a_triggered_prune_drops_originally_empty_text_blocks() {
        let config = ToolResultPruneConfig::new(50, 4, 3).unwrap();
        let mut data = data_with_content(json!([
            { "type": "text", "text": "", "extension": "before" },
            { "type": "text", "text": "A".repeat(51) },
            { "type": "text", "text": "", "extension": "after" }
        ]));

        prune_raw_tool_result_data(&mut data, config)
            .unwrap()
            .unwrap();

        assert_eq!(
            data["message"]["content"][0]["content"],
            json!([{
                "type": "text",
                "text": format!("AAAA{PRUNED_MIDDLE_MARKER}AAA")
            }])
        );
    }
}
