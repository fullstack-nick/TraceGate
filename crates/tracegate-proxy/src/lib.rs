use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use http::{
    HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header::{
        CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName, PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
    },
};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{body::Incoming, service::service_fn};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ServerBuilder,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{net::TcpListener, time::timeout};
use tracegate_core::{
    AppConfig, CapturePolicy, FORWARDED_FOR_HEADER, FORWARDED_HOST_HEADER, FORWARDED_PROTO_HEADER,
    RedactionConfig, Route, Router, StorageConfig, Upstream, request_id_from_headers,
    request_id_header_value,
};
use tracegate_observability::{RequestMetric, Telemetry};
use tracegate_storage::{CaptureInsert, RequestInsert, Storage, StoredHeader, now_ms};
use tracing::{Instrument, field};

type ProxyBody = BoxBody<Bytes, hyper::Error>;
type ProxyClient = Client<HttpConnector, ProxyBody>;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("failed to bind listener: {0}")]
    Bind(#[from] std::io::Error),
    #[error("capture store error: {0}")]
    Storage(#[from] tracegate_storage::StorageError),
}

#[derive(Clone)]
struct Proxy {
    router: Arc<Router>,
    client: ProxyClient,
    telemetry: Telemetry,
    storage: Arc<Storage>,
    storage_config: StorageConfig,
    redaction: RedactionConfig,
}

#[derive(Clone)]
struct RequestTemplate {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
}

#[derive(Debug)]
enum AttemptError {
    Timeout,
    Transport(String),
    BuildRequest(String),
}

#[derive(Debug, Serialize)]
pub struct RequestLogRecord {
    pub request_id: String,
    pub method: String,
    pub path: String,
    pub route_id: Option<String>,
    pub upstream: Option<String>,
    pub status: u16,
    pub latency_ms: u128,
    pub error: Option<String>,
}

impl RequestLogRecord {
    fn emit(&self) {
        tracing::info!(
            request_id = %self.request_id,
            method = %self.method,
            path = %self.path,
            route_id = self.route_id.as_deref().unwrap_or(""),
            upstream = self.upstream.as_deref().unwrap_or(""),
            status = self.status,
            latency_ms = self.latency_ms,
            error = self.error.as_deref().unwrap_or(""),
            "request complete"
        );
    }
}

pub async fn serve(config: AppConfig, telemetry: Telemetry) -> Result<(), ProxyError> {
    let listener = TcpListener::bind(config.listen).await?;
    let admin_listener = TcpListener::bind(config.admin_listen).await?;
    serve_listeners(
        listener,
        admin_listener,
        config,
        telemetry,
        std::future::pending::<()>(),
    )
    .await
}

pub async fn serve_listener<S>(
    listener: TcpListener,
    config: AppConfig,
    telemetry: Telemetry,
    shutdown: S,
) -> Result<(), ProxyError>
where
    S: Future<Output = ()> + Send,
{
    let admin_listener = TcpListener::bind(config.admin_listen).await?;
    serve_listeners(listener, admin_listener, config, telemetry, shutdown).await
}

pub async fn serve_listeners<S>(
    listener: TcpListener,
    admin_listener: TcpListener,
    config: AppConfig,
    telemetry: Telemetry,
    shutdown: S,
) -> Result<(), ProxyError>
where
    S: Future<Output = ()> + Send,
{
    let storage = initialize_storage(&config, &telemetry).await?;
    let proxy = Proxy::new(config, telemetry.clone(), storage.clone());
    spawn_retention_loop(storage, telemetry.clone());
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                break;
            }
            accepted = listener.accept() => {
                let (stream, remote_addr) = accepted?;
                let proxy = proxy.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let proxy = proxy.clone();
                        async move { proxy.handle(request, remote_addr).await }
                    });

                    let io = TokioIo::new(stream);
                    if let Err(err) = ServerBuilder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(io, service)
                        .await
                    {
                        tracing::warn!(error = %err, "connection failed");
                    }
                });
            }
            accepted = admin_listener.accept() => {
                let (stream, _) = accepted?;
                let telemetry = telemetry.clone();
                let storage = proxy.storage.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let telemetry = telemetry.clone();
                        let storage = storage.clone();
                        async move { handle_admin_request(request, telemetry, storage).await }
                    });

                    let io = TokioIo::new(stream);
                    if let Err(err) = ServerBuilder::new(TokioExecutor::new())
                        .serve_connection_with_upgrades(io, service)
                        .await
                    {
                        tracing::warn!(error = %err, "admin connection failed");
                    }
                });
            }
        }
    }

    Ok(())
}

