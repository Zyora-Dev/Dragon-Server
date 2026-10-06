use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Semaphore, oneshot, watch},
    task::JoinHandle,
    time::{sleep, timeout},
};

use crate::process::{ProcessHandle, ProcessManager, ProcessOutput, ProcessSpec};

static NEXT_INSTANCE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstanceIdentity {
    pub application_id: String,
    pub release_id: String,
    pub instance_id: u64,
    pub process_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceState {
    Starting,
    Running,
    Ready,
    Restarting,
    Backoff,
    Stopping,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceFailure {
    StartupTimeout,
    ProbeLimit,
    ProcessExited,
    RestartLimit,
    RestartSpawnFailed,
}

#[derive(Clone, Copy, Debug)]
pub struct RestartBudget {
    pub max_restarts: u32,
    pub initial_delay: Duration,
    pub max_delay: Duration,
}

#[derive(Clone, Copy, Debug, Default)]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure(RestartBudget),
    Always(RestartBudget),
}

impl RestartPolicy {
    fn budget(self) -> Option<RestartBudget> {
        match self {
            Self::Never => None,
            Self::OnFailure(budget) | Self::Always(budget) => Some(budget),
        }
    }

    pub(crate) fn validate(self) -> io::Result<()> {
        if let Some(budget) = self.budget()
            && (!(1..=1000).contains(&budget.max_restarts)
                || budget.initial_delay < Duration::from_millis(1)
                || budget.max_delay > Duration::from_secs(300)
                || budget.initial_delay > budget.max_delay)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid restart policy",
            ));
        }
        Ok(())
    }
}

pub struct ReadinessSpec {
    pub address: SocketAddr,
    pub path: String,
    pub startup_timeout: Duration,
    pub probe_timeout: Duration,
    pub interval: Duration,
    pub max_attempts: u32,
}

pub struct ApplicationSpec {
    pub application_id: String,
    pub release_id: String,
    pub process: ProcessSpec,
    pub readiness: ReadinessSpec,
}

pub struct ApplicationManager {
    processes: Arc<ProcessManager>,
    slots: Arc<Semaphore>,
}

pub struct ApplicationHandle {
    identity: InstanceIdentity,
    state: watch::Receiver<InstanceState>,
    generation: watch::Receiver<u64>,
    stop: Option<oneshot::Sender<()>>,
    completion: JoinHandle<io::Result<ApplicationOutput>>,
}

pub struct ApplicationOutput {
    pub identity: InstanceIdentity,
    pub process: ProcessOutput,
    pub failure: Option<InstanceFailure>,
    pub restart_error: Option<io::Error>,
}

struct Lifecycle {
    stopped: oneshot::Receiver<()>,
    state: watch::Sender<InstanceState>,
    generation: watch::Sender<u64>,
}

impl ReadinessSpec {
    pub(crate) fn validate(&self) -> io::Result<()> {
        let valid_duration = |value: Duration| {
            (Duration::from_millis(1)..=Duration::from_secs(300)).contains(&value)
        };
        if !self.address.ip().is_loopback()
            || self.address.port() == 0
            || !self.path.starts_with('/')
            || self.path.starts_with("//")
            || self.path.len() > 2048
            || !self.path.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
            || self.path.contains(['#', '\\'])
            || !valid_duration(self.startup_timeout)
            || !valid_duration(self.probe_timeout)
            || !valid_duration(self.interval)
            || self.probe_timeout > self.startup_timeout
            || !(1..=1000).contains(&self.max_attempts)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid readiness policy",
            ));
        }
        Ok(())
    }

    async fn probe(&self) -> io::Result<bool> {
        let mut socket = TcpStream::connect(self.address).await?;
        let request = format!(
            "HEAD {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            self.path, self.address
        );
        socket.write_all(request.as_bytes()).await?;
        let mut buffer = [0; 8192];
        let mut used = 0;
        loop {
            let count = socket.read(&mut buffer[used..]).await?;
            if count == 0 {
                return Ok(false);
            }
            used += count;
            let mut headers = [httparse::EMPTY_HEADER; 64];
            let mut response = httparse::Response::new(&mut headers);
            match response.parse(&buffer[..used]) {
                Ok(httparse::Status::Complete(_)) => return Ok(response.code == Some(200)),
                Ok(httparse::Status::Partial) if used < buffer.len() => {}
                _ => return Ok(false),
            }
        }
    }

    async fn wait(&self) -> Result<(), InstanceFailure> {
        let attempts = async {
            for attempt in 0..self.max_attempts {
                if matches!(
                    timeout(self.probe_timeout, self.probe()).await,
                    Ok(Ok(true))
                ) {
                    return Ok(());
                }
                if attempt + 1 < self.max_attempts {
                    sleep(self.interval).await;
                }
            }
            Err(InstanceFailure::ProbeLimit)
        };
        timeout(self.startup_timeout, attempts)
            .await
            .unwrap_or(Err(InstanceFailure::StartupTimeout))
    }
}

impl ApplicationManager {
    pub fn new(max_instances: usize) -> io::Result<Self> {
        Ok(Self {
            processes: Arc::new(ProcessManager::new(max_instances)?),
            slots: Arc::new(Semaphore::new(max_instances)),
        })
    }

    pub fn start(&self, spec: ApplicationSpec) -> io::Result<ApplicationHandle> {
        self.start_with_restart(spec, RestartPolicy::Never)
    }

