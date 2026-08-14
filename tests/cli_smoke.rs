use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Output, Stdio},
    sync::mpsc::{Receiver, SyncSender, sync_channel},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::{
    ffi::OsString,
    os::unix::{ffi::OsStringExt, net::UnixStream},
};

const PHASE7_ORACLE: &str = include_str!("fixtures/cli/upstream_phase7_oracle.json");

struct OwnedScriptChild(Option<Child>);

impl OwnedScriptChild {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn child(&self) -> &Child {
        self.0.as_ref().expect("owned child should exist")
    }

    fn child_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("owned child should exist")
    }

    fn wait_with_output(mut self, timeout: Duration) -> Output {
        let mut child = self.0.take().expect("owned child should exist");
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    return child
                        .wait_with_output()
                        .expect("finished script child output should collect");
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let output = child
                        .wait_with_output()
                        .expect("timed-out script child should reap");
                    panic!(
                        "script child exceeded its bounded deadline; stderr: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                Err(error) => panic!("script child status should be readable: {error}"),
            }
        }
    }
}

impl Drop for OwnedScriptChild {
    fn drop(&mut self) {
        let Some(child) = self.0.as_mut() else {
            return;
        };
        if matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

struct StalledScriptServer {
    base_url: String,
    ready: Receiver<()>,
    worker: Option<thread::JoinHandle<(String, bool)>>,
}

impl StalledScriptServer {
    fn start(partial: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener should bind");
        listener
            .set_nonblocking(true)
            .expect("loopback listener should become nonblocking");
        let base_url = format!(
            "http://{}",
            listener
                .local_addr()
                .expect("loopback listener should have an address")
        );
        let partial = partial.to_owned();
        let (ready_sender, ready) = sync_channel(1);
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "dsh should make the stalled request"
                        );
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("loopback accept failed: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("accepted socket should become blocking");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read should be bounded");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write should be bounded");
            let request =
                String::from_utf8(read_http_request(&mut stream)).expect("request should be UTF-8");
            let declared = partial
                .len()
                .checked_add(1024 * 1024)
                .expect("test response length should fit");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(headers.as_bytes())
                .and_then(|()| stream.write_all(partial.as_bytes()))
                .and_then(|()| stream.flush())
                .expect("partial SSE response should write");
            ready_sender
                .send(())
                .expect("stalled-test receiver should remain alive");
            let closed = wait_for_client_close(&mut stream);
            (request, closed)
        });
        Self {
            base_url,
            ready,
            worker: Some(worker),
        }
    }

    fn wait_until_stalled(&self) {
        self.ready
            .recv_timeout(Duration::from_secs(5))
            .expect("server should reach its stalled response");
    }

    fn finish(mut self) -> (String, bool) {
        self.worker
            .take()
            .expect("stalled server worker should exist")
            .join()
            .expect("stalled server worker should join")
    }
}

fn run(arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dsh"));
    command
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    OwnedScriptChild::new(command.spawn().expect("the test binary should start"))
        .wait_with_output(Duration::from_secs(5))
}

fn text_sse(text: &str) -> String {
    let text = serde_json::to_string(text).expect("test text should encode");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn spawn_response_server(bodies: Vec<String>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener should bind");
    listener
        .set_nonblocking(true)
        .expect("loopback listener should become nonblocking");
    let base_url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("loopback listener should have an address")
    );
    let server = thread::spawn(move || {
        bodies
            .into_iter()
            .map(|body| {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "dsh should make every scripted request"
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("loopback accept failed: {error}"),
                    }
                };
                stream
                    .set_nonblocking(false)
                    .expect("accepted socket should become blocking");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("request read should be bounded");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("response write should be bounded");
                let request = read_http_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("loopback response should write");
                stream.flush().expect("loopback response should flush");
                String::from_utf8(request).expect("request should be UTF-8")
            })
            .collect()
    });
    (base_url, server)
}

