#![cfg(any(target_os = "macos", target_os = "linux"))]

mod support;

use std::{
    io::Write,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use rustix::process::Signal;

use support::{
    fake_deepseek::{
        BacklogThenStalledSseServer, CancelThenSseServer, GatedFirstSseServer, SequenceSseServer,
        SplitSseServer, StalledSseServer,
    },
    process_state,
    pty::{DisabledTerminalMode, JobControlHarness, PtyHarness, TestSessionRoot},
};

static WORKSPACE_NUMBER: AtomicUsize = AtomicUsize::new(0);

struct TestWorkspace(std::path::PathBuf);

impl TestWorkspace {
    fn new() -> Self {
        let number = WORKSPACE_NUMBER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("dsh-phase7-pty-{}-{number}", std::process::id()));
        std::fs::create_dir(&path).expect("test workspace should be created");
        Self(path)
    }
}

fn text_sse(text: &str) -> String {
    let text = serde_json::to_string(text).expect("test text should encode");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn repeated_text_sse(delta_count: usize) -> String {
    repeated_text_sse_with_width(delta_count, 1)
}

fn repeated_text_sse_with_width(delta_count: usize, text_bytes: usize) -> String {
    let text = "x".repeat(text_bytes);
    let delta = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(&text).expect("bounded test text should encode")
    );
    let ending = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mut body = String::new();
    body.try_reserve(delta.len() * delta_count + ending.len())
        .expect("bounded test response should allocate");
    for _ in 0..delta_count {
        body.push_str(&delta);
    }
    body.push_str(ending);
    body
}

fn reasoning_sse(delta_count: usize, final_text: &str) -> String {
    let reasoning_delta =
        concat!("data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"r\"}}]}\n\n",);
    let final_text = serde_json::to_string(final_text).expect("test answer should encode");
    let ending = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{final_text}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    );
    let mut body = String::new();
    body.try_reserve(
        reasoning_delta
            .len()
            .saturating_mul(delta_count)
            .saturating_add(ending.len()),
    )
    .expect("bounded reasoning response should allocate");
    for _ in 0..delta_count {
        body.push_str(reasoning_delta);
    }
    body.push_str(&ending);
    body
}

fn text_backlog_then_read_tool_sse(delta_count: usize, text_bytes: usize) -> String {
    let text = "x".repeat(text_bytes);
    let text_delta = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
        serde_json::to_string(&text).expect("bounded test text should encode")
    );
    let tool_delta = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call-backlog-read",
                    "type": "function",
                    "function": {
                        "name": "read",
                        "arguments": "{\"file_path\":\"note.txt\"}"
                    }
                }]
            }
        }]
    });
    let mut body = String::new();
    body.try_reserve(
        text_delta
            .len()
            .saturating_mul(delta_count)
            .saturating_add(512),
    )
    .expect("bounded backlog response should allocate");
    for _ in 0..delta_count {
        body.push_str(&text_delta);
    }
    body.push_str("data: ");
    body.push_str(&tool_delta.to_string());
    body.push_str(concat!(
        "\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ));
    body
}

fn many_invalid_read_calls_sse(prefix: &str, count: usize, padding_bytes: usize) -> String {
    let mut body = String::new();
    body.try_reserve(count.saturating_mul(padding_bytes.saturating_add(256)))
        .expect("bounded tool-call response should allocate");
    let arguments = serde_json::json!({
        "file_path": "missing.txt",
        "padding": "x".repeat(padding_bytes),
    })
    .to_string();
    assert!(arguments.len() < 256 * 1_024);
    for index in 0..count {
        let delta = serde_json::json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": index,
                        "id": format!("{prefix}-{index}"),
                        "type": "function",
                        "function": { "name": "read", "arguments": arguments }
                    }]
                }
            }]
        });
        body.push_str("data: ");
        body.push_str(&delta.to_string());
        body.push_str("\n\n");
    }
    body.push_str(concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: [DONE]\n\n",
    ));
    body
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

fn wait_for_file(path: &std::path::Path, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while !path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for test marker {path:?}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn job_control_shell_command() -> &'static str {
    r#"owner=$$;
(
  trap '' HUP INT QUIT TERM
  /bin/sleep 8 & timer=$!
  printf '%s\n' "$timer" > guard.timer
  wait "$timer"
  [ -e guard.cancel ] || kill -KILL -- "-$owner"
) & guard=$!
printf '%s\n' "$guard" > approved.guard.pid
while [ ! -s guard.timer ]; do /bin/sleep 0.01; done
read -r timer < guard.timer
trap ': > cleanup-entered; while [ ! -e cleanup-release ]; do /bin/sleep 0.01; done; : > guard.cancel; kill -KILL "$guard" "$timer" 2>/dev/null; wait "$guard" 2>/dev/null; exit 0' TERM
printf '%s\n' "$owner" > approved.pid
: > shell-started
while :; do /bin/sleep 1; done"#
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn interactive_dsh_streams_before_completion_and_exits_cleanly() {
    let first = concat!("data: {\"choices\":[{\"delta\":{\"content\":\"first-marker \"}}]}\n\n",);
    let second = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"second-marker\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let mut server = SplitSseServer::start(first, second);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"stream this\r");
    dsh.expect(b"assistant | first-marker ");
    assert!(
        !dsh.snapshot()
            .windows(b"second-marker".len())
            .any(|window| window == b"second-marker")
    );

    server.release();
    dsh.expect(b"second-marker");
    dsh.expect(b"[done]");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, transcript) = dsh.exit_cleanly();
    let request = server.finish();

    assert!(status.success());
    assert!(request.contains("\"content\":\"stream this\""));
    assert!(!transcript.contains(&0x1b));
}

