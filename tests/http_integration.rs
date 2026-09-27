use dragon_server::{config::Config, server::Server};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    task::JoinHandle,
    time::timeout,
};

struct Harness {
    address: SocketAddr,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<std::io::Result<()>>>,
    directory: tempfile::TempDir,
}

impl Harness {
    async fn start(change: impl FnOnce(&mut Config)) -> Self {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("public")).unwrap();
        std::fs::write(
            directory.path().join("public/index.txt"),
            "Dragon static fixture\n",
        )
        .unwrap();
        std::fs::write(directory.path().join("public/binary.bin"), [0, 255, 17, 0]).unwrap();
        let path = directory.path().join("dragon.toml");
        std::fs::write(&path, include_str!("../examples/minimal/dragon.toml")).unwrap();
        let mut config = Config::load(&path).unwrap();
        change(&mut config);
        config.validate(directory.path()).unwrap();
        let server = Server::new(config).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(server.serve(listener, async {
            let _ = stopped.await;
        }));
        Self {
            address,
            stop: Some(stop),
            task: Some(task),
            directory,
        }
    }

    async fn raw(&self, bytes: &[u8]) -> Vec<u8> {
        timeout(Duration::from_secs(3), async {
            let mut socket = TcpStream::connect(self.address).await.unwrap();
            socket.write_all(bytes).await.unwrap();
            let mut result = Vec::new();
            let read = socket.read_to_end(&mut result).await;
            if let Err(error) = read {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
            }
            result
        })
        .await
        .expect("request timed out")
    }

    async fn get(&self, method: &str, target: &str) -> Vec<u8> {
        self.raw(
            format!("{method} {target} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
    }

    async fn finish(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        timeout(Duration::from_secs(2), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(TcpStream::connect(self.address).await.is_err());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn status(bytes: &[u8]) -> u16 {
    std::str::from_utf8(&bytes[..bytes.iter().position(|byte| *byte == b'\r').unwrap()])
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

fn body(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap()
        + 4;
    &bytes[start..]
}

async fn malformed_exchange(address: SocketAddr, request: &[u8], fragment_size: usize) -> Vec<u8> {
    timeout(Duration::from_secs(4), async {
        let mut socket = TcpStream::connect(address).await.unwrap();
        for fragment in request.chunks(fragment_size) {
            if let Err(error) = socket.write_all(fragment).await {
                assert!(matches!(
                    error.kind(),
                    std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                ));
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut response = Vec::new();
        let outcome = socket.take(65_536).read_to_end(&mut response).await;
        if let Err(error) = outcome {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        assert!(response.len() < 65_536, "unexpected unbounded response");
        response
    })
    .await
    .expect("malformed connection did not close within four seconds")
}

async fn assert_malformed_rejected(harness: &Harness, label: &str, request: &[u8]) -> usize {
    let mut wire = request.to_vec();
    wire.extend_from_slice(
        b"GET /assets/binary.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    );
    for fragment_size in [wire.len(), 7, 1] {
        let response = malformed_exchange(harness.address, &wire, fragment_size).await;
        assert!(
            response.is_empty() || (400..500).contains(&status(&response)),
            "{label}, fragment size {fragment_size}: {}",
            String::from_utf8_lossy(&response)
        );
        assert!(
            !response
                .windows(b"HTTP/1.1 200".len())
                .any(|part| part == b"HTTP/1.1 200"),
            "{label}: processed rejected request or its pipeline"
        );
        assert!(
            !response.windows(4).any(|part| part == [0, 255, 17, 0]),
            "{label}: pipelined route ran"
        );
        let healthy = harness.get("GET", "/hello").await;
        assert_eq!(status(&healthy), 200, "{label}: server did not recover");
        assert_eq!(body(&healthy), b"{\"message\":\"Hello from Dragon\"}");
    }
    3
}

#[tokio::test]
async fn malformed_traffic_corpus_rejects_and_recovers() {
    let harness = Harness::start(|_| {}).await;
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    for headers in [
        "Content-Length: 1\r\nTransfer-Encoding: chunked\r\n",
        "Transfer-Encoding: chunked\r\nContent-Length: 1\r\n",
        "Content-Length: 1\r\nContent-Length: 2\r\n",
        "Content-Length: -1\r\n",
        "Content-Length: +1\r\n",
        "Content-Length: 1, 1\r\n",
        "Content-Length: 18446744073709551616\r\n",
        "Content-Length: \r\n",
        "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n",
        "Transfer-Encoding: gzip, chunked\r\n",
        "Transfer-Encoding : chunked\r\n",
        "X-Broken\r\n",
        "X-Value: first\r\n folded\r\n",
        "Host: other.example\r\n",
        "Expect: unsupported\r\n",
    ] {
        cases.push((
            headers.to_owned(),
            format!("GET /hello HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n").into_bytes(),
        ));
    }
    for chunk in [
        "Z\r\n",
        "-1\r\n",
        "10000000000000000\r\n",
        "1\r\nxXX",
        "0\r\nHost: other.example\r\n\r\n",
        "0\r\nContent-Length: 0\r\n\r\n",
        "0\r\nTransfer-Encoding: chunked\r\n\r\n",
        "0\r\nX-Broken\r\n\r\n",
    ] {
        cases.push((format!("chunk {chunk:?}"), format!("GET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n{chunk}").into_bytes()));
    }
    for target in [
        "/%",
        "/%GG",
        "/%00",
        "/%2fetc",
        "/%5cetc",
        "/%252e%252e/secret",
        "/../secret",
        "/%ff",
        "http://localhost/hello",
    ] {
        cases.push((
            format!("target {target}"),
            format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").into_bytes(),
        ));
    }
    for host in [
        "",
        "localhost:99999",
        "user@localhost",
        "[::1",
        "*",
        "local host",
    ] {
        cases.push((
            format!("host {host}"),
            format!("GET /hello HTTP/1.1\r\nHost: {host}\r\n\r\n").into_bytes(),
        ));
    }
    cases.push((
        "missing host".into(),
        b"GET /hello HTTP/1.1\r\n\r\n".to_vec(),
    ));
    cases.push((
        "header byte budget".into(),
        format!(
            "GET /hello HTTP/1.1\r\nHost: localhost\r\nX-Large: {}\r\n\r\n",
            "x".repeat(17_000)
        )
        .into_bytes(),
    ));
    cases.push(("chunk line budget".into(), format!("GET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n1;{}\r\nx\r\n0\r\n\r\n", "x".repeat(1024)).into_bytes()));
    let mut exchanges = 0;
    for (label, request) in &cases {
        exchanges += assert_malformed_rejected(&harness, label, request).await;
    }
    println!(
        "malformed corpus: {} cases, {exchanges} rejected exchanges, {exchanges} successful recovery checks",
        cases.len()
    );
    harness.finish().await;
}

#[tokio::test]
async fn malformed_traffic_header_byte_mutations_reject_and_recover() {
    let harness = Harness::start(|_| {}).await;
    let mut exchanges = 0;
    for invalid in (0u8..=32).chain([127, 255]) {
        let mut request = b"GET /hello HTTP/1.1\r\nHost: localhost\r\nX-".to_vec();
        request.push(invalid);
        request.extend_from_slice(b"Test: value\r\n\r\n");
        exchanges +=
            assert_malformed_rejected(&harness, &format!("header byte {invalid:#04x}"), &request)
                .await;
    }
    println!(
        "header mutation corpus: 35 cases, {exchanges} rejected exchanges, {exchanges} successful recovery checks"
    );
    harness.finish().await;
}

#[tokio::test]
async fn rejects_truncated_bodies_and_recovers_request_capacity() {
    let harness = Harness::start(|config| config.limits.max_in_flight_requests = 1).await;
    for request in [
        "GET /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 8\r\n\r\nabc",
        "GET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n8\r\nabc",
        "GET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-Unfinished: value",
    ] {
        let mut socket = TcpStream::connect(harness.address).await.unwrap();
        socket.write_all(request.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
        let mut response = Vec::new();
        let outcome = timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
            .await
            .unwrap();
        if let Err(error) = outcome {
            assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
        }
        assert!(response.is_empty() || status(&response) == 400);
        assert_eq!(status(&harness.get("GET", "/hello").await), 200);
    }
    harness.finish().await;
}

#[tokio::test]
async fn handles_fragmented_bodies_before_the_next_request() {
    let harness = Harness::start(|_| {}).await;
    for (head, payload) in [
        ("Content-Length: 10", "abc\r\n\r\ndef"),
        (
            "Transfer-Encoding: chunked",
            "3\r\nabc\r\n2\r\nde\r\n0\r\nX-Check: yes\r\n\r\n",
        ),
    ] {
        let mut socket = TcpStream::connect(harness.address).await.unwrap();
        socket
            .write_all(
                format!("GET /hello HTTP/1.1\r\nHost: localhost\r\n{head}\r\n\r\n").as_bytes(),
            )
            .await
            .unwrap();
        for byte in payload.bytes() {
            socket.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
        socket
            .write_all(
                b"GET /assets/binary.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = Vec::new();
        timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&response)
                .matches("HTTP/1.1 200")
                .count(),
            2
        );
        assert!(response.ends_with(&[0, 255, 17, 0]));
    }
    harness.finish().await;
}

#[tokio::test]
async fn stays_available_across_repeated_concurrent_connection_cycles() {
    let harness = Harness::start(|config| {
        config.limits.max_connections = 16;
        config.limits.max_in_flight_requests = 8;
    })
    .await;
    for _cycle in 0..32 {
        let mut clients = tokio::task::JoinSet::new();
        for _client in 0..8 {
            let address = harness.address;
            clients.spawn(async move {
                let mut socket = TcpStream::connect(address).await.unwrap();
                socket.write_all(b"GET /assets/binary.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
                let mut response = Vec::new();
                timeout(Duration::from_secs(2), socket.read_to_end(&mut response)).await.unwrap().unwrap();
                assert_eq!(status(&response), 200);
                assert_eq!(body(&response), &[0, 255, 17, 0]);
            });
        }
        while let Some(result) = clients.join_next().await {
            result.unwrap();
        }
    }
    assert_eq!(status(&harness.get("GET", "/hello").await), 200);
    harness.finish().await;
}

#[tokio::test]
async fn serves_json_head_static_and_routing_errors() {
    let harness = Harness::start(|_| {}).await;
    let response = harness.get("GET", "/hello?private=value").await;
    assert_eq!(status(&response), 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(body(&response)).unwrap()["message"],
        "Hello from Dragon"
    );
    let head = harness.get("HEAD", "/hello").await;
    assert_eq!(status(&head), 200);
    assert!(body(&head).is_empty());
    assert!(String::from_utf8_lossy(&head).contains("content-length: 31"));
    assert_eq!(
        body(&harness.get("GET", "/assets").await),
        b"Dragon static fixture\n"
    );
    assert_eq!(
        body(&harness.get("GET", "/assets/binary.bin").await),
        &[0, 255, 17, 0]
    );
    for target in ["/missing", "/assets-other", "/assets/missing", "/hello/"] {
        assert_eq!(status(&harness.get("GET", target).await), 404, "{target}");
    }
    let denied = harness.get("POST", "/hello").await;
    assert_eq!(status(&denied), 405);
    assert!(String::from_utf8_lossy(&denied).contains("allow: GET, HEAD"));
    assert_eq!(status(&harness.get("OPTIONS", "*").await), 204);
    assert_eq!(
        status(
            &harness
                .raw(b"GET /hello HTTP/1.1\r\nHost: unknown.example\r\nConnection: close\r\n\r\n")
                .await
        ),
        404
    );
    harness.finish().await;
}

#[tokio::test]
async fn rejects_unsafe_targets_hosts_and_framing() {
    let harness = Harness::start(|_| {}).await;
    for target in [
        "/assets/%2e%2e/secret",
        "/assets/%252e%252e/secret",
        "/assets/%2fetc",
        "/assets/%00",
        "/assets/%zz",
        "http://localhost/hello",
    ] {
        assert_eq!(status(&harness.get("GET", target).await), 400, "{target}");
    }
    for request in [
        "GET /hello HTTP/1.1\r\nConnection: close\r\n\r\n",
        "GET /hello HTTP/1.1\r\nHost: localhost:99999\r\nConnection: close\r\n\r\n",
        "GET /hello HTTP/1.1\r\nHost: localhost\r\nHost: other\r\nConnection: close\r\n\r\n",
        "POST /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc",
        "POST /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\nConnection: close\r\n\r\n0\r\n\r\n",
    ] {
        let response = harness.raw(request.as_bytes()).await;
        assert!(response.is_empty() || status(&response) == 400, "{request}");
    }
    harness.finish().await;
}

#[tokio::test]
async fn supports_chunking_fragmentation_and_pipeline_order() {
    let harness = Harness::start(|_| {}).await;
    let response = harness.raw(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n3\r\nabc\r\n0\r\n\r\n").await;
    assert_eq!(status(&response), 200);
    let response = harness.raw(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\nGET /assets HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await;
    let text = String::from_utf8(response).unwrap();
    assert_eq!(text.matches("HTTP/1.1 200 OK").count(), 2);
    assert!(text.find("Hello from Dragon").unwrap() < text.find("Dragon static fixture").unwrap());
    let mut socket = TcpStream::connect(harness.address).await.unwrap();
    for fragment in [
        "GET /hel",
        "lo HTTP/1.1\r\n",
        "Host: local",
        "host\r\nConnection: close\r\n\r\n",
    ] {
        socket.write_all(fragment.as_bytes()).await.unwrap();
        tokio::task::yield_now().await;
    }
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status(&response), 200);
    harness.finish().await;
}

#[tokio::test]
async fn enforces_body_target_and_header_limits() {
    let harness = Harness::start(|config| {
        config.limits.max_body_bytes = 8;
        config.limits.max_target_bytes = 32;
        config.limits.max_headers = 8;
    })
    .await;
    assert_eq!(
        status(&harness.get("GET", &format!("/{}", "a".repeat(33))).await),
        414
    );
    for request in [
        "GET /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 9\r\nConnection: close\r\n\r\n123456789",
        "GET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n9\r\n123456789\r\n0\r\n\r\n",
    ] {
        assert_eq!(status(&harness.raw(request.as_bytes()).await), 413);
    }
    let headers = (0..10)
        .map(|index| format!("X-Test-{index}: value\r\n"))
        .collect::<String>();
    let response = harness
        .raw(
            format!("GET /hello HTTP/1.1\r\nHost: localhost\r\n{headers}Connection: close\r\n\r\n")
                .as_bytes(),
        )
        .await;
    assert!(response.is_empty() || status(&response) == 431);
    harness.finish().await;
}

#[tokio::test]
async fn times_out_slow_clients_and_drains_shutdown() {
    let harness = Harness::start(|config| {
        config.limits.header_timeout_ms = 80;
        config.limits.idle_timeout_ms = 80;
        config.server.shutdown_timeout_ms = 80;
    })
    .await;
    let mut socket = TcpStream::connect(harness.address).await.unwrap();
    socket
        .write_all(b"GET /hello HTTP/1.1\r\nHost:")
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.is_empty() || status(&response) == 408);
    let _idle = TcpStream::connect(harness.address).await.unwrap();
    harness.finish().await;
}

#[cfg(unix)]
#[tokio::test]
async fn static_service_denies_links_hidden_files_and_streams_large_files() {
    use std::os::unix::fs::symlink;
    let harness = Harness::start(|_| {}).await;
    let root = harness.directory.path().join("public");
    std::fs::write(root.join(".secret"), "hidden").unwrap();
    symlink(
        harness.directory.path().join("dragon.toml"),
        root.join("link.txt"),
    )
    .unwrap();
    let large = vec![0x5a; 131_073];
    std::fs::write(root.join("large.bin"), &large).unwrap();
    for path in ["/assets/.secret", "/assets/link.txt"] {
        assert_eq!(status(&harness.get("GET", path).await), 404);
    }
    assert_eq!(body(&harness.get("GET", "/assets/large.bin").await), large);
    assert!(body(&harness.get("HEAD", "/assets/large.bin").await).is_empty());
    harness.finish().await;
}

#[tokio::test]
async fn bounds_requests_and_recovers_after_completion() {
    let harness = Harness::start(|config| config.limits.max_in_flight_requests = 1).await;
    let mut occupied = TcpStream::connect(harness.address).await.unwrap();
    occupied.write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut interim = vec![0; b"HTTP/1.1 100 Continue\r\n\r\n".len()];
    timeout(Duration::from_secs(2), occupied.read_exact(&mut interim))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(interim, b"HTTP/1.1 100 Continue\r\n\r\n");
    assert_eq!(status(&harness.get("GET", "/hello").await), 503);
    occupied.write_all(b"x").await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), occupied.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(status(&response), 200);
    assert_eq!(status(&harness.get("GET", "/hello").await), 200);
    harness.finish().await;
}

#[tokio::test]
async fn bounds_connections_and_times_out_incomplete_bodies() {
    let harness = Harness::start(|config| {
        config.limits.max_connections = 1;
        config.limits.body_timeout_ms = 150;
    })
    .await;
    let mut occupied = TcpStream::connect(harness.address).await.unwrap();
    occupied.write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut interim = vec![0; b"HTTP/1.1 100 Continue\r\n\r\n".len()];
    timeout(Duration::from_secs(2), occupied.read_exact(&mut interim))
        .await
        .unwrap()
        .unwrap();
    assert!(harness.get("GET", "/hello").await.is_empty());
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), occupied.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.is_empty() || status(&response) == 408);
    harness.finish().await;
}

#[tokio::test]
async fn response_deadline_releases_a_slow_readers_request_slot() {
    let harness = Harness::start(|config| {
        config.limits.max_in_flight_requests = 1;
        config.limits.response_timeout_ms = 100;
    })
    .await;
    let file = std::fs::File::create(harness.directory.path().join("public/large.bin")).unwrap();
    file.set_len(64 * 1024 * 1024).unwrap();
    let mut slow = TcpStream::connect(harness.address).await.unwrap();
    slow.write_all(
        b"GET /assets/large.bin HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await
    .unwrap();
    let mut first = [0; 1];
    timeout(Duration::from_secs(2), slow.read_exact(&mut first))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(status(&harness.get("GET", "/hello").await), 200);
    let mut remainder = Vec::new();
    timeout(Duration::from_secs(2), slow.read_to_end(&mut remainder))
        .await
        .unwrap()
        .unwrap();
    assert!(remainder.len() < 64 * 1024 * 1024);
    harness.finish().await;
}

#[tokio::test]
async fn checks_framing_on_every_keep_alive_request() {
    let harness = Harness::start(|_| {}).await;
    let payload = b"GET /hello HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabcGET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nContent-Length: 5\r\n\r\n0\r\n\r\n";
    let response = harness.raw(payload).await;
    assert!(
        String::from_utf8_lossy(&response)
            .matches("HTTP/1.1 200")
            .count()
            <= 1
    );
    let response = harness.raw(b"GET /hello HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\nX-Trailer: test\r\n\r\nGET /hello HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await;
    assert_eq!(
        String::from_utf8_lossy(&response)
            .matches("HTTP/1.1 200")
            .count(),
        2
    );
    harness.finish().await;
}
