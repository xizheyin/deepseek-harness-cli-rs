use std::io::{self, Read, Write as _};

pub const MAX_RECORD_BYTES: usize = 128 * 1024;
const MAX_SAFE_ID: u64 = 9_007_199_254_740_991;

pub struct ToolDeclaration {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
    pub output: serde_json::Value,
}

pub struct PluginError {
    pub code: &'static str,
    pub message: String,
}

pub fn run(
    plugin_id: &'static str,
    tools: Vec<ToolDeclaration>,
    handle: impl Fn(&str, &serde_json::Value) -> Result<serde_json::Value, PluginError>,
) -> io::Result<()> {
    if std::env::var("DSH_PLUGIN_PROTOCOL").ok().as_deref() != Some("1")
        || std::env::var("DSH_PLUGIN_ID").ok().as_deref() != Some(plugin_id)
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "plugin environment is invalid",
        ));
    }

    let stdin = io::stdin();
    let mut records = RecordReader::new(stdin.lock());
    let host_hello = records
        .next_record()?
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "missing host hello"))?;
    validate_host_hello(&host_hello, plugin_id)?;

    let declarations = tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters,
                "output": tool.output,
            })
        })
        .collect::<Vec<_>>();
    write_record(&serde_json::json!({
        "version": 1,
        "type": "hello",
        "plugin_id": plugin_id,
        "tools": declarations,
    }))?;

    while let Some(record) = records.next_record()? {
        let fields = record
            .as_object()
            .ok_or_else(|| invalid_data("protocol record is not an object"))?;
        match fields.get("type").and_then(serde_json::Value::as_str) {
            Some("call") => {
                require_exact_keys(fields, &["version", "type", "id", "tool", "arguments"])?;
                require_version(fields)?;
                let id = require_id(fields)?;
                let tool = fields
                    .get("tool")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| invalid_data("call tool is invalid"))?;
                let arguments = fields
                    .get("arguments")
                    .ok_or_else(|| invalid_data("call arguments are missing"))?;
                let result = handle(tool, arguments);
                match result {
                    Ok(value) => write_record(&serde_json::json!({
                        "version": 1,
                        "type": "result",
                        "id": id,
                        "ok": true,
                        "value": value,
                    }))?,
                    Err(error) => write_record(&serde_json::json!({
                        "version": 1,
                        "type": "result",
                        "id": id,
                        "ok": false,
                        "error": {"code": error.code, "message": error.message},
                    }))?,
                }
            }
            Some("cancel") => {
                require_exact_keys(fields, &["version", "type", "id"])?;
                require_version(fields)?;
                let _ = require_id(fields)?;
            }
            _ => return Err(invalid_data("unsupported host record")),
        }
    }
    Ok(())
}

// This shared source is compiled once per example binary; text_stats does not
// need the stricter helper that json_format deliberately demonstrates.
#[allow(dead_code)]
pub fn parse_strict_value(text: &str) -> Result<serde_json::Value, ()> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let mut nodes = 0_usize;
    let value = serde::de::DeserializeSeed::deserialize(
        StrictSeed {
            depth: 0,
            nodes: &mut nodes,
        },
        &mut deserializer,
    )
    .map_err(|_| ())?;
    deserializer.end().map_err(|_| ())?;
    Ok(value)
}

fn validate_host_hello(value: &serde_json::Value, plugin_id: &str) -> io::Result<()> {
    let fields = value
        .as_object()
        .ok_or_else(|| invalid_data("host hello is not an object"))?;
    require_exact_keys(fields, &["version", "type", "plugin_id"])?;
    require_version(fields)?;
    if fields.get("type").and_then(serde_json::Value::as_str) != Some("hello")
        || fields.get("plugin_id").and_then(serde_json::Value::as_str) != Some(plugin_id)
    {
        return Err(invalid_data("host hello does not match this plugin"));
    }
    Ok(())
}

fn require_version(fields: &serde_json::Map<String, serde_json::Value>) -> io::Result<()> {
    if fields.get("version").and_then(serde_json::Value::as_u64) == Some(1) {
        Ok(())
    } else {
        Err(invalid_data("unsupported protocol version"))
    }
}