#[test]
fn styled_terminal_uses_product_owned_color_and_semantic_labels() {
    let server = SequenceSseServer::start(vec![text_sse("styled answer")]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn_color(&server.base_url, &workspace.0);

    dsh.expect(b"\x1b[1;36mdsh-rs\x1b[0m");
    dsh.expect("❯".as_bytes());
    dsh.write(b"show the styled interface\r");
    dsh.expect(b"\x1b[36m");
    dsh.expect(b"\x1b[1;36m\xe2\x97\x86 dsh\x1b[0m");
    dsh.expect(b"styled answer");
    dsh.expect(b"\x1b[32m\xe2\x9c\x93\x1b[0m Done");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();

    assert!(status.success());
    assert!(transcript.contains(&0x1b));
    assert!(
        !transcript
            .windows(b"assistant |".len())
            .any(|bytes| bytes == b"assistant |")
    );
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn styled_approval_selector_is_visible_safe_and_restores_the_terminal() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-styled-patch",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("styled patch finished"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn_color(&server.base_url, &workspace.0);

    dsh.expect("❯".as_bytes());
    dsh.write(b"show the styled approval selector\r");
    dsh.approval_ready();
    dsh.expect(b"\x1b[1;30;43m \xe2\x80\xba Reject \x1b[0m");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    dsh.write(b"\x1b[A");
    dsh.expect(b"\x1b[1;30;43m \xe2\x80\xba Allow once \x1b[0m");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    dsh.write(b"\r");
    dsh.expect(b"\x1b[32m\xe2\x9c\x93\x1b[0m Allowed once");
    dsh.expect(b"styled patch finished");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    assert!(transcript.contains(&0x1b));
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn interactive_help_quit_and_idle_ctrl_d_are_real_terminal_commands() {
    let server = SequenceSseServer::start(Vec::new());
    let workspace = TestWorkspace::new();
    let mut quit = PtyHarness::spawn(&server.base_url, &workspace.0);

    quit.expect(b"dsh | interactive; new session ");
    quit.expect(b"dsh > ");
    quit.write(b"/help\r");
    quit.expect(b"[commands]");
    quit.expect(b"/exit  exit dsh");
    quit.expect_occurrences(b"dsh > ", 2);
    quit.write(b"/quit\r");
    let (status, _) = quit.wait_for_exit(Duration::from_secs(5));
    assert!(status.success());
    assert!(server.finish().is_empty());

    let server = SequenceSseServer::start(Vec::new());
    let mut eof = PtyHarness::spawn(&server.base_url, &workspace.0);
    eof.expect(b"dsh > ");
    eof.write(&[0x04]);
    let (status, _) = eof.wait_for_exit(Duration::from_secs(5));
    assert!(status.success());
    assert!(server.finish().is_empty());
}

#[test]
fn unsafe_echo_and_output_terminal_modes_are_rejected_before_the_first_prompt() {
    for mode in [
        DisabledTerminalMode::EchoControls,
        DisabledTerminalMode::OutputPostprocess,
        DisabledTerminalMode::OutputNewlineMapping,
    ] {
        let server = SequenceSseServer::start(Vec::new());
        let workspace = TestWorkspace::new();
        let dsh =
            PtyHarness::spawn_with_disabled_terminal_mode(&server.base_url, &workspace.0, mode);
        let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(5));

        assert_eq!(status.code(), Some(1));
        assert!(
            transcript
                .windows(b"CLI_TERMINAL_UNSUPPORTED".len())
                .any(|window| window == b"CLI_TERMINAL_UNSUPPORTED")
        );
        assert!(!transcript.windows(6).any(|window| window == b"dsh > "));
        assert!(server.finish().is_empty());
    }
}

#[test]
fn real_canonical_terminal_bounds_records_recovers_huge_paste_and_submits_non_lf_veof() {
    let server = SequenceSseServer::start(vec![
        text_sse("exact canonical input accepted"),
        text_sse("veof input accepted"),
    ]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    let exact = "x".repeat(1_000);
    dsh.write(format!("{exact}\r").as_bytes());
    dsh.expect(b"assistant | exact canonical input accepted");
    dsh.expect_occurrences(b"dsh > ", 2);

    dsh.write(format!("{}\r", "y".repeat(1_001)).as_bytes());
    dsh.expect(b"[input exceeds 1000 bytes]");
    dsh.expect_occurrences(b"dsh > ", 3);

    dsh.write("z".repeat(8 * 1_024).as_bytes());
    // macOS deliberately stops accepting a canonical record when the kernel
    // queue fills, before dsh can observe it. The application must not submit
    // that truncated paste, and the standard Ctrl+C recovery must flush it.
    dsh.expect(&vec![b'z'; 1_023]);
    assert_eq!(
        dsh.snapshot()
            .windows(b"[working; press Ctrl+C to stop]".len())
            .filter(|window| *window == b"[working; press Ctrl+C to stop]")
            .count(),
        1
    );
    dsh.write(&[0x03]);
    dsh.expect_occurrences(b"dsh > ", 4);

    dsh.write(b"veof prompt without newline");
    dsh.write(&[0x04]);
    dsh.expect(b"assistant | veof input accepted");
    dsh.expect_occurrences(b"dsh > ", 5);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(&format!("\"content\":\"{exact}\"")));
    assert!(requests[1].contains("\"content\":\"veof prompt without newline\""));
}

#[test]
fn idle_hup_quit_and_term_use_stable_exit_codes() {
    let workspace = TestWorkspace::new();
    for (signal, expected) in [(Signal::HUP, 129), (Signal::QUIT, 131), (Signal::TERM, 143)] {
        let server = SequenceSseServer::start(Vec::new());
        let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);
        dsh.expect(b"dsh > ");
        dsh.signal(signal);
        let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(5));

        assert_eq!(status.code(), Some(expected));
        assert!(!transcript.windows(5).any(|bytes| bytes == b"CLI_"));
        assert!(server.finish().is_empty());
    }
}