impl Proxy {
    fn new(config: AppConfig, telemetry: Telemetry, storage: Arc<Storage>) -> Self {
        let mut connector = HttpConnector::new();
        connector.enforce_http(true);
        let client = Client::builder(TokioExecutor::new()).build(connector);
        let storage_config = config.storage;
        let redaction = config.redaction;

        Self {
            router: Arc::new(Router::new(config.routes)),
            client,
            telemetry,
            storage,
            storage_config,
            redaction,
        }
    }

    async fn handle(
        &self,
        request: Request<Incoming>,
        remote_addr: SocketAddr,
    ) -> Result<Response<ProxyBody>, Infallible> {
        let request_id = request_id_from_headers(request.headers());
        let method = request.method().clone();
        let trace_id = request
            .headers()
            .get("traceparent")
            .and_then(|value| value.to_str().ok())
            .and_then(tracegate_observability::trace_id_hex_from_traceparent)
            .map(str::to_owned);
        let path = redacted_path_and_query(request.uri(), &self.redaction);
        let parent = tracegate_observability::extract_context(request.headers());
        let span = tracing::info_span!(
            "tracegate.request",
            otel.kind = "server",
            request_id = %request_id,
            method = %method,
            path = %path,
            route_id = field::Empty,
            upstream = field::Empty,
            status = field::Empty,
            latency_ms = field::Empty,
            error = field::Empty,
        );
        tracegate_observability::set_span_parent(&span, parent);

        self.handle_instrumented(request, remote_addr, request_id, trace_id, method, path)
            .instrument(span)
            .await
    }

