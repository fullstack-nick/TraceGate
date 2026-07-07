use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
    time::Duration,
};

use axum::{
    Router,
    body::{Body, Bytes as AxumBytes},
    http::{Response, StatusCode},
    routing::any,
};
use criterion::{Criterion, criterion_group, criterion_main};
use tokio::{net::TcpListener, runtime::Runtime, sync::oneshot};
use tracegate_core::{
    AdminConfig, AppConfig, CaptureConfig, CapturePolicy, ObservabilityConfig, PluginConfig,
    PluginConfigValue, PluginHook, RedactionConfig, Route, RouteOptions, RuntimeMode,
    StorageConfig, TlsConfig, Upstream, UpstreamTlsConfig,
};
use tracegate_observability::Telemetry;
use tracegate_proxy::serve_listener;

async fn start_backend() -> (SocketAddr, oneshot::Sender<()>) {
    let app = Router::new().route(
        "/{*path}",
        any(|_body: AxumBytes| async move {
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"ok":true,"body":"benchmark"}"#))
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

async fn start_proxy(
    route: Route,
    storage: Option<StorageConfig>,
    plugins: Vec<PluginConfig>,
) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let mut config = app_config(addr, "127.0.0.1:0".parse().unwrap(), vec![route]);
    if let Some(storage) = storage {
        config.storage = storage;
    }
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

fn app_config(listen: SocketAddr, admin_listen: SocketAddr, routes: Vec<Route>) -> AppConfig {
    AppConfig {
        mode: RuntimeMode::Demo,
        listen,
        admin_listen,
        server_tls: TlsConfig::default(),
        admin: AdminConfig::default(),
        upstream_tls: UpstreamTlsConfig::default(),
        storage: storage_config(),
        redaction: RedactionConfig::default(),
        observability: ObservabilityConfig {
            service_name: "tracegate-bench".to_owned(),
            environment: "bench".to_owned(),
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
    let path = dir.path().join("tracegate-bench.db");
    std::mem::forget(dir);
    StorageConfig {
        url: sqlite_url(&path),
        max_total_capture_bytes: 64 * 1024 * 1024,
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

fn route_to(addr: SocketAddr, capture: CaptureConfig) -> Route {
    Route::new_with_options(
        "bench",
        vec!["*".to_owned()],
        "/api",
        vec![Upstream::parse(&format!("http://{addr}")).unwrap()],
        RouteOptions {
            timeout: Duration::from_secs(3),
            capture,
            ..RouteOptions::default()
        },
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
        routes: vec!["bench".to_owned()],
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

static API_KEY_PLUGIN: OnceLock<PathBuf> = OnceLock::new();

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

fn http_request(addr: SocketAddr, extra_headers: &[(&str, &str)]) {
    let mut stream = TcpStream::connect(addr).unwrap();
    let mut request =
        String::from("GET /api/payments/fail HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected response: {response}"
    );
}

fn release_overhead(c: &mut Criterion) {
    let runtime = Runtime::new().unwrap();
    let (backend_addr, _backend_shutdown) = runtime.block_on(start_backend());

    let capture_off = CaptureConfig::default();
    let capture_always = CaptureConfig {
        policy: CapturePolicy::Always,
        capture_request_body: true,
        capture_response_body_bytes: 4096,
        ..CaptureConfig::default()
    };

    let (proxy_addr, _proxy_shutdown) = runtime.block_on(start_proxy(
        route_to(backend_addr, capture_off.clone()),
        None,
        vec![],
    ));
    let (capture_addr, _capture_shutdown) = runtime.block_on(start_proxy(
        route_to(backend_addr, capture_always),
        Some(storage_config()),
        vec![],
    ));
    let (plugin_addr, _plugin_shutdown) = runtime.block_on(start_proxy(
        route_to(backend_addr, capture_off),
        Some(storage_config()),
        vec![api_key_plugin()],
    ));

    let mut group = c.benchmark_group("release_overhead");
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(2));

    group.bench_function("direct_upstream", |b| {
        b.iter(|| http_request(backend_addr, &[]))
    });
    group.bench_function("proxy_only", |b| b.iter(|| http_request(proxy_addr, &[])));
    group.bench_function("proxy_with_capture", |b| {
        b.iter(|| http_request(capture_addr, &[("content-type", "application/json")]))
    });
    group.bench_function("proxy_with_plugin", |b| {
        b.iter(|| http_request(plugin_addr, &[("x-api-key", "tracegate-demo-key")]))
    });

    group.finish();
}

criterion_group!(benches, release_overhead);
criterion_main!(benches);
