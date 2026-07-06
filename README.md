# TraceGate

TraceGate is a Rust observability API gateway that routes HTTP traffic and is being built toward failure capture, replay, and sandboxed request-policy plugins.

v0.2 provides the gateway foundation plus observability:

- host and path-prefix routing
- Hyper-based reverse proxying
- UUID v7 `x-request-id` propagation
- structured JSON request logs
- W3C `traceparent` propagation
- OpenTelemetry OTLP trace export
- Prometheus metrics on the admin listener
- `/health/live` and `/health/ready`
- per-route timeouts and retry configuration
- local Docker Compose demo with users and payments backends, OpenTelemetry Collector, Jaeger, and Prometheus
- Terraform and SSH-based GCP Compute Engine deployment assets

## Local Demo

```powershell
docker compose up --build
curl.exe -i http://localhost:8080/api/users/123
curl.exe -i http://localhost:8080/api/payments/fail
curl.exe -s http://localhost:9090/health/live
curl.exe -s http://localhost:9090/metrics
docker logs tracegate-tracegate-1 --tail 50
```

Expected behavior:

- `/api/users/123` returns HTTP `200` from the users backend.
- `/api/payments/fail` returns HTTP `500` from the payments backend.
- TraceGate logs one JSON completion event per request with `request_id`, `route_id`, `upstream`, `status`, and `latency_ms`.
- TraceGate injects or propagates `traceparent` to upstream requests.
- Prometheus exposes `tracegate_requests_total`, `tracegate_request_duration_seconds`, and `tracegate_upstream_errors_total`.
- Jaeger is available locally at `http://localhost:16686`; Prometheus is available locally at `http://localhost:9091`.

## Local Checks

```powershell
scripts/cargo.ps1 fmt --check
scripts/cargo.ps1 clippy --workspace --all-targets -- -D warnings
scripts/cargo.ps1 test --workspace
```

## GCP v0.2 Deployment

The live deployment path is intentionally direct-control:

- dedicated TraceGate GCP project
- Terraform-managed `e2-micro` Compute Engine VM in `us-central1-a`
- Docker Compose under systemd with TraceGate, demo backends, OpenTelemetry Collector, Jaeger, and Prometheus
- local image build, `docker save`, `gcloud compute scp`, VM-side `docker load`
- live `curl` smoke plus SSH telemetry/log inspection

Scripts live under `deployments/gcp/scripts`.

```powershell
deployments/gcp/scripts/bootstrap-project.ps1 -BillingAccount <billing-account-id>
deployments/gcp/scripts/terraform-apply.ps1
deployments/gcp/scripts/deploy.ps1
deployments/gcp/scripts/smoke.ps1
deployments/gcp/scripts/inspect-observability.ps1
deployments/gcp/scripts/logs.ps1
```

The guard script refuses to deploy unless the active account is `nickaccturk@gmail.com` and the active project is a dedicated `tracegate-*` project. Only TraceGate's public HTTP port `8080` is exposed by Terraform; telemetry services are inspected over SSH inside the VM.
