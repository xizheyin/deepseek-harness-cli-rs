#![cfg(any(target_os = "macos", target_os = "linux"))]
// This integration binary reuses the complete PTY support module while the
// release journey deliberately exercises only a subset of its fixtures.
#[allow(dead_code)]
mod support;

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use support::{
    fake_deepseek::{CancelThenSseServer, SequenceSseServer},
    pty::{PtyHarness, TestSessionRoot, dsh_binary},
};

struct ReleaseWorkspace(PathBuf);

impl ReleaseWorkspace {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("dsh-phase9-release-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).expect("unique release workspace should be created");
        Self(path)
    }
}

impl Drop for ReleaseWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text_sse(text: &str) -> String {
    text_sse_with_usage(text, None)
}

fn text_sse_with_usage(text: &str, usage: Option<(u64, u64)>) -> String {
    let text = serde_json::to_string(text).expect("test text should encode");
    let usage = usage.map_or_else(String::new, |(prompt, completion)| {
        format!(
            "data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":{prompt},\"completion_tokens\":{completion},\"total_tokens\":{}}}}}\n\n",
            prompt + completion
        )
    });
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n\
         {usage}\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn tool_sse(call_id: &str, name: &str, arguments: serde_json::Value) -> String {
    let arguments = serde_json::to_string(&arguments).expect("tool arguments should encode");
    let delta = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }]
            }
        }]
    });
    format!(
        "data: {delta}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn request_json(request: &str) -> serde_json::Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("HTTP request should contain a body");
    serde_json::from_str(body).expect("HTTP request body should be JSON")
}

fn assert_last_tool_result(request: &str, call_id: &str, content: &str) {
    let request = request_json(request);
    let result = request["messages"]
        .as_array()
        .expect("provider request should contain messages")
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .expect("provider request should contain a tool result");
    assert_eq!(result["tool_call_id"], call_id, "{request:#}");
    assert!(
        result["content"]
            .as_str()
            .is_some_and(|value| value.contains(content)),
        "tool result should contain {content:?}: {request:#}"
    );
}

fn output_with_deadline(mut command: Command, deadline: Duration) -> Output {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("installed dsh should spawn");
    let mut stdout = child.stdout.take().expect("stdout should be piped");
    let mut stderr = child.stderr.take().expect("stderr should be piped");
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout
            .read_to_end(&mut bytes)
            .expect("installed dsh stdout should remain readable");
        bytes
    });
    let stderr_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr
            .read_to_end(&mut bytes)
            .expect("installed dsh stderr should remain readable");
        bytes
    });
    let expires = Instant::now() + deadline;
    let status = loop {
        if let Some(status) = child.try_wait().expect("installed dsh should be waitable") {
            break status;
        }
        if Instant::now() >= expires {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            panic!("installed dsh did not exit within {deadline:?}");
        }
        thread::sleep(Duration::from_millis(10));
    };
    Output {
        status,
        stdout: stdout_reader.join().expect("stdout reader should join"),
        stderr: stderr_reader.join().expect("stderr reader should join"),
    }
}

fn listed_session_id(root: &Path, workspace: &Path) -> String {
    let mut command = Command::new(dsh_binary());
    command
        .args(["--list-sessions", "--workspace"])
        .arg(workspace)
        .arg("--no-color")
        .env_clear()
        .env("DSH_SESSION_ROOT", root)
        .env("HOME", workspace)
        .env("PATH", "/usr/bin:/bin");
    let output = output_with_deadline(command, Duration::from_secs(5));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("session list should be UTF-8");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "one journey must keep one listed session");
    lines[0]
        .split_once('\t')
        .map(|(id, _)| id.to_owned())
        .expect("session list should begin with its canonical ID")
}

fn capture_snapshot(name: &str, bytes: &[u8]) {
    let Some(directory) = std::env::var_os("DSH_SCREENSHOT_DIR") else {
        return;
    };
    assert!(
        matches!(name, "approval.ansi" | "overview.ansi"),
        "snapshot name must be a fixed test asset"
    );
    let directory = PathBuf::from(directory);
    std::fs::create_dir_all(&directory).expect("snapshot directory should be created");
    std::fs::write(directory.join(name), bytes).expect("real PTY snapshot should be written");
}

