use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use deepseek_harness_cli::{
    model::{
        ContentBlock, FinishReasonKind, LlmCallConfig, Message, MessageSource, StreamChunkKind,
    },
    provider::{
        ProviderRequest,
        deepseek::{
            CredentialRef, DEEPSEEK_PROVIDER, DeepSeekConfig, DeepSeekProvider, SecretValue,
            StaticCredentials,
        },
    },
};
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

const OFFLINE_FAKE_KEY: &str = "test-key-for-loopback-only";
const PROXY_CHILD_FLAG: &str = "DSH_TEST_PROXY_CHILD";
const PROXY_TARGET_URL: &str = "DSH_TEST_PROXY_TARGET_URL";

fn spawn_sse_server() -> (String, thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let base_url = format!("http://{address}");
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = read_one_http_request(&mut stream);

        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":null,\"reasoning_content\":\"\"}}]}\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello from loopback\"}}]}\r\n\r\n",
            ": keep-alive\r\n\r\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":4}}\r\n\r\n",
            "data: [DONE]\r\n\r\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
        String::from_utf8(request).unwrap()
    });
    (base_url, server)
}

fn spawn_redirect_server() -> (String, TcpListener, thread::JoinHandle<String>) {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let target_url = format!("http://{}", target.local_addr().unwrap());
    let redirect = TcpListener::bind("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", redirect.local_addr().unwrap());
    let server = thread::spawn(move || {
        let (mut stream, _) = redirect.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let request = read_one_http_request(&mut stream);
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {target_url}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.flush().unwrap();
        String::from_utf8(request).unwrap()
    });
    (base_url, target, server)
}

fn spawn_recording_sse_server(listener: TcpListener) -> thread::JoinHandle<Option<String>> {
    listener.set_nonblocking(true).unwrap();
    thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // The listener is nonblocking so the server thread can
                    // honor its accept deadline. Some Unix implementations
                    // may expose the accepted socket with the same status;
                    // restore a bounded blocking read before consuming the
                    // complete HTTP request.
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let request = read_one_http_request(&mut stream);
                    let body = concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"proxy-check\"}}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n",
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).unwrap();
                    stream.flush().unwrap();
                    return Some(String::from_utf8(request).unwrap());
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("recording server accept failed: {error}"),
            }
        }
    })
}

fn read_one_http_request(stream: &mut TcpStream) -> Vec<u8> {
    const MAX_TEST_REQUEST_BYTES: usize = 2 * 1024 * 1024;

    let mut request = Vec::new();
    let mut buffer = [0_u8; 4_096];
    loop {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "client closed before completing its HTTP request");
        request.extend_from_slice(&buffer[..read]);
        assert!(request.len() <= MAX_TEST_REQUEST_BYTES);

        let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let body_start = header_end + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        if request.len() >= body_start + content_length {
            request.truncate(body_start + content_length);
            return request;
        }
    }
}

