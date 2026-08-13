//! Bounded, lossless JSON values shared by model and session boundaries.

use std::{fmt, mem::ManuallyDrop};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de, de::Visitor};
use serde_json::{Number, Value};
use thiserror::Error;

/// Largest integer that JavaScript can represent without rounding.
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
/// Maximum nesting accepted from an untrusted JSON value.
pub const MAX_JSON_DEPTH: usize = 128;
/// Maximum number of scalar/container nodes in one opaque JSON value.
pub const MAX_JSON_NODES: usize = 1_000_000;
/// Maximum encoded size of one opaque JSON value or event payload.
pub const MAX_JSON_VALUE_BYTES: usize = 8 * 1024 * 1024;

/// A bounded JSON value that is safe to retain in the session core.
#[derive(Clone, Debug)]
pub struct JsonValue {
    value: Value,
    encoded_len: usize,
}

impl JsonValue {
    /// Validate, normalize, and take ownership of a JSON value.
    pub fn new(value: Value) -> Result<Self, JsonValueError> {
        // A caller can build a `serde_json::Value` far deeper than serde's parser accepts.
        // Keep it out of automatic recursive Drop until the depth check has succeeded.
        let guarded = ManuallyDrop::new(value);
        if let Err(error) = inspect_json(&guarded) {
            drop_json_iteratively(ManuallyDrop::into_inner(guarded));
            return Err(error);
        }

        let mut value = ManuallyDrop::into_inner(guarded);
        normalize_numbers(&mut value);
        let encoded = match serde_json::to_vec(&value) {
            Ok(encoded) => encoded,
            Err(error) => {
                drop_json_iteratively(value);
                return Err(JsonValueError::Encode(error.to_string()));
            }
        };
        if encoded.len() > MAX_JSON_VALUE_BYTES {
            let error = JsonValueError::TooLarge {
                maximum: MAX_JSON_VALUE_BYTES,
                actual: encoded.len(),
            };
            drop_json_iteratively(value);
            return Err(error);
        }
        Ok(Self {
            value,
            encoded_len: encoded.len(),
        })
    }

    /// Construct JSON null while retaining the distinction from an absent field.
    #[must_use]
    pub fn null() -> Self {
        Self {
            value: Value::Null,
            encoded_len: 4,
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

    /// Consume the wrapper and return the owned JSON tree.
    #[must_use]
    pub fn into_value(self) -> Value {
        self.value
    }

    /// Compare using JavaScript JSON-number semantics (`1` equals `1.0`).
    #[must_use]
    pub(crate) fn semantically_equals(&self, other: &Self) -> bool {
        json_values_equal(&self.value, &other.value)
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
    let mut pending = vec![(first, second)];
    while let Some((left, right)) = pending.pop() {
        match (left, right) {
            (Value::Null, Value::Null) => {}
            (Value::Bool(left), Value::Bool(right)) if left == right => {}
            (Value::String(left), Value::String(right)) if left == right => {}
            (Value::Number(left), Value::Number(right)) => {
                if left.as_f64() != right.as_f64() {
                    return false;
                }
            }
            (Value::Array(left), Value::Array(right)) => {
                if left.len() != right.len() {
                    return false;
                }
                pending.extend(left.iter().zip(right));
            }
            (Value::Object(left), Value::Object(right)) => {
                if left.len() != right.len() {
                    return false;
                }
                for (key, left_value) in left {
                    let Some(right_value) = right.get(key) else {
                        return false;
                    };
                    pending.push((left_value, right_value));
                }
            }
            _ => return false,
        }
    }
    true
}

fn inspect_json(value: &Value) -> Result<(), JsonValueError> {
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
                let child_depth = depth.saturating_add(1);
                let mut children = Children::Array(items.iter());
                current = children.next().map(|child| (child, child_depth));
                parents.push((children, child_depth));
            }
            Value::Object(fields) => {
                let child_depth = depth.saturating_add(1);
                let mut children = Children::Object(fields.values());
                current = children.next().map(|child| (child, child_depth));
                parents.push((children, child_depth));
            }
            Value::Null | Value::Bool(_) | Value::String(_) => {}
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
    Ok(())
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
