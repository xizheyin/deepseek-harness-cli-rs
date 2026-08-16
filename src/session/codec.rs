//! Exact JSON snapshot boundary for headers and event envelopes.

use serde::{Deserialize, Serialize, ser::SerializeMap};
use serde_json::{Map, Value};

use crate::json_value::deserialize_present_option;
use crate::model::{CallId, JsonValue, Message, TokenUsage, TrueMarker};

use super::{
    EpochHeader, EventKind, EventSeq, RequestHeaderReason, SessionEvent, SessionHeader, StepId,
    SurfaceOp, TodoItem, ToolFailure, TurnEndReason, TurnId, UnixMillis, error::CodecError,
};

/// Maximum bytes accepted by the Phase 1 in-memory snapshot boundary.
pub const MAX_SESSION_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
/// Maximum compact payload/header bytes retained by one live in-memory session.
pub const MAX_SESSION_RETAINED_JSON_BYTES: usize = 16 * 1024 * 1024;
/// Maximum events retained by one in-memory Phase 1 session.
pub const MAX_SESSION_EVENTS: usize = 4_096;

#[derive(Serialize)]
struct SnapshotRef<'a> {
    header: &'a SessionHeader,
    events: &'a [SessionEvent],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotWire {
    header: Value,
    events: Vec<Value>,
}

/// Encode a deterministic in-memory interchange snapshot.
pub(crate) fn encode_snapshot(
    header: &SessionHeader,
    events: &[SessionEvent],
) -> Result<String, CodecError> {
    if events.len() > MAX_SESSION_EVENTS {
        return Err(CodecError::TooManyEvents {
            maximum: MAX_SESSION_EVENTS,
            actual: events.len(),
        });
    }
    let encoded =
        serde_json::to_string(&SnapshotRef { header, events }).map_err(CodecError::Encode)?;
    if encoded.len() > MAX_SESSION_SNAPSHOT_BYTES {
        return Err(CodecError::SnapshotTooLarge {
            maximum: MAX_SESSION_SNAPSHOT_BYTES,
            actual: encoded.len(),
        });
    }
    Ok(encoded)
}

/// Decode the fixed snapshot and exact event envelopes without exposing a partial session.
pub(crate) fn decode_snapshot(
    input: &str,
) -> Result<(SessionHeader, Vec<SessionEvent>), CodecError> {
    if input.len() > MAX_SESSION_SNAPSHOT_BYTES {
        return Err(CodecError::SnapshotTooLarge {
            maximum: MAX_SESSION_SNAPSHOT_BYTES,
            actual: input.len(),
        });
    }
    let root: Value = serde_json::from_str(input)?;
    let snapshot: SnapshotWire = serde_json::from_value(root).map_err(|error| {
        if error.to_string().contains("unknown field")
            || error.to_string().contains("missing field")
        {
            CodecError::SnapshotEnvelope
        } else {
            CodecError::Syntax(error)
        }
    })?;
    if snapshot.events.len() > MAX_SESSION_EVENTS {
        return Err(CodecError::TooManyEvents {
            maximum: MAX_SESSION_EVENTS,
            actual: snapshot.events.len(),
        });
    }
    let header = SessionHeader::from_value(snapshot.header)?;
    let events = snapshot
        .events
        .into_iter()
        .enumerate()
        .map(|(index, event)| decode_event(event, index))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((header, events))
}

impl Serialize for SessionEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(
            4 + usize::from(self.source_event_seqs.is_some())
                + usize::from(self.surface_op.is_some())
                + usize::from(self.ignorable.is_some()),
        ))?;
        map.serialize_entry("type", self.kind.event_type())?;
        map.serialize_entry("seq", &self.seq)?;
        map.serialize_entry("time", &self.time)?;
        map.serialize_entry("data", self.original_data.as_value())?;
        if let Some(sources) = &self.source_event_seqs {
            map.serialize_entry("sourceEventSeqs", sources)?;
        }
        if let Some(operation) = &self.surface_op {
            map.serialize_entry("surfaceOp", operation)?;
        }
        if let Some(ignorable) = &self.ignorable {
            map.serialize_entry("ignorable", ignorable)?;
        }
        map.end()
    }
}

