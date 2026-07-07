use std::{future::Future, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{
        HeaderMap, StatusCode,
        header::{AUTHORIZATION, CONTENT_TYPE},
    },
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracegate_core::AdminConfig;
use tracegate_storage::{ListFilters, RequestDetails, RequestSummary, now_ms};

use crate::{AdminState, GatewayState, ProxyError, validate_reload_immutables};

const REQUIRED_PROMETHEUS_SERIES: &[&str] = &[
    "tracegate_requests_total",
    "tracegate_request_duration_seconds",
    "tracegate_upstream_errors_total",
    "tracegate_captures_total",
    "tracegate_capture_dropped_total",
    "tracegate_storage_retention_runs_total",
    "tracegate_plugin_decisions_total",
    "tracegate_plugin_duration_seconds",
    "tracegate_plugin_timeouts_total",
    "tracegate_plugin_errors_total",
];

pub async fn serve<S>(
    listener: TcpListener,
    state: AdminState,
    shutdown: S,
) -> Result<(), std::io::Error>
where
    S: Future<Output = ()> + Send + 'static,
{
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
}

fn router(state: AdminState) -> Router {
    Router::new()
        .route("/console", get(console))
        .route("/console/", get(console))
        .route("/health/live", get(health_live))
        .route("/health/ready", get(health_ready))
        .route("/metrics", get(metrics))
        .route("/admin/reload", post(reload_gateway))
        .route("/admin/api/overview", get(api_overview))
        .route("/admin/api/requests", get(api_requests))
        .route("/admin/api/requests/{request_id}", get(api_request_details))
        .route("/admin/api/routes", get(api_routes))
        .route("/admin/api/plugins", get(api_plugins))
        .route("/admin/api/telemetry", get(api_telemetry))
        .with_state(state)
}

async fn console() -> Html<&'static str> {
    Html(CONSOLE_HTML)
}

async fn health_live(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !legacy_authorized(&headers, &state.admin_config().await, "/health/live") {
        return plain(StatusCode::UNAUTHORIZED, "unauthorized\n");
    }
    plain(StatusCode::OK, "live\n")
}

async fn health_ready(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !legacy_authorized(&headers, &state.admin_config().await, "/health/ready") {
        return plain(StatusCode::UNAUTHORIZED, "unauthorized\n");
    }
    match state.storage.health_check().await {
        Ok(()) => plain(StatusCode::OK, "ready\n"),
        Err(err) => plain(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("storage not ready: {err}\n"),
        ),
    }
}

async fn metrics(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !legacy_authorized(&headers, &state.admin_config().await, "/metrics") {
        return plain(StatusCode::UNAUTHORIZED, "unauthorized\n");
    }
    if !state.telemetry.prometheus_enabled() {
        return plain(StatusCode::NOT_FOUND, "metrics disabled\n");
    }
    match state.telemetry.render_prometheus() {
        Ok(metrics) => content(
            StatusCode::OK,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
            metrics,
        ),
        Err(err) => plain(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to render metrics: {err}\n"),
        ),
    }
}

async fn reload_gateway(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !legacy_authorized(&headers, &state.admin_config().await, "/admin/reload") {
        return plain(StatusCode::UNAUTHORIZED, "unauthorized\n");
    }
    let Some(path) = state.config_path.clone() else {
        return plain(StatusCode::BAD_REQUEST, "reload config path unavailable\n");
    };
    let new_config = match tracegate_config::load_config(&path) {
        Ok(config) => config,
        Err(err) => {
            return json(
                StatusCode::BAD_REQUEST,
                ReloadResponse::rejected(err.to_string()),
            );
        }
    };

    let mut current = state.current_config.write().await;
    if let Err(err) = validate_reload_immutables(&current, &new_config) {
        return json(StatusCode::BAD_REQUEST, ReloadResponse::rejected(err));
    }
    let new_state = match GatewayState::new(&new_config) {
        Ok(state) => state,
        Err(ProxyError::Policy(err)) => {
            return json(
                StatusCode::BAD_REQUEST,
                ReloadResponse::rejected(err.to_string()),
            );
        }
        Err(err) => {
            return json(
                StatusCode::BAD_REQUEST,
                ReloadResponse::rejected(err.to_string()),
            );
        }
    };
    let routes = new_config.routes.len();
    let plugins = new_config.plugins.len();
    state.gateway_state.store(std::sync::Arc::new(new_state));
    *current = new_config;
    json(
        StatusCode::OK,
        ReloadResponse {
            status: "reloaded",
            error: None,
            routes: Some(routes),
            plugins: Some(plugins),
        },
    )
}

