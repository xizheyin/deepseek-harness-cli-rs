use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::mpsc::{Receiver, SyncSender, sync_channel},
    thread,
    time::{Duration, Instant},
};

// Match the production provider request ceiling so a valid large-session
// second request is not rejected by the offline fixture server first.
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

pub struct SplitSseServer {
    pub base_url: String,
    release: Option<SyncSender<()>>,
    worker: Option<thread::JoinHandle<Option<String>>>,
}

pub struct SequenceSseServer {
    pub base_url: String,
    worker: Option<thread::JoinHandle<Vec<String>>>,
}

pub struct CancelThenSseServer {
    pub base_url: String,
    worker: Option<thread::JoinHandle<(Vec<String>, bool)>>,
}

pub struct GatedFirstSseServer {
    pub base_url: String,
    release: Option<SyncSender<()>>,
    worker: Option<thread::JoinHandle<Vec<String>>>,
}

pub struct StalledSseServer {
    pub base_url: String,
    worker: Option<thread::JoinHandle<(String, bool)>>,
}

pub struct BacklogThenStalledSseServer {
    pub base_url: String,
    second_request_ready: Receiver<()>,
    worker: Option<thread::JoinHandle<(Vec<String>, bool)>>,
}

impl SplitSseServer {
    pub fn start(first: &'static str, second: &'static str) -> Self {
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
        let (release, wait_for_release) = sync_channel(0);
        let worker = thread::spawn(move || {
            let (mut stream, _) = accept_with_deadline(&listener)?;
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read should be bounded");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write should be bounded");
            let request = read_http_request(&mut stream);
            let body_bytes = first
                .len()
                .checked_add(second.len())
                .expect("test body length should fit");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {body_bytes}\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(headers.as_bytes())
                .and_then(|()| stream.write_all(first.as_bytes()))
                .and_then(|()| stream.flush())
                .expect("first SSE fragment should write");
            let _ = wait_for_release.recv_timeout(Duration::from_secs(10));
            stream
                .write_all(second.as_bytes())
                .and_then(|()| stream.flush())
                .expect("remaining SSE fragments should write");
            Some(String::from_utf8(request).expect("request should be UTF-8"))
        });
        Self {
            base_url,
            release: Some(release),
            worker: Some(worker),
        }
    }

    pub fn release(&mut self) {
        if let Some(release) = self.release.take() {
            release.send(()).expect("server should still be waiting");
        }
    }

    pub fn finish(mut self) -> String {
        drop(self.release.take());
        self.worker
            .take()
            .expect("server worker should exist")
            .join()
            .expect("server worker should join")
            .expect("dsh should reach the loopback server")
    }
}

impl SequenceSseServer {
    pub fn start(bodies: Vec<String>) -> Self {
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
            bodies
                .into_iter()
                .map(|body| {
                    let (mut stream, _) = accept_with_deadline(&listener)
                        .expect("dsh should make every scripted request");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .expect("request read should be bounded");
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .expect("response write should be bounded");
                    let request = read_http_request(&mut stream);
                    write_sse_response(&mut stream, &body);
                    String::from_utf8(request).expect("request should be UTF-8")
                })
                .collect()
        });
        Self {
            base_url,
            worker: Some(worker),
        }
    }

    pub fn finish(mut self) -> Vec<String> {
        self.worker
            .take()
            .expect("server worker should exist")
            .join()
            .expect("server worker should join")
    }
}

impl CancelThenSseServer {
    pub fn start(partial: String, completed: String) -> Self {
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
            let (mut first, _) =
                accept_with_deadline(&listener).expect("dsh should make the stalled request");
            first
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read should be bounded");
            first
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write should be bounded");
            let first_request =
                String::from_utf8(read_http_request(&mut first)).expect("request should be UTF-8");
            let declared = partial
                .len()
                .checked_add(1024 * 1024)
                .expect("test body length should fit");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            first
                .write_all(headers.as_bytes())
                .and_then(|()| first.write_all(partial.as_bytes()))
                .and_then(|()| first.flush())
                .expect("partial SSE response should write");
            let first_connection = thread::spawn(move || wait_for_client_close(first));

            // Start the next request's deadline only after cancellation has
            // actually closed the first connection. An early second connect
            // remains safely queued in the listener backlog.
            let first_closed = first_connection
                .join()
                .expect("connection monitor should join");

            let (mut second, _) =
                accept_with_deadline(&listener).expect("dsh should make the post-cancel request");
            second
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read should be bounded");
            second
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write should be bounded");
            let second_request =
                String::from_utf8(read_http_request(&mut second)).expect("request should be UTF-8");
            write_sse_response(&mut second, &completed);
            (vec![first_request, second_request], first_closed)
        });
        Self {
            base_url,
            worker: Some(worker),
        }
    }

    pub fn finish(mut self) -> (Vec<String>, bool) {
        self.worker
            .take()
            .expect("server worker should exist")
            .join()
            .expect("server worker should join")
    }
}

