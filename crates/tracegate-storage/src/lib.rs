use std::{
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use sqlx::{
    PgPool, Postgres, QueryBuilder, Row, Sqlite, SqlitePool,
    postgres::PgPoolOptions,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use thiserror::Error;
use tracegate_core::StorageConfig;

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("invalid storage config: {0}")]
    InvalidConfig(String),
    #[error("failed to open SQLite database: {0}")]
    Connect(#[from] sqlx::Error),
    #[error("failed to run SQLite migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("filesystem error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone)]
pub struct Storage {
    backend: StorageBackend,
    config: StorageConfig,
}

#[derive(Clone)]
enum StorageBackend {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

#[derive(Clone, Debug)]
pub struct RequestInsert {
    pub request_id: String,
    pub trace_id: Option<String>,
    pub route_id: Option<String>,
    pub method: String,
    pub path: String,
    pub redacted_query: Option<String>,
    pub query_hash: Option<String>,
    pub status: u16,
    pub latency_ms: u128,
    pub upstream: Option<String>,
    pub is_error: bool,
    pub is_slow: bool,
    pub capture_policy: String,
    pub capture_dropped: bool,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct StoredHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct CaptureInsert {
    pub request_content_type: Option<String>,
    pub response_content_type: Option<String>,
    pub request_body: Option<Vec<u8>>,
    pub response_body: Option<Vec<u8>>,
    pub request_body_truncated: bool,
    pub response_body_truncated: bool,
    pub request_body_sha256: Option<String>,
    pub response_body_sha256: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ListFilters {
    pub failed: bool,
    pub slow: bool,
    pub route_id: Option<String>,
    pub since_created_at_ms: Option<i64>,
    pub limit: u32,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct RequestSummary {
    pub request_id: String,
    pub trace_id: Option<String>,
    pub route_id: Option<String>,
    pub method: String,
    pub path: String,
    pub redacted_query: Option<String>,
    pub query_hash: Option<String>,
    pub status: i64,
    pub latency_ms: i64,
    pub upstream: Option<String>,
    pub is_error: bool,
    pub is_slow: bool,
    pub capture_policy: String,
    pub capture_dropped: bool,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestDetails {
    pub request: RequestSummary,
    pub request_headers: Vec<StoredHeader>,
    pub response_headers: Vec<StoredHeader>,
    pub capture: Option<CaptureDetails>,
    pub plugin_decisions: Vec<PluginDecision>,
    pub replay_runs: Vec<ReplayRun>,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct CaptureDetails {
    pub request_content_type: Option<String>,
    pub response_content_type: Option<String>,
    pub request_body: Option<Vec<u8>>,
    pub response_body: Option<Vec<u8>>,
    pub request_body_truncated: bool,
    pub response_body_truncated: bool,
    pub request_body_sha256: Option<String>,
    pub response_body_sha256: Option<String>,
    pub body_evicted: bool,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug)]
pub struct ReplayRunInsert {
    pub replay_id: String,
    pub original_request_id: String,
    pub replay_request_id: String,
    pub target: String,
    pub method: String,
    pub path: String,
    pub status: Option<u16>,
    pub latency_ms: u128,
    pub error: Option<String>,
    pub diff_summary: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct ReplayRun {
    pub replay_id: String,
    pub original_request_id: String,
    pub replay_request_id: String,
    pub target: String,
    pub method: String,
    pub path: String,
    pub status: Option<i64>,
    pub latency_ms: i64,
    pub error: Option<String>,
    pub diff_summary: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct PluginEvent {
    pub name: String,
    pub code: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PluginDecisionInsert {
    pub request_id: String,
    pub plugin_id: String,
    pub route_id: String,
    pub action: String,
    pub deny_status: Option<u16>,
    pub set_headers: Vec<String>,
    pub remove_headers: Vec<String>,
    pub events: Vec<PluginEvent>,
    pub duration_ms: u128,
    pub timed_out: bool,
    pub error: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize, sqlx::FromRow)]
pub struct PluginDecisionRow {
    pub plugin_id: String,
    pub route_id: String,
    pub action: String,
    pub deny_status: Option<i64>,
    pub set_headers_json: String,
    pub remove_headers_json: String,
    pub events_json: String,
    pub duration_ms: i64,
    pub timed_out: bool,
    pub error: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginDecision {
    pub plugin_id: String,
    pub route_id: String,
    pub action: String,
    pub deny_status: Option<i64>,
    pub set_headers: Vec<String>,
    pub remove_headers: Vec<String>,
    pub events: Vec<PluginEvent>,
    pub duration_ms: i64,
    pub timed_out: bool,
    pub error: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct RetentionOutcome {
    pub deleted_requests: u64,
    pub evicted_captures: u64,
}

impl Storage {
    pub async fn connect(config: &StorageConfig) -> Result<Self, StorageError> {
        let backend = match config.driver.as_str() {
            "sqlite" => {
                create_sqlite_parent_dir(&config.url)?;

                let options = SqliteConnectOptions::from_str(&config.url)
                    .map_err(|err| StorageError::InvalidConfig(err.to_string()))?
                    .create_if_missing(true)
                    .journal_mode(SqliteJournalMode::Wal)
                    .foreign_keys(true)
                    .busy_timeout(Duration::from_secs(5));
                let pool = SqlitePoolOptions::new()
                    .max_connections(5)
                    .connect_with(options)
                    .await?;

                sqlx::query("PRAGMA journal_mode = WAL")
                    .execute(&pool)
                    .await?;
                sqlx::query("PRAGMA foreign_keys = ON")
                    .execute(&pool)
                    .await?;
                StorageBackend::Sqlite(pool)
            }
            "postgres" => {
                let pool = PgPoolOptions::new()
                    .max_connections(5)
                    .connect(&config.url)
                    .await?;
                StorageBackend::Postgres(pool)
            }
            driver => {
                return Err(StorageError::InvalidConfig(format!(
                    "unsupported storage driver `{driver}`"
                )));
            }
        };

        Ok(Self {
            backend,
            config: config.clone(),
        })
    }

    pub async fn migrate(&self) -> Result<(), StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => {
                sqlx::migrate!("./sqlite_migrations").run(pool).await?;
            }
            StorageBackend::Postgres(pool) => {
                sqlx::migrate!("./postgres_migrations").run(pool).await?;
            }
        }
        Ok(())
    }

    pub async fn health_check(&self) -> Result<(), StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => {
                sqlx::query("SELECT 1").execute(pool).await?;
            }
            StorageBackend::Postgres(pool) => {
                sqlx::query("SELECT 1").execute(pool).await?;
            }
        }
        Ok(())
    }

    pub async fn insert_request(
        &self,
        record: RequestInsert,
        request_headers: Vec<StoredHeader>,
        response_headers: Vec<StoredHeader>,
        capture: Option<CaptureInsert>,
        plugin_decisions: Vec<PluginDecisionInsert>,
    ) -> Result<(), StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => {
                insert_request_sqlite(
                    pool,
                    record,
                    request_headers,
                    response_headers,
                    capture,
                    plugin_decisions,
                )
                .await
            }
            StorageBackend::Postgres(pool) => {
                insert_request_postgres(
                    pool,
                    record,
                    request_headers,
                    response_headers,
                    capture,
                    plugin_decisions,
                )
                .await
            }
        }
    }

    pub async fn list_requests(
        &self,
        filters: ListFilters,
    ) -> Result<Vec<RequestSummary>, StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => list_requests_sqlite(pool, filters).await,
            StorageBackend::Postgres(pool) => list_requests_postgres(pool, filters).await,
        }
    }

    pub async fn show_request(
        &self,
        request_id: &str,
    ) -> Result<Option<RequestDetails>, StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => show_request_sqlite(pool, request_id).await,
            StorageBackend::Postgres(pool) => show_request_postgres(pool, request_id).await,
        }
    }

    pub async fn list_plugin_decisions(
        &self,
        request_id: &str,
    ) -> Result<Vec<PluginDecision>, StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => list_plugin_decisions_sqlite(pool, request_id).await,
            StorageBackend::Postgres(pool) => {
                list_plugin_decisions_postgres(pool, request_id).await
            }
        }
    }

    pub async fn insert_replay_run(&self, run: ReplayRunInsert) -> Result<(), StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => insert_replay_run_sqlite(pool, run).await,
            StorageBackend::Postgres(pool) => insert_replay_run_postgres(pool, run).await,
        }
    }

    pub async fn list_replay_runs(
        &self,
        original_request_id: &str,
    ) -> Result<Vec<ReplayRun>, StorageError> {
        match &self.backend {
            StorageBackend::Sqlite(pool) => {
                list_replay_runs_sqlite(pool, original_request_id).await
            }
            StorageBackend::Postgres(pool) => {
                list_replay_runs_postgres(pool, original_request_id).await
            }
        }
    }

    pub async fn run_retention(&self) -> Result<RetentionOutcome, StorageError> {
        let cutoff = now_ms()
            - i64::from(self.config.retention_days)
                .saturating_mul(24)
                .saturating_mul(60)
                .saturating_mul(60)
                .saturating_mul(1000);
        match &self.backend {
            StorageBackend::Sqlite(pool) => {
                run_retention_sqlite(pool, cutoff, self.config.max_total_capture_bytes).await
            }
            StorageBackend::Postgres(pool) => {
                run_retention_postgres(pool, cutoff, self.config.max_total_capture_bytes).await
            }
        }
    }

    pub async fn backup_to(&self, output: &Path) -> Result<(), StorageError> {
        let StorageBackend::Sqlite(pool) = &self.backend else {
            return Err(StorageError::InvalidConfig(
                "PostgreSQL backups must be created with pg_dump from the deployment scripts"
                    .to_owned(),
            ));
        };

        if let Some(parent) = output.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }

        if output.exists() {
            std::fs::remove_file(output)?;
        }

        let output = output.display().to_string();
        sqlx::query("VACUUM INTO ?")
            .bind(output)
            .execute(pool)
            .await?;
        Ok(())
    }
}