#[test]
fn active_eof_cancels_the_stalled_turn_and_exits_zero_after_cleanup() {
    let partial =
        "data: {\"choices\":[{\"delta\":{\"content\":\"partial-before-eof\"}}]}\n\n".to_owned();
    let server = StalledSseServer::start(partial);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"stall until eof\r");
    dsh.expect(b"assistant | partial-before-eof");
    dsh.write(&[0x04]);
    let (status, _) = dsh.wait_for_exit(Duration::from_secs(5));
    let (request, closed) = server.finish();

    assert!(status.success());
    assert!(closed);
    assert!(request.contains("\"content\":\"stall until eof\""));
}

#[test]
fn active_hup_quit_and_term_cancel_before_their_stable_exit() {
    let workspace = TestWorkspace::new();
    for (signal, expected, marker) in [
        (Signal::HUP, 129, "partial-before-hup"),
        (Signal::QUIT, 131, "partial-before-quit"),
        (Signal::TERM, 143, "partial-before-term"),
    ] {
        let partial = format!(
            "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n",
            serde_json::to_string(marker).unwrap()
        );
        let server = StalledSseServer::start(partial);
        let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

        dsh.expect(b"dsh > ");
        dsh.write(b"stall for signal\r");
        dsh.expect(format!("assistant | {marker}").as_bytes());
        dsh.signal(signal);
        let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(5));
        let (request, closed) = server.finish();

        assert_eq!(status.code(), Some(expected));
        assert!(closed);
        assert!(request.contains("\"content\":\"stall for signal\""));
        assert!(!transcript.windows(5).any(|bytes| bytes == b"CLI_"));
    }
}

#[test]
fn a_non_reading_terminal_hits_one_output_deadline_and_exits_without_recursive_diagnostics() {
    let large_delta = "x".repeat(128 * 1_024);
    let partial =
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{large_delta}\"}}}}]}}\n\n");
    let server = StalledSseServer::start(partial);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.pause_reading();
    let started = std::time::Instant::now();
    dsh.write(b"fill the terminal output queue\r");
    let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(7));
    let elapsed = started.elapsed();
    let (request, closed) = server.finish();

    assert_eq!(status.code(), Some(1));
    assert!(
        closed,
        "output failure must cancel and close the provider stream"
    );
    assert!(request.contains("fill the terminal output queue"));
    assert!(
        elapsed >= Duration::from_millis(4_500),
        "elapsed={elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(7), "elapsed={elapsed:?}");
    assert!(
        !transcript.windows(5).any(|bytes| bytes == b"CLI_"),
        "the final error must not write recursively to the blocked terminal"
    );
}

#[test]
fn trickle_progress_cannot_restart_the_final_backlog_deadline() {
    let server = SequenceSseServer::start(vec![repeated_text_sse_with_width(3_900, 256)]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn_rolling(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.pause_reading();
    let mut trickle = dsh.duplicate_observed_reader();
    let started = Instant::now();
    dsh.write(b"build a maximum bounded final-drain backlog\r");
    let progress = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(4));
        let mut scratch = [0_u8; 512];
        trickle
            .read_with_timeout(&mut scratch, Duration::from_secs(1))
            .expect("trickle PTY read should make bounded progress")
    });

    let progressed = progress.join().expect("trickle reader should join");
    let overall_deadline = started + Duration::from_secs(9);
    let (status, transcript) =
        dsh.wait_for_exit(overall_deadline.saturating_duration_since(Instant::now()));
    let elapsed = started.elapsed();
    let requests = server.finish();

    assert_eq!(status.code(), Some(1));
    assert!(progressed > 0, "the PTY must make observable late progress");
    assert!(
        elapsed >= Duration::from_secs(4),
        "the test must reach its deliberately late progress window: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(8),
        "final drain must use one total deadline instead of restarting per frame: {elapsed:?}"
    );
    assert_eq!(requests.len(), 1);
    assert!(
        !transcript
            .windows(b"CLI_OUTPUT_FAILED".len())
            .any(|window| window == b"CLI_OUTPUT_FAILED"),
        "a broken terminal must not trigger a second blocking diagnostic"
    );
}

#[test]
fn maximum_backlog_signals_discard_without_per_event_output_deadlines() {
    for signal in [Signal::INT, Signal::TSTP, Signal::TERM] {
        let server =
            BacklogThenStalledSseServer::start(text_backlog_then_read_tool_sse(3_800, 256));
        let workspace = TestWorkspace::new();
        std::fs::write(workspace.0.join("note.txt"), "backlog tool input\n")
            .expect("read fixture should be created");
        let mut dsh = PtyHarness::spawn_rolling(&server.base_url, &workspace.0);

        dsh.expect(b"dsh > ");
        dsh.pause_reading();
        dsh.write(b"build a bounded backlog before the signal\r");
        server.wait_until_second_request();
        let signalled = Instant::now();
        dsh.signal(signal);
        dsh.resume_reading();

        match signal {
            Signal::INT => {
                dsh.expect(b"stopped; skipped");
                dsh.expect_occurrences(b"dsh > ", 2);
                assert!(signalled.elapsed() < Duration::from_secs(3));
                let transcript = dsh.snapshot();
                let transcript_text = String::from_utf8_lossy(&transcript);
                let skipped_start = transcript_text
                    .rfind("stopped; skipped ")
                    .expect("stopped summary should include the skipped count")
                    + "stopped; skipped ".len();
                let skipped_end = transcript_text[skipped_start..]
                    .find(" updates")
                    .map(|offset| skipped_start + offset)
                    .expect("stopped summary should close the skipped count");
                let skipped = transcript_text[skipped_start..skipped_end]
                    .parse::<usize>()
                    .expect("stopped summary count should be numeric");
                assert!(
                    skipped >= 3_000,
                    "the test must exercise a maximum-scale committed backlog, got {skipped}"
                );
                assert_eq!(
                    transcript
                        .windows(b"stopped; skipped".len())
                        .filter(|window| *window == b"stopped; skipped")
                        .count(),
                    1
                );
                let (status, _) = dsh.exit_cleanly();
                assert!(status.success());
                let (requests, second_closed) = server.finish();
                assert_eq!(requests.len(), 2);
                assert!(second_closed);
            }
            Signal::TSTP => {
                dsh.wait_until_stopped();
                assert!(signalled.elapsed() < Duration::from_secs(3));
                assert!(
                    !dsh.snapshot()
                        .windows(b"stopped; skipped".len())
                        .any(|window| window == b"stopped; skipped")
                );
                dsh.signal(Signal::CONT);
                dsh.expect_occurrences(b"dsh > ", 2);
                let (status, _) = dsh.exit_cleanly();
                assert!(status.success());
                let (requests, second_closed) = server.finish();
                assert_eq!(requests.len(), 2);
                assert!(second_closed);
            }
            Signal::TERM => {
                let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(3));
                assert_eq!(status.code(), Some(143));
                assert!(signalled.elapsed() < Duration::from_secs(3));
                assert!(
                    !transcript
                        .windows(b"stopped; skipped".len())
                        .any(|window| window == b"stopped; skipped")
                );
                let (requests, second_closed) = server.finish();
                assert_eq!(requests.len(), 2);
                assert!(second_closed);
            }
            _ => unreachable!("the table contains only the three tested signals"),
        }
    }
}

