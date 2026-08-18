mod plugin_support;

use plugin_support::{PluginError, ToolDeclaration};

fn main() -> std::io::Result<()> {
    plugin_support::run(
        "json-tools",
        vec![ToolDeclaration {
            name: "json_format",
            description: "Validate strict JSON and return a deterministic pretty-printed string",
            parameters: serde_json::json!({
                "type":"object",
                "properties":{"json":{"type":"string"}},
                "required":["json"],
                "additionalProperties":false
            }),
            output: serde_json::json!({"type":"string"}),
        }],
        |tool, arguments| {
            if tool != "json_format" {
                return Err(PluginError {
                    code: "UNKNOWN_TOOL",
                    message: "the requested tool is not declared by this plugin".to_owned(),
                });
            }
            let text = arguments
                .as_object()
                .and_then(|fields| fields.get("json"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| PluginError {
                    code: "INVALID_JSON",
                    message: "json must be a string".to_owned(),
                })?;
            let value = plugin_support::parse_strict_value(text).map_err(|()| PluginError {
                code: "INVALID_JSON",
                message: "input is not strict JSON or contains duplicate keys".to_owned(),
            })?;
            serde_json::to_string_pretty(&value)
                .map(serde_json::Value::String)
                .map_err(|_| PluginError {
                    code: "INVALID_JSON",
                    message: "input could not be formatted".to_owned(),
                })
        },
    )
}