    pub fn start_with_restart(
        &self,
        spec: ApplicationSpec,
        policy: RestartPolicy,
    ) -> io::Result<ApplicationHandle> {
        policy.validate()?;
        for name in [&spec.application_id, &spec.release_id] {
            if name.is_empty()
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid application or release ID",
                ));
            }
        }
        spec.readiness.validate()?;
        let slot = self.slots.clone().try_acquire_owned().map_err(|_| {
            io::Error::new(io::ErrorKind::WouldBlock, "application capacity exhausted")
        })?;
        let instance_id = NEXT_INSTANCE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| io::Error::other("instance identity exhausted"))?;
        let identity = InstanceIdentity {
            application_id: spec.application_id.clone(),
            release_id: spec.release_id.clone(),
            instance_id,
            process_generation: 1,
        };
        let child = self.processes.spawn(spec.process.clone())?;
        let (state_sender, state) = watch::channel(InstanceState::Starting);
        let (generation_sender, generation) = watch::channel(1);
        let (stop, stopped) = oneshot::channel();
        let instance = identity.clone();
        let processes = self.processes.clone();
        let completion = tokio::spawn(async move {
            let _slot = slot;
            supervise(
                child,
                instance,
                spec,
                policy,
                processes,
                Lifecycle {
                    stopped,
                    state: state_sender,
                    generation: generation_sender,
                },
            )
            .await
        });
        Ok(ApplicationHandle {
            identity,
            state,
            generation,
            stop: Some(stop),
            completion,
        })
    }
}

async fn supervise(
    mut child: ProcessHandle,
    mut identity: InstanceIdentity,
    spec: ApplicationSpec,
    policy: RestartPolicy,
    processes: Arc<ProcessManager>,
    lifecycle: Lifecycle,
) -> io::Result<ApplicationOutput> {
    let Lifecycle {
        mut stopped,
        state,
        generation,
    } = lifecycle;
    let mut restarts = 0;
    let mut delay = policy
        .budget()
        .map_or(Duration::ZERO, |budget| budget.initial_delay);
    loop {
        state.send_replace(InstanceState::Running);
        let mut failure = tokio::select! {
            biased;
            _ = &mut stopped => None,
            _ = child.exited() => Some(InstanceFailure::ProcessExited),
            result = spec.readiness.wait() => match result {
                Err(failure) => Some(failure),
                Ok(()) => {
                    state.send_replace(InstanceState::Ready);
                    tokio::select! {
                        biased;
                        _ = &mut stopped => None,
                        _ = child.exited() => Some(InstanceFailure::ProcessExited),
                    }
                }
            },
        };
        state.send_replace(if failure.is_some() {
            if policy.budget().is_some() {
                InstanceState::Restarting
            } else {
                InstanceState::Failed
            }
        } else {
            InstanceState::Stopping
        });
        let process = match child.stop().await {
            Ok(process) => process,
            Err(error) => {
                state.send_replace(InstanceState::Failed);
                return Err(error);
            }
        };
        let clean = process.group_cleanup_error.is_none();
        if !matches!(stopped.try_recv(), Err(oneshot::error::TryRecvError::Empty))
            || (matches!(policy, RestartPolicy::OnFailure(_))
                && failure == Some(InstanceFailure::ProcessExited)
                && process.status.success())
        {
            failure = None;
        }
        if clean
            && failure.is_some()
            && let Some(budget) = policy.budget()
        {
            if restarts == budget.max_restarts {
                failure = Some(InstanceFailure::RestartLimit);
            } else {
                state.send_replace(InstanceState::Backoff);
                tokio::select! {
                    biased;
                    _ = &mut stopped => failure = None,
                    _ = sleep(delay) => {},
                }
                if !matches!(stopped.try_recv(), Err(oneshot::error::TryRecvError::Empty)) {
                    failure = None;
                }
                if failure.is_some() {
                    match processes.spawn(spec.process.clone()) {
                        Ok(next) => {
                            child = next;
                            restarts += 1;
                            identity.process_generation += 1;
                            generation.send_replace(identity.process_generation);
                            delay = delay.saturating_mul(2).min(budget.max_delay);
                            continue;
                        }
                        Err(error) => {
                            state.send_replace(InstanceState::Failed);
                            return Ok(ApplicationOutput {
                                identity,
                                process,
                                failure: Some(InstanceFailure::RestartSpawnFailed),
                                restart_error: Some(error),
                            });
                        }
                    }
                }
            }
        }
        state.send_replace(if failure.is_none() && clean {
            InstanceState::Stopped
        } else {
            InstanceState::Failed
        });
        return Ok(ApplicationOutput {
            identity,
            process,
            failure,
            restart_error: None,
        });
    }
}

impl ApplicationHandle {
    pub fn identity(&self) -> &InstanceIdentity {
        &self.identity
    }

    pub fn current_identity(&self) -> InstanceIdentity {
        let mut identity = self.identity.clone();
        identity.process_generation = *self.generation.borrow();
        identity
    }

    pub fn subscribe(&self) -> watch::Receiver<InstanceState> {
        self.state.clone()
    }

    pub fn state(&self) -> InstanceState {
        *self.state.borrow()
    }

    pub async fn wait_ready(&mut self) -> io::Result<()> {
        loop {
            match *self.state.borrow_and_update() {
                InstanceState::Ready => return Ok(()),
                InstanceState::Starting
                | InstanceState::Running
                | InstanceState::Restarting
                | InstanceState::Backoff => {}
                _ => {
                    return Err(io::Error::other(
                        "instance stopped or failed before readiness",
                    ));
                }
            }
            self.state.changed().await.map_err(io::Error::other)?;
        }
    }

    pub async fn wait(mut self) -> io::Result<ApplicationOutput> {
        (&mut self.completion).await.map_err(io::Error::other)?
    }

    pub async fn stop(mut self) -> io::Result<ApplicationOutput> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        (&mut self.completion).await.map_err(io::Error::other)?
    }
}

impl Drop for ApplicationHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
