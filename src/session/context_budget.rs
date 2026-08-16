//! Fixed token estimation and balanced surface-prefix selection.

use serde_json::Value;

use crate::model::{ContentBlock, ContentBlockKind, Message, ToolSchema};

const CHARS_PER_TOKEN: u64 = 4;
const BLOCK_OVERHEAD: u64 = 4;
const ROLE_OVERHEAD: u64 = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SurfacePriceFacts {
    pub(super) tokens: u64,
    pub(super) tool_delta: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CompactablePrefix {
    pub(super) end_exclusive: usize,
    pub(super) shadowed_token_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ContextBudgetError {
    TokenOverflow,
    UnbalancedToolSurface,
}

pub(super) fn estimate_message(message: &Message) -> Result<u64, ContextBudgetError> {
    message
        .content()
        .iter()
        .try_fold(ROLE_OVERHEAD, |total, block| {
            checked_add(total, estimate_content_block(block)?)
        })
}

/// Price the provider-visible assistant content reconstructed from one exact
/// chunk span. An empty stream contributes no surface message or role
/// overhead, matching the fixed upstream TokenMeter.
pub(super) fn estimate_provider_assistant<'a>(
    blocks: impl IntoIterator<Item = &'a ContentBlock>,
) -> Result<u64, ContextBudgetError> {
    let mut tokens = 0_u64;
    let mut has_content = false;
    for block in blocks {
        has_content = true;
        tokens = checked_add(tokens, estimate_content_block(block)?)?;
    }
    if has_content {
        checked_add(tokens, ROLE_OVERHEAD)
    } else {
        Ok(0)
    }
}

pub(super) fn estimate_request_header(
    system: Option<&str>,
    tools: &[ToolSchema],
) -> Result<u64, ContextBudgetError> {
    let system_tokens = system.map_or(Ok(0), |system| {
        checked_add(estimated_text_tokens(system)?, ROLE_OVERHEAD)
    })?;
    let tool_tokens = if tools.is_empty() {
        0
    } else {
        let mut units = 2_u64;
        for (index, tool) in tools.iter().enumerate() {
            if index != 0 {
                units = checked_add(units, 1)?;
            }
            units = checked_add(units, javascript_json_utf16_len(tool.raw().as_value())?)?;
        }
        checked_add(units.div_ceil(CHARS_PER_TOKEN), BLOCK_OVERHEAD)?
    };
    checked_add(system_tokens, tool_tokens)
}

pub(super) fn select_compactable_prefix<T>(
    nodes: &[T],
    retain_tokens: u64,
    maximum_shadowed_nodes: usize,
    facts: impl Fn(&T) -> SurfacePriceFacts,
) -> Result<Option<CompactablePrefix>, ContextBudgetError> {
    if nodes.is_empty() || maximum_shadowed_nodes == 0 {
        return Ok(None);
    }

    // A compaction cut may never separate an assistant tool call from its
    // result. Validate the whole ordered surface before selecting a prefix.
    let mut balance = 0_i64;
    for node in nodes {
        balance = balance
            .checked_add(facts(node).tool_delta)
            .ok_or(ContextBudgetError::TokenOverflow)?;
        if balance < 0 {
            return Err(ContextBudgetError::UnbalancedToolSurface);
        }
    }
    // Upstream always retains at least the newest node, even when the target
    // retention is zero.
    let mut retained = 0_u64;
    let mut keep_from = nodes.len();
    for index in (0..nodes.len()).rev() {
        retained = checked_add(retained, facts(&nodes[index]).tokens)?;
        keep_from = index;
        if retained >= retain_tokens {
            break;
        }
    }
    if keep_from == 0 {
        return Ok(None);
    }

    let candidate = keep_from.min(maximum_shadowed_nodes);
    let mut latest_balanced_cut = 0_usize;
    balance = 0;
    for (index, node) in nodes.iter().take(candidate).enumerate() {
        balance = balance
            .checked_add(facts(node).tool_delta)
            .ok_or(ContextBudgetError::TokenOverflow)?;
        if balance == 0 {
            latest_balanced_cut = index + 1;
        }
    }
    if latest_balanced_cut == 0 {
        return Ok(None);
    }

    let shadowed_token_count = nodes[..latest_balanced_cut]
        .iter()
        .try_fold(0_u64, |total, node| checked_add(total, facts(node).tokens))?;
    Ok(Some(CompactablePrefix {
        end_exclusive: latest_balanced_cut,
        shadowed_token_count,
    }))
}

fn estimate_content_block(block: &ContentBlock) -> Result<u64, ContextBudgetError> {
    match block.kind() {
        ContentBlockKind::Text { text } | ContentBlockKind::Reasoning { text } => {
            estimate_text_block(text)
        }
        ContentBlockKind::ToolCall {
            name, arguments, ..
        } => checked_add(
            checked_add(
                estimated_text_tokens(name)?,
                estimated_text_tokens(arguments)?,
            )?,
            BLOCK_OVERHEAD,
        ),
        ContentBlockKind::ToolResult { .. } => {
            let content = block
                .tool_result_content()
                .ok_or(ContextBudgetError::TokenOverflow)?;
            let nested = content.iter().try_fold(0_u64, |total, nested| {
                checked_add(total, estimate_content_value(nested)?)
            })?;
            checked_add(nested, BLOCK_OVERHEAD)
        }
        ContentBlockKind::Image { .. } | ContentBlockKind::Other { .. } => {
            estimate_unknown_block(block.raw().as_value())
        }
    }
}

fn estimate_content_value(value: &Value) -> Result<u64, ContextBudgetError> {
    let Some(fields) = value.as_object() else {
        return estimate_unknown_block(value);
    };
    match fields.get("type").and_then(Value::as_str) {
        Some("text" | "reasoning") => fields
            .get("text")
            .and_then(Value::as_str)
            .map(estimate_text_block)
            .unwrap_or_else(|| estimate_unknown_block(value)),
        Some("tool-call") => match (
            fields.get("name").and_then(Value::as_str),
            fields.get("arguments").and_then(Value::as_str),
        ) {
            (Some(name), Some(arguments)) => checked_add(
                checked_add(
                    estimated_text_tokens(name)?,
                    estimated_text_tokens(arguments)?,
                )?,
                BLOCK_OVERHEAD,
            ),
            _ => estimate_unknown_block(value),
        },
        Some("tool-result") => match fields.get("content").and_then(Value::as_array) {
            Some(content) => {
                let nested = content.iter().try_fold(0_u64, |total, nested| {
                    checked_add(total, estimate_content_value(nested)?)
                })?;
                checked_add(nested, BLOCK_OVERHEAD)
            }
            None => estimate_unknown_block(value),
        },
        _ => estimate_unknown_block(value),
    }
}

fn estimate_text_block(text: &str) -> Result<u64, ContextBudgetError> {
    checked_add(estimated_text_tokens(text)?, BLOCK_OVERHEAD)
}

fn estimated_text_tokens(text: &str) -> Result<u64, ContextBudgetError> {
    let units = u64::try_from(text.encode_utf16().count())
        .map_err(|_| ContextBudgetError::TokenOverflow)?;
    Ok(units.div_ceil(CHARS_PER_TOKEN))
}

fn estimate_unknown_block(value: &Value) -> Result<u64, ContextBudgetError> {
    let units = javascript_json_utf16_len(value)?;
    checked_add(units.div_ceil(CHARS_PER_TOKEN), BLOCK_OVERHEAD)
}

fn javascript_json_utf16_len(value: &Value) -> Result<u64, ContextBudgetError> {
    match value {
        Value::Null => Ok(4),
        Value::Bool(true) => Ok(4),
        Value::Bool(false) => Ok(5),
        Value::Number(number) => {
            if let Some(value) = number.as_u64() {
                return usize_to_u64(value.to_string().len());
            }
            if let Some(value) = number.as_i64() {
                return usize_to_u64(value.to_string().len());
            }
            let value = number.as_f64().ok_or(ContextBudgetError::TokenOverflow)?;
            let mut buffer = ryu_js::Buffer::new();
            usize_to_u64(buffer.format(value).len())
        }
        Value::String(value) => javascript_string_utf16_len(value),
        Value::Array(items) => {
            let mut total = 2_u64;
            for (index, item) in items.iter().enumerate() {
                if index != 0 {
                    total = checked_add(total, 1)?;
                }
                total = checked_add(total, javascript_json_utf16_len(item)?)?;
            }
            Ok(total)
        }
        Value::Object(fields) => {
            let mut total = 2_u64;
            for (index, (key, item)) in fields.iter().enumerate() {
                if index != 0 {
                    total = checked_add(total, 1)?;
                }
                total = checked_add(total, javascript_string_utf16_len(key)?)?;
                total = checked_add(total, 1)?;
                total = checked_add(total, javascript_json_utf16_len(item)?)?;
            }
            Ok(total)
        }
    }
}

fn javascript_string_utf16_len(value: &str) -> Result<u64, ContextBudgetError> {
    value.chars().try_fold(2_u64, |total, character| {
        let encoded = match character {
            '"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => usize_to_u64(character.len_utf16())?,
        };
        checked_add(total, encoded)
    })
}

fn checked_add(left: u64, right: u64) -> Result<u64, ContextBudgetError> {
    left.checked_add(right)
        .ok_or(ContextBudgetError::TokenOverflow)
}

fn usize_to_u64(value: usize) -> Result<u64, ContextBudgetError> {
    u64::try_from(value).map_err(|_| ContextBudgetError::TokenOverflow)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::model::{ContentBlock, JsonValue, Message, MessageSource, ToolSchema};

    use super::{
        ContextBudgetError, SurfacePriceFacts, estimate_message, select_compactable_prefix,
    };

    fn message(content: Vec<ContentBlock>) -> Message {
        Message::user("message", content, MessageSource::user().unwrap()).unwrap()
    }

    #[test]
    fn fixed_estimator_matches_upstream_utf16_and_block_rules() {
        // Fixed upstream:
        // packages/llm/token-meter/tests/context-breakdown-projection.spec.ts
        assert_eq!(
            estimate_message(&message(vec![ContentBlock::text("").unwrap()])).unwrap(),
            8
        );
        assert_eq!(
            estimate_message(&message(vec![ContentBlock::text("abcd").unwrap()])).unwrap(),
            9
        );
        assert_eq!(
            estimate_message(&message(vec![ContentBlock::text("😀").unwrap()])).unwrap(),
            9
        );
        assert_eq!(
            estimate_message(&message(vec![ContentBlock::text("😀".repeat(5)).unwrap()])).unwrap(),
            11
        );
        assert_eq!(
            estimate_message(&message(vec![
                ContentBlock::tool_call("ignored-id", "bash", "{}").unwrap(),
            ]))
            .unwrap(),
            10
        );
        assert_eq!(
            estimate_message(
                &Message::tool_result(
                    "result",
                    "call",
                    vec![ContentBlock::text("x".repeat(100)).unwrap()],
                    false,
                )
                .unwrap()
            )
            .unwrap(),
            37
        );
    }

    #[test]
    fn unknown_blocks_use_javascript_json_stringify_length() {
        let fixed = ContentBlock::from_value(json!({
            "type": "future",
            "text": "😀\n",
            "n": 0.000001
        }))
        .unwrap();
        let scientific = ContentBlock::from_value(json!({
            "type": "future",
            "text": "😀\n",
            "n": 0.0000001
        }))
        .unwrap();

        // Node's JSON.stringify lengths are 44 and 40 UTF-16 code units.
        assert_eq!(estimate_message(&message(vec![fixed])).unwrap(), 19);
        assert_eq!(estimate_message(&message(vec![scientific])).unwrap(), 18);

        let escaped = ContentBlock::from_value(json!({
            "type": "future",
            "text": "\0\"\\\u{0008}\t\n\u{000c}\r"
        }))
        .unwrap();
        // Node's JSON.stringify emits 47 UTF-16 code units here: generic C0
        // controls use `\uXXXX`, while the named escapes stay two units.
        assert_eq!(estimate_message(&message(vec![escaped])).unwrap(), 20);
    }

    #[test]
    fn request_header_prices_system_and_javascript_tool_schema_shape() {
        let tool = ToolSchema::new(
            "x",
            "😀\n",
            JsonValue::new(json!({
                "minimum": 0.000001,
                "exclusiveMinimum": 0.0000001
            }))
            .unwrap(),
        )
        .unwrap();

        assert_eq!(super::estimate_request_header(None, &[]).unwrap(), 0);
        assert_eq!(
            super::estimate_request_header(Some("abcd"), &[tool]).unwrap(),
            33
        );
    }

    #[derive(Clone, Copy)]
    struct Node {
        tokens: u64,
        tool_delta: i64,
    }

    fn facts(node: &Node) -> SurfacePriceFacts {
        SurfacePriceFacts {
            tokens: node.tokens,
            tool_delta: node.tool_delta,
        }
    }

    #[test]
    fn retention_cut_moves_headward_without_splitting_a_tool_pair() {
        let nodes = [
            Node {
                tokens: 10,
                tool_delta: 0,
            },
            Node {
                tokens: 10,
                tool_delta: 1,
            },
            Node {
                tokens: 10,
                tool_delta: -1,
            },
            Node {
                tokens: 10,
                tool_delta: 0,
            },
        ];
        let selected = select_compactable_prefix(&nodes, 15, 4_094, facts)
            .unwrap()
            .unwrap();

        assert_eq!(selected.end_exclusive, 1);
        assert_eq!(selected.shadowed_token_count, 10);
    }

    #[test]
    fn retention_target_uses_the_smallest_newest_tail_that_reaches_it() {
        let nodes = [
            Node {
                tokens: 10,
                tool_delta: 0,
            },
            Node {
                tokens: 20,
                tool_delta: 0,
            },
            Node {
                tokens: 30,
                tool_delta: 0,
            },
        ];

        let below = select_compactable_prefix(&nodes, 29, 4_094, facts)
            .unwrap()
            .unwrap();
        let exact = select_compactable_prefix(&nodes, 30, 4_094, facts)
            .unwrap()
            .unwrap();
        let one_over = select_compactable_prefix(&nodes, 31, 4_094, facts)
            .unwrap()
            .unwrap();

        assert_eq!(below.end_exclusive, 2);
        assert_eq!(exact.end_exclusive, 2);
        assert_eq!(one_over.end_exclusive, 1);
    }

    #[test]
    fn zero_retention_still_keeps_the_only_complete_tool_pair() {
        let nodes = [
            Node {
                tokens: 10,
                tool_delta: 1,
            },
            Node {
                tokens: 10,
                tool_delta: -1,
            },
        ];
        assert_eq!(
            select_compactable_prefix(&nodes, 0, 4_094, facts).unwrap(),
            None
        );
    }

    #[test]
    fn an_open_tool_pair_in_the_retained_tail_does_not_block_an_older_cut() {
        let nodes = [
            Node {
                tokens: 10,
                tool_delta: 0,
            },
            Node {
                tokens: 10,
                tool_delta: 1,
            },
        ];
        let selected = select_compactable_prefix(&nodes, 0, 4_094, facts)
            .unwrap()
            .unwrap();

        assert_eq!(selected.end_exclusive, 1);
        assert_eq!(selected.shadowed_token_count, 10);
    }

    #[test]
    fn provenance_cap_moves_only_toward_the_surface_head() {
        let nodes = vec![
            Node {
                tokens: 1,
                tool_delta: 0
            };
            4_096
        ];
        let selected = select_compactable_prefix(&nodes, 0, 4_094, facts)
            .unwrap()
            .unwrap();

        assert_eq!(selected.end_exclusive, 4_094);
        assert_eq!(selected.shadowed_token_count, 4_094);
    }

    #[test]
    fn provenance_cap_moves_before_a_tool_pair_crossing_the_cap() {
        let mut nodes = vec![
            Node {
                tokens: 1,
                tool_delta: 0,
            };
            4_093
        ];
        nodes.extend([
            Node {
                tokens: 1,
                tool_delta: 1,
            },
            Node {
                tokens: 1,
                tool_delta: -1,
            },
            Node {
                tokens: 1,
                tool_delta: 0,
            },
        ]);

        let selected = select_compactable_prefix(&nodes, 0, 4_094, facts)
            .unwrap()
            .unwrap();
        assert_eq!(selected.end_exclusive, 4_093);
        assert_eq!(selected.shadowed_token_count, 4_093);
    }

    #[test]
    fn tool_balance_rejects_a_leading_result_and_accepts_multiple_calls() {
        let leading_result = [
            Node {
                tokens: 1,
                tool_delta: -1,
            },
            Node {
                tokens: 1,
                tool_delta: 0,
            },
        ];
        assert_eq!(
            select_compactable_prefix(&leading_result, 0, 4_094, facts),
            Err(ContextBudgetError::UnbalancedToolSurface)
        );

        let multiple_calls = [
            Node {
                tokens: 1,
                tool_delta: 0,
            },
            Node {
                tokens: 1,
                tool_delta: 2,
            },
            Node {
                tokens: 1,
                tool_delta: -1,
            },
            Node {
                tokens: 1,
                tool_delta: -1,
            },
            Node {
                tokens: 1,
                tool_delta: 0,
            },
        ];
        let selected = select_compactable_prefix(&multiple_calls, 0, 4_094, facts)
            .unwrap()
            .unwrap();
        assert_eq!(selected.end_exclusive, 4);
        assert_eq!(selected.shadowed_token_count, 4);
    }
}
