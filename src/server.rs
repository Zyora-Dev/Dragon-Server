use std::{
    convert::Infallible,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::Bytes;
use http_body_util::BodyExt;
use hyper::{
    Request, Response, StatusCode,
    body::{Body, Frame, Incoming, SizeHint},
    header,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinSet,
    time::{Instant, sleep_until, timeout},
};

use crate::{
    config::{Action, Config, Limits},
    routing::{decode_path, normalize_host, select_route},
    static_files::Root,
};

pub struct Server {
    config: Config,
    roots: Vec<Vec<Option<Arc<Root>>>>,
    requests: Arc<Semaphore>,
    files: Arc<Semaphore>,
    next_request: AtomicU64,
}

impl Server {
    pub fn new(mut config: Config) -> io::Result<Self> {
        config
            .validate(&std::env::current_dir()?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let roots = config
            .sites
            .iter()
            .map(|site| {
                site.routes
                    .iter()
                    .map(|route| {
                        route
                            .root
                            .as_ref()
                            .map(|path| Root::open(path).map(Arc::new))
                            .transpose()
                    })
                    .collect::<io::Result<Vec<_>>>()
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            requests: Arc::new(Semaphore::new(config.limits.max_in_flight_requests)),
            files: Arc::new(Semaphore::new(config.limits.max_in_flight_requests)),
            roots,
            config,
            next_request: AtomicU64::new(1),
        })
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let listener = TcpListener::bind(self.config.server.listen).await?;
        self.serve(listener, shutdown).await
    }

    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        let address = listener.local_addr()?;
        let maximum = self.config.limits.max_connections;
        let grace = Duration::from_millis(self.config.server.shutdown_timeout_ms);
        let shared = Arc::new(self);
        let connections = Arc::new(Semaphore::new(maximum));
        let (stop, _) = watch::channel(false);
        let mut tasks = JoinSet::new();
        tokio::pin!(shutdown);
        tracing::info!(event = "server_started", listen = %address, version = crate::version());
        let outcome = loop {
            tokio::select! {
                biased;
                _ = &mut shutdown => break Ok(()),
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = result { tracing::error!(event = "connection_task_failed", %error); }
                }
                accepted = listener.accept() => {
                    let (socket, _) = match accepted { Ok(accepted) => accepted, Err(error) => break Err(error) };
                    let Ok(permit) = connections.clone().try_acquire_owned() else { drop(socket); continue; };
                    let server = shared.clone();
                    let receiver = stop.subscribe();
                    tasks.spawn(async move {
                        let _permit = permit;
                        server.connection(socket, receiver).await;
                    });
                }
            }
        };
        drop(listener);
        stop.send_replace(true);
        if timeout(grace, async { while tasks.join_next().await.is_some() {} })
            .await
            .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
        tracing::info!(event = "server_stopped");
        outcome
    }

    async fn connection(self: Arc<Self>, socket: TcpStream, mut stop: watch::Receiver<bool>) {
        let limits = self.config.limits.clone();
        let initial = Phase::Idle;
        let (clock, mut changes) =
            watch::channel((initial, Instant::now() + initial.duration(&limits)));
        let stream = DeadlineIo {
            socket,
            clock: clock.clone(),
            limits: limits.clone(),
            ingress: crate::ingress::Ingress::new(limits.clone()),
        };
        let server = self.clone();
        let service = service_fn(move |request| server.clone().handle(request, clock.clone()));
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_millis(limits.header_timeout_ms))
            .max_headers(limits.max_headers)
            .max_buf_size(limits.max_header_bytes)
            .keep_alive(true);
        let connection = builder.serve_connection(TokioIo::new(stream), service);
        tokio::pin!(connection);
        let mut draining = false;
        loop {
            let deadline = changes.borrow().1;
            tokio::select! {
                result = &mut connection => {
                    if let Err(error) = result { tracing::debug!(event = "connection_closed", %error); }
                    break;
                }
                _ = stop.changed(), if !draining => {
                    draining = true;
                    connection.as_mut().graceful_shutdown();
                }
                _ = changes.changed() => {}
                _ = sleep_until(deadline) => {
                    if Instant::now() >= changes.borrow().1 {
                        tracing::debug!(event = "connection_deadline");
                        break;
                    }
                }
            }
        }
    }

    async fn handle(
        self: Arc<Self>,
        mut request: Request<Incoming>,
        clock: Clock,
    ) -> Result<Response<ResponseBody>, Infallible> {
        let started = Instant::now();
        let id = self.next_request.fetch_add(1, Ordering::Relaxed);
        set_phase(&clock, Phase::Body, &self.config.limits);
        let permit = self.requests.clone().try_acquire_owned().ok();
        let mut route_id = None;
        let mut response = if permit.is_none() {
            error_response(StatusCode::SERVICE_UNAVAILABLE, true)
        } else if let Some(status) = self.validate_request(&request) {
            error_response(status, true)
        } else {
            match self.consume_body(&mut request).await {
                Err(status) => error_response(status, true),
                Ok(()) => {
                    if request.uri().path() == "*" {
                        let mut response =
                            response(StatusCode::NO_CONTENT, "text/plain", Bytes::new());
                        response.headers_mut().insert(
                            header::ALLOW,
                            header::HeaderValue::from_static("GET, HEAD, OPTIONS"),
                        );
                        response
                    } else {
                        let (response, selected) = self.route(&request).await;
                        route_id = selected;
                        response
                    }
                }
            }
        };
        if request.method() == hyper::Method::HEAD {
            response.body_mut().source = Source::Memory(None);
            response.body_mut().remaining = 0;
        }
        response
            .headers_mut()
            .insert(header::SERVER, header::HeaderValue::from_static("Dragon"));
        response.headers_mut().insert(
            "x-content-type-options",
            header::HeaderValue::from_static("nosniff"),
        );
        response.headers_mut().insert(
            "x-request-id",
            header::HeaderValue::from_str(&id.to_string()).expect("numeric ID"),
        );
        set_phase(&clock, Phase::Response, &self.config.limits);
        let status = response.status().as_u16();
        let body = response.body_mut();
        body.clock = Some(clock.clone());
        body.permit = permit;
        body.access = Some(Access {
            id,
            status,
            started,
            route_id,
            sent: 0,
            complete: body.remaining == 0,
        });
        if body.remaining == 0 {
            mark_complete(&clock);
        }
        Ok(response)
    }

    fn validate_request(&self, request: &Request<Incoming>) -> Option<StatusCode> {
        let limits = &self.config.limits;
        let target = request.uri();
        if target.to_string().len() > limits.max_target_bytes {
            return Some(StatusCode::URI_TOO_LONG);
        }
        if target.scheme().is_some() || target.authority().is_some() {
            return Some(StatusCode::BAD_REQUEST);
        }
        if request.method() == hyper::Method::CONNECT || request.method() == hyper::Method::TRACE {
            return Some(StatusCode::METHOD_NOT_ALLOWED);
        }
        if target.path() == "*" {
            if request.method() != hyper::Method::OPTIONS {
                return Some(StatusCode::BAD_REQUEST);
            }
        } else if decode_path(target.path()).is_err() {
            return Some(StatusCode::BAD_REQUEST);
        }
        let head_size = request.method().as_str().len()
            + target.to_string().len()
            + 14
            + request
                .headers()
                .iter()
                .map(|(name, value)| name.as_str().len() + value.len() + 4)
                .sum::<usize>();
        if request.headers().len() > limits.max_headers || head_size > limits.max_header_bytes {
            return Some(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
        }
        if request.headers().get_all(header::HOST).iter().count() != 1 {
            return Some(StatusCode::BAD_REQUEST);
        }
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .and_then(normalize_host);
        if host.as_deref().is_none_or(|value| value == "*") {
            return Some(StatusCode::BAD_REQUEST);
        }
        if request.headers().contains_key(header::TRANSFER_ENCODING)
            && request.headers().contains_key(header::CONTENT_LENGTH)
        {
            return Some(StatusCode::BAD_REQUEST);
        }
        if request
            .headers()
            .get(header::EXPECT)
            .is_some_and(|value| value != "100-continue")
        {
            return Some(StatusCode::EXPECTATION_FAILED);
        }
        if request
            .body()
            .size_hint()
            .upper()
            .is_some_and(|size| size > limits.max_body_bytes as u64)
        {
            return Some(StatusCode::PAYLOAD_TOO_LARGE);
        }
        None
    }

    async fn consume_body(&self, request: &mut Request<Incoming>) -> Result<(), StatusCode> {
        let limits = &self.config.limits;
        timeout(Duration::from_millis(limits.body_timeout_ms), async {
            let mut total = 0usize;
            while let Some(frame) = request.body_mut().frame().await {
                let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
                if let Some(data) = frame.data_ref() {
                    total = total.saturating_add(data.len());
                    if total > limits.max_body_bytes {
                        return Err(StatusCode::PAYLOAD_TOO_LARGE);
                    }
                }
                if let Some(trailers) = frame.trailers_ref() {
                    let bytes: usize = trailers
                        .iter()
                        .map(|(name, value)| name.as_str().len() + value.len() + 4)
                        .sum();
                    if bytes > limits.max_header_bytes || trailers.len() > limits.max_headers {
                        return Err(StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
                    }
                }
            }
            Ok(())
        })
        .await
        .unwrap_or(Err(StatusCode::REQUEST_TIMEOUT))
    }

    async fn route(&self, request: &Request<Incoming>) -> (Response<ResponseBody>, Option<String>) {
        let host = normalize_host(
            request.headers()[header::HOST]
                .to_str()
                .expect("validated host"),
        )
        .expect("validated authority");
        let authority: hyper::http::uri::Authority = host.parse().expect("validated authority");
        let selected_site = self
            .config
            .sites
            .iter()
            .enumerate()
            .find(|(_, site)| site.hosts.iter().any(|configured| configured == &host))
            .or_else(|| {
                self.config.sites.iter().enumerate().find(|(_, site)| {
                    site.hosts
                        .iter()
                        .any(|configured| configured == authority.host())
                })
            })
            .or_else(|| {
                self.config
                    .sites
                    .iter()
                    .enumerate()
                    .find(|(_, site)| site.hosts.iter().any(|configured| configured == "*"))
            });
        let Some((site_index, site)) = selected_site else {
            return (error_response(StatusCode::NOT_FOUND, false), None);
        };
        let path = decode_path(request.uri().path()).expect("validated path");
        let Some((route_index, route)) = select_route(&site.routes, &path) else {
            return (error_response(StatusCode::NOT_FOUND, false), None);
        };
        let route_id = Some(format!("{}:{route_index}", site.id));
        let mut methods = route.methods.clone();
        if methods.iter().any(|method| method == "GET")
            && !methods.iter().any(|method| method == "HEAD")
        {
            methods.push("HEAD".into());
        }
        if !methods
            .iter()
            .any(|method| method == request.method().as_str())
        {
            let mut response = error_response(StatusCode::METHOD_NOT_ALLOWED, false);
            response.headers_mut().insert(
                header::ALLOW,
                header::HeaderValue::from_str(&methods.join(", ")).expect("validated methods"),
            );
            return (response, route_id);
        }
        match route.action {
            Action::Respond => (
                response(
                    StatusCode::from_u16(route.status.unwrap()).unwrap(),
                    route.content_type.as_deref().unwrap(),
                    Bytes::copy_from_slice(route.body.as_ref().unwrap().as_bytes()),
                ),
                route_id,
            ),
            Action::Static => {
                let root = self.roots[site_index][route_index]
                    .as_ref()
                    .unwrap()
                    .clone();
                let relative = path
                    .strip_prefix(&route.path)
                    .unwrap_or("")
                    .trim_start_matches('/')
                    .to_owned();
                let index = route.index.clone();
                let Ok(permit) = self.files.clone().try_acquire_owned() else {
                    return (
                        error_response(StatusCode::SERVICE_UNAVAILABLE, false),
                        route_id,
                    );
                };
                let opened = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    root.resolve(&relative, index.as_deref())
                })
                .await;
                match opened {
                    Ok(Ok((file, size, mime))) => {
                        let mut response = response(StatusCode::OK, mime, Bytes::new());
                        response.headers_mut().insert(
                            header::CONTENT_LENGTH,
                            header::HeaderValue::from_str(&size.to_string()).unwrap(),
                        );
                        *response.body_mut() =
                            ResponseBody::new(Source::File(tokio::fs::File::from_std(file)), size);
                        (response, route_id)
                    }
                    Ok(Err(_)) => (error_response(StatusCode::NOT_FOUND, false), route_id),
                    Err(error) => {
                        tracing::error!(event = "file_task_failed", %error);
                        (
                            error_response(StatusCode::INTERNAL_SERVER_ERROR, false),
                            route_id,
                        )
                    }
                }
            }
        }
    }
}

