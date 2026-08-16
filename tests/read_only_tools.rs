use std::{
    collections::{BTreeSet, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use deepseek_harness_cli::{
    agent::{AgentLoop, AgentLoopConfig, ToolExecutor, TurnProposal},
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, NonNegativeSafeInteger, StreamChunk,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream, RetryBackoff,
        RetryPolicy,
    },
    session::{EventKind, Session, ToolFailure, TurnEndReason},
    tools::ReadOnlyToolRegistry,
};
use futures_util::stream;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

static NEXT_TEMP_DIR: AtomicU64 = AtomicU64::new(0);
static NEXT_SESSION: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace {
    root: PathBuf,
    workspace: PathBuf,
}

impl TempWorkspace {
    fn new(label: &str) -> Self {
        let unique = NEXT_TEMP_DIR.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "dsh-read-only-tools-{label}-{}-{nanos}-{unique}",
            std::process::id()
        ));
        let workspace = root.join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        Self { root, workspace }
    }

    fn path(&self) -> &Path {
        &self.workspace
    }

    fn write(&self, relative: impl AsRef<Path>, bytes: impl AsRef<[u8]>) {
        let path = self.workspace.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();
    }

    fn mkdir(&self, relative: impl AsRef<Path>) {
        let path = self.workspace.join(relative);
        fs::create_dir_all(&path).unwrap();
    }

    fn outside_file(&self, name: &str, bytes: impl AsRef<[u8]>) -> PathBuf {
        let path = self.root.join(name);
        fs::write(&path, bytes).unwrap();
        path
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ScriptedProvider {
    streams: Mutex<VecDeque<Vec<StreamChunk>>>,
    requests: Mutex<Vec<(Vec<Message>, Vec<String>)>>,
}

impl ScriptedProvider {
    fn for_tool(name: &str, arguments: &str) -> Self {
        Self {
            streams: Mutex::new(vec![tool_response(name, arguments), text_response("done")].into()),
            requests: Mutex::new(Vec::new()),
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
            .unwrap()
            .insert("maxTokens".to_owned(), json!(1_024));
        let effective = serde_json::from_value(raw).unwrap();
        Ok(PreparedProviderCall::new(
            effective,
            LlmCallConfigAdapterDefaults::default(),
            Some(NonNegativeSafeInteger::new(4_096).unwrap()),
        )
        .with_retry_policy(
            RetryPolicy::normal(
                0,
                vec!["SERVER".to_owned()],
                RetryBackoff::new(1.0, 1.0, 0.0).unwrap(),
            )
            .unwrap(),
        ))
    }

    fn preflight_request(
        &self,
        draft: ProviderRequestDraft<'_>,
    ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
        let prepared = self.prepare_call(draft.config().clone())?;
        draft.finish(prepared, 1)
    }

    fn stream(&self, request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        self.requests.lock().unwrap().push((
            request.messages().to_vec(),
            request
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect(),
        ));
        let chunks = self.streams.lock().unwrap().pop_front().unwrap();
        Box::pin(stream::iter(chunks.into_iter().map(Ok)))
    }
}

struct ObservedToolResult {
    text: String,
    error: Option<ToolFailure>,
    meta_present: bool,
    turn_end: TurnEndReason,
}

async fn run_tool(workspace: &Path, name: &str, arguments: Value) -> ObservedToolResult {
    let registry = Arc::new(ReadOnlyToolRegistry::open(workspace).unwrap());
    let schema_names = registry
        .schemas()
        .iter()
        .map(|schema| schema.name().to_owned())
        .collect::<Vec<_>>();
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(registry.schemas().to_vec())
        .unwrap();
    let provider = Arc::new(ScriptedProvider::for_tool(name, &arguments.to_string()));
    let session_id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let mut agent = AgentLoop::new(
        Session::new(format!("read-only-tools-{session_id}")).unwrap(),
        provider.clone(),
        registry.clone(),
        config,
    )
    .unwrap();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user_message("inspect the workspace")]),
            CancellationToken::new(),
        )
        .await
        .unwrap();

    let events = agent.session().events();
    let calls = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.kind(), EventKind::ToolCall { .. }))
        .collect::<Vec<_>>();
    let results = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event.kind(), EventKind::ToolResult { .. }))
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1, "the registry call must be recorded once");
    assert_eq!(
        results.len(),
        1,
        "the registry result must be recorded once"
    );
    let (call_index, call_event) = calls[0];
    let (result_index, result_event) = results[0];
    let assistant_index = events[..call_index]
        .iter()
        .rposition(|event| matches!(event.kind(), EventKind::AssistantMessage { .. }))
        .expect("the model-visible tool call must precede its intention event");
    let step_end_index = events[result_index + 1..]
        .iter()
        .position(|event| matches!(event.kind(), EventKind::StepEnd { .. }))
        .map(|offset| result_index + 1 + offset)
        .expect("the step must close after the tool result");
    assert!(assistant_index < call_index && call_index < result_index);
    assert!(result_index < step_end_index);
    assert_eq!(
        result_event.source_event_seqs(),
        Some([call_event.seq()].as_slice()),
        "the durable result must cite exactly its durable call intention"
    );

    let requests = provider.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "the result must feed one continuation request"
    );
    assert_eq!(
        requests[0].1, schema_names,
        "the provider must receive the same closed registry catalogue"
    );
    assert!(requests[1].0.iter().any(|message| {
        message.content().iter().any(|block| {
            matches!(
                block.kind(),
                ContentBlockKind::ToolResult { tool_call_id, .. }
                    if tool_call_id.as_str() == "call-1"
            )
        })
    }));

    let (message, error, meta) = agent
        .session()
        .events()
        .iter()
        .find_map(|event| match event.kind() {
            EventKind::ToolResult {
                message,
                error,
                meta,
                ..
            } => Some((message, error.clone(), meta)),
            _ => None,
        })
        .expect("the declared tool call must have one durable result");

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

    ObservedToolResult {
        text,
        error,
        meta_present: meta.is_some(),
        turn_end: outcome.reason().clone(),
    }
}

