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
    AdminConfig, AppConfig, CaptureConfig, CapturePolicy, ObservabilityConfig, PluginConfig,
    PluginConfigValue, PluginHook, RedactionConfig, Route, RouteOptions, RuntimeMode,
    StorageConfig, TlsConfig, Upstream, UpstreamTlsConfig,
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
    pub admin: AdminRawConfig,
    #[serde(default)]
    pub upstream_tls: UpstreamTlsRawConfig,
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
    #[serde(default = "default_mode")]
    pub mode: String,
    pub listen: String,
    #[serde(default)]
    pub admin_listen: Option<String>,
    #[serde(default)]
    pub tls: TlsRawConfig,
}

#[derive(Debug, Default, Deserialize)]
pub struct TlsRawConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub cert_path: Option<String>,
    #[serde(default)]
    pub key_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AdminRawConfig {
    #[serde(default = "default_admin_token_env")]
    pub token_env: Option<String>,
    #[serde(default)]
    pub allow_internal_network: bool,
}

impl Default for AdminRawConfig {
    fn default() -> Self {
        Self {
            token_env: default_admin_token_env(),
            allow_internal_network: false,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct UpstreamTlsRawConfig {
    #[serde(default)]
    pub ca_cert_path: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LoggingConfig {
    pub json: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct StorageRawConfig {
    #[serde(default = "default_storage_driver")]
    pub driver: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub url_env: Option<String>,
    #[serde(default)]
    pub retention_days: Option<u32>,
    #[serde(default)]
    pub max_total_capture_bytes: Option<u64>,
    #[serde(default)]
    pub max_capture_bytes_per_request: Option<u64>,
    #[serde(default)]
    pub capture_queue_capacity: Option<usize>,
}

impl Default for StorageRawConfig {
    fn default() -> Self {
        let defaults = StorageConfig::default();
        Self {
            driver: defaults.driver,
            url: Some(defaults.url),
            url_env: None,
            retention_days: Some(defaults.retention_days),
            max_total_capture_bytes: Some(defaults.max_total_capture_bytes),
            max_capture_bytes_per_request: Some(defaults.max_capture_bytes_per_request),
            capture_queue_capacity: Some(defaults.capture_queue_capacity),
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
    #[serde(default = "default_concurrency_limit")]
    pub concurrency_limit: usize,
    #[serde(default = "default_passive_health_failures")]
    pub passive_health_failures: u32,
    #[serde(default = "default_passive_health_cooldown_ms")]
    pub passive_health_cooldown_ms: u64,
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
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub memory_limit_bytes: Option<u64>,
    #[serde(default)]
    pub fuel: Option<u64>,
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
        let mode = validate_mode(&self.server.mode)?;
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
        let server_tls = validate_server_tls(self.server.tls, mode)?;
        let admin = validate_admin(self.admin, admin_listen, mode)?;
        let upstream_tls = validate_upstream_tls(self.upstream_tls)?;
        let observability = validate_observability(self.logging, self.observability)?;
        let storage = validate_storage(self.storage, mode)?;
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

            if route.concurrency_limit == 0 || route.concurrency_limit > 100_000 {
                return Err(ConfigError::Invalid(format!(
                    "route `{}` concurrency_limit must be between 1 and 100000",
                    route.id
                )));
            }

            if route.passive_health_failures == 0 || route.passive_health_failures > 100 {
                return Err(ConfigError::Invalid(format!(
                    "route `{}` passive_health_failures must be between 1 and 100",
                    route.id
                )));
            }

            if route.passive_health_cooldown_ms == 0 || route.passive_health_cooldown_ms > 600_000 {
                return Err(ConfigError::Invalid(format!(
                    "route `{}` passive_health_cooldown_ms must be between 1 and 600000",
                    route.id
                )));
            }

            let upstreams = route
                .upstreams
                .iter()
                .map(|upstream| validate_upstream(&route.id, upstream, mode, admin_listen))
                .collect::<Result<Vec<_>, _>>()?;
            let capture = validate_capture(&route, &storage)?;

            routes.push(Route::new_with_options(
                route.id,
                route.hosts,
                route.path_prefix,
                upstreams,
                RouteOptions {
                    timeout: Duration::from_millis(route.timeout_ms),
                    retries: route.retries,
                    capture,
                    concurrency_limit: route.concurrency_limit,
                    passive_health_failures: route.passive_health_failures,
                    passive_health_cooldown: Duration::from_millis(
                        route.passive_health_cooldown_ms,
                    ),
                },
            ));
        }

        let plugins = validate_plugins(self.plugins, &route_ids, mode)?;

        Ok(AppConfig {
            mode,
            listen,
            admin_listen,
            server_tls,
            admin,
            upstream_tls,
            storage,
            redaction,
            observability,
            routes,
            plugins,
        })
    }
}

fn validate_mode(raw: &str) -> Result<RuntimeMode, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "demo" => Ok(RuntimeMode::Demo),
        "production" => Ok(RuntimeMode::Production),
        value => Err(ConfigError::Invalid(format!(
            "server.mode `{value}` must be demo or production"
        ))),
    }
}

fn validate_server_tls(raw: TlsRawConfig, mode: RuntimeMode) -> Result<TlsConfig, ConfigError> {
    if mode.is_production() && !raw.enabled {
        return Err(ConfigError::Invalid(
            "server.tls.enabled must be true in production mode".to_owned(),
        ));
    }

    let cert_path = normalize_optional_path(raw.cert_path);
    let key_path = normalize_optional_path(raw.key_path);

    if raw.enabled {
        let cert = cert_path.as_ref().ok_or_else(|| {
            ConfigError::Invalid("server.tls.cert_path is required when TLS is enabled".to_owned())
        })?;
        let key = key_path.as_ref().ok_or_else(|| {
            ConfigError::Invalid("server.tls.key_path is required when TLS is enabled".to_owned())
        })?;
        require_readable_file("server.tls.cert_path", cert)?;
        require_readable_file("server.tls.key_path", key)?;
    }

    Ok(TlsConfig {
        enabled: raw.enabled,
        cert_path,
        key_path,
    })
}

fn validate_admin(
    raw: AdminRawConfig,
    admin_listen: SocketAddr,
    mode: RuntimeMode,
) -> Result<AdminConfig, ConfigError> {
    if mode.is_production() && !admin_listen.ip().is_loopback() && !raw.allow_internal_network {
        return Err(ConfigError::Invalid(
            "admin.allow_internal_network must be true when production admin_listen is non-loopback"
                .to_owned(),
        ));
    }

    let token_env = raw
        .token_env
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let token = match token_env.as_deref() {
        Some(name) => std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty()),
        None => None,
    };

    if mode.is_production() && token.is_none() {
        let name = token_env.as_deref().unwrap_or("TRACEGATE_ADMIN_TOKEN");
        return Err(ConfigError::Invalid(format!(
            "admin token env `{name}` must contain a non-empty token in production mode"
        )));
    }

    Ok(AdminConfig {
        token_env,
        token,
        allow_internal_network: raw.allow_internal_network,
    })
}

fn validate_upstream_tls(raw: UpstreamTlsRawConfig) -> Result<UpstreamTlsConfig, ConfigError> {
    let ca_cert_path = normalize_optional_path(raw.ca_cert_path);
    if let Some(path) = ca_cert_path.as_ref() {
        require_readable_file("upstream_tls.ca_cert_path", path)?;
    }
    Ok(UpstreamTlsConfig { ca_cert_path })
}

fn validate_storage(
    raw: StorageRawConfig,
    mode: RuntimeMode,
) -> Result<StorageConfig, ConfigError> {
    let driver = raw.driver.trim().to_ascii_lowercase();
    if driver != "sqlite" && driver != "postgres" {
        return Err(ConfigError::Invalid(format!(
            "storage.driver must be `sqlite` or `postgres`, got `{}`",
            raw.driver
        )));
    }

    let explicit_url = raw
        .url
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let url_env = raw
        .url_env
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let url = match (explicit_url, url_env) {
        (Some(url), None) => url,
        (None, Some(env_name)) => std::env::var(&env_name)
            .map(|value| value.trim().to_owned())
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ConfigError::Invalid(format!(
                    "storage.url_env `{env_name}` must contain a non-empty URL"
                ))
            })?,
        (Some(_), Some(_)) => {
            return Err(ConfigError::Invalid(
                "storage.url and storage.url_env cannot both be set".to_owned(),
            ));
        }
        (None, None) => StorageConfig::default().url,
    };

