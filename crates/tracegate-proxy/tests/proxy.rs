use std::{net::SocketAddr, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::OriginalUri,
    http::{HeaderMap, Response, StatusCode},
    routing::any,
};
use tokio::{net::TcpListener, sync::oneshot};
use tracegate_core::{AppConfig, ObservabilityConfig, Route, Upstream};
use tracegate_observability::{Telemetry, trace_id_hex_from_traceparent};
use tracegate_proxy::{serve_listener, serve_listeners};

async fn start_backend(
    status: StatusCode,
    body: &'static str,
    delay: Option<Duration>,
) -> (SocketAddr, oneshot::Sender<()>) {
    let app = Router::new().route(
        "/{*path}",
        any(move |OriginalUri(uri): OriginalUri| async move {
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
        observability: ObservabilityConfig {
            service_name: "tracegate-test".to_owned(),
            environment: "test".to_owned(),
            otlp_endpoint: None,
            prometheus_enabled: true,
            json_logs: true,
        },
        routes,
    }
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
