use std::{
    collections::{BTreeMap, HashSet},
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::Deserialize;
use thiserror::Error;
use tracegate_core::{
    AppConfig, CaptureConfig, CapturePolicy, ObservabilityConfig, PluginConfig, PluginConfigValue,
    PluginHook, RedactionConfig, Route, StorageConfig, Upstream,
};
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
    pub storage: StorageRawConfig,
    #[serde(default)]
    pub redaction: RedactionRawConfig,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub observability: ObservabilityRawConfig,
    #[serde(default)]
    pub routes: Vec<RouteConfig>,
    #[serde(default)]
    pub plugins: Vec<PluginRawConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServerConfig {
    pub listen: String,
    #[serde(default)]
    pub admin_listen: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LoggingConfig {
    pub json: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct StorageRawConfig {
    #[serde(default = "default_storage_driver")]
    pub driver: String,
    #[serde(default = "default_storage_url")]
    pub url: String,
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    #[serde(default = "default_max_total_capture_bytes")]
    pub max_total_capture_bytes: u64,
    #[serde(default = "default_max_capture_bytes_per_request")]
    pub max_capture_bytes_per_request: u64,
}

impl Default for StorageRawConfig {
    fn default() -> Self {
        let defaults = StorageConfig::default();
        Self {
            driver: defaults.driver,
            url: defaults.url,
            retention_days: defaults.retention_days,
            max_total_capture_bytes: defaults.max_total_capture_bytes,
            max_capture_bytes_per_request: defaults.max_capture_bytes_per_request,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RedactionRawConfig {
    #[serde(default = "default_redaction_headers")]
    pub headers: Vec<String>,
    #[serde(default = "default_redaction_query_params")]
    pub query_params: Vec<String>,
}

impl Default for RedactionRawConfig {
    fn default() -> Self {
        let defaults = RedactionConfig::default();
        Self {
            headers: defaults.headers,
            query_params: defaults.query_params,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ObservabilityRawConfig {
    #[serde(default = "default_service_name")]
    pub service_name: String,
    #[serde(default = "default_environment")]
    pub environment: String,
    #[serde(default)]
    pub otlp_endpoint: Option<String>,
    #[serde(default = "default_prometheus_enabled")]
    pub prometheus_enabled: bool,
    #[serde(default)]
    pub json_logs: Option<bool>,
}

impl Default for ObservabilityRawConfig {
    fn default() -> Self {
        Self {
            service_name: default_service_name(),
            environment: default_environment(),
            otlp_endpoint: None,
            prometheus_enabled: default_prometheus_enabled(),
            json_logs: None,
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
    #[serde(default = "default_capture_policy")]
    pub capture_policy: String,
    #[serde(default = "default_slow_threshold_ms")]
    pub slow_threshold_ms: u64,
    #[serde(default)]
    pub capture_request_body: bool,
    #[serde(default)]
    pub capture_response_body_bytes: u64,
}

#[derive(Debug, Deserialize)]
pub struct PluginRawConfig {
    pub id: String,
    pub path: String,
    #[serde(default = "default_plugin_hook")]
    pub hook: String,
    #[serde(default)]
    pub routes: Vec<String>,
    #[serde(default = "default_plugin_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_plugin_memory_limit_bytes")]
    pub memory_limit_bytes: u64,
    #[serde(default = "default_plugin_fuel")]
    pub fuel: u64,
    #[serde(default)]
    pub body_preview_bytes: u64,
    #[serde(default)]
    pub raw_headers: Vec<String>,
    #[serde(default)]
    pub config: BTreeMap<String, String>,
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
        let admin_listen: SocketAddr = self
            .server
            .admin_listen
            .unwrap_or_else(default_admin_listen)
            .parse()
            .map_err(|err| ConfigError::Invalid(format!("server.admin_listen: {err}")))?;
        let observability = validate_observability(self.logging, self.observability)?;
        let storage = validate_storage(self.storage)?;
        let redaction = validate_redaction(self.redaction)?;

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
            let capture = validate_capture(&route, &storage)?;

            routes.push(Route::new_with_capture(
                route.id,
                route.hosts,
                route.path_prefix,
                upstreams,
                Duration::from_millis(route.timeout_ms),
                route.retries,
                capture,
            ));
        }

        let plugins = validate_plugins(self.plugins, &route_ids)?;

        Ok(AppConfig {
            listen,
            admin_listen,
            storage,
            redaction,
            observability,
            routes,
            plugins,
        })
    }
}

fn validate_storage(raw: StorageRawConfig) -> Result<StorageConfig, ConfigError> {
    let driver = raw.driver.trim().to_ascii_lowercase();
    if driver != "sqlite" {
        return Err(ConfigError::Invalid(format!(
            "storage.driver must be `sqlite` in v0.4, got `{}`",
            raw.driver
        )));
    }

    let url = raw.url.trim().to_owned();
    if url.is_empty() {
        return Err(ConfigError::Invalid(
            "storage.url cannot be empty".to_owned(),
        ));
    }

    if !url.starts_with("sqlite:") {
        return Err(ConfigError::Invalid(format!(
            "storage.url `{url}` must use the sqlite scheme"
        )));
    }

    if raw.retention_days == 0 || raw.retention_days > 365 {
        return Err(ConfigError::Invalid(
            "storage.retention_days must be between 1 and 365".to_owned(),
        ));
    }

    if raw.max_total_capture_bytes == 0 {
        return Err(ConfigError::Invalid(
            "storage.max_total_capture_bytes must be greater than 0".to_owned(),
        ));
    }

    if raw.max_capture_bytes_per_request == 0 {
        return Err(ConfigError::Invalid(
            "storage.max_capture_bytes_per_request must be greater than 0".to_owned(),
        ));
    }

    if raw.max_capture_bytes_per_request > raw.max_total_capture_bytes {
        return Err(ConfigError::Invalid(
            "storage.max_capture_bytes_per_request cannot exceed storage.max_total_capture_bytes"
                .to_owned(),
        ));
    }

    Ok(StorageConfig {
        driver,
        url,
        retention_days: raw.retention_days,
        max_total_capture_bytes: raw.max_total_capture_bytes,
        max_capture_bytes_per_request: raw.max_capture_bytes_per_request,
    })
}

fn validate_redaction(raw: RedactionRawConfig) -> Result<RedactionConfig, ConfigError> {
    let headers = normalize_redaction_list("redaction.headers", raw.headers)?;
    let query_params = normalize_redaction_list("redaction.query_params", raw.query_params)?;

    if headers.is_empty() {
        return Err(ConfigError::Invalid(
            "redaction.headers cannot be empty".to_owned(),
        ));
    }

    if query_params.is_empty() {
        return Err(ConfigError::Invalid(
            "redaction.query_params cannot be empty".to_owned(),
        ));
    }

    Ok(RedactionConfig {
        headers,
        query_params,
    })
}

fn normalize_redaction_list(field: &str, values: Vec<String>) -> Result<Vec<String>, ConfigError> {
    let mut normalized = Vec::with_capacity(values.len());
    let mut seen = HashSet::new();

    for value in values {
        let value = value.trim().to_ascii_lowercase();
        if value.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "{field} cannot contain empty values"
            )));
        }

        if !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err(ConfigError::Invalid(format!(
                "{field} value `{value}` may only contain ASCII letters, digits, hyphen, underscore, or dot"
            )));
        }

        if seen.insert(value.clone()) {
            normalized.push(value);
        }
    }

    Ok(normalized)
}

fn validate_capture(
    route: &RouteConfig,
    storage: &StorageConfig,
) -> Result<CaptureConfig, ConfigError> {
    let policy = match route.capture_policy.trim().to_ascii_lowercase().as_str() {
        "off" => CapturePolicy::Off,
        "errors" => CapturePolicy::Errors,
        "slow" => CapturePolicy::Slow,
        "errors_and_slow" => CapturePolicy::ErrorsAndSlow,
        "always" => CapturePolicy::Always,
        value => {
            return Err(ConfigError::Invalid(format!(
                "route `{}` capture_policy `{value}` must be one of off, errors, slow, errors_and_slow, always",
                route.id
            )));
        }
    };

    if route.slow_threshold_ms == 0 || route.slow_threshold_ms > 600_000 {
        return Err(ConfigError::Invalid(format!(
            "route `{}` slow_threshold_ms must be between 1 and 600000",
            route.id
        )));
    }

    if route.capture_response_body_bytes > storage.max_capture_bytes_per_request {
        return Err(ConfigError::Invalid(format!(
            "route `{}` capture_response_body_bytes cannot exceed storage.max_capture_bytes_per_request",
            route.id
        )));
    }

    Ok(CaptureConfig {
        policy,
        slow_threshold: Duration::from_millis(route.slow_threshold_ms),
        capture_request_body: route.capture_request_body,
        capture_response_body_bytes: route.capture_response_body_bytes,
    })
}

fn validate_observability(
    logging: LoggingConfig,
    raw: ObservabilityRawConfig,
) -> Result<ObservabilityConfig, ConfigError> {
    let service_name = raw.service_name.trim().to_owned();
    if service_name.is_empty() {
        return Err(ConfigError::Invalid(
            "observability.service_name cannot be empty".to_owned(),
        ));
    }

    let environment = raw.environment.trim().to_owned();
    if environment.is_empty() {
        return Err(ConfigError::Invalid(
            "observability.environment cannot be empty".to_owned(),
        ));
    }

    let otlp_endpoint = raw
        .otlp_endpoint
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(|value| {
            let parsed = Url::parse(&value).map_err(|err| {
                ConfigError::Invalid(format!("observability.otlp_endpoint `{value}`: {err}"))
            })?;
            match parsed.scheme() {
                "http" | "https" => Ok(value),
                scheme => Err(ConfigError::Invalid(format!(
                    "observability.otlp_endpoint `{value}` must use http or https, got `{scheme}`"
                ))),
            }
        })
        .transpose()?;

    let json_logs = raw
        .json_logs
        .or(logging.json)
        .unwrap_or_else(default_json_logging);

    Ok(ObservabilityConfig {
        service_name,
        environment,
        otlp_endpoint,
        prometheus_enabled: raw.prometheus_enabled,
        json_logs,
    })
}

fn validate_plugins(
    raw_plugins: Vec<PluginRawConfig>,
    route_ids: &HashSet<String>,
) -> Result<Vec<PluginConfig>, ConfigError> {
    let mut plugin_ids = HashSet::new();
    let mut plugins = Vec::with_capacity(raw_plugins.len());

    for plugin in raw_plugins {
        validate_plugin_id(&plugin.id)?;
        if !plugin_ids.insert(plugin.id.clone()) {
            return Err(ConfigError::Invalid(format!(
                "duplicate plugin id `{}`",
                plugin.id
            )));
        }

        let path = plugin.path.trim();
        if path.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` path cannot be empty",
                plugin.id
            )));
        }

        let hook = match plugin.hook.trim().to_ascii_lowercase().as_str() {
            "before_request" => PluginHook::BeforeRequest,
            value => {
                return Err(ConfigError::Invalid(format!(
                    "plugin `{}` hook `{value}` must be before_request",
                    plugin.id
                )));
            }
        };

        if plugin.routes.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` must target at least one route",
                plugin.id
            )));
        }

        let mut routes = Vec::with_capacity(plugin.routes.len());
        let mut seen_routes = HashSet::new();
        for route in plugin.routes {
            validate_route_id(&route)?;
            if !route_ids.contains(&route) {
                return Err(ConfigError::Invalid(format!(
                    "plugin `{}` references unknown route `{route}`",
                    plugin.id
                )));
            }
            if seen_routes.insert(route.clone()) {
                routes.push(route);
            }
        }

        if plugin.timeout_ms == 0 || plugin.timeout_ms > 5_000 {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` timeout_ms must be between 1 and 5000",
                plugin.id
            )));
        }

        if plugin.memory_limit_bytes < 65_536 || plugin.memory_limit_bytes > 134_217_728 {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` memory_limit_bytes must be between 65536 and 134217728",
                plugin.id
            )));
        }

        if plugin.fuel == 0 {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` fuel must be greater than 0",
                plugin.id
            )));
        }

        if plugin.body_preview_bytes > 65_536 {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` body_preview_bytes cannot exceed 65536",
                plugin.id
            )));
        }

        let raw_headers = normalize_redaction_list(
            &format!("plugin `{}` raw_headers", plugin.id),
            plugin.raw_headers,
        )?;
        let config = validate_plugin_config(&plugin.id, plugin.config)?;

        plugins.push(PluginConfig {
            id: plugin.id,
            path: PathBuf::from(path),
            hook,
            routes,
            timeout: Duration::from_millis(plugin.timeout_ms),
            memory_limit_bytes: plugin.memory_limit_bytes,
            fuel: plugin.fuel,
            body_preview_bytes: plugin.body_preview_bytes,
            raw_headers,
            config,
        });
    }

    Ok(plugins)
}