#[test]
fn interactive_dsh_keeps_committed_history_across_two_turns() {
    let server = SequenceSseServer::start(vec![
        text_sse("first committed answer"),
        text_sse("second committed answer"),
    ]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"first prompt\r");
    dsh.expect(b"assistant | first committed answer");
    dsh.expect_occurrences(b"dsh > ", 2);
    dsh.write(b"second prompt\r");
    dsh.expect(b"assistant | second committed answer");
    dsh.expect_occurrences(b"dsh > ", 3);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("\"content\":\"first prompt\""));
    assert!(requests[1].contains("\"content\":\"first prompt\""));
    assert!(requests[1].contains("\"content\":\"first committed answer\""));
    assert!(requests[1].contains("\"content\":\"second prompt\""));
}

#[test]
fn interactive_resume_reuses_the_stored_context_and_reaches_a_new_prompt() {
    let server = SequenceSseServer::start(vec![
        text_sse("seeded answer"),
        text_sse("answer after interactive resume"),
    ]);
    let workspace = TestWorkspace::new();
    let caller_workspace = TestWorkspace::new();
    let session_root = TestSessionRoot::new();
    let seeded = Command::new(env!("CARGO_BIN_EXE_dsh"))
        .args([
            "--prompt",
            "seed this durable session",
            "--model",
            "deepseek-chat",
            "--workspace",
            workspace.0.to_str().unwrap(),
            "--no-color",
        ])
        .env_clear()
        .env("DEEPSEEK_BASE_URL", &server.base_url)
        .env("DEEPSEEK_API_KEY", "test-key-for-loopback-only")
        .env("DSH_SESSION_ROOT", session_root.path())
        .env("HOME", &workspace.0)
        .env("PATH", "/usr/bin:/bin")
        .env("TERM", "dumb")
        .env("NO_COLOR", "1")
        .output()
        .expect("seed script should run");
    assert!(
        seeded.status.success(),
        "{}",
        String::from_utf8_lossy(&seeded.stderr)
    );
    assert_eq!(seeded.stdout, b"seeded answer\n");

    let entries = std::fs::read_dir(session_root.path())
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(entries.len(), 1);
    let filename = entries[0].file_name().into_string().unwrap();
    let session_id = filename.strip_suffix(".jsonl").unwrap().to_owned();

    let mut dsh = PtyHarness::spawn_resume(
        &server.base_url,
        &caller_workspace.0,
        session_root,
        &session_id,
    );
    dsh.expect(format!("dsh | interactive; resumed session {session_id}").as_bytes());
    dsh.expect(b"dsh > ");
    dsh.write(b"continue interactively\r");
    dsh.expect(b"assistant | answer after interactive resume");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("\"content\":\"seed this durable session\""));
    assert!(requests[1].contains("\"content\":\"seeded answer\""));
    assert!(requests[1].contains("\"content\":\"continue interactively\""));
}

#[test]
fn long_reasoning_across_three_turns_continues_past_the_old_event_ceiling() {
    let server = SequenceSseServer::start(vec![
        reasoning_sse(1_400, "first long answer"),
        reasoning_sse(1_400, "second long answer"),
        reasoning_sse(1_400, "third long answer"),
        text_sse("answer after the old ceiling"),
    ]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn_rolling(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    for (index, (turn, answer)) in [
        (
            b"first long task".as_slice(),
            b"assistant | first long answer".as_slice(),
        ),
        (
            b"second long task".as_slice(),
            b"assistant | second long answer".as_slice(),
        ),
        (
            b"third long task".as_slice(),
            b"assistant | third long answer".as_slice(),
        ),
        (
            b"continue after long reasoning".as_slice(),
            b"assistant | answer after the old ceiling".as_slice(),
        ),
    ]
    .into_iter()
    .enumerate()
    {
        dsh.write(turn);
        dsh.write(b"\r");
        dsh.expect(answer);
        dsh.expect_occurrences(b"dsh > ", index + 2);
    }
    let (status, transcript) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 4);
    assert!(requests[2].contains("first long answer"));
    assert!(requests[2].contains("second long answer"));
    assert!(
        !transcript
            .windows(b"AGENT_EVENT_BUDGET".len())
            .any(|window| window == b"AGENT_EVENT_BUDGET")
    );
}

#[test]
fn durable_session_continues_after_crossing_the_old_real_event_ceiling() {
    let server = SequenceSseServer::start(vec![repeated_text_sse(3_975), repeated_text_sse(120)]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"nearly fill the session event log\r");
    dsh.expect(b"[done]");
    dsh.expect_occurrences(b"dsh > ", 2);

    dsh.write(b"reach the remaining event ceiling\r");
    dsh.expect(b"[done]");
    dsh.expect_occurrences(b"dsh > ", 3);
    let (status, transcript) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 2);
    assert_eq!(
        transcript
            .windows(b"AGENT_EVENT_BUDGET".len())
            .filter(|window| *window == b"AGENT_EVENT_BUDGET")
            .count(),
        0
    );
}

