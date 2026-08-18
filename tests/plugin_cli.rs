#![cfg(any(target_os = "macos", target_os = "linux"))]
#[allow(dead_code)]
mod support;

use std::{
    fs::{self, OpenOptions},
    io::Write as _,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use support::{
    fake_deepseek::SequenceSseServer,
    pty::{PtyHarness, TestSessionRoot},
};

struct PluginWorkspace(PathBuf);

impl PluginWorkspace {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("dsh-plugin-cli-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).expect("plugin CLI workspace should be created");
        Self(path)
    }
}

impl Drop for PluginWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_private(path: &Path, bytes: &[u8], mode: u32) {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .expect("private fixture should be created");
    file.write_all(bytes).expect("fixture should be written");
    file.set_permissions(fs::Permissions::from_mode(mode))
        .expect("fixture permissions should be fixed");
}

fn plugin_fixture(workspace: &Path) -> PathBuf {
    let program = workspace.join("text-plugin.sh");
    write_private(
        &program,
        br#"#!/bin/sh
[ -z "${DEEPSEEK_API_KEY+x}" ] || printf 'leaked\n' > plugin-secret-leak
[ -z "${DSH_SESSION_ROOT+x}" ] || printf 'leaked\n' > plugin-secret-leak
[ -z "${HOME+x}" ] || printf 'leaked\n' > plugin-secret-leak
IFS= read -r hello || exit 2
printf '%s\n' '{"version":1,"type":"hello","plugin_id":"text-tools","tools":[{"name":"text_stats","description":"Count text safely","parameters":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false},"output":{"type":"object","properties":{"words":{"type":"integer"}},"required":["words"],"additionalProperties":false}}]}'
while IFS= read -r call; do
  case "$call" in
    *'"type":"call"'*)
      printf 'call\n' >> plugin-calls.log
      printf '%s\n' '{"version":1,"type":"result","id":1,"ok":true,"value":{"words":2}}'
      ;;
  esac
done
"#,
        0o700,
    );
    let program = fs::canonicalize(program).expect("plugin program should canonicalize");
    let config = workspace.join("plugins.json");
    let body = serde_json::json!({
        "version":1,
        "plugins":[{"id":"text-tools","program":program,"args":[]}]
    });
    write_private(
        &config,
        serde_json::to_string(&body)
            .expect("plugin config should encode")
            .as_bytes(),
        0o600,
    );
    config
}

