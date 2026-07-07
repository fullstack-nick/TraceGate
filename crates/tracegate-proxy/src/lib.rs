use std::{
    collections::{HashMap, VecDeque},
    convert::Infallible,
    fs::File,
    future::Future,
    io::BufReader,
    net::SocketAddr,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicI64, AtomicU32, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use http::{
    HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
    header::{
        AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST, HeaderName,
        PROXY_AUTHENTICATE, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
    },
};
use http_body::{Body, Frame};
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{body::Incoming, service::service_fn};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
    server::conn::auto::Builder as ServerBuilder,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, RwLock, Semaphore, mpsc},
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tracegate_core::{
    AdminConfig, AppConfig, CapturePolicy, FORWARDED_FOR_HEADER, FORWARDED_HOST_HEADER,
    FORWARDED_PROTO_HEADER, RedactionConfig, Route, Router, StorageConfig, TlsConfig, Upstream,
    request_id_from_headers, request_id_header_value,
};
use tracegate_observability::{PluginDecisionMetric, RequestMetric, Telemetry};
use tracegate_storage::{
    CaptureInsert, PluginDecisionInsert, PluginEvent as StoredPluginEvent, RequestInsert, Storage,
    StoredHeader, now_ms,
};
use tracegate_wasm::{
    HeaderMutation, PolicyDecisionRecord, PolicyEngine, PolicyHeader, PolicyRequest,
    status_from_deny,
};
use tracing::{Instrument, field};

type ProxyBody = BoxBody<Bytes, hyper::Error>;
type ProxyConnector = HttpsConnector<HttpConnector>;
type ProxyClient = Client<ProxyConnector, ProxyBody>;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("failed to bind listener: {0}")]
    Bind(#[from] std::io::Error),
    #[error("capture store error: {0}")]
    Storage(#[from] tracegate_storage::StorageError),
    #[error("policy engine error: {0}")]
    Policy(#[from] tracegate_wasm::PolicyError),
    #[error("TLS error: {0}")]
    Tls(String),
}

#[derive(Clone)]
struct Proxy {
    state: Arc<ArcSwap<GatewayState>>,
    client: ProxyClient,
    telemetry: Telemetry,
    capture_writer: CaptureWriter,
}

struct RequestContext {
    remote_addr: SocketAddr,
    request_id: String,
    trace_id: Option<String>,
    method: Method,
    redacted_path: String,
    state: Arc<GatewayState>,
}

struct GatewayState {
    router: Router,
    storage_config: StorageConfig,
    redaction: RedactionConfig,
    policy: PolicyEngine,
    route_runtime: HashMap<String, Arc<RouteRuntime>>,
}

struct RouteRuntime {
    semaphore: Arc<Semaphore>,
    upstreams: Vec<UpstreamRuntime>,
    next_upstream: AtomicUsize,
    failure_threshold: u32,
    cooldown_ms: i64,
}

struct UpstreamRuntime {
    failures: AtomicU32,
    unhealthy_until_ms: AtomicI64,
}

impl GatewayState {
    fn new(config: &AppConfig) -> Result<Self, ProxyError> {
        let mut route_runtime = HashMap::new();
        for route in &config.routes {
            route_runtime.insert(route.id.clone(), Arc::new(RouteRuntime::new(route)));
        }
        Ok(Self {
            router: Router::new(config.routes.clone()),
            storage_config: config.storage.clone(),
            redaction: config.redaction.clone(),
            policy: PolicyEngine::new(&config.plugins)?,
            route_runtime,
        })
    }

    fn runtime_for(&self, route: &Route) -> Option<Arc<RouteRuntime>> {
        self.route_runtime.get(&route.id).cloned()
    }
}

impl RouteRuntime {
    fn new(route: &Route) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(route.concurrency_limit)),
            upstreams: route
                .upstreams
                .iter()
                .map(|_| UpstreamRuntime {
                    failures: AtomicU32::new(0),
                    unhealthy_until_ms: AtomicI64::new(0),
                })
                .collect(),
            next_upstream: AtomicUsize::new(0),
            failure_threshold: route.passive_health_failures,
            cooldown_ms: route
                .passive_health_cooldown
                .as_millis()
                .min(i64::MAX as u128) as i64,
        }
    }

    fn select_upstream(&self, route: &Route) -> Option<(usize, Upstream)> {
        let len = route.upstreams.len();
        if len == 0 {
            return None;
        }
        let now = now_ms();
        for _ in 0..len {
            let index = self.next_upstream.fetch_add(1, Ordering::Relaxed) % len;
            let runtime = &self.upstreams[index];
            if runtime.unhealthy_until_ms.load(Ordering::Relaxed) <= now {
                return Some((index, route.upstreams[index].clone()));
            }
        }
        None
    }

    fn record_upstream_result(&self, index: usize, failed: bool) {
        let Some(runtime) = self.upstreams.get(index) else {
            return;
        };
        if failed {
            let failures = runtime.failures.fetch_add(1, Ordering::Relaxed) + 1;
            if failures >= self.failure_threshold {
                runtime
                    .unhealthy_until_ms
                    .store(now_ms().saturating_add(self.cooldown_ms), Ordering::Relaxed);
                runtime.failures.store(0, Ordering::Relaxed);
            }
        } else {
            runtime.failures.store(0, Ordering::Relaxed);
            runtime.unhealthy_until_ms.store(0, Ordering::Relaxed);
        }
    }
}