async fn api_overview(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !api_authorized(&headers, &state.admin_config().await) {
        return json(StatusCode::UNAUTHORIZED, ApiError::new("unauthorized"));
    }
    let config = state.current_config.read().await.clone();
    let storage = readiness(&state).await;
    json(
        StatusCode::OK,
        OverviewResponse {
            mode: config.mode.to_string(),
            git_sha: std::env::var("TRACEGATE_GIT_SHA").unwrap_or_else(|_| "unknown".to_owned()),
            storage_ready: storage.ready,
            storage_error: storage.error,
            prometheus_enabled: state.telemetry.prometheus_enabled(),
            route_count: config.routes.len(),
            plugin_count: config.plugins.len(),
        },
    )
}

async fn api_requests(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Query(query): Query<RequestListQuery>,
) -> Response {
    if !api_authorized(&headers, &state.admin_config().await) {
        return json(StatusCode::UNAUTHORIZED, ApiError::new("unauthorized"));
    }
    let limit = query.limit.unwrap_or(50).clamp(1, 100);
    match state
        .storage
        .list_requests(ListFilters {
            failed: query.failed.unwrap_or(false),
            slow: query.slow.unwrap_or(false),
            route_id: query.route.filter(|value| !value.trim().is_empty()),
            since_created_at_ms: None,
            limit,
        })
        .await
    {
        Ok(requests) => json(StatusCode::OK, RequestListResponse { limit, requests }),
        Err(err) => json(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::new(format!("failed to list requests: {err}")),
        ),
    }
}

async fn api_request_details(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(request_id): Path<String>,
) -> Response {
    if !api_authorized(&headers, &state.admin_config().await) {
        return json(StatusCode::UNAUTHORIZED, ApiError::new("unauthorized"));
    }
    match state.storage.show_request(&request_id).await {
        Ok(Some(details)) => json(StatusCode::OK, RequestDetailsResponse { details }),
        Ok(None) => json(StatusCode::NOT_FOUND, ApiError::new("request not found")),
        Err(err) => json(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiError::new(format!("failed to show request: {err}")),
        ),
    }
}

async fn api_routes(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !api_authorized(&headers, &state.admin_config().await) {
        return json(StatusCode::UNAUTHORIZED, ApiError::new("unauthorized"));
    }
    let config = state.current_config.read().await.clone();
    let gateway_state = state.gateway_state.load_full();
    let now = now_ms();
    let routes = config
        .routes
        .iter()
        .map(|route| {
            let runtime = gateway_state.route_runtime.get(&route.id);
            let upstreams = route
                .upstreams
                .iter()
                .enumerate()
                .map(|(index, upstream)| {
                    let runtime =
                        runtime.and_then(|route_runtime| route_runtime.upstreams.get(index));
                    let failures = runtime
                        .map(|runtime| runtime.failures.load(std::sync::atomic::Ordering::Relaxed))
                        .unwrap_or(0);
                    let unhealthy_until_ms = runtime
                        .map(|runtime| {
                            runtime
                                .unhealthy_until_ms
                                .load(std::sync::atomic::Ordering::Relaxed)
                        })
                        .unwrap_or(0);
                    RouteUpstreamStatus {
                        index,
                        origin: upstream.origin(),
                        failures,
                        unhealthy: unhealthy_until_ms > now,
                        unhealthy_until_ms: if unhealthy_until_ms > now {
                            Some(unhealthy_until_ms)
                        } else {
                            None
                        },
                    }
                })
                .collect();
            RouteStatus {
                id: route.id.clone(),
                hosts: route.hosts.clone(),
                path_prefix: route.path_prefix.clone(),
                timeout_ms: duration_ms(route.timeout),
                retries: route.retries,
                concurrency_limit: route.concurrency_limit,
                capture_policy: route.capture.policy.to_string(),
                slow_threshold_ms: duration_ms(route.capture.slow_threshold),
                capture_request_body: route.capture.capture_request_body,
                capture_response_body_bytes: route.capture.capture_response_body_bytes,
                passive_health_failures: route.passive_health_failures,
                passive_health_cooldown_ms: duration_ms(route.passive_health_cooldown),
                upstreams,
            }
        })
        .collect();
    json(StatusCode::OK, RoutesResponse { routes })
}

