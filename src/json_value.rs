//! Bounded, lossless JSON values shared by model and session boundaries.

use std::{
    fmt, io,
    mem::{ManuallyDrop, size_of},
    sync::Arc,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de, de::Visitor};
use serde_json::{Number, Value};
use thiserror::Error;

use crate::resident_credit::{
    arc_inner_charge, heap_allocation_charge, string_backing_charge, vec_backing_charge,
};

/// Largest integer that JavaScript can represent without rounding.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// Maximum nesting accepted from an untrusted JSON value.
pub const MAX_JSON_DEPTH: usize = 128;
/// Maximum number of scalar/container nodes in one opaque JSON value.
pub const MAX_JSON_NODES: usize = 1_000_000;
/// Maximum encoded size of one opaque JSON value or event payload.
pub const MAX_JSON_VALUE_BYTES: usize = 8 * 1024 * 1024;

/// A bounded JSON value that is safe to retain in the session core.
#[derive(Clone)]
pub struct JsonValue {
    value: Arc<Value>,
    encoded_len: usize,
    resident_bytes: usize,
}

impl JsonValue {
    /// Validate, normalize, and take ownership of a JSON value.
    pub fn new(value: Value) -> Result<Self, JsonValueError> {
        // A caller can build a `serde_json::Value` far deeper than serde's parser accepts.
        // Keep it out of automatic recursive Drop until the depth check has succeeded.
        let guarded = ManuallyDrop::new(value);
        let resident_bytes = match inspect_json(&guarded) {
            Ok(dynamic_bytes) => arc_inner_charge::<Value>()
                .and_then(|root| root.checked_add(dynamic_bytes))
                .unwrap_or(usize::MAX),
            Err(error) => {
                drop_json_iteratively(ManuallyDrop::into_inner(guarded));
                return Err(error);
            }
        };

        let mut value = ManuallyDrop::into_inner(guarded);
        normalize_numbers(&mut value);
        let encoded_len = match encoded_len(&value) {
            Ok(encoded_len) => encoded_len,
            Err(error) => {
                drop_json_iteratively(value);
                return Err(error);
            }
        };
        if encoded_len > MAX_JSON_VALUE_BYTES {
            let error = JsonValueError::TooLarge {
                maximum: MAX_JSON_VALUE_BYTES,
                actual: encoded_len,
            };
            drop_json_iteratively(value);
            return Err(error);
        }
        Ok(Self {
            value: Arc::new(value),
            encoded_len,
            resident_bytes,
        })
    }

    /// Construct JSON null while retaining the distinction from an absent field.
    #[must_use]
    pub fn null() -> Self {
        Self {
            value: Arc::new(Value::Null),
            encoded_len: 4,
            resident_bytes: arc_inner_charge::<Value>().unwrap_or(usize::MAX),
        }
    }

    /// Borrow the validated JSON tree.
    #[must_use]
    pub fn as_value(&self) -> &Value {
        &self.value
    }

    /// Number of bytes in the normalized compact JSON representation.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.encoded_len
    }

    /// Deterministic charge for the retained JSON allocation graph.
    ///
    /// This is a product accounting value, not an allocator-specific RSS
    /// measurement. Clones share the same graph and therefore the same value.
    #[must_use]
    pub(crate) fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Process-local identity of the shared JSON allocation.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn resident_allocation_id(&self) -> usize {
        Arc::as_ptr(&self.value) as usize
    }

    /// Consume the wrapper and return the owned JSON tree.
    #[must_use]
    pub fn into_value(self) -> Value {
        Arc::try_unwrap(self.value).unwrap_or_else(|shared| (*shared).clone())
    }

    /// Compare using JavaScript JSON-number semantics (`1` equals `1.0`).
    #[must_use]
    pub(crate) fn semantically_equals(&self, other: &Self) -> bool {
        json_values_equal(&self.value, &other.value)
    }
}

impl fmt::Debug for JsonValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JsonValue")
            .field("value", &self.value)
            .field("encoded_len", &self.encoded_len)
            .finish()
    }
}