fn validate_plugin_id(id: &str) -> Result<(), ConfigError> {
    if id.is_empty() {
        return Err(ConfigError::Invalid("plugin id cannot be empty".to_owned()));
    }

    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(ConfigError::Invalid(format!(
            "plugin id `{id}` may only contain ASCII letters, digits, hyphen, or underscore"
        )));
    }

    Ok(())
}

fn validate_plugin_config(
    plugin_id: &str,
    raw: BTreeMap<String, String>,
) -> Result<Vec<PluginConfigValue>, ConfigError> {
    let mut values = Vec::with_capacity(raw.len());

    for (key, value) in raw {
        let key = key.trim().to_owned();
        if key.is_empty() {
            return Err(ConfigError::Invalid(format!(
                "plugin `{plugin_id}` config key cannot be empty"
            )));
        }
        if !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err(ConfigError::Invalid(format!(
                "plugin `{plugin_id}` config key `{key}` may only contain ASCII letters, digits, hyphen, underscore, or dot"
            )));
        }
        if value.chars().count() > 4096 {
            return Err(ConfigError::Invalid(format!(
                "plugin `{plugin_id}` config value `{key}` cannot exceed 4096 characters"
            )));
        }
        values.push(PluginConfigValue { key, value });
    }

    Ok(values)
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