#[test]
fn durable_session_continues_after_crossing_the_old_retained_json_ceiling() {
    let server = SequenceSseServer::start(vec![
        many_invalid_read_calls_sse("large-call-a", 16, 220_000),
        many_invalid_read_calls_sse("large-call-b", 4, 220_000),
        text_sse("answer after the old retained limit"),
    ]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn_rolling(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"cross the retained session byte ceiling with hidden arguments\r");
    dsh.expect(b"assistant | answer after the old retained limit");
    dsh.expect(b"[done]");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, transcript) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 3);
    assert!(
        requests[1]
            .to_ascii_lowercase()
            .contains("x-deepseek-harness-compact: 1\r\n")
    );
    assert!(requests[2].contains("large-call-a-15"));
    assert_eq!(
        transcript
            .windows(b"AGENT_EVENT_BUDGET".len())
            .filter(|window| *window == b"AGENT_EVENT_BUDGET")
            .count(),
        0
    );
}

#[test]
fn ctrl_c_cancels_a_stalled_stream_and_the_next_turn_still_works() {
    let partial =
        concat!("data: {\"choices\":[{\"delta\":{\"content\":\"partial-before-cancel\"}}]}\n\n",)
            .to_owned();
    let server = CancelThenSseServer::start(partial, text_sse("answer-after-cancel"));
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    let initial_terminal_state = dsh.terminal_state();
    dsh.write(b"stall this turn\r");
    dsh.expect(b"assistant | partial-before-cancel");
    dsh.write(&[0x03]);
    dsh.expect(b"stopped; skipped");
    dsh.expect_occurrences(b"dsh > ", 2);
    dsh.write(b"continue after cancel\r");
    dsh.expect(b"assistant | answer-after-cancel");
    dsh.expect_occurrences(b"dsh > ", 3);
    assert_eq!(dsh.terminal_state(), initial_terminal_state);
    let (status, _) = dsh.exit_cleanly();
    let (requests, first_connection_closed) = server.finish();

    assert!(status.success());
    assert!(first_connection_closed);
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains("\"content\":\"stall this turn\""));
    assert!(requests[1].contains("\"content\":\"continue after cancel\""));
    assert!(!requests[1].contains("partial-before-cancel"));
}

#[test]
fn approval_and_suspend_resume_leave_the_real_terminal_state_unchanged() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-terminal-state",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("answer after selector suspension"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    let initial = dsh.terminal_state();
    dsh.write(b"test approval terminal state\r");
    dsh.approval_ready();
    assert_ne!(dsh.terminal_state(), initial);
    dsh.signal(Signal::TSTP);
    dsh.wait_until_stopped();
    assert_eq!(dsh.terminal_state(), initial);
    dsh.signal(Signal::CONT);
    dsh.expect_occurrences(b"dsh > ", 2);
    assert_eq!(dsh.terminal_state(), initial);

    dsh.write(b"continue after selector suspension\r");
    dsh.expect(b"assistant | answer after selector suspension");
    dsh.expect_occurrences(b"dsh > ", 3);

    let (status, _) = dsh.exit_cleanly();
    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn zero_width_terminal_uses_the_compact_selector_instead_of_failing() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-zero-width",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("zero width approval completed"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn_color(&server.base_url, &workspace.0);

    dsh.expect("❯".as_bytes());
    dsh.resize(24, 0);
    dsh.write(b"exercise a zero width terminal\r");
    dsh.approval_ready();
    dsh.expect(b"arrows \xc2\xb7 Enter confirm \xc2\xb7 Esc cancel");
    dsh.write(b"\x1b[A");
    dsh.expect(b"\x1b[1;30;43m \xe2\x80\xba Allow once \x1b[0m");
    dsh.write(b"\x1b[B");
    dsh.expect(b"\x1b[1;30;43m \xe2\x80\xba Reject \x1b[0m");
    assert!(
        !dsh.snapshot()
            .windows(b"\x1b[5A".len())
            .any(|window| window == b"\x1b[5A"),
        "a narrow or unknown terminal must append redraws instead of guessing wrapped rows"
    );
    dsh.write(b"\r");
    dsh.expect(b"zero width approval completed");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, _) = dsh.exit_cleanly();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn ctrl_z_cleans_an_approved_shell_group_before_bash_fg_resumes_dsh() {
    let server = SequenceSseServer::start(vec![tool_sse(
        "call-job-control",
        "bash",
        serde_json::json!({
            "command": job_control_shell_command(),
            "description": "exercise bounded job cleanup",
            "timeoutMs": 25_000
        }),
    )]);
    let workspace = TestWorkspace::new();
    let mut terminal = JobControlHarness::spawn(&server.base_url, &workspace.0);
    let dsh_group = terminal.start_dsh_job();

    terminal.write(b"run the job-control cleanup fixture\r");
    terminal.approval_ready();
    terminal.write(b"y\r");
    terminal.expect(b"[approval: allowed once]");
    wait_for_file(&workspace.0.join("shell-started"), Duration::from_secs(5));
    let approved_group = terminal.remember_approved_group();
    assert_eq!(terminal.foreground_group(), dsh_group);
    assert_eq!(process_state::is_stopped(dsh_group), Some(false));

    terminal.write(&[0x1a]);
    wait_for_file(&workspace.0.join("cleanup-entered"), Duration::from_secs(5));
    assert_eq!(
        process_state::is_stopped(dsh_group),
        Some(false),
        "dsh must keep supervising its tool while cleanup is still pending"
    );
    assert_eq!(terminal.foreground_group(), dsh_group);
    assert!(rustix::process::test_kill_process_group(approved_group).is_ok());
    std::fs::write(workspace.0.join("cleanup-release"), b"").expect("cleanup gate should release");

    terminal.expect_occurrences(JobControlHarness::SHELL_PROMPT, 3);
    assert_eq!(process_state::is_stopped(dsh_group), Some(true));
    assert_eq!(terminal.foreground_group(), terminal.shell_group());
    assert!(
        terminal.approved_group_is_gone(Duration::from_secs(2)),
        "approved process group must be gone before dsh suspends"
    );

    let background_checkpoint = terminal.checkpoint();
    terminal.write(b"bg %1\r");
    terminal.expect_after(background_checkpoint, JobControlHarness::SHELL_PROMPT);
    let stopped_checkpoint = terminal.checkpoint();
    terminal.write(
        b"limit=$((SECONDS+5)); while ! jobs -s %1 | /usr/bin/grep -q Stopped && [ \"$SECONDS\" -lt \"$limit\" ]; do :; done; jobs -s %1 | /usr/bin/grep -q Stopped && printf 'JC_BG_STOPPED\\n'\r",
    );
    terminal.expect_after(stopped_checkpoint, b"JC_BG_STOPPED");
    assert_eq!(process_state::is_stopped(dsh_group), Some(true));
    assert_eq!(terminal.foreground_group(), terminal.shell_group());
    assert!(
        !terminal.snapshot()[background_checkpoint..]
            .windows(b"dsh > ".len())
            .any(|window| window == b"dsh > "),
        "a background dsh must stop again without writing to the terminal"
    );

    let foreground_checkpoint = terminal.checkpoint();
    terminal.write(b"fg %1\r");
    terminal.expect_after(foreground_checkpoint, b"dsh > ");
    assert_eq!(process_state::is_stopped(dsh_group), Some(false));
    assert_eq!(terminal.foreground_group(), dsh_group);
    let exit_checkpoint = terminal.checkpoint();
    terminal.write(b"/exit\r");
    terminal.expect_after(exit_checkpoint, JobControlHarness::SHELL_PROMPT);
    let status_checkpoint = terminal.checkpoint();
    terminal.write(b"printf 'JC_DSH_STATUS:%s\\n' \"$?\"\r");
    terminal.expect_after(status_checkpoint, b"JC_DSH_STATUS:0");
    let (status, transcript) = terminal.finish_shell(Duration::from_secs(5));
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 1);
    assert!(!transcript.windows(5).any(|bytes| bytes == b"CLI_"));
}