impl PartialEq for JsonValue {
    fn eq(&self, other: &Self) -> bool {
        self.semantically_equals(other)
    }
}

impl Eq for JsonValue {}

impl Serialize for JsonValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for JsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

impl TryFrom<Value> for JsonValue {
    type Error = JsonValueError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<JsonValue> for Value {
    fn from(value: JsonValue) -> Self {
        value.into_value()
    }
}

/// A non-negative integer inside JavaScript's exact integer range.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NonNegativeSafeInteger(u64);

impl NonNegativeSafeInteger {
    /// Validate one non-negative exact integer.
    pub fn new(value: u64) -> Result<Self, JsonValueError> {
        if value > MAX_SAFE_INTEGER {
            return Err(JsonValueError::NonNegativeSafeInteger);
        }
        Ok(Self(value))
    }

    /// Return the validated integer.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for NonNegativeSafeInteger {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u64(self.0)
    }
}

impl<'de> Deserialize<'de> for NonNegativeSafeInteger {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_safe_u64(deserializer, false).map(Self)
    }
}

/// A positive finite number, used for provider-requested retry delays.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct PositiveFiniteNumber(f64);

impl PositiveFiniteNumber {
    /// Validate a positive finite number.
    pub fn new(value: f64) -> Result<Self, JsonValueError> {
        if !value.is_finite() || value <= 0.0 {
            return Err(JsonValueError::PositiveFiniteNumber);
        }
        Ok(Self(value))
    }

    /// Return the validated number.
    #[must_use]
    pub fn get(self) -> f64 {
        self.0
    }
}

// NaN and infinities cannot enter this type, so equality is reflexive.
impl Eq for PositiveFiniteNumber {}

impl Serialize for PositiveFiniteNumber {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_js_number(self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for PositiveFiniteNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct PositiveVisitor;

        impl Visitor<'_> for PositiveVisitor {
            type Value = PositiveFiniteNumber;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a positive finite number")
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                PositiveFiniteNumber::new(value as f64).map_err(E::custom)
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                PositiveFiniteNumber::new(value as f64).map_err(E::custom)
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                PositiveFiniteNumber::new(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(PositiveVisitor)
    }
}

/// Errors raised while admitting untrusted JSON into the core.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum JsonValueError {
    /// The number cannot cross the JavaScript JSON boundary without ambiguity.
    #[error(
        "JSON numbers must be finite, must not be negative zero, and integral values must be JavaScript-safe"
    )]
    InvalidNumber,
    /// A retained tree is too deeply nested for bounded Rust traversal/drop.
    #[error("JSON nesting exceeds the maximum depth of {maximum}")]
    TooDeep { maximum: usize },
    /// A retained tree contains too many nodes.
    #[error("JSON value exceeds the maximum of {maximum} nodes")]
    TooManyNodes { maximum: usize },
    /// A retained value is too large once encoded.
    #[error("JSON value is {actual} bytes; maximum is {maximum} bytes")]
    TooLarge { maximum: usize, actual: usize },
    /// Serialization failed after validation.
    #[error("JSON value cannot be encoded: {0}")]
    Encode(String),
    /// An integer would not survive JavaScript number conversion exactly.
    #[error("value must be a non-negative JavaScript-safe integer")]
    NonNegativeSafeInteger,
    /// A positive finite floating-point value was required.
    #[error("value must be a positive finite number")]
    PositiveFiniteNumber,
}

