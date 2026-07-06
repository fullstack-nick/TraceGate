use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::{HeaderMap, Method, StatusCode},
    routing::any,
};
use axum_server::tls_rustls::RustlsConfig;
use clap::{Parser, ValueEnum};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing_subscriber::{EnvFilter, fmt};

#[derive(Debug, Parser)]
struct Cli {
    #[arg(long, value_enum)]
    service: ServiceKind,
    #[arg(long)]
    listen: String,
    #[arg(long)]
    tls_cert: Option<PathBuf>,
    #[arg(long)]
    tls_key: Option<PathBuf>,
}

#[derive(Clone, Debug, ValueEnum)]
enum ServiceKind {
    Users,
    Payments,
    Replay,
}

#[derive(Clone)]
struct AppState {
    service: ServiceKind,
}

#[derive(Serialize)]
struct DemoResponse {
    service: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    query: Option<String>,
    ok: bool,
    body_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    original_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    replay_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .flatten_event(true)
        .init();

    let cli = Cli::parse();
    let listen = SocketAddr::from_str(&cli.listen)?;
    let state = AppState {
        service: cli.service,
    };
    let app = Router::new()
        .route("/{*path}", any(handler))
        .with_state(state);

    tracing::info!(listen = %listen, tls = cli.tls_cert.is_some(), "starting demo backend");
    match (cli.tls_cert, cli.tls_key) {
        (Some(cert), Some(key)) => {
            let tls = RustlsConfig::from_pem_file(cert, key).await?;
            axum_server::bind_rustls(listen, tls)
                .serve(app.into_make_service())
                .await?;
        }
        (None, None) => {
            let listener = TcpListener::bind(listen).await?;
            axum::serve(listener, app).await?;
        }
        _ => anyhow::bail!("--tls-cert and --tls-key must be provided together"),
    }
    Ok(())
}

async fn handler(
    State(state): State<AppState>,
    method: Method,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<DemoResponse>) {
    match state.service {
        ServiceKind::Users => (
            StatusCode::OK,
            Json(DemoResponse {
                service: "users",
                method: None,
                path: uri.path().to_owned(),
                query: None,
                ok: true,
                body_len: body.len(),
                replay: None,
                original_request_id: None,
                replay_request_id: None,
                payload: None,
            }),
        ),
        ServiceKind::Replay => {
            let replay = header_value(&headers, "x-tracegate-replay");
            let original_request_id = header_value(&headers, "x-tracegate-original-request-id");
            let replay_request_id = header_value(&headers, "x-request-id");
            tracing::info!(
                method = %method,
                path = uri.path(),
                query = uri.query().unwrap_or(""),
                body_len = body.len(),
                replay = replay.as_deref().unwrap_or(""),
                original_request_id = original_request_id.as_deref().unwrap_or(""),
                replay_request_id = replay_request_id.as_deref().unwrap_or(""),
                "replay target received request"
            );
            (
                StatusCode::OK,
                Json(DemoResponse {
                    service: "replay",
                    method: Some(method.to_string()),
                    path: uri.path().to_owned(),
                    query: uri.query().map(str::to_owned),
                    ok: true,
                    body_len: body.len(),
                    replay,
                    original_request_id,
                    replay_request_id,
                    payload: None,
                }),
            )
        }
        ServiceKind::Payments if uri.path() == "/api/payments/slow" => {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
            (
                StatusCode::OK,
                Json(DemoResponse {
                    service: "payments",
                    method: None,
                    path: uri.path().to_owned(),
                    query: None,
                    ok: true,
                    body_len: body.len(),
                    replay: None,
                    original_request_id: None,
                    replay_request_id: None,
                    payload: None,
                }),
            )
        }
        ServiceKind::Payments if uri.path() == "/api/payments/large-fail" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(DemoResponse {
                service: "payments",
                method: None,
                path: uri.path().to_owned(),
                query: None,
                ok: false,
                body_len: body.len(),
                replay: None,
                original_request_id: None,
                replay_request_id: None,
                payload: Some("x".repeat(8192)),
            }),
        ),
        ServiceKind::Payments if uri.path() == "/api/payments/fail" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(DemoResponse {
                service: "payments",
                method: None,
                path: uri.path().to_owned(),
                query: None,
                ok: false,
                body_len: body.len(),
                replay: None,
                original_request_id: None,
                replay_request_id: None,
                payload: None,
            }),
        ),
        ServiceKind::Payments => (
            StatusCode::OK,
            Json(DemoResponse {
                service: "payments",
                method: None,
                path: uri.path().to_owned(),
                query: None,
                ok: true,
                body_len: body.len(),
                replay: None,
                original_request_id: None,
                replay_request_id: None,
                payload: None,
            }),
        ),
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}
