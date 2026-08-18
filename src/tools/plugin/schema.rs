use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use thiserror::Error;

use crate::model::JsonValue;

const MAX_SCHEMA_BYTES: usize = 32 * 1024;
const MAX_SCHEMA_NODES: usize = 512;
const MAX_SCHEMA_DEPTH: usize = 16;
const MAX_SCHEMA_PROPERTIES: usize = 256;
const MAX_SCHEMA_REQUIRED: usize = 64;
const MAX_SCHEMA_ENUM_VALUES: usize = 64;
const MAX_SCHEMA_DESCRIPTION_BYTES: usize = 1024;
const MAX_PLUGIN_VALUE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SchemaRoot {
    Parameters,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum PluginSchemaError {
    #[error("plugin schema exceeds its encoded-size limit")]
    TooLarge,
    #[error("plugin schema has too many nodes")]
    TooManyNodes,
    #[error("plugin schema is nested too deeply")]
    TooDeep,
    #[error("plugin schema has too many properties")]
    TooManyProperties,
    #[error("plugin schema has too many required names")]
    TooManyRequired,
    #[error("plugin schema has too many enum values")]
    TooManyEnumValues,
    #[error("plugin schema contains an unsupported or malformed declaration")]
    InvalidSchema,
    #[error("plugin value exceeds its encoded-size limit")]
    ValueTooLarge,
    #[error("plugin value does not match its declared schema")]
    InvalidValue,
}

#[derive(Clone)]
pub(crate) struct CompiledPluginSchema {
    raw: JsonValue,
    node: Arc<SchemaNode>,
}

impl std::fmt::Debug for CompiledPluginSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompiledPluginSchema")
            .field("encoded_bytes", &self.raw.encoded_len())
            .finish_non_exhaustive()
    }
}

impl CompiledPluginSchema {
    pub(crate) fn compile(raw: JsonValue, root: SchemaRoot) -> Result<Self, PluginSchemaError> {
        if raw.encoded_len() > MAX_SCHEMA_BYTES {
            return Err(PluginSchemaError::TooLarge);
        }
        let mut budget = CompileBudget::default();
        let node = compile_node(raw.as_value(), 0, &mut budget)?;
        if root == SchemaRoot::Parameters && !matches!(node, SchemaNode::Object { .. }) {
            return Err(PluginSchemaError::InvalidSchema);
        }
        Ok(Self {
            raw,
            node: Arc::new(node),
        })
    }

    pub(crate) fn raw(&self) -> &JsonValue {
        &self.raw
    }

    pub(crate) fn validate(&self, value: &JsonValue) -> Result<(), PluginSchemaError> {
        if value.encoded_len() > MAX_PLUGIN_VALUE_BYTES {
            return Err(PluginSchemaError::ValueTooLarge);
        }
        validate_node(&self.node, value.as_value())
    }
}

