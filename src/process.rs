use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    io,
    path::PathBuf,
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
    signal::unix::{Signal as ChildSignal, SignalKind, signal},
    sync::{Semaphore, oneshot},
    task::JoinHandle,
    time::timeout,
};

const OUTPUT_LIMIT: usize = 32_768;

pub struct ProcessSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: BTreeMap<OsString, OsString>,
}

pub struct ProcessManager {
    slots: Arc<Semaphore>,
    shutdown_timeout: Duration,
}

pub struct ProcessHandle {
    stop: Option<oneshot::Sender<()>>,
    completion: JoinHandle<io::Result<ProcessOutput>>,
}

pub struct ProcessOutput {
    pub status: ExitStatus,
    pub shutdown_escalated: bool,
    pub group_cleanup_error: Option<io::Error>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub output_complete: bool,
}

#[derive(Default)]
struct Tail {
    bytes: VecDeque<u8>,
    truncated: bool,
}

struct Capture {
    tail: Arc<Mutex<Tail>>,
    task: JoinHandle<io::Result<()>>,
}

struct OwnedProcess {
    child: Child,
    group: Pid,
    changed: ChildSignal,
    armed: bool,
}

impl OwnedProcess {
    fn signal(&self, signal: Signal) -> io::Result<()> {
        match kill_process_group(self.group, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(io::Error::new(
                error.kind(),
                format!("signal process group with {signal:?}: {error}"),
            )),
        }
    }

    async fn exited(&mut self) -> io::Result<()> {
        loop {
            match waitid(
                WaitId::Pid(self.group),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            ) {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(rustix::io::Errno::CHILD) => {
                    self.armed = false;
                    return Err(io::Error::other(
                        "managed child was reaped outside its owner",
                    ));
                }
                Err(error) => {
                    return Err(io::Error::new(
                        error.kind(),
                        format!("observe child exit without reaping: {error}"),
                    ));
                }
            }
            self.changed
                .recv()
                .await
                .ok_or_else(|| io::Error::other("child signal stream closed"))?;
        }
    }

    async fn supervise(
        mut self,
        stopped: oneshot::Receiver<()>,
        grace: Duration,
    ) -> io::Result<(ExitStatus, bool, Option<io::Error>)> {
        let mut escalated = false;
        tokio::select! {
            result = self.exited() => result?,
            _ = stopped => {
                self.signal(Signal::TERM)?;
                match timeout(grace, self.exited()).await {
                    Ok(result) => result?,
                    Err(_) => escalated = true,
                }
            }
        }
        let group_cleanup_error = self.signal(Signal::KILL).err();
        if group_cleanup_error.is_some() {
            self.child.start_kill()?;
        }
        self.armed = false;
        let status = self.child.wait().await?;
        Ok((status, escalated, group_cleanup_error))
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.signal(Signal::KILL);
        }
    }
}

impl Capture {
    fn start(mut pipe: impl AsyncRead + Unpin + Send + 'static) -> Self {
        let tail = Arc::new(Mutex::new(Tail::default()));
        let sink = tail.clone();
        let task = tokio::spawn(async move {
            let mut buffer = [0u8; 4096];
            loop {
                let count = pipe.read(&mut buffer).await?;
                if count == 0 {
                    return Ok(());
                }
                let mut tail = sink.lock().expect("output lock poisoned");
                let excess = (tail.bytes.len() + count).saturating_sub(OUTPUT_LIMIT);
                if excess != 0 {
                    tail.bytes.drain(..excess);
                    tail.truncated = true;
                }
                tail.bytes.extend(&buffer[..count]);
            }
        });
        Self { tail, task }
    }

    async fn finish(&mut self) -> (Vec<u8>, bool, bool) {
        let complete = match timeout(Duration::from_secs(1), &mut self.task).await {
            Ok(Ok(Ok(()))) => true,
            Ok(_) => false,
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
                false
            }
        };
        let tail = self.tail.lock().expect("output lock poisoned");
        (
            tail.bytes.iter().copied().collect(),
            tail.truncated,
            complete,
        )
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ProcessManager {
    pub fn new(max_children: usize) -> io::Result<Self> {
        Self::with_shutdown_timeout(max_children, Duration::from_secs(1))
    }

    pub fn with_shutdown_timeout(
        max_children: usize,
        shutdown_timeout: Duration,
    ) -> io::Result<Self> {
        if !(1..=1024).contains(&max_children) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_children must be 1..=1024",
            ));
        }
        if shutdown_timeout < Duration::from_millis(1) || shutdown_timeout > Duration::from_secs(60)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "shutdown timeout must be between 1 ms and 60 seconds",
            ));
        }
        Ok(Self {
            slots: Arc::new(Semaphore::new(max_children)),
            shutdown_timeout,
        })
    }

    pub fn spawn(&self, spec: ProcessSpec) -> io::Result<ProcessHandle> {
        tokio::runtime::Handle::try_current()
            .map_err(|_| io::Error::other("process manager requires an active Tokio runtime"))?;
        if !spec.executable.is_absolute() || !spec.cwd.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "executable and cwd must be absolute",
            ));
        }
        if !spec.executable.is_file() || !spec.cwd.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "executable file or working directory missing",
            ));
        }
        let size = spec.args.iter().map(|value| value.len()).sum::<usize>()
            + spec
                .env
                .iter()
                .map(|(key, value)| key.len() + value.len())
                .sum::<usize>();
        if spec.args.len() > 4096 || spec.env.len() > 4096 || size > 1_048_576 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "launch arguments or environment exceed limits",
            ));
        }
        let permit = self.slots.clone().try_acquire_owned().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "managed child capacity exhausted",
            )
        })?;
        let changed = signal(SignalKind::child())?;
        let child = Command::new(spec.executable)
            .args(spec.args)
            .current_dir(spec.cwd)
            .env_clear()
            .envs(spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0)
            .spawn()?;
        let group =
            Pid::from_raw(child.id().expect("spawned child ID") as i32).expect("positive child ID");
        let mut owned = OwnedProcess {
            child,
            group,
            changed,
            armed: true,
        };
        let mut stdout = Capture::start(owned.child.stdout.take().expect("piped stdout"));
        let mut stderr = Capture::start(owned.child.stderr.take().expect("piped stderr"));
        let (stop, stopped) = oneshot::channel();
        let grace = self.shutdown_timeout;
        let completion = tokio::spawn(async move {
            let _permit = permit;
            let (status, shutdown_escalated, group_cleanup_error) =
                owned.supervise(stopped, grace).await?;
            let (
                (stdout, stdout_truncated, stdout_complete),
                (stderr, stderr_truncated, stderr_complete),
            ) = tokio::join!(stdout.finish(), stderr.finish());
            Ok(ProcessOutput {
                status,
                shutdown_escalated,
                group_cleanup_error,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                output_complete: stdout_complete && stderr_complete,
            })
        });
        Ok(ProcessHandle {
            stop: Some(stop),
            completion,
        })
    }
}

impl ProcessHandle {
    pub async fn wait(mut self) -> io::Result<ProcessOutput> {
        (&mut self.completion).await.map_err(io::Error::other)?
    }

    pub async fn stop(mut self) -> io::Result<ProcessOutput> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        (&mut self.completion).await.map_err(io::Error::other)?
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