fn default_storage_driver() -> String {
    StorageConfig::default().driver
}

fn default_storage_url() -> String {
    StorageConfig::default().url
}

fn default_retention_days() -> u32 {
    StorageConfig::default().retention_days
}

fn default_max_total_capture_bytes() -> u64 {
    StorageConfig::default().max_total_capture_bytes
}

fn default_max_capture_bytes_per_request() -> u64 {
    StorageConfig::default().max_capture_bytes_per_request
}

fn default_redaction_headers() -> Vec<String> {
    RedactionConfig::default().headers
}

fn default_redaction_query_params() -> Vec<String> {
    RedactionConfig::default().query_params
}

fn default_admin_listen() -> String {
    "127.0.0.1:9090".to_owned()
}

fn default_service_name() -> String {
    "tracegate".to_owned()
}

fn default_environment() -> String {
    "local".to_owned()
}

fn default_prometheus_enabled() -> bool {
    true
}

fn default_timeout_ms() -> u64 {
    3000
}

fn default_capture_policy() -> String {
    "off".to_owned()
}

fn default_slow_threshold_ms() -> u64 {
    500
}

fn default_plugin_hook() -> String {
    "before_request".to_owned()
}

fn default_plugin_timeout_ms() -> u64 {
    5
}

fn default_plugin_memory_limit_bytes() -> u64 {
    16 * 1024 * 1024
}

