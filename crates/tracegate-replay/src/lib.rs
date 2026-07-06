use std::time::Instant;

use http::header::{
    ACCEPT, ACCEPT_LANGUAGE, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName,
    PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE, USER_AGENT,
};
use reqwest::{Client, Method, Url};
use serde::Serialize;
use thiserror::Error;
use tracegate_core::REQUEST_ID_HEADER;
use tracegate_storage::{
    CaptureDetails, ListFilters, ReplayRunInsert, RequestDetails, Storage, StoredHeader, now_ms,
};
use uuid::Uuid;

pub const REPLAY_HEADER: &str = "x-tracegate-replay";
pub const ORIGINAL_REQUEST_ID_HEADER: &str = "x-tracegate-original-request-id";

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplaySelector {
    Id(String),
    LastFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayOptions {
    pub selector: ReplaySelector,
    pub target: String,
    pub confirm_side_effects: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct ReplayOutcome {
    pub replay_id: String,
    pub original_request_id: String,
    pub replay_request_id: String,
    pub target: String,
    pub method: String,
    pub path: String,
    pub status: u16,
    pub latency_ms: u128,
    pub response_body_bytes: usize,
    pub diff_summary: String,
}

#[derive(Debug, Error)]
pub enum ReplayError {
    #[error("request `{0}` not found")]
    RequestNotFound(String),
    #[error("no failed request found")]
    NoFailedRequest,
    #[error("method `{method}` requires --confirm-side-effects before replay")]
    MissingSideEffectConfirmation { method: String },
    #[error("cannot replay `{request_id}`: request body is missing")]
    MissingBody { request_id: String },
    #[error("cannot replay `{request_id}`: request body was evicted")]
    EvictedBody { request_id: String },
    #[error("cannot replay `{request_id}`: request body was truncated")]
    TruncatedBody { request_id: String },
    #[error("invalid replay target `{target}`: {reason}")]
    InvalidTarget { target: String, reason: String },
    #[error("replay target `{target}` matches original upstream for request `{request_id}`")]
    OriginalUpstreamTarget { request_id: String, target: String },
    #[error("invalid stored method `{method}` for request `{request_id}`: {reason}")]
    InvalidMethod {
        request_id: String,
        method: String,
        reason: String,
    },
    #[error("invalid stored header `{name}` for request `{request_id}`: {reason}")]
    InvalidHeader {
        request_id: String,
        name: String,
        reason: String,
    },
    #[error("storage error: {0}")]
    Storage(#[from] tracegate_storage::StorageError),
    #[error("replay dispatch `{replay_id}` failed for request `{request_id}`: {reason}")]
    DispatchFailed {
        replay_id: String,
        request_id: String,
        reason: String,
    },
}

pub async fn replay(
    storage: &Storage,
    options: ReplayOptions,
) -> Result<ReplayOutcome, ReplayError> {
    let request = select_request(storage, &options.selector).await?;
    let target = validate_target(&options.target)?;
    reject_original_upstream(&request, &target)?;

    let replay_request = build_replay_request(&request, &target, options.confirm_side_effects)?;
    dispatch_replay(storage, request, replay_request).await
}

async fn select_request(
    storage: &Storage,
    selector: &ReplaySelector,
) -> Result<RequestDetails, ReplayError> {
    match selector {
        ReplaySelector::Id(request_id) => storage
            .show_request(request_id)
            .await?
            .ok_or_else(|| ReplayError::RequestNotFound(request_id.clone())),
        ReplaySelector::LastFailed => {
            let rows = storage
                .list_requests(ListFilters {
                    failed: true,
                    limit: 1,
                    ..ListFilters::default()
                })
                .await?;
            let request_id = rows
                .first()
                .map(|request| request.request_id.clone())
                .ok_or(ReplayError::NoFailedRequest)?;

            storage
                .show_request(&request_id)
                .await?
                .ok_or(ReplayError::RequestNotFound(request_id))
        }
    }
}

#[derive(Clone, Debug)]
struct ValidatedTarget {
    origin: String,
}

fn validate_target(target: &str) -> Result<ValidatedTarget, ReplayError> {
    let parsed = Url::parse(target).map_err(|err| ReplayError::InvalidTarget {
        target: target.to_owned(),
        reason: err.to_string(),
    })?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ReplayError::InvalidTarget {
            target: target.to_owned(),
            reason: "target must use http or https".to_owned(),
        });
    }

    if parsed.host_str().is_none() {
        return Err(ReplayError::InvalidTarget {
            target: target.to_owned(),
            reason: "target must include a host".to_owned(),
        });
    }

    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ReplayError::InvalidTarget {
            target: target.to_owned(),
            reason: "target must be an origin without path, query, or fragment".to_owned(),
        });
    }

    Ok(ValidatedTarget {
        origin: parsed.origin().ascii_serialization(),
    })
}