fn spawn_http_error_server(
    status: &'static str,
    body: &'static str,
    response_count: usize,
) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback listener should bind");
    listener
        .set_nonblocking(true)
        .expect("loopback listener should become nonblocking");
    let base_url = format!(
        "http://{}",
        listener
            .local_addr()
            .expect("loopback listener should have an address")
    );
    let worker = thread::spawn(move || {
        (0..response_count)
            .map(|_| {
                let deadline = Instant::now() + Duration::from_secs(5);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(connection) => break connection,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "dsh should make every bounded error retry"
                            );
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("loopback accept failed: {error}"),
                    }
                };
                stream
                    .set_nonblocking(false)
                    .expect("accepted socket should become blocking");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("request read should be bounded");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("response write should be bounded");
                let request = read_http_request(&mut stream);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .and_then(|()| stream.flush())
                    .expect("loopback error response should write");
                String::from_utf8(request).expect("request should be UTF-8")
            })
            .collect()
    });
    (base_url, worker)
}

fn read_http_request(stream: &mut impl Read) -> Vec<u8> {
    const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;

    let mut request = Vec::new();
    let mut scratch = [0_u8; 4_096];
    loop {
        let count = stream
            .read(&mut scratch)
            .expect("request should be readable");
        assert!(count > 0, "client closed before the request completed");
        request.extend_from_slice(&scratch[..count]);
        assert!(request.len() <= MAX_REQUEST_BYTES);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("length should parse"))
            })
            .expect("request should have Content-Length");
        if request.len() >= body_start + content_length {
            request.truncate(body_start + content_length);
            return request;
        }
    }
}

fn wait_for_client_close(stream: &mut TcpStream) -> bool {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("connection monitor should be bounded");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut scratch = [0_u8; 1];
    loop {
        match stream.read(&mut scratch) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe
                        | std::io::ErrorKind::UnexpectedEof
                ) =>
            {
                return true;
            }
            Err(_) => return false,
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout should be valid UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr should be valid UTF-8")
}

fn script_workspace(label: &str) -> std::path::PathBuf {
    let workspace =
        std::env::temp_dir().join(format!("dsh-phase7-script-{}-{label}", std::process::id()));
    std::fs::create_dir_all(&workspace).expect("test workspace should be created");
    workspace
}

fn run_script(base_url: &str, workspace: &std::path::Path, prompt: &str) -> Output {
    let mut command = prompt_script_command(base_url, workspace, prompt);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    OwnedScriptChild::new(
        command
            .spawn()
            .expect("the real dsh script entry should run"),
    )
    .wait_with_output(Duration::from_secs(10))
}

fn script_command(base_url: &str, workspace: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dsh"));
    command
        .args(["--model", "deepseek-chat", "--workspace"])
        .arg(workspace)
        .arg("--no-color")
        .env_clear()
        .env("DEEPSEEK_BASE_URL", base_url)
        .env("DEEPSEEK_API_KEY", "test-key-for-loopback-only")
        .env("PATH", "/usr/bin:/bin");
    command
}

