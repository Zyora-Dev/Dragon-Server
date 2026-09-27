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

#[test]
#[ignore = "native child fixture invoked explicitly by process integration tests"]
fn child_fixture() {
    let Ok(mode) = std::env::var("DRAGON_FIXTURE_MODE") else {
        return;
    };
    match mode.as_str() {
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