fn reject_original_upstream(
    request: &RequestDetails,
    target: &ValidatedTarget,
) -> Result<(), ReplayError> {
    let Some(upstream) = request.request.upstream.as_deref() else {
        return Ok(());
    };

    let Ok(parsed) = Url::parse(upstream) else {
        return Ok(());
    };
    if parsed.origin().ascii_serialization() == target.origin {
        return Err(ReplayError::OriginalUpstreamTarget {
            request_id: request.request.request_id.clone(),
            target: target.origin.clone(),
        });
    }

    Ok(())
}

#[derive(Clone, Debug)]
struct ReplayRequest {
    replay_id: String,
    replay_request_id: String,
    target: String,
    method: Method,
    path: String,
    body: Option<Vec<u8>>,
    headers: Vec<(HeaderName, String)>,
}

fn build_replay_request(
    request: &RequestDetails,
    target: &ValidatedTarget,
    confirm_side_effects: bool,
) -> Result<ReplayRequest, ReplayError> {
    let method = Method::from_bytes(request.request.method.as_bytes()).map_err(|err| {
        ReplayError::InvalidMethod {
            request_id: request.request.request_id.clone(),
            method: request.request.method.clone(),
            reason: err.to_string(),
        }
    })?;
    if is_side_effect_method(&method) && !confirm_side_effects {
        return Err(ReplayError::MissingSideEffectConfirmation {
            method: request.request.method.clone(),
        });
    }

    let body = replay_body(request, &method)?;
    let headers = replay_headers(request)?;

    Ok(ReplayRequest {
        replay_id: Uuid::now_v7().to_string(),
        replay_request_id: Uuid::now_v7().to_string(),
        target: target.origin.clone(),
        method,
        path: stored_path_and_query(request),
        body,
        headers,
    })
}

fn replay_body(request: &RequestDetails, method: &Method) -> Result<Option<Vec<u8>>, ReplayError> {
    let request_id = request.request.request_id.clone();
    let capture = request.capture.as_ref();

    if method_requires_captured_body(method) {
        let capture = capture.ok_or_else(|| ReplayError::MissingBody {
            request_id: request_id.clone(),
        })?;
        validate_capture_body(request_id.clone(), capture)?;
        return capture
            .request_body
            .clone()
            .ok_or(ReplayError::MissingBody { request_id })
            .map(Some);
    }

    if let Some(capture) = capture {
        if capture.request_body_truncated && capture.request_body.is_some() {
            return Err(ReplayError::TruncatedBody { request_id });
        }
        if capture.body_evicted && capture.request_body.is_some() {
            return Err(ReplayError::EvictedBody { request_id });
        }
        return Ok(capture.request_body.clone());
    }

    Ok(None)
}

fn validate_capture_body(request_id: String, capture: &CaptureDetails) -> Result<(), ReplayError> {
    if capture.body_evicted {
        return Err(ReplayError::EvictedBody { request_id });
    }
    if capture.request_body_truncated {
        return Err(ReplayError::TruncatedBody { request_id });
    }
    Ok(())
}

fn replay_headers(request: &RequestDetails) -> Result<Vec<(HeaderName, String)>, ReplayError> {
    request
        .request_headers
        .iter()
        .filter(|header| is_replay_header_allowed(&header.name))
        .map(|header| replay_header(request, header))
        .collect()
}

fn replay_header(
    request: &RequestDetails,
    header: &StoredHeader,
) -> Result<(HeaderName, String), ReplayError> {
    let name = HeaderName::from_bytes(header.name.as_bytes()).map_err(|err| {
        ReplayError::InvalidHeader {
            request_id: request.request.request_id.clone(),
            name: header.name.clone(),
            reason: err.to_string(),
        }
    })?;

    Ok((name, header.value.clone()))
}

fn is_replay_header_allowed(name: &str) -> bool {
    let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
        return false;
    };

    matches!(name, CONTENT_TYPE | ACCEPT | ACCEPT_LANGUAGE | USER_AGENT)
        && !is_hop_by_hop_header(&name)
}