fn prompt_script_command(base_url: &str, workspace: &std::path::Path, prompt: &str) -> Command {
    let mut command = script_command(base_url, workspace);
    command.args(["--prompt", prompt]);
    command
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn send_signal(child: &Child, signal: rustix::process::Signal) {
    rustix::process::kill_process(rustix::process::Pid::from_child(child), signal)
        .expect("owned script child should accept the signal");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_until_stopped(child: &Child) {
    use rustix::process::{WaitOptions, waitpid};

    let pid = rustix::process::Pid::from_child(child);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match waitpid(Some(pid), WaitOptions::NOHANG | WaitOptions::UNTRACED) {
            Ok(Some((_, status))) if status.stopped() => return,
            Ok(Some((_, status))) => {
                panic!("script child changed state before stopping: {status:?}")
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => panic!("script child did not stop before its deadline"),
            Err(error) => panic!("script child stop status should be observable: {error}"),
        }
    }
}

fn hold_open_stdin(
    mut stdin: std::process::ChildStdin,
) -> (Receiver<()>, SyncSender<()>, thread::JoinHandle<()>) {
    let (ready_sender, ready) = sync_channel(1);
    let (release, wait_for_release) = sync_channel(0);
    let worker = thread::spawn(move || {
        let admitted = vec![b'x'; 1024 * 1024];
        stdin
            .write_all(&admitted)
            .expect("script stdin should accept its exact bounded prompt");
        stdin.flush().expect("script stdin should flush");
        ready_sender
            .send(())
            .expect("input readiness receiver should remain alive");
        let _ = wait_for_release.recv_timeout(Duration::from_secs(10));
    });
    (ready, release, worker)
}

fn tool_round_sse(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let calls = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, arguments))| {
            serde_json::json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": serde_json::to_string(arguments)
                        .expect("tool arguments should encode")
                }
            })
        })
        .collect::<Vec<_>>();
    let delta = serde_json::json!({
        "choices": [{ "delta": { "tool_calls": calls } }]
    });
    format!(
        "data: {delta}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

fn text_and_tool_round_sse(
    text: &str,
    call_id: &str,
    name: &str,
    arguments: serde_json::Value,
) -> String {
    let text_delta = serde_json::json!({
        "choices": [{ "delta": { "content": text } }]
    });
    let tool_delta = serde_json::json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(&arguments)
                            .expect("tool arguments should encode")
                    }
                }]
            }
        }]
    });
    format!(
        "data: {text_delta}\n\n\
         data: {tool_delta}\n\n\
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn wait_for_script_output(child: &OwnedScriptChild) -> thread::JoinHandle<()> {
    let output_reader = rustix::io::dup(
        child
            .child()
            .stdout
            .as_ref()
            .expect("script stdout should be piped"),
    )
    .expect("script stdout reader should duplicate");
    let (ready_sender, ready) = sync_channel(1);
    let reader = thread::spawn(move || {
        let mut reader = File::from(output_reader);
        let mut byte = [0_u8; 1];
        reader
            .read_exact(&mut byte)
            .expect("script should start writing its final output");
        ready_sender
            .send(())
            .expect("output readiness receiver should remain alive");
    });
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("script should reach final output");
    reader
}

#[test]
fn help_describes_only_available_options() {
    for argument in ["--help", "-h"] {
        let output = run(&[argument]);

        assert!(output.status.success());
        assert_eq!(stderr(&output), "");
        let help = stdout(&output);
        assert!(help.contains("Usage: dsh [OPTIONS]"));
        assert!(help.contains("--prompt"));
        assert!(help.contains("--model"));
        assert!(help.contains("--workspace"));
        assert!(help.contains("--help"));
        assert!(help.contains("--version"));
        assert!(!help.contains("not implemented"));
    }
}

#[test]
fn version_comes_from_the_package_manifest() {
    for argument in ["--version", "-V"] {
        let output = run(&[argument]);

        assert!(output.status.success());
        assert_eq!(stdout(&output), "dsh 0.1.0-alpha.0\n");
        assert_eq!(stderr(&output), "");
    }
}

#[test]
fn help_version_and_usage_errors_stop_before_workspace_credentials_or_network() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("sentinel listener should bind");
    listener
        .set_nonblocking(true)
        .expect("sentinel listener should be nonblocking");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let workspace = script_workspace("startup-short-circuit");
    let missing = workspace.join("must-not-be-opened");
    let missing_text = missing
        .to_str()
        .expect("test workspace path should be Unicode");

    for (arguments, expected) in [
        (vec!["--help"], 0),
        (vec!["--version"], 0),
        (vec!["--help", "--workspace", missing_text], 2),
        (vec!["--workspace", missing_text, "--unknown"], 2),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dsh"));
        command
            .args(arguments)
            .env("DEEPSEEK_BASE_URL", &base_url)
            .env("DEEPSEEK_API_KEY", "startup-sentinel-secret")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = OwnedScriptChild::new(command.spawn().expect("dsh should spawn"))
            .wait_with_output(Duration::from_secs(5));
        assert_eq!(output.status.code(), Some(expected));
        assert!(!stdout(&output).contains("startup-sentinel-secret"));
        assert!(!stderr(&output).contains("startup-sentinel-secret"));
        assert!(!missing.exists());
        assert!(matches!(
            listener.accept(),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
    }

    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[test]
fn real_script_entry_reaches_the_agent_and_loopback_provider() {
    let (base_url, server) = spawn_response_server(vec![text_sse("hello from real dsh")]);
    let workspace = std::env::temp_dir().join(format!("dsh-phase7-script-{}", std::process::id()));
    std::fs::create_dir_all(&workspace).expect("test workspace should be created");
    let output = run_script(&base_url, &workspace, "say hello");
    let requests = server.join().expect("loopback server should join");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "hello from real dsh\n");
    assert_eq!(stderr(&output), "");
    assert_eq!(requests.len(), 1);

    let request = &requests[0];
    assert!(request.starts_with("POST /chat/completions HTTP/1.1\r\n"));
    assert!(request.contains("\"content\":\"say hello\""));
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key-for-loopback-only\r\n")
    );
    std::fs::remove_dir_all(&workspace).expect("test workspace should be removed");
}

