use std::{
    collections::{BTreeMap, HashSet},
    io::Read,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub server: Server,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub logging: Logging,
    #[serde(default)]
    pub applications: Vec<Application>,
    pub sites: Vec<Site>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Application {
    pub id: String,
    pub release: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub address: SocketAddr,
    pub readiness_path: String,
    pub startup_timeout_ms: u64,
    pub probe_timeout_ms: u64,
    pub probe_interval_ms: u64,
    pub probe_attempts: u32,
    #[serde(default)]
    pub restart: Restart,
}

#[derive(Debug, Default, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Restart {
    #[default]
    Never,
    OnFailure {
        max_restarts: u32,
        initial_delay_ms: u64,
        max_delay_ms: u64,
    },
    Always {
        max_restarts: u32,
        initial_delay_ms: u64,
        max_delay_ms: u64,
    },
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Application {
    pub(crate) fn spec(&self) -> crate::application::ApplicationSpec {
        crate::application::ApplicationSpec {
            application_id: self.id.clone(),
            release_id: self.release.clone(),
            process: crate::process::ProcessSpec {
                executable: self.executable.clone(),
                args: self.args.iter().map(Into::into).collect(),
                cwd: self.cwd.clone(),
                env: self
                    .env
                    .iter()
                    .map(|(key, value)| (key.into(), value.into()))
                    .collect(),
            },
            readiness: crate::application::ReadinessSpec {
                address: self.address,
                path: self.readiness_path.clone(),
                startup_timeout: std::time::Duration::from_millis(self.startup_timeout_ms),
                probe_timeout: std::time::Duration::from_millis(self.probe_timeout_ms),
                interval: std::time::Duration::from_millis(self.probe_interval_ms),
                max_attempts: self.probe_attempts,
            },
        }
    }

    pub(crate) fn policy(&self) -> crate::application::RestartPolicy {
        use crate::application::{RestartBudget, RestartPolicy};
        use std::time::Duration;
        match self.restart {
            Restart::Never => RestartPolicy::Never,
            Restart::OnFailure {
                max_restarts,
                initial_delay_ms,
                max_delay_ms,
            }
            | Restart::Always {
                max_restarts,
                initial_delay_ms,
                max_delay_ms,
            } => {
                let budget = RestartBudget {
                    max_restarts,
                    initial_delay: Duration::from_millis(initial_delay_ms),
                    max_delay: Duration::from_millis(max_delay_ms),
                };
                if matches!(self.restart, Restart::Always { .. }) {
                    RestartPolicy::Always(budget)
                } else {
                    RestartPolicy::OnFailure(budget)
                }
            }
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: SocketAddr,
    #[serde(default = "shutdown_timeout")]
    pub shutdown_timeout_ms: u64,
    pub tls: Option<Tls>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    #[serde(default = "shutdown_timeout")]
    pub handshake_timeout_ms: u64,
}

fn shutdown_timeout() -> u64 {
    10_000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_connections: usize,
    pub max_in_flight_requests: usize,
    pub max_headers: usize,
    pub max_header_bytes: usize,
    pub max_target_bytes: usize,
    pub max_body_bytes: usize,
    pub max_proxy_response_bytes: usize,
    pub header_timeout_ms: u64,
    pub body_timeout_ms: u64,
    pub response_timeout_ms: u64,
    pub idle_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            max_in_flight_requests: 256,
            max_headers: 100,
            max_header_bytes: 16_384,
            max_target_bytes: 8192,
            max_body_bytes: 1_048_576,
            max_proxy_response_bytes: 16_777_216,
            header_timeout_ms: 10_000,
            body_timeout_ms: 30_000,
            response_timeout_ms: 30_000,
            idle_timeout_ms: 15_000,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub level: String,
    pub format: String,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            level: "info".into(),
            format: "json".into(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    pub id: String,
    pub hosts: Vec<String>,
    pub routes: Vec<Route>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub path: String,
    #[serde(rename = "match")]
    pub matcher: Match,
    pub methods: Vec<String>,
    pub action: Action,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    pub body: Option<String>,
    pub root: Option<PathBuf>,
    pub index: Option<String>,
    pub application: Option<String>,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub websocket: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Match {
    Exact,
    Prefix,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Respond,
    Static,
    Proxy,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path)
            .map_err(|error| format!("cannot open config {}: {error}", path.display()))?;
        let mut bytes = Vec::new();
        file.take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("cannot read config: {error}"))?;
        if bytes.len() > 1_048_576 {
            return Err("configuration exceeds 1 MiB".into());
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| "configuration must be UTF-8")?;
        let mut config: Self =
            toml::from_str(text).map_err(|error| format!("invalid TOML: {error}"))?;
        config.validate(path.parent().unwrap_or(Path::new(".")))?;
        Ok(config)
    }

    pub fn validate(&mut self, base: &Path) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("schema_version must be 1".into());
        }
        if let Some(tls) = &mut self.server.tls {
            if !(1..=60_000).contains(&tls.handshake_timeout_ms) {
                return Err("TLS handshake_timeout_ms must be between 1 and 60000".into());
            }
            for path in [&mut tls.certificate, &mut tls.private_key] {
                *path = base
                    .join(&*path)
                    .canonicalize()
                    .map_err(|error| format!("invalid TLS file: {error}"))?;
                let metadata = std::fs::metadata(&*path)
                    .map_err(|error| format!("invalid TLS file: {error}"))?;
                if !metadata.is_file() || metadata.len() > 1_048_576 {
                    return Err("TLS files must be regular files no larger than 1 MiB".into());
                }
            }
        }
        let limits = &self.limits;
        for (name, value, maximum) in [
            ("max_connections", limits.max_connections, 65_536),
            (
                "max_in_flight_requests",
                limits.max_in_flight_requests,
                65_536,
            ),
            ("max_headers", limits.max_headers, 1024),
            ("max_header_bytes", limits.max_header_bytes, 1_048_576),
            ("max_target_bytes", limits.max_target_bytes, 1_048_576),
            ("max_body_bytes", limits.max_body_bytes, 1_073_741_824),
            (
                "max_proxy_response_bytes",
                limits.max_proxy_response_bytes,
                67_108_864,
            ),
        ] {
            if value == 0 || value > maximum {
                return Err(format!("{name} must be between 1 and {maximum}"));
            }
        }
        if limits.max_header_bytes < 8192 {
            return Err("max_header_bytes must be at least 8192 for the HTTP engine".into());
        }
        if limits.max_target_bytes > limits.max_header_bytes {
            return Err("max_target_bytes must not exceed max_header_bytes".into());
        }
        for (name, value) in [
            ("header_timeout_ms", limits.header_timeout_ms),
            ("body_timeout_ms", limits.body_timeout_ms),
            ("response_timeout_ms", limits.response_timeout_ms),
            ("idle_timeout_ms", limits.idle_timeout_ms),
            ("shutdown_timeout_ms", self.server.shutdown_timeout_ms),
        ] {
            if value == 0 || value > 3_600_000 {
                return Err(format!("{name} must be between 1 and 3600000"));
            }
        }
        if !["error", "warn", "info", "debug", "trace"].contains(&self.logging.level.as_str())
            || self.logging.format != "json"
        {
            return Err("logging requires a valid level and format = 'json'".into());
        }
        if self.sites.is_empty() {
            return Err("at least one site is required".into());
        }
        if self.applications.len() > 64 {
            return Err("at most 64 applications are allowed".into());
        }
        let mut application_ids = HashSet::new();
        let mut addresses = HashSet::new();
        for application in &mut self.applications {
            for name in [&application.id, &application.release] {
                if name.is_empty()
                    || name.len() > 128
                    || !name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
                {
                    return Err("invalid application or release ID".into());
                }
            }
            if !application_ids.insert(application.id.clone())
                || !addresses.insert(application.address)
                || application.address.port() == self.server.listen.port()
            {
                return Err(
                    "application IDs and endpoints must be unique and separate from the listener"
                        .into(),
                );
            }
            for path in [&mut application.executable, &mut application.cwd] {
                if !path.is_absolute() {
                    *path = base.join(&*path);
                }
                *path = path
                    .canonicalize()
                    .map_err(|_| "application executable/cwd does not exist")?;
            }
            if !application.executable.is_file()
                || !application.cwd.is_dir()
                || application.args.len() > 4096
                || application.env.len() > 4096
                || application.args.iter().map(String::len).sum::<usize>()
                    + application
                        .env
                        .iter()
                        .map(|(key, value)| key.len() + value.len())
                        .sum::<usize>()
                    > 1_048_576
                || application.args.iter().any(|value| value.contains('\0'))
                || application.env.iter().any(|(key, value)| {
                    key.is_empty() || key.contains(['=', '\0']) || value.contains('\0')
                })
            {
                return Err("invalid application executable/cwd, arguments or environment".into());
            }
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            {
                application
                    .spec()
                    .readiness
                    .validate()
                    .map_err(|error| error.to_string())?;
                application
                    .policy()
                    .validate()
                    .map_err(|error| error.to_string())?;
            }
            #[cfg(not(any(target_os = "linux", target_os = "macos")))]
            return Err("application hosting requires Linux or macOS".into());
        }
        let mut ids = HashSet::new();
        let mut hosts = HashSet::new();
        for site in &mut self.sites {
            if site.id.is_empty()
                || !site
                    .id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
                || !ids.insert(site.id.clone())
            {
                return Err("site IDs must be unique nonempty ASCII identifiers".into());
            }
            if site.hosts.is_empty() || site.routes.is_empty() {
                return Err(format!("site {} needs hosts and routes", site.id));
            }
            for host in &mut site.hosts {
                *host = crate::routing::normalize_host(host).ok_or("invalid configured host")?;
                if !hosts.insert(host.clone()) {
                    return Err(format!("duplicate host {host}"));
                }
            }
            let mut selectors = HashSet::new();
            for route in &mut site.routes {
                if (route.streaming || route.websocket) && route.action != Action::Proxy {
                    return Err("streaming and websocket require a proxy route".into());
                }
                if crate::routing::decode_path(&route.path).as_deref() != Ok(route.path.as_str())
                    || route.path.contains('?')
                    || (route.path.len() > 1 && route.path.ends_with('/'))
                {
                    return Err("route paths must be decoded absolute paths without dot segments or trailing slashes".into());
                }
                if !selectors.insert((route.path.clone(), route.matcher)) {
                    return Err("duplicate route selector".into());
                }
                if route.methods.is_empty() {
                    return Err("route methods cannot be empty".into());
                }
                let mut methods = HashSet::new();
                for method in &route.methods {
                    if !["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
                        .contains(&method.as_str())
                        || !methods.insert(method)
                    {
                        return Err("unsupported or repeated route method".into());
                    }
                }
                if route.methods.iter().any(|method| method == "HEAD")
                    && !route.methods.iter().any(|method| method == "GET")
                {
                    return Err("HEAD requires GET on the same route".into());
                }
                match route.action {
                    Action::Proxy => {
                        if route
                            .application
                            .as_ref()
                            .is_none_or(|id| !application_ids.contains(id))
                            || route.root.is_some()
                            || route.index.is_some()
                            || route.status.is_some()
                            || route.content_type.is_some()
                            || route.body.is_some()
                        {
                            return Err("proxy routes require a configured application and cannot contain static/response fields".into());
                        }
                    }
                    Action::Respond => {
                        if route.root.is_some()
                            || route.index.is_some()
                            || route.application.is_some()
                        {
                            return Err(
                                "respond routes cannot contain root/index/application".into()
                            );
                        }
                        let status = route.status.ok_or("respond route requires status")?;
                        if !(200..=599).contains(&status)
                            || status == 204
                            || status == 205
                            || status == 304
                        {
                            return Err("respond status must allow a final response body".into());
                        }
                        let content_type = route
                            .content_type
                            .as_ref()
                            .ok_or("respond route requires content_type")?;
                        hyper::header::HeaderValue::from_str(content_type)
                            .map_err(|_| "invalid content_type")?;
                        let body = route.body.as_ref().ok_or("respond route requires body")?;
                        if content_type
                            .split(';')
                            .next()
                            .is_some_and(|mime| mime.trim() == "application/json")
                        {
                            serde_json::from_str::<serde_json::Value>(body)
                                .map_err(|_| "respond body is not valid JSON")?;
                        }
                    }
                    Action::Static => {
                        if route.body.is_some()
                            || route.status.is_some()
                            || route.content_type.is_some()
                            || route.application.is_some()
                        {
                            return Err("static routes cannot contain response fields".into());
                        }
                        if route
                            .methods
                            .iter()
                            .any(|method| method != "GET" && method != "HEAD")
                        {
                            return Err("static routes support only GET/HEAD".into());
                        }
                        if let Some(index) = &route.index
                            && (index.is_empty()
                                || index.starts_with('.')
                                || index.contains(['/', '\\', '\0']))
                        {
                            return Err("index must be a single non-hidden filename".into());
                        }
                        let root = route.root.as_mut().ok_or("static route requires root")?;
                        if !root.is_absolute() {
                            *root = base.join(&*root);
                        }
                        if !root.is_dir() {
                            return Err(format!(
                                "static root is not a directory: {}",
                                root.display()
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