fn is_hop_by_hop_header(name: &HeaderName) -> bool {
    matches!(
        *name,
        CONNECTION
            | CONTENT_LENGTH
            | HOST
            | PROXY_AUTHENTICATE
            | PROXY_AUTHORIZATION
            | TE
            | TRAILER
            | TRANSFER_ENCODING
            | UPGRADE
    )
}

async fn dispatch_replay(
    storage: &Storage,
    request: RequestDetails,
    replay_request: ReplayRequest,
) -> Result<ReplayOutcome, ReplayError> {
    let client = Client::new();
    let url = format!("{}{}", replay_request.target, replay_request.path);
    let started = Instant::now();
    let mut builder = client.request(replay_request.method.clone(), &url);

    for (name, value) in &replay_request.headers {
        builder = builder.header(name, value);
    }
    builder = builder
        .header(REPLAY_HEADER, "true")
        .header(
            ORIGINAL_REQUEST_ID_HEADER,
            request.request.request_id.as_str(),
        )
        .header(REQUEST_ID_HEADER, replay_request.replay_request_id.as_str())
        .header("traceparent", fresh_traceparent());

    if let Some(body) = replay_request.body.clone() {
        builder = builder.body(body);
    }

    let result = builder.send().await;
    let latency_ms = started.elapsed().as_millis();

    match result {
        Ok(response) => {
            let status = response.status().as_u16();
            let response_body_bytes = match response.bytes().await {
                Ok(body) => body.len(),
                Err(err) => {
                    return persist_dispatch_failure(
                        storage,
                        request,
                        replay_request,
                        latency_ms,
                        err.to_string(),
                    )
                    .await;
                }
            };
            let diff_summary = format!(
                "original_status={} replay_status={} response_body_bytes={}",
                request.request.status, status, response_body_bytes
            );

            storage
                .insert_replay_run(ReplayRunInsert {
                    replay_id: replay_request.replay_id.clone(),
                    original_request_id: request.request.request_id.clone(),
                    replay_request_id: replay_request.replay_request_id.clone(),
                    target: replay_request.target.clone(),
                    method: replay_request.method.to_string(),
                    path: replay_request.path.clone(),
                    status: Some(status),
                    latency_ms,
                    error: None,
                    diff_summary: Some(diff_summary.clone()),
                    created_at_ms: now_ms(),
                })
                .await?;

            Ok(ReplayOutcome {
                replay_id: replay_request.replay_id,
                original_request_id: request.request.request_id,
                replay_request_id: replay_request.replay_request_id,
                target: replay_request.target,
                method: replay_request.method.to_string(),
                path: replay_request.path,
                status,
                latency_ms,
                response_body_bytes,
                diff_summary,
            })
        }
        Err(err) => {
            persist_dispatch_failure(
                storage,
                request,
                replay_request,
                latency_ms,
                err.to_string(),
            )
            .await
        }
    }
}

async fn persist_dispatch_failure(
    storage: &Storage,
    request: RequestDetails,
    replay_request: ReplayRequest,
    latency_ms: u128,
    reason: String,
) -> Result<ReplayOutcome, ReplayError> {
    storage
        .insert_replay_run(ReplayRunInsert {
            replay_id: replay_request.replay_id.clone(),
            original_request_id: request.request.request_id.clone(),
            replay_request_id: replay_request.replay_request_id,
            target: replay_request.target,
            method: replay_request.method.to_string(),
            path: replay_request.path,
            status: None,
            latency_ms,
            error: Some(reason.clone()),
            diff_summary: Some("dispatch_failed".to_owned()),
            created_at_ms: now_ms(),
        })
        .await?;

    Err(ReplayError::DispatchFailed {
        replay_id: replay_request.replay_id,
        request_id: request.request.request_id,
        reason,
    })
}

fn is_side_effect_method(method: &Method) -> bool {
    matches!(
        *method,
        Method::POST | Method::PUT | Method::PATCH | Method::DELETE
    )
}

fn method_requires_captured_body(method: &Method) -> bool {
    matches!(*method, Method::POST | Method::PUT | Method::PATCH)
}

fn stored_path_and_query(request: &RequestDetails) -> String {
    match request.request.redacted_query.as_deref() {
        Some(query) => format!("{}?{query}", request.request.path),
        None => request.request.path.clone(),
    }
}