type Clock = watch::Sender<(Phase, Instant)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Head,
    Body,
    Response,
    Complete,
}

impl Phase {
    fn duration(self, limits: &Limits) -> Duration {
        Duration::from_millis(match self {
            Self::Idle => limits.idle_timeout_ms,
            Self::Head => limits.header_timeout_ms,
            Self::Body => limits.body_timeout_ms,
            Self::Response | Self::Complete => limits.response_timeout_ms,
        })
    }
}

fn set_phase(clock: &Clock, phase: Phase, limits: &Limits) {
    clock.send_replace((phase, Instant::now() + phase.duration(limits)));
}

fn mark_complete(clock: &Clock) {
    clock.send_modify(|(phase, _)| *phase = Phase::Complete);
}

struct DeadlineIo {
    socket: TcpStream,
    clock: Clock,
    limits: Limits,
    ingress: crate::ingress::Ingress,
}

impl AsyncRead for DeadlineIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            match self.ingress.deliver(buffer) {
                Ok(true) => return Poll::Ready(Ok(())),
                Err(error) => return Poll::Ready(Err(error)),
                Ok(false) => {}
            }
            let capacity = self.ingress.capacity();
            if capacity == 0 {
                return Poll::Ready(Err(io::ErrorKind::InvalidData.into()));
            }
            let mut bytes = [0u8; 16_384];
            let mut read = ReadBuf::new(&mut bytes[..capacity]);
            match Pin::new(&mut self.socket).poll_read(context, &mut read) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) if read.filled().is_empty() => return Poll::Ready(Ok(())),
                Poll::Ready(Ok(())) => {
                    if self.clock.borrow().0 == Phase::Idle {
                        set_phase(&self.clock, Phase::Head, &self.limits);
                    }
                    self.ingress.append(read.filled());
                }
            }
        }
    }
}