#[derive(Clone)]
struct RequestTemplate {
    method: Method,
    uri: Uri,
    version: Version,
    headers: HeaderMap,
}

#[derive(Clone)]
struct CaptureWriter {
    sender: mpsc::Sender<CaptureWrite>,
}

struct CaptureWrite {
    record: RequestInsert,
    request_headers: Vec<StoredHeader>,
    response_headers: Vec<StoredHeader>,
    capture: Option<CaptureInsert>,
    plugin_decisions: Vec<PluginDecisionInsert>,
}

#[derive(Clone)]
struct AdminState {
    telemetry: Telemetry,
    storage: Arc<Storage>,
    gateway_state: Arc<ArcSwap<GatewayState>>,
    config_path: Option<PathBuf>,
    current_config: Arc<RwLock<AppConfig>>,
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
    serve_with_optional_config_path(config, telemetry, None).await
}

pub async fn serve_with_config_path(
    config_path: PathBuf,
    config: AppConfig,
    telemetry: Telemetry,
) -> Result<(), ProxyError> {
    serve_with_optional_config_path(config, telemetry, Some(config_path)).await
}

async fn serve_with_optional_config_path(
    config: AppConfig,
    telemetry: Telemetry,
    config_path: Option<PathBuf>,
) -> Result<(), ProxyError> {
    let listener = TcpListener::bind(config.listen).await?;
    let admin_listener = TcpListener::bind(config.admin_listen).await?;
    serve_listeners_with_config_path(
        listener,
        admin_listener,
        config,
        telemetry,
        config_path,
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
    serve_listeners_with_config_path(listener, admin_listener, config, telemetry, None, shutdown)
        .await
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
    serve_listeners_with_config_path(listener, admin_listener, config, telemetry, None, shutdown)
        .await
}

async fn serve_listeners_with_config_path<S>(
    listener: TcpListener,
    admin_listener: TcpListener,
    config: AppConfig,
    telemetry: Telemetry,
    config_path: Option<PathBuf>,
    shutdown: S,
) -> Result<(), ProxyError>
where
    S: Future<Output = ()> + Send,
{
    let storage = initialize_storage(&config, &telemetry).await?;
    let capture_writer = CaptureWriter::spawn(
        storage.clone(),
        telemetry.clone(),
        config.storage.capture_queue_capacity,
    );
    let proxy = Proxy::new(config.clone(), telemetry.clone(), capture_writer.clone())?;
    let admin_state = AdminState {
        telemetry: telemetry.clone(),
        storage: storage.clone(),
        gateway_state: proxy.state.clone(),
        config_path,
        current_config: Arc::new(RwLock::new(config.clone())),
    };
    let tls_acceptor = tls_acceptor(&config.server_tls)?;
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
                let tls_acceptor = tls_acceptor.clone();
                tokio::spawn(async move {
                    serve_proxy_stream(stream, remote_addr, proxy, tls_acceptor).await;
                });
            }
            accepted = admin_listener.accept() => {
                let (stream, _) = accepted?;
                let admin_state = admin_state.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request| {
                        let admin_state = admin_state.clone();
                        async move { handle_admin_request(request, admin_state).await }
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
    fn new(
        config: AppConfig,
        telemetry: Telemetry,
        capture_writer: CaptureWriter,
    ) -> Result<Self, ProxyError> {
        let client = build_upstream_client(&config)?;
        let state = Arc::new(ArcSwap::from_pointee(GatewayState::new(&config)?));

        Ok(Self {
            state,
            client,
            telemetry,
            capture_writer,
        })
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
        let state = self.state.load_full();
        let path = redacted_path_and_query(request.uri(), &state.redaction);
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

        self.handle_instrumented(
            request,
            RequestContext {
                remote_addr,
                request_id,
                trace_id,
                method,
                redacted_path: path,
                state,
            },
        )
        .instrument(span)
        .await
    }

    async fn handle_instrumented(
        &self,
        request: Request<Incoming>,
        context: RequestContext,
    ) -> Result<Response<ProxyBody>, Infallible> {
        let RequestContext {
            remote_addr,
            request_id,
            trace_id,
            method,
            redacted_path,
            state,
        } = context;
        let started = Instant::now();
        let request_headers_for_storage = stored_headers(request.headers(), &state.redaction);
        let request_path = request.uri().path().to_owned();
        let redacted_query = redacted_query(request.uri(), &state.redaction);
        let query_hash = request.uri().query().map(sha256_hex);
        let host = request
            .headers()
            .get(HOST)
            .and_then(|value| value.to_str().ok());

        let Some(matched) = state.router.match_route(host, request.uri().path()) else {
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
                request_id: request_id.clone(),
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
                    plugin_decisions: Vec::new(),
                    permit: None,
                },
            ));
        };

        let route_id = matched.route.id.clone();
        let Some(route_runtime) = state.runtime_for(&matched.route) else {
            let response = response_with_request_id(
                StatusCode::SERVICE_UNAVAILABLE,
                "route runtime unavailable",
                &request_id,
            );
            return Ok(response);
        };
        let mut permit = match route_runtime.semaphore.clone().try_acquire_owned() {
            Ok(permit) => Some(permit),
            Err(_) => {
                let response = response_with_request_id(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "route concurrency limit reached",
                    &request_id,
                );
                let status = response.status();
                self.log_request(RequestLogRecord {
                    request_id: request_id.clone(),
                    method: method.to_string(),
                    path: redacted_path.clone(),
                    route_id: Some(route_id.clone()),
                    upstream: None,
                    status: status.as_u16(),
                    latency_ms: started.elapsed().as_millis(),
                    error: Some("concurrency_limit".to_owned()),
                });
                record_span_fields(
                    Some(&route_id),
                    None,
                    status,
                    started,
                    Some("concurrency_limit"),
                );
                self.telemetry.record_request(RequestMetric {
                    route_id: Some(route_id.clone()),
                    method: method.to_string(),
                    status: status.as_u16(),
                    latency_seconds: started.elapsed().as_secs_f64(),
                    upstream_error: true,
                });
                let record = RequestInsert {
                    request_id,
                    trace_id,
                    route_id: Some(route_id),
                    method: method.to_string(),
                    path: request_path,
                    redacted_query,
                    query_hash,
                    status: status.as_u16(),
                    latency_ms: started.elapsed().as_millis(),
                    upstream: None,
                    is_error: true,
                    is_slow: false,
                    capture_policy: matched.route.capture.policy.to_string(),
                    capture_dropped: true,
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
                        plugin_decisions: Vec::new(),
                        permit: None,
                    },
                ));
            }
        };
        let request_content_type = content_type(request.headers());
        let retry_eligible = retry_eligible(&method, request.headers());
        let mut template_headers = request.headers().clone();
        let policy_headers = policy_headers(request.headers());
        let policy_preview_limit = state.policy.max_body_preview_bytes(&route_id);
        let (request_parts, request_body) = request.into_parts();
        let mut request_body = request_body.boxed();
        let body_preview = if policy_preview_limit > 0 {
            match split_body_preview(request_body, policy_preview_limit).await {
                Ok((preview, body)) => {
                    request_body = body;
                    Some(preview)
                }
                Err(err) => {
                    let response = response_with_request_id_text(
                        StatusCode::FORBIDDEN,
                        "request denied by policy",
                        &request_id,
                    );
                    let status = response.status();
                    let error = Some(format!("policy_body_preview_failed: {err}"));
                    self.log_request(RequestLogRecord {
                        request_id: request_id.clone(),
                        method: method.to_string(),
                        path: redacted_path.clone(),
                        route_id: Some(route_id.clone()),
                        upstream: None,
                        status: status.as_u16(),
                        latency_ms: started.elapsed().as_millis(),
                        error: error.clone(),
                    });
                    record_span_fields(Some(&route_id), None, status, started, error.as_deref());
                    self.telemetry.record_request(RequestMetric {
                        route_id: Some(route_id.clone()),
                        method: method.to_string(),
                        status: status.as_u16(),
                        latency_seconds: started.elapsed().as_secs_f64(),
                        upstream_error: false,
                    });
                    let record = RequestInsert {
                        request_id,
                        trace_id,
                        route_id: Some(route_id),
                        method: method.to_string(),
                        path: request_path,
                        redacted_query,
                        query_hash,
                        status: status.as_u16(),
                        latency_ms: started.elapsed().as_millis(),
                        upstream: None,
                        is_error: false,
                        is_slow: false,
                        capture_policy: matched.route.capture.policy.to_string(),
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
                            plugin_decisions: Vec::new(),
                            permit: permit.take(),
                        },
                    ));
                }
            }
        } else {
            None
        };

        let policy_evaluation = state
            .policy
            .evaluate(PolicyRequest {
                route_id: route_id.clone(),
                request_id: request_id.clone(),
                method: method.to_string(),
                path: request_path.clone(),
                query: redacted_query.clone(),
                headers: policy_headers,
                sensitive_headers: state.redaction.headers.clone(),
                client_address: remote_addr.to_string(),
                body_preview,
            })
            .await;
        self.record_plugin_metrics(&policy_evaluation.records);

        if let Some(deny) = policy_evaluation.denied.clone() {
            let status = status_from_deny(&deny);
            let response = response_with_request_id_text(status, &deny.message, &request_id);
            self.log_request(RequestLogRecord {
                request_id: request_id.clone(),
                method: method.to_string(),
                path: redacted_path.clone(),
                route_id: Some(route_id.clone()),
                upstream: None,
                status: status.as_u16(),
                latency_ms: started.elapsed().as_millis(),
                error: Some("policy_denied".to_owned()),
            });
            record_span_fields(
                Some(&route_id),
                None,
                status,
                started,
                Some("policy_denied"),
            );
            self.telemetry.record_request(RequestMetric {
                route_id: Some(route_id.clone()),
                method: method.to_string(),
                status: status.as_u16(),
                latency_seconds: started.elapsed().as_secs_f64(),
                upstream_error: status.is_server_error(),
            });
            let is_slow = started.elapsed() >= matched.route.capture.slow_threshold;
            let record = RequestInsert {
                request_id: request_id.clone(),
                trace_id,
                route_id: Some(route_id.clone()),
                method: method.to_string(),
                path: request_path,
                redacted_query,
                query_hash,
                status: status.as_u16(),
                latency_ms: started.elapsed().as_millis(),
                upstream: None,
                is_error: status.is_server_error(),
                is_slow,
                capture_policy: matched.route.capture.policy.to_string(),
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
                    plugin_decisions: plugin_decision_inserts(
                        &request_id,
                        &policy_evaluation.records,
                    ),
                    permit: permit.take(),
                },
            ));
        }

        apply_policy_mutations(
            &mut template_headers,
            &policy_evaluation.set_headers,
            &policy_evaluation.remove_headers,
        );
        let Some((upstream_index, upstream)) = route_runtime.select_upstream(&matched.route) else {
            let response = response_with_request_id(
                StatusCode::SERVICE_UNAVAILABLE,
                "all route upstreams are temporarily unhealthy",
                &request_id,
            );
            let status = response.status();
            self.log_request(RequestLogRecord {
                request_id: request_id.clone(),
                method: method.to_string(),
                path: redacted_path.clone(),
                route_id: Some(route_id.clone()),
                upstream: None,
                status: status.as_u16(),
                latency_ms: started.elapsed().as_millis(),
                error: Some("all_upstreams_unhealthy".to_owned()),
            });
            record_span_fields(
                Some(&route_id),
                None,
                status,
                started,
                Some("all_upstreams_unhealthy"),
            );
            self.telemetry.record_request(RequestMetric {
                route_id: Some(route_id.clone()),
                method: method.to_string(),
                status: status.as_u16(),
                latency_seconds: started.elapsed().as_secs_f64(),
                upstream_error: true,
            });
            let record = RequestInsert {
                request_id: request_id.clone(),
                trace_id,
                route_id: Some(route_id),
                method: method.to_string(),
                path: request_path,
                redacted_query,
                query_hash,
                status: status.as_u16(),
                latency_ms: started.elapsed().as_millis(),
                upstream: None,
                is_error: true,
                is_slow: false,
                capture_policy: matched.route.capture.policy.to_string(),
                capture_dropped: true,
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
                    plugin_decisions: plugin_decision_inserts(
                        &request_id,
                        &policy_evaluation.records,
                    ),
                    permit: permit.take(),
                },
            ));
        };
        let upstream_origin = upstream.origin();
        let request_capture_enabled = matched.route.capture.policy != CapturePolicy::Off
            && matched.route.capture.capture_request_body
            && request_content_type
                .as_deref()
                .map(is_capturable_content_type)
                .unwrap_or(false);
        let request_capture = Arc::new(Mutex::new(CaptureBuffer::new(
            request_capture_enabled,
            state.storage_config.max_capture_bytes_per_request,
        )));
        let template = RequestTemplate {
            method,
            uri: request_parts.uri,
            version: request_parts.version,
            headers: template_headers,
        };

        let result = if retry_eligible {
            drop(request_body);
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
                CapturingBody::new(request_body, request_capture.clone(), None).boxed(),
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
        route_runtime.record_upstream_result(upstream_index, upstream_error);
        let is_slow = started.elapsed() >= matched.route.capture.slow_threshold;
        let should_capture = matched
            .route
            .capture
            .policy
            .should_capture(upstream_error, is_slow);

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
            let remaining = state
                .storage_config
                .max_capture_bytes_per_request
                .saturating_sub(request_captured_len as u64);
            remaining.min(matched.route.capture.capture_response_body_bytes)
        } else {
            0
        };

        let record = RequestInsert {
            request_id: request_id.clone(),
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
                plugin_decisions: plugin_decision_inserts(&request_id, &policy_evaluation.records),
                permit: permit.take(),
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

    fn record_plugin_metrics(&self, records: &[PolicyDecisionRecord]) {
        for record in records {
            let outcome = if record.timed_out {
                "timeout"
            } else if record.error.is_some() {
                "error"
            } else {
                "ok"
            };
            self.telemetry.record_plugin_decision(PluginDecisionMetric {
                plugin_id: record.plugin_id.clone(),
                route_id: record.route_id.clone(),
                action: record.action.clone(),
                outcome: outcome.to_owned(),
                duration_seconds: record.duration.as_secs_f64(),
                timed_out: record.timed_out,
                errored: record.error.is_some(),
            });
        }
    }

    fn attach_storage_finalizer(
        &self,
        response: Response<ProxyBody>,
        input: StorageFinalizerInput,
    ) -> Response<ProxyBody> {
        let (parts, body) = response.into_parts();
        let response_headers = stored_headers(&parts.headers, &self.state.load().redaction);
        let response_content_type = content_type(&parts.headers);
        let response_capture = Arc::new(Mutex::new(CaptureBuffer::new(
            input.response_capture_enabled,
            input.response_capture_limit,
        )));
        let finalizer = CaptureFinalizer {
            capture_writer: self.capture_writer.clone(),
            telemetry: self.telemetry.clone(),
            record: Some(input.record),
            request_headers: Some(input.request_headers),
            response_headers: Some(response_headers),
            request_capture: input.request_capture,
            response_capture: response_capture.clone(),
            should_capture: input.should_capture,
            request_content_type: input.request_content_type,
            response_content_type,
            plugin_decisions: Some(input.plugin_decisions),
            finalized: false,
            _permit: input.permit,
        };
        let body = CapturingBody::new(body, response_capture, Some(finalizer)).boxed();

        Response::from_parts(parts, body)
    }
}