impl GatedFirstSseServer {
    pub fn start(first: String, second: String, remaining: Vec<String>) -> Self {
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
        let (release, wait_for_release) = sync_channel(0);
        let worker = thread::spawn(move || {
            let (mut stream, _) =
                accept_with_deadline(&listener).expect("dsh should make the gated request");
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read should be bounded");
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write should be bounded");
            let mut requests = vec![
                String::from_utf8(read_http_request(&mut stream)).expect("request should be UTF-8"),
            ];
            let body_bytes = first
                .len()
                .checked_add(second.len())
                .expect("test response length should fit");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {body_bytes}\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(headers.as_bytes())
                .and_then(|()| stream.write_all(first.as_bytes()))
                .and_then(|()| stream.flush())
                .expect("gated SSE prefix should write");
            wait_for_release
                .recv_timeout(Duration::from_secs(10))
                .expect("the test should release the gated response");
            stream
                .write_all(second.as_bytes())
                .and_then(|()| stream.flush())
                .expect("gated SSE suffix should write");

            for body in remaining {
                let (mut stream, _) = accept_with_deadline(&listener)
                    .expect("dsh should make every remaining request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("request read should be bounded");
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .expect("response write should be bounded");
                requests.push(
                    String::from_utf8(read_http_request(&mut stream))
                        .expect("request should be UTF-8"),
                );
                write_sse_response(&mut stream, &body);
            }
            requests
        });
        Self {
            base_url,
            release: Some(release),
            worker: Some(worker),
        }
    }

    pub fn release(&mut self) {
        self.release
            .take()
            .expect("release sender should exist")
            .send(())
            .expect("server should still be waiting");
    }

    pub fn finish(mut self) -> Vec<String> {
        drop(self.release.take());
        self.worker
            .take()
            .expect("server worker should exist")
            .join()
            .expect("server worker should join")
    }
}

impl StalledSseServer {
    pub fn start(partial: String) -> Self {
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
            let (mut stream, _) =
                accept_with_deadline(&listener).expect("dsh should make the stalled request");
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
                .expect("test body length should fit");
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n"
            );
            stream
                .write_all(headers.as_bytes())
                .and_then(|()| stream.write_all(partial.as_bytes()))
                .and_then(|()| stream.flush())
                .expect("partial SSE response should write");
            let closed = wait_for_client_close(stream);
            (request, closed)
        });
        Self {
            base_url,
            worker: Some(worker),
        }
    }

    pub fn finish(mut self) -> (String, bool) {
        self.worker
            .take()
            .expect("server worker should exist")
            .join()
            .expect("server worker should join")
    }
}

impl BacklogThenStalledSseServer {
    pub fn start(first_response: String) -> Self {
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
        let (ready_sender, second_request_ready) = sync_channel(1);
        let worker = thread::spawn(move || {
            let (mut first, _) =
                accept_with_deadline(&listener).expect("dsh should make the backlog request");
            first
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read should be bounded");
            first
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write should be bounded");
            let first_request =
                String::from_utf8(read_http_request(&mut first)).expect("request should be UTF-8");
            write_sse_response(&mut first, &first_response);

            let (mut second, _) =
                accept_with_deadline(&listener).expect("dsh should make the post-tool request");
            second
                .set_read_timeout(Some(Duration::from_secs(5)))
                .expect("request read should be bounded");
            second
                .set_write_timeout(Some(Duration::from_secs(5)))
                .expect("response write should be bounded");
            let second_request =
                String::from_utf8(read_http_request(&mut second)).expect("request should be UTF-8");
            let headers = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n";
            second
                .write_all(headers.as_bytes())
                .and_then(|()| second.flush())
                .expect("stalled response headers should write");
            ready_sender
                .send(())
                .expect("backlog test should still be waiting");
            let closed = wait_for_client_close(second);
            (vec![first_request, second_request], closed)
        });
        Self {
            base_url,
            second_request_ready,
            worker: Some(worker),
        }
    }

    pub fn wait_until_second_request(&self) {
        self.second_request_ready
            .recv_timeout(Duration::from_secs(10))
            .expect("Agent should reach its second request after committing the backlog");
    }

    pub fn finish(mut self) -> (Vec<String>, bool) {
        self.worker
            .take()
            .expect("backlog server worker should exist")
            .join()
            .expect("backlog server worker should join")
    }
}

fn wait_for_client_close(mut stream: TcpStream) -> bool {
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

fn write_sse_response(stream: &mut TcpStream, body: &str) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .and_then(|()| stream.write_all(body.as_bytes()))
        .and_then(|()| stream.flush())
        .expect("SSE response should write");
}

fn accept_with_deadline(listener: &TcpListener) -> Option<(TcpStream, std::net::SocketAddr)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, address)) => {
                stream
                    .set_nonblocking(false)
                    .expect("accepted socket should become blocking");
                return Some((stream, address));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("loopback accept failed: {error}"),
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
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