#[test]
fn installed_dsh_renders_the_real_readme_scene() {
    let workspace = ReleaseWorkspace::new();
    std::fs::create_dir(workspace.0.join("src")).expect("source directory should be created");
    let target = workspace.0.join("src/message.txt");
    std::fs::write(&target, "release needle: old\n").expect("message fixture should be written");
    let patch = "--- a/src/message.txt\n+++ b/src/message.txt\n@@ -1 +1 @@\n-release needle: old\n+release needle: ready\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-readme-read",
            "read",
            serde_json::json!({ "file_path": "src/message.txt" }),
        ),
        tool_sse(
            "call-readme-patch",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("Updated the release needle and kept the project checkable."),
    ]);
    let mut dsh = PtyHarness::spawn_color_with_session_root(
        &server.base_url,
        &workspace.0,
        TestSessionRoot::new(),
    );

    dsh.expect("❯".as_bytes());
    dsh.write(b"Review src/message.txt and prepare the release update\r");
    dsh.expect(b"Completed  Read");
    dsh.approval_ready_for_call(b"call-readme-patch");
    let selection = dsh.checkpoint();
    dsh.write(b"\x1b[A");
    dsh.expect_after(selection, b"> Allow once");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "release needle: old\n"
    );
    capture_snapshot("approval.ansi", &dsh.snapshot());
    dsh.write(b"\r");
    dsh.expect(b"Approved; awaiting result");
    dsh.expect(b"Updated  src/message.txt");
    dsh.expect(b"Updated the release needle and kept the project checkable.");
    dsh.expect(b"Turn complete");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    capture_snapshot("overview.ansi", &dsh.snapshot());
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "release needle: ready\n"
    );
    assert_eq!(requests.len(), 3);
    assert!(requests[1].contains("release needle: old"));
    assert!(requests[2].contains("release needle: ready"));
}