pub(crate) fn deserialize_safe_u64<'de, D>(deserializer: D, positive: bool) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct SafeIntegerVisitor {
        positive: bool,
    }

    impl Visitor<'_> for SafeIntegerVisitor {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            if self.positive {
                formatter.write_str("a positive JavaScript-safe integer")
            } else {
                formatter.write_str("a non-negative JavaScript-safe integer")
            }
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let value =
                u64::try_from(value).map_err(|_| E::custom("integer must not be negative"))?;
            self.visit_u64(value)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value > MAX_SAFE_INTEGER || (self.positive && value == 0) {
                return Err(E::custom(if self.positive {
                    "integer must be positive and JavaScript-safe"
                } else {
                    "integer must be non-negative and JavaScript-safe"
                }));
            }
            Ok(value)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !value.is_finite()
                || value.fract() != 0.0
                || value < 0.0
                || (value == 0.0 && value.is_sign_negative())
                || value > MAX_SAFE_INTEGER as f64
                || (self.positive && value == 0.0)
            {
                return Err(E::custom(if self.positive {
                    "number must be a positive JavaScript-safe integer"
                } else {
                    "number must be a non-negative JavaScript-safe integer"
                }));
            }
            Ok(value as u64)
        }
    }

    deserializer.deserialize_any(SafeIntegerVisitor { positive })
}

pub(crate) fn deserialize_safe_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    struct SignedSafeIntegerVisitor;

    impl Visitor<'_> for SignedSafeIntegerVisitor {
        type Value = i64;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a signed JavaScript-safe integer")
        }

        fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let maximum = MAX_SAFE_INTEGER as i64;
            if !(-maximum..=maximum).contains(&value) {
                return Err(E::custom("integer is outside the JavaScript-safe range"));
            }
            Ok(value)
        }

        fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if value > MAX_SAFE_INTEGER {
                return Err(E::custom("integer is outside the JavaScript-safe range"));
            }
            Ok(value as i64)
        }

        fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let maximum = MAX_SAFE_INTEGER as f64;
            if !value.is_finite()
                || value.fract() != 0.0
                || (value == 0.0 && value.is_sign_negative())
                || !(-maximum..=maximum).contains(&value)
            {
                return Err(E::custom("number must be a signed JavaScript-safe integer"));
            }
            Ok(value as i64)
        }
    }

    deserializer.deserialize_any(SignedSafeIntegerVisitor)
}

/// Deserialize a field that is optional only by absence. If the key is present,
/// JSON `null` is passed to `T` and normally rejected instead of becoming `None`.
pub(crate) fn deserialize_present_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub(crate) fn serialize_js_number<S>(value: f64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER as f64 {
        if value >= 0.0 {
            return serializer.serialize_u64(value as u64);
        }
        return serializer.serialize_i64(value as i64);
    }
    serializer.serialize_f64(value)
}

pub(crate) fn json_values_equal(first: &Value, second: &Value) -> bool {
    json_values_equal_at(first, second, 0)
}

fn json_values_equal_at(first: &Value, second: &Value, depth: usize) -> bool {
    if depth > MAX_JSON_DEPTH {
        return false;
    }
    match (first, second) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_values_equal_at(left, right, depth.saturating_add(1)))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left_value)| {
                    right.get(key).is_some_and(|right_value| {
                        json_values_equal_at(left_value, right_value, depth.saturating_add(1))
                    })
                })
        }
        _ => false,
    }
}

