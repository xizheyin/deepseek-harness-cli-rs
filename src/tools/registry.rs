use std::{path::Path, sync::Arc};

use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{
        ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
        ToolExecutorError,
    },
    model::{ContentBlock, JsonValue, ToolSchema},
};

use super::{
    MAX_TOOL_CONTENT_BYTES,
    arguments::{parse_glob, parse_grep, parse_list, parse_read},
    error::{ToolCallError, ToolCallResult, ToolRegistryBuildError},
    workspace::Workspace,
    {glob, grep, list, read},
};

#[cfg(unix)]
use super::patch;
#[cfg(unix)]
use crate::agent::{ToolPreparation, ToolPreparationFuture};

/// Immutable catalogue and capability root for the four Phase 4 read-only tools.
pub struct ReadOnlyToolRegistry {
    workspace: Workspace,
    schemas: Arc<[ToolSchema]>,
}

impl std::fmt::Debug for ReadOnlyToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ReadOnlyToolRegistry")
            .field("workspace_configured", &true)
            .field("schema_count", &self.schemas.len())
            .finish()
    }
}

impl ReadOnlyToolRegistry {
    /// Open and permanently bind the registry to one existing workspace directory.
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, ToolRegistryBuildError> {
        let workspace = Workspace::open(workspace.as_ref())?;
        let schemas = build_schemas()?.into();
        Ok(Self { workspace, schemas })
    }

    /// Ordered tool declarations sent to the model by `AgentLoopConfig`.
    #[must_use]
    pub fn schemas(&self) -> &[ToolSchema] {
        &self.schemas
    }

    /// Normalized startup workspace used only for application assembly/display.
    #[must_use]
    pub fn workspace(&self) -> &Path {
        self.workspace.display_root()
    }
}

impl ToolExecutor for ReadOnlyToolRegistry {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let workspace = self.workspace.clone();
        Box::pin(async move {
            let outcome = dispatch(
                &workspace,
                request.name(),
                request.arguments().as_value(),
                &cancellation,
            )
            .await;
            match outcome {
                Ok(text) => normalize_success(text),
                Err(error) => error.into_execution_result(),
            }
        })
    }
}

/// Capability-bound catalogue containing the four read tools plus one
/// approval-gated, two-stage `apply_patch` mutation tool.
#[cfg(unix)]
pub struct WorkspaceToolRegistry {
    workspace: Workspace,
    schemas: Arc<[ToolSchema]>,
}

#[cfg(unix)]
impl std::fmt::Debug for WorkspaceToolRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceToolRegistry")
            .field("workspace_configured", &true)
            .field("schema_count", &self.schemas.len())
            .finish()
    }
}

#[cfg(unix)]
impl WorkspaceToolRegistry {
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, ToolRegistryBuildError> {
        let workspace = Workspace::open(workspace.as_ref())?;
        let mut schemas = build_schemas()?;
        schemas.push(schema(
            "apply_patch",
            "Prepare one bounded single-file unified diff for approval and atomic publication.",
            json!({
                "type": "object",
                "properties": {
                    "patch": {
                        "type": "string",
                        "description": "One strict create/update unified diff; runtime maximum is 262144 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 262144
                    }
                },
                "required": ["patch"],
                "additionalProperties": false
            }),
        )?);
        Ok(Self {
            workspace,
            schemas: schemas.into(),
        })
    }

    #[must_use]
    pub fn schemas(&self) -> &[ToolSchema] {
        &self.schemas
    }

    #[must_use]
    pub fn workspace(&self) -> &Path {
        self.workspace.display_root()
    }
}

#[cfg(unix)]
impl ToolExecutor for WorkspaceToolRegistry {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        if request.name() == "apply_patch" {
            return Box::pin(async {
                ToolCallError::model(
                    "ApprovalError",
                    "APPROVAL_REQUIRED",
                    "apply_patch must use the Agent approval preparation stage",
                )
                .into_execution_result()
            });
        }
        let workspace = self.workspace.clone();
        Box::pin(async move {
            let outcome = dispatch(
                &workspace,
                request.name(),
                request.arguments().as_value(),
                &cancellation,
            )
            .await;
            match outcome {
                Ok(text) => normalize_success(text),
                Err(error) => error.into_execution_result(),
            }
        })
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let workspace = self.workspace.clone();
        Box::pin(async move {
            if request.name() == "apply_patch" {
                return patch::prepare(&workspace, request.arguments().as_value(), &cancellation)
                    .await;
            }
            let outcome = dispatch(
                &workspace,
                request.name(),
                request.arguments().as_value(),
                &cancellation,
            )
            .await;
            let result = match outcome {
                Ok(text) => normalize_success(text),
                Err(error) => error.into_execution_result(),
            }?;
            Ok(ToolPreparation::Complete(result))
        })
    }
}