async fn api_plugins(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !api_authorized(&headers, &state.admin_config().await) {
        return json(StatusCode::UNAUTHORIZED, ApiError::new("unauthorized"));
    }
    let config = state.current_config.read().await.clone();
    let plugins = config
        .plugins
        .iter()
        .map(|plugin| PluginSummary {
            id: plugin.id.clone(),
            hook: plugin.hook.to_string(),
            routes: plugin.routes.clone(),
            timeout_ms: duration_ms(plugin.timeout),
            memory_limit_bytes: plugin.memory_limit_bytes,
            fuel: plugin.fuel,
            body_preview_bytes: plugin.body_preview_bytes,
            raw_headers: plugin.raw_headers.clone(),
            config_keys: plugin
                .config
                .iter()
                .map(|value| value.key.clone())
                .collect(),
        })
        .collect();
    json(StatusCode::OK, PluginsResponse { plugins })
}

async fn api_telemetry(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    if !api_authorized(&headers, &state.admin_config().await) {
        return json(StatusCode::UNAUTHORIZED, ApiError::new("unauthorized"));
    }
    let storage = readiness(&state).await;
    let metrics = if state.telemetry.prometheus_enabled() {
        state.telemetry.render_prometheus().ok()
    } else {
        None
    };
    let series = REQUIRED_PROMETHEUS_SERIES
        .iter()
        .map(|name| SeriesStatus {
            name: (*name).to_owned(),
            present: metrics
                .as_ref()
                .map(|metrics| metrics.contains(name))
                .unwrap_or(false),
        })
        .collect();
    json(
        StatusCode::OK,
        TelemetryResponse {
            admin_ready: true,
            storage_ready: storage.ready,
            storage_error: storage.error,
            prometheus_enabled: state.telemetry.prometheus_enabled(),
            series,
        },
    )
}

async fn readiness(state: &AdminState) -> Readiness {
    match state.storage.health_check().await {
        Ok(()) => Readiness {
            ready: true,
            error: None,
        },
        Err(err) => Readiness {
            ready: false,
            error: Some(err.to_string()),
        },
    }
}

impl AdminState {
    async fn admin_config(&self) -> AdminConfig {
        self.current_config.read().await.admin.clone()
    }
}

fn legacy_authorized(headers: &HeaderMap, admin: &AdminConfig, path: &str) -> bool {
    match admin.token.as_deref() {
        Some(token) => bearer_matches(headers, token),
        None => path != "/admin/reload",
    }
}

fn api_authorized(headers: &HeaderMap, admin: &AdminConfig) -> bool {
    let Some(token) = admin.token.as_deref() else {
        return false;
    };
    bearer_matches(headers, token)
}

fn bearer_matches(headers: &HeaderMap, token: &str) -> bool {
    let Some(header) = headers.get(AUTHORIZATION) else {
        return false;
    };
    let Ok(value) = header.to_str() else {
        return false;
    };
    value == format!("Bearer {token}")
}

fn plain(status: StatusCode, body: &str) -> Response {
    content(status, "text/plain; charset=utf-8", body.to_owned())
}

fn content(status: StatusCode, content_type: &'static str, body: String) -> Response {
    let mut response = body.into_response();
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        content_type.parse().expect("valid content type"),
    );
    response
}

fn json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, Json(body)).into_response()
}

fn duration_ms(duration: Duration) -> u128 {
    duration.as_millis()
}

#[derive(Debug, Deserialize)]
struct RequestListQuery {
    failed: Option<bool>,
    slow: Option<bool>,
    route: Option<String>,
    limit: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
}

