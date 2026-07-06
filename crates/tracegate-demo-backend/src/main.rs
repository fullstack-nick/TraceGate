use std::{net::SocketAddr, str::FromStr};

use axum::{
    Json, Router,
    body::Bytes,
    extract::{OriginalUri, State},
    http::StatusCode,
    routing::any,
};
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
}

#[derive(Clone, Debug, ValueEnum)]
enum ServiceKind {
    Users,
    Payments,
}

#[derive(Clone)]
struct AppState {
    service: ServiceKind,
}

#[derive(Serialize)]
struct DemoResponse {
    service: &'static str,
    path: String,
    ok: bool,
    body_len: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    payload: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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

    tracing::info!(listen = %listen, "starting demo backend");
    let listener = TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handler(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    body: Bytes,
) -> (StatusCode, Json<DemoResponse>) {
    match state.service {
        ServiceKind::Users => (
            StatusCode::OK,
            Json(DemoResponse {
                service: "users",
                path: uri.path().to_owned(),
                ok: true,
                body_len: body.len(),
                payload: None,
            }),
        ),
        ServiceKind::Payments if uri.path() == "/api/payments/slow" => {
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
            (
                StatusCode::OK,
                Json(DemoResponse {
                    service: "payments",
                    path: uri.path().to_owned(),
                    ok: true,
                    body_len: body.len(),
                    payload: None,
                }),
            )
        }
        ServiceKind::Payments if uri.path() == "/api/payments/large-fail" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(DemoResponse {
                service: "payments",
                path: uri.path().to_owned(),
                ok: false,
                body_len: body.len(),
                payload: Some("x".repeat(8192)),
            }),
        ),
        ServiceKind::Payments if uri.path() == "/api/payments/fail" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(DemoResponse {
                service: "payments",
                path: uri.path().to_owned(),
                ok: false,
                body_len: body.len(),
                payload: None,
            }),
        ),
        ServiceKind::Payments => (
            StatusCode::OK,
            Json(DemoResponse {
                service: "payments",
                path: uri.path().to_owned(),
                ok: true,
                body_len: body.len(),
                payload: None,
            }),
        ),
    }
}