pin_project_lite::pin_project! {
    struct PrefixBody {
        prefix: VecDeque<Frame<Bytes>>,
        #[pin]
        inner: ProxyBody,
    }
}

impl PrefixBody {
    fn new(prefix: VecDeque<Frame<Bytes>>, inner: ProxyBody) -> Self {
        Self { prefix, inner }
    }
}

impl Body for PrefixBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let mut this = self.project();
        if let Some(frame) = this.prefix.pop_front() {
            return Poll::Ready(Some(Ok(frame)));
        }
        this.inner.as_mut().poll_frame(cx)
    }
}

async fn split_body_preview(
    mut body: ProxyBody,
    limit: u64,
) -> Result<(Vec<u8>, ProxyBody), hyper::Error> {
    if limit == 0 {
        return Ok((Vec::new(), body));
    }

    let limit = limit.min(usize::MAX as u64) as usize;
    let mut preview = Vec::with_capacity(limit.min(8192));
    let mut prefix = VecDeque::new();

    while preview.len() < limit {
        let Some(frame) = body.frame().await else {
            break;
        };
        let frame = frame?;
        if let Some(data) = frame.data_ref() {
            let remaining = limit.saturating_sub(preview.len());
            preview.extend_from_slice(&data[..remaining.min(data.len())]);
        }
        prefix.push_back(frame);
    }

    Ok((preview, PrefixBody::new(prefix, body).boxed()))
}