pub(crate) fn decode_event(value: Value, index: usize) -> Result<SessionEvent, CodecError> {
    let Value::Object(mut fields) = value else {
        return Err(envelope_error(index, "event must be a JSON object"));
    };
    const ALLOWED: [&str; 7] = [
        "type",
        "seq",
        "time",
        "data",
        "sourceEventSeqs",
        "surfaceOp",
        "ignorable",
    ];
    if let Some(key) = fields.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(envelope_error(index, format!("unknown field {key:?}")));
    }
    let event_type = take_string(&mut fields, "type", index)?;
    if event_type == "request/header-delta" {
        return Err(envelope_error(
            index,
            "legacy request/header-delta is unsupported",
        ));
    }
    let seq = take_typed::<EventSeq>(&mut fields, "seq", index)?;
    let time = take_typed::<UnixMillis>(&mut fields, "time", index)?;
    let data = fields
        .remove("data")
        .ok_or_else(|| envelope_error(index, "missing data"))?;
    let source_event_seqs = take_optional::<Vec<EventSeq>>(&mut fields, "sourceEventSeqs", index)?;
    let surface_op = take_optional::<SurfaceOp>(&mut fields, "surfaceOp", index)?;
    let ignorable = take_optional::<TrueMarker>(&mut fields, "ignorable", index)?;
    let original_data = JsonValue::new(data.clone()).map_err(|error| CodecError::EventData {
        index,
        detail: error.to_string(),
    })?;
    let kind = decode_kind(
        &event_type,
        data,
        ignorable.is_some(),
        index,
        &original_data,
    )?;
    Ok(SessionEvent {
        seq,
        time,
        kind,
        surface_op,
        source_event_seqs,
        ignorable,
        original_data,
    })
}

fn decode_kind(
    event_type: &str,
    data: Value,
    ignorable: bool,
    index: usize,
    original_data: &JsonValue,
) -> Result<EventKind, CodecError> {
    macro_rules! payload {
        ($wire:ty) => {
            serde_json::from_value::<$wire>(data).map_err(|source| CodecError::EventPayload {
                index,
                event_type: event_type.to_owned(),
                source,
            })?
        };
    }
    Ok(match event_type {
        "turn/start" => {
            let data = payload!(TurnStartData);
            EventKind::TurnStart { turn: data.turn }
        }
        "turn/end" => {
            let data = payload!(TurnEndData);
            EventKind::TurnEnd {
                turn: data.turn,
                reason: data.reason,
            }
        }
        "step/start" => {
            let data = payload!(StepData);
            EventKind::StepStart {
                turn: data.turn,
                step: data.step,
            }
        }
        "step/end" => {
            let data = payload!(StepData);
            EventKind::StepEnd {
                turn: data.turn,
                step: data.step,
            }
        }
        "user/message" => EventKind::UserMessage {
            message: serde_json::from_value(data).map_err(|source| CodecError::EventPayload {
                index,
                event_type: event_type.to_owned(),
                source,
            })?,
        },
        "assistant/chunk" => {
            let data = payload!(AssistantChunkData);
            EventKind::AssistantChunk {
                turn: data.turn,
                step: data.step,
                chunk: data.chunk,
            }
        }
        "assistant/message" => {
            let data = payload!(AssistantMessageData);
            EventKind::AssistantMessage {
                turn: data.turn,
                step: data.step,
                message: data.message,
                usage: data.usage,
            }
        }
        "tool/call" => {
            let data = payload!(ToolCallData);
            EventKind::ToolCall {
                turn: data.turn,
                step: data.step,
                call_id: data.call_id,
                name: data.name,
                arguments: data.arguments,
            }
        }
        "tool/result" => {
            let data = payload!(ToolResultData);
            EventKind::ToolResult {
                turn: data.turn,
                step: data.step,
                message: data.message,
                error: data.error,
                meta: data.meta,
            }
        }
        "todo/write" => EventKind::TodoWrite {
            todos: payload!(TodoWriteData).todos,
        },
        "request/header" => {
            let data = payload!(RequestHeaderData);
            EventKind::RequestHeader {
                header: data.header,
                reason: data.reason,
            }
        }
        "request/context" => EventKind::RequestContext {
            context: serde_json::from_value(data).map_err(|source| CodecError::EventPayload {
                index,
                event_type: event_type.to_owned(),
                source,
            })?,
        },
        "llm/retry" => EventKind::LlmRetry {
            retry: payload!(super::LlmRetryEvent),
        },
        "llm/retry-started" => EventKind::LlmRetryStarted {
            started: payload!(super::LlmRetryStartedEvent),
        },
        "approval/asked" => EventKind::ApprovalAsked {
            asked: payload!(super::ApprovalAskedEvent),
        },
        "approval/decided" => EventKind::ApprovalDecided {
            decided: payload!(super::ApprovalDecidedEvent),
        },
        "session/end-seed" => {
            let _: EmptyData = payload!(EmptyData);
            EventKind::EndSeed
        }
        _ if ignorable => EventKind::Unknown {
            event_type: event_type.to_owned(),
            data: original_data.clone(),
        },
        _ => {
            return Err(CodecError::UnknownRequiredEvent {
                index,
                event_type: event_type.to_owned(),
            });
        }
    })
}

