#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::{collections::BTreeMap, io::Write, time::Duration};

use dragon_server::process::{ProcessManager, ProcessSpec};
use tokio::time::{Instant, timeout};

fn fixture(mode: &str, directory: &std::path::Path) -> ProcessSpec {
    ProcessSpec {
        executable: std::env::current_exe().unwrap(),
        args: [
            "--ignored",
            "--exact",
            "child_fixture",
            "--nocapture",
            "--skip",
            "$(touch argv-injected); literal argument",
        ]
        .map(Into::into)
        .to_vec(),
        cwd: directory.to_owned(),
        env: BTreeMap::from([
            ("DRAGON_FIXTURE_MODE".into(), mode.into()),
            (
                "DRAGON_LITERAL".into(),
                "$(touch injected); literal value".into(),
            ),
        ]),
    }
}

#[tokio::test]
#[ignore = "requires DRAGON_NIRAL_ROOT and DRAGON_NODE pointing to a local Niral checkout and Node executable"]
async fn application_niral_production_smoke() {
    use dragon_server::application::{ApplicationSpec, ReadinessSpec};

    let root = std::fs::canonicalize(std::env::var_os("DRAGON_NIRAL_ROOT").unwrap()).unwrap();
    let node = std::fs::canonicalize(std::env::var_os("DRAGON_NODE").unwrap()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("routes")).unwrap();
    std::fs::write(
        directory.path().join("package.json"),
        r#"{"type":"module"}"#,
    )
    .unwrap();
    std::fs::write(
        directory.path().join("routes/index.niral"),
        r#"<server>
export function load() { return { message: "Niral SSR under Dragon" }; }
</server>
<script>let { message } = $props;</script>
<h1>{message}</h1>
"#,
    )
    .unwrap();
    let processes = ProcessManager::new(1).unwrap();
    let build = processes
        .spawn(ProcessSpec {
            executable: node.clone(),
            args: vec![root.join("bin/niral.js").into(), "build".into(), ".".into()],
            cwd: directory.path().to_owned(),
            env: BTreeMap::new(),
        })
        .unwrap();
    let build = timeout(Duration::from_secs(30), build.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let spec = ApplicationSpec {
        application_id: "test-app".into(),
        release_id: "local-production-build".into(),
        process: ProcessSpec {
            executable: node.clone(),
            args: vec![
                "--input-type=module".into(),
                "--eval".into(),
                r#"import { pathToFileURL } from 'node:url';
const { createProdServer } = await import(pathToFileURL(process.env.NIRAL_PROD_MODULE));
const app = createProdServer({ dist: 'dist', cwd: process.cwd() });
app.server.listen(Number(process.env.NIRAL_TEST_PORT), '127.0.0.1');
process.once('SIGTERM', async () => { await app.shutdown(); process.exit(0); });
"#
                .into(),
            ],
            cwd: directory.path().to_owned(),
            env: BTreeMap::from([
                (
                    "NIRAL_PROD_MODULE".into(),
                    root.join("src/server/prod.js").into(),
                ),
                ("NIRAL_TEST_PORT".into(), address.port().to_string().into()),
            ]),
        },
        readiness: ReadinessSpec {
            address,
            path: "/@niral/health".into(),
            startup_timeout: Duration::from_secs(10),
            probe_timeout: Duration::from_millis(500),
            interval: Duration::from_millis(25),
            max_attempts: 200,
        },
    };
    let mut config = hosted_config(spec);
    config.limits.response_timeout_ms = 5000;
    config.sites[0].routes[0].streaming = true;
    config.sites[0].routes[0].websocket = true;
    let server = dragon_server::server::Server::new(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public_address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(listener, async {
        let _ = stopped.await;
    }));
    let client = processes
        .spawn(ProcessSpec {
            executable: node,
            args: vec![
                "--input-type=module".into(),
                "--eval".into(),
                r#"
import assert from 'node:assert/strict';
const base = process.env.NIRAL_TEST_URL;
const health = await fetch(base + '/@niral/health');
assert.equal(health.status, 200);
assert.ok((await health.json()).release);
const page = await fetch(base + '/');
assert.equal(page.status, 200);
assert.equal(page.headers.get('server'), 'Dragon');
assert.match(page.headers.get('content-type'), /text\/html/);
const html = await page.text();
assert.ok(html.includes('Niral SSR under Dragon'));
const assets = [...html.matchAll(/(?:src|href)="(\/assets\/[^"?#]+)[^"]*"/g)];
assert.ok(assets.length > 0, 'production assets present');
for (const [, asset] of assets) {
  const response = await fetch(base + asset);
  assert.equal(response.status, 200, asset);
  assert.ok((await response.arrayBuffer()).byteLength > 0, asset);
}
assert.equal((await fetch(base + '/missing-smoke-route')).status, 404);
console.log('Niral health, dynamic SSR, production assets and 404 checks passed through Dragon');
"#
                .into(),
            ],
            cwd: directory.path().to_owned(),
            env: BTreeMap::from([(
                "NIRAL_TEST_URL".into(),
                format!("http://{public_address}").into(),
            )]),
        })
        .unwrap();
    let client = timeout(Duration::from_secs(15), client.wait()).await;
    let _ = stop.send(());
    let cleanup = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let client = client.unwrap().unwrap();
    assert!(
        client.status.success(),
        "{}",
        String::from_utf8_lossy(&client.stderr)
    );
    println!("{}", String::from_utf8_lossy(&client.stdout));
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    assert!(
        tokio::net::TcpStream::connect(public_address)
            .await
            .is_err()
    );
    assert!(client.group_cleanup_error.is_none());
    assert!(cleanup.is_ok(), "Niral cleanup failed: {cleanup:?}");
    println!("Dragon and Niral listeners closed; cleanup: {cleanup:?}");
}

#[test]
#[ignore = "native child fixture invoked explicitly by process integration tests"]
fn child_fixture() {
    let Ok(mut mode) = std::env::var("DRAGON_FIXTURE_MODE") else {
        return;
    };
    if matches!(
        mode.as_str(),
        "restart-crash" | "restart-success" | "restart-recover" | "restart-unready"
    ) {
        let count = std::fs::read_to_string("launches")
            .map(|value| value.parse::<u32>().unwrap())
            .unwrap_or(0)
            + 1;
        std::fs::write("launches", count.to_string()).unwrap();
        let _ = std::fs::remove_file("leaf-ready");
        let spec = fixture("leaf", &std::env::current_dir().unwrap());
        let mut descendant = std::process::Command::new(spec.executable)
            .args(spec.args)
            .env_clear()
            .envs(spec.env)
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !std::path::Path::new("leaf-ready").exists() {
            if std::time::Instant::now() >= deadline {
                descendant.kill().unwrap();
                descendant.wait().unwrap();
                panic!("restart fixture helper did not start");
            }
            std::thread::yield_now();
        }
        std::thread::spawn(move || descendant.wait().unwrap());
        if mode != "restart-unready" && (mode != "restart-recover" || count == 1) {
            std::process::exit(if mode == "restart-success" { 0 } else { 7 });
        }
        mode = "http-ready".into();
    }
    match mode.as_str() {
        "proxy" => {
            let listener =
                std::net::TcpListener::bind(std::env::var("DRAGON_FIXTURE_ADDRESS").unwrap())
                    .unwrap();
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let (target, head_size) = loop {
                    let mut buffer = [0; 4096];
                    let count = std::io::Read::read(&mut socket, &mut buffer).unwrap();
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&buffer[..count]);
                    assert!(request.len() < 131_072);
                    let mut headers = [httparse::EMPTY_HEADER; 100];
                    let mut parsed = httparse::Request::new(&mut headers);
                    if let Ok(httparse::Status::Complete(head_size)) = parsed.parse(&request) {
                        let length = parsed
                            .headers
                            .iter()
                            .find(|header| header.name.eq_ignore_ascii_case("content-length"))
                            .map(|header| {
                                std::str::from_utf8(header.value)
                                    .unwrap()
                                    .parse::<usize>()
                                    .unwrap()
                            })
                            .unwrap_or(0);
                        if request.len() >= head_size + length {
                            break (parsed.path.unwrap().to_owned(), head_size);
                        }
                    }
                };
                if target == "/exit" {
                    return;
                }
                if target == "/ws-bad" {
                    let _ = socket.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: wrong\r\n\r\n");
                    continue;
                }
                if target == "/ws" {
                    let mut headers = [httparse::EMPTY_HEADER; 100];
                    let mut parsed = httparse::Request::new(&mut headers);
                    parsed.parse(&request).unwrap();
                    let mut handshake = hyper::Request::builder().method("GET").uri("/ws");
                    for header in parsed.headers {
                        handshake = handshake.header(header.name, header.value);
                    }
                    let handshake = handshake.body(()).unwrap();
                    let mut response =
                        tungstenite::handshake::server::create_response(&handshake).unwrap();
                    if handshake.headers().get("sec-websocket-protocol").is_some() {
                        response
                            .headers_mut()
                            .insert("sec-websocket-protocol", "echo".parse().unwrap());
                    }
                    socket
                        .write_all(b"HTTP/1.1 101 Switching Protocols\r\n")
                        .unwrap();
                    for (name, value) in response.headers() {
                        write!(socket, "{}: {}\r\n", name.as_str(), value.to_str().unwrap())
                            .unwrap();
                    }
                    socket.write_all(b"\r\n").unwrap();
                    let mut websocket = tungstenite::WebSocket::from_partially_read(
                        socket,
                        request[head_size..].to_vec(),
                        tungstenite::protocol::Role::Server,
                        None,
                    );
                    while let Ok(message) = websocket.read() {
                        if message.is_close() {
                            let _ = websocket.flush();
                            break;
                        }
                        if message.is_ping() {
                            let _ = websocket.flush();
                        } else if websocket.send(message).is_err() {
                            break;
                        }
                    }
                    continue;
                }
                if target == "/stream" || target == "/stream-stall" {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nD\r\ndata: first\n\n\r\n");
                    let deadline = std::time::Instant::now() + Duration::from_secs(3);
                    while !std::path::Path::new("finish-stream").exists()
                        && std::time::Instant::now() < deadline
                    {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let _ = socket.write_all(b"C\r\ndata: last\n\n\r\n0\r\n\r\n");
                    continue;
                }
                if target == "/slow" {
                    std::thread::sleep(Duration::from_millis(500));
                }
                if target == "/oversized" {
                    let _ =
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n");
                    continue;
                }
                if target == "/bad" {
                    let _ = socket.write_all(b"not HTTP\r\n\r\n");
                    continue;
                }
                if target == "/chunked" {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close, x-private\r\nX-Private: hidden\r\n\r\n4\r\ntest\r\n0\r\n\r\n");
                    continue;
                }
                let body = if target == "/echo-body" {
                    &request[head_size..]
                } else {
                    &request[..]
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\nSet-Cookie: first=1\r\nSet-Cookie: second=2\r\nConnection: close, x-private\r\nX-Private: hidden\r\n\r\n",
                    body.len()
                );
                let _ = socket.write_all(head.as_bytes());
                if !request.starts_with(b"HEAD ") {
                    let _ = socket.write_all(body);
                }
            }
        }
        "http-ready" | "http-hang" | "http-oversized" | "http-redirect" | "http-malformed"
        | "http-truncated" => {
            let address = std::env::var("DRAGON_FIXTURE_ADDRESS").unwrap();
            let listener = std::net::TcpListener::bind(address).unwrap();
            std::fs::write("started", "listening").unwrap();
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0; 4096];
                let _ = std::io::Read::read(&mut socket, &mut request);
                std::fs::write("probe-started", "probe").unwrap();
                if std::path::Path::new("exit-now").exists() {
                    std::process::exit(9);
                }
                if mode == "http-hang" {
                    loop {
                        std::thread::park();
                    }
                }
                if mode == "http-oversized" {
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nX-Large: ");
                    let _ = socket.write_all(&vec![b'x'; 9000]);
                    continue;
                }
                let response: &[u8] = if mode == "http-redirect" {
                    b"HTTP/1.1 302 Found\r\nLocation: /other\r\n\r\n"
                } else if mode == "http-malformed" {
                    b"not HTTP\r\n\r\n"
                } else if mode == "http-truncated" {
                    b"HTTP/1.1 200 OK\r\nX-Incomplete: "
                } else if std::path::Path::new("allow-ready").exists() {
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"
                } else {
                    b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\n\r\n"
                };
                let _ = socket.write_all(response);
            }
        }
        "output" => {
            assert!(std::env::var_os("HOME").is_none());
            assert!(
                std::env::args().any(|value| value == "$(touch argv-injected); literal argument")
            );
            assert_eq!(
                std::env::var("DRAGON_LITERAL").unwrap(),
                "$(touch injected); literal value"
            );
            std::fs::write("cwd-marker", "correct directory").unwrap();
            std::io::stdout().write_all(&vec![b'o'; 100_000]).unwrap();
            std::io::stderr().write_all(&vec![b'e'; 100_000]).unwrap();
            std::io::stdout().flush().unwrap();
            std::io::stderr().flush().unwrap();
            std::process::exit(7);
        }
        "hold" => {
            std::fs::write("started", "ready").unwrap();
            loop {
                std::thread::park();
            }
        }
        "graceful" | "stubborn" | "tree" | "tree-exit" | "leaf" => {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let mut terminate =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .unwrap();
                    if mode == "leaf" {
                        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                        std::fs::write("leaf-address", listener.local_addr().unwrap().to_string())
                            .unwrap();
                        std::fs::write("leaf-ready", "ready").unwrap();
                        loop {
                            tokio::select! {
                                _ = listener.accept() => {},
                                _ = terminate.recv() => {},
                            }
                        }
                    }
                    if mode == "tree" || mode == "tree-exit" {
                        let spec = fixture("leaf", &std::env::current_dir().unwrap());
                        let mut descendant = tokio::process::Command::new(spec.executable)
                            .args(spec.args)
                            .env_clear()
                            .envs(spec.env)
                            .spawn()
                            .unwrap();
                        timeout(Duration::from_secs(3), async {
                            while !std::path::Path::new("leaf-ready").exists() {
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        std::fs::write("started", "ready").unwrap();
                        if mode == "tree-exit" {
                            std::process::exit(0);
                        }
                        let _ = descendant.wait().await;
                        std::future::pending::<()>().await;
                    }
                    std::fs::write("started", "ready").unwrap();
                    terminate.recv().await.unwrap();
                    if mode == "graceful" {
                        std::fs::write("graceful-stop", "flushed").unwrap();
                        std::process::exit(0);
                    }
                    std::future::pending::<()>().await;
                });
        }
        _ => panic!("unknown fixture mode"),
    }
}

async fn started(directory: &std::path::Path) {
    timeout(Duration::from_secs(3), async {
        while !directory.join("started").exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn application_spec(
    mode: &str,
    directory: &std::path::Path,
) -> dragon_server::application::ApplicationSpec {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let mut process = fixture(mode, directory);
    process
        .env
        .insert("DRAGON_FIXTURE_ADDRESS".into(), address.to_string().into());
    dragon_server::application::ApplicationSpec {
        application_id: "test-app".into(),
        release_id: "release-1".into(),
        process,
        readiness: dragon_server::application::ReadinessSpec {
            address,
            path: "/ready".into(),
            startup_timeout: Duration::from_secs(3),
            probe_timeout: Duration::from_millis(100),
            interval: Duration::from_millis(10),
            max_attempts: 300,
        },
    }
}

fn hosted_config(
    spec: dragon_server::application::ApplicationSpec,
) -> dragon_server::config::Config {
    let mut config: dragon_server::config::Config = toml::from_str(
        r#"
schema_version = 1
[server]
listen = "127.0.0.1:0"
[[sites]]
id = "hosted"
hosts = ["localhost", "127.0.0.1"]
[[sites.routes]]
path = "/"
match = "prefix"
methods = ["GET", "HEAD", "POST"]
action = "proxy"
application = "test-app"
"#,
    )
    .unwrap();
    config.limits.response_timeout_ms = 200;
    config
        .applications
        .push(dragon_server::config::Application {
            id: spec.application_id,
            release: spec.release_id,
            executable: spec.process.executable,
            args: spec
                .process
                .args
                .into_iter()
                .map(|value| value.into_string().unwrap())
                .collect(),
            cwd: spec.process.cwd,
            env: spec
                .process
                .env
                .into_iter()
                .map(|(key, value)| (key.into_string().unwrap(), value.into_string().unwrap()))
                .collect(),
            address: spec.readiness.address,
            readiness_path: spec.readiness.path,
            startup_timeout_ms: 3000,
            probe_timeout_ms: 100,
            probe_interval_ms: 10,
            probe_attempts: 300,
            restart: dragon_server::config::Restart::Never,
        });
    config
}

async fn proxy_raw(address: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    timeout(Duration::from_secs(5), async {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(request).await.unwrap();
        let mut bytes = Vec::new();
        socket.take(262_144).read_to_end(&mut bytes).await.unwrap();
        bytes
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn hosted_https_streaming_and_websocket_idle_timeout() {
    use futures_util::{SinkExt, StreamExt};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_rustls::{TlsConnector, rustls};
    let directory = tempfile::tempdir().unwrap();
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let certificate = directory.path().join("certificate.pem");
    let private_key = directory.path().join("private-key.pem");
    std::fs::write(&certificate, certified.cert.pem()).unwrap();
    std::fs::write(&private_key, certified.key_pair.serialize_pem()).unwrap();
    let mut config = hosted_config(application_spec("proxy", directory.path()));
    config.server.tls = Some(dragon_server::config::Tls {
        certificate,
        private_key,
        handshake_timeout_ms: 1000,
    });
    config.sites[0].routes[0].streaming = true;
    config.sites[0].routes[0].websocket = true;
    config.limits.idle_timeout_ms = 200;
    let server = dragon_server::server::Server::new(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(listener, async {
        let _ = stopped.await;
    }));
    let mut roots = rustls::RootCertStore::empty();
    roots.add(certified.cert.der().clone()).unwrap();
    let connector = TlsConnector::from(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    let connect = || async {
        connector
            .connect(
                rustls::pki_types::ServerName::try_from("localhost").unwrap(),
                tokio::net::TcpStream::connect(address).await.unwrap(),
            )
            .await
            .unwrap()
    };
    let mut socket = connect().await;
    socket.write_all(b"GET /echo HTTP/1.1\r\nHost: localhost\r\nOrigin: https://localhost\r\nCookie: session=tls\r\nX-Forwarded-Proto: spoof\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"));
    assert!(response.contains("x-forwarded-proto: https"));
    assert!(
        response.contains("origin: https://localhost") && response.contains("cookie: session=tls")
    );
    assert!(!response.contains("spoof"));
    let mut socket = connect().await;
    socket
        .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), async {
        while !response
            .windows(13)
            .any(|bytes| bytes == b"data: first\n\n")
        {
            let mut buffer = [0; 4096];
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            response.extend_from_slice(&buffer[..count]);
        }
    })
    .await
    .unwrap();
    std::fs::write(directory.path().join("finish-stream"), "finish").unwrap();
    timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.windows(12).any(|bytes| bytes == b"data: last\n\n"));
    let (mut websocket, _) = tokio_tungstenite::client_async("wss://localhost/ws", connect().await)
        .await
        .unwrap();
    websocket
        .send(tungstenite::Message::Text("secure".into()))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        tungstenite::Message::Text("secure".into())
    );
    assert!(
        timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .is_none_or(|result| result.is_err())
    );
    stop.send(()).unwrap();
    let _ = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn hosted_websocket_echo_admission_early_frames_and_shutdown() {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tungstenite::{Message, client::IntoClientRequest, protocol::Role};
    let directory = tempfile::tempdir().unwrap();
    let mut config = hosted_config(application_spec("proxy", directory.path()));
    config.sites[0].routes[0].websocket = true;
    config.limits.max_in_flight_requests = 1;
    config.limits.idle_timeout_ms = 1000;
    let server = dragon_server::server::Server::new(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(listener, async {
        let _ = stopped.await;
    }));
    let invalid = proxy_raw(address, b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: invalid\r\nSec-WebSocket-Version: 13\r\n\r\n").await;
    assert!(invalid.starts_with(b"HTTP/1.1 400"));
    assert!(
        tokio_tungstenite::connect_async(format!("ws://{address}/ws-bad"))
            .await
            .is_err()
    );
    let mut request = format!("ws://{address}/ws").into_client_request().unwrap();
    request
        .headers_mut()
        .insert("sec-websocket-protocol", "echo".parse().unwrap());
    let (mut websocket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(response.headers()["sec-websocket-protocol"], "echo");
    websocket
        .send(Message::Binary(vec![0, 255, 128, 42].into()))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Binary(vec![0, 255, 128, 42].into())
    );
    websocket
        .send(Message::Ping(vec![1, 2].into()))
        .await
        .unwrap();
    assert_eq!(
        timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Pong(vec![1, 2].into())
    );
    let overload = proxy_raw(
        address,
        b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(overload.starts_with(b"HTTP/1.1 503"));
    websocket.close(None).await.unwrap();
    let _ = timeout(Duration::from_secs(2), websocket.next())
        .await
        .unwrap();
    drop(websocket);
    timeout(Duration::from_secs(2), async {
        loop {
            let response = proxy_raw(
                address,
                b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await;
            if response.starts_with(b"HTTP/1.1 200") {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(b"GET /ws HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n").await.unwrap();
    let mut early =
        tokio_tungstenite::WebSocketStream::from_raw_socket(&mut socket, Role::Client, None).await;
    early.send(Message::Text("early".into())).await.unwrap();
    drop(early);
    let mut head = Vec::new();
    timeout(Duration::from_secs(2), async {
        while !head.ends_with(b"\r\n\r\n") {
            head.push(socket.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(head.starts_with(b"HTTP/1.1 101"));
    let mut websocket =
        tokio_tungstenite::WebSocketStream::from_raw_socket(socket, Role::Client, None).await;
    assert_eq!(
        timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        Message::Text("early".into())
    );
    stop.send(()).unwrap();
    let _ = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        timeout(Duration::from_secs(2), websocket.next())
            .await
            .unwrap()
            .is_none_or(|message| message.is_err())
    );
}

#[tokio::test]
async fn hosted_streaming_delivers_before_eof_and_closes_idle_streams() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for stalled in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut config = hosted_config(application_spec("proxy", directory.path()));
        config.sites[0].routes[0].streaming = true;
        config.limits.max_proxy_response_bytes = 1;
        config.limits.max_in_flight_requests = 1;
        config.limits.response_timeout_ms = 500;
        let server = dragon_server::server::Server::new(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.serve(listener, async {
            let _ = stopped.await;
        }));
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket
            .write_all(b"GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut received = Vec::new();
        timeout(Duration::from_secs(2), async {
            while !received
                .windows(13)
                .any(|bytes| bytes == b"data: first\n\n")
            {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert_ne!(count, 0, "stream closed before its first event");
                received.extend_from_slice(&buffer[..count]);
            }
        })
        .await
        .unwrap();
        let overload = proxy_raw(
            address,
            b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(overload.starts_with(b"HTTP/1.1 503"));
        if !stalled {
            std::fs::write(directory.path().join("finish-stream"), "finish").unwrap();
        }
        timeout(Duration::from_secs(2), socket.read_to_end(&mut received))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            received.windows(12).any(|bytes| bytes == b"data: last\n\n"),
            !stalled
        );
        assert_eq!(received.ends_with(b"0\r\n\r\n"), !stalled);
        stop.send(()).unwrap();
        let _ = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}

#[tokio::test]
async fn hosted_proxy_preserves_requests_and_bounds_upstream_failures() {
    let directory = tempfile::tempdir().unwrap();
    let config = hosted_config(application_spec("proxy", directory.path()));
    let upstream = config.applications[0].address;
    let server = dragon_server::server::Server::new(config).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(listener, async {
        let _ = stopped.await;
    }));
    let bytes = proxy_raw(address, b"POST /echo?value=one%20two HTTP/1.1\r\nHost: localhost:9999\r\nOrigin: http://localhost:9999\r\nCookie: session=test\r\nX-Forwarded-For: spoof\r\nForwarded: spoof\r\nX-Real-IP: spoof\r\nX-Private: hidden\r\nConnection: close, x-private\r\nContent-Length: 7\r\n\r\npayload").await;
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with("HTTP/1.1 200"), "{text}");
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(body.starts_with("POST /echo?value=one%20two HTTP/1.1"));
    assert!(body.ends_with("payload"));
    assert!(body.contains("host: localhost:9999"));
    assert!(body.contains("origin: http://localhost:9999"));
    assert!(body.contains("cookie: session=test"));
    assert!(body.contains("x-forwarded-proto: http"));
    assert!(!text.contains("spoof") && !text.contains("hidden"));
    assert!(head.contains("set-cookie: first=1") && head.contains("set-cookie: second=2"));
    assert!(head.contains("server: Dragon"));
    let bytes = proxy_raw(address, b"POST /echo-body HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n\x00\xffab\r\n0\r\nX-Trailer: ignored\r\n\r\n").await;
    assert!(bytes.starts_with(b"HTTP/1.1 200"));
    assert!(bytes.ends_with(b"\x00\xffab"));
    let head = proxy_raw(
        address,
        b"HEAD /echo HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(head.starts_with(b"HTTP/1.1 200") && head.ends_with(b"\r\n\r\n"));
    for (path, expected) in [
        ("/chunked", 200),
        ("/oversized", 502),
        ("/bad", 502),
        ("/slow", 504),
    ] {
        let bytes = proxy_raw(
            address,
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await;
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.starts_with(&format!("HTTP/1.1 {expected}")),
            "{path}: {text}"
        );
        if path == "/chunked" {
            assert!(
                text.ends_with("test")
                    && !text.contains("transfer-encoding")
                    && !text.contains("hidden")
            );
        }
    }
    let upgrade = proxy_raw(address, b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close, upgrade\r\nUpgrade: websocket\r\n\r\n").await;
    assert!(upgrade.starts_with(b"HTTP/1.1 501"));
    stop.send(()).unwrap();
    let cleanup = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(cleanup.is_ok(), "proxy cleanup failed: {cleanup:?}");
    assert!(tokio::net::TcpStream::connect(upstream).await.is_err());
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[tokio::test]
async fn hosted_proxy_withholds_traffic_after_application_exit() {
    let directory = tempfile::tempdir().unwrap();
    let server = dragon_server::server::Server::new(hosted_config(application_spec(
        "proxy",
        directory.path(),
    )))
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.serve(listener, async {
        let _ = stopped.await;
    }));
    let failed = proxy_raw(
        address,
        b"GET /exit HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(failed.starts_with(b"HTTP/1.1 502"));
    timeout(Duration::from_secs(3), async {
        loop {
            let bytes = proxy_raw(
                address,
                b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await;
            if bytes.starts_with(b"HTTP/1.1 503") {
                break;
            }
            assert!(bytes.starts_with(b"HTTP/1.1 502"));
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    stop.send(()).unwrap();
    let _ = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn hosted_startup_failure_and_cancellation_clean_up() {
    for cancel in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut config = hosted_config(application_spec("http-ready", directory.path()));
        config.applications[0].startup_timeout_ms = 200;
        let upstream = config.applications[0].address;
        let server = dragon_server::server::Server::new(config).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.serve(listener, async {
            let _ = stopped.await;
        }));
        started(directory.path()).await;
        if cancel {
            let _ = stop.send(());
        }
        let outcome = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        if !cancel {
            assert!(outcome.is_err());
        }
        assert!(tokio::net::TcpStream::connect(upstream).await.is_err());
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
}

#[test]
fn hosted_configuration_rejects_invalid_launch_and_proxy_fields() {
    let directory = tempfile::tempdir().unwrap();
    for case in 0..9 {
        let mut config = hosted_config(application_spec("proxy", directory.path()));
        match case {
            0 => config.applications[0].address = "192.0.2.1:8199".parse().unwrap(),
            1 => config.applications[0].probe_attempts = 0,
            2 => config.applications[0]
                .env
                .insert("bad=key".into(), "value".into())
                .map(|_| ())
                .unwrap_or(()),
            3 => config.sites[0].routes[0].application = Some("missing".into()),
            4 => config.sites[0].routes[0].body = Some("not permitted".into()),
            5 => config.applications[0].readiness_path = "//invalid".into(),
            6 => config.applications[0].args.push("bad\0argument".into()),
            7 => {
                config.applications[0].restart = dragon_server::config::Restart::Always {
                    max_restarts: 0,
                    initial_delay_ms: 1,
                    max_delay_ms: 1,
                }
            }
            _ => config.applications[0].executable = directory.path().join("missing"),
        }
        assert!(config.validate(directory.path()).is_err(), "case {case}");
    }
}

#[tokio::test]
async fn application_readiness_is_distinct_from_process_existence() {
    use dragon_server::application::{ApplicationManager, InstanceState};
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    let spec = application_spec("http-ready", directory.path());
    let address = spec.readiness.address;
    let mut instance = manager.start(spec).unwrap();
    let identity = instance.identity().clone();
    let states = instance.subscribe();
    started(directory.path()).await;
    assert_eq!(instance.state(), InstanceState::Running);
    assert!(
        timeout(Duration::from_millis(40), instance.wait_ready())
            .await
            .is_err()
    );
    std::fs::write(directory.path().join("allow-ready"), "ready").unwrap();
    timeout(Duration::from_secs(2), instance.wait_ready())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(instance.state(), InstanceState::Ready);
    let output = timeout(Duration::from_secs(3), instance.stop())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.identity, identity);
    assert!(output.failure.is_none());
    assert!(output.process.group_cleanup_error.is_none());
    assert_eq!(*states.borrow(), InstanceState::Stopped);
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    let next = manager
        .start(application_spec("hold", directory.path()))
        .unwrap();
    assert_ne!(identity.instance_id, next.identity().instance_id);
    assert_eq!(next.identity().process_generation, 1);
    next.stop().await.unwrap();
}

#[tokio::test]
async fn application_restart_budget_is_bounded() {
    use dragon_server::application::{
        ApplicationManager, InstanceFailure, InstanceState, RestartBudget, RestartPolicy,
    };
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    let started = Instant::now();
    let instance = manager
        .start_with_restart(
            application_spec("restart-crash", directory.path()),
            RestartPolicy::OnFailure(RestartBudget {
                max_restarts: 2,
                initial_delay: Duration::from_millis(20),
                max_delay: Duration::from_millis(40),
            }),
        )
        .unwrap();
    let identity = instance.identity().clone();
    let states = instance.subscribe();
    let output = timeout(Duration::from_secs(4), instance.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        output.failure,
        Some(InstanceFailure::RestartLimit),
        "cleanup: {:?}",
        output.process.group_cleanup_error
    );
    assert_eq!(output.identity.instance_id, identity.instance_id);
    assert_eq!(output.identity.process_generation, 3);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches")).unwrap(),
        "3"
    );
    assert!(started.elapsed() >= Duration::from_millis(60));
    assert_eq!(*states.borrow(), InstanceState::Failed);
    application_capacity_recovers(&manager, directory.path()).await;
}

fn restart_budget() -> dragon_server::application::RestartBudget {
    dragon_server::application::RestartBudget {
        max_restarts: 2,
        initial_delay: Duration::from_millis(20),
        max_delay: Duration::from_millis(30),
    }
}

#[tokio::test]
async fn application_restart_repeats_readiness_and_updates_generation() {
    use dragon_server::application::{ApplicationManager, InstanceState, RestartPolicy};
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    let mut instance = manager
        .start_with_restart(
            application_spec("restart-recover", directory.path()),
            RestartPolicy::OnFailure(restart_budget()),
        )
        .unwrap();
    let initial = instance.identity().clone();
    let mut states = instance.subscribe();
    timeout(Duration::from_secs(3), async {
        loop {
            if *states.borrow_and_update() == InstanceState::Running
                && instance.current_identity().process_generation == 2
            {
                break;
            }
            states.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(
        timeout(Duration::from_millis(50), instance.wait_ready())
            .await
            .is_err()
    );
    assert_eq!(instance.state(), InstanceState::Running);
    std::fs::write(directory.path().join("allow-ready"), "ready").unwrap();
    timeout(Duration::from_secs(3), instance.wait_ready())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(instance.current_identity().instance_id, initial.instance_id);
    assert_eq!(instance.current_identity().process_generation, 2);
    let output = timeout(Duration::from_secs(3), instance.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(output.failure.is_none());
    assert_eq!(output.identity.process_generation, 2);
    assert_eq!(
        std::fs::read_to_string(directory.path().join("launches")).unwrap(),
        "2"
    );
    application_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn application_restart_backoff_reserves_capacity_and_is_cancellable() {
    use dragon_server::application::{ApplicationManager, InstanceState, RestartPolicy};
    let manager = ApplicationManager::new(1).unwrap();
    for operation in ["stop", "drop", "cancel"] {
        let directory = tempfile::tempdir().unwrap();
        let mut budget = restart_budget();
        budget.initial_delay = Duration::from_secs(10);
        budget.max_delay = Duration::from_secs(10);
        let instance = manager
            .start_with_restart(
                application_spec("restart-crash", directory.path()),
                RestartPolicy::Always(budget),
            )
            .unwrap();
        let mut states = instance.subscribe();
        timeout(
            Duration::from_secs(3),
            states.wait_for(|state| *state == InstanceState::Backoff),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            manager
                .start(application_spec("output", directory.path()))
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
        match operation {
            "stop" => {
                let output = timeout(Duration::from_secs(1), instance.stop())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(output.failure.is_none());
                assert_eq!(output.identity.process_generation, 1);
            }
            "drop" => drop(instance),
            _ => assert!(
                timeout(Duration::from_millis(10), instance.wait())
                    .await
                    .is_err()
            ),
        }
        application_capacity_recovers(&manager, directory.path()).await;
        assert_eq!(*states.borrow(), InstanceState::Stopped);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("launches")).unwrap(),
            "1"
        );
        let address = std::fs::read_to_string(directory.path().join("leaf-address")).unwrap();
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
}

#[tokio::test]
async fn application_restart_modes_distinguish_successful_exit() {
    use dragon_server::application::{
        ApplicationManager, InstanceFailure, InstanceState, RestartPolicy,
    };
    let manager = ApplicationManager::new(1).unwrap();
    for (policy, launches, failure, state) in [
        (
            RestartPolicy::Never,
            "1",
            Some(InstanceFailure::ProcessExited),
            InstanceState::Failed,
        ),
        (
            RestartPolicy::OnFailure(restart_budget()),
            "1",
            None,
            InstanceState::Stopped,
        ),
        (
            RestartPolicy::Always(restart_budget()),
            "3",
            Some(InstanceFailure::RestartLimit),
            InstanceState::Failed,
        ),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let instance = manager
            .start_with_restart(
                application_spec("restart-success", directory.path()),
                policy,
            )
            .unwrap();
        let states = instance.subscribe();
        let output = timeout(Duration::from_secs(4), instance.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.failure, failure);
        assert_eq!(*states.borrow(), state);
        assert!(output.process.status.success());
        assert_eq!(
            std::fs::read_to_string(directory.path().join("launches")).unwrap(),
            launches
        );
    }
}

#[tokio::test]
async fn application_restart_spawn_failure_is_terminal() {
    use dragon_server::application::{
        ApplicationManager, InstanceFailure, InstanceState, RestartPolicy,
    };
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    let executable = directory.path().join("executable");
    std::os::unix::fs::symlink(std::env::current_exe().unwrap(), &executable).unwrap();
    let mut spec = application_spec("restart-crash", directory.path());
    spec.process.executable = executable.clone();
    let mut budget = restart_budget();
    budget.initial_delay = Duration::from_millis(300);
    budget.max_delay = budget.initial_delay;
    let instance = manager
        .start_with_restart(spec, RestartPolicy::Always(budget))
        .unwrap();
    let mut states = instance.subscribe();
    timeout(
        Duration::from_secs(3),
        states.wait_for(|state| *state == InstanceState::Backoff),
    )
    .await
    .unwrap()
    .unwrap();
    std::fs::remove_file(executable).unwrap();
    let output = timeout(Duration::from_secs(3), instance.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.failure, Some(InstanceFailure::RestartSpawnFailed));
    assert!(output.restart_error.is_some());
    assert_eq!(output.identity.process_generation, 1);
    assert_eq!(output.process.status.code(), Some(7));
    assert_eq!(*states.borrow(), InstanceState::Failed);
    application_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn application_restart_rejects_invalid_policy_before_spawn() {
    use dragon_server::application::{ApplicationManager, RestartPolicy};
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    for invalid in 0..5 {
        let mut budget = restart_budget();
        match invalid {
            0 => budget.max_restarts = 0,
            1 => budget.max_restarts = 1001,
            2 => budget.initial_delay = Duration::ZERO,
            3 => budget.max_delay = Duration::from_secs(301),
            _ => budget.max_delay = Duration::from_millis(1),
        }
        assert_eq!(
            manager
                .start_with_restart(
                    application_spec("output", directory.path()),
                    RestartPolicy::OnFailure(budget)
                )
                .err()
                .unwrap()
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!directory.path().join("cwd-marker").exists());
    }
    application_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn application_restart_readiness_failures_consume_budget() {
    use dragon_server::application::{ApplicationManager, InstanceFailure, RestartPolicy};
    let manager = ApplicationManager::new(1).unwrap();
    for deadline in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut spec = application_spec("restart-unready", directory.path());
        if deadline {
            spec.readiness.startup_timeout = Duration::from_millis(150);
        } else {
            spec.readiness.max_attempts = 15;
        }
        let output = timeout(
            Duration::from_secs(5),
            manager
                .start_with_restart(spec, RestartPolicy::OnFailure(restart_budget()))
                .unwrap()
                .wait(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(output.failure, Some(InstanceFailure::RestartLimit));
        assert_eq!(output.identity.process_generation, 3);
        assert_eq!(
            std::fs::read_to_string(directory.path().join("launches")).unwrap(),
            "3"
        );
        assert!(directory.path().join("probe-started").exists());
        application_capacity_recovers(&manager, directory.path()).await;
    }
}

#[tokio::test]
async fn application_restart_reaps_exited_leaders_before_replacement() {
    use dragon_server::application::{
        ApplicationManager, InstanceFailure, InstanceState, RestartPolicy,
    };
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    let instance = manager
        .start_with_restart(
            application_spec("output", directory.path()),
            RestartPolicy::Always(restart_budget()),
        )
        .unwrap();
    let states = instance.subscribe();
    let output = timeout(Duration::from_secs(4), instance.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(output.process.group_cleanup_error.is_none());
    assert_eq!(output.failure, Some(InstanceFailure::RestartLimit));
    assert_eq!(output.identity.process_generation, 3);
    assert_eq!(*states.borrow(), InstanceState::Failed);
    application_capacity_recovers(&manager, directory.path()).await;
}

async fn application_capacity_recovers(
    manager: &dragon_server::application::ApplicationManager,
    directory: &std::path::Path,
) {
    timeout(Duration::from_secs(4), async {
        loop {
            match manager.start(application_spec("output", directory)) {
                Ok(instance) => {
                    assert_eq!(
                        instance.wait().await.unwrap().process.status.code(),
                        Some(7)
                    );
                    return;
                }
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn application_startup_deadline_and_probe_budget_clean_up() {
    use dragon_server::application::{ApplicationManager, InstanceFailure, InstanceState};
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    let mut spec = application_spec("http-hang", directory.path());
    spec.readiness.startup_timeout = Duration::from_millis(200);
    spec.readiness.probe_timeout = Duration::from_millis(200);
    let address = spec.readiness.address;
    let instance = manager.start(spec).unwrap();
    let states = instance.subscribe();
    let output = timeout(Duration::from_secs(3), instance.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.failure, Some(InstanceFailure::StartupTimeout));
    assert_eq!(*states.borrow(), InstanceState::Failed);
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    application_capacity_recovers(&manager, directory.path()).await;

    let mut spec = application_spec("hold", directory.path());
    spec.readiness.max_attempts = 2;
    let output = timeout(Duration::from_secs(3), manager.start(spec).unwrap().wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.failure, Some(InstanceFailure::ProbeLimit));
    application_capacity_recovers(&manager, directory.path()).await;

    let directory = tempfile::tempdir().unwrap();
    let mut spec = application_spec("http-hang", directory.path());
    spec.readiness.probe_timeout = Duration::from_millis(40);
    spec.readiness.max_attempts = 5;
    let output = timeout(Duration::from_secs(2), manager.start(spec).unwrap().wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.failure, Some(InstanceFailure::ProbeLimit));
    assert!(directory.path().join("probe-started").exists());
    application_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn application_rejects_bad_probe_responses() {
    use dragon_server::application::{ApplicationManager, InstanceFailure, InstanceState};
    let manager = ApplicationManager::new(1).unwrap();
    for mode in [
        "http-ready",
        "http-redirect",
        "http-oversized",
        "http-malformed",
        "http-truncated",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut spec = application_spec(mode, directory.path());
        spec.readiness.max_attempts = 30;
        let mut instance = manager.start(spec).unwrap();
        let states = instance.subscribe();
        assert!(
            timeout(Duration::from_secs(3), instance.wait_ready())
                .await
                .unwrap()
                .is_err()
        );
        let output = timeout(Duration::from_secs(3), instance.wait())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(output.failure, Some(InstanceFailure::ProbeLimit), "{mode}");
        assert_eq!(*states.borrow(), InstanceState::Failed);
        assert!(
            directory.path().join("probe-started").exists(),
            "{mode} was not probed"
        );
    }
}

#[tokio::test]
async fn application_exit_before_and_after_readiness_is_failed() {
    use dragon_server::application::{ApplicationManager, InstanceFailure, InstanceState};
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    let mut instance = manager
        .start(application_spec("output", directory.path()))
        .unwrap();
    assert!(instance.wait_ready().await.is_err());
    let output = instance.wait().await.unwrap();
    assert_eq!(output.failure, Some(InstanceFailure::ProcessExited));
    assert_eq!(output.process.status.code(), Some(7));

    std::fs::write(directory.path().join("allow-ready"), "ready").unwrap();
    let spec = application_spec("http-ready", directory.path());
    let address = spec.readiness.address;
    let mut instance = manager.start(spec).unwrap();
    let mut states = instance.subscribe();
    timeout(Duration::from_secs(3), instance.wait_ready())
        .await
        .unwrap()
        .unwrap();
    std::fs::write(directory.path().join("exit-now"), "exit").unwrap();
    let _socket = tokio::net::TcpStream::connect(address).await.unwrap();
    timeout(Duration::from_secs(3), async {
        while *states.borrow_and_update() != InstanceState::Failed {
            states.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let output = instance.wait().await.unwrap();
    assert_eq!(output.failure, Some(InstanceFailure::ProcessExited));
    assert_eq!(output.process.status.code(), Some(9));
}

#[tokio::test]
async fn application_stop_drop_and_cancel_interrupt_probe_and_recover_capacity() {
    use dragon_server::application::{ApplicationManager, InstanceState};
    let manager = ApplicationManager::new(1).unwrap();
    for operation in ["stop", "drop", "cancel"] {
        let directory = tempfile::tempdir().unwrap();
        let mut spec = application_spec("http-hang", directory.path());
        spec.readiness.startup_timeout = Duration::from_secs(30);
        spec.readiness.probe_timeout = Duration::from_secs(30);
        let address = spec.readiness.address;
        let instance = manager.start(spec).unwrap();
        let states = instance.subscribe();
        timeout(Duration::from_secs(3), async {
            while !directory.path().join("probe-started").exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        match operation {
            "stop" => {
                let output = timeout(Duration::from_secs(3), instance.stop())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(output.failure.is_none());
            }
            "drop" => drop(instance),
            _ => assert!(
                timeout(Duration::from_millis(10), instance.wait())
                    .await
                    .is_err()
            ),
        }
        application_capacity_recovers(&manager, directory.path()).await;
        assert!(matches!(
            *states.borrow(),
            InstanceState::Stopped | InstanceState::Failed
        ));
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }
}

#[tokio::test]
async fn application_invalid_spec_has_no_execution_side_effects() {
    use dragon_server::application::ApplicationManager;
    let directory = tempfile::tempdir().unwrap();
    let manager = ApplicationManager::new(1).unwrap();
    for invalid in 0..10 {
        let mut spec = application_spec("output", directory.path());
        match invalid {
            0 => spec.application_id.clear(),
            1 => spec.release_id = "../release".into(),
            2 => spec.readiness.address = "192.0.2.1:80".parse().unwrap(),
            3 => spec.readiness.address.set_port(0),
            4 => spec.readiness.path = "/\r\nInjected: yes".into(),
            5 => spec.readiness.path = "//other/ready".into(),
            6 => spec.readiness.max_attempts = 0,
            7 => spec.readiness.interval = Duration::ZERO,
            8 => spec.readiness.startup_timeout = Duration::from_secs(301),
            _ => spec.readiness.probe_timeout = Duration::from_secs(4),
        }
        assert_eq!(
            manager.start(spec).err().unwrap().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert!(!directory.path().join("cwd-marker").exists());
    }
    application_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn process_captures_bounded_output_and_preserves_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ProcessManager::new(1).unwrap();
    let child = manager.spawn(fixture("output", directory.path())).unwrap();
    let output = timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(7));
    assert!(output.group_cleanup_error.is_none());
    assert_eq!(output.stdout, vec![b'o'; 32_768]);
    assert_eq!(output.stderr, vec![b'e'; 32_768]);
    assert!(output.stdout_truncated && output.stderr_truncated && output.output_complete);
    assert!(directory.path().join("cwd-marker").is_file());
    assert!(!directory.path().join("injected").exists());
    assert!(!directory.path().join("argv-injected").exists());
}

#[tokio::test]
async fn process_limits_stop_and_handle_drop_recover_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ProcessManager::new(1).unwrap();
    let child = manager.spawn(fixture("hold", directory.path())).unwrap();
    started(directory.path()).await;
    assert_eq!(
        manager
            .spawn(fixture("hold", directory.path()))
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::WouldBlock
    );
    let output = timeout(Duration::from_secs(3), child.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    let child = manager.spawn(fixture("hold", directory.path())).unwrap();
    drop(child);
    assert_capacity_recovers(&manager, directory.path()).await;
}

async fn assert_capacity_recovers(manager: &ProcessManager, directory: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match manager.spawn(fixture("output", directory)) {
            Ok(child) => {
                assert_eq!(
                    timeout(Duration::from_secs(3), child.wait())
                        .await
                        .unwrap()
                        .unwrap()
                        .status
                        .code(),
                    Some(7)
                );
                break;
            }
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock),
        }
        assert!(
            Instant::now() < deadline,
            "dropped child did not release capacity"
        );
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn process_cancelled_wait_still_cleans_up_child() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ProcessManager::new(1).unwrap();
    let child = manager.spawn(fixture("hold", directory.path())).unwrap();
    started(directory.path()).await;
    assert!(
        timeout(Duration::from_millis(20), child.wait())
            .await
            .is_err()
    );
    assert_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn process_rejects_invalid_launches_without_losing_capacity() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ProcessManager::new(1).unwrap();
    assert!(ProcessManager::new(0).is_err());
    assert!(ProcessManager::new(1025).is_err());
    assert!(ProcessManager::with_shutdown_timeout(1, Duration::ZERO).is_err());
    assert!(ProcessManager::with_shutdown_timeout(1, Duration::from_secs(61)).is_err());
    let mut spec = fixture("output", directory.path());
    spec.executable = "relative-executable".into();
    assert_eq!(
        manager.spawn(spec).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
    let mut spec = fixture("output", directory.path());
    spec.executable = directory.path().join("missing");
    assert_eq!(
        manager.spawn(spec).err().unwrap().kind(),
        std::io::ErrorKind::NotFound
    );
    let mut spec = fixture("output", directory.path());
    spec.args.push("x".repeat(1_048_577).into());
    assert_eq!(
        manager.spawn(spec).err().unwrap().kind(),
        std::io::ErrorKind::InvalidInput
    );
    let mut spec = fixture("output", directory.path());
    spec.args.push("invalid\0argument".into());
    assert!(manager.spawn(spec).is_err());
    assert_eq!(
        manager
            .spawn(fixture("output", directory.path()))
            .unwrap()
            .wait()
            .await
            .unwrap()
            .status
            .code(),
        Some(7)
    );
}

#[tokio::test]
async fn process_graceful_shutdown_preserves_application_exit() {
    let directory = tempfile::tempdir().unwrap();
    let manager = ProcessManager::new(1).unwrap();
    let child = manager
        .spawn(fixture("graceful", directory.path()))
        .unwrap();
    started(directory.path()).await;
    let output = timeout(Duration::from_secs(3), child.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(output.status.success());
    assert!(!output.shutdown_escalated);
    assert!(
        output.group_cleanup_error.is_none(),
        "graceful cleanup failed: {:?}",
        output.group_cleanup_error
    );
    assert!(output.output_complete);
    assert_eq!(
        std::fs::read(directory.path().join("graceful-stop")).unwrap(),
        b"flushed"
    );
    assert_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn process_shutdown_deadline_forces_uncooperative_child() {
    use std::os::unix::process::ExitStatusExt;
    let directory = tempfile::tempdir().unwrap();
    let grace = Duration::from_millis(80);
    let manager = ProcessManager::with_shutdown_timeout(1, grace).unwrap();
    let child = manager
        .spawn(fixture("stubborn", directory.path()))
        .unwrap();
    started(directory.path()).await;
    let beginning = Instant::now();
    let output = timeout(Duration::from_secs(3), child.stop())
        .await
        .unwrap()
        .unwrap();
    assert!(beginning.elapsed() >= grace);
    assert_eq!(output.status.signal(), Some(9));
    assert!(output.shutdown_escalated);
    assert!(output.group_cleanup_error.is_none());
    assert!(output.output_complete);
    assert_capacity_recovers(&manager, directory.path()).await;
}

#[tokio::test]
async fn process_cleans_descendants_on_stop_drop_and_leader_exit() {
    for action in ["stop", "drop", "exit"] {
        let directory = tempfile::tempdir().unwrap();
        let manager = ProcessManager::with_shutdown_timeout(1, Duration::from_millis(80)).unwrap();
        let mode = if action == "exit" {
            "tree-exit"
        } else {
            "tree"
        };
        let child = manager.spawn(fixture(mode, directory.path())).unwrap();
        started(directory.path()).await;
        let address = std::fs::read_to_string(directory.path().join("leaf-address")).unwrap();
        if action != "exit" {
            tokio::net::TcpStream::connect(&address).await.unwrap();
        }
        match action {
            "drop" => drop(child),
            "stop" => {
                let output = timeout(Duration::from_secs(3), child.stop())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(output.shutdown_escalated);
                assert!(output.group_cleanup_error.is_none());
                assert!(output.output_complete);
            }
            _ => {
                let output = timeout(Duration::from_secs(3), child.wait())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(output.status.success());
                assert!(!output.shutdown_escalated);
                assert!(output.output_complete);
            }
        }
        assert_capacity_recovers(&manager, directory.path()).await;
        assert!(
            tokio::net::TcpStream::connect(&address).await.is_err(),
            "descendant survived {action}"
        );
    }
}
