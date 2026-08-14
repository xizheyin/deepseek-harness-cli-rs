use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use deepseek_harness_cli::{
    agent::{
        AgentLoop, AgentLoopConfig, ToolExecutionFuture, ToolExecutionRequest, ToolExecutor,
        TurnProposal,
    },
    model::{
        ContentBlock, ContentBlockType, FinishReason, LlmCallConfig, LlmCallConfigAdapterDefaults,
        Message, MessageSource, NonNegativeSafeInteger, StreamChunk,
    },
    provider::{
        ModelProvider, PreparedProviderCall, ProviderPrepareError, ProviderRequest, ProviderStream,
        RetryBackoff, RetryPolicy,
    },
    session::{EventKind, Session, TurnEndReason},
    tools::ReadOnlyToolRegistry,
};
use futures_util::stream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

static NEXT_TEMP_ROOT: AtomicU64 = AtomicU64::new(0);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

struct TempTree {
    root: PathBuf,
    workspace: PathBuf,
}

impl TempTree {
    fn new(label: &str) -> Self {
        let unique = NEXT_TEMP_ROOT.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("the test clock must be after the Unix epoch")
            .as_nanos();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let temporary_base = PathBuf::from("/tmp");
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let temporary_base = std::env::temp_dir();
        let root = temporary_base.join(format!(
            "dsh-read-only-security-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).expect("the temporary workspace must be creatable");
        Self { root, workspace }
    }

    fn workspace(&self) -> &Path {
        &self.workspace
    }

    fn write_workspace(&self, relative: impl AsRef<Path>, content: &[u8]) -> PathBuf {
        self.write_at(self.workspace.join(relative), content)
    }

    fn write_at(&self, path: PathBuf, content: &[u8]) -> PathBuf {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the temporary parent directory must be creatable");
        }
        fs::write(&path, content).expect("the temporary file must be writable");
        path
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ScriptedProvider {
    streams: Mutex<VecDeque<Vec<StreamChunk>>>,
}

impl ScriptedProvider {
    fn for_tool(name: &str, arguments: &Value) -> Self {
        Self {
            streams: Mutex::new(
                vec![
                    tool_response(name, &arguments.to_string()),
                    text_response("done"),
                ]
                .into(),
            ),
        }
    }
}

impl ModelProvider for ScriptedProvider {
    fn prepare_call(
        &self,
        config: LlmCallConfig,
    ) -> Result<PreparedProviderCall, ProviderPrepareError> {
        let mut raw = config.raw().as_value().clone();
        raw.as_object_mut()
            .expect("the validated call config must be an object")
            .insert("maxTokens".to_owned(), json!(1_024));
        let effective = serde_json::from_value(raw)
            .expect("adding one bounded integer must preserve a valid call config");
        Ok(PreparedProviderCall::new(
            effective,
            LlmCallConfigAdapterDefaults::default(),
            Some(NonNegativeSafeInteger::new(4_096).expect("4096 is a safe integer")),
        )
        .with_retry_policy(
            RetryPolicy::normal(
                0,
                vec!["SERVER".to_owned()],
                RetryBackoff::new(1.0, 1.0, 0.0).expect("the fixed backoff is valid"),
            )
            .expect("the fixed retry policy is valid"),
        ))
    }

    fn stream(
        &self,
        _request: ProviderRequest,
        _cancellation: CancellationToken,
    ) -> ProviderStream {
        let chunks = self
            .streams
            .lock()
            .expect("the scripted provider lock must not be poisoned")
            .pop_front()
            .expect("the test provides one stream per model request");
        Box::pin(stream::iter(chunks.into_iter().map(Ok)))
    }
}

struct ObservedResult {
    text: String,
    error_code: Option<String>,
}

struct CancelBeforeRegistry {
    registry: ReadOnlyToolRegistry,
}

impl ToolExecutor for CancelBeforeRegistry {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        cancellation.cancel();
        self.registry.execute(request, cancellation)
    }
}

async fn run_tool(
    registry: Arc<ReadOnlyToolRegistry>,
    name: &str,
    arguments: Value,
) -> ObservedResult {
    let schemas = registry.schemas().to_vec();
    let executor: Arc<dyn ToolExecutor> = registry;
    let config = AgentLoopConfig::new(
        LlmCallConfig::new("mock", "model").expect("the fixed model route is valid"),
    )
    .with_tools(schemas)
    .expect("the registry schemas are valid");
    let provider = Arc::new(ScriptedProvider::for_tool(name, &arguments));
    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let mut agent = AgentLoop::new(
        Session::new(format!("read-only-security-{session_id}"))
            .expect("the fixed session id is valid"),
        provider,
        executor,
        config,
    )
    .expect("the fresh Agent must be constructible");

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user_message("inspect the test workspace")]),
            CancellationToken::new(),
        )
        .await
        .expect("a model-facing filesystem error must not break Agent infrastructure");
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);

    let (message, error) = agent
        .session()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::ToolResult { message, error, .. } => Some((message, error)),
            _ => None,
        })
        .expect("the tool call must have one durable result");

    let mut text = String::new();
    for outer in message.content() {
        let Some(content) = outer.tool_result_content() else {
            continue;
        };
        for block in content {
            if let Some(value) = block.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(value);
            }
        }
    }

    ObservedResult {
        text,
        error_code: error.as_ref().map(|failure| failure.code.clone()),
    }
}