fn require_id(fields: &serde_json::Map<String, serde_json::Value>) -> io::Result<u64> {
    fields
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .filter(|id| (1..=MAX_SAFE_ID).contains(id))
        .ok_or_else(|| invalid_data("protocol call ID is invalid"))
}

fn require_exact_keys(
    fields: &serde_json::Map<String, serde_json::Value>,
    expected: &[&str],
) -> io::Result<()> {
    if fields.len() == expected.len() && expected.iter().all(|key| fields.contains_key(*key)) {
        Ok(())
    } else {
        Err(invalid_data("protocol record fields are invalid"))
    }
}

fn write_record(value: &serde_json::Value) -> io::Result<()> {
    let mut encoded = serde_json::to_vec(value).map_err(|_| invalid_data("encode failed"))?;
    if encoded.len().saturating_add(1) > MAX_RECORD_BYTES {
        return Err(invalid_data("encoded record is too large"));
    }
    encoded.push(b'\n');
    let mut stdout = io::stdout().lock();
    stdout.write_all(&encoded)?;
    stdout.flush()
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

struct RecordReader<R> {
    input: R,
    pending: Vec<u8>,
}

impl<R: Read> RecordReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            pending: Vec::new(),
        }
    }

    fn next_record(&mut self) -> io::Result<Option<serde_json::Value>> {
        loop {
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let end = end + 1;
                if end > MAX_RECORD_BYTES {
                    return Err(invalid_data("protocol record is too large"));
                }
                let record = self.pending.drain(..end).collect::<Vec<_>>();
                let body = &record[..record.len() - 1];
                if body.first() != Some(&b'{')
                    || body.last() != Some(&b'}')
                    || body.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
                {
                    return Err(invalid_data("protocol framing is invalid"));
                }
                return parse_strict_bytes(body).map(Some);
            }
            if self.pending.len() == MAX_RECORD_BYTES {
                return Err(invalid_data("protocol record is too large"));
            }
            let mut buffer = [0_u8; 8 * 1024];
            let count = self.input.read(&mut buffer)?;
            if count == 0 {
                return if self.pending.is_empty() {
                    Ok(None)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "partial protocol record",
                    ))
                };
            }
            if self.pending.len().saturating_add(count) > MAX_RECORD_BYTES {
                return Err(invalid_data("protocol record is too large"));
            }
            self.pending.extend_from_slice(&buffer[..count]);
        }
    }
}

fn parse_strict_bytes(bytes: &[u8]) -> io::Result<serde_json::Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let mut nodes = 0_usize;
    let value = serde::de::DeserializeSeed::deserialize(
        StrictSeed {
            depth: 0,
            nodes: &mut nodes,
        },
        &mut deserializer,
    )
    .map_err(|_| invalid_data("JSON record is invalid"))?;
    deserializer
        .end()
        .map_err(|_| invalid_data("JSON record has trailing data"))?;
    Ok(value)
}

struct StrictSeed<'a> {
    depth: usize,
    nodes: &'a mut usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for StrictSeed<'_> {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        *self.nodes = self.nodes.saturating_add(1);
        if *self.nodes > 65_536 {
            return Err(serde::de::Error::custom("too many JSON nodes"));
        }
        deserializer.deserialize_any(StrictVisitor { seed: self })
    }
}

struct StrictVisitor<'a> {
    seed: StrictSeed<'a>,
}

impl<'de> serde::de::Visitor<'de> for StrictVisitor<'_> {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(value.into())
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        serde::de::DeserializeSeed::deserialize(self.seed, deserializer)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        if self.seed.depth >= 64 {
            return Err(serde::de::Error::custom("JSON nesting is too deep"));
        }
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(StrictSeed {
            depth: self.seed.depth + 1,
            nodes: self.seed.nodes,
        })? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        if self.seed.depth >= 64 {
            return Err(serde::de::Error::custom("JSON nesting is too deep"));
        }
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom("duplicate JSON key"));
            }
            let value = map.next_value_seed(StrictSeed {
                depth: self.seed.depth + 1,
                nodes: self.seed.nodes,
            })?;
            values.insert(key, value);
        }
        Ok(serde_json::Value::Object(values))
    }
}