fn default_plugin_fuel() -> u64 {
    10_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_raw_config() -> RawConfig {
        RawConfig {
            server: ServerConfig {
                listen: "127.0.0.1:8080".to_owned(),
                admin_listen: None,
            },
            storage: StorageRawConfig::default(),
            redaction: RedactionRawConfig::default(),
            logging: LoggingConfig { json: Some(true) },
            observability: ObservabilityRawConfig::default(),
            routes: vec![RouteConfig {
                id: "users".to_owned(),
                hosts: vec!["*".to_owned()],
                path_prefix: "/api/users".to_owned(),
                upstreams: vec!["http://users-service:3000".to_owned()],
                timeout_ms: 3000,
                retries: 1,
                capture_policy: "off".to_owned(),
                slow_threshold_ms: 500,
                capture_request_body: false,
                capture_response_body_bytes: 0,
            }],
            plugins: Vec::new(),
        }
    }

    #[test]
    fn validates_good_config() {
        let config = valid_raw_config().validate().unwrap();

        assert_eq!(config.listen.to_string(), "127.0.0.1:8080");
        assert_eq!(config.admin_listen.to_string(), "127.0.0.1:9090");
        assert_eq!(config.storage.driver, "sqlite");
        assert_eq!(config.storage.retention_days, 7);
        assert!(config.redaction.is_sensitive_header("Authorization"));
        assert!(config.redaction.is_sensitive_query_param("ACCESS_TOKEN"));
        assert_eq!(config.observability.service_name, "tracegate");
        assert!(config.observability.prometheus_enabled);
        assert_eq!(config.routes.len(), 1);
        assert_eq!(config.routes[0].capture.policy, CapturePolicy::Off);
        assert!(config.plugins.is_empty());
    }

    #[test]
    fn parses_v2_observability_config() {
        let raw = r#"
[server]
listen = "127.0.0.1:8080"
admin_listen = "127.0.0.1:9091"

[observability]
service_name = "tracegate-test"
environment = "test"
otlp_endpoint = "http://otel-collector:4317"
prometheus_enabled = true
json_logs = false

[[routes]]
id = "users"
hosts = ["*"]
path_prefix = "/api/users"
upstreams = ["http://users-service:3000"]
"#;

        let config = toml::from_str::<RawConfig>(raw)
            .unwrap()
            .validate()
            .unwrap();

        assert_eq!(config.admin_listen.to_string(), "127.0.0.1:9091");
        assert_eq!(config.observability.service_name, "tracegate-test");
        assert_eq!(config.observability.environment, "test");
        assert_eq!(
            config.observability.otlp_endpoint.as_deref(),
            Some("http://otel-collector:4317")
        );
        assert!(!config.observability.json_logs);
    }

    #[test]
    fn parses_v3_capture_store_config() {
        let raw = r#"
[server]
listen = "127.0.0.1:8080"

[storage]
driver = "sqlite"
url = "sqlite://tracegate-test.db"
retention_days = 3
max_total_capture_bytes = 4096
max_capture_bytes_per_request = 2048

[redaction]
headers = ["Authorization", "Cookie", "X-Api-Key"]
query_params = ["token", "api_key"]

[[routes]]
id = "payments"
hosts = ["*"]
path_prefix = "/api/payments"
upstreams = ["http://payments-service:4000"]
capture_policy = "errors_and_slow"
slow_threshold_ms = 250
capture_request_body = true
capture_response_body_bytes = 1024
"#;

        let config = toml::from_str::<RawConfig>(raw)
            .unwrap()
            .validate()
            .unwrap();

        assert_eq!(config.storage.url, "sqlite://tracegate-test.db");
        assert_eq!(config.storage.retention_days, 3);
        assert!(config.redaction.is_sensitive_header("authorization"));
        assert!(config.redaction.is_sensitive_query_param("API_KEY"));
        assert_eq!(
            config.routes[0].capture.policy,
            CapturePolicy::ErrorsAndSlow
        );
        assert_eq!(
            config.routes[0].capture.slow_threshold,
            Duration::from_millis(250)
        );
        assert!(config.routes[0].capture.capture_request_body);
        assert_eq!(config.routes[0].capture.capture_response_body_bytes, 1024);
    }

    #[test]
    fn parses_v5_plugin_config() {
        let raw = r#"
[server]
listen = "127.0.0.1:8080"

[[routes]]
id = "payments"
hosts = ["*"]
path_prefix = "/api/payments"
upstreams = ["http://payments-service:4000"]

[[plugins]]
id = "api-key-guard"
path = "/usr/local/share/tracegate/plugins/api-key-guard.wasm"
hook = "before_request"
routes = ["payments"]
timeout_ms = 10
memory_limit_bytes = 16777216
fuel = 1000000
body_preview_bytes = 1024
raw_headers = ["X-API-Key"]
config = { header = "x-api-key", expected = "demo-key" }
"#;

        let config = toml::from_str::<RawConfig>(raw)
            .unwrap()
            .validate()
            .unwrap();

        assert_eq!(config.plugins.len(), 1);
        let plugin = &config.plugins[0];
        assert_eq!(plugin.id, "api-key-guard");
        assert_eq!(plugin.hook, PluginHook::BeforeRequest);
        assert_eq!(plugin.routes, vec!["payments"]);
        assert_eq!(plugin.raw_headers, vec!["x-api-key"]);
        assert_eq!(plugin.body_preview_bytes, 1024);
        assert!(plugin.config.iter().any(|item| item.key == "expected"));
    }

    #[test]
    fn rejects_invalid_admin_listen() {
        let mut raw = valid_raw_config();
        raw.server.admin_listen = Some("not-an-address".to_owned());

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("server.admin_listen"));
    }

    #[test]
    fn rejects_empty_service_name() {
        let mut raw = valid_raw_config();
        raw.observability.service_name = " ".to_owned();

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("observability.service_name"));
    }

    #[test]
    fn rejects_malformed_otlp_endpoint() {
        let mut raw = valid_raw_config();
        raw.observability.otlp_endpoint = Some("otel-collector:4317".to_owned());

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("observability.otlp_endpoint"));
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
            capture_policy: "off".to_owned(),
            slow_threshold_ms: 500,
            capture_request_body: false,
            capture_response_body_bytes: 0,
        });

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("duplicate route id"));
    }

    #[test]
    fn rejects_invalid_storage_caps() {
        let mut raw = valid_raw_config();
        raw.storage.max_total_capture_bytes = 1024;
        raw.storage.max_capture_bytes_per_request = 2048;

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("max_capture_bytes_per_request"));
    }

    #[test]
    fn rejects_route_capture_larger_than_storage_cap() {
        let mut raw = valid_raw_config();
        raw.storage.max_capture_bytes_per_request = 1024;
        raw.routes[0].capture_response_body_bytes = 2048;

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("capture_response_body_bytes"));
    }

    #[test]
    fn rejects_non_origin_upstream() {
        let mut raw = valid_raw_config();
        raw.routes[0].upstreams = vec!["http://users-service:3000/base".to_owned()];

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("must be an origin"));
    }

    #[test]
    fn rejects_plugin_unknown_route() {
        let mut raw = valid_raw_config();
        raw.plugins.push(PluginRawConfig {
            id: "guard".to_owned(),
            path: "/tmp/guard.wasm".to_owned(),
            hook: "before_request".to_owned(),
            routes: vec!["payments".to_owned()],
            timeout_ms: 5,
            memory_limit_bytes: 16 * 1024 * 1024,
            fuel: 1_000_000,
            body_preview_bytes: 0,
            raw_headers: Vec::new(),
            config: BTreeMap::new(),
        });

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("unknown route"));
    }
}
