# TraceGate

TraceGate is a Rust observability API gateway that routes HTTP traffic, records failure evidence, replays captured requests, and runs sandboxed request-policy plugins.

v0.7 provides the gateway foundation, observability, SQLite demo storage, PostgreSQL production storage, safe replay, a sandboxed WASM `before_request` policy hook, production-mode hardening, and the TraceGate Console/full demo path:

- host and path-prefix routing
- Hyper-based reverse proxying
- UUID v7 `x-request-id` propagation
- structured JSON request logs
- W3C `traceparent` propagation
- OpenTelemetry OTLP trace export
- Prometheus metrics on the admin listener
- SQLite request metadata and bounded capture storage in demo mode
- PostgreSQL request metadata and bounded capture storage in production mode
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
- bearer-authenticated admin endpoints when an admin token is configured
- `POST /admin/reload` for atomic route/plugin/redaction/capture hot reloads
- read-only TraceGate Console on the admin listener at `/console/`
- bearer-protected admin JSON APIs under `/admin/api/*`
- route health, recent request, request detail, replay run, plugin decision, plugin summary, and telemetry status views
- provisioned Grafana `TraceGate Overview` dashboard
- rustls data-plane TLS and HTTPS upstream verification in production mode
- per-route timeouts, retry configuration, concurrency limits, and passive upstream health
- bounded capture persistence with drop accounting under backpressure
- local Docker Compose demo with users, payments, a safe replay target, OpenTelemetry Collector, Jaeger, and Prometheus
- repeatable local and GCP full-demo scripts
- Terraform and SSH-based GCP Compute Engine deployment assets with production-mode HTTPS/PostgreSQL Compose

## Local Demo

```powershell
$adminToken = "tracegate-local-admin"
docker compose up --build
curl.exe -i http://localhost:8080/api/users/123
curl.exe -i http://localhost:8080/api/payments/fail
curl.exe -i http://localhost:8080/api/plugin-timeout/proof
curl.exe -i -H "x-api-key: tracegate-demo-key" http://localhost:8080/api/payments/fail
curl.exe -i -H "x-api-key: tracegate-demo-key" "http://localhost:8080/api/payments/slow?token=secret&visible=yes"
curl.exe -i -X POST -H "x-api-key: tracegate-demo-key" -H "content-type: application/json" -H "authorization: Bearer secret" --data "{\"card\":\"4242\",\"note\":\"capture proof\"}" "http://localhost:8080/api/payments/large-fail?api_key=secret&visible=yes"
curl.exe -s -H "Authorization: Bearer $adminToken" http://localhost:9090/health/live
curl.exe -s -H "Authorization: Bearer $adminToken" http://localhost:9090/metrics
curl.exe -s http://localhost:9090/console/
curl.exe -s -H "Authorization: Bearer $adminToken" http://localhost:9090/admin/api/overview
curl.exe -s -H "Authorization: Bearer $adminToken" "http://localhost:9090/admin/api/requests?failed=true&limit=10"
docker logs tracegate-tracegate-1 --tail 50
docker compose exec tracegate tracegate plugins inspect /usr/local/share/tracegate/plugins/api-key-guard.wasm
docker compose exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --failed
docker compose exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --slow
docker compose exec tracegate tracegate replay --config /etc/tracegate/tracegate.toml --last-failed --target http://replay-target:4000 --confirm-side-effects
docker compose exec tracegate tracegate requests show --config /etc/tracegate/tracegate.toml --id <request-id>
docker compose logs replay-target --tail 50
scripts\full-demo.ps1
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
- TraceGate Console is available locally at `http://localhost:9090/console/`; enter `tracegate-local-admin` as the bearer token.
- Jaeger is available locally at `http://localhost:16686`; Prometheus is available locally at `http://localhost:9091`; Grafana is available locally at `http://localhost:3000`.
- `scripts\full-demo.ps1` verifies the local console APIs, route health, plugin summaries, telemetry status, replay-run display, plugin-deny display, and Grafana dashboard provisioning.

## Local Production Mode

Local production mode uses PostgreSQL, rustls on the data-plane listener, HTTPS demo upstreams, bearer-authenticated admin routes, and the same production config validation rules used on GCP.

```powershell
scripts\prepare-production-compose.ps1
docker compose -f docker-compose.production.yml up --build
curl.exe --cacert data\production\tls\ca.crt https://localhost:8443/api/users/123
curl.exe -i http://localhost:19090/health/live
curl.exe -i -H "Authorization: Bearer <TRACEGATE_ADMIN_TOKEN>" http://localhost:19090/health/ready
curl.exe -X POST -H "Authorization: Bearer <TRACEGATE_ADMIN_TOKEN>" http://localhost:19090/admin/reload
docker compose -f docker-compose.production.yml exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --failed
```

`scripts\prepare-production-compose.ps1` writes ignored local secrets to `.env.production`, writes the Prometheus admin-token file, and generates a private CA plus TraceGate/upstream certificates under `data\production\tls`.

## Local Checks

```powershell
scripts/cargo.ps1 fmt --check
scripts/cargo.ps1 clippy --workspace --all-targets -- -D warnings
scripts/cargo.ps1 test --workspace

docker run -d --name tracegate-postgres-test -p 55432:5432 -e POSTGRES_USER=tracegate -e POSTGRES_PASSWORD=tracegate -e POSTGRES_DB=tracegate_test postgres:16-alpine
$env:TRACEGATE_TEST_POSTGRES_URL='postgres://tracegate:tracegate@localhost:55432/tracegate_test'
scripts/cargo.ps1 test -p tracegate-storage
```

## GCP Deployment

The live deployment path is intentionally direct-control:

- dedicated TraceGate GCP project
- Terraform-managed `e2-micro` Compute Engine VM in `us-central1-a`
- Docker Compose under systemd with TraceGate, demo backends, a safe internal replay target, OpenTelemetry Collector, Jaeger, and Prometheus
- production mode uses HTTPS on the public TraceGate listener, internal PostgreSQL, generated private CA material, HTTPS demo upstreams, Grafana dashboard provisioning, and bearer-authenticated admin/console inspection over the VM Docker network
- local image build, `docker save`, `gcloud compute scp`, VM-side `docker load`
- live HTTPS `curl --cacert` smoke plus SSH console/API/Grafana/telemetry/log/storage inspection

Scripts live under `deployments/gcp/scripts`.

```powershell
deployments/gcp/scripts/bootstrap-project.ps1 -BillingAccount <billing-account-id>
deployments/gcp/scripts/terraform-apply.ps1
deployments/gcp/scripts/deploy.ps1
deployments/gcp/scripts/smoke.ps1
deployments/gcp/scripts/full-demo.ps1
deployments/gcp/scripts/inspect-captures.ps1
deployments/gcp/scripts/inspect-replay.ps1
deployments/gcp/scripts/backup-storage.ps1
deployments/gcp/scripts/inspect-observability.ps1
deployments/gcp/scripts/logs.ps1
```

The guard script refuses to deploy unless the active account is `nickaccturk@gmail.com` and the active project is a dedicated `tracegate-*` project. Only TraceGate's public data-plane port `8080` is exposed by Terraform; production traffic on that port is HTTPS. Admin, Console, PostgreSQL, Grafana, telemetry services, and the replay target are inspected over SSH inside the VM.
