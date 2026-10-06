use std::{
    convert::Infallible,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt, Full};
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
    tls: Option<tokio_rustls::TlsAcceptor>,
    roots: Vec<Vec<Option<Arc<Root>>>>,
    requests: Arc<Semaphore>,
    files: Arc<Semaphore>,
    next_request: AtomicU64,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    applications:
        std::collections::HashMap<String, watch::Receiver<crate::application::InstanceState>>,
}

impl Server {
    pub fn new(mut config: Config) -> io::Result<Self> {
        config
            .validate(&std::env::current_dir()?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let tls = config.server.tls.as_ref().map(load_tls).transpose()?;
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
            tls,
            config,
            next_request: AtomicU64::new(1),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            applications: std::collections::HashMap::new(),
        })
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> io::Result<()> {
        let listener = TcpListener::bind(self.config.server.listen).await?;
        self.serve(listener, shutdown).await
    }

    pub async fn serve(
        mut self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        let address = listener.local_addr()?;
        let maximum = self.config.limits.max_connections;
        let grace = Duration::from_millis(self.config.server.shutdown_timeout_ms);
        tokio::pin!(shutdown);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let handles = {
            let manager =
                crate::application::ApplicationManager::new(self.config.applications.len().max(1))?;
            let mut handles = Vec::new();
            for application in &self.config.applications {
                let mut handle =
                    match manager.start_with_restart(application.spec(), application.policy()) {
                        Ok(handle) => handle,
                        Err(error) => {
                            let _ = stop_applications(handles).await;
                            return Err(error);
                        }
                    };
                self.applications
                    .insert(application.id.clone(), handle.subscribe());
                let ready = tokio::select! {
                    biased;
                    _ = &mut shutdown => None,
                    ready = handle.wait_ready() => Some(ready),
                };
                handles.push(handle);
                match ready {
                    Some(Ok(())) => {
                        tracing::info!(event = "application_ready", application_id = %application.id)
                    }
                    Some(Err(error)) => {
                        let _ = stop_applications(handles).await;
                        return Err(error);
                    }
                    None => return stop_applications(handles).await,
                }
            }
            handles
        };
        let shared = Arc::new(self);
        let connections = Arc::new(Semaphore::new(maximum));
        let (stop, _) = watch::channel(false);
        let mut tasks = JoinSet::new();
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
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let cleanup = stop_applications(handles).await;
            outcome.and(cleanup)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        outcome
    }