#[test]
fn real_script_output_matches_the_committed_phase7_headless_oracle_scope() {
    let oracle: serde_json::Value = serde_json::from_str(PHASE7_ORACLE).unwrap();
    assert_eq!(oracle["schemaVersion"], 1);
    assert_eq!(
        oracle["upstream"]["commit"],
        "47f943859bef60e4160492346772ded9b24f765a"
    );

    let workspace = script_workspace("oracle");
    let (base_url, server) = spawn_response_server(vec![text_sse("final answer")]);
    let completed = run_script(&base_url, &workspace, "canonical final answer");
    let requests = server.join().expect("loopback server should join");

    assert_eq!(
        completed.status.code(),
        oracle["scenarios"]["headless"]["completed"]["code"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
    );
    assert_eq!(
        stdout(&completed),
        oracle["scenarios"]["headless"]["completed"]["stdout"]
            .as_str()
            .unwrap()
    );
    assert_eq!(
        stderr(&completed),
        oracle["scenarios"]["headless"]["completed"]["stderr"]
            .as_str()
            .unwrap()
    );
    assert_eq!(requests.len(), 1);

    std::fs::write(workspace.join("note.txt"), "oracle tool sentinel\n")
        .expect("oracle input file should be created");
    let first = text_and_tool_round_sse(
        "intermediate answer",
        "call-oracle-read",
        "read",
        serde_json::json!({ "file_path": "note.txt" }),
    );
    let (base_url, server) = spawn_response_server(vec![first, text_sse("final answer")]);
    let final_only = run_script(&base_url, &workspace, "canonical final-only answer");
    let requests = server.join().expect("loopback server should join");
    assert_eq!(
        stdout(&final_only),
        oracle["scenarios"]["headless"]["completed"]["stdout"]
            .as_str()
            .unwrap()
    );
    assert_eq!(stderr(&final_only), "");
    assert!(!stdout(&final_only).contains("intermediate answer"));
    assert_eq!(requests.len(), 2);
    assert!(requests[1].contains("intermediate answer"));
    assert!(requests[1].contains("oracle tool sentinel"));

    let max_tokens = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, server) = spawn_response_server(vec![max_tokens.to_owned()]);
    let noncompleted = run_script(&base_url, &workspace, "canonical noncompleted turn");
    let requests = server.join().expect("loopback server should join");

    assert_eq!(
        noncompleted.status.code(),
        oracle["scenarios"]["headless"]["aborted"]["code"]
            .as_i64()
            .and_then(|code| i32::try_from(code).ok())
    );
    assert_eq!(
        stdout(&noncompleted),
        oracle["scenarios"]["headless"]["aborted"]["stdout"]
            .as_str()
            .unwrap()
    );
    assert_eq!(stderr(&noncompleted), "");
    assert_eq!(requests.len(), 1);
    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[test]
fn real_script_model_failure_is_separate_and_never_prints_the_key() {
    let failed = concat!(
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"content_filter\"}]}\n\n",
        "data: [DONE]\n\n"
    );
    let (base_url, server) = spawn_response_server(vec![failed.to_owned()]);
    let workspace = script_workspace("provider-error");
    let output = run_script(&base_url, &workspace, "trigger a provider failure");
    let requests = server.join().expect("loopback server should join");
    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");

    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "\n");
    assert!(stderr(&output).contains("CONTENT_FILTER"));
    assert!(!stdout(&output).contains("test-key-for-loopback-only"));
    assert!(!stderr(&output).contains("test-key-for-loopback-only"));
    assert_eq!(requests.len(), 1);
}