    async fn handle_instrumented(
        &self,
        request: Request<Incoming>,
        remote_addr: SocketAddr,
        request_id: String,
        trace_id: Option<String>,
        method: Method,
        redacted_path: String,
    ) -> Result<Response<ProxyBody>, Infallible> {
        let started = Instant::now();
        let request_headers_for_storage = stored_headers(request.headers(), &self.redaction);
        let request_path = request.uri().path().to_owned();
        let redacted_query = redacted_query(request.uri(), &self.redaction);
        let query_hash = request.uri().query().map(sha256_hex);
        let host = request
            .headers()
            .get(HOST)
            .and_then(|value| value.to_str().ok());

        let Some(matched) = self.router.match_route(host, request.uri().path()) else {
            let response = response_with_request_id(
                StatusCode::NOT_FOUND,
                "no route matched request",
                &request_id,
            );
            let status = response.status();
            self.log_request(RequestLogRecord {
                request_id: request_id.clone(),
                method: method.to_string(),
                path: redacted_path.clone(),
                route_id: None,
                upstream: None,
                status: status.as_u16(),
                latency_ms: started.elapsed().as_millis(),
                error: Some("no_route".to_owned()),
            });
            record_span_fields(None, None, status, started, Some("no_route"));
            self.telemetry.record_request(RequestMetric {
                route_id: None,
                method: method.to_string(),
                status: status.as_u16(),
                latency_seconds: started.elapsed().as_secs_f64(),
                upstream_error: false,
            });
            let record = RequestInsert {
                request_id,
                trace_id,
                route_id: None,
                method: method.to_string(),
                path: request_path,
                redacted_query,
                query_hash,
                status: status.as_u16(),
                latency_ms: started.elapsed().as_millis(),
                upstream: None,
                is_error: false,
                is_slow: false,
                capture_policy: CapturePolicy::Off.to_string(),
                capture_dropped: false,
                created_at_ms: now_ms(),
            };

            return Ok(self.attach_storage_finalizer(
                response,
                StorageFinalizerInput {
                    record,
                    request_headers: request_headers_for_storage,
                    request_capture: Arc::new(Mutex::new(CaptureBuffer::disabled())),
                    should_capture: false,
                    request_content_type: None,
                    response_capture_enabled: false,
                    response_capture_limit: 0,
                },
            ));
        };

        let upstream = matched.route.select_upstream();
        let upstream_origin = upstream.origin();
        let retry_eligible = retry_eligible(&method, request.headers());
        let request_content_type = content_type(request.headers());
        let request_capture_enabled = matched.route.capture.policy != CapturePolicy::Off
            && matched.route.capture.capture_request_body
            && request_content_type
                .as_deref()
                .map(is_capturable_content_type)
                .unwrap_or(false);
        let request_capture = Arc::new(Mutex::new(CaptureBuffer::new(
            request_capture_enabled,
            self.storage_config.max_capture_bytes_per_request,
        )));
        let template = RequestTemplate {
            method,
            uri: request.uri().clone(),
            version: request.version(),
            headers: request.headers().clone(),
        };

        let result = if retry_eligible {
            drop(request.into_body());
            self.request_with_retries(
                &matched.route,
                &upstream,
                &template,
                &request_id,
                remote_addr,
            )
            .await
        } else {
            self.request_once(
                &matched.route,
                &upstream,
                &template,
                CapturingBody::new(request.into_body().boxed(), request_capture.clone(), None)
                    .boxed(),
                &request_id,
                remote_addr,
            )
            .await
        };

        let (response, error) = match result {
            Ok(response) => (response, None),
            Err(AttemptError::Timeout) => (
                response_with_request_id(
                    StatusCode::GATEWAY_TIMEOUT,
                    "upstream request timed out",
                    &request_id,
                ),
                Some("timeout".to_owned()),
            ),
            Err(AttemptError::Transport(err)) => (
                response_with_request_id(
                    StatusCode::BAD_GATEWAY,
                    "upstream request failed",
                    &request_id,
                ),
                Some(err),
            ),
            Err(AttemptError::BuildRequest(err)) => (
                response_with_request_id(
                    StatusCode::BAD_GATEWAY,
                    "failed to build upstream request",
                    &request_id,
                ),
                Some(err),
            ),
        };
        let status = response.status();
        let upstream_error = error.is_some() || status.is_server_error();
        let is_slow = started.elapsed() >= matched.route.capture.slow_threshold;
        let should_capture = matched
            .route
            .capture
            .policy
            .should_capture(upstream_error, is_slow);
        let route_id = matched.route.id.clone();

        self.log_request(RequestLogRecord {
            request_id: request_id.clone(),
            method: template.method.to_string(),
            path: redacted_path,
            route_id: Some(route_id.clone()),
            upstream: Some(upstream_origin.clone()),
            status: status.as_u16(),
            latency_ms: started.elapsed().as_millis(),
            error: error.clone(),
        });
        record_span_fields(
            Some(&route_id),
            Some(&upstream_origin),
            status,
            started,
            error.as_deref(),
        );
        self.telemetry.record_request(RequestMetric {
            route_id: Some(route_id.clone()),
            method: template.method.to_string(),
            status: status.as_u16(),
            latency_seconds: started.elapsed().as_secs_f64(),
            upstream_error,
        });

        let response_capture_enabled = should_capture
            && matched.route.capture.capture_response_body_bytes > 0
            && content_type(response.headers())
                .as_deref()
                .map(is_capturable_content_type)
                .unwrap_or(false);
        let response_capture_limit = if response_capture_enabled {
            let request_captured_len = request_capture
                .lock()
                .expect("request capture poisoned")
                .captured_len();
            let remaining = self
                .storage_config
                .max_capture_bytes_per_request
                .saturating_sub(request_captured_len as u64);
            remaining.min(matched.route.capture.capture_response_body_bytes)
        } else {
            0
        };

        let record = RequestInsert {
            request_id,
            trace_id,
            route_id: Some(route_id),
            method: template.method.to_string(),
            path: request_path,
            redacted_query,
            query_hash,
            status: status.as_u16(),
            latency_ms: started.elapsed().as_millis(),
            upstream: Some(upstream_origin),
            is_error: upstream_error,
            is_slow,
            capture_policy: matched.route.capture.policy.to_string(),
            capture_dropped: false,
            created_at_ms: now_ms(),
        };

        Ok(self.attach_storage_finalizer(
            response,
            StorageFinalizerInput {
                record,
                request_headers: request_headers_for_storage,
                request_capture,
                should_capture,
                request_content_type,
                response_capture_enabled,
                response_capture_limit,
            },
        ))
    }