fn user_message(text: &str) -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text(text).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn tool_response(name: &str, arguments: &str) -> Vec<StreamChunk> {
    let block = ContentBlock::tool_call("call-1", name, arguments).unwrap();
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::block_end(0, block).unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]
}

fn text_response(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::text_delta(0, text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn assert_error_code(result: &ObservedToolResult, expected: &str) {
    assert_eq!(
        result.error.as_ref().map(|error| error.code.as_str()),
        Some(expected),
        "unexpected tool result: {}",
        result.text
    );
    assert_eq!(result.turn_end, TurnEndReason::Completed);
}

fn assert_tool_executor<T: ToolExecutor>() {}

#[test]
fn schemas_have_stable_order_and_closed_parameter_objects() {
    assert_tool_executor::<ReadOnlyToolRegistry>();
    let workspace = TempWorkspace::new("schemas");
    let registry = ReadOnlyToolRegistry::open(workspace.path()).unwrap();
    let debug = format!("{registry:?}");
    assert!(!debug.contains(workspace.path().to_string_lossy().as_ref()));
    assert!(debug.contains("schema_count: 4"));
    let schemas = registry.schemas();

    assert_eq!(
        schemas
            .iter()
            .map(|schema| schema.name())
            .collect::<Vec<_>>(),
        ["list", "glob", "grep", "read"]
    );

    let expected = [
        ("list", &[][..], &["path"][..]),
        ("glob", &["pattern"][..], &["path", "pattern"][..]),
        (
            "grep",
            &["pattern"][..],
            &["include", "path", "pattern"][..],
        ),
        (
            "read",
            &["file_path"][..],
            &["file_path", "limit", "offset"][..],
        ),
    ];

    for (schema, (name, required, properties)) in schemas.iter().zip(expected) {
        assert_eq!(schema.name(), name);
        let parameters = schema.parameters().as_value();
        assert_eq!(parameters.get("type"), Some(&json!("object")));
        assert_eq!(
            parameters.get("additionalProperties"),
            Some(&Value::Bool(false))
        );
        let actual_required = parameters
            .get("required")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().unwrap())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        assert_eq!(actual_required, required);
        let actual_properties = parameters["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            actual_properties,
            properties.iter().copied().collect::<BTreeSet<_>>()
        );
    }

    assert_eq!(
        schemas[3].parameters().as_value()["properties"]["offset"]["minimum"],
        json!(1)
    );
    assert_eq!(
        schemas[3].parameters().as_value()["properties"]["limit"]["maximum"],
        json!(2_000)
    );

    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/tools/upstream_phase4_oracle.json")).unwrap();
    for schema in schemas.iter().filter(|schema| schema.name() != "list") {
        let upstream = &oracle["schemaSurface"]["tools"][schema.name()]["parameters"];
        let rust = schema.parameters().as_value();
        assert_eq!(
            upstream["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<BTreeSet<_>>(),
            rust["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<BTreeSet<_>>()
        );
        assert_eq!(upstream["required"], rust["required"]);
        assert_eq!(upstream.get("additionalProperties"), None);
        assert_eq!(rust["additionalProperties"], json!(false));
    }

    for (tool_index, field) in [
        (0, "path"),
        (1, "pattern"),
        (1, "path"),
        (2, "pattern"),
        (2, "path"),
        (2, "include"),
        (3, "file_path"),
    ] {
        let property = &schemas[tool_index].parameters().as_value()["properties"][field];
        assert_eq!(property["minLength"], json!(1));
        assert_eq!(property["maxLength"], json!(4_096));
        assert!(property.get("pattern").is_some());
    }
}

#[tokio::test]
async fn strict_argument_validation_rejects_unknown_and_missing_fields() {
    let workspace = TempWorkspace::new("arguments");
    workspace.write("readme.txt", b"hello\n");

    let unknown = run_tool(
        workspace.path(),
        "list",
        json!({"path": ".", "unexpected": true}),
    )
    .await;
    assert_error_code(&unknown, "INVALID_ARGS");

    let missing = run_tool(workspace.path(), "read", json!({"offset": 1})).await;
    assert_error_code(&missing, "INVALID_ARGS");

    let invalid_limit = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "readme.txt", "limit": 0}),
    )
    .await;
    assert_error_code(&invalid_limit, "INVALID_ARGS");

    // JSON Schema treats an exactly integral JSON number as an integer. The
    // public Agent path canonicalizes 1.0 to 1 before the registry parses it.
    let integral_float = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "readme.txt", "offset": 1.0}),
    )
    .await;
    assert_eq!(integral_float.error, None);
    assert!(integral_float.text.contains("hello"));

    for (tool, arguments) in [
        ("list", json!({"path": null})),
        ("glob", json!({"pattern": "*", "path": null})),
        ("grep", json!({"pattern": "x", "path": null})),
        ("grep", json!({"pattern": "x", "include": null})),
        ("read", json!({"file_path": "readme.txt", "offset": null})),
        ("read", json!({"file_path": "readme.txt", "limit": null})),
    ] {
        let explicit_null = run_tool(workspace.path(), tool, arguments).await;
        assert_error_code(&explicit_null, "INVALID_ARGS");
    }
}

