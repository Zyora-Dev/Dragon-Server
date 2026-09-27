use std::{
    collections::HashSet,
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
    pub sites: Vec<Site>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub listen: SocketAddr,
    #[serde(default = "shutdown_timeout")]
    pub shutdown_timeout_ms: u64,
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
                    Action::Respond => {
                        if route.root.is_some() || route.index.is_some() {
                            return Err("respond routes cannot contain root/index".into());
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
