# TraceGate

TraceGate is a Rust observability API gateway that routes HTTP traffic and is being built toward failure capture, replay, and sandboxed request-policy plugins.

v0.1 provides the gateway foundation:

- host and path-prefix routing
- Hyper-based reverse proxying
- UUID v7 `x-request-id` propagation
- structured JSON request logs
- per-route timeouts and retry configuration
- local Docker Compose demo with users and payments backends
- Terraform and SSH-based GCP Compute Engine deployment assets

## Local Demo

```powershell
docker compose up --build
curl.exe -i http://localhost:8080/api/users/123
curl.exe -i http://localhost:8080/api/payments/fail
docker logs tracegate-tracegate-1 --tail 50
```

Expected behavior:

- `/api/users/123` returns HTTP `200` from the users backend.
- `/api/payments/fail` returns HTTP `500` from the payments backend.
- TraceGate logs one JSON completion event per request with `request_id`, `route_id`, `upstream`, `status`, and `latency_ms`.

## Local Checks

```powershell
$cargo = "$env:USERPROFILE\.cargo\bin\cargo.exe"
& $cargo fmt --check
& $cargo clippy --workspace --all-targets -- -D warnings
& $cargo test --workspace
```

## GCP v0.1 Deployment

The live deployment path is intentionally direct-control:

- dedicated TraceGate GCP project
- Terraform-managed `e2-micro` Compute Engine VM in `us-central1-a`
- Docker Compose under systemd
- local image build, `docker save`, `gcloud compute scp`, VM-side `docker load`
- live `curl` smoke plus SSH log inspection

Scripts live under `deployments/gcp/scripts`.

```powershell
deployments/gcp/scripts/bootstrap-project.ps1 -BillingAccount <billing-account-id>
deployments/gcp/scripts/terraform-apply.ps1
deployments/gcp/scripts/deploy.ps1
deployments/gcp/scripts/smoke.ps1
deployments/gcp/scripts/logs.ps1
```

The guard script refuses to deploy unless the active account is `nickaccturk@gmail.com` and the active project is a dedicated `tracegate-*` project.