struct StorageFinalizerInput {
    record: RequestInsert,
    request_headers: Vec<StoredHeader>,
    request_capture: Arc<Mutex<CaptureBuffer>>,
    should_capture: bool,
    request_content_type: Option<String>,
    response_capture_enabled: bool,
    response_capture_limit: u64,
    plugin_decisions: Vec<PluginDecisionInsert>,
    permit: Option<OwnedSemaphorePermit>,
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

async fn serve_proxy_stream(
    stream: TcpStream,
    remote_addr: SocketAddr,
    proxy: Proxy,
    tls_acceptor: Option<TlsAcceptor>,
) {
    if let Some(acceptor) = tls_acceptor {
        match acceptor.accept(stream).await {
            Ok(stream) => {
                let service = service_fn(move |request| {
                    let proxy = proxy.clone();
                    async move { proxy.handle(request, remote_addr).await }
                });
                let io = TokioIo::new(stream);
                if let Err(err) = ServerBuilder::new(TokioExecutor::new())
                    .serve_connection_with_upgrades(io, service)
                    .await
                {
                    tracing::warn!(error = %err, "TLS connection failed");
                }
            }
            Err(err) => {
                tracing::warn!(error = %err, "TLS handshake failed");
            }
        }
    } else {
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
    }
}

fn tls_acceptor(config: &TlsConfig) -> Result<Option<TlsAcceptor>, ProxyError> {
    install_rustls_crypto_provider();
    if !config.enabled {
        return Ok(None);
    }
    let cert_path = config
        .cert_path
        .as_ref()
        .ok_or_else(|| ProxyError::Tls("server TLS cert_path is missing".to_owned()))?;
    let key_path = config
        .key_path
        .as_ref()
        .ok_or_else(|| ProxyError::Tls("server TLS key_path is missing".to_owned()))?;
    let certs = load_certs(cert_path)?;
    let key = load_private_key(key_path)?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|err| ProxyError::Tls(err.to_string()))?;
    Ok(Some(TlsAcceptor::from(Arc::new(config))))
}

