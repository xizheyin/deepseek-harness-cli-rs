use std::collections::BTreeSet;

use thiserror::Error;

use crate::{
    json_value::MAX_SAFE_INTEGER,
    model::{JsonValue, ToolSchema},
};

use super::{
    MAX_TOOLS_PER_PLUGIN, is_plugin_id, is_plugin_tool_name,
    json::parse_strict_json,
    schema::{CompiledPluginSchema, SchemaRoot},
};

const PROTOCOL_VERSION: u64 = 1;
const MAX_PROTOCOL_LINE_BYTES: usize = 128 * 1024;
const MAX_PLUGIN_VALUE_BYTES: usize = 64 * 1024;
const MAX_TOOL_DESCRIPTION_BYTES: usize = 1024;
const MAX_PLUGIN_ERROR_MESSAGE_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PluginProtocolError {
    #[error("plugin protocol record exceeds its size limit")]
    RecordTooLarge,
    #[error("plugin protocol record has invalid NDJSON framing")]
    InvalidFraming,
    #[error("plugin sent a message reserved for the host")]
    WrongDirection,
    #[error("plugin protocol message is invalid")]
    InvalidMessage,
    #[error("plugin protocol message could not be encoded")]
    Encode,
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct PluginCallId(u64);

impl PluginCallId {
    pub(crate) fn new(value: u64) -> Result<Self, PluginProtocolError> {
        if value == 0 || value > MAX_SAFE_INTEGER {
            return Err(PluginProtocolError::InvalidMessage);
        }
        Ok(Self(value))
    }

    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone)]
pub(crate) struct PluginTool {
    model_schema: ToolSchema,
    parameter_schema: CompiledPluginSchema,
    output_schema: CompiledPluginSchema,
}

impl std::fmt::Debug for PluginTool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginTool")
            .field("name", &self.model_schema.name())
            .finish_non_exhaustive()
    }
}

impl PluginTool {
    pub(crate) fn model_schema(&self) -> &ToolSchema {
        &self.model_schema
    }

    pub(crate) fn output_schema(&self) -> &CompiledPluginSchema {
        &self.output_schema
    }

    pub(crate) fn parameter_schema(&self) -> &CompiledPluginSchema {
        &self.parameter_schema
    }
}

#[derive(Clone)]
pub(crate) struct PluginHello {
    plugin_id: String,
    tools: Box<[PluginTool]>,
}

impl std::fmt::Debug for PluginHello {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginHello")
            .field("plugin_id", &self.plugin_id)
            .field("tool_count", &self.tools.len())
            .finish()
    }
}

impl PluginHello {
    pub(crate) fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    pub(crate) fn tools(&self) -> &[PluginTool] {
        &self.tools
    }
}

#[derive(Clone)]
pub(crate) struct PluginDeclaredError {
    code: String,
    message: String,
}

impl std::fmt::Debug for PluginDeclaredError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PluginDeclaredError")
            .field("code", &self.code)
            .field("message_bytes", &self.message.len())
            .finish()
    }
}

impl PluginDeclaredError {
    pub(crate) fn code(&self) -> &str {
        &self.code
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone)]
pub(crate) enum PluginResultPayload {
    Success(JsonValue),
    Failure(PluginDeclaredError),
}