impl ApiError {
    fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ReloadResponse {
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    routes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    plugins: Option<usize>,
}

impl ReloadResponse {
    fn rejected(error: impl Into<String>) -> Self {
        Self {
            status: "rejected",
            error: Some(error.into()),
            routes: None,
            plugins: None,
        }
    }
}

#[derive(Debug, Serialize)]
struct OverviewResponse {
    mode: String,
    git_sha: String,
    storage_ready: bool,
    storage_error: Option<String>,
    prometheus_enabled: bool,
    route_count: usize,
    plugin_count: usize,
}

#[derive(Debug, Serialize)]
struct RequestListResponse {
    limit: u32,
    requests: Vec<RequestSummary>,
}

#[derive(Debug, Serialize)]
struct RequestDetailsResponse {
    details: RequestDetails,
}

#[derive(Debug, Serialize)]
struct RoutesResponse {
    routes: Vec<RouteStatus>,
}

#[derive(Debug, Serialize)]
struct RouteStatus {
    id: String,
    hosts: Vec<String>,
    path_prefix: String,
    timeout_ms: u128,
    retries: u32,
    concurrency_limit: usize,
    capture_policy: String,
    slow_threshold_ms: u128,
    capture_request_body: bool,
    capture_response_body_bytes: u64,
    passive_health_failures: u32,
    passive_health_cooldown_ms: u128,
    upstreams: Vec<RouteUpstreamStatus>,
}

#[derive(Debug, Serialize)]
struct RouteUpstreamStatus {
    index: usize,
    origin: String,
    failures: u32,
    unhealthy: bool,
    unhealthy_until_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
struct PluginsResponse {
    plugins: Vec<PluginSummary>,
}

#[derive(Debug, Serialize)]
struct PluginSummary {
    id: String,
    hook: String,
    routes: Vec<String>,
    timeout_ms: u128,
    memory_limit_bytes: u64,
    fuel: u64,
    body_preview_bytes: u64,
    raw_headers: Vec<String>,
    config_keys: Vec<String>,
}

#[derive(Debug, Serialize)]
struct TelemetryResponse {
    admin_ready: bool,
    storage_ready: bool,
    storage_error: Option<String>,
    prometheus_enabled: bool,
    series: Vec<SeriesStatus>,
}

#[derive(Debug, Serialize)]
struct SeriesStatus {
    name: String,
    present: bool,
}

struct Readiness {
    ready: bool,
    error: Option<String>,
}

const CONSOLE_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>TraceGate Console</title>
  <style>
    :root {
      color-scheme: light;
      --bg: #f7f8fa;
      --panel: #ffffff;
      --panel-2: #eef2f6;
      --ink: #17202a;
      --muted: #697483;
      --line: #d8dee6;
      --accent: #0f766e;
      --accent-2: #1f5eff;
      --bad: #b42318;
      --warn: #a15c00;
      --good: #137333;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      background: var(--bg);
      color: var(--ink);
      font: 14px/1.4 system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    }
    header {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 16px;
      min-height: 64px;
      padding: 12px 20px;
      border-bottom: 1px solid var(--line);
      background: var(--panel);
    }
    h1, h2, h3 { margin: 0; font-weight: 650; letter-spacing: 0; }
    h1 { font-size: 20px; }
    h2 { font-size: 15px; margin-bottom: 10px; }
    h3 { font-size: 13px; margin-bottom: 6px; color: var(--muted); }
    button, input, select {
      font: inherit;
      min-height: 34px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: #fff;
      color: var(--ink);
    }
    button {
      padding: 0 12px;
      cursor: pointer;
      background: var(--accent);
      border-color: var(--accent);
      color: #fff;
    }
    button.secondary {
      background: #fff;
      color: var(--ink);
      border-color: var(--line);
    }
    input, select { padding: 0 9px; }
    .auth {
      display: grid;
      grid-template-columns: minmax(190px, 320px) auto auto;
      gap: 8px;
      align-items: center;
    }
    main {
      display: grid;
      grid-template-columns: 320px 1fr 330px;
      gap: 14px;
      padding: 14px;
    }
    section {
      background: var(--panel);
      border: 1px solid var(--line);
      border-radius: 8px;
      min-width: 0;
      padding: 12px;
    }
    .stack { display: grid; gap: 14px; align-content: start; }
    .toolbar {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 8px;
      margin-bottom: 10px;
    }
    .list {
      display: grid;
      gap: 7px;
      max-height: calc(100vh - 220px);
      overflow: auto;
      padding-right: 3px;
    }
    .row {
      width: 100%;
      display: grid;
      grid-template-columns: 54px 1fr;
      gap: 8px;
      padding: 8px;
      background: #fff;
      color: var(--ink);
      border: 1px solid var(--line);
      border-radius: 6px;
      text-align: left;
    }
    .row.active { border-color: var(--accent-2); box-shadow: 0 0 0 1px var(--accent-2) inset; }
    .status { font-weight: 700; }
    .s2xx { color: var(--good); }
    .s4xx, .s5xx { color: var(--bad); }
    .meta, .muted { color: var(--muted); }
    .meta { font-size: 12px; overflow-wrap: anywhere; }
    .grid2 {
      display: grid;
      grid-template-columns: repeat(2, minmax(0, 1fr));
      gap: 10px;
    }
    .facts {
      display: grid;
      grid-template-columns: minmax(130px, 0.6fr) minmax(0, 1fr);
      gap: 6px 12px;
      margin: 0;
    }
    .facts dt { color: var(--muted); }
    .facts dd { margin: 0; overflow-wrap: anywhere; }
    .table {
      width: 100%;
      border-collapse: collapse;
      table-layout: fixed;
      font-size: 12px;
    }
    .table th, .table td {
      border-bottom: 1px solid var(--line);
      padding: 6px;
      text-align: left;
      vertical-align: top;
      overflow-wrap: anywhere;
    }
    .pill {
      display: inline-flex;
      align-items: center;
      min-height: 22px;
      padding: 0 7px;
      border-radius: 999px;
      background: var(--panel-2);
      color: var(--ink);
      font-size: 12px;
      font-weight: 650;
    }
    .pill.bad { background: #fdebea; color: var(--bad); }
    .pill.good { background: #e9f6ee; color: var(--good); }
    .pill.warn { background: #fff4df; color: var(--warn); }
    pre {
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      background: #101820;
      color: #e8eef5;
      border-radius: 6px;
      padding: 10px;
      max-height: 260px;
      overflow: auto;
    }
    .empty {
      display: grid;
      min-height: 110px;
      place-items: center;
      color: var(--muted);
      border: 1px dashed var(--line);
      border-radius: 6px;
    }
    @media (max-width: 1080px) {
      main { grid-template-columns: 1fr; }
      .list { max-height: 360px; }
      .auth { grid-template-columns: 1fr auto; }
      .auth button:last-child { grid-column: 2; }
    }
    @media (max-width: 560px) {
      header { align-items: stretch; flex-direction: column; }
      .auth { grid-template-columns: 1fr; }
      .toolbar, .grid2 { grid-template-columns: 1fr; }
    }
  </style>
</head>
<body>
  <header>
    <div>
      <h1>TraceGate Console</h1>
      <div id="overview" class="meta">Waiting for admin token.</div>
    </div>
    <div class="auth">
      <input id="token" type="password" autocomplete="off" placeholder="Admin bearer token">
      <button id="save">Apply</button>
      <button id="refresh" class="secondary">Refresh</button>
    </div>
  </header>
  <main>
    <section>
      <h2>Recent Requests</h2>
      <div class="toolbar">
        <select id="filter">
          <option value="failed">Failed</option>
          <option value="slow">Slow</option>
          <option value="all">All</option>
        </select>
        <select id="limit">
          <option>25</option>
          <option selected>50</option>
          <option>100</option>
        </select>
      </div>
      <div id="requests" class="list"></div>
    </section>
    <div class="stack">
      <section>
        <h2>Request Detail</h2>
        <div id="detail" class="empty">Select a request.</div>
      </section>
      <section>
        <h2>Replay Runs</h2>
        <div id="replay" class="empty">No request selected.</div>
      </section>
      <section>
        <h2>Plugin Decisions</h2>
        <div id="decisions" class="empty">No request selected.</div>
      </section>
    </div>
    <div class="stack">
      <section>
        <h2>Route Health</h2>
        <div id="routes" class="empty">Waiting for data.</div>
      </section>
      <section>
        <h2>Plugins</h2>
        <div id="plugins" class="empty">Waiting for data.</div>
      </section>
      <section>
        <h2>Telemetry</h2>
        <div id="telemetry" class="empty">Waiting for data.</div>
      </section>
    </div>
  </main>
  <script>
    const $ = (id) => document.getElementById(id);
    const state = { token: sessionStorage.getItem("tracegate.adminToken") || "", selected: null };
    $("token").value = state.token;

    $("save").addEventListener("click", () => {
      state.token = $("token").value.trim();
      sessionStorage.setItem("tracegate.adminToken", state.token);
      loadAll();
    });
    $("refresh").addEventListener("click", loadAll);
    $("filter").addEventListener("change", loadRequests);
    $("limit").addEventListener("change", loadRequests);

    function headers() {
      return state.token ? { "Authorization": `Bearer ${state.token}` } : {};
    }

    async function api(path) {
      const res = await fetch(path, { headers: headers() });
      const text = await res.text();
      let body;
      try { body = text ? JSON.parse(text) : {}; } catch (_) { body = { error: text }; }
      if (!res.ok) throw new Error(body.error || `${res.status} ${res.statusText}`);
      return body;
    }

    function statusClass(status) {
      if (status >= 500) return "s5xx";
      if (status >= 400) return "s4xx";
      if (status >= 200 && status < 300) return "s2xx";
      return "";
    }

    function setEmpty(id, text) {
      const node = $(id);
      node.className = "empty";
      node.textContent = text;
    }

    function facts(items) {
      const dl = document.createElement("dl");
      dl.className = "facts";
      for (const [key, value] of items) {
        const dt = document.createElement("dt");
        dt.textContent = key;
        const dd = document.createElement("dd");
        dd.textContent = value ?? "";
        dl.append(dt, dd);
      }
      return dl;
    }

    function table(columns, rows) {
      const el = document.createElement("table");
      el.className = "table";
      const thead = document.createElement("thead");
      const head = document.createElement("tr");
      for (const column of columns) {
        const th = document.createElement("th");
        th.textContent = column.label;
        head.appendChild(th);
      }
      thead.appendChild(head);
      const tbody = document.createElement("tbody");
      for (const row of rows) {
        const tr = document.createElement("tr");
        for (const column of columns) {
          const td = document.createElement("td");
          const value = column.value(row);
          if (value instanceof Node) td.appendChild(value);
          else td.textContent = value ?? "";
          tr.appendChild(td);
        }
        tbody.appendChild(tr);
      }
      el.append(thead, tbody);
      return el;
    }

    function pill(text, kind) {
      const el = document.createElement("span");
      el.className = `pill ${kind || ""}`.trim();
      el.textContent = text;
      return el;
    }

    async function loadOverview() {
      const data = await api("/admin/api/overview");
      $("overview").textContent = `${data.mode} | ${data.git_sha} | routes ${data.route_count} | plugins ${data.plugin_count} | storage ${data.storage_ready ? "ready" : "not ready"}`;
    }

    async function loadRequests() {
      const filter = $("filter").value;
      const params = new URLSearchParams({ limit: $("limit").value });
      if (filter === "failed") params.set("failed", "true");
      if (filter === "slow") params.set("slow", "true");
      const data = await api(`/admin/api/requests?${params.toString()}`);
      const list = $("requests");
      list.className = "list";
      list.replaceChildren();
      if (!data.requests.length) {
        setEmpty("requests", "No matching requests.");
        return;
      }
      for (const req of data.requests) {
        const row = document.createElement("button");
        row.className = `row ${req.request_id === state.selected ? "active" : ""}`.trim();
        row.addEventListener("click", () => {
          state.selected = req.request_id;
          loadDetail(req.request_id);
          loadRequests();
        });
        const status = document.createElement("div");
        status.className = `status ${statusClass(req.status)}`;
        status.textContent = req.status;
        const meta = document.createElement("div");
        const title = document.createElement("div");
        title.textContent = `${req.method} ${req.path}`;
        const detail = document.createElement("div");
        detail.className = "meta";
        detail.textContent = `${req.route_id || "no-route"} | ${req.latency_ms}ms | ${req.request_id}`;
        meta.append(title, detail);
        row.append(status, meta);
        list.appendChild(row);
      }
    }

    async function loadDetail(id) {
      const data = await api(`/admin/api/requests/${encodeURIComponent(id)}`);
      const detail = data.details;
      const req = detail.request;
      const root = $("detail");
      root.className = "";
      root.replaceChildren(facts([
        ["Request ID", req.request_id],
        ["Trace ID", req.trace_id || ""],
        ["Route", req.route_id || ""],
        ["Method", req.method],
        ["Path", req.redacted_query ? `${req.path}?${req.redacted_query}` : req.path],
        ["Status", req.status],
        ["Latency", `${req.latency_ms}ms`],
        ["Upstream", req.upstream || ""],
        ["Captured", detail.capture ? "yes" : "no"],
        ["Capture dropped", String(req.capture_dropped)]
      ]));
      if (detail.capture) {
        const pre = document.createElement("pre");
        pre.textContent = JSON.stringify(detail.capture, null, 2);
        root.appendChild(pre);
      }
      renderReplay(detail.replay_runs);
      renderDecisions(detail.plugin_decisions);
    }

    function renderReplay(rows) {
      const root = $("replay");
      root.className = "";
      root.replaceChildren();
      if (!rows.length) {
        setEmpty("replay", "No replay runs for this request.");
        return;
      }
      root.appendChild(table([
        { label: "Status", value: (row) => row.status ?? row.error ?? "" },
        { label: "Target", value: (row) => row.target },
        { label: "Replay ID", value: (row) => row.replay_id },
        { label: "Latency", value: (row) => `${row.latency_ms}ms` }
      ], rows));
    }

    function renderDecisions(rows) {
      const root = $("decisions");
      root.className = "";
      root.replaceChildren();
      if (!rows.length) {
        setEmpty("decisions", "No plugin decisions for this request.");
        return;
      }
      root.appendChild(table([
        { label: "Plugin", value: (row) => row.plugin_id },
        { label: "Action", value: (row) => row.timed_out ? pill("timeout", "bad") : row.action },
        { label: "Deny", value: (row) => row.deny_status ?? "" },
        { label: "Events", value: (row) => row.events.map((event) => event.code ? `${event.name}:${event.code}` : event.name).join(", ") }
      ], rows));
    }

    async function loadRoutes() {
      const data = await api("/admin/api/routes");
      const root = $("routes");
      root.className = "";
      root.replaceChildren();
      root.appendChild(table([
        { label: "Route", value: (row) => row.id },
        { label: "Path", value: (row) => row.path_prefix },
        { label: "Capture", value: (row) => row.capture_policy },
        { label: "Upstreams", value: (row) => row.upstreams.map((upstream) => `${upstream.unhealthy ? "down" : "up"} ${upstream.origin}`).join("\n") }
      ], data.routes));
    }

    async function loadPlugins() {
      const data = await api("/admin/api/plugins");
      const root = $("plugins");
      root.className = "";
      root.replaceChildren();
      root.appendChild(table([
        { label: "Plugin", value: (row) => row.id },
        { label: "Routes", value: (row) => row.routes.join(", ") },
        { label: "Limits", value: (row) => `${row.timeout_ms}ms | ${row.memory_limit_bytes} bytes` },
        { label: "Config", value: (row) => row.config_keys.join(", ") }
      ], data.plugins));
    }

    async function loadTelemetry() {
      const data = await api("/admin/api/telemetry");
      const root = $("telemetry");
      root.className = "";
      root.replaceChildren();
      root.appendChild(facts([
        ["Admin", data.admin_ready ? "ready" : "not ready"],
        ["Storage", data.storage_ready ? "ready" : data.storage_error],
        ["Prometheus", data.prometheus_enabled ? "enabled" : "disabled"]
      ]));
      root.appendChild(table([
        { label: "Series", value: (row) => row.name },
        { label: "Present", value: (row) => row.present ? pill("yes", "good") : pill("no", "warn") }
      ], data.series));
    }

    async function loadAll() {
      if (!state.token) {
        setEmpty("requests", "Enter the admin token.");
        setEmpty("detail", "Select a request.");
        setEmpty("replay", "No request selected.");
        setEmpty("decisions", "No request selected.");
        setEmpty("routes", "Enter the admin token.");
        setEmpty("plugins", "Enter the admin token.");
        setEmpty("telemetry", "Enter the admin token.");
        return;
      }
      try {
        await Promise.all([loadOverview(), loadRequests(), loadRoutes(), loadPlugins(), loadTelemetry()]);
      } catch (error) {
        $("overview").textContent = error.message;
      }
    }

    loadAll();
  </script>
</body>
</html>
"#;
