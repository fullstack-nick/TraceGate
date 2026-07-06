CREATE TABLE IF NOT EXISTS requests (
    request_id TEXT PRIMARY KEY NOT NULL,
    trace_id TEXT,
    route_id TEXT,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    redacted_query TEXT,
    query_hash TEXT,
    status INTEGER NOT NULL,
    latency_ms INTEGER NOT NULL,
    upstream TEXT,
    is_error INTEGER NOT NULL,
    is_slow INTEGER NOT NULL,
    capture_policy TEXT NOT NULL,
    capture_dropped INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS requests_created_at_idx ON requests(created_at_ms);
CREATE INDEX IF NOT EXISTS requests_route_idx ON requests(route_id, created_at_ms);
CREATE INDEX IF NOT EXISTS requests_error_idx ON requests(is_error, created_at_ms);
CREATE INDEX IF NOT EXISTS requests_slow_idx ON requests(is_slow, created_at_ms);

CREATE TABLE IF NOT EXISTS request_headers (
    request_id TEXT NOT NULL REFERENCES requests(request_id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (request_id, name, value)
);

CREATE TABLE IF NOT EXISTS response_headers (
    request_id TEXT NOT NULL REFERENCES requests(request_id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (request_id, name, value)
);

CREATE TABLE IF NOT EXISTS captures (
    request_id TEXT PRIMARY KEY NOT NULL REFERENCES requests(request_id) ON DELETE CASCADE,
    request_content_type TEXT,
    response_content_type TEXT,
    request_body BLOB,
    response_body BLOB,
    request_body_truncated INTEGER NOT NULL,
    response_body_truncated INTEGER NOT NULL,
    request_body_sha256 TEXT,
    response_body_sha256 TEXT,
    body_evicted INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS captures_created_at_idx ON captures(created_at_ms);