#[tokio::test]
async fn all_tools_accept_absolute_paths_only_when_they_stay_inside_the_workspace() {
    let workspace = TempWorkspace::new("inside-absolute");
    workspace.mkdir("src");
    workspace.write("src/lib.rs", b"fn inside_absolute() {}\n");
    let directory = workspace.path().join("src");
    let file = directory.join("lib.rs");

    for (tool, arguments, expected) in [
        (
            "list",
            json!({"path": directory.to_string_lossy()}),
            "lib.rs",
        ),
        (
            "glob",
            json!({"pattern": "*.rs", "path": directory.to_string_lossy()}),
            "src/lib.rs",
        ),
        (
            "grep",
            json!({"pattern": "inside_absolute", "path": directory.to_string_lossy()}),
            "inside_absolute",
        ),
        (
            "read",
            json!({"file_path": file.to_string_lossy()}),
            "inside_absolute",
        ),
    ] {
        let result = run_tool(workspace.path(), tool, arguments).await;
        assert_eq!(result.error, None, "{tool} failed: {}", result.text);
        assert!(result.text.contains(expected), "{tool}: {}", result.text);
    }
}

#[tokio::test]
async fn list_glob_grep_and_read_return_model_visible_results() {
    let workspace = TempWorkspace::new("ordinary");
    workspace.mkdir("src");
    workspace.write("README.md", b"alpha\nbeta\n");
    workspace.write("src/lib.rs", b"pub fn alpha() {}\n");
    workspace.write("src/other.txt", b"not selected\n");

    let list = run_tool(workspace.path(), "list", json!({"path": "."})).await;
    assert_eq!(list.error, None);
    assert_eq!(list.turn_end, TurnEndReason::Completed);
    assert!(list.text.contains("README.md"), "{}", list.text);
    assert!(list.text.contains("src/"), "{}", list.text);

    let glob = run_tool(
        workspace.path(),
        "glob",
        json!({"pattern": "*.rs", "path": "."}),
    )
    .await;
    assert_eq!(glob.error, None);
    assert!(glob.text.contains("src/lib.rs"), "{}", glob.text);
    assert!(!glob.text.contains("other.txt"), "{}", glob.text);

    let grep = run_tool(
        workspace.path(),
        "grep",
        json!({"pattern": "alpha", "path": ".", "include": "*.rs"}),
    )
    .await;
    assert_eq!(grep.error, None);
    assert!(grep.text.contains("src/lib.rs"), "{}", grep.text);
    assert!(grep.text.contains("pub fn alpha"), "{}", grep.text);
    assert!(!grep.text.contains("README.md"), "{}", grep.text);

    let read = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "README.md", "offset": 2, "limit": 1}),
    )
    .await;
    assert_eq!(read.error, None);
    assert!(read.text.contains("beta"), "{}", read.text);
    assert!(!read.text.contains("alpha"), "{}", read.text);
}

