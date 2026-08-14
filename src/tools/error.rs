use std::{io, path::PathBuf};

use thiserror::Error;

use crate::{
    agent::{ToolExecutionResult, ToolExecutorError},
    model::{ContentBlock, ModelError},
    session::ToolFailure,
};

/// Failure while fixing the immutable workspace/tool catalogue.
#[derive(Debug, Error)]
pub enum ToolRegistryBuildError {
    #[error("the read-only workspace is not an accessible directory")]
    InvalidWorkspace {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the built-in {tool} schema is invalid")]
    InvalidSchema {
        tool: &'static str,
        #[source]
        source: ModelError,
    },
}

#[derive(Debug)]
pub(crate) enum ToolCallError {
    Model {
        name: &'static str,
        code: &'static str,
        message: String,
    },
    Infrastructure,
}

impl ToolCallError {
    pub(crate) fn has_code(&self, expected: &str) -> bool {
        matches!(self, Self::Model { code, .. } if *code == expected)
    }

    pub(crate) fn invalid_args(message: impl Into<String>) -> Self {
        Self::model("ToolArgsError", "INVALID_ARGS", message)
    }

    pub(crate) fn workspace_denied() -> Self {
        Self::model(
            "FsError",
            "WORKSPACE_PATH_DENIED",
            "the requested path is outside the workspace or crosses an unsafe symbolic link",
        )
    }

    pub(crate) fn not_found(path: &str) -> Self {
        Self::model(
            "FsError",
            "FS_NOT_FOUND",
            format!("workspace path `{path}` was not found"),
        )
    }

    pub(crate) fn not_directory(path: &str) -> Self {
        Self::model(
            "FsError",
            "FS_NOT_DIRECTORY",
            format!("workspace path `{path}` is not a directory"),
        )
    }

    pub(crate) fn not_regular_file(path: &str) -> Self {
        Self::model(
            "FsError",
            "FS_NOT_REGULAR_FILE",
            format!("workspace path `{path}` is not a regular file"),
        )
    }

    pub(crate) fn not_text(path: &str) -> Self {
        Self::model(
            "FsError",
            "FS_NOT_TEXT",
            format!("workspace file `{path}` is binary or is not valid UTF-8"),
        )
    }

    pub(crate) fn too_large(path: &str) -> Self {
        Self::model(
            "FsError",
            "FS_TOO_LARGE",
            format!("workspace file `{path}` exceeds the read limit"),
        )
    }

    pub(crate) fn changed(path: &str) -> Self {
        Self::model(
            "FsError",
            "FS_CHANGED",
            format!("workspace file `{path}` changed while it was being read"),
        )
    }

    pub(crate) fn invalid_pattern(message: impl Into<String>) -> Self {
        Self::model("SearchError", "SEARCH_INVALID_PATTERN", message)
    }

    pub(crate) fn search_limit(message: impl Into<String>) -> Self {
        Self::model("SearchError", "SEARCH_LIMIT_EXCEEDED", message)
    }

    pub(crate) fn output_limit() -> Self {
        Self::model(
            "ToolOutputError",
            "TOOL_OUTPUT_LIMIT",
            "the normalized tool output exceeds the configured limit",
        )
    }

    pub(crate) fn aborted() -> Self {
        Self::model("AbortError", "ABORTED", "tool execution was cancelled")
    }

    pub(crate) fn unknown_tool() -> Self {
        Self::model(
            "ToolError",
            "UNKNOWN_TOOL",
            "the requested tool is not present in this read-only registry",
        )
    }

    pub(crate) fn io(error: &io::Error, path: &str, expects_directory: bool) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::not_found(path),
            io::ErrorKind::PermissionDenied => Self::model(
                "FsError",
                "FS_PERMISSION_DENIED",
                format!("permission denied for workspace path `{path}`"),
            ),
            io::ErrorKind::NotADirectory => Self::not_directory(path),
            io::ErrorKind::InvalidInput => Self::workspace_denied(),
            _ if expects_directory => Self::model(
                "FsError",
                "FS_IO_ERROR",
                format!("could not list workspace directory `{path}`"),
            ),
            _ => Self::model(
                "FsError",
                "FS_IO_ERROR",
                format!("could not read workspace path `{path}`"),
            ),
        }
    }

    pub(crate) fn model(
        name: &'static str,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self::Model {
            name,
            code,
            message: message.into(),
        }
    }

    pub(crate) fn into_execution_result(self) -> Result<ToolExecutionResult, ToolExecutorError> {
        let (name, code, message) = self.into_model_parts()?;
        let content = ContentBlock::text(format!("Error: {message}"))
            .map_err(|_| ToolExecutorError::new("read-only tool error normalization failed"))?;
        ToolExecutionResult::model_error(
            vec![content],
            ToolFailure {
                name: name.to_owned(),
                code: code.to_owned(),
            },
        )
        .map_err(|_| ToolExecutorError::new("read-only tool error normalization failed"))
    }

    pub(crate) fn into_model_parts(
        self,
    ) -> Result<(&'static str, &'static str, String), ToolExecutorError> {
        let Self::Model {
            name,
            code,
            message,
        } = self
        else {
            return Err(ToolExecutorError::new(
                "built-in tool infrastructure failed",
            ));
        };
        Ok((name, code, message))
    }
}

pub(crate) type ToolCallResult<T> = Result<T, ToolCallError>;