    async fn connection(self: Arc<Self>, socket: TcpStream, mut stop: watch::Receiver<bool>) {
        if *stop.borrow() {
            return;
        }
        let socket: Box<dyn Transport> = if let Some(tls) = &self.tls {
            let handshake = timeout(
                Duration::from_millis(
                    self.config
                        .server
                        .tls
                        .as_ref()
                        .unwrap()
                        .handshake_timeout_ms,
                ),
                tls.accept(socket),
            );
            tokio::select! {
                _ = stop.changed() => return,
                result = handshake => match result {
                    Ok(Ok(stream)) => Box::new(stream),
                    _ => { tracing::debug!(event = "tls_handshake_rejected"); return; }
                }
            }
        } else {
            Box::new(socket)
        };
        let limits = self.config.limits.clone();
        let initial = Phase::Idle;
        let (clock, mut changes) =
            watch::channel((initial, Instant::now() + initial.duration(&limits)));
        let upgraded = Arc::new(AtomicBool::new(false));
        let (tunnels, mut pending) = tokio::sync::mpsc::channel(1);
        let stream = DeadlineIo {
            socket,
            upgraded: upgraded.clone(),
            clock: clock.clone(),
            limits: limits.clone(),
            ingress: crate::ingress::Ingress::new(limits.clone()),
        };
        let server = self.clone();
        let service_clock = clock.clone();
        let service = service_fn(move |request| {
            server
                .clone()
                .handle(request, service_clock.clone(), tunnels.clone())
        });
        let mut builder = http1::Builder::new();
        builder
            .timer(TokioTimer::new())
            .header_read_timeout(Duration::from_millis(limits.header_timeout_ms))
            .max_headers(limits.max_headers)
            .max_buf_size(limits.max_header_bytes)
            .keep_alive(true);
        let connection = builder
            .serve_connection(TokioIo::new(stream), service)
            .with_upgrades();
        tokio::pin!(connection);
        let mut draining = false;
        loop {
            let deadline = changes.borrow().1;
            tokio::select! {
                result = &mut connection => {
                    if let Err(error) = result { tracing::debug!(event = "connection_closed", %error); }
                    else if !draining && let Ok(tunnel) = pending.try_recv() {
                        upgraded.store(true, Ordering::Release);
                        set_phase(&clock, Phase::Tunnel, &limits);
                        tunnel.run(&mut stop, &mut changes).await;
                    }
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
        tunnels: tokio::sync::mpsc::Sender<Tunnel>,
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
                Ok(bytes) => {
                    if request.uri().path() == "*" {
                        let mut response =
                            response(StatusCode::NO_CONTENT, "text/plain", Bytes::new());
                        response.headers_mut().insert(
                            header::ALLOW,
                            header::HeaderValue::from_static("GET, HEAD, OPTIONS"),
                        );
                        response
                    } else {
                        set_phase(&clock, Phase::Upstream, &self.config.limits);
                        let (response, selected) = self.route(&mut request, bytes).await;
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
        if request.headers().contains_key(header::UPGRADE)
            && response.status() != StatusCode::SWITCHING_PROTOCOLS
        {
            response.headers_mut().insert(
                header::CONNECTION,
                header::HeaderValue::from_static("close"),
            );
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
        let streaming = matches!(response.body().source, Source::Stream(_));
        set_phase(
            &clock,
            if streaming {
                Phase::Streaming
            } else {
                Phase::Response
            },
            &self.config.limits,
        );
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
            complete: body.is_end_stream(),
        });
        if body.is_end_stream() {
            mark_complete(&clock);
        }
        if let Some(mut tunnel) = body.tunnel.take() {
            tunnel._permit = body.permit.take();
            if tunnels.try_send(tunnel).is_err() {
                return Ok(error_response(StatusCode::SERVICE_UNAVAILABLE, true));
            }
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

    async fn consume_body(&self, request: &mut Request<Incoming>) -> Result<Bytes, StatusCode> {
        let limits = &self.config.limits;
        timeout(Duration::from_millis(limits.body_timeout_ms), async {
            let mut total = 0usize;
            let mut bytes = BytesMut::new();
            while let Some(frame) = request.body_mut().frame().await {
                let frame = frame.map_err(|_| StatusCode::BAD_REQUEST)?;
                if let Some(data) = frame.data_ref() {
                    total = total.saturating_add(data.len());
                    if total > limits.max_body_bytes {
                        return Err(StatusCode::PAYLOAD_TOO_LARGE);
                    }
                    bytes.extend_from_slice(data);
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
            Ok(bytes.freeze())
        })
        .await
        .unwrap_or(Err(StatusCode::REQUEST_TIMEOUT))
    }

    async fn route(
        &self,
        request: &mut Request<Incoming>,
        bytes: Bytes,
    ) -> (Response<ResponseBody>, Option<String>) {
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
            Action::Proxy => {
                let id = route.application.as_ref().expect("validated application");
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                let ready = self.applications.get(id).is_some_and(|state| {
                    *state.borrow() == crate::application::InstanceState::Ready
                });
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                let ready = false;
                if !ready {
                    return (
                        error_response(StatusCode::SERVICE_UNAVAILABLE, false),
                        route_id,
                    );
                }
                if request.headers().contains_key(header::UPGRADE) {
                    if !route.websocket {
                        return (error_response(StatusCode::NOT_IMPLEMENTED, true), route_id);
                    }
                    if websocket_accept(request).is_err()
                        || !bytes.is_empty()
                        || request.headers().contains_key(header::TRANSFER_ENCODING)
                    {
                        return (error_response(StatusCode::BAD_REQUEST, true), route_id);
                    }
                }
                let application = self
                    .config
                    .applications
                    .iter()
                    .find(|application| &application.id == id)
                    .unwrap();
                let forwarded = timeout(
                    Duration::from_millis(self.config.limits.response_timeout_ms),
                    self.forward(application.address, request, bytes, route.streaming),
                )
                .await;
                let result = match forwarded {
                    Ok(Ok(response)) => response,
                    Ok(Err(_)) => error_response(StatusCode::BAD_GATEWAY, false),
                    Err(_) => error_response(StatusCode::GATEWAY_TIMEOUT, false),
                };
                (result, route_id)
            }
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

    async fn forward(
        &self,
        address: std::net::SocketAddr,
        request: &mut Request<Incoming>,
        bytes: Bytes,
        streaming: bool,
    ) -> io::Result<Response<ResponseBody>> {
        let websocket = request.headers().contains_key(header::UPGRADE);
        let socket = TcpStream::connect(address).await?;
        let mut builder = hyper::client::conn::http1::Builder::new();
        builder
            .max_headers(self.config.limits.max_headers)
            .max_buf_size(self.config.limits.max_header_bytes);
        let (mut sender, connection) = builder
            .handshake(TokioIo::new(socket))
            .await
            .map_err(io::Error::other)?;
        let driver = UpstreamDriver(tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
        }));
        let mut upstream = Request::new(Full::new(bytes));
        *upstream.method_mut() = request.method().clone();
        *upstream.uri_mut() = request.uri().clone();
        *upstream.headers_mut() = request.headers().clone();
        strip_hop_headers(upstream.headers_mut());
        let forwarded: Vec<_> = upstream
            .headers()
            .keys()
            .filter(|name| {
                name.as_str().starts_with("x-forwarded-")
                    || name.as_str() == "forwarded"
                    || name.as_str() == "x-real-ip"
            })
            .cloned()
            .collect();
        for name in forwarded {
            upstream.headers_mut().remove(name);
        }
        upstream.headers_mut().remove(header::CONTENT_LENGTH);
        upstream.headers_mut().remove(header::EXPECT);
        upstream
            .headers_mut()
            .insert(header::HOST, request.headers()[header::HOST].clone());
        upstream
            .headers_mut()
            .insert("x-forwarded-host", request.headers()[header::HOST].clone());
        upstream.headers_mut().insert(
            "x-forwarded-proto",
            header::HeaderValue::from_static(if self.tls.is_some() { "https" } else { "http" }),
        );
        upstream.headers_mut().insert(
            header::CONNECTION,
            header::HeaderValue::from_static("close"),
        );
        if websocket {
            upstream.headers_mut().insert(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            );
            upstream.headers_mut().insert(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            );
            for name in [
                header::SEC_WEBSOCKET_KEY,
                header::SEC_WEBSOCKET_VERSION,
                header::SEC_WEBSOCKET_PROTOCOL,
            ] {
                upstream.headers_mut().remove(&name);
                for value in request.headers().get_all(&name) {
                    upstream.headers_mut().append(name.clone(), value.clone());
                }
            }
            upstream
                .headers_mut()
                .remove(header::SEC_WEBSOCKET_EXTENSIONS);
        }
        let mut upstream = sender
            .send_request(upstream)
            .await
            .map_err(io::Error::other)?;
        if upstream.headers().len() > self.config.limits.max_headers
            || upstream
                .headers()
                .iter()
                .map(|(name, value)| name.as_str().len() + value.len() + 4)
                .sum::<usize>()
                > self.config.limits.max_header_bytes
        {
            return Err(io::Error::other("invalid upstream response head"));
        }
        if upstream.status() == StatusCode::SWITCHING_PROTOCOLS && websocket {
            let expected = websocket_accept(request)?;
            if upstream
                .headers()
                .get_all(header::SEC_WEBSOCKET_ACCEPT)
                .iter()
                .count()
                != 1
                || upstream.headers().get(header::SEC_WEBSOCKET_ACCEPT) != Some(&expected)
                || !header_token(upstream.headers(), header::CONNECTION, "upgrade")
                || !header_token(upstream.headers(), header::UPGRADE, "websocket")
                || upstream
                    .headers()
                    .contains_key(header::SEC_WEBSOCKET_EXTENSIONS)
                || upstream.headers().contains_key(header::CONTENT_LENGTH)
                || upstream.headers().contains_key(header::TRANSFER_ENCODING)
            {
                return Err(io::Error::other("invalid WebSocket handshake"));
            }
            let protocols: Vec<_> = upstream
                .headers()
                .get_all(header::SEC_WEBSOCKET_PROTOCOL)
                .iter()
                .collect();
            if protocols.len() > 1
                || protocols.first().is_some_and(|value| {
                    value.to_str().map_or(true, |selected| {
                        selected.contains(',')
                            || !header_token(
                                request.headers(),
                                header::SEC_WEBSOCKET_PROTOCOL,
                                selected,
                            )
                    })
                })
            {
                return Err(io::Error::other("unrequested WebSocket subprotocol"));
            }
            let protocol = protocols.first().map(|value| (*value).clone());
            let downstream = hyper::upgrade::on(request);
            let backend = hyper::upgrade::on(&mut upstream);
            let (mut parts, _) = upstream.into_parts();
            strip_hop_headers(&mut parts.headers);
            parts.headers.insert(
                header::CONNECTION,
                header::HeaderValue::from_static("upgrade"),
            );
            parts.headers.insert(
                header::UPGRADE,
                header::HeaderValue::from_static("websocket"),
            );
            parts.headers.insert(header::SEC_WEBSOCKET_ACCEPT, expected);
            if let Some(protocol) = protocol {
                parts
                    .headers
                    .insert(header::SEC_WEBSOCKET_PROTOCOL, protocol);
            }
            let mut body = ResponseBody::new(Source::Memory(None), 0);
            body.tunnel = Some(Tunnel {
                downstream,
                backend,
                _driver: driver,
                _permit: None,
            });
            return Ok(Response::from_parts(parts, body));
        }
        if upstream.status().is_informational() {
            return Err(io::Error::other("unexpected upstream upgrade"));
        }
        if streaming {
            let (mut parts, body) = upstream.into_parts();
            strip_hop_headers(&mut parts.headers);
            return Ok(Response::from_parts(
                parts,
                ResponseBody::new(
                    Source::Stream(Box::new(ProxyStream {
                        body,
                        _driver: driver,
                    })),
                    0,
                ),
            ));
        }
        let _driver = driver;
        let mut body = BytesMut::new();
        let maximum = self.config.limits.max_proxy_response_bytes;
        if upstream
            .body()
            .size_hint()
            .upper()
            .is_some_and(|size| size > maximum as u64)
        {
            return Err(io::Error::other("upstream response exceeds limit"));
        }
        while let Some(frame) = upstream.body_mut().frame().await {
            let frame = frame.map_err(io::Error::other)?;
            if let Some(data) = frame.data_ref() {
                if data.len() > maximum.saturating_sub(body.len()) {
                    return Err(io::Error::other("upstream response exceeds limit"));
                }
                body.extend_from_slice(data);
            }
        }
        let (mut parts, _) = upstream.into_parts();
        strip_hop_headers(&mut parts.headers);
        if request.method() != hyper::Method::HEAD && parts.status != StatusCode::NOT_MODIFIED {
            parts.headers.remove(header::CONTENT_LENGTH);
            if parts.status != StatusCode::NO_CONTENT && parts.status != StatusCode::RESET_CONTENT {
                parts.headers.insert(
                    header::CONTENT_LENGTH,
                    header::HeaderValue::from_str(&body.len().to_string()).unwrap(),
                );
            }
        }
        let size = body.len() as u64;
        Ok(Response::from_parts(
            parts,
            ResponseBody::new(Source::Memory(Some(body.freeze())), size),
        ))
    }
}

fn load_tls(tls: &crate::config::Tls) -> io::Result<tokio_rustls::TlsAcceptor> {
    use std::io::Read;
    use tokio_rustls::rustls;
    let read = |path: &std::path::Path| -> io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("TLS file is not regular"));
        }
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes)?;
        if bytes.len() > 1_048_576 {
            return Err(io::Error::other("TLS file exceeds 1 MiB"));
        }
        Ok(bytes)
    };
    let certificates = rustls_pemfile::certs(&mut read(&tls.certificate)?.as_slice())
        .collect::<io::Result<Vec<_>>>()?;
    if certificates.is_empty() || certificates.len() > 32 {
        return Err(io::Error::other("TLS requires 1 to 32 certificates"));
    }
    let key = rustls_pemfile::private_key(&mut read(&tls.private_key)?.as_slice())?
        .ok_or_else(|| io::Error::other("TLS private key missing"))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(io::Error::other)?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .map_err(io::Error::other)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

struct UpstreamDriver(tokio::task::JoinHandle<()>);

struct Tunnel {
    downstream: hyper::upgrade::OnUpgrade,
    backend: hyper::upgrade::OnUpgrade,
    _driver: UpstreamDriver,
    _permit: Option<OwnedSemaphorePermit>,
}

impl Tunnel {
    async fn run(
        self,
        stop: &mut watch::Receiver<bool>,
        changes: &mut watch::Receiver<(Phase, Instant)>,
    ) {
        if *stop.borrow() {
            return;
        }
        let Self {
            downstream,
            backend,
            _driver,
            _permit,
        } = self;
        let copy = async {
            let (downstream, backend) =
                tokio::try_join!(downstream, backend).map_err(io::Error::other)?;
            tokio::io::copy_bidirectional_with_sizes(
                &mut TokioIo::new(downstream),
                &mut TokioIo::new(backend),
                16_384,
                16_384,
            )
            .await
        };
        tokio::pin!(copy);
        loop {
            let deadline = changes.borrow().1;
            tokio::select! {
                result = &mut copy => {
                    match result {
                        Ok((sent, received)) => tracing::debug!(event = "websocket_closed", sent, received),
                        Err(error) => tracing::debug!(event = "websocket_closed", %error),
                    }
                    return;
                }
                _ = stop.changed() => return,
                _ = changes.changed() => {},
                _ = sleep_until(deadline) => {
                    if Instant::now() >= changes.borrow().1 { return; }
                }
            }
        }
    }
}

fn header_token(headers: &header::HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers
        .get_all(&name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| {
            if name == header::SEC_WEBSOCKET_PROTOCOL {
                value.trim() == token
            } else {
                value.trim().eq_ignore_ascii_case(token)
            }
        })
}

fn websocket_accept(request: &Request<Incoming>) -> io::Result<header::HeaderValue> {
    use base64::Engine;
    let bad = || io::Error::other("invalid WebSocket request");
    let key = request
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .ok_or_else(bad)?;
    if request
        .headers()
        .get_all(header::SEC_WEBSOCKET_KEY)
        .iter()
        .count()
        != 1
        || request
            .headers()
            .get_all(header::SEC_WEBSOCKET_VERSION)
            .iter()
            .count()
            != 1
        || request.headers().get_all(header::UPGRADE).iter().count() != 1
        || header_token(request.headers(), header::CONNECTION, "close")
        || base64::engine::general_purpose::STANDARD
            .decode(key.as_bytes())
            .map_err(|_| bad())?
            .len()
            != 16
    {
        return Err(bad());
    }
    let mut handshake = Request::new(());
    *handshake.method_mut() = request.method().clone();
    *handshake.version_mut() = request.version();
    *handshake.uri_mut() = request.uri().clone();
    *handshake.headers_mut() = request.headers().clone();
    let response =
        tungstenite::handshake::server::create_response(&handshake).map_err(io::Error::other)?;
    Ok(response.headers()[header::SEC_WEBSOCKET_ACCEPT].clone())
}

impl Drop for UpstreamDriver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn strip_hop_headers(headers: &mut header::HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| header::HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn stop_applications(handles: Vec<crate::application::ApplicationHandle>) -> io::Result<()> {
    let mut tasks = JoinSet::new();
    for handle in handles {
        tasks.spawn(async move {
            let id = handle.identity().application_id.clone();
            let output = handle.stop().await?;
            tracing::info!(event = "application_stopped", application_id = %id, status = %output.process.status);
            if let Some(error) = output.process.group_cleanup_error {
                tracing::error!(event = "application_cleanup_failed", application_id = %id, %error);
                return Err(error);
            }
            Ok(())
        });
    }
    let mut outcome = Ok(());
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result.map_err(io::Error::other).and_then(|result| result) {
            outcome = Err(error);
        }
    }
    outcome
}

type Clock = watch::Sender<(Phase, Instant)>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Head,
    Body,
    Upstream,
    Streaming,
    Tunnel,
    Response,
    Complete,
}

impl Phase {
    fn duration(self, limits: &Limits) -> Duration {
        Duration::from_millis(match self {
            Self::Idle => limits.idle_timeout_ms,
            Self::Head => limits.header_timeout_ms,
            Self::Body => limits.body_timeout_ms,
            Self::Upstream => limits.response_timeout_ms.saturating_add(1000),
            Self::Response | Self::Complete | Self::Streaming => limits.response_timeout_ms,
            Self::Tunnel => limits.idle_timeout_ms,
        })
    }
}

fn set_phase(clock: &Clock, phase: Phase, limits: &Limits) {
    clock.send_replace((phase, Instant::now() + phase.duration(limits)));
}

fn mark_complete(clock: &Clock) {
    clock.send_modify(|(phase, _)| *phase = Phase::Complete);
}

trait Transport: AsyncRead + AsyncWrite + Unpin + Send {}
impl<Stream: AsyncRead + AsyncWrite + Unpin + Send> Transport for Stream {}

struct DeadlineIo {
    socket: Box<dyn Transport>,
    upgraded: Arc<AtomicBool>,
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
        if self.upgraded.load(Ordering::Acquire) {
            if self.ingress.deliver_upgraded(buffer) {
                set_phase(&self.clock, Phase::Tunnel, &self.limits);
                return Poll::Ready(Ok(()));
            }
            let before = buffer.filled().len();
            let result = Pin::new(&mut self.socket).poll_read(context, buffer);
            if matches!(result, Poll::Ready(Ok(()))) && buffer.filled().len() > before {
                set_phase(&self.clock, Phase::Tunnel, &self.limits);
            }
            return result;
        }
        loop {
            match self.ingress.deliver(buffer) {
                Ok(true) => return Poll::Ready(Ok(())),
                Err(error) => return Poll::Ready(Err(error)),
                Ok(false) => {}
            }
            if self.ingress.awaiting_upgrade() {
                return Poll::Pending;
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
        let result = Pin::new(&mut self.socket).poll_write(context, bytes);
        let phase = self.clock.borrow().0;
        if matches!(result, Poll::Ready(Ok(size)) if size > 0)
            && matches!(phase, Phase::Streaming | Phase::Tunnel)
        {
            set_phase(&self.clock, phase, &self.limits);
        }
        result
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
    Stream(Box<ProxyStream>),
}

struct ProxyStream {
    body: Incoming,
    _driver: UpstreamDriver,
}

pub struct ResponseBody {
    source: Source,
    remaining: u64,
    clock: Option<Clock>,
    permit: Option<OwnedSemaphorePermit>,
    access: Option<Access>,
    tunnel: Option<Tunnel>,
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
            tunnel: None,
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
        let body = self.as_mut().get_mut();
        if let Source::Stream(stream) = &mut body.source {
            loop {
                match Pin::new(&mut stream.body).poll_frame(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Some(Err(error))) => {
                        return Poll::Ready(Some(Err(io::Error::other(error))));
                    }
                    Poll::Ready(Some(Ok(frame))) => {
                        let complete = stream.body.is_end_stream();
                        if complete {
                            if let Some(access) = &mut body.access {
                                access.complete = true;
                            }
                            if let Some(clock) = &body.clock {
                                mark_complete(clock);
                            }
                        }
                        if let Ok(data) = frame.into_data() {
                            if let Some(access) = &mut body.access {
                                access.sent += data.len() as u64;
                            }
                            return Poll::Ready(Some(Ok(Frame::data(data))));
                        }
                    }
                    Poll::Ready(None) => {
                        if let Some(access) = &mut body.access {
                            access.complete = true;
                        }
                        if let Some(clock) = &body.clock {
                            mark_complete(clock);
                        }
                        return Poll::Ready(None);
                    }
                }
            }
        }
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let capacity = self.remaining.min(16_384) as usize;
        let bytes = match &mut self.source {
            Source::Stream(_) => unreachable!(),
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
        match &self.source {
            Source::Stream(stream) => stream.body.is_end_stream(),
            _ => self.remaining == 0,
        }
    }
    fn size_hint(&self) -> SizeHint {
        match &self.source {
            Source::Stream(stream) => stream.body.size_hint(),
            _ => SizeHint::with_exact(self.remaining),
        }
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
