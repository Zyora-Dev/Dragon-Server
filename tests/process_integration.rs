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
    assert_eq!(
        *states.borrow(),
        if output.process.group_cleanup_error.is_some() {
            InstanceState::Failed
        } else {
            InstanceState::Stopped
        }
    );
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
async fn application_restart_stops_on_reported_cleanup_error() {
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
    if output.process.group_cleanup_error.is_some() {
        assert_eq!(output.failure, Some(InstanceFailure::ProcessExited));
        assert!(output.identity.process_generation <= 3);
    } else {
        assert_eq!(output.failure, Some(InstanceFailure::RestartLimit));
        assert_eq!(output.identity.process_generation, 3);
    }
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
