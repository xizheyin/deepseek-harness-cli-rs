mod plugin_support;

use plugin_support::{PluginError, ToolDeclaration};

fn main() -> std::io::Result<()> {
    plugin_support::run(
        "text-tools",
        vec![ToolDeclaration {
            name: "text_stats",
            description: "Count Unicode characters, words, and lines in supplied text",
            parameters: serde_json::json!({
                "type":"object",
                "properties":{"text":{"type":"string"}},
                "required":["text"],
                "additionalProperties":false
            }),
            output: serde_json::json!({
                "type":"object",
                "properties":{
                    "characters":{"type":"integer"},
                    "words":{"type":"integer"},
                    "lines":{"type":"integer"}
                },
                "required":["characters","words","lines"],
                "additionalProperties":false
            }),
        }],
        |tool, arguments| {
            if tool != "text_stats" {
                return Err(PluginError {
                    code: "UNKNOWN_TOOL",
                    message: "the requested tool is not declared by this plugin".to_owned(),
                });
            }
            let fields = arguments.as_object().ok_or_else(|| PluginError {
                code: "INVALID_TEXT",
                message: "arguments must be an object".to_owned(),
            })?;
            let text = fields
                .get("text")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| PluginError {
                    code: "INVALID_TEXT",
                    message: "text must be a string".to_owned(),
                })?;
            Ok(serde_json::json!({
                "characters": text.chars().count(),
                "words": text.split_whitespace().count(),
                "lines": if text.is_empty() { 0 } else { text.lines().count() },
            }))
        },
    )
}