fn build_upstream_client(config: &AppConfig) -> Result<ProxyClient, ProxyError> {
    install_rustls_crypto_provider();
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca_path) = config.upstream_tls.ca_cert_path.as_ref() {
        for cert in load_certs(ca_path)? {
            roots
                .add(cert)
                .map_err(|err| ProxyError::Tls(format!("invalid upstream CA: {err}")))?;
        }
    }
    let tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
    Ok(Client::builder(TokioExecutor::new()).build(connector))
}

fn install_rustls_crypto_provider() {
    static INSTALL: OnceLock<()> = OnceLock::new();
    INSTALL.get_or_init(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

fn load_certs(path: &PathBuf) -> Result<Vec<CertificateDer<'static>>, ProxyError> {
    let file = File::open(path)
        .map_err(|err| ProxyError::Tls(format!("failed to open cert {}: {err}", path.display())))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| ProxyError::Tls(format!("failed to read cert {}: {err}", path.display())))
}

fn load_private_key(path: &PathBuf) -> Result<PrivateKeyDer<'static>, ProxyError> {
    let file = File::open(path)
        .map_err(|err| ProxyError::Tls(format!("failed to open key {}: {err}", path.display())))?;
    let mut reader = BufReader::new(file);
    rustls_pemfile::private_key(&mut reader)
        .map_err(|err| ProxyError::Tls(format!("failed to read key {}: {err}", path.display())))?
        .ok_or_else(|| ProxyError::Tls(format!("no private key found in {}", path.display())))
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
    capture_writer: CaptureWriter,
    telemetry: Telemetry,
    record: Option<RequestInsert>,
    request_headers: Option<Vec<StoredHeader>>,
    response_headers: Option<Vec<StoredHeader>>,
    request_capture: Arc<Mutex<CaptureBuffer>>,
    response_capture: Arc<Mutex<CaptureBuffer>>,
    should_capture: bool,
    request_content_type: Option<String>,
    response_content_type: Option<String>,
    plugin_decisions: Option<Vec<PluginDecisionInsert>>,
    finalized: bool,
    _permit: Option<OwnedSemaphorePermit>,
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
        let plugin_decisions = self.plugin_decisions.take().unwrap_or_default();
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
        self.capture_writer.enqueue(
            CaptureWrite {
                record,
                request_headers,
                response_headers,
                capture,
                plugin_decisions,
            },
            &self.telemetry,
        );
    }
}