#[test]
fn terminating_signals_override_a_pending_ctrl_z_after_shell_cleanup() {
    for (signal, expected) in [(Signal::HUP, 129), (Signal::QUIT, 131), (Signal::TERM, 143)] {
        let server = SequenceSseServer::start(vec![tool_sse(
            "call-job-control-exit",
            "bash",
            serde_json::json!({
                "command": job_control_shell_command(),
                "description": "exercise terminating signal priority",
                "timeoutMs": 25_000
            }),
        )]);
        let workspace = TestWorkspace::new();
        let mut terminal = JobControlHarness::spawn(&server.base_url, &workspace.0);
        let dsh_group = terminal.start_dsh_job();

        terminal.write(b"run the terminating-signal fixture\r");
        terminal.approval_ready();
        terminal.write(b"y\r");
        wait_for_file(&workspace.0.join("shell-started"), Duration::from_secs(5));
        terminal.remember_approved_group();
        terminal.write(&[0x1a]);
        wait_for_file(&workspace.0.join("cleanup-entered"), Duration::from_secs(5));
        assert_eq!(process_state::is_stopped(dsh_group), Some(false));

        rustix::process::kill_process(dsh_group, signal)
            .expect("owned dsh job should accept the terminating signal");
        // Keep the tool cleanup gate closed briefly so the dispatcher observes
        // the stronger exit intent before the original suspend can settle.
        std::thread::sleep(Duration::from_millis(100));
        std::fs::write(workspace.0.join("cleanup-release"), b"")
            .expect("cleanup gate should release");

        terminal.expect_occurrences(JobControlHarness::SHELL_PROMPT, 3);
        assert_ne!(process_state::is_stopped(dsh_group), Some(true));
        assert_eq!(terminal.foreground_group(), terminal.shell_group());
        assert!(terminal.approved_group_is_gone(Duration::from_secs(2)));
        terminal.write(b"printf 'JC_DSH_STATUS:%s\\n' \"$?\"\r");
        terminal.expect(format!("JC_DSH_STATUS:{expected}").as_bytes());
        let (status, transcript) = terminal.finish_shell(Duration::from_secs(5));
        let requests = server.finish();

        assert!(status.success());
        assert_eq!(requests.len(), 1);
        assert_eq!(
            transcript
                .windows(b"dsh > ".len())
                .filter(|window| *window == b"dsh > ")
                .count(),
            1,
            "a terminating signal must exit instead of resuming a suspended dsh"
        );
    }
}

#[test]
fn short_approval_answer_allows_one_patch_after_the_preview() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-patch",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("patch finished"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"change note\r");
    dsh.expect(b"[approval requested]");
    dsh.expect(b"--- a/note.txt");
    dsh.approval_ready();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "old\n",
        "the file must not change before approval"
    );
    dsh.write(b"y\r");
    dsh.expect(b"[approval: allowed once]");
    dsh.expect(b"[tool result: success]");
    dsh.expect(b"assistant | patch finished");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("call-patch"));
}

#[test]
fn arrow_selection_requires_enter_before_it_applies_a_patch() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-arrow-patch",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("arrow selection finished"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"change note with the selector\r");
    dsh.approval_ready();
    dsh.write(b"\x1b[A");
    dsh.expect(b"[x] Allow once");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    dsh.write(b"\r");
    dsh.expect(b"[approval: allowed once]");
    dsh.expect(b"assistant | arrow selection finished");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn escape_cancels_the_selector_without_applying_a_patch() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-escape-patch",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("escape cancellation recorded"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"cancel this patch from the selector\r");
    dsh.approval_ready();
    dsh.write(b"\x1b");
    dsh.expect(b"[approval: cancelled]");
    dsh.expect(b"assistant | escape cancellation recorded");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn bracketed_paste_sequence_rearms_before_a_real_selector_choice() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-pasted-patch",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("pasted input never authorized"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"reject bracketed paste authority\r");
    dsh.approval_ready();
    dsh.write(b"\x1b[200~y\r\x1b[201~");
    dsh.approval_ready_occurrence(2);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    dsh.write(b"y\r");
    dsh.expect(b"[approval: allowed once]");
    dsh.expect(b"assistant | pasted input never authorized");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    assert_eq!(server.finish().len(), 2);
}