fn request(provider: &DeepSeekProvider) -> ProviderRequest {
    let config = LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap();
    let prepared = provider.prepare_call(config).unwrap();
    let message = Message::user(
        "user-loopback",
        vec![ContentBlock::text("say hello").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap();
    ProviderRequest::new(prepared, vec![message])
        .unwrap()
        .with_session_id("offline-loopback-session")
        .unwrap()
}

#[tokio::test]
async fn real_reqwest_transport_streams_from_loopback_without_environment_credentials() {
    let (base_url, server) = spawn_sse_server();
    let reference = CredentialRef::new("OFFLINE_LOOPBACK_FAKE_KEY").unwrap();
    let config = DeepSeekConfig::new(base_url, reference.clone()).unwrap();
    let credentials = StaticCredentials::new(reference, SecretValue::new(OFFLINE_FAKE_KEY));
    let provider = DeepSeekProvider::new(config, Arc::new(credentials)).unwrap();

    let results = provider
        .stream(request(&provider), CancellationToken::new())
        .collect::<Vec<_>>()
        .await;
    let chunks = results.into_iter().collect::<Result<Vec<_>, _>>().unwrap();

    assert_eq!(chunks.len(), 5);
    assert!(matches!(
        chunks[0].kind(),
        StreamChunkKind::BlockStart { .. }
    ));
    assert!(matches!(
        chunks[1].kind(),
        StreamChunkKind::TextDelta { text, .. } if text == "hello from loopback"
    ));
    assert!(matches!(chunks[2].kind(), StreamChunkKind::BlockEnd { .. }));
    assert!(matches!(
        chunks[3].kind(),
        StreamChunkKind::Usage { usage }
            if usage.input_tokens().get() == 2 && usage.output_tokens().get() == 4
    ));
    assert!(matches!(
        chunks[4].kind(),
        StreamChunkKind::Finish { reason, .. }
            if matches!(reason.kind(), FinishReasonKind::Stop)
    ));

    let captured = server.join().unwrap();
    let lower = captured.to_ascii_lowercase();
    assert!(captured.starts_with("POST /chat/completions HTTP/1.1\r\n"));
    assert!(lower.contains("authorization: bearer test-key-for-loopback-only\r\n"));
    assert!(lower.contains("accept: text/event-stream\r\n"));
    assert!(lower.contains("x-deepseek-harness-session-id: offline-loopback-session\r\n"));
    assert!(lower.contains("user-agent: dsh-rs/"));
    assert!(captured.contains(r#""model":"deepseek-chat""#));
    assert!(captured.contains(r#""content":"say hello""#));
}

#[tokio::test]
async fn real_transport_does_not_follow_redirects_or_forward_authorization() {
    let (base_url, target, server) = spawn_redirect_server();
    let reference = CredentialRef::new("OFFLINE_REDIRECT_FAKE_KEY").unwrap();
    let config = DeepSeekConfig::new(base_url, reference.clone()).unwrap();
    let credentials = StaticCredentials::new(reference, SecretValue::new(OFFLINE_FAKE_KEY));
    let provider = DeepSeekProvider::new(config, Arc::new(credentials)).unwrap();

    let chunks = provider
        .stream(request(&provider), CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let StreamChunkKind::Finish { reason, .. } = chunks.last().unwrap().kind() else {
        panic!("redirect response must end in one failure finish");
    };
    assert!(matches!(
        reason.kind(),
        FinishReasonKind::Error { failure } if failure.code() == "HTTP_302"
    ));

    let captured = server.join().unwrap().to_ascii_lowercase();
    assert!(captured.contains("authorization: bearer test-key-for-loopback-only\r\n"));
    assert!(matches!(
        target.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn real_transport_proxy_child() {
    if std::env::var_os(PROXY_CHILD_FLAG).is_none() {
        return;
    }
    let base_url = std::env::var(PROXY_TARGET_URL).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let reference = CredentialRef::new("OFFLINE_PROXY_FAKE_KEY").unwrap();
        let config = DeepSeekConfig::new(base_url, reference.clone()).unwrap();
        let credentials = StaticCredentials::new(reference, SecretValue::new(OFFLINE_FAKE_KEY));
        let provider = DeepSeekProvider::new(config, Arc::new(credentials)).unwrap();
        let chunks = provider
            .stream(request(&provider), CancellationToken::new())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(matches!(
            chunks.last().unwrap().kind(),
            StreamChunkKind::Finish { reason, .. }
                if matches!(reason.kind(), FinishReasonKind::Stop)
        ));
    });
}

#[test]
fn real_transport_ignores_process_proxy_and_keeps_authorization_on_target() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    let target_url = format!("http://{}", target.local_addr().unwrap());
    let target_server = spawn_recording_sse_server(target);
    let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_url = format!("http://{}", proxy.local_addr().unwrap());
    let proxy_server = spawn_recording_sse_server(proxy);

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "real_transport_proxy_child", "--nocapture"])
        .env(PROXY_CHILD_FLAG, "1")
        .env(PROXY_TARGET_URL, &target_url)
        .env("http_proxy", &proxy_url)
        .env("HTTP_PROXY", &proxy_url)
        .env("https_proxy", &proxy_url)
        .env("HTTPS_PROXY", &proxy_url)
        .env("all_proxy", &proxy_url)
        .env("ALL_PROXY", &proxy_url)
        .env("no_proxy", "")
        .env("NO_PROXY", "")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let target_request = target_server
        .join()
        .unwrap()
        .expect("target was not reached");
    assert!(
        target_request
            .to_ascii_lowercase()
            .contains("authorization: bearer test-key-for-loopback-only\r\n")
    );
    assert!(
        proxy_server.join().unwrap().is_none(),
        "proxy saw the request"
    );
}