impl std::fmt::Debug for PluginResultPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success(value) => formatter
                .debug_tuple("Success")
                .field(&format_args!("{} encoded bytes", value.encoded_len()))
                .finish(),
            Self::Failure(error) => formatter.debug_tuple("Failure").field(error).finish(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PluginResult {
    id: PluginCallId,
    payload: PluginResultPayload,
}

impl PluginResult {
    pub(crate) fn id(&self) -> PluginCallId {
        self.id
    }

    pub(crate) fn payload(&self) -> &PluginResultPayload {
        &self.payload
    }
}

#[derive(Clone, Debug)]
pub(crate) enum PluginMessage {
    Hello(PluginHello),
    Result(PluginResult),
}

pub(crate) fn parse_plugin_line(line_with_lf: &[u8]) -> Result<PluginMessage, PluginProtocolError> {
    if line_with_lf.len() > MAX_PROTOCOL_LINE_BYTES {
        return Err(PluginProtocolError::RecordTooLarge);
    }
    let Some(body) = line_with_lf.strip_suffix(b"\n") else {
        return Err(PluginProtocolError::InvalidFraming);
    };
    if body.first() != Some(&b'{')
        || body.last() != Some(&b'}')
        || body.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
    {
        return Err(PluginProtocolError::InvalidFraming);
    }
    let value = parse_strict_json(body).map_err(|_| PluginProtocolError::InvalidMessage)?;
    let fields = value
        .as_object()
        .ok_or(PluginProtocolError::InvalidMessage)?;
    let message_type = fields
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(PluginProtocolError::InvalidMessage)?;
    match message_type {
        "hello" => parse_hello(fields).map(PluginMessage::Hello),
        "result" => parse_result(fields).map(PluginMessage::Result),
        "call" | "cancel" => Err(PluginProtocolError::WrongDirection),
        _ => Err(PluginProtocolError::InvalidMessage),
    }
}

pub(crate) fn encode_hello(plugin_id: &str) -> Result<Vec<u8>, PluginProtocolError> {
    if !is_plugin_id(plugin_id) {
        return Err(PluginProtocolError::InvalidMessage);
    }
    encode_line(serde_json::json!({
        "version":PROTOCOL_VERSION,
        "type":"hello",
        "plugin_id":plugin_id
    }))
}

pub(crate) fn encode_call(
    id: PluginCallId,
    tool: &str,
    arguments: &JsonValue,
) -> Result<Vec<u8>, PluginProtocolError> {
    if !is_plugin_tool_name(tool) || arguments.encoded_len() > MAX_PLUGIN_VALUE_BYTES {
        return Err(PluginProtocolError::InvalidMessage);
    }
    encode_line(serde_json::json!({
        "version":PROTOCOL_VERSION,
        "type":"call",
        "id":id.get(),
        "tool":tool,
        "arguments":arguments
    }))
}

pub(crate) fn encode_cancel(id: PluginCallId) -> Result<Vec<u8>, PluginProtocolError> {
    encode_line(serde_json::json!({
        "version":PROTOCOL_VERSION,
        "type":"cancel",
        "id":id.get()
    }))
}

fn parse_hello(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<PluginHello, PluginProtocolError> {
    require_exact_keys(fields, &["version", "type", "plugin_id", "tools"])?;
    require_version(fields)?;
    let plugin_id = fields
        .get("plugin_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| is_plugin_id(value))
        .ok_or(PluginProtocolError::InvalidMessage)?
        .to_owned();
    let raw_tools = fields
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .filter(|tools| tools.len() <= MAX_TOOLS_PER_PLUGIN)
        .ok_or(PluginProtocolError::InvalidMessage)?;
    let mut names = BTreeSet::new();
    let mut tools = Vec::new();
    tools
        .try_reserve_exact(raw_tools.len())
        .map_err(|_| PluginProtocolError::InvalidMessage)?;
    for raw_tool in raw_tools {
        let tool = parse_tool(raw_tool)?;
        if !names.insert(tool.model_schema.name().to_owned()) {
            return Err(PluginProtocolError::InvalidMessage);
        }
        tools.push(tool);
    }
    Ok(PluginHello {
        plugin_id,
        tools: tools.into_boxed_slice(),
    })
}

fn parse_tool(value: &serde_json::Value) -> Result<PluginTool, PluginProtocolError> {
    let fields = value
        .as_object()
        .ok_or(PluginProtocolError::InvalidMessage)?;
    require_exact_keys(fields, &["name", "description", "parameters", "output"])?;
    let name = fields
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|value| is_plugin_tool_name(value))
        .ok_or(PluginProtocolError::InvalidMessage)?;
    let description = fields
        .get("description")
        .and_then(serde_json::Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= MAX_TOOL_DESCRIPTION_BYTES
                && !value.chars().any(char::is_control)
        })
        .ok_or(PluginProtocolError::InvalidMessage)?;
    let parameters = JsonValue::new(
        fields
            .get("parameters")
            .cloned()
            .ok_or(PluginProtocolError::InvalidMessage)?,
    )
    .map_err(|_| PluginProtocolError::InvalidMessage)?;
    let output = JsonValue::new(
        fields
            .get("output")
            .cloned()
            .ok_or(PluginProtocolError::InvalidMessage)?,
    )
    .map_err(|_| PluginProtocolError::InvalidMessage)?;
    let parameter_schema = CompiledPluginSchema::compile(parameters, SchemaRoot::Parameters)
        .map_err(|_| PluginProtocolError::InvalidMessage)?;
    let output_schema = CompiledPluginSchema::compile(output, SchemaRoot::Output)
        .map_err(|_| PluginProtocolError::InvalidMessage)?;
    let model_schema = ToolSchema::new(name, description, parameter_schema.raw().clone())
        .map_err(|_| PluginProtocolError::InvalidMessage)?;
    Ok(PluginTool {
        model_schema,
        parameter_schema,
        output_schema,
    })
}

fn parse_result(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<PluginResult, PluginProtocolError> {
    require_version(fields)?;
    let id = fields
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or(PluginProtocolError::InvalidMessage)
        .and_then(PluginCallId::new)?;
    let ok = fields
        .get("ok")
        .and_then(serde_json::Value::as_bool)
        .ok_or(PluginProtocolError::InvalidMessage)?;
    let payload = if ok {
        require_exact_keys(fields, &["version", "type", "id", "ok", "value"])?;
        let value = JsonValue::new(
            fields
                .get("value")
                .cloned()
                .ok_or(PluginProtocolError::InvalidMessage)?,
        )
        .map_err(|_| PluginProtocolError::InvalidMessage)?;
        if value.encoded_len() > MAX_PLUGIN_VALUE_BYTES {
            return Err(PluginProtocolError::InvalidMessage);
        }
        PluginResultPayload::Success(value)
    } else {
        require_exact_keys(fields, &["version", "type", "id", "ok", "error"])?;
        PluginResultPayload::Failure(parse_declared_error(
            fields
                .get("error")
                .ok_or(PluginProtocolError::InvalidMessage)?,
        )?)
    };
    Ok(PluginResult { id, payload })
}

fn parse_declared_error(
    value: &serde_json::Value,
) -> Result<PluginDeclaredError, PluginProtocolError> {
    let fields = value
        .as_object()
        .ok_or(PluginProtocolError::InvalidMessage)?;
    require_exact_keys(fields, &["code", "message"])?;
    let code = fields
        .get("code")
        .and_then(serde_json::Value::as_str)
        .filter(|value| is_plugin_error_code(value))
        .ok_or(PluginProtocolError::InvalidMessage)?
        .to_owned();
    let message = fields
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.len() <= MAX_PLUGIN_ERROR_MESSAGE_BYTES)
        .ok_or(PluginProtocolError::InvalidMessage)?
        .to_owned();
    Ok(PluginDeclaredError { code, message })
}

fn require_version(
    fields: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), PluginProtocolError> {
    if fields.get("version").and_then(serde_json::Value::as_u64) == Some(PROTOCOL_VERSION) {
        Ok(())
    } else {
        Err(PluginProtocolError::InvalidMessage)
    }
}

fn require_exact_keys(
    fields: &serde_json::Map<String, serde_json::Value>,
    expected: &[&str],
) -> Result<(), PluginProtocolError> {
    if fields.len() == expected.len() && fields.keys().all(|key| expected.contains(&key.as_str())) {
        Ok(())
    } else {
        Err(PluginProtocolError::InvalidMessage)
    }
}

fn is_plugin_error_code(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z'))
        && value.len() <= 64
        && bytes.all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

fn encode_line(value: serde_json::Value) -> Result<Vec<u8>, PluginProtocolError> {
    let mut encoded = serde_json::to_vec(&value).map_err(|_| PluginProtocolError::Encode)?;
    let final_len = encoded
        .len()
        .checked_add(1)
        .ok_or(PluginProtocolError::RecordTooLarge)?;
    if final_len > MAX_PROTOCOL_LINE_BYTES {
        return Err(PluginProtocolError::RecordTooLarge);
    }
    encoded
        .try_reserve_exact(1)
        .map_err(|_| PluginProtocolError::Encode)?;
    encoded.push(b'\n');
    Ok(encoded)
}

#[cfg(test)]
mod tests {
    use crate::model::JsonValue;

    use super::{
        MAX_PLUGIN_VALUE_BYTES, MAX_PROTOCOL_LINE_BYTES, PluginCallId, PluginMessage,
        PluginProtocolError, PluginResultPayload, encode_call, encode_cancel, encode_hello,
        parse_plugin_line,
    };

    fn hello_line() -> Vec<u8> {
        concat!(
            r#"{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}"#,
            "\n"
        )
        .as_bytes()
        .to_vec()
    }

    #[test]
    fn hello_compiles_host_only_output_schema() {
        let PluginMessage::Hello(hello) = parse_plugin_line(&hello_line()).unwrap() else {
            panic!("expected plugin hello")
        };
        assert_eq!(hello.plugin_id(), "text-tools");
        assert_eq!(hello.tools().len(), 1);
        let tool = &hello.tools()[0];
        assert_eq!(tool.model_schema().name(), "text_stats");
        assert!(tool.model_schema().raw().as_value().get("output").is_none());
        assert!(
            tool.output_schema()
                .validate(&JsonValue::new(serde_json::json!({"words":2})).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn result_null_is_present_and_declared_errors_are_closed() {
        let PluginMessage::Result(result) = parse_plugin_line(
            concat!(
                r#"{"version":1,"type":"result","id":1,"ok":true,"value":null}"#,
                "\n"
            )
            .as_bytes(),
        )
        .unwrap() else {
            panic!("expected plugin result")
        };
        assert_eq!(result.id(), PluginCallId::new(1).unwrap());
        assert!(
            matches!(result.payload(), PluginResultPayload::Success(value) if value.as_value().is_null())
        );

        assert!(
            parse_plugin_line(
                concat!(
                    r#"{"version":1,"type":"result","id":1,"ok":false,"error":{"code":"BAD_INPUT","message":"bad","extra":1}}"#,
                    "\n"
                )
                .as_bytes()
            )
            .is_err()
        );
    }

    #[test]
    fn framing_and_duplicate_keys_fail_closed() {
        assert_eq!(
            parse_plugin_line(b" {}\n").unwrap_err(),
            PluginProtocolError::InvalidFraming
        );
        assert_eq!(
            parse_plugin_line(b"{}\r\n").unwrap_err(),
            PluginProtocolError::InvalidFraming
        );
        assert_eq!(
            parse_plugin_line(b"{} ").unwrap_err(),
            PluginProtocolError::InvalidFraming
        );
        assert_eq!(
            parse_plugin_line(b"{\r\"type\":\"result\"}\n").unwrap_err(),
            PluginProtocolError::InvalidFraming
        );
        assert_eq!(
            parse_plugin_line(b"{\n\"type\":\"result\"}\n").unwrap_err(),
            PluginProtocolError::InvalidFraming
        );
        assert!(
            parse_plugin_line(
                concat!(
                    r#"{"version":1,"type":"result","id":1,"\u0069d":2,"ok":true,"value":null}"#,
                    "\n"
                )
                .as_bytes()
            )
            .is_err()
        );
        let oversized = vec![b'x'; MAX_PROTOCOL_LINE_BYTES + 1];
        assert_eq!(
            parse_plugin_line(&oversized).unwrap_err(),
            PluginProtocolError::RecordTooLarge
        );
    }

    #[test]
    fn record_and_runtime_value_limits_accept_exact_and_reject_one_over() {
        let prefix = b"{\"padding\":\"";
        let suffix = b"\"}\n";
        let padding = MAX_PROTOCOL_LINE_BYTES - prefix.len() - suffix.len();
        let mut exact = Vec::with_capacity(MAX_PROTOCOL_LINE_BYTES);
        exact.extend_from_slice(prefix);
        exact.extend(std::iter::repeat_n(b'x', padding));
        exact.extend_from_slice(suffix);
        assert_eq!(exact.len(), MAX_PROTOCOL_LINE_BYTES);
        assert_eq!(
            parse_plugin_line(&exact).unwrap_err(),
            PluginProtocolError::InvalidMessage
        );
        exact.insert(exact.len() - suffix.len(), b'x');
        assert_eq!(
            parse_plugin_line(&exact).unwrap_err(),
            PluginProtocolError::RecordTooLarge
        );

        let exact_value = format!(
            "{{\"version\":1,\"type\":\"result\",\"id\":1,\"ok\":true,\"value\":\"{}\"}}\n",
            "x".repeat(MAX_PLUGIN_VALUE_BYTES - 2)
        );
        assert!(parse_plugin_line(exact_value.as_bytes()).is_ok());
        let one_over_value = format!(
            "{{\"version\":1,\"type\":\"result\",\"id\":1,\"ok\":true,\"value\":\"{}\"}}\n",
            "x".repeat(MAX_PLUGIN_VALUE_BYTES - 1)
        );
        assert!(parse_plugin_line(one_over_value.as_bytes()).is_err());
    }

    #[test]
    fn malformed_encoding_partial_records_and_unknown_fields_fail_closed() {
        for line in [
            &b""[..],
            &b"{}"[..],
            &b"\xef\xbb\xbf{}\n"[..],
            &b"{}\n{}\n"[..],
            &b"{\xff}\n"[..],
            &b"\n"[..],
        ] {
            assert!(parse_plugin_line(line).is_err());
        }
        for line in [
            concat!(
                r#"{"version":1,"type":"result","id":1,"ok":true,"value":null,"extra":1}"#,
                "\n"
            ),
            concat!(
                r#"{"version":1,"type":"result","id":1,"ok":false,"value":null,"error":{"code":"BAD","message":"bad"}}"#,
                "\n"
            ),
            concat!(r#"{"version":1,"type":"result","id":1,"ok":true}"#, "\n"),
        ] {
            assert!(parse_plugin_line(line.as_bytes()).is_err());
        }
    }

    #[test]
    fn call_ids_must_be_literal_positive_safe_integers() {
        for id in ["0", "-1", "1.0", "1e0", "9007199254740992"] {
            let line = format!(
                "{{\"version\":1,\"type\":\"result\",\"id\":{id},\"ok\":true,\"value\":null}}\n"
            );
            assert!(parse_plugin_line(line.as_bytes()).is_err(), "accepted {id}");
        }
        let maximum =
            "{\"version\":1,\"type\":\"result\",\"id\":9007199254740991,\"ok\":true,\"value\":null}\n"
                .to_owned();
        assert!(parse_plugin_line(maximum.as_bytes()).is_ok());
    }

    #[test]
    fn host_messages_are_compact_bounded_and_never_parse_as_plugin_messages() {
        let id = PluginCallId::new(1).unwrap();
        let hello = encode_hello("text-tools").unwrap();
        let arguments = JsonValue::new(serde_json::json!({"text":"line\nnext"})).unwrap();
        let call = encode_call(id, "text_stats", &arguments).unwrap();
        let cancel = encode_cancel(id).unwrap();
        for line in [&hello, &call, &cancel] {
            assert_eq!(line.last(), Some(&b'\n'));
            assert_eq!(line.iter().filter(|byte| **byte == b'\n').count(), 1);
        }
        assert_eq!(
            parse_plugin_line(&hello).unwrap_err(),
            PluginProtocolError::InvalidMessage
        );
        assert_eq!(
            parse_plugin_line(&call).unwrap_err(),
            PluginProtocolError::WrongDirection
        );
        assert_eq!(
            parse_plugin_line(&cancel).unwrap_err(),
            PluginProtocolError::WrongDirection
        );
    }
}
