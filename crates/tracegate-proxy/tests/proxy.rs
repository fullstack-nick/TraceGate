use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes as AxumBytes},
    extract::OriginalUri,
    http::{HeaderMap, Response, StatusCode},
    routing::any,
};
use tokio::{net::TcpListener, sync::oneshot};
use tracegate_core::{
    AppConfig, CaptureConfig, CapturePolicy, ObservabilityConfig, PluginConfig, PluginConfigValue,
    PluginHook, RedactionConfig, Route, StorageConfig, Upstream,
};
use tracegate_observability::{Telemetry, trace_id_hex_from_traceparent};
use tracegate_proxy::{serve_listener, serve_listeners};
use tracegate_storage::{ListFilters, Storage};

async fn start_backend(
    status: StatusCode,
    body: &'static str,
    delay: Option<Duration>,
) -> (SocketAddr, oneshot::Sender<()>) {
    let app = Router::new().route(
        "/{*path}",
        any(
            move |OriginalUri(uri): OriginalUri, _body: AxumBytes| async move {
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }

                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(format!(
                        r#"{{"body":"{body}","path":"{}"}}"#,
                        uri.path()
                    )))
                    .unwrap()
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

async fn start_header_echo_backend() -> (SocketAddr, oneshot::Sender<()>) {
    let app = Router::new().route(
        "/{*path}",
        any(|headers: HeaderMap| async move {
            let traceparent = headers
                .get("traceparent")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned();

            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/plain")
                .body(Body::from(traceparent))
                .unwrap()
        }),
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

async fn start_policy_header_backend() -> (SocketAddr, Arc<AtomicUsize>, oneshot::Sender<()>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_handler = hits.clone();
    let app = Router::new().route(
        "/{*path}",
        any(move |headers: HeaderMap, body: AxumBytes| {
            let hits = hits_for_handler.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let policy = headers
                    .get("x-tracegate-policy")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("")
                    .to_owned();

                Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/plain")
                    .body(Body::from(format!("{policy}:{}", body.len())))
                    .unwrap()
            }
        }),
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

    (addr, hits, shutdown_tx)
}

async fn start_proxy(route: Route) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let config = app_config(addr, "127.0.0.1:0".parse().unwrap(), vec![route]);
    let telemetry = Telemetry::new(&config.observability);

    tokio::spawn(async move {
        serve_listener(listener, config, telemetry, async {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    (addr, shutdown_tx)
}

async fn start_proxy_with_storage(
    route: Route,
    storage: StorageConfig,
) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let mut config = app_config(addr, "127.0.0.1:0".parse().unwrap(), vec![route]);
    config.storage = storage;
    let telemetry = Telemetry::new(&config.observability);

    tokio::spawn(async move {
        serve_listener(listener, config, telemetry, async {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    (addr, shutdown_tx)
}

async fn start_proxy_with_storage_and_plugins(
    route: Route,
    storage: StorageConfig,
    plugins: Vec<PluginConfig>,
) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let mut config = app_config(addr, "127.0.0.1:0".parse().unwrap(), vec![route]);
    config.storage = storage;
    config.plugins = plugins;
    let telemetry = Telemetry::new(&config.observability);

    tokio::spawn(async move {
        serve_listener(listener, config, telemetry, async {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    (addr, shutdown_tx)
}

async fn start_proxy_with_admin(route: Route) -> (SocketAddr, SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let admin_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let admin_addr = admin_listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let config = app_config(addr, admin_addr, vec![route]);
    let telemetry = Telemetry::new(&config.observability);

    tokio::spawn(async move {
        serve_listeners(listener, admin_listener, config, telemetry, async {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    (addr, admin_addr, shutdown_tx)
}

async fn start_broken_backend() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });

    addr
}

fn app_config(listen: SocketAddr, admin_listen: SocketAddr, routes: Vec<Route>) -> AppConfig {
    AppConfig {
        listen,
        admin_listen,
        storage: storage_config(),
        redaction: RedactionConfig::default(),
        observability: ObservabilityConfig {
            service_name: "tracegate-test".to_owned(),
            environment: "test".to_owned(),
            otlp_endpoint: None,
            prometheus_enabled: true,
            json_logs: true,
        },
        routes,
        plugins: Vec::new(),
    }
}

fn storage_config() -> StorageConfig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tracegate.db");
    std::mem::forget(dir);
    StorageConfig {
        url: sqlite_url(&path),
        max_total_capture_bytes: 16 * 1024 * 1024,
        max_capture_bytes_per_request: 1024 * 1024,
        ..StorageConfig::default()
    }
}

fn sqlite_url(path: &Path) -> String {
    let path = path.display().to_string().replace('\\', "/");
    if path.starts_with('/') {
        format!("sqlite://{path}")
    } else {
        format!("sqlite:///{path}")
    }
}

async fn wait_for_request(
    storage: &Storage,
    request_id: &str,
) -> tracegate_storage::RequestDetails {
    for _ in 0..40 {
        if let Some(details) = storage.show_request(request_id).await.unwrap() {
            return details;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    panic!("request {request_id} was not persisted");
}

fn route_to(addr: SocketAddr, path_prefix: &str, timeout: Duration, retries: u32) -> Route {
    Route::new(
        "test",
        vec!["*".to_owned()],
        path_prefix,
        vec![Upstream::parse(&format!("http://{addr}")).unwrap()],
        timeout,
        retries,
    )
}

fn route_to_with_capture(
    addr: SocketAddr,
    path_prefix: &str,
    timeout: Duration,
    capture: CaptureConfig,
) -> Route {
    Route::new_with_capture(
        "test",
        vec!["*".to_owned()],
        path_prefix,
        vec![Upstream::parse(&format!("http://{addr}")).unwrap()],
        timeout,
        0,
        capture,
    )
}

fn api_key_plugin() -> PluginConfig {
    PluginConfig {
        id: "api-key-guard".to_owned(),
        path: example_plugin_path(
            "api-key-guard",
            "tracegate_api_key_guard.wasm",
            &API_KEY_PLUGIN,
        ),
        hook: PluginHook::BeforeRequest,
        routes: vec!["test".to_owned()],
        timeout: Duration::from_millis(100),
        memory_limit_bytes: 16 * 1024 * 1024,
        fuel: 10_000_000,
        body_preview_bytes: 0,
        raw_headers: vec!["x-api-key".to_owned()],
        config: vec![
            PluginConfigValue {
                key: "header".to_owned(),
                value: "x-api-key".to_owned(),
            },
            PluginConfigValue {
                key: "expected".to_owned(),
                value: "tracegate-demo-key".to_owned(),
            },
        ],
    }
}

fn header_normalizer_plugin() -> PluginConfig {
    PluginConfig {
        id: "header-normalizer".to_owned(),
        path: example_plugin_path(
            "header-normalizer",
            "tracegate_header_normalizer.wasm",
            &HEADER_NORMALIZER_PLUGIN,
        ),
        hook: PluginHook::BeforeRequest,
        routes: vec!["test".to_owned()],
        timeout: Duration::from_millis(100),
        memory_limit_bytes: 16 * 1024 * 1024,
        fuel: 10_000_000,
        body_preview_bytes: 16,
        raw_headers: Vec::new(),
        config: vec![
            PluginConfigValue {
                key: "set_header".to_owned(),
                value: "x-tracegate-policy".to_owned(),
            },
            PluginConfigValue {
                key: "set_value".to_owned(),
                value: "normalized".to_owned(),
            },
        ],
    }
}

fn timeout_normalizer_plugin() -> PluginConfig {
    PluginConfig {
        id: "timeout-normalizer".to_owned(),
        path: example_plugin_path(
            "header-normalizer",
            "tracegate_header_normalizer.wasm",
            &HEADER_NORMALIZER_PLUGIN,
        ),
        hook: PluginHook::BeforeRequest,
        routes: vec!["test".to_owned()],
        timeout: Duration::from_millis(1),
        memory_limit_bytes: 16 * 1024 * 1024,
        fuel: 1_000_000_000,
        body_preview_bytes: 0,
        raw_headers: Vec::new(),
        config: vec![PluginConfigValue {
            key: "spin_iterations".to_owned(),
            value: "100000000".to_owned(),
        }],
    }
}

static API_KEY_PLUGIN: OnceLock<PathBuf> = OnceLock::new();
static HEADER_NORMALIZER_PLUGIN: OnceLock<PathBuf> = OnceLock::new();

fn example_plugin_path(
    plugin_dir: &str,
    wasm_name: &str,
    cache: &'static OnceLock<PathBuf>,
) -> PathBuf {
    cache
        .get_or_init(|| {
            let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .canonicalize()
                .unwrap();
            let manifest = repo
                .join("examples")
                .join("plugins")
                .join(plugin_dir)
                .join("Cargo.toml");
            let status = Command::new("cargo")
                .args([
                    "build",
                    "--manifest-path",
                    manifest.to_str().unwrap(),
                    "--target",
                    "wasm32-wasip2",
                    "--release",
                ])
                .current_dir(&repo)
                .status()
                .unwrap();
            assert!(status.success(), "failed to build {plugin_dir}");
            repo.join("examples")
                .join("plugins")
                .join(plugin_dir)
                .join("target")
                .join("wasm32-wasip2")
                .join("release")
                .join(wasm_name)
        })
        .clone()
}

#[tokio::test]
async fn propagates_incoming_traceparent_to_upstream() {
    let (backend_addr, backend_shutdown) = start_header_echo_backend().await;
    let (proxy_addr, proxy_shutdown) = start_proxy(route_to(
        backend_addr,
        "/api/users",
        Duration::from_secs(1),
        0,
    ))
    .await;

    let traceparent = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{proxy_addr}/api/users/123"))
        .header("traceparent", traceparent)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let upstream_traceparent = response.text().await.unwrap();
    assert_eq!(
        trace_id_hex_from_traceparent(&upstream_traceparent),
        trace_id_hex_from_traceparent(traceparent)
    );

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn injects_traceparent_when_missing() {
    let (backend_addr, backend_shutdown) = start_header_echo_backend().await;
    let (proxy_addr, proxy_shutdown) = start_proxy(route_to(
        backend_addr,
        "/api/users",
        Duration::from_secs(1),
        0,
    ))
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/users/123"))
        .await
        .unwrap();
    let traceparent = response.text().await.unwrap();

    assert!(traceparent.starts_with("00-"));
    assert_eq!(traceparent.len(), 55);

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn admin_health_and_metrics_endpoints_respond() {
    let (backend_addr, backend_shutdown) = start_backend(StatusCode::OK, "users", None).await;
    let (proxy_addr, admin_addr, proxy_shutdown) = start_proxy_with_admin(route_to(
        backend_addr,
        "/api/users",
        Duration::from_secs(1),
        0,
    ))
    .await;

    let live = reqwest::get(format!("http://{admin_addr}/health/live"))
        .await
        .unwrap();
    let ready = reqwest::get(format!("http://{admin_addr}/health/ready"))
        .await
        .unwrap();

    assert_eq!(live.status(), StatusCode::OK);
    assert_eq!(ready.status(), StatusCode::OK);

    let response = reqwest::get(format!("http://{proxy_addr}/api/users/123"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let metrics = reqwest::get(format!("http://{admin_addr}/metrics"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert!(metrics.contains("tracegate_requests_total"));
    assert!(metrics.contains("tracegate_request_duration_seconds"));
    assert!(metrics.contains("route_id=\"test\""));

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn proxies_successful_request_and_returns_request_id() {
    let (backend_addr, backend_shutdown) = start_backend(StatusCode::OK, "users", None).await;
    let (proxy_addr, proxy_shutdown) = start_proxy(route_to(
        backend_addr,
        "/api/users",
        Duration::from_secs(1),
        0,
    ))
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/users/123"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-request-id").is_some());
    assert!(response.text().await.unwrap().contains("/api/users/123"));

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn preserves_backend_500_response() {
    let (backend_addr, backend_shutdown) =
        start_backend(StatusCode::INTERNAL_SERVER_ERROR, "payments", None).await;
    let (proxy_addr, proxy_shutdown) = start_proxy(route_to(
        backend_addr,
        "/api/payments",
        Duration::from_secs(1),
        0,
    ))
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/payments/fail"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn captures_failed_request_with_redaction_and_truncation() {
    let (backend_addr, backend_shutdown) =
        start_backend(StatusCode::INTERNAL_SERVER_ERROR, "payments", None).await;
    let storage = StorageConfig {
        max_total_capture_bytes: 4096,
        max_capture_bytes_per_request: 32,
        ..storage_config()
    };
    let storage_for_query = storage.clone();
    let (proxy_addr, proxy_shutdown) = start_proxy_with_storage(
        route_to_with_capture(
            backend_addr,
            "/api/payments",
            Duration::from_secs(1),
            CaptureConfig {
                policy: CapturePolicy::ErrorsAndSlow,
                slow_threshold: Duration::from_millis(250),
                capture_request_body: true,
                capture_response_body_bytes: 16,
            },
        ),
        storage,
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!(
            "http://{proxy_addr}/api/payments/fail?token=secret&visible=yes"
        ))
        .header("authorization", "Bearer secret")
        .header("content-type", "application/json")
        .body(r#"{"card":"4242424242424242","note":"this body is intentionally long"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let request_id = response
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let _ = response.text().await.unwrap();

    let storage = Storage::connect(&storage_for_query).await.unwrap();
    storage.migrate().await.unwrap();
    let details = wait_for_request(&storage, &request_id).await;

    assert_eq!(
        details.request.redacted_query.as_deref(),
        Some("visible=yes")
    );
    assert!(details.request.query_hash.is_some());
    assert!(details.request.is_error);
    assert!(
        !details
            .request_headers
            .iter()
            .any(|h| h.name == "authorization")
    );
    assert!(
        details
            .request_headers
            .iter()
            .any(|h| h.name == "content-type" && h.value.starts_with("application/json"))
    );

    let capture = details.capture.unwrap();
    assert_eq!(capture.request_body.as_ref().unwrap().len(), 32);
    assert!(capture.request_body_truncated);
    assert!(capture.response_body_truncated);
    assert!(capture.request_body_sha256.is_some());

    let rows = storage
        .list_requests(ListFilters {
            failed: true,
            limit: 10,
            ..ListFilters::default()
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn wasm_policy_denies_missing_api_key_before_upstream() {
    let (backend_addr, hits, backend_shutdown) = start_policy_header_backend().await;
    let storage = storage_config();
    let storage_for_query = storage.clone();
    let (proxy_addr, proxy_shutdown) = start_proxy_with_storage_and_plugins(
        route_to(backend_addr, "/api/payments", Duration::from_secs(1), 0),
        storage,
        vec![api_key_plugin()],
    )
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/payments/ok"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let request_id = response
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let _ = response.text().await.unwrap();

    let storage = Storage::connect(&storage_for_query).await.unwrap();
    storage.migrate().await.unwrap();
    let details = wait_for_request(&storage, &request_id).await;

    assert_eq!(details.request.status, 403);
    assert_eq!(details.request.upstream.as_deref(), None);
    assert_eq!(details.plugin_decisions.len(), 1);
    assert_eq!(details.plugin_decisions[0].plugin_id, "api-key-guard");
    assert_eq!(details.plugin_decisions[0].action, "deny");
    assert_eq!(details.plugin_decisions[0].deny_status, Some(403));

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn wasm_policy_allows_valid_key_and_mutates_headers_with_body_preview() {
    let (backend_addr, hits, backend_shutdown) = start_policy_header_backend().await;
    let storage = storage_config();
    let storage_for_query = storage.clone();
    let (proxy_addr, proxy_shutdown) = start_proxy_with_storage_and_plugins(
        route_to(backend_addr, "/api/payments", Duration::from_secs(1), 0),
        storage,
        vec![api_key_plugin(), header_normalizer_plugin()],
    )
    .await;

    let response = reqwest::Client::new()
        .post(format!("http://{proxy_addr}/api/payments/ok"))
        .header("x-api-key", "tracegate-demo-key")
        .body("hello preview")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let request_id = response
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(response.text().await.unwrap(), "normalized:13");
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let storage = Storage::connect(&storage_for_query).await.unwrap();
    storage.migrate().await.unwrap();
    let details = wait_for_request(&storage, &request_id).await;

    assert_eq!(details.plugin_decisions.len(), 2);
    assert_eq!(details.plugin_decisions[0].action, "allow");
    assert_eq!(details.plugin_decisions[1].plugin_id, "header-normalizer");
    assert_eq!(
        details.plugin_decisions[1].set_headers,
        vec!["x-tracegate-policy"]
    );
    assert!(
        details.plugin_decisions[1]
            .events
            .iter()
            .any(|event| event.name == "headers-normalized-with-body-preview")
    );

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn wasm_policy_timeout_denies_before_upstream() {
    let (backend_addr, hits, backend_shutdown) = start_policy_header_backend().await;
    let storage = storage_config();
    let storage_for_query = storage.clone();
    let (proxy_addr, proxy_shutdown) = start_proxy_with_storage_and_plugins(
        route_to(backend_addr, "/api/payments", Duration::from_secs(1), 0),
        storage,
        vec![timeout_normalizer_plugin()],
    )
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/payments/timeout"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let request_id = response
        .headers()
        .get("x-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let _ = response.text().await.unwrap();

    let storage = Storage::connect(&storage_for_query).await.unwrap();
    storage.migrate().await.unwrap();
    let details = wait_for_request(&storage, &request_id).await;

    assert_eq!(details.request.status, 403);
    assert_eq!(details.request.upstream.as_deref(), None);
    assert_eq!(details.plugin_decisions.len(), 1);
    assert_eq!(details.plugin_decisions[0].plugin_id, "timeout-normalizer");
    assert_eq!(details.plugin_decisions[0].action, "deny");
    assert!(details.plugin_decisions[0].timed_out);

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn returns_404_for_no_route() {
    let (backend_addr, backend_shutdown) = start_backend(StatusCode::OK, "users", None).await;
    let (proxy_addr, proxy_shutdown) = start_proxy(route_to(
        backend_addr,
        "/api/users",
        Duration::from_secs(1),
        0,
    ))
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/orders/123"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(response.headers().get("x-request-id").is_some());

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn returns_504_for_timeout() {
    let (backend_addr, backend_shutdown) =
        start_backend(StatusCode::OK, "slow", Some(Duration::from_millis(250))).await;
    let (proxy_addr, proxy_shutdown) = start_proxy(route_to(
        backend_addr,
        "/api/slow",
        Duration::from_millis(25),
        0,
    ))
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/slow"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);

    let _ = proxy_shutdown.send(());
    let _ = backend_shutdown.send(());
}

#[tokio::test]
async fn returns_502_for_unavailable_upstream() {
    let unavailable_addr = start_broken_backend().await;

    let (proxy_addr, proxy_shutdown) = start_proxy(route_to(
        unavailable_addr,
        "/api/users",
        Duration::from_millis(100),
        0,
    ))
    .await;

    let response = reqwest::get(format!("http://{proxy_addr}/api/users/123"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(response.headers().get("x-request-id").is_some());

    let _ = proxy_shutdown.send(());
}