#[tokio::test]
async fn read_distinguishes_missing_binary_and_invalid_utf8_files() {
    let workspace = TempWorkspace::new("read-errors");
    workspace.write("binary.bin", [b'a', 0, b'b']);
    workspace.write("invalid.txt", [b'a', 0xff, b'\n']);

    let missing = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "missing.txt"}),
    )
    .await;
    assert_error_code(&missing, "FS_NOT_FOUND");

    let binary = run_tool(workspace.path(), "read", json!({"file_path": "binary.bin"})).await;
    assert_error_code(&binary, "FS_NOT_TEXT");

    let invalid = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "invalid.txt"}),
    )
    .await;
    assert_error_code(&invalid, "FS_NOT_TEXT");

    let mut late_binary = vec![b'a'; 70 * 1024];
    late_binary.push(0);
    workspace.write("late-binary.txt", late_binary);
    let late = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "late-binary.txt", "limit": 1}),
    )
    .await;
    assert_error_code(&late, "FS_NOT_TEXT");
}

#[tokio::test]
async fn read_line_output_and_file_size_limits_are_explicit() {
    let workspace = TempWorkspace::new("read-limits");
    workspace.write("long-line.txt", format!("{}\n", "x".repeat(2_001)));
    let truncated = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "long-line.txt"}),
    )
    .await;
    assert_eq!(truncated.error, None);
    assert!(
        truncated
            .text
            .contains("... (line truncated to 2000 chars)"),
        "{}",
        truncated.text
    );

    let high_offset_tail = (0..2_000)
        .map(|_| "z".repeat(24))
        .collect::<Vec<_>>()
        .join("\n");
    let high_offset = format!("{}{high_offset_tail}", "\n".repeat(1_000_000));
    workspace.write("high-offset.txt", high_offset);
    let bounded = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "high-offset.txt", "offset": 1_000_001, "limit": 2_000}),
    )
    .await;
    assert_eq!(bounded.error, None, "{}", bounded.text);
    assert!(bounded.text.len() <= 64 * 1024);
    assert!(bounded.text.contains("(Output capped."), "{}", bounded.text);

    let many_lines = (0..30)
        .map(|_| "y".repeat(2_000))
        .collect::<Vec<_>>()
        .join("\n");
    workspace.write("many-lines.txt", many_lines);
    let capped = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "many-lines.txt"}),
    )
    .await;
    assert_eq!(capped.error, None);
    assert!(capped.text.contains("(Output capped."), "{}", capped.text);

    workspace.write("exact-size.txt", vec![b'z'; 16 * 1024 * 1024]);
    let exact = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "exact-size.txt", "limit": 1}),
    )
    .await;
    assert_eq!(exact.error, None, "{}", exact.text);

    workspace.write("over-size.txt", vec![b'z'; 16 * 1024 * 1024 + 1]);
    let over = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "over-size.txt"}),
    )
    .await;
    assert_error_code(&over, "FS_TOO_LARGE");
}