fn tool_sse() -> String {
    let arguments = serde_json::to_string(&serde_json::json!({"text":"two words"}))
        .expect("tool arguments should encode");
    let delta = serde_json::json!({
        "choices":[{"delta":{"tool_calls":[{
            "index":0,
            "id":"call-plugin-1",
            "type":"function",
            "function":{"name":"text_stats","arguments":arguments}
        }]}}]
    });
    format!(
        "data: {delta}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn invalid_plugin_tool_sse() -> String {
    let arguments = serde_json::to_string(&serde_json::json!({"extra":true}))
        .expect("tool arguments should encode");
    let delta = serde_json::json!({
        "choices":[{"delta":{"tool_calls":[{
            "index":0,
            "id":"call-plugin-invalid",
            "type":"function",
            "function":{"name":"text_stats","arguments":arguments}
        }]}}]
    });
    format!(
        "data: {delta}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn text_sse(text: &str) -> String {
    let text = serde_json::to_string(text).expect("text should encode");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn request_json(request: &str) -> serde_json::Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request should contain a body");
    serde_json::from_str(body).expect("HTTP request body should be JSON")
}

fn request_has_tool(request: &str, name: &str) -> bool {
    request_json(request)["tools"]
        .as_array()
        .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
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
fn configured_plugin_is_model_visible_but_dispatches_only_after_terminal_approval() {
    let workspace = PluginWorkspace::new();
    let config = plugin_fixture(&workspace.0);
    let configured_program = serde_json::from_str::<serde_json::Value>(
        &fs::read_to_string(&config).expect("plugin config should be readable"),
    )
    .unwrap()["plugins"][0]["program"]
        .as_str()
        .expect("fixture program should be a string")
        .to_owned();
    let session_root = TestSessionRoot::new();
    let server = SequenceSseServer::start(vec![tool_sse(), text_sse("plugin round complete")]);
    let mut dsh = PtyHarness::spawn_color_with_plugin_config_and_session_root(
        &server.base_url,
        &workspace.0,
        &config,
        session_root.clone(),
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"count two words with the configured plugin\r");
    dsh.approval_ready();
    dsh.expect(b"not sandboxed");
    assert!(
        !workspace.0.join("plugin-calls.log").exists(),
        "the plugin must not receive a call before approval"
    );
    dsh.write(b"\x1b[A\r");
    dsh.expect(b"Allowed once");
    dsh.expect(b"Tool finished");
    dsh.expect(b"plugin round complete");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");

    assert_eq!(
        fs::read_to_string(workspace.0.join("plugin-calls.log")).unwrap(),
        "call\n"
    );
    assert!(
        !workspace.0.join("plugin-secret-leak").exists(),
        "host credentials and state paths must not enter the plugin environment"
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    let first = request_json(&requests[0]);
    let tool = first["tools"]
        .as_array()
        .and_then(|tools| {
            tools
                .iter()
                .find(|tool| tool["function"]["name"] == "text_stats")
        })
        .expect("plugin tool should be visible to the model");
    assert!(tool["function"].get("output").is_none());
    let second = request_json(&requests[1]);
    let result = second["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")
        })
        .expect("plugin result should be sent back to the model");
    assert_eq!(result["tool_call_id"], "call-plugin-1");
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|content| content.contains("words") && content.contains('2'))
    );
    assert!(!transcript.contains(config.to_string_lossy().as_ref()));

    let session_id = only_session_id(session_root.path());
    let journal_path = session_root.path().join(format!("{session_id}.jsonl"));
    let journal = fs::read_to_string(&journal_path).expect("plugin Session should be readable");
    let event_types = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|record| record["type"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    let position = |kind: &str| {
        event_types
            .iter()
            .position(|actual| actual == kind)
            .unwrap_or_else(|| panic!("missing {kind} in {event_types:?}"))
    };
    assert!(
        position("assistant/message") < position("tool/call")
            && position("tool/call") < position("approval/asked")
            && position("approval/asked") < position("approval/decided")
            && position("approval/decided") < position("tool/result"),
        "plugin events must retain the shared approval order: {event_types:?}"
    );
    let upstream: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/tools/upstream_phase5_oracle.json")).unwrap();
    let expected = upstream["approvalPipeline"]["askAllowed"]["relevantEventTypes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect::<Vec<_>>();
    let actual = event_types
        .iter()
        .map(String::as_str)
        .filter(|kind| expected.contains(kind))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    assert!(!journal.contains(config.to_string_lossy().as_ref()));
    assert!(!journal.contains(&configured_program));

    let without_server = SequenceSseServer::start(vec![text_sse("resume without plugins")]);
    let mut without = PtyHarness::spawn_resume(
        &without_server.base_url,
        &workspace.0,
        session_root.clone(),
        &session_id,
    );
    without.expect(b"dsh >");
    without.write(b"continue without the plugin config\r");
    without.expect(b"resume without plugins");
    without.expect_occurrences(b"dsh >", 2);
    assert!(without.exit_cleanly().0.success());
    let without_requests = without_server.finish();
    assert_eq!(without_requests.len(), 1);
    assert!(!request_has_tool(&without_requests[0], "text_stats"));

    let with_server = SequenceSseServer::start(vec![text_sse("resume with plugins")]);
    let mut with = PtyHarness::spawn_resume_color_with_plugin_config(
        &with_server.base_url,
        &workspace.0,
        session_root,
        &session_id,
        &config,
    );
    with.expect("❯".as_bytes());
    with.write(b"continue with the plugin config\r");
    with.expect(b"resume with plugins");
    with.expect_occurrences("❯".as_bytes(), 2);
    assert!(with.exit_cleanly().0.success());
    let with_requests = with_server.finish();
    assert_eq!(with_requests.len(), 1);
    assert!(request_has_tool(&with_requests[0], "text_stats"));
}

#[test]
fn script_mode_records_a_policy_denial_without_dispatching_the_plugin() {
    let workspace = PluginWorkspace::new();
    let config = plugin_fixture(&workspace.0);
    let server = SequenceSseServer::start(vec![tool_sse(), text_sse("script denial observed")]);
    let dsh = PtyHarness::spawn_script_with_plugin_config(
        &server.base_url,
        &workspace.0,
        &config,
        "try the configured plugin",
    );

    let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(10));
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");
    assert!(
        transcript.contains("script denial observed"),
        "{transcript}"
    );
    assert!(
        !workspace.0.join("plugin-calls.log").exists(),
        "script mode must deny plugin dispatch instead of waiting for approval"
    );
    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    let second = request_json(&requests[1]);
    let result = second["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")
        })
        .expect("policy denial should be returned to the model");
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|content| content.contains("denied by policy"))
    );
}