fn take_string(
    fields: &mut Map<String, Value>,
    field: &'static str,
    index: usize,
) -> Result<String, CodecError> {
    let Some(Value::String(value)) = fields.remove(field) else {
        return Err(envelope_error(index, format!("{field} must be a string")));
    };
    Ok(value)
}

fn take_typed<T: for<'de> Deserialize<'de>>(
    fields: &mut Map<String, Value>,
    field: &'static str,
    index: usize,
) -> Result<T, CodecError> {
    let value = fields
        .remove(field)
        .ok_or_else(|| envelope_error(index, format!("missing {field}")))?;
    serde_json::from_value(value)
        .map_err(|error| envelope_error(index, format!("invalid {field}: {error}")))
}

fn take_optional<T: for<'de> Deserialize<'de>>(
    fields: &mut Map<String, Value>,
    field: &'static str,
    index: usize,
) -> Result<Option<T>, CodecError> {
    fields
        .remove(field)
        .map(|value| {
            serde_json::from_value(value)
                .map_err(|error| envelope_error(index, format!("invalid {field}: {error}")))
        })
        .transpose()
}

fn envelope_error(index: usize, detail: impl Into<String>) -> CodecError {
    CodecError::EventEnvelope {
        index,
        detail: detail.into(),
    }
}

pub(crate) fn event_data_value(event: &SessionEvent) -> Result<Value, serde_json::Error> {
    Ok(event.original_data.as_value().clone())
}