async fn dispatch(
    workspace: &Workspace,
    name: &str,
    arguments: &serde_json::Value,
    cancellation: &CancellationToken,
) -> ToolCallResult<String> {
    if cancellation.is_cancelled() {
        return Err(ToolCallError::aborted());
    }
    match name {
        "list" => list::execute(workspace, parse_list(arguments)?, cancellation).await,
        "glob" => glob::execute(workspace, parse_glob(arguments)?, cancellation).await,
        "grep" => grep::execute(workspace, parse_grep(arguments)?, cancellation).await,
        "read" => read::execute(workspace, parse_read(arguments)?, cancellation).await,
        _ => Err(ToolCallError::unknown_tool()),
    }
}

fn normalize_success(text: String) -> Result<ToolExecutionResult, ToolExecutorError> {
    if text.len() > MAX_TOOL_CONTENT_BYTES {
        return ToolCallError::output_limit().into_execution_result();
    }
    let block = ContentBlock::text(text)
        .map_err(|_| ToolExecutorError::new("read-only tool output normalization failed"))?;
    if block.raw().encoded_len() > MAX_TOOL_CONTENT_BYTES {
        return ToolCallError::output_limit().into_execution_result();
    }
    ToolExecutionResult::success(vec![block])
        .map_err(|_| ToolExecutorError::new("read-only tool output normalization failed"))
}

fn build_schemas() -> Result<Vec<ToolSchema>, ToolRegistryBuildError> {
    Ok(vec![
        schema(
            "list",
            "List one workspace directory without reading file contents.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Nonblank workspace-relative or inside-workspace absolute directory path (default: .); maximum is 4096 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 4096,
                        "pattern": "^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]*$"
                    }
                },
                "additionalProperties": false
            }),
        )?,
        schema(
            "glob",
            "Find workspace files whose relative path matches a glob pattern.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern; a basename pattern matches at any depth; runtime maximum is 4096 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 4096,
                        "pattern": "^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]*$"
                    },
                    "path": {
                        "type": "string",
                        "description": "Nonblank workspace-relative or inside-workspace absolute directory path (default: .); maximum is 4096 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 4096,
                        "pattern": "^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]*$"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )?,
        schema(
            "grep",
            "Search workspace file lines with a Rust regular expression.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression; runtime maximum is 4096 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 4096,
                        "pattern": "^[^\\u0000-\\u001F\\u007F-\\u009F]+$"
                    },
                    "path": {
                        "type": "string",
                        "description": "Nonblank workspace-relative or inside-workspace absolute file or directory path (default: .); maximum is 4096 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 4096,
                        "pattern": "^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]*$"
                    },
                    "include": {
                        "type": "string",
                        "description": "One positive file glob, for example *.{rs,toml}; runtime maximum is 4096 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 4096,
                        "pattern": "^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]*$"
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )?,
        schema(
            "read",
            "Read a bounded UTF-8 page from one regular workspace file.",
            json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Nonblank workspace-relative or inside-workspace absolute regular file path; maximum is 4096 UTF-8 bytes",
                        "minLength": 1,
                        "maxLength": 4096,
                        "pattern": "^(?=.*\\S)[^\\u0000-\\u001F\\u007F-\\u009F]*$"
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "One-based first line (default: 1)"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 2000,
                        "description": "Maximum lines to return (default: 2000)"
                    }
                },
                "required": ["file_path"],
                "additionalProperties": false
            }),
        )?,
    ])
}

fn schema(
    name: &'static str,
    description: &'static str,
    parameters: serde_json::Value,
) -> Result<ToolSchema, ToolRegistryBuildError> {
    let parameters =
        JsonValue::new(parameters).map_err(|source| ToolRegistryBuildError::InvalidSchema {
            tool: name,
            source: source.into(),
        })?;
    ToolSchema::new(name, description, parameters)
        .map_err(|source| ToolRegistryBuildError::InvalidSchema { tool: name, source })
}

#[cfg(test)]
mod tests {
    use super::{MAX_TOOL_CONTENT_BYTES, normalize_success};
    use crate::tools::EMPTY_TEXT_BLOCK_JSON_BYTES;

    #[test]
    fn normalized_content_budget_accepts_the_exact_json_limit_and_rejects_one_more() {
        let exact = "x".repeat(MAX_TOOL_CONTENT_BYTES - EMPTY_TEXT_BLOCK_JSON_BYTES);
        let accepted = normalize_success(exact).unwrap();
        assert!(!accepted.is_error());
        assert_eq!(
            accepted.content()[0].raw().encoded_len(),
            MAX_TOOL_CONTENT_BYTES
        );

        let one_over = "x".repeat(MAX_TOOL_CONTENT_BYTES - EMPTY_TEXT_BLOCK_JSON_BYTES + 1);
        let rejected = normalize_success(one_over).unwrap();
        assert!(rejected.is_error());
        assert_eq!(
            rejected.error().map(|error| error.code.as_str()),
            Some("TOOL_OUTPUT_LIMIT")
        );
    }
}