    async fn request_with_retries(
        &self,
        route: &Route,
        upstream: &Upstream,
        template: &RequestTemplate,
        request_id: &str,
        remote_addr: SocketAddr,
    ) -> Result<Response<ProxyBody>, AttemptError> {
        let attempts = route.retries.saturating_add(1);
        let mut last_error = None;

        for _ in 0..attempts {
            let body = Empty::<Bytes>::new()
                .map_err(|never| match never {})
                .boxed();
            match self
                .request_once(route, upstream, template, body, request_id, remote_addr)
                .await
            {
                Ok(response) => return Ok(response),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error.unwrap_or_else(|| AttemptError::Transport("no attempts made".to_owned())))
    }

    async fn request_once(
        &self,
        route: &Route,
        upstream: &Upstream,
        template: &RequestTemplate,
        body: ProxyBody,
        request_id: &str,
        remote_addr: SocketAddr,
    ) -> Result<Response<ProxyBody>, AttemptError> {
        let request = build_upstream_request(template, body, upstream, request_id, remote_addr)?;
        let response = timeout(route.timeout, self.client.request(request))
            .await
            .map_err(|_| AttemptError::Timeout)?
            .map_err(|err| AttemptError::Transport(err.to_string()))?;

        let mut response = response.map(|body| body.boxed());
        response.headers_mut().insert(
            HeaderName::from_static(tracegate_core::REQUEST_ID_HEADER),
            request_id_header_value(request_id),
        );

        Ok(response)
    }

    fn log_request(&self, record: RequestLogRecord) {
        record.emit();
    }

    fn attach_storage_finalizer(
        &self,
        response: Response<ProxyBody>,
        input: StorageFinalizerInput,
    ) -> Response<ProxyBody> {
        let (parts, body) = response.into_parts();
        let response_headers = stored_headers(&parts.headers, &self.redaction);
        let response_content_type = content_type(&parts.headers);
        let response_capture = Arc::new(Mutex::new(CaptureBuffer::new(
            input.response_capture_enabled,
            input.response_capture_limit,
        )));
        let finalizer = CaptureFinalizer {
            storage: self.storage.clone(),
            telemetry: self.telemetry.clone(),
            record: Some(input.record),
            request_headers: Some(input.request_headers),
            response_headers: Some(response_headers),
            request_capture: input.request_capture,
            response_capture: response_capture.clone(),
            should_capture: input.should_capture,
            request_content_type: input.request_content_type,
            response_content_type,
            finalized: false,
        };
        let body = CapturingBody::new(body, response_capture, Some(finalizer)).boxed();

        Response::from_parts(parts, body)
    }
}

struct StorageFinalizerInput {
    record: RequestInsert,
    request_headers: Vec<StoredHeader>,
    request_capture: Arc<Mutex<CaptureBuffer>>,
    should_capture: bool,
    request_content_type: Option<String>,
    response_capture_enabled: bool,
    response_capture_limit: u64,
}

async fn initialize_storage(
    config: &AppConfig,
    telemetry: &Telemetry,
) -> Result<Arc<Storage>, ProxyError> {
    let storage = Arc::new(Storage::connect(&config.storage).await?);
    storage.migrate().await?;
    let outcome = storage.run_retention().await?;
    telemetry.record_retention_run();
    tracing::info!(
        deleted_requests = outcome.deleted_requests,
        evicted_captures = outcome.evicted_captures,
        "capture-store retention completed"
    );
    Ok(storage)
}

fn spawn_retention_loop(storage: Arc<Storage>, telemetry: Telemetry) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60 * 60));
        loop {
            interval.tick().await;
            match storage.run_retention().await {
                Ok(outcome) => {
                    telemetry.record_retention_run();
                    tracing::info!(
                        deleted_requests = outcome.deleted_requests,
                        evicted_captures = outcome.evicted_captures,
                        "capture-store retention completed"
                    );
                }
                Err(err) => {
                    tracing::warn!(error = %err, "capture-store retention failed");
                    telemetry.record_capture_dropped();
                }
            }
        }
    });
}

struct CapturingBody {
    inner: ProxyBody,
    capture: Arc<Mutex<CaptureBuffer>>,
    finalizer: Option<CaptureFinalizer>,
}