async fn insert_request_sqlite(
    pool: &SqlitePool,
    record: RequestInsert,
    request_headers: Vec<StoredHeader>,
    response_headers: Vec<StoredHeader>,
    capture: Option<CaptureInsert>,
    plugin_decisions: Vec<PluginDecisionInsert>,
) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM requests WHERE request_id = ?")
        .bind(&record.request_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO requests (
            request_id, trace_id, route_id, method, path, redacted_query, query_hash,
            status, latency_ms, upstream, is_error, is_slow, capture_policy,
            capture_dropped, created_at_ms
        )
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&record.request_id)
    .bind(&record.trace_id)
    .bind(&record.route_id)
    .bind(&record.method)
    .bind(&record.path)
    .bind(&record.redacted_query)
    .bind(&record.query_hash)
    .bind(i64::from(record.status))
    .bind(record.latency_ms.min(i64::MAX as u128) as i64)
    .bind(&record.upstream)
    .bind(record.is_error)
    .bind(record.is_slow)
    .bind(&record.capture_policy)
    .bind(record.capture_dropped)
    .bind(record.created_at_ms)
    .execute(&mut *tx)
    .await?;

    insert_request_children_sqlite(
        &mut tx,
        &record,
        request_headers,
        response_headers,
        capture,
        plugin_decisions,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn insert_request_children_sqlite(
    tx: &mut sqlx::Transaction<'_, Sqlite>,
    record: &RequestInsert,
    request_headers: Vec<StoredHeader>,
    response_headers: Vec<StoredHeader>,
    capture: Option<CaptureInsert>,
    plugin_decisions: Vec<PluginDecisionInsert>,
) -> Result<(), StorageError> {
    for header in request_headers {
        sqlx::query("INSERT INTO request_headers (request_id, name, value) VALUES (?, ?, ?)")
            .bind(&record.request_id)
            .bind(header.name)
            .bind(header.value)
            .execute(&mut **tx)
            .await?;
    }
    for header in response_headers {
        sqlx::query("INSERT INTO response_headers (request_id, name, value) VALUES (?, ?, ?)")
            .bind(&record.request_id)
            .bind(header.name)
            .bind(header.value)
            .execute(&mut **tx)
            .await?;
    }
    if let Some(capture) = capture {
        sqlx::query(
            r#"
            INSERT INTO captures (
                request_id, request_content_type, response_content_type,
                request_body, response_body, request_body_truncated,
                response_body_truncated, request_body_sha256, response_body_sha256,
                body_evicted, created_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?)
            "#,
        )
        .bind(&record.request_id)
        .bind(capture.request_content_type)
        .bind(capture.response_content_type)
        .bind(capture.request_body)
        .bind(capture.response_body)
        .bind(capture.request_body_truncated)
        .bind(capture.response_body_truncated)
        .bind(capture.request_body_sha256)
        .bind(capture.response_body_sha256)
        .bind(record.created_at_ms)
        .execute(&mut **tx)
        .await?;
    }
    for decision in plugin_decisions {
        sqlx::query(
            r#"
            INSERT INTO plugin_decisions (
                request_id, plugin_id, route_id, action, deny_status,
                set_headers_json, remove_headers_json, events_json,
                duration_ms, timed_out, error, created_at_ms
            )
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(decision.request_id)
        .bind(decision.plugin_id)
        .bind(decision.route_id)
        .bind(decision.action)
        .bind(decision.deny_status.map(i64::from))
        .bind(json_or_empty_array(&decision.set_headers))
        .bind(json_or_empty_array(&decision.remove_headers))
        .bind(json_or_empty_array(&decision.events))
        .bind(decision.duration_ms.min(i64::MAX as u128) as i64)
        .bind(decision.timed_out)
        .bind(decision.error)
        .bind(decision.created_at_ms)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn insert_request_postgres(
    pool: &PgPool,
    record: RequestInsert,
    request_headers: Vec<StoredHeader>,
    response_headers: Vec<StoredHeader>,
    capture: Option<CaptureInsert>,
    plugin_decisions: Vec<PluginDecisionInsert>,
) -> Result<(), StorageError> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM requests WHERE request_id = $1")
        .bind(&record.request_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        r#"
        INSERT INTO requests (
            request_id, trace_id, route_id, method, path, redacted_query, query_hash,
            status, latency_ms, upstream, is_error, is_slow, capture_policy,
            capture_dropped, created_at_ms
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
        "#,
    )
    .bind(&record.request_id)
    .bind(&record.trace_id)
    .bind(&record.route_id)
    .bind(&record.method)
    .bind(&record.path)
    .bind(&record.redacted_query)
    .bind(&record.query_hash)
    .bind(i64::from(record.status))
    .bind(record.latency_ms.min(i64::MAX as u128) as i64)
    .bind(&record.upstream)
    .bind(record.is_error)
    .bind(record.is_slow)
    .bind(&record.capture_policy)
    .bind(record.capture_dropped)
    .bind(record.created_at_ms)
    .execute(&mut *tx)
    .await?;

    for header in request_headers {
        sqlx::query("INSERT INTO request_headers (request_id, name, value) VALUES ($1, $2, $3)")
            .bind(&record.request_id)
            .bind(header.name)
            .bind(header.value)
            .execute(&mut *tx)
            .await?;
    }
    for header in response_headers {
        sqlx::query("INSERT INTO response_headers (request_id, name, value) VALUES ($1, $2, $3)")
            .bind(&record.request_id)
            .bind(header.name)
            .bind(header.value)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(capture) = capture {
        sqlx::query(
            r#"
            INSERT INTO captures (
                request_id, request_content_type, response_content_type,
                request_body, response_body, request_body_truncated,
                response_body_truncated, request_body_sha256, response_body_sha256,
                body_evicted, created_at_ms
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, false, $10)
            "#,
        )
        .bind(&record.request_id)
        .bind(capture.request_content_type)
        .bind(capture.response_content_type)
        .bind(capture.request_body)
        .bind(capture.response_body)
        .bind(capture.request_body_truncated)
        .bind(capture.response_body_truncated)
        .bind(capture.request_body_sha256)
        .bind(capture.response_body_sha256)
        .bind(record.created_at_ms)
        .execute(&mut *tx)
        .await?;
    }
    for decision in plugin_decisions {
        sqlx::query(
            r#"
            INSERT INTO plugin_decisions (
                request_id, plugin_id, route_id, action, deny_status,
                set_headers_json, remove_headers_json, events_json,
                duration_ms, timed_out, error, created_at_ms
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            "#,
        )
        .bind(decision.request_id)
        .bind(decision.plugin_id)
        .bind(decision.route_id)
        .bind(decision.action)
        .bind(decision.deny_status.map(i64::from))
        .bind(json_or_empty_array(&decision.set_headers))
        .bind(json_or_empty_array(&decision.remove_headers))
        .bind(json_or_empty_array(&decision.events))
        .bind(decision.duration_ms.min(i64::MAX as u128) as i64)
        .bind(decision.timed_out)
        .bind(decision.error)
        .bind(decision.created_at_ms)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn list_requests_sqlite(
    pool: &SqlitePool,
    filters: ListFilters,
) -> Result<Vec<RequestSummary>, StorageError> {
    let mut query = request_list_query::<Sqlite>(filters);
    Ok(query
        .build_query_as::<RequestSummary>()
        .fetch_all(pool)
        .await?)
}

async fn list_requests_postgres(
    pool: &PgPool,
    filters: ListFilters,
) -> Result<Vec<RequestSummary>, StorageError> {
    let mut query = request_list_query::<Postgres>(filters);
    Ok(query
        .build_query_as::<RequestSummary>()
        .fetch_all(pool)
        .await?)
}

fn request_list_query<DB: sqlx::Database>(filters: ListFilters) -> QueryBuilder<'static, DB>
where
    i64: sqlx::Encode<'static, DB> + sqlx::Type<DB>,
    String: sqlx::Encode<'static, DB> + sqlx::Type<DB>,
    bool: sqlx::Encode<'static, DB> + sqlx::Type<DB>,
{
    let mut query = QueryBuilder::<DB>::new(
        r#"
        SELECT request_id, trace_id, route_id, method, path, redacted_query,
               query_hash, status, latency_ms, upstream, is_error, is_slow,
               capture_policy, capture_dropped, created_at_ms
        FROM requests
        WHERE 1 = 1
        "#,
    );
    if filters.failed {
        query.push(" AND is_error = ");
        query.push_bind(true);
    }
    if filters.slow {
        query.push(" AND is_slow = ");
        query.push_bind(true);
    }
    if let Some(route_id) = filters.route_id {
        query.push(" AND route_id = ");
        query.push_bind(route_id);
    }
    if let Some(cutoff) = filters.since_created_at_ms {
        query.push(" AND created_at_ms >= ");
        query.push_bind(cutoff);
    }
    query.push(" ORDER BY created_at_ms DESC LIMIT ");
    query.push_bind(i64::from(filters.limit.clamp(1, 1000)));
    query
}

async fn show_request_sqlite(
    pool: &SqlitePool,
    request_id: &str,
) -> Result<Option<RequestDetails>, StorageError> {
    let request = sqlx::query_as::<_, RequestSummary>(REQUEST_SELECT_SQLITE)
        .bind(request_id)
        .fetch_optional(pool)
        .await?;
    assemble_request_details_sqlite(pool, request_id, request).await
}

async fn show_request_postgres(
    pool: &PgPool,
    request_id: &str,
) -> Result<Option<RequestDetails>, StorageError> {
    let request = sqlx::query_as::<_, RequestSummary>(REQUEST_SELECT_POSTGRES)
        .bind(request_id)
        .fetch_optional(pool)
        .await?;
    assemble_request_details_postgres(pool, request_id, request).await
}

async fn assemble_request_details_sqlite(
    pool: &SqlitePool,
    request_id: &str,
    request: Option<RequestSummary>,
) -> Result<Option<RequestDetails>, StorageError> {
    let Some(request) = request else {
        return Ok(None);
    };
    let request_headers = sqlx::query_as::<_, StoredHeader>(
        "SELECT name, value FROM request_headers WHERE request_id = ? ORDER BY name, value",
    )
    .bind(request_id)
    .fetch_all(pool)
    .await?;
    let response_headers = sqlx::query_as::<_, StoredHeader>(
        "SELECT name, value FROM response_headers WHERE request_id = ? ORDER BY name, value",
    )
    .bind(request_id)
    .fetch_all(pool)
    .await?;
    let capture = sqlx::query_as::<_, CaptureDetails>(CAPTURE_SELECT_SQLITE)
        .bind(request_id)
        .fetch_optional(pool)
        .await?;
    let replay_runs = list_replay_runs_sqlite(pool, request_id).await?;
    let plugin_decisions = list_plugin_decisions_sqlite(pool, request_id).await?;
    Ok(Some(RequestDetails {
        request,
        request_headers,
        response_headers,
        capture,
        plugin_decisions,
        replay_runs,
    }))
}

async fn assemble_request_details_postgres(
    pool: &PgPool,
    request_id: &str,
    request: Option<RequestSummary>,
) -> Result<Option<RequestDetails>, StorageError> {
    let Some(request) = request else {
        return Ok(None);
    };
    let request_headers = sqlx::query_as::<_, StoredHeader>(
        "SELECT name, value FROM request_headers WHERE request_id = $1 ORDER BY name, value",
    )
    .bind(request_id)
    .fetch_all(pool)
    .await?;
    let response_headers = sqlx::query_as::<_, StoredHeader>(
        "SELECT name, value FROM response_headers WHERE request_id = $1 ORDER BY name, value",
    )
    .bind(request_id)
    .fetch_all(pool)
    .await?;
    let capture = sqlx::query_as::<_, CaptureDetails>(CAPTURE_SELECT_POSTGRES)
        .bind(request_id)
        .fetch_optional(pool)
        .await?;
    let replay_runs = list_replay_runs_postgres(pool, request_id).await?;
    let plugin_decisions = list_plugin_decisions_postgres(pool, request_id).await?;
    Ok(Some(RequestDetails {
        request,
        request_headers,
        response_headers,
        capture,
        plugin_decisions,
        replay_runs,
    }))
}

async fn list_plugin_decisions_sqlite(
    pool: &SqlitePool,
    request_id: &str,
) -> Result<Vec<PluginDecision>, StorageError> {
    let rows = sqlx::query_as::<_, PluginDecisionRow>(PLUGIN_DECISIONS_SELECT_SQLITE)
        .bind(request_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(PluginDecision::from).collect())
}

async fn list_plugin_decisions_postgres(
    pool: &PgPool,
    request_id: &str,
) -> Result<Vec<PluginDecision>, StorageError> {
    let rows = sqlx::query_as::<_, PluginDecisionRow>(PLUGIN_DECISIONS_SELECT_POSTGRES)
        .bind(request_id)
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(PluginDecision::from).collect())
}

async fn insert_replay_run_sqlite(
    pool: &SqlitePool,
    run: ReplayRunInsert,
) -> Result<(), StorageError> {
    sqlx::query(REPLAY_INSERT_SQLITE)
        .bind(run.replay_id)
        .bind(run.original_request_id)
        .bind(run.replay_request_id)
        .bind(run.target)
        .bind(run.method)
        .bind(run.path)
        .bind(run.status.map(i64::from))
        .bind(run.latency_ms.min(i64::MAX as u128) as i64)
        .bind(run.error)
        .bind(run.diff_summary)
        .bind(run.created_at_ms)
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_replay_run_postgres(
    pool: &PgPool,
    run: ReplayRunInsert,
) -> Result<(), StorageError> {
    sqlx::query(REPLAY_INSERT_POSTGRES)
        .bind(run.replay_id)
        .bind(run.original_request_id)
        .bind(run.replay_request_id)
        .bind(run.target)
        .bind(run.method)
        .bind(run.path)
        .bind(run.status.map(i64::from))
        .bind(run.latency_ms.min(i64::MAX as u128) as i64)
        .bind(run.error)
        .bind(run.diff_summary)
        .bind(run.created_at_ms)
        .execute(pool)
        .await?;
    Ok(())
}

async fn list_replay_runs_sqlite(
    pool: &SqlitePool,
    original_request_id: &str,
) -> Result<Vec<ReplayRun>, StorageError> {
    Ok(sqlx::query_as::<_, ReplayRun>(REPLAY_LIST_SQLITE)
        .bind(original_request_id)
        .fetch_all(pool)
        .await?)
}

async fn list_replay_runs_postgres(
    pool: &PgPool,
    original_request_id: &str,
) -> Result<Vec<ReplayRun>, StorageError> {
    Ok(sqlx::query_as::<_, ReplayRun>(REPLAY_LIST_POSTGRES)
        .bind(original_request_id)
        .fetch_all(pool)
        .await?)
}

async fn run_retention_sqlite(
    pool: &SqlitePool,
    cutoff: i64,
    max_total_capture_bytes: u64,
) -> Result<RetentionOutcome, StorageError> {
    let deleted_requests = sqlx::query("DELETE FROM requests WHERE created_at_ms < ?")
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    let mut total = total_capture_bytes_sqlite(pool).await?;
    evict_capture_bodies_sqlite(pool, &mut total, max_total_capture_bytes)
        .await
        .map(|evicted_captures| RetentionOutcome {
            deleted_requests,
            evicted_captures,
        })
}

async fn run_retention_postgres(
    pool: &PgPool,
    cutoff: i64,
    max_total_capture_bytes: u64,
) -> Result<RetentionOutcome, StorageError> {
    let deleted_requests = sqlx::query("DELETE FROM requests WHERE created_at_ms < $1")
        .bind(cutoff)
        .execute(pool)
        .await?
        .rows_affected();
    let mut total = total_capture_bytes_postgres(pool).await?;
    evict_capture_bodies_postgres(pool, &mut total, max_total_capture_bytes)
        .await
        .map(|evicted_captures| RetentionOutcome {
            deleted_requests,
            evicted_captures,
        })
}

async fn total_capture_bytes_sqlite(pool: &SqlitePool) -> Result<i64, StorageError> {
    Ok(sqlx::query_scalar::<_, i64>(TOTAL_CAPTURE_BYTES)
        .fetch_one(pool)
        .await?)
}

async fn total_capture_bytes_postgres(pool: &PgPool) -> Result<i64, StorageError> {
    Ok(sqlx::query_scalar::<_, i64>(TOTAL_CAPTURE_BYTES)
        .fetch_one(pool)
        .await?)
}

async fn evict_capture_bodies_sqlite(
    pool: &SqlitePool,
    total: &mut i64,
    max_total_capture_bytes: u64,
) -> Result<u64, StorageError> {
    let max_total = max_total_capture_bytes.min(i64::MAX as u64) as i64;
    let mut evicted_captures = 0;
    while *total > max_total {
        let candidates = sqlx::query(EVICTION_CANDIDATES_SQLITE)
            .fetch_all(pool)
            .await?;
        if candidates.is_empty() {
            break;
        }
        for candidate in candidates {
            let request_id: String = candidate.get("request_id");
            let bytes: i64 = candidate.get("bytes");
            sqlx::query(
                "UPDATE captures SET request_body = NULL, response_body = NULL, body_evicted = 1 WHERE request_id = ?",
            )
            .bind(request_id)
            .execute(pool)
            .await?;
            evicted_captures += 1;
            *total = total.saturating_sub(bytes.max(0));
            if *total <= max_total {
                break;
            }
        }
    }
    Ok(evicted_captures)
}

async fn evict_capture_bodies_postgres(
    pool: &PgPool,
    total: &mut i64,
    max_total_capture_bytes: u64,
) -> Result<u64, StorageError> {
    let max_total = max_total_capture_bytes.min(i64::MAX as u64) as i64;
    let mut evicted_captures = 0;
    while *total > max_total {
        let candidates = sqlx::query(EVICTION_CANDIDATES_POSTGRES)
            .fetch_all(pool)
            .await?;
        if candidates.is_empty() {
            break;
        }
        for candidate in candidates {
            let request_id: String = candidate.get("request_id");
            let bytes: i64 = candidate.get("bytes");
            sqlx::query(
                "UPDATE captures SET request_body = NULL, response_body = NULL, body_evicted = true WHERE request_id = $1",
            )
            .bind(request_id)
            .execute(pool)
            .await?;
            evicted_captures += 1;
            *total = total.saturating_sub(bytes.max(0));
            if *total <= max_total {
                break;
            }
        }
    }
    Ok(evicted_captures)
}

const REQUEST_SELECT_SQLITE: &str = r#"
    SELECT request_id, trace_id, route_id, method, path, redacted_query,
           query_hash, status, latency_ms, upstream, is_error, is_slow,
           capture_policy, capture_dropped, created_at_ms
    FROM requests
    WHERE request_id = ?
"#;

const REQUEST_SELECT_POSTGRES: &str = r#"
    SELECT request_id, trace_id, route_id, method, path, redacted_query,
           query_hash, status, latency_ms, upstream, is_error, is_slow,
           capture_policy, capture_dropped, created_at_ms
    FROM requests
    WHERE request_id = $1
"#;

const CAPTURE_SELECT_SQLITE: &str = r#"
    SELECT request_content_type, response_content_type, request_body, response_body,
           request_body_truncated, response_body_truncated, request_body_sha256,
           response_body_sha256, body_evicted, created_at_ms
    FROM captures
    WHERE request_id = ?
"#;

const CAPTURE_SELECT_POSTGRES: &str = r#"
    SELECT request_content_type, response_content_type, request_body, response_body,
           request_body_truncated, response_body_truncated, request_body_sha256,
           response_body_sha256, body_evicted, created_at_ms
    FROM captures
    WHERE request_id = $1
"#;

const PLUGIN_DECISIONS_SELECT_SQLITE: &str = r#"
    SELECT plugin_id, route_id, action, deny_status, set_headers_json,
           remove_headers_json, events_json, duration_ms, timed_out, error,
           created_at_ms
    FROM plugin_decisions
    WHERE request_id = ?
    ORDER BY id
"#;

const PLUGIN_DECISIONS_SELECT_POSTGRES: &str = r#"
    SELECT plugin_id, route_id, action, deny_status, set_headers_json,
           remove_headers_json, events_json, duration_ms, timed_out, error,
           created_at_ms
    FROM plugin_decisions
    WHERE request_id = $1
    ORDER BY id
"#;

const REPLAY_INSERT_SQLITE: &str = r#"
    INSERT INTO replay_runs (
        replay_id, original_request_id, replay_request_id, target, method, path,
        status, latency_ms, error, diff_summary, created_at_ms
    )
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#;

const REPLAY_INSERT_POSTGRES: &str = r#"
    INSERT INTO replay_runs (
        replay_id, original_request_id, replay_request_id, target, method, path,
        status, latency_ms, error, diff_summary, created_at_ms
    )
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
"#;

const REPLAY_LIST_SQLITE: &str = r#"
    SELECT replay_id, original_request_id, replay_request_id, target, method, path,
           status, latency_ms, error, diff_summary, created_at_ms
    FROM replay_runs
    WHERE original_request_id = ?
    ORDER BY created_at_ms DESC
"#;

const REPLAY_LIST_POSTGRES: &str = r#"
    SELECT replay_id, original_request_id, replay_request_id, target, method, path,
           status, latency_ms, error, diff_summary, created_at_ms
    FROM replay_runs
    WHERE original_request_id = $1
    ORDER BY created_at_ms DESC
"#;

const TOTAL_CAPTURE_BYTES: &str = r#"
    SELECT COALESCE(
        SUM(COALESCE(length(request_body), 0) + COALESCE(length(response_body), 0)),
        0
    )
    FROM captures
"#;

const EVICTION_CANDIDATES_SQLITE: &str = r#"
    SELECT request_id,
           COALESCE(length(request_body), 0) + COALESCE(length(response_body), 0) AS bytes
    FROM captures
    WHERE body_evicted = 0
      AND (request_body IS NOT NULL OR response_body IS NOT NULL)
    ORDER BY created_at_ms ASC
    LIMIT 100
"#;

const EVICTION_CANDIDATES_POSTGRES: &str = r#"
    SELECT request_id,
           COALESCE(length(request_body), 0) + COALESCE(length(response_body), 0) AS bytes
    FROM captures
    WHERE body_evicted = false
      AND (request_body IS NOT NULL OR response_body IS NOT NULL)
    ORDER BY created_at_ms ASC
    LIMIT 100
"#;

impl From<PluginDecisionRow> for PluginDecision {
    fn from(row: PluginDecisionRow) -> Self {
        Self {
            plugin_id: row.plugin_id,
            route_id: row.route_id,
            action: row.action,
            deny_status: row.deny_status,
            set_headers: serde_json::from_str(&row.set_headers_json).unwrap_or_default(),
            remove_headers: serde_json::from_str(&row.remove_headers_json).unwrap_or_default(),
            events: serde_json::from_str(&row.events_json).unwrap_or_default(),
            duration_ms: row.duration_ms,
            timed_out: row.timed_out,
            error: row.error,
            created_at_ms: row.created_at_ms,
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn json_or_empty_array<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "[]".to_owned())
}

fn create_sqlite_parent_dir(url: &str) -> Result<(), StorageError> {
    let Some(path) = sqlite_file_path(url) else {
        return Ok(());
    };

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    Ok(())
}

fn sqlite_file_path(url: &str) -> Option<PathBuf> {
    if url == "sqlite::memory:" || url == "sqlite://:memory:" {
        return None;
    }

    let path = url.strip_prefix("sqlite://")?;
    if path.is_empty() || path == ":memory:" {
        return None;
    }

    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sqlite_url(path: &Path) -> String {
        let path = path.display().to_string().replace('\\', "/");
        if path.starts_with('/') {
            format!("sqlite://{path}")
        } else {
            format!("sqlite:///{path}")
        }
    }

    async fn storage(max_total_capture_bytes: u64) -> (Storage, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            url: sqlite_url(&dir.path().join("tracegate.db")),
            max_total_capture_bytes,
            max_capture_bytes_per_request: 2048,
            ..StorageConfig::default()
        };
        let storage = Storage::connect(&config).await.unwrap();
        storage.migrate().await.unwrap();
        (storage, dir)
    }

    fn insert(request_id: &str, created_at_ms: i64) -> RequestInsert {
        RequestInsert {
            request_id: request_id.to_owned(),
            trace_id: Some("trace".to_owned()),
            route_id: Some("payments".to_owned()),
            method: "POST".to_owned(),
            path: "/api/payments".to_owned(),
            redacted_query: Some("mode=test".to_owned()),
            query_hash: Some("hash".to_owned()),
            status: 500,
            latency_ms: 42,
            upstream: Some("http://payments-service:4000".to_owned()),
            is_error: true,
            is_slow: false,
            capture_policy: "errors_and_slow".to_owned(),
            capture_dropped: false,
            created_at_ms,
        }
    }

    #[tokio::test]
    async fn migrates_inserts_lists_and_shows_requests() {
        let (storage, _dir) = storage(4096).await;
        storage
            .insert_request(
                insert("req-1", now_ms()),
                vec![StoredHeader {
                    name: "content-type".to_owned(),
                    value: "application/json".to_owned(),
                }],
                vec![StoredHeader {
                    name: "content-type".to_owned(),
                    value: "application/json".to_owned(),
                }],
                Some(CaptureInsert {
                    request_content_type: Some("application/json".to_owned()),
                    response_content_type: Some("application/json".to_owned()),
                    request_body: Some(br#"{"ok":true}"#.to_vec()),
                    response_body: Some(br#"{"ok":false}"#.to_vec()),
                    request_body_truncated: false,
                    response_body_truncated: false,
                    request_body_sha256: Some("request-hash".to_owned()),
                    response_body_sha256: Some("response-hash".to_owned()),
                }),
                vec![PluginDecisionInsert {
                    request_id: "req-1".to_owned(),
                    plugin_id: "api-key-guard".to_owned(),
                    route_id: "payments".to_owned(),
                    action: "deny".to_owned(),
                    deny_status: Some(403),
                    set_headers: vec!["x-policy".to_owned()],
                    remove_headers: vec!["x-remove-me".to_owned()],
                    events: vec![PluginEvent {
                        name: "missing-api-key".to_owned(),
                        code: Some("auth".to_owned()),
                    }],
                    duration_ms: 2,
                    timed_out: false,
                    error: None,
                    created_at_ms: now_ms(),
                }],
            )
            .await
            .unwrap();

        let rows = storage
            .list_requests(ListFilters {
                failed: true,
                limit: 10,
                ..ListFilters::default()
            })
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "req-1");

        let details = storage.show_request("req-1").await.unwrap().unwrap();
        assert_eq!(details.request_headers[0].name, "content-type");
        assert_eq!(
            details.capture.unwrap().request_body,
            Some(br#"{"ok":true}"#.to_vec())
        );
        assert_eq!(details.plugin_decisions.len(), 1);
        assert_eq!(details.plugin_decisions[0].plugin_id, "api-key-guard");
        assert_eq!(details.plugin_decisions[0].set_headers, vec!["x-policy"]);
    }

    #[tokio::test]
    async fn retention_deletes_old_requests_and_evacuates_body_bytes() {
        let (storage, _dir) = storage(8).await;
        let old = now_ms() - 8 * 24 * 60 * 60 * 1000;
        storage
            .insert_request(insert("old", old), vec![], vec![], None, vec![])
            .await
            .unwrap();
        storage
            .insert_request(
                insert("new", now_ms()),
                vec![],
                vec![],
                Some(CaptureInsert {
                    request_content_type: Some("text/plain".to_owned()),
                    response_content_type: Some("text/plain".to_owned()),
                    request_body: Some(b"12345678".to_vec()),
                    response_body: Some(b"abcdefghi".to_vec()),
                    request_body_truncated: false,
                    response_body_truncated: false,
                    request_body_sha256: Some("request-hash".to_owned()),
                    response_body_sha256: Some("response-hash".to_owned()),
                }),
                vec![],
            )
            .await
            .unwrap();

        let outcome = storage.run_retention().await.unwrap();
        assert_eq!(outcome.deleted_requests, 1);
        assert_eq!(outcome.evicted_captures, 1);

        let details = storage.show_request("new").await.unwrap().unwrap();
        let capture = details.capture.unwrap();
        assert!(capture.body_evicted);
        assert!(capture.request_body.is_none());
        assert!(capture.response_body.is_none());
        assert_eq!(capture.request_body_sha256.as_deref(), Some("request-hash"));
    }

    #[tokio::test]
    async fn creates_consistent_backup_file() {
        let (storage, dir) = storage(4096).await;
        storage
            .insert_request(insert("req-1", now_ms()), vec![], vec![], None, vec![])
            .await
            .unwrap();

        let backup = dir.path().join("backup.db");
        storage.backup_to(&backup).await.unwrap();

        assert!(backup.exists());
        assert!(backup.metadata().unwrap().len() > 0);
    }

    #[tokio::test]
    async fn persists_replay_runs_with_request_details() {
        let (storage, _dir) = storage(4096).await;
        storage
            .insert_request(insert("req-1", now_ms()), vec![], vec![], None, vec![])
            .await
            .unwrap();
        storage
            .insert_replay_run(ReplayRunInsert {
                replay_id: "rep-1".to_owned(),
                original_request_id: "req-1".to_owned(),
                replay_request_id: "req-replay-1".to_owned(),
                target: "http://replay-target:4000".to_owned(),
                method: "POST".to_owned(),
                path: "/api/payments?mode=test".to_owned(),
                status: Some(200),
                latency_ms: 12,
                error: None,
                diff_summary: Some("original_status=500 replay_status=200".to_owned()),
                created_at_ms: now_ms(),
            })
            .await
            .unwrap();

        let details = storage.show_request("req-1").await.unwrap().unwrap();
        assert_eq!(details.replay_runs.len(), 1);
        assert_eq!(details.replay_runs[0].replay_id, "rep-1");
        assert_eq!(details.replay_runs[0].status, Some(200));
        assert_eq!(
            details.replay_runs[0].diff_summary.as_deref(),
            Some("original_status=500 replay_status=200")
        );
    }

    #[tokio::test]
    async fn postgres_migrates_inserts_lists_shows_and_replays_when_url_is_set() {
        let Ok(url) = std::env::var("TRACEGATE_TEST_POSTGRES_URL") else {
            eprintln!("skipping postgres storage test; TRACEGATE_TEST_POSTGRES_URL is not set");
            return;
        };

        let config = StorageConfig {
            driver: "postgres".to_owned(),
            url,
            max_total_capture_bytes: 4096,
            max_capture_bytes_per_request: 2048,
            ..StorageConfig::default()
        };
        let storage = Storage::connect(&config).await.unwrap();
        storage.migrate().await.unwrap();

        let request_id = format!("pg-{}", now_ms());
        let replay_id = format!("{request_id}-replay");
        let route_id = format!("{request_id}-route");
        let mut record = insert(&request_id, now_ms());
        record.route_id = Some(route_id.clone());

        storage
            .insert_request(
                record,
                vec![StoredHeader {
                    name: "content-type".to_owned(),
                    value: "application/json".to_owned(),
                }],
                vec![StoredHeader {
                    name: "content-type".to_owned(),
                    value: "application/json".to_owned(),
                }],
                Some(CaptureInsert {
                    request_content_type: Some("application/json".to_owned()),
                    response_content_type: Some("application/json".to_owned()),
                    request_body: Some(br#"{"ok":true}"#.to_vec()),
                    response_body: Some(br#"{"ok":false}"#.to_vec()),
                    request_body_truncated: false,
                    response_body_truncated: false,
                    request_body_sha256: Some("request-hash".to_owned()),
                    response_body_sha256: Some("response-hash".to_owned()),
                }),
                vec![PluginDecisionInsert {
                    request_id: request_id.clone(),
                    plugin_id: "api-key-guard".to_owned(),
                    route_id: route_id.clone(),
                    action: "deny".to_owned(),
                    deny_status: Some(403),
                    set_headers: vec!["x-policy".to_owned()],
                    remove_headers: vec!["x-remove-me".to_owned()],
                    events: vec![PluginEvent {
                        name: "missing-api-key".to_owned(),
                        code: Some("auth".to_owned()),
                    }],
                    duration_ms: 2,
                    timed_out: false,
                    error: None,
                    created_at_ms: now_ms(),
                }],
            )
            .await
            .unwrap();

        storage
            .insert_replay_run(ReplayRunInsert {
                replay_id: replay_id.clone(),
                original_request_id: request_id.clone(),
                replay_request_id: format!("{request_id}-request"),
                target: "http://replay-target:4000".to_owned(),
                method: "POST".to_owned(),
                path: "/api/payments?mode=test".to_owned(),
                status: Some(200),
                latency_ms: 12,
                error: None,
                diff_summary: Some("original_status=500 replay_status=200".to_owned()),
                created_at_ms: now_ms(),
            })
            .await
            .unwrap();

        let rows = storage
            .list_requests(ListFilters {
                failed: true,
                route_id: Some(route_id.clone()),
                limit: 10,
                ..ListFilters::default()
            })
            .await
            .unwrap();
        assert!(rows.iter().any(|row| row.request_id == request_id));

        let details = storage.show_request(&request_id).await.unwrap().unwrap();
        assert_eq!(details.request.route_id.as_deref(), Some(route_id.as_str()));
        assert_eq!(details.request_headers[0].name, "content-type");
        assert_eq!(
            details.capture.unwrap().request_body,
            Some(br#"{"ok":true}"#.to_vec())
        );
        assert_eq!(details.plugin_decisions.len(), 1);
        assert_eq!(details.plugin_decisions[0].plugin_id, "api-key-guard");
        assert_eq!(details.plugin_decisions[0].set_headers, vec!["x-policy"]);
        assert_eq!(details.replay_runs.len(), 1);
        assert_eq!(details.replay_runs[0].replay_id, replay_id);
        assert_eq!(details.replay_runs[0].status, Some(200));
    }
}