#[tokio::test]
async fn search_patterns_encoding_and_inline_limits_are_bounded() {
    let workspace = TempWorkspace::new("search-limits");
    for index in 0..=100 {
        workspace.write(format!("glob-{index:03}.rs"), b"plain\n");
    }
    let same_time = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    for index in 0..=100 {
        let file = fs::File::options()
            .write(true)
            .open(workspace.path().join(format!("glob-{index:03}.rs")))
            .unwrap();
        file.set_times(fs::FileTimes::new().set_modified(same_time))
            .unwrap();
    }
    let glob = run_tool(workspace.path(), "glob", json!({"pattern": "*.rs"})).await;
    assert_eq!(glob.error, None);
    assert!(glob.text.contains("glob-000.rs"), "{}", glob.text);
    assert!(glob.text.contains("glob-099.rs"), "{}", glob.text);
    assert!(!glob.text.contains("glob-100.rs"), "{}", glob.text);
    assert!(glob.text.contains("Showing 100 of 101"), "{}", glob.text);

    let grep_lines = (1..=251)
        .map(|index| format!("needle {index}"))
        .collect::<Vec<_>>()
        .join("\n");
    workspace.write("matches.txt", grep_lines);
    let grep = run_tool(
        workspace.path(),
        "grep",
        json!({"pattern": "needle", "path": "matches.txt"}),
    )
    .await;
    assert_eq!(grep.error, None);
    assert!(grep.text.contains("Found 251 matches"), "{}", grep.text);
    assert!(grep.text.contains("Line 250:"), "{}", grep.text);
    assert!(!grep.text.contains("Line 251:"), "{}", grep.text);
    assert!(grep.text.contains("Showing 250 of 251"), "{}", grep.text);

    workspace.write(
        "invalid-bytes.txt",
        [b'n', b'e', b'e', b'd', b'l', b'e', 0xff],
    );
    let invalid = run_tool(
        workspace.path(),
        "grep",
        json!({"pattern": "needle", "path": "invalid-bytes.txt"}),
    )
    .await;
    assert_eq!(invalid.error, None);
    assert!(
        invalid.text.contains("(line is not valid UTF-8)"),
        "{}",
        invalid.text
    );

    let direct_include = run_tool(
        workspace.path(),
        "grep",
        json!({"pattern": "needle", "path": "matches.txt", "include": "*.txt"}),
    )
    .await;
    assert_eq!(direct_include.error, None);
    assert!(direct_include.text.contains("Found 251 matches"));

    let bad_regex = run_tool(
        workspace.path(),
        "grep",
        json!({"pattern": "[", "path": "."}),
    )
    .await;
    assert_error_code(&bad_regex, "SEARCH_INVALID_PATTERN");
    let bad_glob = run_tool(workspace.path(), "glob", json!({"pattern": "["})).await;
    assert_error_code(&bad_glob, "SEARCH_INVALID_PATTERN");
}