impl CapturingBody {
    fn new(
        inner: ProxyBody,
        capture: Arc<Mutex<CaptureBuffer>>,
        finalizer: Option<CaptureFinalizer>,
    ) -> Self {
        Self {
            inner,
            capture,
            finalizer,
        }
    }

    fn finalize(&mut self) {
        if let Some(finalizer) = self.finalizer.as_mut() {
            finalizer.finalize();
        }
    }
}

impl Body for CapturingBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.as_mut().get_mut();
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    this.capture
                        .lock()
                        .expect("capture buffer poisoned")
                        .record(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(err))) => {
                this.finalize();
                Poll::Ready(Some(Err(err)))
            }
            Poll::Ready(None) => {
                this.finalize();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for CapturingBody {
    fn drop(&mut self) {
        self.finalize();
    }
}

struct CaptureFinalizer {
    storage: Arc<Storage>,
    telemetry: Telemetry,
    record: Option<RequestInsert>,
    request_headers: Option<Vec<StoredHeader>>,
    response_headers: Option<Vec<StoredHeader>>,
    request_capture: Arc<Mutex<CaptureBuffer>>,
    response_capture: Arc<Mutex<CaptureBuffer>>,
    should_capture: bool,
    request_content_type: Option<String>,
    response_content_type: Option<String>,
    finalized: bool,
}

impl CaptureFinalizer {
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;

        let Some(record) = self.record.take() else {
            return;
        };
        let request_headers = self.request_headers.take().unwrap_or_default();
        let response_headers = self.response_headers.take().unwrap_or_default();
        let request_snapshot = self
            .request_capture
            .lock()
            .expect("request capture poisoned")
            .snapshot();
        let response_snapshot = self
            .response_capture
            .lock()
            .expect("response capture poisoned")
            .snapshot();
        let capture = self.should_capture.then(|| CaptureInsert {
            request_content_type: self.request_content_type.take(),
            response_content_type: self.response_content_type.take(),
            request_body: request_snapshot.body,
            response_body: response_snapshot.body,
            request_body_truncated: request_snapshot.truncated,
            response_body_truncated: response_snapshot.truncated,
            request_body_sha256: request_snapshot.sha256,
            response_body_sha256: response_snapshot.sha256,
        });
        let wrote_capture = capture.is_some();
        let storage = self.storage.clone();
        let telemetry = self.telemetry.clone();

        tokio::spawn(async move {
            match storage
                .insert_request(record, request_headers, response_headers, capture)
                .await
            {
                Ok(()) => {
                    if wrote_capture {
                        telemetry.record_capture();
                    }
                }
                Err(err) => {
                    tracing::warn!(error = %err, "failed to persist request capture");
                    telemetry.record_capture_dropped();
                }
            }
        });
    }
}

struct CaptureBuffer {
    enabled: bool,
    limit: usize,
    body: Vec<u8>,
    truncated: bool,
    saw_body: bool,
    hasher: Sha256,
}

struct CaptureSnapshot {
    body: Option<Vec<u8>>,
    truncated: bool,
    sha256: Option<String>,
}

impl CaptureBuffer {
    fn disabled() -> Self {
        Self::new(false, 0)
    }

    fn new(enabled: bool, limit: u64) -> Self {
        let limit = limit.min(usize::MAX as u64) as usize;
        Self {
            enabled,
            limit,
            body: Vec::with_capacity(limit.min(8192)),
            truncated: false,
            saw_body: false,
            hasher: Sha256::new(),
        }
    }

    fn record(&mut self, chunk: &Bytes) {
        if !self.enabled {
            return;
        }

        self.saw_body = true;
        self.hasher.update(chunk);

        let remaining = self.limit.saturating_sub(self.body.len());
        if remaining == 0 {
            if !chunk.is_empty() {
                self.truncated = true;
            }
            return;
        }

        let copy_len = remaining.min(chunk.len());
        self.body.extend_from_slice(&chunk[..copy_len]);
        if copy_len < chunk.len() {
            self.truncated = true;
        }
    }

    fn captured_len(&self) -> usize {
        self.body.len()
    }

