use std::{cell::Cell, fmt};

use serde::de::{DeserializeSeed, Error as _, MapAccess, SeqAccess, Visitor};

const MAX_PARSED_JSON_DEPTH: usize = 64;
const MAX_PARSED_JSON_NODES: usize = 65_536;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StrictJsonError;

pub(super) fn parse_strict_json(bytes: &[u8]) -> Result<serde_json::Value, StrictJsonError> {
    let nodes = Cell::new(0_usize);
    let seed = StrictSeed {
        depth: 0,
        nodes: &nodes,
    };
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = seed
        .deserialize(&mut deserializer)
        .map_err(|_| StrictJsonError)?;
    deserializer.end().map_err(|_| StrictJsonError)?;
    Ok(value)
}

#[derive(Clone, Copy)]
struct StrictSeed<'a> {
    depth: usize,
    nodes: &'a Cell<usize>,
}

impl<'de> DeserializeSeed<'de> for StrictSeed<'_> {
    type Value = serde_json::Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let next = self
            .nodes
            .get()
            .checked_add(1)
            .ok_or_else(|| D::Error::custom("JSON node count overflowed"))?;
        if next > MAX_PARSED_JSON_NODES {
            return Err(D::Error::custom("JSON node count exceeds the plugin limit"));
        }
        self.nodes.set(next);
        deserializer.deserialize_any(StrictVisitor { seed: self })
    }
}

struct StrictVisitor<'a> {
    seed: StrictSeed<'a>,
}

impl<'de> Visitor<'de> for StrictVisitor<'_> {
    type Value = serde_json::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a bounded JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| E::custom("JSON number is not finite"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value.to_owned()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(serde_json::Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(serde_json::Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        if self.seed.depth >= MAX_PARSED_JSON_DEPTH {
            return Err(A::Error::custom("JSON nesting exceeds the plugin limit"));
        }
        let child = StrictSeed {
            depth: self.seed.depth + 1,
            nodes: self.seed.nodes,
        };
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(child)? {
            values.push(value);
        }
        Ok(serde_json::Value::Array(values))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        if self.seed.depth >= MAX_PARSED_JSON_DEPTH {
            return Err(A::Error::custom("JSON nesting exceeds the plugin limit"));
        }
        let child = StrictSeed {
            depth: self.seed.depth + 1,
            nodes: self.seed.nodes,
        };
        let mut values = serde_json::Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(A::Error::custom("duplicate JSON object key"));
            }
            let value = object.next_value_seed(child)?;
            values.insert(key, value);
        }
        Ok(serde_json::Value::Object(values))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_strict_json;

    #[test]
    fn strict_json_rejects_duplicate_trailing_and_excessively_deep_values() {
        assert!(parse_strict_json(br#"{"a":1,"a":2}"#).is_err());
        assert!(parse_strict_json(br#"{"id":1,"\u0069d":2}"#).is_err());
        assert!(parse_strict_json(br#"{"a":{"b":1,"b":2}}"#).is_err());
        assert!(parse_strict_json(br#"{} {}"#).is_err());

        let mut deep = String::new();
        for _ in 0..66 {
            deep.push('[');
        }
        deep.push_str("null");
        for _ in 0..66 {
            deep.push(']');
        }
        assert!(parse_strict_json(deep.as_bytes()).is_err());
    }

    #[test]
    fn strict_json_preserves_the_complete_supported_value_domain() {
        let value =
            parse_strict_json(br#"{"a":[null,true,-2,3,1.5,"text"],"object":{"x":1}}"#).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"a":[null,true,-2,3,1.5,"text"],"object":{"x":1}})
        );
    }

    #[test]
    fn strict_json_accepts_64_container_levels_and_rejects_65() {
        for (open, close) in [('[', ']'), ('{', '}')] {
            let nested = |depth: usize| {
                let mut value = String::new();
                for _ in 0..depth {
                    value.push(open);
                    if open == '{' {
                        value.push_str(r#""x":"#);
                    }
                }
                value.push_str("null");
                for _ in 0..depth {
                    value.push(close);
                }
                value
            };
            assert!(parse_strict_json(nested(64).as_bytes()).is_ok());
            assert!(parse_strict_json(nested(65).as_bytes()).is_err());
        }
    }
}