fn inspect_json(value: &Value) -> Result<usize, JsonValueError> {
    enum Children<'a> {
        Array(std::slice::Iter<'a, Value>),
        Object(serde_json::map::Values<'a>),
    }

    impl<'a> Children<'a> {
        fn next(&mut self) -> Option<&'a Value> {
            match self {
                Self::Array(items) => items.next(),
                Self::Object(fields) => fields.next(),
            }
        }
    }

    let mut current = Some((value, 0_usize));
    let mut parents: Vec<(Children<'_>, usize)> = Vec::new();
    let mut nodes = 0_usize;
    let mut resident_bytes = 0_usize;
    while let Some((node, depth)) = current.take() {
        nodes = nodes.saturating_add(1);
        if nodes > MAX_JSON_NODES {
            return Err(JsonValueError::TooManyNodes {
                maximum: MAX_JSON_NODES,
            });
        }
        if depth > MAX_JSON_DEPTH {
            return Err(JsonValueError::TooDeep {
                maximum: MAX_JSON_DEPTH,
            });
        }
        match node {
            Value::String(value) => {
                add_resident_charge(&mut resident_bytes, string_backing_charge(value.capacity()));
            }
            Value::Number(number) => {
                let Some(number) = number.as_f64() else {
                    return Err(JsonValueError::InvalidNumber);
                };
                if !number.is_finite()
                    || (number == 0.0 && number.is_sign_negative())
                    || (number.fract() == 0.0 && number.abs() > MAX_SAFE_INTEGER as f64)
                {
                    return Err(JsonValueError::InvalidNumber);
                }
            }
            Value::Array(items) => {
                add_resident_charge(
                    &mut resident_bytes,
                    vec_backing_charge::<Value>(items.capacity()),
                );
                let child_depth = depth.saturating_add(1);
                let mut children = Children::Array(items.iter());
                current = children.next().map(|child| (child, child_depth));
                parents.push((children, child_depth));
            }
            Value::Object(fields) => {
                add_resident_charge(
                    &mut resident_bytes,
                    json_object_backing_charge(fields.len()),
                );
                for key in fields.keys() {
                    add_resident_charge(&mut resident_bytes, string_backing_charge(key.capacity()));
                }
                let child_depth = depth.saturating_add(1);
                let mut children = Children::Object(fields.values());
                current = children.next().map(|child| (child, child_depth));
                parents.push((children, child_depth));
            }
            Value::Null | Value::Bool(_) => {}
        }

        while current.is_none() {
            let Some((children, child_depth)) = parents.last_mut() else {
                break;
            };
            if let Some(child) = children.next() {
                current = Some((child, *child_depth));
            } else {
                parents.pop();
            }
        }
    }
    Ok(resident_bytes)
}

fn add_resident_charge(total: &mut usize, charge: Option<usize>) {
    *total = total
        .checked_add(charge.unwrap_or(usize::MAX))
        .unwrap_or(usize::MAX);
}

fn json_object_backing_charge(entries: usize) -> Option<usize> {
    // `serde_json::Map` uses `BTreeMap` without the optional
    // `preserve_order` feature. Removing its final entry may retain one empty
    // root leaf, and a non-root node may be only half full, so charge at least
    // one conservative maximum-sized node plus one per five later entries.
    const NODE_CAPACITY: usize = 11;
    const MIN_ENTRIES_PER_NODE: usize = 5;
    let nodes = if entries == 0 {
        1
    } else {
        entries
            .checked_sub(1)?
            .checked_add(MIN_ENTRIES_PER_NODE - 1)?
            .checked_div(MIN_ENTRIES_PER_NODE)?
            .checked_add(1)?
    };
    let slots = NODE_CAPACITY.checked_mul(size_of::<String>().checked_add(size_of::<Value>())?)?;
    let edges = (NODE_CAPACITY + 1).checked_mul(size_of::<usize>())?;
    let node_bytes = 16_usize.checked_add(slots)?.checked_add(edges)?;
    let one_node = heap_allocation_charge(node_bytes, 16)?;
    nodes.checked_mul(one_node)
}

fn encoded_len(value: &Value) -> Result<usize, JsonValueError> {
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| JsonValueError::Encode(error.to_string()))?;
    Ok(writer.len)
}

#[derive(Default)]
struct CountingWriter {
    len: usize,
}

impl io::Write for CountingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.len = self
            .len
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("encoded JSON length overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn normalize_numbers(value: &mut Value) {
    match value {
        Value::Number(number) => {
            let Some(value) = number.as_f64() else {
                return;
            };
            if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER as f64 {
                *number = if value >= 0.0 {
                    Number::from(value as u64)
                } else {
                    Number::from(value as i64)
                };
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_numbers(item);
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                normalize_numbers(item);
            }
        }
        Value::Null | Value::Bool(_) | Value::String(_) => {}
    }
}

