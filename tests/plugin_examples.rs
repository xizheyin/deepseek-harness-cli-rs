#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
mod support;

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use rustix::{io::Errno, process::Pid};

use support::{
    fake_deepseek::SequenceSseServer,
    pty::{PtyHarness, TestSessionRoot},
};

struct ExampleWorkspace(PathBuf);

impl ExampleWorkspace {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("dsh-plugin-examples-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).expect("example workspace should be created");
        Self(path)
    }
}

impl Drop for ExampleWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn example_binary(environment: &str, name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(environment) {
        return fs::canonicalize(path).expect("configured example binary should exist");
    }
    let test_binary = std::env::current_exe().expect("test binary path should be available");
    let debug = test_binary
        .parent()
        .and_then(Path::parent)
        .expect("test binary should live under target/debug/deps");
    fs::canonicalize(debug.join("examples").join(name))
        .expect("run `cargo build --examples` before this focused acceptance test")
}

fn plugin_config(workspace: &Path) -> PathBuf {
    let body = serde_json::json!({
        "version":1,
        "plugins":[
            {
                "id":"text-tools",
                "program":example_binary("DSH_TEXT_STATS_PLUGIN", "text_stats_plugin"),
                "args":[]
            },
            {
                "id":"json-tools",
                "program":example_binary("DSH_JSON_FORMAT_PLUGIN", "json_format_plugin"),
                "args":[]
            }
        ]
    });
    let path = workspace.join("plugins.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("example config should be created");
    file.write_all(
        serde_json::to_string(&body)
            .expect("example config should encode")
            .as_bytes(),
    )
    .expect("example config should be written");
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .expect("example config should be private");
    path
}

fn fault_plugin_config(workspace: &Path, mode: &str) -> PathBuf {
    fault_plugin_config_with_marker(workspace, mode, None)
}

fn fault_plugin_config_with_marker(workspace: &Path, mode: &str, marker: Option<&Path>) -> PathBuf {
    let mut arguments = vec![mode.to_owned()];
    if let Some(marker) = marker {
        arguments.push(marker.to_string_lossy().into_owned());
    }
    let body = serde_json::json!({
        "version":1,
        "plugins":[{
            "id":"fault-tools",
            "program":example_binary("DSH_FAULT_PLUGIN", "fault_plugin"),
            "args":arguments
        }]
    });
    let path = workspace.join("fault-plugins.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .expect("fault config should be created");
    file.write_all(
        serde_json::to_string(&body)
            .expect("fault config should encode")
            .as_bytes(),
    )
    .expect("fault config should be written");
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .expect("fault config should be private");
    path
}

fn tool_sse(call_id: &str, name: &str, arguments: serde_json::Value) -> String {
    let arguments = serde_json::to_string(&arguments).expect("tool arguments should encode");
    let delta = serde_json::json!({
        "choices":[{"delta":{"tool_calls":[{
            "index":0,
            "id":call_id,
            "type":"function",
            "function":{"name":name,"arguments":arguments}
        }]}}]
    });
    format!(
        "data: {delta}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn text_sse(text: &str) -> String {
    let text = serde_json::to_string(text).expect("answer should encode");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn request_json(request: &str) -> serde_json::Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("request should contain an HTTP body");
    serde_json::from_str(body).expect("request body should be JSON")
}

fn last_tool_content(request: &str) -> String {
    let request = request_json(request);
    request["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")
        })
        .and_then(|message| message["content"].as_str())
        .expect("request should contain a tool result")
        .to_owned()
}

fn approve_once(dsh: &mut PtyHarness) {
    let selection = dsh.checkpoint();
    dsh.write(b"\x1b[A");
    dsh.expect_after(selection, b"> Allow once");
    dsh.write(b"\r");
}

fn only_session_id(root: &Path) -> String {
    let mut sessions = fs::read_dir(root)
        .expect("session root should exist")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
                .then(|| path.file_stem()?.to_str().map(str::to_owned))
                .flatten()
        })
        .collect::<Vec<_>>();
    assert_eq!(sessions.len(), 1, "expected one durable session");
    sessions.remove(0)
}

