use serde::Deserialize;
use serde_json::Value;

use super::error::{ToolCallError, ToolCallResult};

pub(crate) const MAX_TOOL_ARGUMENT_STRING_BYTES: usize = 4 * 1024;
pub(crate) const READ_MAX_LINES: u64 = 2_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListWire {
    #[serde(default, deserialize_with = "deserialize_present")]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobWire {
    pattern: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepWire {
    pattern: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    path: Option<String>,
    #[serde(default, deserialize_with = "deserialize_present")]
    include: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct ReadWire {
    file_path: String,
    #[serde(default, deserialize_with = "deserialize_present")]
    offset: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_present")]
    limit: Option<u64>,
}

#[derive(Clone)]
pub(crate) struct ListArgs {
    pub(crate) path: String,
}

#[derive(Clone)]
pub(crate) struct GlobArgs {
    pub(crate) pattern: String,
    pub(crate) path: String,
}

#[derive(Clone)]
pub(crate) struct GrepArgs {
    pub(crate) pattern: String,
    pub(crate) path: String,
    pub(crate) include: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ReadArgs {
    pub(crate) file_path: String,
    pub(crate) offset: usize,
    pub(crate) limit: usize,
}

pub(crate) fn parse_list(value: &Value) -> ToolCallResult<ListArgs> {
    let wire: ListWire = parse(value, "list")?;
    let path = wire.path.unwrap_or_else(|| ".".to_owned());
    validate_path(&path, "list.path")?;
    Ok(ListArgs { path })
}

pub(crate) fn parse_glob(value: &Value) -> ToolCallResult<GlobArgs> {
    let wire: GlobWire = parse(value, "glob")?;
    validate_nonblank(&wire.pattern, "glob.pattern")?;
    validate_string(&wire.pattern, "glob.pattern")?;
    let path = wire.path.unwrap_or_else(|| ".".to_owned());
    validate_path(&path, "glob.path")?;
    Ok(GlobArgs {
        pattern: wire.pattern,
        path,
    })
}

pub(crate) fn parse_grep(value: &Value) -> ToolCallResult<GrepArgs> {
    let wire: GrepWire = parse(value, "grep")?;
    if wire.pattern.is_empty() {
        return Err(ToolCallError::invalid_args(
            "grep.pattern must not be empty",
        ));
    }
    validate_string(&wire.pattern, "grep.pattern")?;
    let path = wire.path.unwrap_or_else(|| ".".to_owned());
    validate_path(&path, "grep.path")?;
    if let Some(include) = wire.include.as_deref() {
        validate_nonblank(include, "grep.include")?;
        validate_string(include, "grep.include")?;
        if include.starts_with('!') || has_top_level_comma(include) {
            return Err(ToolCallError::invalid_args(
                "grep.include must be one positive glob pattern",
            ));
        }
    }
    Ok(GrepArgs {
        pattern: wire.pattern,
        path,
        include: wire.include,
    })
}

pub(crate) fn parse_read(value: &Value) -> ToolCallResult<ReadArgs> {
    let wire: ReadWire = parse(value, "read")?;
    validate_path(&wire.file_path, "read.file_path")?;
    let offset = wire.offset.unwrap_or(1);
    let limit = wire.limit.unwrap_or(READ_MAX_LINES);
    if offset == 0 {
        return Err(ToolCallError::invalid_args(
            "read.offset must be a positive integer",
        ));
    }
    if limit == 0 || limit > READ_MAX_LINES {
        return Err(ToolCallError::invalid_args(format!(
            "read.limit must be between 1 and {READ_MAX_LINES}"
        )));
    }
    let offset = usize::try_from(offset)
        .map_err(|_| ToolCallError::invalid_args("read.offset is too large"))?;
    let limit = usize::try_from(limit)
        .map_err(|_| ToolCallError::invalid_args("read.limit is too large"))?;
    Ok(ReadArgs {
        file_path: wire.file_path,
        offset,
        limit,
    })
}

fn parse<T: for<'de> Deserialize<'de>>(value: &Value, tool: &str) -> ToolCallResult<T> {
    serde_json::from_value(value.clone()).map_err(|_| {
        ToolCallError::invalid_args(format!(
            "{tool} arguments must match the advertised object schema"
        ))
    })
}

fn deserialize_present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

fn validate_path(value: &str, field: &str) -> ToolCallResult<()> {
    validate_nonblank(value, field)?;
    validate_string(value, field)
}

fn validate_nonblank(value: &str, field: &str) -> ToolCallResult<()> {
    if value.trim().is_empty() {
        return Err(ToolCallError::invalid_args(format!(
            "{field} must not be blank"
        )));
    }
    Ok(())
}

fn validate_string(value: &str, field: &str) -> ToolCallResult<()> {
    if value.len() > MAX_TOOL_ARGUMENT_STRING_BYTES || value.chars().any(char::is_control) {
        return Err(ToolCallError::invalid_args(format!(
            "{field} must be at most {MAX_TOOL_ARGUMENT_STRING_BYTES} bytes and contain no control characters"
        )));
    }
    Ok(())
}

fn has_top_level_comma(pattern: &str) -> bool {
    let mut brace_depth = 0_usize;
    let mut escaped = false;
    for character in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' => escaped = true,
            '{' => brace_depth = brace_depth.saturating_add(1),
            '}' => brace_depth = brace_depth.saturating_sub(1),
            ',' if brace_depth == 0 => return true,
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{MAX_TOOL_ARGUMENT_STRING_BYTES, parse_glob, parse_grep, parse_list, parse_read};

    #[test]
    fn fixed_argument_boundaries_accept_the_limit_and_reject_one_more() {
        let at_limit = "x".repeat(MAX_TOOL_ARGUMENT_STRING_BYTES);
        assert!(parse_grep(&json!({"pattern": at_limit})).is_ok());
        let over_limit = "x".repeat(MAX_TOOL_ARGUMENT_STRING_BYTES + 1);
        assert!(parse_grep(&json!({"pattern": over_limit})).is_err());
        let too_many_utf8_bytes = "界".repeat(1_366);
        assert!(too_many_utf8_bytes.chars().count() < MAX_TOOL_ARGUMENT_STRING_BYTES);
        assert!(parse_grep(&json!({"pattern": too_many_utf8_bytes})).is_err());

        assert!(parse_read(&json!({"file_path": "a", "limit": 2_000})).is_ok());
        assert!(parse_read(&json!({"file_path": "a", "limit": 2_001})).is_err());
        assert!(parse_read(&json!({"file_path": "a", "offset": 0})).is_err());
    }

    #[test]
    fn include_is_one_positive_glob_but_brace_alternation_is_valid() {
        assert!(parse_grep(&json!({"pattern": "x", "include": "*.{rs,toml}"})).is_ok());
        assert!(parse_grep(&json!({"pattern": "x", "include": "!*.rs"})).is_err());
        assert!(parse_grep(&json!({"pattern": "x", "include": "*.rs,*.toml"})).is_err());
    }

    #[test]
    fn explicit_null_never_masquerades_as_an_omitted_optional_field() {
        assert!(parse_list(&json!({"path": null})).is_err());
        assert!(parse_glob(&json!({"pattern": "*", "path": null})).is_err());
        assert!(parse_grep(&json!({"pattern": "x", "path": null})).is_err());
        assert!(parse_grep(&json!({"pattern": "x", "include": null})).is_err());
        assert!(parse_read(&json!({"file_path": "a", "offset": null})).is_err());
        assert!(parse_read(&json!({"file_path": "a", "limit": null})).is_err());
    }

    #[test]
    fn second_stage_string_rules_match_the_documented_runtime_contract() {
        assert!(parse_list(&json!({"path": ""})).is_err());
        assert!(parse_list(&json!({"path": "   "})).is_err());
        assert!(parse_list(&json!({"path": "bad\nname"})).is_err());

        assert!(parse_glob(&json!({"pattern": "   "})).is_err());
        assert!(parse_glob(&json!({"pattern": "bad\tpattern"})).is_err());
        assert!(parse_glob(&json!({"pattern": "*", "path": "   "})).is_err());

        assert!(parse_grep(&json!({"pattern": "   "})).is_ok());
        assert!(parse_grep(&json!({"pattern": "bad\npattern"})).is_err());
        assert!(parse_grep(&json!({"pattern": "x", "include": "   "})).is_err());

        assert!(parse_read(&json!({"file_path": "   "})).is_err());
        assert!(parse_read(&json!({"file_path": "bad\rname"})).is_err());
    }
}
