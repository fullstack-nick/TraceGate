use std::{net::SocketAddr, time::Duration};

use axum::{
    Router,
    body::Body,
    extract::OriginalUri,
    http::{Response, StatusCode},
    routing::any,
};
use tokio::{net::TcpListener, sync::oneshot};
use tracegate_core::{AppConfig, Route, Upstream};
use tracegate_proxy::serve_listener;

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

async fn start_proxy(route: Route) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    tokio::spawn(async move {
        serve_listener(
            listener,
            AppConfig {
                listen: addr,
                json_logs: true,
                routes: vec![route],
            },
            async {
                let _ = shutdown_rx.await;
            },
        )
        .await
        .unwrap();
    });

    (addr, shutdown_tx)
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