    if url.is_empty() {
        return Err(ConfigError::Invalid(
            "storage.url cannot be empty".to_owned(),
        ));
    }

    if driver == "sqlite" && !url.starts_with("sqlite:") {
        return Err(ConfigError::Invalid(format!(
            "storage.url `{url}` must use the sqlite scheme"
        )));
    }

    if driver == "postgres" && !url.starts_with("postgres://") && !url.starts_with("postgresql://")
    {
        return Err(ConfigError::Invalid(format!(
            "storage.url `{url}` must use the postgres scheme"
        )));
    }

    let retention_days = required_or_default(
        "storage.retention_days",
        raw.retention_days,
        StorageConfig::default().retention_days,
        mode,
    )?;
    let max_total_capture_bytes = required_or_default(
        "storage.max_total_capture_bytes",
        raw.max_total_capture_bytes,
        StorageConfig::default().max_total_capture_bytes,
        mode,
    )?;
    let max_capture_bytes_per_request = required_or_default(
        "storage.max_capture_bytes_per_request",
        raw.max_capture_bytes_per_request,
        StorageConfig::default().max_capture_bytes_per_request,
        mode,
    )?;
    let capture_queue_capacity = required_or_default(
        "storage.capture_queue_capacity",
        raw.capture_queue_capacity,
        StorageConfig::default().capture_queue_capacity,
        mode,
    )?;