impl CaptureWriter {
    fn spawn(storage: Arc<Storage>, telemetry: Telemetry, capacity: usize) -> Self {
        let (sender, mut receiver) = mpsc::channel::<CaptureWrite>(capacity);
        tokio::spawn(async move {
            while let Some(write) = receiver.recv().await {
                let wrote_capture = write.capture.is_some() && !write.record.capture_dropped;
                match storage
                    .insert_request(
                        write.record,
                        write.request_headers,
                        write.response_headers,
                        write.capture,
                        write.plugin_decisions,
                    )
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
            }
        });

        Self { sender }
    }

    fn enqueue(&self, write: CaptureWrite, telemetry: &Telemetry) {
        match self.sender.try_send(write) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(returned)) => {
                let returned = metadata_only_capture_fallback(returned, telemetry);
                tracing::warn!("capture writer queue full; recording metadata-only fallback");
                if let Err(err) = self.sender.try_send(returned) {
                    tracing::warn!(error = %err, "failed to enqueue metadata-only capture fallback");
                    telemetry.record_capture_dropped();
                }
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                tracing::warn!("capture writer queue closed; dropping capture record");
                telemetry.record_capture_dropped();
            }
        }
    }
}

fn metadata_only_capture_fallback(mut write: CaptureWrite, telemetry: &Telemetry) -> CaptureWrite {
    let had_capture = write.capture.take().is_some();
    write.record.capture_dropped = true;
    if had_capture {
        telemetry.record_capture_dropped();
    }
    write
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

fn policy_headers(headers: &HeaderMap) -> Vec<PolicyHeader> {
    headers
        .iter()
        .map(|(name, value)| {
            let value = value
                .to_str()
                .map(truncate_header_value)
                .unwrap_or_else(|_| "<non-utf8>".to_owned());
            PolicyHeader {
                name: name.as_str().to_ascii_lowercase(),
                value,
            }
        })
        .collect()
}

fn apply_policy_mutations(
    headers: &mut HeaderMap,
    set_headers: &[HeaderMutation],
    remove_headers: &[String],
) {
    for name in remove_headers {
        if let Ok(name) = HeaderName::from_bytes(name.as_bytes()) {
            headers.remove(name);
        }
    }

    for mutation in set_headers {
        let Ok(name) = HeaderName::from_bytes(mutation.name.as_bytes()) else {
            continue;
        };
        let Ok(value) = HeaderValue::from_str(&mutation.value) else {
            continue;
        };
        headers.insert(name, value);
    }
}

fn plugin_decision_inserts(
    request_id: &str,
    records: &[PolicyDecisionRecord],
) -> Vec<PluginDecisionInsert> {
    records
        .iter()
        .map(|record| PluginDecisionInsert {
            request_id: request_id.to_owned(),
            plugin_id: record.plugin_id.clone(),
            route_id: record.route_id.clone(),
            action: record.action.clone(),
            deny_status: record.deny_status,
            set_headers: record.set_headers.clone(),
            remove_headers: record.remove_headers.clone(),
            events: record
                .events
                .iter()
                .map(|event| StoredPluginEvent {
                    name: event.name.clone(),
                    code: event.code.clone(),
                })
                .collect(),
            duration_ms: record.duration.as_millis(),
            timed_out: record.timed_out,
            error: record.error.clone(),
            created_at_ms: now_ms(),
        })
        .collect()
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
    admin_state: AdminState,
) -> Result<Response<ProxyBody>, Infallible> {
    let admin = {
        let config = admin_state.current_config.read().await;
        config.admin.clone()
    };
    if !admin_authorized(&request, &admin) {
        return Ok(text_response(StatusCode::UNAUTHORIZED, "unauthorized\n"));
    }

    let response = match (request.method(), request.uri().path()) {
        (&Method::GET, "/health/live") => text_response(StatusCode::OK, "live\n"),
        (&Method::GET, "/health/ready") => match admin_state.storage.health_check().await {
            Ok(()) => text_response(StatusCode::OK, "ready\n"),
            Err(err) => text_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("storage not ready: {err}\n"),
            ),
        },
        (&Method::GET, "/metrics") if admin_state.telemetry.prometheus_enabled() => {
            match admin_state.telemetry.render_prometheus() {
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
        (&Method::POST, "/admin/reload") => reload_gateway(admin_state).await,
        _ => text_response(StatusCode::NOT_FOUND, "not found\n"),
    };

    Ok(response)
}

fn admin_authorized(request: &Request<Incoming>, admin: &AdminConfig) -> bool {
    let Some(token) = admin.token.as_deref() else {
        return request.uri().path() != "/admin/reload";
    };
    let Some(header) = request.headers().get(AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = header.to_str() else {
        return false;
    };
    value == format!("Bearer {token}")
}

async fn reload_gateway(admin_state: AdminState) -> Response<ProxyBody> {
    let Some(path) = admin_state.config_path.clone() else {
        return text_response(StatusCode::BAD_REQUEST, "reload config path unavailable\n");
    };
    let new_config = match tracegate_config::load_config(&path) {
        Ok(config) => config,
        Err(err) => {
            return response_with_content_type(
                StatusCode::BAD_REQUEST,
                "application/json",
                format!(
                    r#"{{"status":"rejected","error":"{}"}}"#,
                    json_escape(&err.to_string())
                ),
            );
        }
    };

    let mut current = admin_state.current_config.write().await;
    if let Err(err) = validate_reload_immutables(&current, &new_config) {
        return response_with_content_type(
            StatusCode::BAD_REQUEST,
            "application/json",
            format!(r#"{{"status":"rejected","error":"{}"}}"#, json_escape(&err)),
        );
    }
    let new_state = match GatewayState::new(&new_config) {
        Ok(state) => state,
        Err(err) => {
            return response_with_content_type(
                StatusCode::BAD_REQUEST,
                "application/json",
                format!(
                    r#"{{"status":"rejected","error":"{}"}}"#,
                    json_escape(&err.to_string())
                ),
            );
        }
    };
    let routes = new_config.routes.len();
    let plugins = new_config.plugins.len();
    admin_state.gateway_state.store(Arc::new(new_state));
    *current = new_config;
    response_with_content_type(
        StatusCode::OK,
        "application/json",
        format!(r#"{{"status":"reloaded","routes":{routes},"plugins":{plugins}}}"#),
    )
}

fn validate_reload_immutables(current: &AppConfig, next: &AppConfig) -> Result<(), String> {
    if current.mode != next.mode {
        return Err("server.mode cannot change during hot reload".to_owned());
    }
    if current.listen != next.listen {
        return Err("server.listen cannot change during hot reload".to_owned());
    }
    if current.admin_listen != next.admin_listen {
        return Err("server.admin_listen cannot change during hot reload".to_owned());
    }
    if current.server_tls != next.server_tls {
        return Err("server.tls cannot change during hot reload".to_owned());
    }
    if current.admin.token_env != next.admin.token_env
        || current.admin.allow_internal_network != next.admin.allow_internal_network
    {
        return Err("admin auth settings cannot change during hot reload".to_owned());
    }
    if current.upstream_tls != next.upstream_tls {
        return Err("upstream_tls cannot change during hot reload".to_owned());
    }
    if current.storage != next.storage {
        return Err("storage settings cannot change during hot reload".to_owned());
    }
    Ok(())
}

fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
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
    response_with_request_id_text(status, body, request_id)
}

fn response_with_request_id_text(
    status: StatusCode,
    body: &str,
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

fn full_body(body: &str) -> ProxyBody {
    Full::new(Bytes::from(body.to_owned()))
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

    #[test]
    fn capture_queue_full_fallback_is_metadata_only() {
        let telemetry = Telemetry::new(&tracegate_core::ObservabilityConfig {
            service_name: "tracegate-test".to_owned(),
            environment: "test".to_owned(),
            otlp_endpoint: None,
            prometheus_enabled: true,
            json_logs: true,
        });
        let write = CaptureWrite {
            record: RequestInsert {
                request_id: "req".to_owned(),
                trace_id: None,
                route_id: Some("payments".to_owned()),
                method: "POST".to_owned(),
                path: "/api/payments/fail".to_owned(),
                redacted_query: None,
                query_hash: None,
                status: 500,
                latency_ms: 12,
                upstream: Some("https://payments-service:4443".to_owned()),
                is_error: true,
                is_slow: false,
                capture_policy: CapturePolicy::Errors.to_string(),
                capture_dropped: false,
                created_at_ms: 1,
            },
            request_headers: Vec::new(),
            response_headers: Vec::new(),
            capture: Some(CaptureInsert {
                request_content_type: Some("application/json".to_owned()),
                response_content_type: Some("application/json".to_owned()),
                request_body: Some(b"request".to_vec()),
                response_body: Some(b"response".to_vec()),
                request_body_truncated: false,
                response_body_truncated: false,
                request_body_sha256: Some("request-sha".to_owned()),
                response_body_sha256: Some("response-sha".to_owned()),
            }),
            plugin_decisions: Vec::new(),
        };

        let fallback = metadata_only_capture_fallback(write, &telemetry);

        assert!(fallback.record.capture_dropped);
        assert!(fallback.capture.is_none());
        assert!(
            telemetry
                .render_prometheus()
                .unwrap()
                .contains("tracegate_capture_dropped_total")
        );
    }
}