fn registry(tree: &TempTree) -> Arc<ReadOnlyToolRegistry> {
    Arc::new(
        ReadOnlyToolRegistry::open(tree.workspace())
            .expect("the temporary workspace must be a valid capability root"),
    )
}

fn assert_denied_without(result: &ObservedResult, forbidden: &[&str]) {
    assert_eq!(
        result.error_code.as_deref(),
        Some("WORKSPACE_PATH_DENIED"),
        "unexpected result: {}",
        result.text
    );
    for value in forbidden {
        assert!(
            !result.text.contains(value),
            "denied output disclosed `{value}`: {}",
            result.text
        );
    }
}

fn user_message(text: &str) -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text(text).expect("the fixed user text is valid")],
        MessageSource::user().expect("the fixed user source is valid"),
    )
    .expect("the fixed user message is valid")
}

fn tool_response(name: &str, arguments: &str) -> Vec<StreamChunk> {
    let block =
        ContentBlock::tool_call("call-1", name, arguments).expect("the fixed tool call is valid");
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall)
            .expect("the tool block start is valid"),
        StreamChunk::block_end(0, block).expect("the tool block end is valid"),
        StreamChunk::finish(
            FinishReason::tool_calls().expect("the finish reason is valid"),
            None,
        )
        .expect("the tool finish chunk is valid"),
    ]
}

fn text_response(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).expect("the text block start is valid"),
        StreamChunk::text_delta(0, text).expect("the text delta is valid"),
        StreamChunk::block_end(
            0,
            ContentBlock::text(text).expect("the fixed assistant text is valid"),
        )
        .expect("the text block end is valid"),
        StreamChunk::finish(
            FinishReason::stop().expect("the finish reason is valid"),
            None,
        )
        .expect("the text finish chunk is valid"),
    ]
}