    fn snapshot(&self) -> CaptureSnapshot {
        if !self.enabled || !self.saw_body {
            return CaptureSnapshot {
                body: None,
                truncated: false,
                sha256: None,
            };
        }

        CaptureSnapshot {
            body: Some(self.body.clone()),
            truncated: self.truncated,
            sha256: Some(format!("{:x}", self.hasher.clone().finalize())),
        }
    }
}

fn stored_headers(headers: &HeaderMap, redaction: &RedactionConfig) -> Vec<StoredHeader> {
    let mut stored = headers
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str().to_ascii_lowercase();
            if redaction.is_sensitive_header(&name) {
                return None;
            }

            let value = value
                .to_str()
                .map(truncate_header_value)
                .unwrap_or_else(|_| "<non-utf8>".to_owned());
            Some(StoredHeader { name, value })
        })
        .collect::<Vec<_>>();
    stored.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.value.cmp(&right.value))
    });
    stored
}

fn truncate_header_value(value: &str) -> String {
    const MAX_HEADER_VALUE_CHARS: usize = 4096;
    if value.chars().count() <= MAX_HEADER_VALUE_CHARS {
        value.to_owned()
    } else {
        format!(
            "{}...[truncated]",
            value
                .chars()
                .take(MAX_HEADER_VALUE_CHARS)
                .collect::<String>()
        )
    }
}

fn redacted_path_and_query(uri: &Uri, redaction: &RedactionConfig) -> String {
    match redacted_query(uri, redaction) {
        Some(query) => format!("{}?{query}", uri.path()),
        None => uri.path().to_owned(),
    }
}

fn redacted_query(uri: &Uri, redaction: &RedactionConfig) -> Option<String> {
    let query = uri.query()?;
    let kept = query
        .split('&')
        .filter(|part| {
            let key = part.split_once('=').map(|(key, _)| key).unwrap_or(part);
            !redaction.is_sensitive_query_param(key)
        })
        .collect::<Vec<_>>();

    if kept.is_empty() {
        None
    } else {
        Some(kept.join("&"))
    }
}

fn sha256_hex(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(255).collect())
}

fn is_capturable_content_type(value: &str) -> bool {
    let media_type = value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase();

    media_type.starts_with("text/")
        || matches!(
            media_type.as_str(),
            "application/json"
                | "application/xml"
                | "application/x-www-form-urlencoded"
                | "application/graphql"
                | "application/problem+json"
        )
        || media_type.ends_with("+json")
        || media_type.ends_with("+xml")
}

fn build_upstream_request(
    template: &RequestTemplate,
    body: ProxyBody,
    upstream: &Upstream,
    request_id: &str,
    remote_addr: SocketAddr,
) -> Result<Request<ProxyBody>, AttemptError> {
    let path_and_query = template
        .uri
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/");
    let uri: Uri = format!(
        "{}://{}{}",
        upstream.scheme, upstream.authority, path_and_query
    )
    .parse()
    .map_err(|err| AttemptError::BuildRequest(format!("{err}")))?;

    let mut request = Request::new(body);
    *request.method_mut() = template.method.clone();
    *request.uri_mut() = uri;
    *request.version_mut() = template.version;
    *request.headers_mut() = template.headers.clone();

    remove_hop_by_hop_headers(request.headers_mut());
    request
        .headers_mut()
        .insert(HOST, header_value(&upstream.authority)?);
    request.headers_mut().insert(
        HeaderName::from_static(tracegate_core::REQUEST_ID_HEADER),
        request_id_header_value(request_id),
    );

    if let Some(host) = template.headers.get(HOST).cloned() {
        request
            .headers_mut()
            .insert(HeaderName::from_static(FORWARDED_HOST_HEADER), host);
    }
    request.headers_mut().insert(
        HeaderName::from_static(FORWARDED_FOR_HEADER),
        header_value(&remote_addr.ip().to_string())?,
    );
    request.headers_mut().insert(
        HeaderName::from_static(FORWARDED_PROTO_HEADER),
        HeaderValue::from_static("http"),
    );
    tracegate_observability::inject_context(request.headers_mut());

    Ok(request)
}