pub(crate) fn kind_data_value(kind: &EventKind) -> Result<Value, serde_json::Error> {
    match kind {
        EventKind::TurnStart { turn } => serde_json::to_value(TurnStartData { turn: *turn }),
        EventKind::TurnEnd { turn, reason } => serde_json::to_value(TurnEndRef {
            turn: *turn,
            reason,
        }),
        EventKind::StepStart { turn, step } | EventKind::StepEnd { turn, step } => {
            serde_json::to_value(StepData {
                turn: *turn,
                step: *step,
            })
        }
        EventKind::UserMessage { message } => serde_json::to_value(message),
        EventKind::AssistantChunk { turn, step, chunk } => {
            serde_json::to_value(AssistantChunkRef {
                turn: *turn,
                step: *step,
                chunk,
            })
        }
        EventKind::AssistantMessage {
            turn,
            step,
            message,
            usage,
        } => serde_json::to_value(AssistantMessageRef {
            turn: *turn,
            step: *step,
            message,
            usage: usage.as_ref(),
        }),
        EventKind::ToolCall {
            turn,
            step,
            call_id,
            name,
            arguments,
        } => serde_json::to_value(ToolCallRef {
            turn: *turn,
            step: *step,
            call_id,
            name,
            arguments,
        }),
        EventKind::ToolResult {
            turn,
            step,
            message,
            error,
            meta,
        } => serde_json::to_value(ToolResultRef {
            turn: *turn,
            step: *step,
            message,
            error: error.as_ref(),
            meta: meta.as_ref(),
        }),
        EventKind::TodoWrite { todos } => serde_json::to_value(TodoWriteRef { todos }),
        EventKind::RequestHeader { header, reason } => {
            serde_json::to_value(RequestHeaderRef { header, reason })
        }
        EventKind::RequestContext { context } => serde_json::to_value(context),
        EventKind::LlmRetry { retry } => serde_json::to_value(retry),
        EventKind::LlmRetryStarted { started } => serde_json::to_value(started),
        EventKind::ApprovalAsked { asked } => serde_json::to_value(asked),
        EventKind::ApprovalDecided { decided } => serde_json::to_value(decided),
        EventKind::EndSeed => serde_json::to_value(EmptyData {}),
        EventKind::Unknown { data, .. } => Ok(data.as_value().clone()),
    }
}

#[derive(Deserialize, Serialize)]
struct TurnStartData {
    turn: TurnId,
}

#[derive(Deserialize)]
struct TurnEndData {
    turn: TurnId,
    reason: TurnEndReason,
}

#[derive(Serialize)]
struct TurnEndRef<'a> {
    turn: TurnId,
    reason: &'a TurnEndReason,
}

#[derive(Deserialize, Serialize)]
struct StepData {
    turn: TurnId,
    step: StepId,
}

#[derive(Deserialize)]
struct AssistantChunkData {
    turn: TurnId,
    step: StepId,
    chunk: crate::model::StreamChunk,
}

#[derive(Serialize)]
struct AssistantChunkRef<'a> {
    turn: TurnId,
    step: StepId,
    chunk: &'a crate::model::StreamChunk,
}

#[derive(Deserialize)]
struct AssistantMessageData {
    turn: TurnId,
    step: StepId,
    message: Message,
    #[serde(default, deserialize_with = "deserialize_present_option")]
    usage: Option<TokenUsage>,
}

#[derive(Serialize)]
struct AssistantMessageRef<'a> {
    turn: TurnId,
    step: StepId,
    message: &'a Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<&'a TokenUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallData {
    turn: TurnId,
    step: StepId,
    call_id: CallId,
    name: String,
    arguments: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallRef<'a> {
    turn: TurnId,
    step: StepId,
    call_id: &'a CallId,
    name: &'a str,
    arguments: &'a str,
}

#[derive(Deserialize)]
struct ToolResultData {
    turn: TurnId,
    step: StepId,
    message: Message,
    #[serde(default)]
    error: Option<ToolFailure>,
    #[serde(default)]
    meta: Option<JsonValue>,
}

#[derive(Serialize)]
struct ToolResultRef<'a> {
    turn: TurnId,
    step: StepId,
    message: &'a Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<&'a ToolFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    meta: Option<&'a JsonValue>,
}

#[derive(Deserialize)]
struct TodoWriteData {
    todos: Vec<TodoItem>,
}

#[derive(Serialize)]
struct TodoWriteRef<'a> {
    todos: &'a [TodoItem],
}

#[derive(Deserialize)]
struct RequestHeaderData {
    header: EpochHeader,
    reason: RequestHeaderReason,
}

#[derive(Serialize)]
struct RequestHeaderRef<'a> {
    header: &'a EpochHeader,
    reason: &'a RequestHeaderReason,
}

#[derive(Deserialize, Serialize)]
struct EmptyData {}