#[test]
fn installed_dsh_runs_both_real_example_plugins_through_approval_and_session_results() {
    let workspace = ExampleWorkspace::new();
    let config = plugin_config(&workspace.0);
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-text-stats",
            "text_stats",
            serde_json::json!({"text":"one two\nthree"}),
        ),
        tool_sse(
            "call-json-format",
            "json_format",
            serde_json::json!({"json":"{\"b\":2,\"a\":1}"}),
        ),
        text_sse("both example plugins completed"),
    ]);
    let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
        &server.base_url,
        &workspace.0,
        &config,
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"use both configured example plugins\r");
    dsh.approval_ready();
    approve_once(&mut dsh);
    dsh.expect(b"Plugin completed");
    dsh.approval_ready_for_call(b"call-json-format");
    approve_once(&mut dsh);
    dsh.expect_occurrences(b"Plugin completed", 2);
    dsh.expect(b"both example plugins completed");
    dsh.expect(b"Turn complete");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    assert!(status.success(), "{}", String::from_utf8_lossy(&transcript));

    let requests = server.finish();
    assert_eq!(requests.len(), 3);
    assert!(last_tool_content(&requests[1]).contains("characters"));
    let formatted = last_tool_content(&requests[2]);
    assert!(formatted.contains("\n  \"a\": 1"), "{formatted}");
    assert!(formatted.contains("\n  \"b\": 2"), "{formatted}");
    for request in &requests {
        let request = request_json(request);
        for tool in request["tools"].as_array().into_iter().flatten() {
            assert!(tool["function"].get("output").is_none());
        }
    }
}

#[test]
fn wrong_id_fault_plugin_never_becomes_a_success_and_is_reaped_on_exit() {
    let workspace = ExampleWorkspace::new();
    let config = fault_plugin_config(&workspace.0, "wrong-id");
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-fault",
            "fault_probe",
            serde_json::json!({"value":"probe"}),
        ),
        text_sse("fault result handled without replay"),
    ]);
    let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
        &server.base_url,
        &workspace.0,
        &config,
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"run the configured fault probe\r");
    dsh.approval_ready();
    approve_once(&mut dsh);
    dsh.expect(b"Outcome unknown");
    dsh.expect(b"fault result handled without replay");
    dsh.expect(b"Turn complete");
    dsh.expect(b"1 issue");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");
    assert!(!transcript.contains("TOOL_OUTCOME_UNKNOWN"), "{transcript}");
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(last_tool_content(&requests[1]).contains("no trustworthy matching result"));
    assert!(!transcript.contains(config.to_string_lossy().as_ref()));
}

#[test]
fn crash_after_dispatch_is_recorded_once_and_never_replayed_after_resume() {
    let workspace = ExampleWorkspace::new();
    let dispatch_marker = workspace.0.join("crash-dispatch.marker");
    let config =
        fault_plugin_config_with_marker(&workspace.0, "crash-after-call", Some(&dispatch_marker));
    let session_root = TestSessionRoot::new();
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-crash-after-dispatch",
            "fault_probe",
            serde_json::json!({"value":"crash after this call"}),
        ),
        text_sse("plugin crash was recorded"),
    ]);
    let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config_and_session_root(
        &server.base_url,
        &workspace.0,
        &config,
        session_root.clone(),
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"run the crashing plugin once\r");
    dsh.approval_ready();
    approve_once(&mut dsh);
    dsh.expect(b"Outcome unknown");
    dsh.expect(b"plugin crash was recorded");
    dsh.expect(b"Turn complete");
    dsh.expect(b"1 issue");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");
    assert!(!transcript.contains("TOOL_OUTCOME_UNKNOWN"), "{transcript}");
    assert_eq!(
        fs::read_to_string(&dispatch_marker).unwrap(),
        "dispatched\n"
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(last_tool_content(&requests[1]).contains("no trustworthy matching result"));

    let session_id = only_session_id(session_root.path());
    let resumed_server = SequenceSseServer::start(vec![text_sse("resume did not replay the call")]);
    let mut resumed = PtyHarness::spawn_resume_color(
        &resumed_server.base_url,
        &workspace.0,
        session_root,
        &session_id,
    );
    resumed.expect("❯".as_bytes());
    resumed.write(b"continue without the crashed plugin\r");
    resumed.expect(b"resume did not replay the call");
    resumed.expect(b"Turn complete");
    resumed.expect_occurrences("❯".as_bytes(), 2);
    assert!(resumed.exit_cleanly().0.success());
    assert_eq!(
        fs::read_to_string(&dispatch_marker).unwrap(),
        "dispatched\n"
    );
    let resumed_requests = resumed_server.finish();
    assert_eq!(resumed_requests.len(), 1);
    assert!(
        !request_json(&resumed_requests[0])["tools"]
            .as_array()
            .is_some_and(|tools| tools
                .iter()
                .any(|tool| tool["function"]["name"] == "fault_probe"))
    );
}