#[tokio::test]
async fn grep_accepts_exactly_the_aggregate_byte_budget_and_rejects_one_more_byte() {
    let workspace = TempWorkspace::new("grep-aggregate");
    let eight_mib = b"x\n".repeat(4 * 1024 * 1024);
    for index in 0..4 {
        workspace.write(format!("part-{index}.txt"), &eight_mib);
    }
    workspace.write("tail-empty.txt", b"");

    let exact = run_tool(
        workspace.path(),
        "grep",
        json!({"pattern": "never-match", "path": "."}),
    )
    .await;
    assert_eq!(exact.error, None, "{}", exact.text);
    assert_eq!(exact.text, "No matches found");

    workspace.write("zz-tail-one-byte.txt", b"x");
    let over = run_tool(
        workspace.path(),
        "grep",
        json!({"pattern": "never-match", "path": "."}),
    )
    .await;
    assert_error_code(&over, "SEARCH_LIMIT_EXCEEDED");
}

#[tokio::test]
async fn parent_and_absolute_outside_paths_are_denied_without_disclosure() {
    let workspace = TempWorkspace::new("outside");
    let outside = workspace.outside_file("outside-secret.txt", b"OUTSIDE_SENTINEL\n");

    let parent = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "../outside-secret.txt"}),
    )
    .await;
    assert_error_code(&parent, "WORKSPACE_PATH_DENIED");
    assert!(!parent.text.contains("OUTSIDE_SENTINEL"));

    let absolute = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": outside.to_string_lossy()}),
    )
    .await;
    assert_error_code(&absolute, "WORKSPACE_PATH_DENIED");
    assert!(!absolute.text.contains("OUTSIDE_SENTINEL"));
    assert!(!absolute.text.contains(outside.to_string_lossy().as_ref()));
}

#[cfg(unix)]
#[tokio::test]
async fn an_external_symlink_cannot_disclose_its_target() {
    use std::os::unix::fs::symlink;

    let workspace = TempWorkspace::new("symlink");
    let outside = workspace.outside_file("symlink-secret.txt", b"SYMLINK_SENTINEL\n");
    symlink(&outside, workspace.path().join("link.txt")).unwrap();

    let result = run_tool(workspace.path(), "read", json!({"file_path": "link.txt"})).await;
    assert_error_code(&result, "WORKSPACE_PATH_DENIED");
    assert!(!result.text.contains("SYMLINK_SENTINEL"));
    assert!(!result.text.contains(outside.to_string_lossy().as_ref()));
}

#[tokio::test]
async fn list_order_and_default_truncation_are_stable() {
    let ordered = TempWorkspace::new("order");
    ordered.write("zeta.txt", b"z");
    ordered.write("alpha.txt", b"a");
    ordered.write("middle.txt", b"m");

    let first = run_tool(ordered.path(), "list", json!({"path": "."})).await;
    let second = run_tool(ordered.path(), "list", json!({"path": "."})).await;
    assert_eq!(first.error, None);
    assert_eq!(first.text, second.text);
    let alpha = first.text.find("alpha.txt").unwrap();
    let middle = first.text.find("middle.txt").unwrap();
    let zeta = first.text.find("zeta.txt").unwrap();
    assert!(alpha < middle && middle < zeta, "{}", first.text);

    let truncated = TempWorkspace::new("truncation");
    for index in 0..=500 {
        truncated.write(format!("entry-{index:03}.txt"), b"x");
    }
    let result = run_tool(truncated.path(), "list", json!({"path": "."})).await;
    assert_eq!(result.error, None);
    assert!(result.text.contains("entry-000.txt"), "{}", result.text);
    assert!(result.text.contains("entry-499.txt"), "{}", result.text);
    assert!(!result.text.contains("entry-500.txt"), "{}", result.text);
    assert!(
        result.text.contains("Showing 500 of 501"),
        "{}",
        result.text
    );
}

