use std::{collections::HashSet, fs, net::SocketAddr, path::Path, time::Duration};

use serde::Deserialize;
use thiserror::Error;
use tracegate_core::{AppConfig, Route, Upstream};
use url::Url;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config `{path}`: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse TOML config `{path}`: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
pub struct RawConfig {
    pub server: ServerConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
}

#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    #[serde(default = "default_json_logging")]
    pub json: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            json: default_json_logging(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RouteConfig {
    pub id: String,
    pub hosts: Vec<String>,
    pub path_prefix: String,
    pub upstreams: Vec<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub retries: u32,
}

pub fn load_config(path: impl AsRef<Path>) -> Result<AppConfig, ConfigError> {
    let path = path.as_ref();
    let display = path.display().to_string();
    let raw = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: display.clone(),
        source,
    })?;
    let parsed: RawConfig = toml::from_str(&raw).map_err(|source| ConfigError::Parse {
        path: display,
        source,
    })?;
    parsed.validate()
}

impl RawConfig {
    pub fn validate(self) -> Result<AppConfig, ConfigError> {
        let listen: SocketAddr = self
            .server
            .listen
            .parse()
            .map_err(|err| ConfigError::Invalid(format!("server.listen: {err}")))?;

        if self.routes.is_empty() {
            return Err(ConfigError::Invalid(
                "at least one route must be configured".to_owned(),
            ));
        }

        let mut route_ids = HashSet::new();
        let mut routes = Vec::with_capacity(self.routes.len());

        for route in self.routes {
            validate_route_id(&route.id)?;
            if !route_ids.insert(route.id.clone()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate route id `{}`",
                    route.id
                )));
            }

            validate_hosts(&route.id, &route.hosts)?;
            validate_path_prefix(&route.id, &route.path_prefix)?;

            if route.upstreams.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "route `{}` must define at least one upstream",
                    route.id
                )));
            }

            if route.timeout_ms == 0 || route.timeout_ms > 60_000 {
                return Err(ConfigError::Invalid(format!(
                    "route `{}` timeout_ms must be between 1 and 60000",
                    route.id
                )));
            }

            if route.retries > 3 {
                return Err(ConfigError::Invalid(format!(
                    "route `{}` retries must be between 0 and 3",
                    route.id
                )));
            }

            let upstreams = route
                .upstreams
                .iter()
                .map(|upstream| validate_upstream(&route.id, upstream))
                .collect::<Result<Vec<_>, _>>()?;

            routes.push(Route::new(
                route.id,
                route.hosts,
                route.path_prefix,
                upstreams,
                Duration::from_millis(route.timeout_ms),
                route.retries,
            ));
        }

        Ok(AppConfig {
            listen,
            json_logs: self.logging.json,
            routes,
        })
    }
}

fn validate_route_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty() {
        return Err(ConfigError::Invalid("route id cannot be empty".to_owned()));
    }

    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(ConfigError::Invalid(format!(
            "route id `{id}` may only contain ASCII letters, digits, hyphen, or underscore"
        )));
    }

    Ok(())
}

fn validate_hosts(route_id: &str, hosts: &[String]) -> Result<(), ConfigError> {
    if hosts.is_empty() {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` must define at least one host"
        )));
    }

    for host in hosts {
        if host.trim().is_empty() {
            return Err(ConfigError::Invalid(format!(
                "route `{route_id}` contains an empty host"
            )));
        }

        if host != "*" && (host.contains('/') || host.contains(':')) {
            return Err(ConfigError::Invalid(format!(
                "route `{route_id}` host `{host}` must be `*` or a hostname without scheme/port"
            )));
        }
    }

    Ok(())
}

fn validate_path_prefix(route_id: &str, path_prefix: &str) -> Result<(), ConfigError> {
    if !path_prefix.starts_with('/') {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` path_prefix must start with `/`"
        )));
    }

    if path_prefix.len() > 1 && path_prefix.ends_with('/') {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` path_prefix must not end with `/`"
        )));
    }

    Ok(())
}

fn validate_upstream(route_id: &str, value: &str) -> Result<Upstream, ConfigError> {
    let parsed = Url::parse(value).map_err(|err| {
        ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` is invalid: {err}"
        ))
    })?;

    if parsed.scheme() != "http" {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` must use http in v0.1"
        )));
    }

    if parsed.host_str().is_none() {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` must include a host"
        )));
    }

    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` must be an origin such as http://service:3000"
        )));
    }

    Upstream::parse(value).map_err(|err| ConfigError::Invalid(err.to_string()))
}

fn default_json_logging() -> bool {
    true
}

fn default_timeout_ms() -> u64 {
    3000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_raw_config() -> RawConfig {
        RawConfig {
            server: ServerConfig {
                listen: "127.0.0.1:8080".to_owned(),
            },
            logging: LoggingConfig { json: true },
            routes: vec![RouteConfig {
                id: "users".to_owned(),
                hosts: vec!["*".to_owned()],
                path_prefix: "/api/users".to_owned(),
                upstreams: vec!["http://users-service:3000".to_owned()],
                timeout_ms: 3000,
                retries: 1,
            }],
        }
    }

    #[test]
    fn validates_good_config() {
        let config = valid_raw_config().validate().unwrap();

        assert_eq!(config.listen.to_string(), "127.0.0.1:8080");
        assert_eq!(config.routes.len(), 1);
    }

    #[test]
    fn rejects_duplicate_route_ids() {
        let mut raw = valid_raw_config();
        raw.routes.push(RouteConfig {
            id: "users".to_owned(),
            hosts: vec!["localhost".to_owned()],
            path_prefix: "/other".to_owned(),
            upstreams: vec!["http://other:3000".to_owned()],
            timeout_ms: 3000,
            retries: 0,
        });

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate route id"));
    }

    #[test]
    fn rejects_non_origin_upstream() {
        let mut raw = valid_raw_config();
        raw.routes[0].upstreams = vec!["http://users-service:3000/base".to_owned()];

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("must be an origin"));
    }
}