#[test]
fn terminal_reject_and_cancel_never_dispatch_a_plugin_call() {
    let rejected_workspace = PluginWorkspace::new();
    let rejected_config = plugin_fixture(&rejected_workspace.0);
    let rejected_server = SequenceSseServer::start(vec![
        tool_sse(),
        text_sse("plugin rejection returned to the model"),
    ]);
    let mut rejected = PtyHarness::spawn_color_with_plugin_config(
        &rejected_server.base_url,
        &rejected_workspace.0,
        &rejected_config,
    );
    rejected.expect("❯".as_bytes());
    rejected.write(b"reject the configured plugin\r");
    rejected.approval_ready();
    rejected.write(b"\r");
    rejected.expect(b"Rejected");
    rejected.expect(b"Tool failed");
    rejected.expect(b"plugin rejection returned to the model");
    rejected.expect_occurrences("❯".as_bytes(), 2);
    assert!(rejected.exit_cleanly().0.success());
    assert_eq!(rejected_server.finish().len(), 2);
    assert!(!rejected_workspace.0.join("plugin-calls.log").exists());

    let cancelled_workspace = PluginWorkspace::new();
    let cancelled_config = plugin_fixture(&cancelled_workspace.0);
    let cancelled_server = SequenceSseServer::start(vec![tool_sse()]);
    let mut cancelled = PtyHarness::spawn_color_with_plugin_config(
        &cancelled_server.base_url,
        &cancelled_workspace.0,
        &cancelled_config,
    );
    cancelled.expect("❯".as_bytes());
    cancelled.write(b"cancel the configured plugin\r");
    cancelled.approval_ready();
    cancelled.write(b"\x1b");
    cancelled.expect(b"Cancelled");
    cancelled.expect_occurrences("❯".as_bytes(), 2);
    assert!(cancelled.exit_cleanly().0.success());
    assert_eq!(cancelled_server.finish().len(), 1);
    assert!(!cancelled_workspace.0.join("plugin-calls.log").exists());
}

#[test]
fn invalid_plugin_arguments_return_a_result_without_approval_or_dispatch() {
    let workspace = PluginWorkspace::new();
    let config = plugin_fixture(&workspace.0);
    let server = SequenceSseServer::start(vec![
        invalid_plugin_tool_sse(),
        text_sse("invalid arguments observed"),
    ]);
    let mut dsh =
        PtyHarness::spawn_color_with_plugin_config(&server.base_url, &workspace.0, &config);

    dsh.expect("❯".as_bytes());
    dsh.write(b"send an invalid plugin call\r");
    dsh.expect(b"invalid arguments observed");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();
    let transcript = String::from_utf8_lossy(&transcript);
    assert!(status.success(), "{transcript}");
    assert!(!transcript.contains("approval requested"));
    assert!(!workspace.0.join("plugin-calls.log").exists());

    let requests = server.finish();
    assert_eq!(requests.len(), 2);
    let second = request_json(&requests[1]);
    let result = second["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| message["role"] == "tool")
        })
        .expect("invalid arguments should be correlated to the tool call");
    assert_eq!(result["tool_call_id"], "call-plugin-invalid");
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|content| content.contains("do not match"))
    );
}

#[test]
fn invalid_plugin_config_reports_the_plugin_id_before_session_or_network_work() {
    let workspace = PluginWorkspace::new();
    let config = workspace.0.join("broken-plugins.json");
    let missing_program = workspace.0.join("missing-plugin");
    write_private(
        &config,
        serde_json::to_string(&serde_json::json!({
            "version":1,
            "plugins":[{"id":"broken-plugin","program":missing_program,"args":[]}]
        }))
        .unwrap()
        .as_bytes(),
        0o600,
    );
    let session_root = workspace.0.join("sessions");
    let output = Command::new(env!("CARGO_BIN_EXE_dsh"))
        .args([
            "--prompt",
            "must not run",
            "--workspace",
            workspace.0.to_str().unwrap(),
            "--plugin-config",
            config.to_str().unwrap(),
            "--no-color",
        ])
        .env_clear()
        .env("DEEPSEEK_API_KEY", "test-key-for-loopback-only")
        .env("DEEPSEEK_BASE_URL", "http://127.0.0.1:1")
        .env("DSH_SESSION_ROOT", &session_root)
        .env("HOME", &workspace.0)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("invalid config should fail promptly");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("CLI_PLUGIN_CONFIG_INVALID"), "{stderr}");
    assert!(stderr.contains("broken-plugin"), "{stderr}");
    assert!(!stderr.contains(missing_program.to_string_lossy().as_ref()));
    assert!(!session_root.exists());
}