fn drop_json_iteratively(value: Value) {
    enum Children {
        Array(std::vec::IntoIter<Value>),
        Object(serde_json::map::IntoValues),
    }

    impl Children {
        fn next(&mut self) -> Option<Value> {
            match self {
                Self::Array(items) => items.next(),
                Self::Object(fields) => fields.next(),
            }
        }
    }

    let mut current = Some(value);
    let mut parents = Vec::new();
    while let Some(node) = current.take() {
        match node {
            Value::Array(items) => {
                let mut children = Children::Array(items.into_iter());
                current = children.next();
                parents.push(children);
            }
            Value::Object(fields) => {
                let mut children = Children::Object(fields.into_values());
                current = children.next();
                parents.push(children);
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }

        while current.is_none() {
            let Some(children) = parents.last_mut() else {
                break;
            };
            if let Some(child) = children.next() {
                current = Some(child);
            } else {
                parents.pop();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::JsonValue;
    use crate::resident_credit::{arc_inner_charge, string_backing_charge, vec_backing_charge};

    #[test]
    fn encoded_length_matches_compact_json_for_escape_and_unicode_shapes() {
        for value in [
            json!(null),
            json!({"text": "\0\n\"\\é😀\u{2028}"}),
            json!([0, 1.0, -2, true, false]),
            json!({"nested": [{"x": "y"}]}),
        ] {
            let actual = JsonValue::new(value).unwrap();
            let expected = serde_json::to_vec(actual.as_value()).unwrap().len();
            assert_eq!(actual.encoded_len(), expected);
        }
    }

    #[test]
    fn semantic_equality_does_not_need_a_width_sized_worklist() {
        let left = JsonValue::new(Value::Array(vec![Value::Bool(true); 100_000])).unwrap();
        let right = left.clone();
        assert_eq!(left, right);

        let mut different = vec![Value::Bool(true); 100_000];
        different[99_999] = Value::Bool(false);
        let different = JsonValue::new(Value::Array(different)).unwrap();
        assert_ne!(left, different);
    }

    #[test]
    fn resident_charge_uses_capacity_and_clones_share_one_graph() {
        let mut text = String::with_capacity(4_096);
        text.push('x');
        let text_capacity = text.capacity();
        let value = JsonValue::new(Value::String(text)).unwrap();
        assert_eq!(
            value.resident_bytes(),
            arc_inner_charge::<Value>().unwrap() + string_backing_charge(text_capacity).unwrap()
        );
        assert!(value.resident_bytes() > value.encoded_len());

        let clone = value.clone();
        assert_eq!(
            clone.resident_allocation_id(),
            value.resident_allocation_id()
        );
        assert_eq!(clone.resident_bytes(), value.resident_bytes());

        let independent = JsonValue::new(Value::String("x".to_owned())).unwrap();
        assert_ne!(
            independent.resident_allocation_id(),
            value.resident_allocation_id()
        );
    }

    #[test]
    fn resident_charge_includes_wide_container_backing_and_small_allocations() {
        let mut items = Vec::with_capacity(64);
        for _ in 0..32 {
            items.push(Value::String(String::from("x")));
        }
        let array_capacity = items.capacity();
        let value = JsonValue::new(Value::Array(items)).unwrap();
        let structural_floor = arc_inner_charge::<Value>().unwrap()
            + vec_backing_charge::<Value>(array_capacity).unwrap();
        assert!(value.resident_bytes() > structural_floor);
        assert!(value.resident_bytes() > value.encoded_len());

        let object = JsonValue::new(json!({"key": true})).unwrap();
        assert!(
            object.resident_bytes()
                > arc_inner_charge::<Value>().unwrap()
                    + string_backing_charge("key".len()).unwrap()
        );

        let empty = JsonValue::new(Value::Object(serde_json::Map::new())).unwrap();
        let mut retained_empty = serde_json::Map::new();
        retained_empty.insert("removed".to_owned(), Value::Null);
        retained_empty.remove("removed");
        let retained_empty = JsonValue::new(Value::Object(retained_empty)).unwrap();
        let root_only = arc_inner_charge::<Value>().unwrap();
        assert!(empty.resident_bytes() > root_only);
        assert!(retained_empty.resident_bytes() >= empty.resident_bytes());
    }
}