#[test]
fn installed_dsh_completes_one_safe_resumable_compacting_journey() {
    let workspace = ReleaseWorkspace::new();
    std::fs::create_dir(workspace.0.join("src")).expect("source directory should be created");
    std::fs::write(
        workspace.0.join("src/config.rs"),
        "pub const ANSWER: u32 = 41;\n",
    )
    .expect("source fixture should be written");
    std::fs::write(
        workspace.0.join("check.sh"),
        concat!(
            "#!/bin/sh\n",
            "set -eu\n",
            "grep -q 'ANSWER: u32 = 42' src/config.rs\n",
            "printf 'run\\n' >> phase9-test-ran.txt\n",
            "printf 'release-test-ok\\n'\n",
        ),
    )
    .expect("test script should be written");

    let patch = "--- a/src/config.rs\n+++ b/src/config.rs\n@@ -1 +1 @@\n-pub const ANSWER: u32 = 41;\n+pub const ANSWER: u32 = 42;\n";
    let old_answer = format!(
        "Initial repository pass completed. OLD_PREFIX_SENTINEL {}",
        "o".repeat(8_000)
    );
    let first_server = SequenceSseServer::start(vec![
        tool_sse("call-list", "list", serde_json::json!({ "path": "src" })),
        tool_sse(
            "call-grep",
            "grep",
            serde_json::json!({ "pattern": "ANSWER", "path": "src", "include": "*.rs" }),
        ),
        tool_sse(
            "call-read",
            "read",
            serde_json::json!({ "file_path": "src/config.rs" }),
        ),
        tool_sse(
            "call-patch",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        tool_sse(
            "call-test",
            "bash",
            serde_json::json!({
                "command": "/bin/sh ./check.sh",
                "description": "run the repository acceptance check",
                "timeoutMs": 25_000
            }),
        ),
        text_sse_with_usage(&old_answer, Some((640_000, 2_000))),
    ]);
    let session_root = TestSessionRoot::new();
    let mut first = PtyHarness::spawn_color_with_session_root(
        &first_server.base_url,
        &workspace.0,
        session_root.clone(),
    );

    first.expect("❯".as_bytes());
    first.write(b"inspect the project, fix ANSWER, and run its check\r");
    for marker in [
        b"Completed  List".as_slice(),
        b"Completed  Search",
        b"Completed  Read",
    ] {
        first.expect(marker);
    }
    first.approval_ready_for_call(b"call-patch");
    let patch_selection = first.checkpoint();
    first.write(b"\x1b[A");
    first.expect_after(patch_selection, b"> Allow once");
    first.write(b"\r");
    first.expect(b"Approved; awaiting result");
    first.expect(b"Updated  src/config.rs");
    first.approval_ready_for_call(b"call-test");
    let shell_selection = first.checkpoint();
    first.write(b"\x1b[A");
    first.expect_after(shell_selection, b"> Allow once");
    first.write(b"\r");
    first.expect(b"Approved; awaiting result  Command");
    first.expect(b"Exit 0");
    first.expect(b"OLD_PREFIX_SENTINEL");
    first.expect(b"Turn complete");
    first.expect_occurrences("❯".as_bytes(), 2);
    let (status, _) = first.exit_cleanly();
    assert!(status.success());
    let first_requests = first_server.finish();
    assert_eq!(first_requests.len(), 6);
    assert_last_tool_result(&first_requests[1], "call-list", "config.rs");
    assert_last_tool_result(&first_requests[2], "call-grep", "ANSWER: u32 = 41");
    assert_last_tool_result(
        &first_requests[3],
        "call-read",
        "pub const ANSWER: u32 = 41",
    );
    assert_last_tool_result(&first_requests[4], "call-patch", "Updated workspace file");
    assert_last_tool_result(&first_requests[5], "call-test", "release-test-ok");

    assert_eq!(
        std::fs::read_to_string(workspace.0.join("src/config.rs")).unwrap(),
        "pub const ANSWER: u32 = 42;\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("phase9-test-ran.txt")).unwrap(),
        "run\n"
    );
    let session_id = listed_session_id(session_root.path(), &workspace.0);

    let recent_answer = format!("{} RECENT_TAIL_ANSWER_SENTINEL", "r".repeat(660_000));
    let cancelled =
        concat!("data: {\"choices\":[{\"delta\":{\"content\":\"partial-before-cancel\"}}]}\n\n",)
            .to_owned();
    let second_server = CancelThenSseServer::start(
        cancelled,
        text_sse_with_usage(&recent_answer, Some((650_000, 165_000))),
    );
    let caller = ReleaseWorkspace::new();
    let mut second = PtyHarness::spawn_resume_color(
        &second_server.base_url,
        &caller.0,
        session_root.clone(),
        &session_id,
    );

    second.expect("❯".as_bytes());
    second.write(b"start a cancellable follow-up\r");
    second.expect(b"partial-before-cancel");
    let cancelled_ready = second.checkpoint();
    second.write(&[0x03]);
    second.expect(b"stopped; skipped");
    second.expect_after(cancelled_ready, b"Ready");
    second.expect_occurrences("❯".as_bytes(), 2);
    second.write(b"record a large recent context\r");
    second.expect(b"RECENT_TAIL_ANSWER_SENTINEL");
    second.expect(b"Turn complete");
    second.expect_occurrences("❯".as_bytes(), 3);
    let (status, _) = second.exit_cleanly();
    let (requests, first_connection_closed) = second_server.finish();
    assert!(status.success());
    assert!(first_connection_closed);
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("record a large recent context"));
    assert!(!requests[1].contains("partial-before-cancel"));

    let target_prompt = "TARGET_PROMPT_SENTINEL continue after compaction";
    let summary = "SUMMARY_CHECKPOINT_SENTINEL preserve the repository decision and test result";
    let third_server = SequenceSseServer::start(vec![
        text_sse_with_usage(summary, Some((1_000, 50))),
        text_sse("continued after the automatic summary"),
    ]);
    let mut third = PtyHarness::spawn_resume_color(
        &third_server.base_url,
        &caller.0,
        session_root.clone(),
        &session_id,
    );

    third.expect("❯".as_bytes());
    third.write(format!("{target_prompt}\r").as_bytes());
    third.expect(b"continued after the automatic summary");
    third.expect(b"Turn complete");
    third.expect_occurrences("❯".as_bytes(), 2);
    let (status, _) = third.exit_cleanly();
    let requests = third_server.finish();
    assert!(status.success());
    assert_eq!(requests.len(), 2);
    assert!(
        requests[0]
            .to_ascii_lowercase()
            .contains("x-deepseek-harness-compact: 1\r\n")
    );
    assert!(requests[0].contains("OLD_PREFIX_SENTINEL"));
    assert!(!requests[0].contains(target_prompt));
    assert!(requests[1].contains("SUMMARY_CHECKPOINT_SENTINEL"));
    assert!(requests[1].contains("RECENT_TAIL_ANSWER_SENTINEL"));
    assert!(requests[1].contains(target_prompt));
    assert!(!requests[1].contains("OLD_PREFIX_SENTINEL"));

    assert_eq!(
        std::fs::read_to_string(workspace.0.join("src/config.rs")).unwrap(),
        "pub const ANSWER: u32 = 42;\n"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.0.join("phase9-test-ran.txt")).unwrap(),
        "run\n",
        "resume and compaction must not replay old tool side effects"
    );

    let journal_path = session_root.path().join(format!("{session_id}.jsonl"));
    let journal = std::fs::read_to_string(journal_path).expect("journal should remain readable");
    let rows = journal
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    let start = rows
        .iter()
        .position(|row| row["type"] == "compaction/start")
        .expect("one automatic compaction should start");
    assert_eq!(
        rows[start..start + 4]
            .iter()
            .map(|row| row["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "compaction/start",
            "compaction/summary",
            "user/message",
            "compaction/end",
        ]
    );
    assert!(rows[start + 3]["data"]["error"].is_null());
    assert_eq!(
        rows.iter()
            .filter(|row| row["type"] == "compaction/start")
            .count(),
        1
    );
}