#[test]
fn real_script_missing_key_and_http_failure_are_bounded_and_redacted() {
    let workspace = script_workspace("provider-boundaries");

    let listener = TcpListener::bind("127.0.0.1:0").expect("zero-connect listener should bind");
    listener
        .set_nonblocking(true)
        .expect("zero-connect listener should be nonblocking");
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let mut command = prompt_script_command(&base_url, &workspace, "missing credential");
    command
        .env_remove("DEEPSEEK_API_KEY")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = OwnedScriptChild::new(command.spawn().expect("script child should spawn"))
        .wait_with_output(Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "\n");
    assert!(!stderr(&output).contains("test-key-for-loopback-only"));
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));

    let body = r#"{"error":{"message":"upstream sentinel secret","type":"server_error"}}"#;
    // The default provider policy performs two bounded retries for SERVER.
    // Keep returning the same failure so the final durable code stays SERVER
    // instead of turning into a later connection error.
    let (base_url, server) = spawn_http_error_server("500 Internal Server Error", body, 3);
    let output = run_script(&base_url, &workspace, "provider HTTP failure");
    let requests = server.join().expect("error server should join");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stdout(&output), "\n");
    assert!(stderr(&output).contains("error [SERVER]"));
    // Provider text is bounded and control-cleaned, but it is still useful
    // diagnostic content. The actual secret boundary is the credential.
    assert!(!stderr(&output).contains("test-key-for-loopback-only"));
    assert_eq!(requests.len(), 3);
    assert!(requests[0].contains("\"content\":\"provider HTTP failure\""));

    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[test]
fn script_denies_patch_and_shell_without_any_side_effect() {
    let patch = "--- a/note.txt\n+++ b/note.txt\n@@ -1 +1 @@\n-old\n+new\n";
    let shell_sentinel =
        std::env::temp_dir().join(format!("dsh-phase7-denied-shell-{}", std::process::id()));
    let _ = std::fs::remove_file(&shell_sentinel);
    let shell_command = format!(
        "printf ran > '{}'",
        shell_sentinel
            .to_str()
            .expect("test sentinel path should be Unicode")
    );
    let first = tool_round_sse(&[
        (
            "call-patch-denied",
            "apply_patch",
            serde_json::json!({ "patch": patch }),
        ),
        (
            "call-shell-denied",
            "bash",
            serde_json::json!({
                "command": shell_command,
                "description": "this must be denied"
            }),
        ),
    ]);
    let (base_url, server) = spawn_response_server(vec![first, text_sse("denials recorded")]);
    let workspace = script_workspace("deny-tools");
    let target = workspace.join("note.txt");
    std::fs::write(&target, "old\n").expect("test file should be created");
    let output = run_script(&base_url, &workspace, "try two unsafe tools");
    let requests = server.join().expect("loopback server should join");

    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "denials recorded\n");
    assert_eq!(stderr(&output), "");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old\n");
    assert!(!shell_sentinel.exists());
    assert_eq!(requests.len(), 2);
    let request = request_json(&requests[1]);
    let messages = request["messages"]
        .as_array()
        .expect("second request should contain messages");
    for call_id in ["call-patch-denied", "call-shell-denied"] {
        assert!(
            messages.iter().any(|message| {
                message["tool_call_id"] == call_id
                    && message["content"]
                        .as_str()
                        .is_some_and(|content| content.to_ascii_lowercase().contains("policy"))
            }),
            "missing correlated policy result for {call_id}: {request:#}"
        );
    }
    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[test]