#[tokio::test]
async fn every_tool_rejects_parent_absolute_and_prefix_sibling_paths() {
    let tree = TempTree::new("outside-paths");
    let sibling_secret = tree.write_at(
        tree.root.join("sibling").join("secret.txt"),
        b"SIBLING_SECRET_SENTINEL\n",
    );
    let prefix_secret = tree.write_at(
        tree.root.join("workspace-private").join("secret.txt"),
        b"PREFIX_SECRET_SENTINEL\n",
    );
    let registry = registry(&tree);

    for (name, arguments) in [
        ("list", json!({"path": "../sibling"})),
        ("glob", json!({"pattern": "*", "path": "../sibling"})),
        ("grep", json!({"pattern": "SECRET", "path": "../sibling"})),
        ("read", json!({"file_path": "../sibling/secret.txt"})),
    ] {
        let result = run_tool(Arc::clone(&registry), name, arguments).await;
        assert_denied_without(&result, &["SIBLING_SECRET_SENTINEL"]);
    }

    let sibling_directory = sibling_secret
        .parent()
        .expect("the sibling secret must have a parent");
    for (name, arguments) in [
        ("list", json!({"path": sibling_directory.to_string_lossy()})),
        (
            "glob",
            json!({"pattern": "*", "path": sibling_directory.to_string_lossy()}),
        ),
        (
            "grep",
            json!({"pattern": "SECRET", "path": sibling_directory.to_string_lossy()}),
        ),
        (
            "read",
            json!({"file_path": sibling_secret.to_string_lossy()}),
        ),
    ] {
        let result = run_tool(Arc::clone(&registry), name, arguments).await;
        assert_denied_without(
            &result,
            &[
                "SIBLING_SECRET_SENTINEL",
                sibling_secret.to_string_lossy().as_ref(),
            ],
        );
    }

    let prefix_directory = prefix_secret
        .parent()
        .expect("the prefix-sibling secret must have a parent");
    for (name, arguments) in [
        ("list", json!({"path": prefix_directory.to_string_lossy()})),
        (
            "glob",
            json!({"pattern": "*", "path": prefix_directory.to_string_lossy()}),
        ),
        (
            "grep",
            json!({"pattern": "SECRET", "path": prefix_directory.to_string_lossy()}),
        ),
        (
            "read",
            json!({"file_path": prefix_secret.to_string_lossy()}),
        ),
    ] {
        let result = run_tool(Arc::clone(&registry), name, arguments).await;
        assert_denied_without(
            &result,
            &[
                "PREFIX_SECRET_SENTINEL",
                prefix_secret.to_string_lossy().as_ref(),
            ],
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn file_symlinks_allow_only_live_targets_inside_the_workspace() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new("file-symlinks");
    tree.write_workspace("real/internal.txt", b"INTERNAL_LINK_SENTINEL\n");
    let outside = tree.write_at(tree.root.join("outside.txt"), b"EXTERNAL_LINK_SENTINEL\n");
    symlink(
        "real/internal.txt",
        tree.workspace.join("internal-link.txt"),
    )
    .expect("an internal test symlink must be creatable");
    symlink(&outside, tree.workspace.join("external-link.txt"))
        .expect("an external test symlink must be creatable");
    symlink("missing.txt", tree.workspace.join("broken-link.txt"))
        .expect("a broken test symlink must be creatable");
    symlink("loop-b.txt", tree.workspace.join("loop-a.txt"))
        .expect("the first loop symlink must be creatable");
    symlink("loop-a.txt", tree.workspace.join("loop-b.txt"))
        .expect("the second loop symlink must be creatable");
    let registry = registry(&tree);

    let internal = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "internal-link.txt"}),
    )
    .await;
    assert_eq!(internal.error_code, None, "{}", internal.text);
    assert!(internal.text.contains("INTERNAL_LINK_SENTINEL"));

    let external = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "external-link.txt"}),
    )
    .await;
    assert_denied_without(
        &external,
        &["EXTERNAL_LINK_SENTINEL", outside.to_string_lossy().as_ref()],
    );

    let broken = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "broken-link.txt"}),
    )
    .await;
    assert_eq!(broken.error_code.as_deref(), Some("FS_NOT_FOUND"));

    let cycle = run_tool(registry, "read", json!({"file_path": "loop-a.txt"})).await;
    assert!(
        cycle.error_code.is_some(),
        "a symbolic-link loop must not be returned as a successful file: {}",
        cycle.text
    );
    assert!(!cycle.text.contains("INTERNAL_LINK_SENTINEL"));
    assert!(!cycle.text.contains("EXTERNAL_LINK_SENTINEL"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn recursive_search_never_descends_into_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let tree = TempTree::new("directory-symlinks");
    tree.write_workspace(
        "real-dir/inside.rs",
        b"fn inside() { /* INTERNAL_DIRECTORY_MARKER */ }\n",
    );
    let outside_dir = tree.root.join("outside-dir");
    tree.write_at(
        outside_dir.join("outside.rs"),
        b"fn outside() { /* EXTERNAL_DIRECTORY_MARKER */ }\n",
    );
    symlink("real-dir", tree.workspace.join("internal-dir-link"))
        .expect("the internal directory symlink must be creatable");
    symlink(&outside_dir, tree.workspace.join("external-dir-link"))
        .expect("the external directory symlink must be creatable");
    let registry = registry(&tree);

    let glob = run_tool(
        Arc::clone(&registry),
        "glob",
        json!({"pattern": "*.rs", "path": "."}),
    )
    .await;
    assert_eq!(glob.error_code, None, "{}", glob.text);
    assert!(glob.text.contains("real-dir/inside.rs"), "{}", glob.text);
    assert!(!glob.text.contains("internal-dir-link"), "{}", glob.text);
    assert!(!glob.text.contains("external-dir-link"), "{}", glob.text);
    assert!(!glob.text.contains("outside.rs"), "{}", glob.text);

    let grep = run_tool(
        Arc::clone(&registry),
        "grep",
        json!({"pattern": "DIRECTORY_MARKER", "path": "."}),
    )
    .await;
    assert_eq!(grep.error_code, None, "{}", grep.text);
    assert!(grep.text.contains("INTERNAL_DIRECTORY_MARKER"));
    assert!(!grep.text.contains("EXTERNAL_DIRECTORY_MARKER"));
    assert!(!grep.text.contains("internal-dir-link"));
    assert!(!grep.text.contains("external-dir-link"));

    for (tool, arguments) in [
        ("list", json!({"path": "internal-dir-link"})),
        (
            "glob",
            json!({"pattern": "*.rs", "path": "internal-dir-link"}),
        ),
        (
            "grep",
            json!({"pattern": "DIRECTORY_MARKER", "path": "internal-dir-link"}),
        ),
    ] {
        let selected_alias = run_tool(Arc::clone(&registry), tool, arguments).await;
        assert!(
            selected_alias.error_code.is_some(),
            "{tool} must not recurse through a selected directory symlink: {}",
            selected_alias.text
        );
        assert!(!selected_alias.text.contains("INTERNAL_DIRECTORY_MARKER"));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn discovered_control_names_fail_without_injecting_output() {
    let tree = TempTree::new("control-name");
    fs::write(
        tree.workspace.join("evil\ninjected.txt"),
        b"INVALID_NAME_SECRET\n",
    )
    .expect("the control-character test file must be writable");
    let registry = registry(&tree);

    for (tool, arguments) in [
        ("list", json!({"path": "."})),
        ("glob", json!({"pattern": "*", "path": "."})),
        ("grep", json!({"pattern": "SECRET", "path": "."})),
    ] {
        let result = run_tool(Arc::clone(&registry), tool, arguments).await;
        assert_eq!(
            result.error_code.as_deref(),
            Some("FS_INVALID_NAME"),
            "unexpected {tool} result: {}",
            result.text
        );
        assert!(!result.text.contains("INVALID_NAME_SECRET"));
        assert!(!result.text.contains("injected.txt"));
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn discovered_non_utf8_names_fail_without_disclosing_content() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let tree = TempTree::new("non-utf8-name");
    let name = OsString::from_vec(vec![b'e', b'v', b'i', b'l', 0x80, b'.', b't', b'x', b't']);
    fs::write(tree.workspace.join(name), b"NON_UTF8_NAME_SECRET\n")
        .expect("Linux must permit the non-UTF-8 test file name");
    let registry = registry(&tree);

    for (tool, arguments) in [
        ("list", json!({"path": "."})),
        ("glob", json!({"pattern": "*", "path": "."})),
        ("grep", json!({"pattern": "SECRET", "path": "."})),
    ] {
        let result = run_tool(Arc::clone(&registry), tool, arguments).await;
        assert_eq!(result.error_code.as_deref(), Some("FS_INVALID_NAME"));
        assert!(!result.text.contains("NON_UTF8_NAME_SECRET"));
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn replacing_the_workspace_path_does_not_replace_the_open_capability_root() {
    let tree = TempTree::new("root-replacement");
    tree.write_workspace("old.txt", b"ORIGINAL_ROOT_SENTINEL\n");
    let registry = registry(&tree);

    let original_location = tree.root.join("original-workspace");
    fs::rename(tree.workspace(), &original_location)
        .expect("the opened temporary workspace must be renameable");
    fs::create_dir(tree.workspace()).expect("the replacement workspace path must be creatable");
    fs::write(
        tree.workspace.join("replacement-secret.txt"),
        b"REPLACEMENT_ROOT_SECRET\n",
    )
    .expect("the replacement secret must be writable");

    let original = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "old.txt"}),
    )
    .await;
    assert_eq!(original.error_code, None, "{}", original.text);
    assert!(original.text.contains("ORIGINAL_ROOT_SENTINEL"));

    let replacement = run_tool(
        registry,
        "read",
        json!({"file_path": "replacement-secret.txt"}),
    )
    .await;
    assert_eq!(replacement.error_code.as_deref(), Some("FS_NOT_FOUND"));
    assert!(!replacement.text.contains("REPLACEMENT_ROOT_SECRET"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn read_rejects_directories_sockets_and_fifos_without_blocking() {
    use std::os::unix::net::UnixListener;
    use std::process::Command;
    use std::time::Duration;

    let tree = TempTree::new("special-files");
    fs::create_dir(tree.workspace.join("directory"))
        .expect("the temporary directory must be creatable");
    let _listener = UnixListener::bind(tree.workspace.join("agent.sock"))
        .expect("the temporary Unix socket must be bindable");
    let status = Command::new("mkfifo")
        .arg(tree.workspace.join("agent.fifo"))
        .status()
        .expect("the platform mkfifo test helper must start");
    assert!(status.success(), "the temporary FIFO must be creatable");
    let registry = registry(&tree);

    let listing = run_tool(Arc::clone(&registry), "list", json!({"path": "."})).await;
    assert_eq!(listing.error_code, None);
    assert!(listing.text.contains("directory\tdirectory/"));
    assert!(listing.text.contains("other\tagent.sock"));
    assert!(listing.text.contains("other\tagent.fifo"));

    let directory = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "directory"}),
    )
    .await;
    assert_eq!(directory.error_code.as_deref(), Some("FS_NOT_REGULAR_FILE"));

    let socket = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "agent.sock"}),
    )
    .await;
    assert!(
        socket.error_code.is_some(),
        "a Unix socket must never be returned as file content: {}",
        socket.text
    );

    let fifo = tokio::time::timeout(
        Duration::from_secs(1),
        run_tool(registry, "read", json!({"file_path": "agent.fifo"})),
    )
    .await
    .expect("opening a FIFO must not wait for a writer");
    assert_eq!(fifo.error_code.as_deref(), Some("FS_NOT_REGULAR_FILE"));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn ordinary_workspace_permission_failures_are_not_misreported_as_escape_attempts() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;

    let tree = TempTree::new("permissions");
    tree.write_workspace("private/secret.txt", b"PERMISSION_SECRET\n");
    let registry = registry(&tree);
    let private = tree.workspace.join("private");
    fs::set_permissions(&private, fs::Permissions::from_mode(0o000))
        .expect("the temporary directory permissions must be changeable");

    let result = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "private/secret.txt"}),
    )
    .await;

    fs::set_permissions(&private, fs::Permissions::from_mode(0o700))
        .expect("the temporary directory must be restored for cleanup");
    assert_eq!(result.error_code.as_deref(), Some("FS_PERMISSION_DENIED"));
    assert!(!result.text.contains("PERMISSION_SECRET"));

    let protected = tree.write_workspace("protected.txt", b"LINK_PERMISSION_SECRET\n");
    symlink("protected.txt", tree.workspace.join("protected-link.txt"))
        .expect("the internal file symlink must be creatable");
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o000))
        .expect("the temporary file permissions must be changeable");
    let through_link = run_tool(
        Arc::clone(&registry),
        "read",
        json!({"file_path": "protected-link.txt"}),
    )
    .await;
    fs::set_permissions(&protected, fs::Permissions::from_mode(0o600))
        .expect("the temporary file must be restored for cleanup");
    assert_eq!(
        through_link.error_code.as_deref(),
        Some("FS_PERMISSION_DENIED")
    );
    assert!(!through_link.text.contains("LINK_PERMISSION_SECRET"));

    tree.write_workspace(
        "locked-target/secret.txt",
        b"LOCKED_TARGET_PERMISSION_SECRET\n",
    );
    symlink(
        "locked-target/secret.txt",
        tree.workspace.join("locked-target-link.txt"),
    )
    .expect("the internal nested file symlink must be creatable");
    let locked_target = tree.workspace.join("locked-target");
    fs::set_permissions(&locked_target, fs::Permissions::from_mode(0o000))
        .expect("the target directory permissions must be changeable");
    let through_locked_parent = run_tool(
        registry,
        "read",
        json!({"file_path": "locked-target-link.txt"}),
    )
    .await;
    fs::set_permissions(&locked_target, fs::Permissions::from_mode(0o700))
        .expect("the target directory must be restored for cleanup");
    assert_eq!(
        through_locked_parent.error_code.as_deref(),
        Some("FS_PERMISSION_DENIED")
    );
    assert!(
        !through_locked_parent
            .text
            .contains("LOCKED_TARGET_PERMISSION_SECRET")
    );
}