    if retention_days == 0 || retention_days > 365 {
        return Err(ConfigError::Invalid(
            "storage.retention_days must be between 1 and 365".to_owned(),
        ));
    }

    if max_total_capture_bytes == 0 {
        return Err(ConfigError::Invalid(
            "storage.max_total_capture_bytes must be greater than 0".to_owned(),
        ));
    }

    if max_capture_bytes_per_request == 0 {
        return Err(ConfigError::Invalid(
            "storage.max_capture_bytes_per_request must be greater than 0".to_owned(),
        ));
    }

    if max_capture_bytes_per_request > max_total_capture_bytes {
        return Err(ConfigError::Invalid(
            "storage.max_capture_bytes_per_request cannot exceed storage.max_total_capture_bytes"
                .to_owned(),
        ));
    }

    if capture_queue_capacity == 0 || capture_queue_capacity > 1_000_000 {
        return Err(ConfigError::Invalid(
            "storage.capture_queue_capacity must be between 1 and 1000000".to_owned(),
        ));
    }

    Ok(StorageConfig {
        driver,
        url,
        retention_days,
        max_total_capture_bytes,
        max_capture_bytes_per_request,
        capture_queue_capacity,
    })
}

fn required_or_default<T: Copy>(
    field: &str,
    value: Option<T>,
    default: T,
    mode: RuntimeMode,
) -> Result<T, ConfigError> {
    match (value, mode.is_production()) {
        (Some(value), _) => Ok(value),
        (None, false) => Ok(default),
        (None, true) => Err(ConfigError::Invalid(format!(
            "{field} must be explicitly set in production mode"
        ))),
    }
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
    mode: RuntimeMode,
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

        let timeout_ms = required_or_default(
            &format!("plugin `{}` timeout_ms", plugin.id),
            plugin.timeout_ms,
            default_plugin_timeout_ms(),
            mode,
        )?;
        let memory_limit_bytes = required_or_default(
            &format!("plugin `{}` memory_limit_bytes", plugin.id),
            plugin.memory_limit_bytes,
            default_plugin_memory_limit_bytes(),
            mode,
        )?;
        let fuel = required_or_default(
            &format!("plugin `{}` fuel", plugin.id),
            plugin.fuel,
            default_plugin_fuel(),
            mode,
        )?;

        if timeout_ms == 0 || timeout_ms > 5_000 {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` timeout_ms must be between 1 and 5000",
                plugin.id
            )));
        }

        if !(65_536..=134_217_728).contains(&memory_limit_bytes) {
            return Err(ConfigError::Invalid(format!(
                "plugin `{}` memory_limit_bytes must be between 65536 and 134217728",
                plugin.id
            )));
        }

        if fuel == 0 {
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
            timeout: Duration::from_millis(timeout_ms),
            memory_limit_bytes,
            fuel,
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

fn validate_upstream(
    route_id: &str,
    value: &str,
    mode: RuntimeMode,
    admin_listen: SocketAddr,
) -> Result<Upstream, ConfigError> {
    let parsed = Url::parse(value).map_err(|err| {
        ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` is invalid: {err}"
        ))
    })?;

    if mode.is_production() && parsed.scheme() != "https" {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` must use https in production mode"
        )));
    }

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` must use http or https"
        )));
    }

    let host = parsed.host_str().ok_or_else(|| {
        ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` must include a host"
        ))
    })?;

    if mode.is_production()
        && is_blocked_upstream_host(host, parsed.port_or_known_default(), admin_listen)
    {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` targets a blocked local, metadata, or admin address"
        )));
    }

    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ConfigError::Invalid(format!(
            "route `{route_id}` upstream `{value}` must be an origin such as http://service:3000"
        )));
    }

    Upstream::parse(value).map_err(|err| ConfigError::Invalid(err.to_string()))
}