fn piped_script_input_preserves_split_and_exact_limit_prompts() {
    let workspace = script_workspace("piped-input");

    let (base_url, server) = spawn_response_server(vec![text_sse("split input accepted")]);
    let mut command = script_command(&base_url, &workspace);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    let mut stdin = child
        .child_mut()
        .stdin
        .take()
        .expect("script stdin should be piped");
    stdin
        .write_all(b"split ")
        .expect("first input should write");
    stdin.flush().expect("first input should flush");
    assert!(child.child_mut().try_wait().unwrap().is_none());
    stdin
        .write_all(b"prompt")
        .expect("second input should write");
    drop(stdin);
    let output = child.wait_with_output(Duration::from_secs(5));
    let requests = server.join().expect("loopback server should join");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "split input accepted\n");
    assert!(requests[0].contains("\"content\":\"split prompt\""));

    let exact_prompt = "x".repeat(1024 * 1024);
    let (base_url, server) = spawn_response_server(vec![text_sse("exact input accepted")]);
    let mut command = script_command(&base_url, &workspace);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    let mut stdin = child
        .child_mut()
        .stdin
        .take()
        .expect("script stdin should be piped");
    stdin
        .write_all(exact_prompt.as_bytes())
        .expect("exact bounded input should write");
    drop(stdin);
    let output = child.wait_with_output(Duration::from_secs(10));
    let requests = server.join().expect("loopback server should join");
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "exact input accepted\n");
    assert!(requests[0].contains(&format!("\"content\":\"{exact_prompt}\"")));

    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[test]