async fn handle_admin_request(
    request: Request<Incoming>,
    telemetry: Telemetry,
    storage: Arc<Storage>,
) -> Result<Response<ProxyBody>, Infallible> {
    let response = match (request.method(), request.uri().path()) {
        (&Method::GET, "/health/live") => text_response(StatusCode::OK, "live\n"),
        (&Method::GET, "/health/ready") => match storage.health_check().await {
            Ok(()) => text_response(StatusCode::OK, "ready\n"),
            Err(err) => text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("storage not ready: {err}\n"),
            ),
        },
        (&Method::GET, "/metrics") if telemetry.prometheus_enabled() => {
            match telemetry.render_prometheus() {
                Ok(metrics) => response_with_content_type(
                    StatusCode::OK,
                    "application/openmetrics-text; version=1.0.0; charset=utf-8",
                    metrics,
                ),
                Err(err) => text_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("failed to render metrics: {err}\n"),
                ),
            }
        }
        (&Method::GET, "/metrics") => text_response(StatusCode::NOT_FOUND, "metrics disabled\n"),
        _ => text_response(StatusCode::NOT_FOUND, "not found\n"),
    };

    Ok(response)
}

fn record_span_fields(
    route_id: Option<&str>,
    upstream: Option<&str>,
    status: StatusCode,
    started: Instant,
    error: Option<&str>,
) {
    let span = tracing::Span::current();
    if let Some(route_id) = route_id {
        span.record("route_id", route_id);
    }
    if let Some(upstream) = upstream {
        span.record("upstream", upstream);
    }
    span.record("status", status.as_u16());
    span.record("latency_ms", started.elapsed().as_millis());
    if let Some(error) = error {
        span.record("error", error);
    }
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    for header in [
        CONNECTION,
        HeaderName::from_static("keep-alive"),
        PROXY_AUTHENTICATE,
        PROXY_AUTHORIZATION,
        TE,
        TRAILER,
        TRANSFER_ENCODING,
        UPGRADE,
    ] {
        headers.remove(header);
    }
}

fn response_with_request_id(
    status: StatusCode,
    body: &'static str,
    request_id: &str,
) -> Response<ProxyBody> {
    let mut response = Response::new(full_body(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        HeaderName::from_static(tracegate_core::REQUEST_ID_HEADER),
        request_id_header_value(request_id),
    );
    response
}

fn full_body(body: &'static str) -> ProxyBody {
    Full::new(Bytes::from_static(body.as_bytes()))
        .map_err(|never| match never {})
        .boxed()
}

fn text_response(status: StatusCode, body: &str) -> Response<ProxyBody> {
    response_with_content_type(status, "text/plain; charset=utf-8", body.to_owned())
}

fn response_with_content_type(
    status: StatusCode,
    content_type: &'static str,
    body: String,
) -> Response<ProxyBody> {
    let mut response = Response::new(
        Full::new(Bytes::from(body))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static(content_type),
    );
    response
}

fn header_value(value: &str) -> Result<HeaderValue, AttemptError> {
    HeaderValue::from_str(value).map_err(|err| AttemptError::BuildRequest(err.to_string()))
}

pub fn retry_eligible(method: &Method, headers: &HeaderMap) -> bool {
    matches!(method, &Method::GET | &Method::HEAD | &Method::OPTIONS)
        && headers
            .get(CONTENT_LENGTH)
            .map(|value| value == "0")
            .unwrap_or(true)
        && !headers.contains_key(TRANSFER_ENCODING)
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderMap;

    #[test]
    fn retries_only_empty_idempotent_methods() {
        assert!(retry_eligible(&Method::GET, &HeaderMap::new()));
        assert!(retry_eligible(&Method::HEAD, &HeaderMap::new()));
        assert!(!retry_eligible(&Method::POST, &HeaderMap::new()));

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("12"));
        assert!(!retry_eligible(&Method::GET, &headers));
    }

    #[test]
    fn request_log_record_serializes_required_fields() {
        let value = serde_json::to_value(RequestLogRecord {
            request_id: "req".to_owned(),
            method: "GET".to_owned(),
            path: "/api/users".to_owned(),
            route_id: Some("users".to_owned()),
            upstream: Some("http://users-service:3000".to_owned()),
            status: 200,
            latency_ms: 3,
            error: None,
        })
        .unwrap();

        for field in [
            "request_id",
            "method",
            "path",
            "route_id",
            "upstream",
            "status",
            "latency_ms",
            "error",
        ] {
            assert!(value.get(field).is_some(), "missing field {field}");
        }
    }
}
