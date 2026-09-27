use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    process::Command,
    time::timeout,
};

fn fixture(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("public")).unwrap();
    let path = directory.path().join("dragon.toml");
    std::fs::write(&path, text).unwrap();
    (directory, path)
}

#[tokio::test]
async fn reports_version_config_errors_and_bind_conflicts() {
    let version = Command::new(env!("CARGO_BIN_EXE_dragon"))
        .arg("--version")
        .output()
        .await
        .unwrap();
    assert!(version.status.success());
    assert!(String::from_utf8_lossy(&version.stdout).contains("0.1.0"));
    let (_directory, path) = fixture("schema_version = 9\n");
    let invalid = Command::new(env!("CARGO_BIN_EXE_dragon"))
        .args(["start", "--config"])
        .arg(path)
        .output()
        .await
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("configuration error"));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let text = include_str!("../examples/minimal/dragon.toml").replace(
        "127.0.0.1:8080",
        &listener.local_addr().unwrap().to_string(),
    );
    let (_directory, path) = fixture(&text);
    let failed = timeout(
        Duration::from_secs(3),
        Command::new(env!("CARGO_BIN_EXE_dragon"))
            .kill_on_drop(true)
            .args(["start", "--config"])
            .arg(path)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&failed.stderr).contains("server error"));
}

#[cfg(unix)]
#[tokio::test]
async fn handles_sigterm_and_omits_sensitive_request_data_from_logs() {
    let text =
        include_str!("../examples/minimal/dragon.toml").replace("127.0.0.1:8080", "127.0.0.1:0");
    let (_directory, path) = fixture(&text);
    let mut child = Command::new(env!("CARGO_BIN_EXE_dragon"))
        .args(["start", "--config"])
        .arg(path)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut first = String::new();
    timeout(Duration::from_secs(3), stderr.read_line(&mut first))
        .await
        .unwrap()
        .unwrap();
    let event: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(event["fields"]["event"], "server_started");
    let address = event["fields"]["listen"].as_str().unwrap();
    let mut socket = TcpStream::connect(address).await.unwrap();
    socket.write_all(b"GET /hello?secret=do-not-log HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer confidential\r\nCookie: secret-cookie\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    timeout(Duration::from_secs(2), socket.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert!(response.starts_with(b"HTTP/1.1 200"));
    let pid = rustix::process::Pid::from_raw(child.id().unwrap() as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::TERM).unwrap();
    let exit = timeout(Duration::from_secs(3), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(exit.success());
    let mut logs = String::new();
    stderr.read_to_string(&mut logs).await.unwrap();
    assert!(logs.contains("server_stopped"));
    assert!(logs.contains("request_finished"));
    for secret in ["do-not-log", "confidential", "secret-cookie"] {
        assert!(!logs.contains(secret));
    }
    assert!(TcpStream::connect(address).await.is_err());
}
