CREATE TABLE IF NOT EXISTS plugin_decisions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    request_id TEXT NOT NULL REFERENCES requests(request_id) ON DELETE CASCADE,
    plugin_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    action TEXT NOT NULL,
    deny_status INTEGER,
    set_headers_json TEXT NOT NULL,
    remove_headers_json TEXT NOT NULL,
    events_json TEXT NOT NULL,
    duration_ms INTEGER NOT NULL,
    timed_out INTEGER NOT NULL,
    error TEXT,
    created_at_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS plugin_decisions_request_idx ON plugin_decisions(request_id, id);
CREATE INDEX IF NOT EXISTS plugin_decisions_plugin_idx ON plugin_decisions(plugin_id, created_at_ms);
CREATE INDEX IF NOT EXISTS plugin_decisions_route_idx ON plugin_decisions(route_id, created_at_ms);