#[test]
fn matching_result_settles_the_current_call_before_extra_output_poisons_future_calls() {
    for mode in ["extra-output", "duplicate-result"] {
        let workspace = ExampleWorkspace::new();
        let config = fault_plugin_config(&workspace.0, mode);
        let server = SequenceSseServer::start(vec![
            tool_sse(
                &format!("call-{mode}"),
                "fault_probe",
                serde_json::json!({"value":"settle this call"}),
            ),
            tool_sse(
                &format!("call-after-{mode}"),
                "fault_probe",
                serde_json::json!({"value":"must not dispatch"}),
            ),
            text_sse("the matching plugin result stayed authoritative"),
        ]);
        let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
            &server.base_url,
            &workspace.0,
            &config,
        );

        dsh.expect("❯".as_bytes());
        dsh.write(b"run the configured extra-output probe\r");
        dsh.approval_ready();
        approve_once(&mut dsh);
        dsh.expect(b"Plugin completed");
        dsh.expect(b"Plugin failed");
        dsh.expect(b"the matching plugin result stayed authoritative");
        dsh.expect(b"Turn complete");
        dsh.expect_occurrences("❯".as_bytes(), 2);
        let (status, transcript) = dsh.exit_cleanly();
        let transcript = String::from_utf8_lossy(&transcript);
        assert!(status.success(), "{mode}: {transcript}");
        assert!(
            !transcript.contains("TOOL_OUTCOME_UNKNOWN"),
            "{mode}: {transcript}"
        );
        assert_eq!(
            transcript.matches("  call  ").count(),
            1,
            "the known-dead plugin must not ask for a second approval: {mode}: {transcript}"
        );

        let requests = server.finish();
        assert_eq!(requests.len(), 3);
        assert!(last_tool_content(&requests[1]).contains("ok"));
        assert!(last_tool_content(&requests[2]).contains("plugin is unavailable"));
    }
}

#[test]
fn cancellation_is_latched_even_when_the_fault_plugin_returns_a_matching_result() {
    let workspace = ExampleWorkspace::new();
    let config = fault_plugin_config(&workspace.0, "cancel-settle");
    let server = SequenceSseServer::start(vec![tool_sse(
        "call-cancel-fault",
        "fault_probe",
        serde_json::json!({"value":"wait for cancel"}),
    )]);
    let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
        &server.base_url,
        &workspace.0,
        &config,
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"start the cancellable fault probe\r");
    dsh.approval_ready();
    approve_once(&mut dsh);
    dsh.expect(b"Approved; awaiting result");
    dsh.write(&[0x03]);
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");
    assert!(transcript.contains("stopped"), "{transcript}");
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn matching_value_that_breaks_the_declared_output_schema_is_a_definite_error() {
    let workspace = ExampleWorkspace::new();
    let config = fault_plugin_config(&workspace.0, "invalid-output");
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-invalid-output",
            "fault_probe",
            serde_json::json!({"value":"return the wrong type"}),
        ),
        text_sse("invalid plugin output was handled"),
    ]);
    let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
        &server.base_url,
        &workspace.0,
        &config,
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"run the invalid-output probe\r");
    dsh.approval_ready();
    approve_once(&mut dsh);
    dsh.expect(b"Plugin failed");
    dsh.expect(b"invalid plugin output was handled");
    dsh.expect(b"Turn complete");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");
    assert!(transcript.contains("PLUGIN_OUTPUT_INVALID"), "{transcript}");
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    assert!(last_tool_content(&requests[1]).contains("declared output schema"));
}