fn fresh_traceparent() -> String {
    let trace_id = Uuid::now_v7().simple().to_string();
    let span_source = Uuid::now_v7().simple().to_string();
    let span_id = &span_source[..16];
    format!("00-{trace_id}-{span_id}-01")
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::Path};

    use axum::{
        Json, Router,
        body::Bytes,
        extract::OriginalUri,
        http::{HeaderMap, Method as AxumMethod},
        routing::any,
    };
    use serde::Serialize;
    use tokio::{net::TcpListener, sync::oneshot};
    use tracegate_core::StorageConfig;
    use tracegate_storage::{CaptureInsert, RequestInsert};

    use super::*;

    #[derive(Serialize)]
    struct EchoResponse {
        method: String,
        path: String,
        body_len: usize,
        replay: String,
        original_request_id: String,
        replay_request_id: String,
        authorization_seen: bool,
    }

    async fn start_echo_target() -> (SocketAddr, oneshot::Sender<()>) {
        let app = Router::new().route(
            "/{*path}",
            any(
                |method: AxumMethod,
                 OriginalUri(uri): OriginalUri,
                 headers: HeaderMap,
                 body: Bytes| async move {
                    Json(EchoResponse {
                        method: method.to_string(),
                        path: uri.path().to_owned(),
                        body_len: body.len(),
                        replay: header(&headers, REPLAY_HEADER),
                        original_request_id: header(&headers, ORIGINAL_REQUEST_ID_HEADER),
                        replay_request_id: header(&headers, REQUEST_ID_HEADER),
                        authorization_seen: headers.contains_key("authorization"),
                    })
                },
            ),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        (addr, shutdown_tx)
    }

    fn header(headers: &HeaderMap, name: &str) -> String {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_owned()
    }

    async fn storage() -> (Storage, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            url: sqlite_url(&dir.path().join("tracegate.db")),
            ..StorageConfig::default()
        };
        let storage = Storage::connect(&config).await.unwrap();
        storage.migrate().await.unwrap();
        (storage, dir)
    }

    fn sqlite_url(path: &Path) -> String {
        let path = path.display().to_string().replace('\\', "/");
        if path.starts_with('/') {
            format!("sqlite://{path}")
        } else {
            format!("sqlite:///{path}")
        }
    }

    fn request(method: &str, request_id: &str) -> RequestInsert {
        RequestInsert {
            request_id: request_id.to_owned(),
            trace_id: Some("trace".to_owned()),
            route_id: Some("payments".to_owned()),
            method: method.to_owned(),
            path: "/api/payments/fail".to_owned(),
            redacted_query: Some("visible=yes".to_owned()),
            query_hash: Some("hash".to_owned()),
            status: 500,
            latency_ms: 42,
            upstream: Some("http://payments-service:4000".to_owned()),
            is_error: true,
            is_slow: false,
            capture_policy: "errors_and_slow".to_owned(),
            capture_dropped: false,
            created_at_ms: now_ms(),
        }
    }

    fn request_headers() -> Vec<StoredHeader> {
        vec![
            StoredHeader {
                name: "authorization".to_owned(),
                value: "Bearer secret".to_owned(),
            },
            StoredHeader {
                name: "content-length".to_owned(),
                value: "13".to_owned(),
            },
            StoredHeader {
                name: "content-type".to_owned(),
                value: "application/json".to_owned(),
            },
            StoredHeader {
                name: "host".to_owned(),
                value: "original.example".to_owned(),
            },
        ]
    }

    fn capture(body: Option<Vec<u8>>, truncated: bool, evicted: bool) -> CaptureInsert {
        CaptureInsert {
            request_content_type: Some("application/json".to_owned()),
            response_content_type: Some("application/json".to_owned()),
            request_body: if evicted { None } else { body },
            response_body: Some(br#"{"ok":false}"#.to_vec()),
            request_body_truncated: truncated,
            response_body_truncated: false,
            request_body_sha256: Some("request-hash".to_owned()),
            response_body_sha256: Some("response-hash".to_owned()),
        }
    }

    #[tokio::test]
    async fn replays_get_request_and_records_audit() {
        let (storage, _dir) = storage().await;
        let (target, shutdown) = start_echo_target().await;
        storage
            .insert_request(
                request("GET", "req-get"),
                request_headers(),
                vec![],
                None,
                vec![],
            )
            .await
            .unwrap();

        let outcome = replay(
            &storage,
            ReplayOptions {
                selector: ReplaySelector::Id("req-get".to_owned()),
                target: format!("http://{target}"),
                confirm_side_effects: false,
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.status, 200);
        assert_eq!(outcome.method, "GET");
        assert_eq!(outcome.path, "/api/payments/fail?visible=yes");
        let details = storage.show_request("req-get").await.unwrap().unwrap();
        assert_eq!(details.replay_runs.len(), 1);
        assert_eq!(details.replay_runs[0].status, Some(200));

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn post_requires_side_effect_confirmation() {
        let (storage, _dir) = storage().await;
        storage
            .insert_request(
                request("POST", "req-post"),
                request_headers(),
                vec![],
                Some(capture(Some(br#"{"ok":true}"#.to_vec()), false, false)),
                vec![],
            )
            .await
            .unwrap();

        let err = replay(
            &storage,
            ReplayOptions {
                selector: ReplaySelector::Id("req-post".to_owned()),
                target: "http://127.0.0.1:4000".to_owned(),
                confirm_side_effects: false,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(
            err,
            ReplayError::MissingSideEffectConfirmation { .. }
        ));
    }

    #[tokio::test]
    async fn replays_post_body_with_confirmation() {
        let (storage, _dir) = storage().await;
        let (target, shutdown) = start_echo_target().await;
        storage
            .insert_request(
                request("POST", "req-post"),
                request_headers(),
                vec![],
                Some(capture(Some(br#"{"ok":true}"#.to_vec()), false, false)),
                vec![],
            )
            .await
            .unwrap();

        let outcome = replay(
            &storage,
            ReplayOptions {
                selector: ReplaySelector::Id("req-post".to_owned()),
                target: format!("http://{target}"),
                confirm_side_effects: true,
            },
        )
        .await
        .unwrap();

        assert_eq!(outcome.status, 200);
        assert!(outcome.diff_summary.contains("original_status=500"));
        let details = storage.show_request("req-post").await.unwrap().unwrap();
        assert_eq!(details.replay_runs[0].method, "POST");

        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn rejects_missing_evicted_or_truncated_required_body() {
        for (request_id, capture, expected) in [
            ("missing", None, "missing"),
            (
                "evicted",
                Some(capture(Some(br#"{"ok":true}"#.to_vec()), false, true)),
                "evicted",
            ),
            (
                "truncated",
                Some(capture(Some(br#"{"ok":true}"#.to_vec()), true, false)),
                "truncated",
            ),
        ] {
            let (storage, _dir) = storage().await;
            storage
                .insert_request(
                    request("POST", request_id),
                    request_headers(),
                    vec![],
                    capture,
                    vec![],
                )
                .await
                .unwrap();

            let err = replay(
                &storage,
                ReplayOptions {
                    selector: ReplaySelector::Id(request_id.to_owned()),
                    target: "http://127.0.0.1:4000".to_owned(),
                    confirm_side_effects: true,
                },
            )
            .await
            .unwrap_err()
            .to_string();

            assert!(err.contains(expected), "{err}");
        }
    }

    #[tokio::test]
    async fn validates_replay_targets() {
        for target in [
            "ftp://127.0.0.1:4000",
            "http://127.0.0.1:4000/path",
            "http://127.0.0.1:4000?x=1",
            "http://",
        ] {
            assert!(validate_target(target).is_err(), "{target}");
        }
        assert_eq!(
            validate_target("http://127.0.0.1:4000").unwrap().origin,
            "http://127.0.0.1:4000"
        );
    }

    #[tokio::test]
    async fn rejects_original_upstream_target() {
        let (storage, _dir) = storage().await;
        storage
            .insert_request(
                request("GET", "req-get"),
                request_headers(),
                vec![],
                None,
                vec![],
            )
            .await
            .unwrap();

        let err = replay(
            &storage,
            ReplayOptions {
                selector: ReplaySelector::Id("req-get".to_owned()),
                target: "http://payments-service:4000".to_owned(),
                confirm_side_effects: false,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ReplayError::OriginalUpstreamTarget { .. }));
    }

    #[tokio::test]
    async fn records_dispatch_failures() {
        let (storage, _dir) = storage().await;
        storage
            .insert_request(
                request("GET", "req-get"),
                request_headers(),
                vec![],
                None,
                vec![],
            )
            .await
            .unwrap();

        let err = replay(
            &storage,
            ReplayOptions {
                selector: ReplaySelector::Id("req-get".to_owned()),
                target: "http://127.0.0.1:9".to_owned(),
                confirm_side_effects: false,
            },
        )
        .await
        .unwrap_err();

        assert!(matches!(err, ReplayError::DispatchFailed { .. }));
        let details = storage.show_request("req-get").await.unwrap().unwrap();
        assert_eq!(details.replay_runs.len(), 1);
        assert!(details.replay_runs[0].error.is_some());
        assert_eq!(
            details.replay_runs[0].diff_summary.as_deref(),
            Some("dispatch_failed")
        );
    }
}