fn piped_script_input_rejects_limit_plus_one_and_invalid_utf8_before_network() {
    let workspace = script_workspace("piped-invalid");
    for (bytes, expected) in [
        (vec![b'x'; 1024 * 1024 + 1], "CLI_INPUT_TOO_LARGE"),
        (vec![0xff], "CLI_INPUT_INVALID"),
    ] {
        let mut command = script_command("http://127.0.0.1:9", &workspace);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
        let mut stdin = child
            .child_mut()
            .stdin
            .take()
            .expect("script stdin should be piped");
        let _ = stdin.write_all(&bytes);
        drop(stdin);
        let output = child.wait_with_output(Duration::from_secs(5));
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), format!("dsh: {expected}\n"));
    }
    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn blocked_piped_input_uses_stable_signal_exit_codes_and_tstp_resume() {
    use rustix::process::Signal;

    let workspace = script_workspace("input-signals");
    for (signal, expected) in [
        (Signal::INT, 130),
        (Signal::HUP, 129),
        (Signal::QUIT, 131),
        (Signal::TERM, 143),
    ] {
        let mut command = script_command("http://127.0.0.1:9", &workspace);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
        let stdin = child
            .child_mut()
            .stdin
            .take()
            .expect("script stdin should be piped");
        let (ready, release, input_worker) = hold_open_stdin(stdin);
        ready
            .recv_timeout(Duration::from_secs(5))
            .expect("input worker should prove the product reader is active");
        send_signal(child.child(), signal);
        let output = child.wait_with_output(Duration::from_secs(5));
        drop(release);
        input_worker.join().expect("input writer should join");
        assert_eq!(output.status.code(), Some(expected));
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), "");
    }

    let mut command = script_command("http://127.0.0.1:9", &workspace);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    let stdin = child
        .child_mut()
        .stdin
        .take()
        .expect("script stdin should be piped");
    let (ready, release, input_worker) = hold_open_stdin(stdin);
    ready
        .recv_timeout(Duration::from_secs(5))
        .expect("input worker should prove the product reader is active");
    send_signal(child.child(), Signal::TSTP);
    wait_until_stopped(child.child());
    send_signal(child.child(), Signal::CONT);
    let output = child.wait_with_output(Duration::from_secs(5));
    drop(release);
    input_worker.join().expect("input writer should join");
    assert_eq!(output.status.code(), Some(148));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "");

    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn active_script_turn_cleans_up_before_signal_exit() {
    use rustix::process::Signal;

    let workspace = script_workspace("active-signals");
    for (signal, expected) in [
        (Signal::INT, 130),
        (Signal::HUP, 129),
        (Signal::QUIT, 131),
        (Signal::TERM, 143),
    ] {
        let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"script-stalled\"}}]}\n\n";
        let server = StalledScriptServer::start(partial);
        let mut command = prompt_script_command(&server.base_url, &workspace, "stall this script");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
        server.wait_until_stalled();
        send_signal(child.child(), signal);
        let output = child.wait_with_output(Duration::from_secs(5));
        let (request, closed) = server.finish();
        assert_eq!(output.status.code(), Some(expected));
        assert_eq!(stdout(&output), "");
        assert_eq!(stderr(&output), "");
        assert!(closed);
        assert!(request.contains("\"content\":\"stall this script\""));
    }

    let partial = "data: {\"choices\":[{\"delta\":{\"content\":\"script-stalled\"}}]}\n\n";
    let server = StalledScriptServer::start(partial);
    let mut command = prompt_script_command(&server.base_url, &workspace, "suspend this script");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    server.wait_until_stalled();
    send_signal(child.child(), Signal::TSTP);
    wait_until_stopped(child.child());
    send_signal(child.child(), Signal::CONT);
    let output = child.wait_with_output(Duration::from_secs(5));
    let (_, closed) = server.finish();
    assert_eq!(output.status.code(), Some(148));
    assert!(closed);

    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn final_script_output_is_bounded_signal_aware_and_supports_dev_null() {
    use rustix::process::Signal;

    let workspace = script_workspace("output-boundaries");
    let large_text = "x".repeat(512 * 1024);

    let (base_url, server) = spawn_response_server(vec![text_sse(&large_text)]);
    let mut command = prompt_script_command(&base_url, &workspace, "fill the output pipe");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    let started = Instant::now();
    let output = child.wait_with_output(Duration::from_secs(7));
    let requests = server.join().expect("loopback server should join");
    assert_eq!(output.status.code(), Some(1));
    assert!(started.elapsed() >= Duration::from_millis(4_500));
    assert!(started.elapsed() < Duration::from_secs(7));
    assert!(!output.stdout.is_empty());
    assert_eq!(stderr(&output), "");
    assert_eq!(requests.len(), 1);

    for (signal, expected) in [
        (Signal::INT, 130),
        (Signal::HUP, 129),
        (Signal::QUIT, 131),
        (Signal::TERM, 143),
    ] {
        let (base_url, server) = spawn_response_server(vec![text_sse(&large_text)]);
        let mut command = prompt_script_command(&base_url, &workspace, "signal blocked output");
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
        let reader = wait_for_script_output(&child);
        send_signal(child.child(), signal);
        let output = child.wait_with_output(Duration::from_secs(5));
        reader.join().expect("output readiness reader should join");
        let requests = server.join().expect("loopback server should join");
        assert_eq!(output.status.code(), Some(expected));
        assert_eq!(stderr(&output), "");
        assert_eq!(requests.len(), 1);
    }

    let (base_url, server) = spawn_response_server(vec![text_sse(&large_text)]);
    let mut command = prompt_script_command(&base_url, &workspace, "suspend blocked output");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    let reader = wait_for_script_output(&child);
    send_signal(child.child(), Signal::TSTP);
    wait_until_stopped(child.child());
    send_signal(child.child(), Signal::CONT);
    let output = child.wait_with_output(Duration::from_secs(5));
    reader.join().expect("output readiness reader should join");
    assert_eq!(output.status.code(), Some(148));
    assert_eq!(stderr(&output), "");
    assert_eq!(server.join().expect("loopback server should join").len(), 1);

    let (base_url, server) = spawn_response_server(vec![text_sse("discarded output")]);
    let mut command = prompt_script_command(&base_url, &workspace, "write to dev null");
    command
        .stdout(Stdio::from(
            File::options()
                .write(true)
                .open("/dev/null")
                .expect("/dev/null should open for stdout"),
        ))
        .stderr(Stdio::from(
            File::options()
                .write(true)
                .open("/dev/null")
                .expect("/dev/null should open for stderr"),
        ));
    let child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    let output = child.wait_with_output(Duration::from_secs(5));
    assert!(output.status.success());
    assert_eq!(server.join().expect("loopback server should join").len(), 1);

    let (base_url, server) = spawn_response_server(vec![text_sse("flags unchanged")]);
    let (mut pipe_reader, pipe_writer) = UnixStream::pair().expect("test pipe should open");
    let retained_writer = rustix::io::dup(&pipe_writer).expect("writer should duplicate");
    let flags_before = rustix::fs::fcntl_getfl(&retained_writer)
        .expect("inherited writer flags should be readable");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe_reader
            .read_to_end(&mut bytes)
            .expect("script pipe should drain");
        bytes
    });
    let mut command = prompt_script_command(&base_url, &workspace, "preserve output flags");
    let writer_fd: std::os::fd::OwnedFd = pipe_writer.into();
    command.stdout(Stdio::from(writer_fd)).stderr(Stdio::from(
        File::options()
            .write(true)
            .open("/dev/null")
            .expect("/dev/null should open for stderr"),
    ));
    let child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    drop(command);
    let output = child.wait_with_output(Duration::from_secs(5));
    let flags_after = rustix::fs::fcntl_getfl(&retained_writer)
        .expect("inherited writer flags should remain readable");
    // `std::process::Command` may add platform-private close-on-fork bits while
    // wiring a child fd. The product invariant is that dsh never changes the
    // shared status flags that could alter its parent or sibling process.
    let mutable_status = rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::APPEND;
    assert_eq!(flags_after & mutable_status, flags_before & mutable_status);
    drop(retained_writer);
    assert_eq!(
        reader.join().expect("script pipe reader should join"),
        b"flags unchanged\n"
    );
    assert!(output.status.success());
    assert_eq!(server.join().expect("loopback server should join").len(), 1);

    let (base_url, server) = spawn_response_server(vec![text_sse("broken output")]);
    let mut command = prompt_script_command(&base_url, &workspace, "close output reader");
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = OwnedScriptChild::new(command.spawn().expect("script child should spawn"));
    drop(child.child_mut().stdout.take());
    let output = child.wait_with_output(Duration::from_secs(5));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(stderr(&output), "");
    assert_eq!(server.join().expect("loopback server should join").len(), 1);

    std::fs::remove_dir_all(workspace).expect("test workspace should be removed");
}