#[test]
fn complete_and_partial_input_before_a_fresh_approval_cannot_authorize() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let tool = tool_sse(
        "call-fenced",
        "apply_patch",
        serde_json::json!({ "patch": patch }),
    );
    let split = tool
        .find("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}")
        .expect("tool fixture should contain a final response boundary");
    let mut server = GatedFirstSseServer::start(
        tool[..split].to_owned(),
        tool[split..].to_owned(),
        vec![text_sse("fenced patch finished")],
    );
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"change note safely\r");
    dsh.expect(b"[working; press Ctrl+C to stop]");
    dsh.write(b"y\rallow stale-partial");
    server.release();

    dsh.approval_ready();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    dsh.write(b"y\r");
    dsh.expect(b"[approval: allowed once]");
    dsh.expect(b"assistant | fenced patch finished");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, transcript) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    assert_eq!(requests.len(), 2);
    assert!(
        !transcript
            .windows(b"[approval answer not recognized]".len())
            .any(|bytes| bytes == b"[approval answer not recognized]")
    );
}

#[test]
fn continuous_stale_input_and_selection_without_enter_remain_fail_closed() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let tool = tool_sse(
        "call-stale-flood",
        "apply_patch",
        serde_json::json!({ "patch": patch }),
    );
    let split = tool
        .find("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}")
        .expect("tool fixture should contain a final response boundary");
    let mut server = GatedFirstSseServer::start(
        tool[..split].to_owned(),
        tool[split..].to_owned(),
        vec![text_sse("stale input never authorized the patch")],
    );
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"change note behind a fresh fence\r");
    dsh.expect(b"[working; press Ctrl+C to stop]");
    let mut stale_writer = dsh.duplicate_writer();
    let flood = std::thread::spawn(move || {
        for _ in 0..250 {
            if stale_writer
                .write_all(b"y\rallow 00000000-0000-4000-8000-000000000000\r")
                .is_err()
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    server.release();
    flood.join().expect("stale input writer should join");

    dsh.approval_ready();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    dsh.write(b"y");
    dsh.expect(b"[x] Allow once");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    dsh.write(b"\r");
    dsh.expect(b"[approval: allowed once]");
    dsh.expect(b"assistant | stale input never authorized the patch");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    assert_eq!(requests.len(), 2);
}

#[test]
fn approval_preview_output_failure_is_bounded_and_never_changes_the_file() {
    let addition = "x".repeat(48 * 1_024);
    let patch = format!("--- /dev/null\n+++ b/large-preview.txt\n@@ -0,0 +1 @@\n+{addition}\n");
    let tool = tool_sse(
        "call-preview-output",
        "apply_patch",
        serde_json::json!({ "patch": patch }),
    );
    let split = tool
        .find("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}")
        .expect("tool fixture should contain a final response boundary");
    let mut server = GatedFirstSseServer::start(
        tool[..split].to_owned(),
        tool[split..].to_owned(),
        Vec::new(),
    );
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("large-preview.txt");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"prepare a large patch preview\r");
    dsh.expect(b"[working; press Ctrl+C to stop]");
    dsh.pause_reading();
    let started = std::time::Instant::now();
    server.release();
    let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(7));
    let elapsed = started.elapsed();
    let requests = server.finish();

    assert_eq!(status.code(), Some(1));
    assert_eq!(requests.len(), 1);
    assert!(
        !target.exists(),
        "an unseen approval must never commit a patch"
    );
    assert!(
        elapsed >= Duration::from_millis(4_500),
        "elapsed={elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(7), "elapsed={elapsed:?}");
    assert!(
        !transcript
            .windows(b"[approval required]".len())
            .any(|bytes| bytes == b"[approval required]"),
        "the transcript must not claim the selector became answerable"
    );
}

#[test]
fn default_enter_rejects_a_patch_and_the_session_continues() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-reject",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("rejection recorded"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"change note\r");
    dsh.expect(b"[approval requested]");
    dsh.approval_ready();
    dsh.write(b"\r");
    dsh.expect(b"[approval: rejected]");
    dsh.expect(b"[tool result: error]");
    dsh.expect(b"assistant | rejection recorded");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("call-reject"));
    assert!(requests[1].to_ascii_lowercase().contains("rejected"));
}

#[test]
fn ctrl_c_at_patch_approval_cancels_without_a_write_and_a_later_turn_works() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-cancel",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("answer after approval cancel"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"change note\r");
    dsh.approval_ready();
    dsh.write(&[0x03]);
    dsh.expect(b"stopped; skipped");
    dsh.expect_occurrences(b"dsh > ", 2);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");

    dsh.write(b"continue safely\r");
    dsh.expect(b"assistant | answer after approval cancel");
    dsh.expect_occurrences(b"dsh > ", 3);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("\"content\":\"continue safely\""));
}

#[test]
fn ctrl_d_at_patch_approval_cancels_without_a_write_and_exits_zero() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let server = SequenceSseServer::start(vec![tool_sse(
        "call-eof",
        "apply_patch",
        serde_json::json!({ "patch": patch }),
    )]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"change note\r");
    dsh.approval_ready();
    dsh.write(&[0x04]);
    let (status, _) = dsh.wait_for_exit(Duration::from_secs(5));
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    assert_eq!(requests.len(), 1);
}