#[tokio::test]
async fn a_pre_cancelled_turn_never_dispatches_the_registry() {
    let workspace = TempWorkspace::new("pre-cancel");
    workspace.write("must-not-read.txt", b"PRE_CANCEL_SENTINEL\n");
    let registry = Arc::new(ReadOnlyToolRegistry::open(workspace.path()).unwrap());
    let config = AgentLoopConfig::new(LlmCallConfig::new("mock", "model").unwrap())
        .with_tools(registry.schemas().to_vec())
        .unwrap();
    let provider = Arc::new(ScriptedProvider::for_tool(
        "read",
        r#"{"file_path":"must-not-read.txt"}"#,
    ));
    let mut agent = AgentLoop::new(
        Session::new("read-only-tools-pre-cancel").unwrap(),
        provider,
        registry,
        config,
    )
    .unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user_message("do not start")]),
            cancellation,
        )
        .await
        .unwrap();

    assert!(matches!(outcome.reason(), TurnEndReason::Aborted { .. }));
    assert_eq!(outcome.steps(), 0);
    assert!(!agent.session().events().iter().any(|event| matches!(
        event.kind(),
        EventKind::ToolCall { .. } | EventKind::ToolResult { .. }
    )));
}

#[tokio::test]
async fn canonical_results_and_workspace_denials_match_the_phase4_oracle_scope() {
    let oracle: Value =
        serde_json::from_str(include_str!("fixtures/tools/upstream_phase4_oracle.json")).unwrap();
    assert_eq!(
        oracle["upstream"]["commit"],
        json!("47f943859bef60e4160492346772ded9b24f765a")
    );
    assert_eq!(
        oracle["config"]["shippedCli"]["sampleOverCapGlobResults"],
        json!(false)
    );
    assert_eq!(
        oracle["schemaSurface"]["modelFacingListPresent"],
        json!(false),
        "the Rust list tool must remain a documented product extension"
    );

    let workspace = TempWorkspace::new("oracle");
    workspace.mkdir("src");
    workspace.mkdir(".git");
    workspace.write("read.txt", b"alpha\r\nbeta\nthird\n");
    workspace.write("empty.txt", b"");
    workspace.write("src/old.ts", b"const oldValue = true\n");
    workspace.write("src/new.ts", b"needle first\r\nneutral\nneedle second\n");
    workspace.write(".hidden.ts", b"const hiddenValue = true\n");
    workspace.write("ignored.ts", b"const ignoredValue = true\n");
    workspace.write(".gitignore", b"ignored.ts\n");
    workspace.write(".git/config.ts", b"const vcsInternal = true\n");
    fs::create_dir_all(workspace.root.join("outside")).unwrap();
    fs::write(
        workspace.root.join("outside/outside.txt"),
        b"outside sentinel\n",
    )
    .unwrap();
    fs::write(
        workspace.root.join("outside/outside.ts"),
        b"outsideNeedle\n",
    )
    .unwrap();
    set_modified(workspace.path().join("src/old.ts"), 946_684_800);
    set_modified(workspace.path().join("src/new.ts"), 978_307_200);
    set_modified(workspace.path().join(".hidden.ts"), 1_009_843_200);
    set_modified(workspace.path().join("ignored.ts"), 1_041_379_200);
    set_modified(workspace.path().join(".git/config.ts"), 915_148_800);

    let cases = [
        (
            "read",
            "/canonical/read/inputs/full",
            "/canonical/read/full/content/0/text",
        ),
        (
            "read",
            "/canonical/read/inputs/window",
            "/canonical/read/window/content/0/text",
        ),
        (
            "read",
            "/canonical/read/inputs/empty",
            "/canonical/read/empty/content/0/text",
        ),
        (
            "glob",
            "/canonical/glob/inputs/matching",
            "/canonical/glob/matching/content/0/text",
        ),
        (
            "glob",
            "/canonical/glob/inputs/noMatches",
            "/canonical/glob/noMatches/content/0/text",
        ),
        (
            "grep",
            "/canonical/grep/inputs/matching",
            "/canonical/grep/matching/content/0/text",
        ),
        (
            "grep",
            "/canonical/grep/inputs/noMatches",
            "/canonical/grep/noMatches/content/0/text",
        ),
    ];
    for (tool, input_pointer, output_pointer) in cases {
        let arguments = oracle
            .pointer(input_pointer)
            .unwrap_or_else(|| panic!("missing oracle input at {input_pointer}"))
            .clone();
        let actual = run_tool(workspace.path(), tool, arguments).await;
        assert_eq!(actual.error, None, "{tool} failed: {}", actual.text);
        assert!(
            !actual.meta_present,
            "Rust Phase 4 deliberately retains only model-visible content"
        );
        let expected = oracle
            .pointer(output_pointer)
            .and_then(Value::as_str)
            .unwrap();
        let expected = expected.replace("<workspace>/", "");
        assert_eq!(actual.text, expected, "oracle mismatch for {tool}");
    }

    let missing = run_tool(
        workspace.path(),
        "read",
        json!({"file_path": "missing.txt"}),
    )
    .await;
    assert_error_code(
        &missing,
        oracle["canonical"]["read"]["missing"]["error"]["info"]["code"]
            .as_str()
            .unwrap(),
    );

    assert_eq!(
        oracle["ambientReadAcceptance"]["parentTraversal"]["outcome"],
        json!("accepted")
    );
    for (tool, input_pointer, forbidden) in [
        (
            "read",
            "/ambientReadAcceptance/parentTraversal/inputs/read",
            "outside sentinel",
        ),
        (
            "glob",
            "/ambientReadAcceptance/parentTraversal/inputs/glob",
            "outside.ts",
        ),
        (
            "grep",
            "/ambientReadAcceptance/parentTraversal/inputs/grep",
            "outsideNeedle",
        ),
    ] {
        let arguments = oracle
            .pointer(input_pointer)
            .unwrap_or_else(|| panic!("missing oracle input at {input_pointer}"))
            .clone();
        let denied = run_tool(workspace.path(), tool, arguments).await;
        assert_error_code(&denied, "WORKSPACE_PATH_DENIED");
        assert!(!denied.text.contains(forbidden));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = workspace.root.join("outside/outside.txt");
        symlink(outside, workspace.path().join("outside-link.txt")).unwrap();
        assert_eq!(
            oracle["ambientReadAcceptance"]["symlink"]["outcome"],
            json!("accepted")
        );
        let linked = run_tool(
            workspace.path(),
            "read",
            json!({"file_path": "outside-link.txt"}),
        )
        .await;
        assert_error_code(&linked, "WORKSPACE_PATH_DENIED");
        assert!(!linked.text.contains("outside sentinel"));

        workspace.mkdir("list-dir/folder");
        workspace.write("list-dir/alpha.txt", b"alpha");
        workspace.write("list-dir/zeta.txt", b"zeta");
        symlink("alpha.txt", workspace.path().join("list-dir/linked-alpha")).unwrap();
        symlink("missing", workspace.path().join("list-dir/broken-link")).unwrap();
        assert_eq!(
            oracle["listDirPrimitive"]["checks"]["followsWorkingFileSymlink"],
            json!(true)
        );
        let rust_list = run_tool(
            workspace.path(),
            "list",
            oracle["listDirPrimitive"]["input"].clone(),
        )
        .await;
        assert_eq!(rust_list.error, None, "{}", rust_list.text);
        assert!(rust_list.text.contains("symlink\tlinked-alpha@"));
        assert!(rust_list.text.contains("symlink\tbroken-link@"));
    }
}

fn set_modified(path: PathBuf, unix_seconds: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_times(
        fs::FileTimes::new()
            .set_modified(UNIX_EPOCH + std::time::Duration::from_secs(unix_seconds)),
    )
    .unwrap();
}
