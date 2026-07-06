use std::{
    collections::HashSet,
    fmt,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use http::{
    HeaderMap, Uri,
    header::{HeaderName, HeaderValue},
};
use thiserror::Error;
use uuid::Uuid;

pub const REQUEST_ID_HEADER: &str = "x-request-id";
pub const FORWARDED_HOST_HEADER: &str = "x-forwarded-host";
pub const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";
pub const FORWARDED_PROTO_HEADER: &str = "x-forwarded-proto";

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("invalid upstream URI `{value}`: {reason}")]
    InvalidUpstream { value: String, reason: String },
}

#[derive(Clone, Debug)]
pub struct AppConfig {
    pub listen: SocketAddr,
    pub admin_listen: SocketAddr,
    pub storage: StorageConfig,
    pub redaction: RedactionConfig,
    pub observability: ObservabilityConfig,
    pub routes: Vec<Route>,
}

#[derive(Clone, Debug)]
pub struct ObservabilityConfig {
    pub service_name: String,
    pub environment: String,
    pub otlp_endpoint: Option<String>,
    pub prometheus_enabled: bool,
    pub json_logs: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageConfig {
    pub driver: String,
    pub url: String,
    pub retention_days: u32,
    pub max_total_capture_bytes: u64,
    pub max_capture_bytes_per_request: u64,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            driver: "sqlite".to_owned(),
            url: "sqlite://tracegate.db".to_owned(),
            retention_days: 7,
            max_total_capture_bytes: 1_073_741_824,
            max_capture_bytes_per_request: 1_048_576,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RedactionConfig {
    pub headers: Vec<String>,
    pub query_params: Vec<String>,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            headers: vec![
                "authorization".to_owned(),
                "cookie".to_owned(),
                "set-cookie".to_owned(),
                "x-api-key".to_owned(),
            ],
            query_params: vec![
                "token".to_owned(),
                "access_token".to_owned(),
                "api_key".to_owned(),
            ],
        }
    }
}

impl RedactionConfig {
    pub fn is_sensitive_header(&self, name: &str) -> bool {
        self.headers
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(name))
    }

    pub fn is_sensitive_query_param(&self, name: &str) -> bool {
        self.query_params
            .iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(name))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapturePolicy {
    Off,
    Errors,
    Slow,
    ErrorsAndSlow,
    Always,
}

impl CapturePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Errors => "errors",
            Self::Slow => "slow",
            Self::ErrorsAndSlow => "errors_and_slow",
            Self::Always => "always",
        }
    }

    pub fn should_capture(self, is_error: bool, is_slow: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Errors => is_error,
            Self::Slow => is_slow,
            Self::ErrorsAndSlow => is_error || is_slow,
            Self::Always => true,
        }
    }
}

impl fmt::Display for CapturePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone, Debug)]
pub struct CaptureConfig {
    pub policy: CapturePolicy,
    pub slow_threshold: Duration,
    pub capture_request_body: bool,
    pub capture_response_body_bytes: u64,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            policy: CapturePolicy::Off,
            slow_threshold: Duration::from_millis(500),
            capture_request_body: false,
            capture_response_body_bytes: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Route {
    pub id: String,
    pub hosts: Vec<String>,
    pub path_prefix: String,
    pub upstreams: Vec<Upstream>,
    pub timeout: Duration,
    pub retries: u32,
    pub capture: CaptureConfig,
    next_upstream: Arc<AtomicUsize>,
}

impl Route {
    pub fn new(
        id: impl Into<String>,
        hosts: Vec<String>,
        path_prefix: impl Into<String>,
        upstreams: Vec<Upstream>,
        timeout: Duration,
        retries: u32,
    ) -> Self {
        Self::new_with_capture(
            id,
            hosts,
            path_prefix,
            upstreams,
            timeout,
            retries,
            CaptureConfig::default(),
        )
    }