fn is_blocked_upstream_host(host: &str, port: Option<u16>, admin_listen: SocketAddr) -> bool {
    let host = host.trim_matches(['[', ']']).to_ascii_lowercase();
    if matches!(
        host.as_str(),
        "localhost" | "metadata.google.internal" | "169.254.169.254" | "::1"
    ) {
        return true;
    }
    if host.starts_with("127.") || host == "0.0.0.0" {
        return true;
    }
    match (host.parse::<std::net::IpAddr>(), port) {
        (Ok(ip), Some(port)) => admin_listen.ip() == ip && admin_listen.port() == port,
        _ => false,
    }
}

fn normalize_optional_path(value: Option<String>) -> Option<PathBuf> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn require_readable_file(field: &str, path: &Path) -> Result<(), ConfigError> {
    let metadata = fs::metadata(path).map_err(|err| {
        ConfigError::Invalid(format!(
            "{field} `{}` is not readable: {err}",
            path.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(ConfigError::Invalid(format!(
            "{field} `{}` must be a file",
            path.display()
        )));
    }
    Ok(())
}

fn default_mode() -> String {
    "demo".to_owned()
}

fn default_json_logging() -> bool {
    true
}

fn default_storage_driver() -> String {
    StorageConfig::default().driver
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

fn default_admin_token_env() -> Option<String> {
    Some("TRACEGATE_ADMIN_TOKEN".to_owned())
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

fn default_concurrency_limit() -> usize {
    100
}

fn default_passive_health_failures() -> u32 {
    3
}

fn default_passive_health_cooldown_ms() -> u64 {
    10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_raw_config() -> RawConfig {
        RawConfig {
            server: ServerConfig {
                mode: "demo".to_owned(),
                listen: "127.0.0.1:8080".to_owned(),
                admin_listen: None,
                tls: TlsRawConfig::default(),
            },
            admin: AdminRawConfig::default(),
            upstream_tls: UpstreamTlsRawConfig::default(),
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
                concurrency_limit: 100,
                passive_health_failures: 3,
                passive_health_cooldown_ms: 10_000,
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
            concurrency_limit: 100,
            passive_health_failures: 3,
            passive_health_cooldown_ms: 10_000,
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
        raw.storage.max_total_capture_bytes = Some(1024);
        raw.storage.max_capture_bytes_per_request = Some(2048);

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("max_capture_bytes_per_request"));
    }

    #[test]
    fn rejects_route_capture_larger_than_storage_cap() {
        let mut raw = valid_raw_config();
        raw.storage.max_capture_bytes_per_request = Some(1024);
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
            timeout_ms: Some(5),
            memory_limit_bytes: Some(16 * 1024 * 1024),
            fuel: Some(1_000_000),
            body_preview_bytes: 0,
            raw_headers: Vec::new(),
            config: BTreeMap::new(),
        });

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("unknown route"));
    }

    #[test]
    fn rejects_production_without_admin_token() {
        let mut raw = valid_raw_config();
        raw.server.mode = "production".to_owned();
        raw.server.tls.enabled = true;
        raw.server.tls.cert_path = Some("missing.crt".to_owned());
        raw.server.tls.key_path = Some("missing.key".to_owned());

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("server.tls.cert_path") || err.contains("admin token"));
    }

    #[test]
    fn rejects_production_http_upstream() {
        let dir = tempfile::tempdir().unwrap();
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::write(&cert, "not a real cert").unwrap();
        std::fs::write(&key, "not a real key").unwrap();

        let mut raw = valid_raw_config();
        raw.server.mode = "production".to_owned();
        raw.server.tls.enabled = true;
        raw.server.tls.cert_path = Some(cert.display().to_string());
        raw.server.tls.key_path = Some(key.display().to_string());
        raw.admin.token_env = Some("TRACEGATE_CONFIG_TEST_TOKEN".to_owned());
        unsafe {
            std::env::set_var("TRACEGATE_CONFIG_TEST_TOKEN", "secret");
        }
        raw.storage.retention_days = Some(7);
        raw.storage.max_total_capture_bytes = Some(4096);
        raw.storage.max_capture_bytes_per_request = Some(1024);
        raw.storage.capture_queue_capacity = Some(16);

        let err = raw.validate().unwrap_err().to_string();
        assert!(err.contains("must use https in production mode"));
    }
}
