CREATE TABLE IF NOT EXISTS replay_runs (
    replay_id TEXT PRIMARY KEY NOT NULL,
    original_request_id TEXT NOT NULL REFERENCES requests(request_id) ON DELETE CASCADE,
    replay_request_id TEXT NOT NULL,
    target TEXT NOT NULL,
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    status BIGINT,
    latency_ms BIGINT NOT NULL,
    error TEXT,
    diff_summary TEXT,
    created_at_ms BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS replay_runs_original_request_idx ON replay_runs(original_request_id, created_at_ms);
CREATE INDEX IF NOT EXISTS replay_runs_created_at_idx ON replay_runs(created_at_ms);
