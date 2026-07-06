# TraceGate

TraceGate is a Rust observability API gateway that routes HTTP traffic and is being built toward failure capture, replay, and sandboxed request-policy plugins.

v0.5 provides the gateway foundation, observability, a SQLite capture store, safe replay, and a sandboxed WASM `before_request` policy hook:

- host and path-prefix routing
- Hyper-based reverse proxying
- UUID v7 `x-request-id` propagation
- structured JSON request logs
- W3C `traceparent` propagation
- OpenTelemetry OTLP trace export
- Prometheus metrics on the admin listener
- SQLite request metadata and bounded capture storage
- redacted header/query storage for captured requests
- `tracegate requests list` and `tracegate requests show`
- `tracegate replay --id` and `tracegate replay --last-failed`
- replay audit records with status, latency, target, and replay metadata
- `tracegate plugins inspect`
- Wasmtime Component Model policy plugins with timeout, fuel, memory, and import limits
- pre-upstream allow, deny, header mutation, event, and bounded body-preview decisions
- sanitized plugin decision storage and Prometheus plugin metrics
- bundled `api-key-guard` and `header-normalizer` example plugins
- `tracegate storage migrate`, `prune`, and `backup`
- `/health/live` and `/health/ready`
- per-route timeouts and retry configuration
- local Docker Compose demo with users, payments, a safe replay target, OpenTelemetry Collector, Jaeger, and Prometheus
- Terraform and SSH-based GCP Compute Engine deployment assets

## Local Demo

```powershell
docker compose up --build
curl.exe -i http://localhost:8080/api/users/123
curl.exe -i http://localhost:8080/api/payments/fail
curl.exe -i http://localhost:8080/api/plugin-timeout/proof
curl.exe -i -H "x-api-key: tracegate-demo-key" http://localhost:8080/api/payments/fail
curl.exe -i -H "x-api-key: tracegate-demo-key" "http://localhost:8080/api/payments/slow?token=secret&visible=yes"
curl.exe -i -X POST -H "x-api-key: tracegate-demo-key" -H "content-type: application/json" -H "authorization: Bearer secret" --data "{\"card\":\"4242\",\"note\":\"capture proof\"}" "http://localhost:8080/api/payments/large-fail?api_key=secret&visible=yes"
curl.exe -s http://localhost:9090/health/live
curl.exe -s http://localhost:9090/metrics
docker logs tracegate-tracegate-1 --tail 50
docker compose exec tracegate tracegate plugins inspect /usr/local/share/tracegate/plugins/api-key-guard.wasm
docker compose exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --failed
docker compose exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --slow
docker compose exec tracegate tracegate replay --config /etc/tracegate/tracegate.toml --last-failed --target http://replay-target:4000 --confirm-side-effects
docker compose exec tracegate tracegate requests show --config /etc/tracegate/tracegate.toml --id <request-id>
docker compose logs replay-target --tail 50
```

Expected behavior:

- `/api/users/123` returns HTTP `200` from the users backend.
- `/api/payments/fail` without `x-api-key` returns HTTP `403` from the policy layer and does not reach the payments backend.
- `/api/plugin-timeout/proof` returns HTTP `403` from the fail-closed policy timeout path and does not reach the payments backend.
- `/api/payments/fail` with `x-api-key: tracegate-demo-key` returns HTTP `500` from the payments backend.
- TraceGate logs one JSON completion event per request with `request_id`, `route_id`, `upstream`, `status`, and `latency_ms`.
- TraceGate injects or propagates `traceparent` to upstream requests.
- Prometheus exposes `tracegate_requests_total`, `tracegate_request_duration_seconds`, and `tracegate_upstream_errors_total`.
- Prometheus exposes `tracegate_captures_total`, `tracegate_capture_dropped_total`, and `tracegate_storage_retention_runs_total`.
- Prometheus exposes `tracegate_plugin_decisions_total`, `tracegate_plugin_duration_seconds`, `tracegate_plugin_timeouts_total`, and `tracegate_plugin_errors_total`.
- `tracegate requests show` includes sanitized plugin decisions: action, deny status, header names, event names/codes, timing, timeout, and error flags.
- Failed and slow payment requests are persisted in `/var/lib/tracegate/tracegate.db`.
- Stored query strings omit configured sensitive params such as `token`, `access_token`, and `api_key`; stored headers omit configured sensitive headers such as `authorization`, `cookie`, `set-cookie`, and `x-api-key`.
- Replaying a captured failed payment request sends it to the safe replay target, adds replay metadata headers, and persists a replay audit record.
- Mutating replay methods require `--confirm-side-effects`.
- Jaeger is available locally at `http://localhost:16686`; Prometheus is available locally at `http://localhost:9091`.

## Local Checks

```powershell
scripts/cargo.ps1 fmt --check
scripts/cargo.ps1 clippy --workspace --all-targets -- -D warnings
scripts/cargo.ps1 test --workspace
```

## GCP Deployment

The live deployment path is intentionally direct-control:

- dedicated TraceGate GCP project
- Terraform-managed `e2-micro` Compute Engine VM in `us-central1-a`
- Docker Compose under systemd with TraceGate, demo backends, a safe internal replay target, OpenTelemetry Collector, Jaeger, and Prometheus
- local image build, `docker save`, `gcloud compute scp`, VM-side `docker load`
- live `curl` smoke plus SSH telemetry/log inspection

Scripts live under `deployments/gcp/scripts`.

```powershell
deployments/gcp/scripts/bootstrap-project.ps1 -BillingAccount <billing-account-id>
deployments/gcp/scripts/terraform-apply.ps1
deployments/gcp/scripts/deploy.ps1
deployments/gcp/scripts/smoke.ps1
deployments/gcp/scripts/inspect-captures.ps1
deployments/gcp/scripts/inspect-replay.ps1
deployments/gcp/scripts/backup-storage.ps1
deployments/gcp/scripts/inspect-observability.ps1
deployments/gcp/scripts/logs.ps1
```

The guard script refuses to deploy unless the active account is `nickaccturk@gmail.com` and the active project is a dedicated `tracegate-*` project. Only TraceGate's public HTTP port `8080` is exposed by Terraform; telemetry services and the replay target are inspected over SSH inside the VM.