impl AsyncWrite for DeadlineIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.socket).poll_write(context, bytes)
    }
    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.socket).poll_flush(context);
        if matches!(result, Poll::Ready(Ok(()))) && self.clock.borrow().0 == Phase::Complete {
            set_phase(&self.clock, Phase::Idle, &self.limits);
        }
        result
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.socket).poll_shutdown(context)
    }
}

enum Source {
    Memory(Option<Bytes>),
    File(tokio::fs::File),
}

pub struct ResponseBody {
    source: Source,
    remaining: u64,
    clock: Option<Clock>,
    permit: Option<OwnedSemaphorePermit>,
    access: Option<Access>,
}

struct Access {
    id: u64,
    status: u16,
    started: Instant,
    route_id: Option<String>,
    sent: u64,
    complete: bool,
}

impl ResponseBody {
    fn new(source: Source, remaining: u64) -> Self {
        Self {
            source,
            remaining,
            clock: None,
            permit: None,
            access: None,
        }
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let capacity = self.remaining.min(16_384) as usize;
        let bytes = match &mut self.source {
            Source::Memory(bytes) => bytes.take().unwrap_or_default(),
            Source::File(file) => {
                let mut buffer = [0u8; 16_384];
                let mut read = ReadBuf::new(&mut buffer[..capacity]);
                match Pin::new(file).poll_read(context, &mut read) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Some(Err(error))),
                    Poll::Ready(Ok(())) => Bytes::copy_from_slice(read.filled()),
                }
            }
        };
        if bytes.is_empty() {
            return Poll::Ready(Some(Err(io::ErrorKind::UnexpectedEof.into())));
        }
        self.remaining -= bytes.len() as u64;
        let complete = self.remaining == 0;
        if let Some(access) = &mut self.access {
            access.sent += bytes.len() as u64;
            access.complete = complete;
        }
        if complete && let Some(clock) = &self.clock {
            mark_complete(clock);
        }
        Poll::Ready(Some(Ok(Frame::data(bytes))))
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        if let Some(access) = &self.access {
            tracing::info!(
                event = "request_finished",
                request_id = access.id,
                route_id = access.route_id.as_deref().unwrap_or("unmatched"),
                status = access.status,
                body_bytes_produced = access.sent,
                body_complete = access.complete,
                duration_ms = access.started.elapsed().as_millis() as u64
            );
        }
    }
}

fn response(status: StatusCode, content_type: &str, bytes: Bytes) -> Response<ResponseBody> {
    let size = bytes.len() as u64;
    let mut response = Response::new(ResponseBody::new(Source::Memory(Some(bytes)), size));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(content_type).expect("validated content type"),
    );
    if status != StatusCode::NO_CONTENT {
        response.headers_mut().insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_str(&size.to_string()).unwrap(),
        );
    }
    response
}

fn error_response(status: StatusCode, close: bool) -> Response<ResponseBody> {
    let mut response = response(
        status,
        "application/json",
        Bytes::from(
            serde_json::json!({"error": status.canonical_reason().unwrap_or("Request failed")})
                .to_string(),
        ),
    );
    if close {
        response.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("close"),
        );
    }
    if status == StatusCode::METHOD_NOT_ALLOWED {
        response.headers_mut().insert(
            header::ALLOW,
            header::HeaderValue::from_static("GET, HEAD, OPTIONS"),
        );
    }
    response
}