#[tokio::test]
async fn a_pre_cancelled_registry_call_returns_aborted_without_reading() {
    let tree = TempTree::new("pre-cancel");
    tree.write_workspace("must-not-read.txt", b"PRE_CANCEL_SECRET\n");
    let registry = ReadOnlyToolRegistry::open(tree.workspace())
        .expect("the temporary workspace must be a valid capability root");
    let schemas = registry.schemas().to_vec();
    let executor: Arc<dyn ToolExecutor> = Arc::new(CancelBeforeRegistry { registry });
    let config = AgentLoopConfig::new(
        LlmCallConfig::new("mock", "model").expect("the fixed model route is valid"),
    )
    .with_tools(schemas)
    .expect("the registry schemas are valid");
    let arguments = json!({"file_path": "must-not-read.txt"});
    let provider = Arc::new(ScriptedProvider::for_tool("read", &arguments));
    let mut agent = AgentLoop::new(
        Session::new("read-only-security-pre-cancel").expect("the fixed session id is valid"),
        provider,
        executor,
        config,
    )
    .expect("the fresh Agent must be constructible");
    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user_message("inspect the test file")]),
            CancellationToken::new(),
        )
        .await
        .expect("registry cancellation is an ordinary model-facing result");

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    let (message, error) = agent
        .session()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::ToolResult { message, error, .. } => Some((message, error)),
            _ => None,
        })
        .expect("the cancelled registry call must have a durable result");
    assert_eq!(
        error.as_ref().map(|failure| failure.code.as_str()),
        Some("ABORTED")
    );
    for outer in message.content() {
        let Some(content) = outer.tool_result_content() else {
            continue;
        };
        for block in content {
            assert!(
                !block
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains("PRE_CANCEL_SECRET"))
            );
        }
    }
}