#[test]
fn ignored_plugin_cancellation_is_force_cleaned_before_the_prompt_returns() {
    let workspace = ExampleWorkspace::new();
    let child_marker = workspace.0.join("fault-child.pid");
    let config =
        fault_plugin_config_with_marker(&workspace.0, "ignore-cancel", Some(&child_marker));
    let server = SequenceSseServer::start(vec![tool_sse(
        "call-ignore-cancel",
        "fault_probe",
        serde_json::json!({"value":"ignore cancel"}),
    )]);
    let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
        &server.base_url,
        &workspace.0,
        &config,
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"start the uncooperative fault probe\r");
    dsh.approval_ready();
    approve_once(&mut dsh);
    dsh.expect(b"Approved; awaiting result");
    let marker_deadline = Instant::now() + Duration::from_secs(3);
    while !child_marker.exists() {
        assert!(
            Instant::now() < marker_deadline,
            "fault child did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let child_pid = fs::read_to_string(&child_marker)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let child_pid = Pid::from_raw(child_pid).unwrap();
    dsh.write(&[0x03]);
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");
    assert!(transcript.contains("stopped"), "{transcript}");
    assert_eq!(server.finish().len(), 1);
    assert_eq!(
        rustix::process::test_kill_process(child_pid),
        Err(Errno::SRCH)
    );
}

#[test]
fn protocol_stdout_and_stderr_limits_fail_closed_without_hanging_the_cli() {
    for (index, mode) in ["stdout-flood", "stderr-flood"].into_iter().enumerate() {
        let workspace = ExampleWorkspace::new();
        let config = fault_plugin_config(&workspace.0, mode);
        let server = SequenceSseServer::start(vec![
            tool_sse(
                &format!("call-output-limit-{index}"),
                "fault_probe",
                serde_json::json!({"value":mode}),
            ),
            text_sse("bounded plugin fault handled"),
        ]);
        let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
            &server.base_url,
            &workspace.0,
            &config,
        );

        dsh.expect("❯".as_bytes());
        dsh.write(b"run the bounded output fault\r");
        dsh.approval_ready();
        approve_once(&mut dsh);
        dsh.expect(b"Outcome unknown");
        dsh.expect(b"bounded plugin fault handled");
        dsh.expect(b"Turn complete");
        dsh.expect(b"1 issue");
        dsh.expect_occurrences("❯".as_bytes(), 2);
        let (status, transcript) = dsh.exit_cleanly();
        let transcript = String::from_utf8_lossy(&transcript);
        assert!(status.success(), "{mode}: {transcript}");
        assert!(
            !transcript.contains("TOOL_OUTCOME_UNKNOWN"),
            "{mode}: {transcript}"
        );
        let requests = server.finish();
        assert_eq!(requests.len(), 2);
        assert!(
            last_tool_content(&requests[1]).contains("no trustworthy matching result"),
            "{mode}: the model-visible tool result must retain the unknown outcome"
        );
    }
}

#[test]
fn ctrl_c_during_plugin_handshake_cancels_startup_and_reaps_the_process_group() {
    let workspace = ExampleWorkspace::new();
    let child_marker = workspace.0.join("startup-child.pid");
    let config = fault_plugin_config_with_marker(&workspace.0, "stall-hello", Some(&child_marker));
    let mut dsh = PtyHarness::spawn_installed_color_with_plugin_config(
        "http://127.0.0.1:1",
        &workspace.0,
        &config,
    );
    let marker_deadline = Instant::now() + Duration::from_secs(3);
    while !child_marker.exists() {
        assert!(
            Instant::now() < marker_deadline,
            "startup fault child did not start"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let child_pid = fs::read_to_string(&child_marker)
        .unwrap()
        .trim()
        .parse::<i32>()
        .unwrap();
    let child_pid = Pid::from_raw(child_pid).unwrap();
    dsh.write(&[0x03]);
    let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(10));
    assert_eq!(
        status.code(),
        Some(130),
        "{}",
        String::from_utf8_lossy(&transcript)
    );
    assert_eq!(
        rustix::process::test_kill_process(child_pid),
        Err(Errno::SRCH)
    );
}