    pub fn new_with_capture(
        id: impl Into<String>,
        hosts: Vec<String>,
        path_prefix: impl Into<String>,
        upstreams: Vec<Upstream>,
        timeout: Duration,
        retries: u32,
        capture: CaptureConfig,
    ) -> Self {
        Self {
            id: id.into(),
            hosts,
            path_prefix: path_prefix.into(),
            upstreams,
            timeout,
            retries,
            capture,
            next_upstream: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn select_upstream(&self) -> Upstream {
        let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
        self.upstreams[index % self.upstreams.len()].clone()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Upstream {
    pub uri: Uri,
    pub scheme: String,
    pub authority: String,
}

impl Upstream {
    pub fn parse(value: &str) -> Result<Self, CoreError> {
        let uri: Uri = value.parse().map_err(|err| CoreError::InvalidUpstream {
            value: value.to_owned(),
            reason: format!("{err}"),
        })?;

        let scheme = uri
            .scheme_str()
            .ok_or_else(|| CoreError::InvalidUpstream {
                value: value.to_owned(),
                reason: "missing scheme".to_owned(),
            })?
            .to_owned();

        let authority = uri
            .authority()
            .ok_or_else(|| CoreError::InvalidUpstream {
                value: value.to_owned(),
                reason: "missing authority".to_owned(),
            })?
            .as_str()
            .to_owned();

        Ok(Self {
            uri,
            scheme,
            authority,
        })
    }

    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.authority)
    }
}

#[derive(Clone, Debug)]
pub struct MatchedRoute {
    pub route: Route,
}

#[derive(Clone, Debug)]
pub struct Router {
    routes: Vec<Route>,
}

impl Router {
    pub fn new(routes: Vec<Route>) -> Self {
        Self { routes }
    }

    pub fn match_route(&self, host_header: Option<&str>, path: &str) -> Option<MatchedRoute> {
        let normalized_host = host_header.and_then(normalize_host);

        self.routes
            .iter()
            .enumerate()
            .filter(|(_, route)| host_matches(route, normalized_host.as_deref()))
            .filter(|(_, route)| path_matches(&route.path_prefix, path))
            .max_by(|(left_index, left), (right_index, right)| {
                left.path_prefix
                    .len()
                    .cmp(&right.path_prefix.len())
                    .then_with(|| right_index.cmp(left_index))
            })
            .map(|(_, route)| MatchedRoute {
                route: route.clone(),
            })
    }

    pub fn route_ids(&self) -> HashSet<&str> {
        self.routes.iter().map(|route| route.id.as_str()).collect()
    }
}

pub fn request_id_from_headers(headers: &HeaderMap) -> String {
    let header_name = HeaderName::from_static(REQUEST_ID_HEADER);
    if let Some(value) = headers.get(header_name)
        && let Ok(value) = value.to_str()
        && Uuid::parse_str(value).is_ok()
    {
        return value.to_owned();
    }

    Uuid::now_v7().to_string()
}

pub fn request_id_header_value(request_id: &str) -> HeaderValue {
    HeaderValue::from_str(request_id)
        .unwrap_or_else(|_| HeaderValue::from_static("invalid-request-id"))
}

pub fn normalize_host(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }

    let without_port = if host.starts_with('[') {
        host.split(']').next().map(|value| format!("{value}]"))?
    } else {
        host.split(':').next()?.to_owned()
    };

    let normalized = without_port.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn host_matches(route: &Route, host: Option<&str>) -> bool {
    route.hosts.iter().any(|candidate| {
        candidate == "*"
            || host
                .map(|host| candidate.eq_ignore_ascii_case(host))
                .unwrap_or(false)
    })
}

fn path_matches(prefix: &str, path: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .map(|suffix| suffix.starts_with('/'))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;

    fn route(id: &str, hosts: Vec<&str>, path_prefix: &str) -> Route {
        Route::new(
            id,
            hosts.into_iter().map(str::to_owned).collect(),
            path_prefix,
            vec![Upstream::parse("http://127.0.0.1:3000").unwrap()],
            Duration::from_secs(1),
            0,
        )
    }

    #[test]
    fn matches_exact_host_before_wildcard_by_longest_prefix() {
        let router = Router::new(vec![
            route("wild", vec!["*"], "/api"),
            route("users", vec!["example.com"], "/api/users"),
        ]);

        let matched = router
            .match_route(Some("example.com:8080"), "/api/users/123")
            .unwrap();

        assert_eq!(matched.route.id, "users");
    }

    #[test]
    fn does_not_match_partial_path_segment() {
        let router = Router::new(vec![route("users", vec!["*"], "/api/users")]);

        assert!(
            router
                .match_route(Some("localhost"), "/api/users2")
                .is_none()
        );
    }

    #[test]
    fn preserves_valid_uuid_request_id() {
        let request_id = Uuid::now_v7().to_string();
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            HeaderValue::from_str(&request_id).unwrap(),
        );

        assert_eq!(request_id_from_headers(&headers), request_id);
    }

    #[test]
    fn generates_uuid_when_request_id_is_missing_or_invalid() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static(REQUEST_ID_HEADER),
            HeaderValue::from_static("not-a-uuid"),
        );

        let generated = request_id_from_headers(&headers);
        assert!(Uuid::parse_str(&generated).is_ok());
    }
}