#[test]
fn terminating_signals_at_patch_approval_restore_the_terminal_without_a_write() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    for (signal, expected_exit) in [(Signal::HUP, 129), (Signal::QUIT, 131), (Signal::TERM, 143)] {
        let server = SequenceSseServer::start(vec![tool_sse(
            "call-terminating-approval",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        )]);
        let workspace = TestWorkspace::new();
        let target = workspace.0.join("note.txt");
        std::fs::write(&target, "old\n").expect("test file should be created");
        let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

        dsh.expect(b"dsh > ");
        dsh.write(b"request a patch, then terminate the terminal turn\r");
        dsh.approval_ready();
        dsh.signal(signal);
        let (status, transcript) = dsh.wait_for_exit(Duration::from_secs(5));
        let requests = server.finish();

        assert_eq!(status.code(), Some(expected_exit));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
        assert_eq!(requests.len(), 1);
        assert!(
            !transcript
                .windows(b"[approval: allowed once]".len())
                .any(|window| window == b"[approval: allowed once]")
        );
    }
}

#[test]
fn foreground_shell_runs_only_after_the_confirmed_terminal_approval() {
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-shell",
            "bash",
            serde_json::json!({
                "command": "printf shell-ok > shell-result.txt",
                "description": "create a bounded test sentinel",
                "timeoutMs": 25_000
            }),
        ),
        text_sse("shell finished"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join("shell-result.txt");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"run the safe shell command\r");
    dsh.expect(b"[tool requested]");
    dsh.expect(b"tool | bash");
    dsh.approval_ready();
    assert!(!target.exists());
    dsh.write(b"y\r");
    dsh.expect(b"[approval: allowed once]");
    dsh.expect(b"[tool result: success]");
    dsh.expect(b"assistant | shell finished");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "shell-ok");
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("call-shell"));
}

#[test]
fn read_only_tool_status_and_result_reach_the_real_terminal() {
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-read",
            "read",
            serde_json::json!({ "file_path": "note.txt" }),
        ),
        text_sse("read finished"),
    ]);
    let workspace = TestWorkspace::new();
    std::fs::write(workspace.0.join("note.txt"), "read-only sentinel\n")
        .expect("test file should be created");
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"read note\r");
    dsh.expect(b"[tool requested]");
    dsh.expect(b"tool | read");
    dsh.expect(b"[tool result: success]");
    dsh.expect(b"assistant | read finished");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, _) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("read-only sentinel"));
}

#[test]
fn model_terminal_controls_are_rendered_as_visible_plain_text() {
    let malicious = "safe\u{1b}]52;c;clipboard\u{7}\u{202e}end";
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-visible-tool",
            "unknown\u{202e}",
            serde_json::json!({}),
        ),
        text_sse(malicious),
    ]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"show unsafe text\r");
    dsh.expect(b"tool | unknown\\u{202e}");
    dsh.expect(b"assistant | safe\\u{1b}]52;c;clipboard\\u{7}\\u{202e}end");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, transcript) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert_eq!(requests.len(), 2);
    assert!(!transcript.contains(&0x1b));
    assert!(
        !transcript
            .windows(3)
            .any(|bytes| bytes == [0xe2, 0x80, 0xae])
    );
}

#[test]
fn styled_terminal_never_treats_model_controls_as_product_ansi() {
    let malicious = "safe\u{1b}]52;c;clipboard\u{7}\u{202e}end";
    let server = SequenceSseServer::start(vec![text_sse(malicious)]);
    let workspace = TestWorkspace::new();
    let mut dsh = PtyHarness::spawn_color(&server.base_url, &workspace.0);

    dsh.expect("❯".as_bytes());
    dsh.write(b"render hostile model text safely\r");
    dsh.expect(b"safe\\u{1b}]52;c;clipboard\\u{7}\\u{202e}end");
    dsh.expect_occurrences("❯".as_bytes(), 2);
    let (status, transcript) = dsh.exit_cleanly();

    assert!(status.success());
    assert!(
        transcript.contains(&0x1b),
        "product styling should be present"
    );
    assert!(
        !transcript
            .windows(b"\x1b]52".len())
            .any(|window| window == b"\x1b]52"),
        "model OSC bytes must never reach the terminal"
    );
    assert!(
        !transcript
            .windows("\u{202e}".len())
            .any(|window| window == "\u{202e}".as_bytes()),
        "model bidi controls must remain visible escapes"
    );
    assert_eq!(server.finish().len(), 1);
}

#[test]
fn approval_preview_and_path_bidi_are_visible_text_not_terminal_controls() {
    let unsafe_name = "note\u{202e}.txt";
    let unsafe_line = "safe\u{202e}end";
    let patch = format!("--- /dev/null\n+++ b/{unsafe_name}\n@@ -0,0 +1 @@\n+{unsafe_line}\n");
    let server = SequenceSseServer::start(vec![
        tool_sse(
            "call-visible-preview",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        text_sse("unsafe preview rejected"),
    ]);
    let workspace = TestWorkspace::new();
    let target = workspace.0.join(unsafe_name);
    let mut dsh = PtyHarness::spawn(&server.base_url, &workspace.0);

    dsh.expect(b"dsh > ");
    dsh.write(b"show a control-safe patch preview\r");
    dsh.approval_ready();
    let transcript = dsh.snapshot();
    assert!(
        transcript
            .windows(b"note\\u{202e}.txt".len())
            .any(|window| window == b"note\\u{202e}.txt")
    );
    assert!(
        transcript
            .windows(b"safe\\u{202e}end".len())
            .any(|window| window == b"safe\\u{202e}end")
    );
    assert!(!transcript.contains(&0x1b));
    assert!(
        !transcript
            .windows("\u{202e}".len())
            .any(|window| window == "\u{202e}".as_bytes())
    );

    dsh.write(b"n\r");
    dsh.expect(b"assistant | unsafe preview rejected");
    dsh.expect_occurrences(b"dsh > ", 2);
    let (status, transcript) = dsh.exit_cleanly();
    let requests = server.finish();

    assert!(status.success());
    assert!(!target.exists());
    assert_eq!(requests.len(), 2);
    assert!(!transcript.contains(&0x1b));
}