#[derive(Clone)]
enum SchemaNode {
    Scalar {
        kind: ScalarKind,
        allowed: Option<Vec<JsonValue>>,
    },
    Array {
        items: Box<SchemaNode>,
    },
    Object {
        properties: BTreeMap<String, SchemaNode>,
        required: BTreeSet<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScalarKind {
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

#[derive(Default)]
struct CompileBudget {
    nodes: usize,
    properties: usize,
    required: usize,
    enum_values: usize,
}

fn compile_node(
    value: &serde_json::Value,
    container_depth: usize,
    budget: &mut CompileBudget,
) -> Result<SchemaNode, PluginSchemaError> {
    budget.nodes = budget
        .nodes
        .checked_add(1)
        .ok_or(PluginSchemaError::TooManyNodes)?;
    if budget.nodes > MAX_SCHEMA_NODES {
        return Err(PluginSchemaError::TooManyNodes);
    }
    let fields = value.as_object().ok_or(PluginSchemaError::InvalidSchema)?;
    validate_description(fields.get("description"))?;
    let kind = fields
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or(PluginSchemaError::InvalidSchema)?;
    match kind {
        "string" => compile_scalar(fields, ScalarKind::String, budget),
        "number" => compile_scalar(fields, ScalarKind::Number, budget),
        "integer" => compile_scalar(fields, ScalarKind::Integer, budget),
        "boolean" => compile_scalar(fields, ScalarKind::Boolean, budget),
        "null" => compile_scalar(fields, ScalarKind::Null, budget),
        "array" => {
            reject_unknown(fields, &["type", "description", "items"])?;
            let next_depth = enter_container(container_depth)?;
            let items = fields
                .get("items")
                .ok_or(PluginSchemaError::InvalidSchema)?;
            Ok(SchemaNode::Array {
                items: Box::new(compile_node(items, next_depth, budget)?),
            })
        }
        "object" => {
            reject_unknown(
                fields,
                &[
                    "type",
                    "description",
                    "properties",
                    "required",
                    "additionalProperties",
                ],
            )?;
            let next_depth = enter_container(container_depth)?;
            if fields.get("additionalProperties") != Some(&serde_json::Value::Bool(false)) {
                return Err(PluginSchemaError::InvalidSchema);
            }
            let raw_properties = fields
                .get("properties")
                .and_then(serde_json::Value::as_object)
                .ok_or(PluginSchemaError::InvalidSchema)?;
            budget.properties = budget
                .properties
                .checked_add(raw_properties.len())
                .ok_or(PluginSchemaError::TooManyProperties)?;
            if budget.properties > MAX_SCHEMA_PROPERTIES {
                return Err(PluginSchemaError::TooManyProperties);
            }
            let mut properties = BTreeMap::new();
            for (name, schema) in raw_properties {
                if name.is_empty() || name.chars().any(char::is_control) {
                    return Err(PluginSchemaError::InvalidSchema);
                }
                properties.insert(name.clone(), compile_node(schema, next_depth, budget)?);
            }

            let raw_required = fields
                .get("required")
                .and_then(serde_json::Value::as_array)
                .ok_or(PluginSchemaError::InvalidSchema)?;
            budget.required = budget
                .required
                .checked_add(raw_required.len())
                .ok_or(PluginSchemaError::TooManyRequired)?;
            if budget.required > MAX_SCHEMA_REQUIRED {
                return Err(PluginSchemaError::TooManyRequired);
            }
            let mut required = BTreeSet::new();
            for name in raw_required {
                let name = name.as_str().ok_or(PluginSchemaError::InvalidSchema)?;
                if !properties.contains_key(name) {
                    return Err(PluginSchemaError::InvalidSchema);
                }
                if !required.insert(name.to_owned()) {
                    return Err(PluginSchemaError::InvalidSchema);
                }
            }
            Ok(SchemaNode::Object {
                properties,
                required,
            })
        }
        _ => Err(PluginSchemaError::InvalidSchema),
    }
}

fn enter_container(current: usize) -> Result<usize, PluginSchemaError> {
    let next = current.checked_add(1).ok_or(PluginSchemaError::TooDeep)?;
    if next > MAX_SCHEMA_DEPTH {
        return Err(PluginSchemaError::TooDeep);
    }
    Ok(next)
}

fn compile_scalar(
    fields: &serde_json::Map<String, serde_json::Value>,
    kind: ScalarKind,
    budget: &mut CompileBudget,
) -> Result<SchemaNode, PluginSchemaError> {
    reject_unknown(fields, &["type", "description", "enum"])?;
    let allowed = fields
        .get("enum")
        .map(|values| {
            let values = values.as_array().ok_or(PluginSchemaError::InvalidSchema)?;
            if values.is_empty() {
                return Err(PluginSchemaError::InvalidSchema);
            }
            budget.enum_values = budget
                .enum_values
                .checked_add(values.len())
                .ok_or(PluginSchemaError::TooManyEnumValues)?;
            if budget.enum_values > MAX_SCHEMA_ENUM_VALUES {
                return Err(PluginSchemaError::TooManyEnumValues);
            }
            let mut admitted = Vec::new();
            admitted
                .try_reserve_exact(values.len())
                .map_err(|_| PluginSchemaError::InvalidSchema)?;
            for value in values {
                if !matches_scalar(kind, value) {
                    return Err(PluginSchemaError::InvalidSchema);
                }
                let value =
                    JsonValue::new(value.clone()).map_err(|_| PluginSchemaError::InvalidSchema)?;
                if !admitted
                    .iter()
                    .any(|existing: &JsonValue| existing.semantically_equals(&value))
                {
                    admitted.push(value);
                }
            }
            Ok(admitted)
        })
        .transpose()?;
    Ok(SchemaNode::Scalar { kind, allowed })
}

fn reject_unknown(
    fields: &serde_json::Map<String, serde_json::Value>,
    allowed: &[&str],
) -> Result<(), PluginSchemaError> {
    if fields.keys().any(|name| !allowed.contains(&name.as_str())) {
        return Err(PluginSchemaError::InvalidSchema);
    }
    Ok(())
}

fn validate_description(value: Option<&serde_json::Value>) -> Result<(), PluginSchemaError> {
    let Some(value) = value else {
        return Ok(());
    };
    let value = value.as_str().ok_or(PluginSchemaError::InvalidSchema)?;
    if value.len() > MAX_SCHEMA_DESCRIPTION_BYTES || value.chars().any(char::is_control) {
        return Err(PluginSchemaError::InvalidSchema);
    }
    Ok(())
}

fn matches_scalar(kind: ScalarKind, value: &serde_json::Value) -> bool {
    match kind {
        ScalarKind::String => value.is_string(),
        ScalarKind::Number => value.is_number(),
        ScalarKind::Integer => value
            .as_number()
            .is_some_and(|number| number.is_i64() || number.is_u64()),
        ScalarKind::Boolean => value.is_boolean(),
        ScalarKind::Null => value.is_null(),
    }
}

fn validate_node(node: &SchemaNode, value: &serde_json::Value) -> Result<(), PluginSchemaError> {
    match node {
        SchemaNode::Scalar { kind, allowed } => {
            if !matches_scalar(*kind, value) {
                return Err(PluginSchemaError::InvalidValue);
            }
            if let Some(allowed) = allowed {
                let value =
                    JsonValue::new(value.clone()).map_err(|_| PluginSchemaError::InvalidValue)?;
                if !allowed
                    .iter()
                    .any(|candidate| candidate.semantically_equals(&value))
                {
                    return Err(PluginSchemaError::InvalidValue);
                }
            }
            Ok(())
        }
        SchemaNode::Array { items } => {
            let values = value.as_array().ok_or(PluginSchemaError::InvalidValue)?;
            values
                .iter()
                .try_for_each(|value| validate_node(items, value))
        }
        SchemaNode::Object {
            properties,
            required,
        } => {
            let values = value.as_object().ok_or(PluginSchemaError::InvalidValue)?;
            if values.keys().any(|name| !properties.contains_key(name))
                || required.iter().any(|name| !values.contains_key(name))
            {
                return Err(PluginSchemaError::InvalidValue);
            }
            values.iter().try_for_each(|(name, value)| {
                let schema = properties
                    .get(name)
                    .ok_or(PluginSchemaError::InvalidValue)?;
                validate_node(schema, value)
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::model::JsonValue;

    use super::{CompiledPluginSchema, PluginSchemaError, SchemaRoot};

    fn json(value: serde_json::Value) -> JsonValue {
        JsonValue::new(value).unwrap()
    }

    #[test]
    fn closed_parameter_schema_validates_required_nested_and_enum_values() {
        let schema = CompiledPluginSchema::compile(
            json(serde_json::json!({
                "type":"object",
                "properties":{
                    "mode":{"type":"string","enum":["words","lines"]},
                    "values":{"type":"array","items":{"type":"integer"}}
                },
                "required":["mode","values"],
                "additionalProperties":false
            })),
            SchemaRoot::Parameters,
        )
        .unwrap();

        assert!(
            schema
                .validate(&json(serde_json::json!({"mode":"words","values":[1,2]})))
                .is_ok()
        );
        assert!(
            schema
                .validate(&json(serde_json::json!({"mode":"bytes","values":[1]})))
                .is_err()
        );
        assert!(
            schema
                .validate(&json(serde_json::json!({"mode":"words"})))
                .is_err()
        );
        assert!(
            schema
                .validate(&json(
                    serde_json::json!({"mode":"words","values":[],"extra":true})
                ))
                .is_err()
        );
        assert!(
            schema
                .validate(&json(serde_json::json!({"mode":"words","values":[1.5]})))
                .is_err()
        );
    }

    #[test]
    fn output_may_be_scalar_but_parameter_root_must_be_a_closed_object() {
        let scalar = json(serde_json::json!({"type":"string"}));
        let output = CompiledPluginSchema::compile(scalar.clone(), SchemaRoot::Output).unwrap();
        assert!(output.validate(&json(serde_json::json!("ok"))).is_ok());
        assert!(output.validate(&json(serde_json::json!(1))).is_err());
        assert!(CompiledPluginSchema::compile(scalar, SchemaRoot::Parameters).is_err());

        let open = json(serde_json::json!({
            "type":"object",
            "properties":{},
            "required":[]
        }));
        assert!(CompiledPluginSchema::compile(open, SchemaRoot::Parameters).is_err());
    }

    #[test]
    fn schemas_reject_unknown_keywords_missing_object_fields_and_depth_one_over() {
        let unknown = json(serde_json::json!({"type":"string","pattern":"x"}));
        assert!(CompiledPluginSchema::compile(unknown, SchemaRoot::Output).is_err());

        let missing_required = json(serde_json::json!({
            "type":"object",
            "properties":{},
            "additionalProperties":false
        }));
        assert!(CompiledPluginSchema::compile(missing_required, SchemaRoot::Parameters).is_err());

        let mut node = serde_json::json!({"type":"string"});
        for _ in 0..17 {
            node = serde_json::json!({"type":"array","items":node});
        }
        assert_eq!(
            CompiledPluginSchema::compile(json(node), SchemaRoot::Output).unwrap_err(),
            PluginSchemaError::TooDeep
        );
    }

    #[test]
    fn properties_are_bounded_across_the_whole_schema() {
        let nested_properties = (0..129)
            .map(|index| {
                (
                    format!("left_{index}"),
                    serde_json::json!({"type":"string"}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let other_properties = (0..128)
            .map(|index| {
                (
                    format!("right_{index}"),
                    serde_json::json!({"type":"string"}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let schema = json(serde_json::json!({
            "type":"object",
            "properties":{
                "left":{
                    "type":"object",
                    "properties":nested_properties,
                    "required":[],
                    "additionalProperties":false
                },
                "right":{
                    "type":"object",
                    "properties":other_properties,
                    "required":[],
                    "additionalProperties":false
                }
            },
            "required":[],
            "additionalProperties":false
        }));
        assert_eq!(
            CompiledPluginSchema::compile(schema, SchemaRoot::Parameters).unwrap_err(),
            PluginSchemaError::TooManyProperties
        );
    }

    #[test]
    fn required_enum_node_and_runtime_value_limits_have_exact_boundaries() {
        let properties = (0..65)
            .map(|index| (format!("p{index}"), serde_json::json!({"type":"string"})))
            .collect::<serde_json::Map<_, _>>();
        let required = (0..64).map(|index| format!("p{index}")).collect::<Vec<_>>();
        let exact_required = json(serde_json::json!({
            "type":"object",
            "properties":properties,
            "required":required,
            "additionalProperties":false
        }));
        assert!(CompiledPluginSchema::compile(exact_required, SchemaRoot::Parameters).is_ok());
        let properties = (0..65)
            .map(|index| (format!("p{index}"), serde_json::json!({"type":"string"})))
            .collect::<serde_json::Map<_, _>>();
        let required = (0..65).map(|index| format!("p{index}")).collect::<Vec<_>>();
        let one_over_required = json(serde_json::json!({
            "type":"object",
            "properties":properties,
            "required":required,
            "additionalProperties":false
        }));
        assert_eq!(
            CompiledPluginSchema::compile(one_over_required, SchemaRoot::Parameters).unwrap_err(),
            PluginSchemaError::TooManyRequired
        );

        let exact_enum = json(serde_json::json!({
            "type":"integer",
            "enum":(0..64).collect::<Vec<_>>()
        }));
        assert!(CompiledPluginSchema::compile(exact_enum, SchemaRoot::Output).is_ok());
        let one_over_enum = json(serde_json::json!({
            "type":"integer",
            "enum":(0..65).collect::<Vec<_>>()
        }));
        assert_eq!(
            CompiledPluginSchema::compile(one_over_enum, SchemaRoot::Output).unwrap_err(),
            PluginSchemaError::TooManyEnumValues
        );

        let exact_nodes = (0..256)
            .map(|index| {
                let schema = if index == 255 {
                    serde_json::json!({"type":"string"})
                } else {
                    serde_json::json!({"type":"array","items":{"type":"string"}})
                };
                (format!("p{index}"), schema)
            })
            .collect::<serde_json::Map<_, _>>();
        let exact_nodes = json(serde_json::json!({
            "type":"object",
            "properties":exact_nodes,
            "required":[],
            "additionalProperties":false
        }));
        assert!(CompiledPluginSchema::compile(exact_nodes, SchemaRoot::Parameters).is_ok());
        let one_over_nodes = (0..256)
            .map(|index| {
                (
                    format!("p{index}"),
                    serde_json::json!({"type":"array","items":{"type":"string"}}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let one_over_nodes = json(serde_json::json!({
            "type":"object",
            "properties":one_over_nodes,
            "required":[],
            "additionalProperties":false
        }));
        assert_eq!(
            CompiledPluginSchema::compile(one_over_nodes, SchemaRoot::Parameters).unwrap_err(),
            PluginSchemaError::TooManyNodes
        );

        let string_schema = CompiledPluginSchema::compile(
            json(serde_json::json!({"type":"string"})),
            SchemaRoot::Output,
        )
        .unwrap();
        assert!(
            string_schema
                .validate(&json(serde_json::json!("x".repeat(64 * 1024 - 2))))
                .is_ok()
        );
        assert_eq!(
            string_schema
                .validate(&json(serde_json::json!("x".repeat(64 * 1024 - 1))))
                .unwrap_err(),
            PluginSchemaError::ValueTooLarge
        );
    }

    #[test]
    fn duplicate_required_names_are_rejected_instead_of_silently_deduplicated() {
        let schema = json(serde_json::json!({
            "type":"object",
            "properties":{"value":{"type":"string"}},
            "required":["value","value"],
            "additionalProperties":false
        }));
        assert_eq!(
            CompiledPluginSchema::compile(schema, SchemaRoot::Parameters).unwrap_err(),
            PluginSchemaError::InvalidSchema
        );
    }
}