#[test]
fn empty_non_terminal_input_is_a_bounded_script_usage_error() {
    let output = run(&[]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), "dsh: CLI_INPUT_INVALID\n");
}

#[test]
fn unknown_arguments_fail_and_are_reported() {
    let output = run(&["--unknown", "value"]);

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    assert_eq!(
        stderr(&output),
        "dsh: CLI_USAGE: unknown command-line option\n"
    );
    assert!(!stderr(&output).contains("--unknown"));
}

#[test]
fn upstream_profile_and_web_launcher_commands_are_intentionally_absent() {
    for arguments in [
        &["web"][..],
        &["plugin", "--profile", "code", "add", "some-package"][..],
        &["--profile", "headless", "task"][..],
        &["--profile", "web", "--dump-config"][..],
        &["--profile", "web", "--dump-default-config"][..],
        &["--profile", "web", "--patch", "extra.yml"][..],
    ] {
        let output = run(arguments);

        assert_eq!(output.status.code(), Some(2));
        assert_eq!(stdout(&output), "");
        assert!(stderr(&output).starts_with("dsh: CLI_USAGE:"));
        assert!(!stderr(&output).contains("some-package"));
    }
}

#[cfg(unix)]
#[test]
fn non_unicode_arguments_fail_without_panicking() {
    let output = Command::new(env!("CARGO_BIN_EXE_dsh"))
        .arg(OsString::from_vec(vec![0xff]))
        .output()
        .expect("the test binary should start");

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stdout(&output), "");
    let error = stderr(&output);
    assert!(error.contains("arguments must be valid Unicode"));
    assert!(!error.contains("panicked"));
}
