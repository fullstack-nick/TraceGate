# TraceGate Plan

## Product Definition

**TraceGate is a Rust-based observability API gateway that routes HTTP traffic, records failures and latency evidence, exports OpenTelemetry telemetry, and replays captured requests for debugging with sandboxed WebAssembly request-policy plugins.**

TraceGate is an API gateway underneath, but the product is not a general replacement for Envoy, Kong, NGINX, or Traefik. The gateway is the engine. The product value is failure debugging:

```text
Client
  -> TraceGate
  -> route match
  -> before_request WASM policy
  -> request ID + trace context
  -> upstream proxy
  -> status/latency/error recording
  -> replayable capture
  -> response to client
```

The headline is:

> Rust observability gateway for recording and replaying API failures.

## Success Criteria

TraceGate reaches v1.0 when all of this works end to end:

- `tracegate serve --config tracegate.toml` runs a production-shaped gateway process.
- HTTP requests route by host and path prefix to one or more upstream services.
- Every request gets a request ID, structured logs, metrics, and trace context propagation.
- Failed and slow requests are captured with bounded, redacted, retention-controlled storage.
- A captured request can be replayed against an explicit local or staging target.
- Replay preserves method, path, query, selected headers, and captured body while adding replay metadata headers.
- A sandboxed Wasmtime `before_request` plugin can allow, deny, or mutate request headers.
- The demo starts with `docker compose up --build` and shows routing, traces, metrics, capture, replay, and plugin blocking.
- CI runs formatting, linting, unit tests, integration tests, plugin contract tests, and benchmark smoke checks.
- Production mode refuses unsafe configuration for admin access, capture retention, and plugin limits.
- Every completed milestone is committed, pushed to GitHub, deployed on GCP, and verified through live HTTP calls against the GCP endpoint.
- Every completed milestone has live operational proof from the GCP VM: process status, container status, logs, storage state, and the expected response after the live call.
- Every local feature, config, script, plugin, migration, backend fixture, and demo asset required for a milestone is available in the GCP deployment path before that milestone counts as implemented.

## GCP Cloud-Native Delivery Model

TraceGate is a GCP-deployed project from the first runnable milestone. Local development is useful for fast iteration, but local success never counts as stage completion by itself.

The canonical v1 live deployment is:

```text
GitHub repository
  -> Terraform-managed GCP infrastructure
  -> Compute Engine Linux VM
  -> Docker Engine + Docker Compose
  -> systemd-managed TraceGate deployment
  -> direct SSH inspection and live HTTP verification
```

Locked GCP decisions:

- GCP is the production proof environment for every stage.
- The project is built under GCP free-trial/free-tier constraints.
- Every stage starts with a current free-tier budget and quota check before resources are created or changed.
- Terraform is the source of truth for GCP infrastructure.
- Direct SSH is part of the official workflow for deployment control, runtime inspection, log review, storage inspection, and debugging.
- The v1 runtime target is a Terraform-managed Compute Engine VM running Docker Compose services under systemd.
- Managed GKE, Cloud SQL, Memorystore, external load balancers, and Cloud Run are excluded from v1 because the free-tier/free-trial budget and direct-control workflow take priority.
- GitHub is the source of truth for deployable code. A stage is not complete until the relevant commit is pushed.
- No local-only deliverables count. Anything built locally must be deployable and testable on the live GCP VM.

The required stage completion sequence is:

```text
1. Implement locally.
2. Run local tests.
3. Commit and push to GitHub.
4. Apply or verify Terraform for the stage.
5. Deploy the pushed commit to the GCP VM.
6. Call the live GCP endpoint from outside the VM.
7. SSH into the VM.
8. Inspect process/container status, logs, config, storage, and telemetry.
9. Save concise proof commands and observed results in the stage proof artifact.
10. Mark the stage complete only after live behavior matches the expected behavior.
```

Live proof must include:

- Git commit SHA deployed on the VM.
- Terraform plan/apply summary for infrastructure changes.
- VM external endpoint used for testing.
- Live `curl` or CLI command output from outside the VM.
- SSH inspection commands for systemd, Docker, logs, config, and storage.
- Evidence that the request reached TraceGate and the expected upstream.
- Evidence that TraceGate recorded, traced, replayed, or blocked the request when that feature is part of the stage.
- Cleanup or budget check output proving the deployment is still inside the intended GCP cost envelope.

The live deployment layout is part of the product:

```text
deployments/gcp/terraform/
  - VM, firewall rules, service account, static IP strategy, and budget-conscious variables

deployments/gcp/scripts/
  - build, upload, deploy, restart, log, status, smoke, and rollback scripts

deployments/gcp/systemd/
  - tracegate service unit and environment file template

deployments/gcp/compose/
  - TraceGate, demo backends, collector, Prometheus, Grafana, and replay target

proof/
  - durable per-stage proof commands and live observations
```

## Research Baseline

The initial technology direction is sound, with one important constraint around the Rust OpenTelemetry SDK status:

- Rust remains a strong portfolio signal: Stack Overflow's 2025 survey lists Rust as the most admired programming language at 72%.
- OpenTelemetry is a strong observability choice: CNCF announced OpenTelemetry graduation on May 21, 2026, as a vendor-neutral standard for traces, metrics, and logs.
- OpenTelemetry Rust must be isolated behind our own adapter crate because the OpenTelemetry Rust documentation currently marks traces, metrics, and logs as beta.
- Wasmtime is the right plugin runtime family because its documentation describes support for WebAssembly, WASI, and the Component Model, plus configurable CPU and memory controls.
- Hyper is the right low-level HTTP foundation because it supports HTTP/1 and HTTP/2, async client/server APIs, and production use.
- Axum is the right admin/control-plane framework because it is built for Tokio and Hyper and uses Tower middleware.
- SQLx is the right storage layer because it supports async Rust, compile-time checked queries, SQLite, and PostgreSQL without an ORM DSL.
- SQLite WAL is good for embedded single-host mode, but it is not the multi-instance production store because SQLite WAL requires all database users to be on the same host.
- Google Cloud's current free program documentation describes $300 in free credits and free usage for 20+ products up to limits, so each stage must verify the active account, credits, quotas, and resource pricing before deployment.
- Google Cloud documents Terraform as a way to automate infrastructure on Google Cloud and describes the Google Cloud provider as a way to configure and manage Google Cloud resources with declarative tooling.
- Google Cloud documents SSH access to Compute Engine Linux VMs, which matches the project's direct-control deployment and inspection model.

Reference links:

- [Stack Overflow 2025 Technology Survey](https://survey.stackoverflow.co/2025/technology)
- [CNCF OpenTelemetry Graduation Announcement](https://www.cncf.io/announcements/2026/05/21/cloud-native-computing-foundation-announces-opentelemetrys-graduation-solidifying-status-as-the-de-facto-observability-standard/)
- [OpenTelemetry Rust Documentation](https://opentelemetry.io/docs/languages/rust/)
- [Wasmtime Documentation](https://docs.wasmtime.dev/)
- [Hyper Documentation](https://docs.rs/hyper/latest/hyper/)
- [Axum Documentation](https://docs.rs/axum/latest/axum/)
- [SQLx Repository](https://github.com/transact-rs/sqlx)
- [SQLite WAL Documentation](https://sqlite.org/wal.html)
- [Google Cloud Free Program](https://cloud.google.com/free)
- [Terraform on Google Cloud](https://docs.cloud.google.com/docs/terraform)
- [Connect to Compute Engine Linux VMs with SSH](https://docs.cloud.google.com/compute/docs/connect/standard-ssh)

## Open Decision Questions Locked Now

| Question | Locked decision | Reason |
| --- | --- | --- |
| Is this a plain API gateway or an observability product? | It is an observability gateway with an API gateway data plane. | Routing is necessary, but replay and failure analysis make the project useful and distinctive. |
| Is TraceGate trying to replace Envoy, Kong, or NGINX? | No. TraceGate is a developer/debugging gateway focused on HTTP observability, capture, replay, and policy hooks. | This gives a strong product defense and keeps scope coherent. |
| Which HTTP stack should be used? | Hyper for proxy internals, Axum for admin/control HTTP, Tower for middleware layers, Tokio for async runtime. | This uses the Rust ecosystem's production HTTP foundation while keeping admin APIs ergonomic. |
| Should routing use wildcard strings or explicit prefix matching? | Use explicit host-aware `path_prefix` routes with longest-prefix match. | Prefix matching is predictable, fast, testable, and enough for API gateway routing. |
| Should TraceGate buffer whole requests and responses? | No. The proxy path is streaming. Capture uses a bounded tee that stores only configured request and response bytes. | This protects latency, memory, and large payload handling. |
| Should request bodies be captured? | Metadata is always recorded. Body capture is per-route, bounded, content-type aware, redacted, and retention-controlled. | Replay needs bodies, but production capture must avoid unbounded sensitive data collection. |
| Which storage backend is the long-run decision? | SQLx with SQLite for embedded single-node mode and PostgreSQL for production multi-instance mode. | SQLite keeps local demos simple; PostgreSQL is the durable shared production store. |
| Should we add Redis? | Redis is excluded from the v1 architecture. | The product does not require distributed rate limiting or queues to prove its core value. |
| Should replay target the original upstream by default? | No. Replay requires an explicit `--target` or configured replay environment. | This prevents accidental production side effects. |
| Should replay support POST, PUT, PATCH, and DELETE? | Yes, with `--confirm-side-effects` and explicit target selection. | Real API failures often involve mutating requests, so the tool must support them safely. |
| Should redaction be a plugin? | No. Redaction is built into the gateway core. | Privacy and secret handling are baseline safety features, not plugin behavior. |
| What WASM interface should be stable? | One stable `before_request` hook through v1.0 using Wasmtime and a WIT-defined Component Model contract. | One strong hook is enough for security-policy demos and production hardening. |
| What can a plugin do? | A plugin can allow, deny, set headers, remove headers, and emit a policy decision event. It cannot perform network or filesystem I/O. | This keeps plugins useful while preserving a tight sandbox. |
| What admin security model should be used? | Admin API binds to `127.0.0.1` by default and always requires a bearer admin token. Remote admin mode requires TLS. | Control surfaces must fail closed in production. |
| Should there be a UI? | Yes. TraceGate Console is a v1 feature for inspecting captured traffic, replay runs, routes, plugins, and telemetry health. CLI remains the primary automation surface. | The console makes the product demo clearer without replacing the CLI. |
| What is the canonical live deployment platform? | GCP Compute Engine VM provisioned by Terraform, running Docker Compose under systemd. | This gives the most control, direct SSH visibility, and the best fit for free-tier/free-trial constraints. |
| What makes a milestone complete? | Local tests plus GitHub push plus live GCP deployment plus live HTTP proof plus SSH log/runtime inspection. | Implementation is not real until the feature works in the cloud environment. |
| Can a feature remain local-only? | No. Every milestone feature must be deployable and testable on the GCP VM. | The live system is the product; local-only code creates false progress. |
| Should Kubernetes be part of v1? | No. v1 uses Terraform-managed Compute Engine, Docker Compose, and systemd. | Managed Kubernetes adds cost and less direct control; single-VM GCP deployment better matches the project constraints. |
| Should gRPC and eBPF be included? | No. v1 is HTTP API traffic. eBPF is a different product. | The value is replayable HTTP failure analysis, not kernel tracing or service mesh coverage. |
| Which license should be used? | Apache-2.0. | It is production-friendly and includes an explicit patent grant. |

## Architecture

### Main Components

```text
tracegate-cli
  - clap command surface
  - serve, config check, requests list, replay, plugins inspect

tracegate-core
  - config model
  - route matcher
  - request ID model
  - shared errors and types

tracegate-proxy
  - Hyper client/server proxy path
  - Tower layers for timeout, retry, concurrency, tracing, and policy execution
  - streaming request/response forwarding

tracegate-observability
  - tracing subscriber setup
  - OpenTelemetry adapter
  - Prometheus metrics endpoint
  - W3C trace context propagation

tracegate-store
  - SQLx repository layer
  - SQLite migrations
  - PostgreSQL migrations
  - retention enforcement
  - redacted capture persistence

tracegate-replay
  - replay builder
  - target validation
  - side-effect confirmation guard
  - replay result recording

tracegate-wasm
  - Wasmtime engine setup
  - WIT contract bindings
  - plugin lifecycle
  - timeout, fuel, memory, and host capability limits

tracegate-console
  - Axum-served local web console
  - route, request, replay, plugin, and telemetry views

tracegate-gcp-deploy
  - Terraform infrastructure
  - direct SSH deployment scripts
  - systemd service definitions
  - Docker Compose live runtime
  - live smoke/proof commands
```

### Request Flow

```text
1. Accept client request.
2. Validate method, URI, headers, and body size limits.
3. Generate or validate `x-request-id`.
4. Extract or create W3C trace context.
5. Match route by host and longest path prefix.
6. Run `before_request` plugin chain for the route.
7. Apply plugin decision: allow, deny, or header mutation.
8. Proxy request to upstream with timeout, retry, and concurrency limits.
9. Stream response back to client.
10. Record metadata, status, latency, route, upstream, trace ID, plugin decisions, and capture pointers.
11. Export metrics and traces.
12. Enforce retention and storage budgets in the background.
```

### Routing Contract

Routing is deterministic:

- Host match first.
- Longest `path_prefix` match second.
- Route order is only a tie-breaker for exact equal prefixes.
- Upstreams are a list, even when one upstream is configured.
- The default upstream policy is round-robin with passive health.
- `strip_prefix` and `rewrite_prefix` are explicit route settings.
- TraceGate proxies requests; it does not issue HTTP redirects for normal routing.

Example:

```toml
[[routes]]
id = "users"
hosts = ["localhost"]
path_prefix = "/api/users"
upstreams = ["http://users-service:3000"]
strip_prefix = false
timeout_ms = 3000
retries = 1
concurrency_limit = 100

[[routes]]
id = "payments"
hosts = ["localhost"]
path_prefix = "/api/payments"
upstreams = ["http://payments-service:4000"]
strip_prefix = false
timeout_ms = 3000
retries = 0
concurrency_limit = 50
slow_threshold_ms = 500
capture_policy = "errors_and_slow"
capture_request_body = true
capture_response_body_bytes = 4096
```

## Configuration Contract

The main config file is `tracegate.toml`.

```toml
[server]
listen = "0.0.0.0:8080"
admin_listen = "127.0.0.1:9090"
shutdown_grace_ms = 10000

[admin]
token_env = "TRACEGATE_ADMIN_TOKEN"

[storage]
driver = "sqlite"
url = "sqlite://tracegate.db"
retention_days = 7
max_total_capture_bytes = 1073741824
max_capture_bytes_per_request = 1048576

[redaction]
headers = ["authorization", "cookie", "set-cookie", "x-api-key"]
query_params = ["token", "access_token", "api_key"]

[observability]
service_name = "tracegate"
environment = "demo"
otlp_endpoint = "http://otel-collector:4317"
prometheus_enabled = true
json_logs = true

[[routes]]
id = "users"
hosts = ["localhost"]
path_prefix = "/api/users"
upstreams = ["http://users-service:3000"]
timeout_ms = 3000
retries = 1
capture_policy = "errors_and_slow"
slow_threshold_ms = 500

[[plugins]]
id = "api-key-guard"
path = "/usr/local/share/tracegate/plugins/api-key-guard.wasm"
hook = "before_request"
routes = ["payments"]
timeout_ms = 50
memory_limit_bytes = 16777216
fuel = 10000000
body_preview_bytes = 0
raw_headers = ["x-api-key"]
config = { header = "x-api-key", expected = "tracegate-demo-key", message = "missing or invalid API key" }

[[plugins]]
id = "header-normalizer"
path = "/usr/local/share/tracegate/plugins/header-normalizer.wasm"
hook = "before_request"
routes = ["payments"]
timeout_ms = 50
memory_limit_bytes = 16777216
fuel = 10000000
body_preview_bytes = 1024
raw_headers = []
config = { set_header = "x-tracegate-policy", set_value = "normalized" }

[[plugins]]
id = "timeout-normalizer"
path = "/usr/local/share/tracegate/plugins/header-normalizer.wasm"
hook = "before_request"
routes = ["plugin-timeout"]
timeout_ms = 1
memory_limit_bytes = 16777216
fuel = 1000000000
body_preview_bytes = 0
raw_headers = []
config = { spin_iterations = "100000000" }
```

Production mode validates:

- Admin token is present.
- External admin bind requires TLS.
- Capture has retention and total byte caps.
- Redaction lists are non-empty.
- Plugin timeout and memory limits are set.
- Upstream URLs are valid and do not target blocked local/admin addresses unless explicitly marked as demo mode.

## Storage Model

Core tables:

- `requests`: request ID, trace ID, route ID, method, path, query hash, status, latency, upstream, timestamps, slow/error flags.
- `request_headers`: redacted request headers attached to a request.
- `response_headers`: redacted response headers attached to a request.
- `captures`: bounded request body bytes, bounded response body bytes, content type, truncation flags, hashes.
- `plugin_decisions`: plugin ID, route ID, request ID, allow/deny, mutations, duration, timeout flag.
- `replay_runs`: replay ID, original request ID, target, status, latency, diff summary, timestamp.
- `route_snapshots`: route config hash used for historical debugging.

SQLite mode:

- Used for local, demo, and single-node deployments.
- Uses WAL mode.
- Stores captures in the database with strict byte caps.

PostgreSQL mode:

- Used for production multi-instance deployments.
- Uses the same repository interfaces and migration names.
- Stores bounded captures in SQL tables with retention cleanup.

## Replay Contract

Replay is explicit and auditable:

```bash
tracegate replay --id req_01JZ... --target http://localhost:4000 --confirm-side-effects
tracegate replay --last-failed --route payments --target http://localhost:4000 --confirm-side-effects
tracegate replay --where "status >= 500 and route = 'payments'" --target http://localhost:4000 --limit 10 --confirm-side-effects
```

Replay behavior:

- Preserves original method, path, query, and captured body.
- Rebuilds headers from the redacted stored set and the replay allowlist.
- Never reuses sensitive headers removed by redaction.
- Adds `x-tracegate-replay: true`.
- Adds `x-tracegate-original-request-id`.
- Generates a new request ID and new trace span for the replay run.
- Records replay status, latency, and response summary.
- Requires `--confirm-side-effects` for POST, PUT, PATCH, and DELETE.
- Requires a target that is not the original production upstream unless production replay is explicitly enabled in config.

## WASM Plugin Contract

TraceGate supports one stable hook through v1.0:

```text
before_request(request: RequestPolicyInput) -> RequestPolicyDecision
```

Plugin input:

- route ID
- request ID
- method
- path
- query parameters after redaction
- headers after redaction
- client address metadata
- bounded body preview when route config enables body preview

Plugin output:

- allow
- deny with status and message
- set header
- remove header
- emit structured policy event

Sandbox rules:

- Wasmtime Component Model with WIT contract.
- No filesystem access.
- No network access.
- No environment variable access.
- No inherited process handles.
- Timeout enforced per invocation.
- Memory limit enforced per plugin instance.
- Fuel or epoch interruption enabled.
- Plugin errors fail closed for protected routes.

First bundled example plugins:

- `api-key-guard`: denies requests missing a configured API key header.
- `header-normalizer`: adds or removes configured headers for upstream compatibility.
- `timeout-normalizer`: demo configuration of `header-normalizer` that intentionally exceeds its deadline and proves fail-closed timeout handling.

## Observability Contract

TraceGate emits:

- Structured JSON logs using `tracing`.
- Request spans with route ID, upstream, method, status, latency, request ID, trace ID, and replay flag.
- W3C `traceparent` propagation.
- OTLP traces to an OpenTelemetry Collector.
- Prometheus metrics on the admin port.

Required metrics:

- `tracegate_requests_total`
- `tracegate_request_duration_seconds`
- `tracegate_upstream_errors_total`
- `tracegate_captures_total`
- `tracegate_capture_dropped_total`
- `tracegate_replay_runs_total`
- `tracegate_plugin_decisions_total`
- `tracegate_plugin_duration_seconds`
- `tracegate_plugin_timeouts_total`
- `tracegate_plugin_errors_total`
- `tracegate_storage_retention_runs_total`

Docker Compose includes:

- TraceGate
- users backend
- payments backend
- OpenTelemetry Collector
- Jaeger or Tempo-compatible trace viewer
- Prometheus
- Grafana dashboard provisioning

The same Compose topology runs on the GCP VM. Local Compose and GCP Compose must stay aligned so the demo path and the live proof path exercise the same product behavior.

## Security and Robustness Rules

Core safety rules:

- Proxy routing fails closed when no route matches.
- Recording failure does not break proxy traffic; the request is marked `capture_dropped`.
- Plugin failure fails closed on routes that attach the plugin.
- Admin API never binds externally without TLS and bearer token auth.
- Sensitive headers and query parameters are redacted before logs, storage, and replay; plugin input receives only safe headers plus explicit per-plugin `raw_headers` allowlist entries.
- Body capture is bounded by route and global caps.
- Retention cleanup is part of the gateway process and exposed through metrics.
- Replay is never silent; every replay creates a `replay_runs` record.
- Timeouts, concurrency limits, and retries are per-route config.
- Graceful shutdown drains in-flight requests within `shutdown_grace_ms`.

Production hardening:

- Incoming TLS termination through rustls.
- Upstream HTTPS verification enabled.
- Health endpoint split: `/health/live` for process liveness and `/health/ready` for config, storage, and upstream readiness.
- Hot config reload with atomic swap and validation before activation.
- Config hash attached to request records.
- Backpressure on capture writer queue.
- Bounded cardinality labels for metrics.
- No secrets in logs, traces, metrics, replay records, or plugin events.

## CLI Contract

```bash
tracegate serve --config tracegate.toml
tracegate config check --config tracegate.toml
tracegate routes list --config tracegate.toml
tracegate requests list --failed --since 1h
tracegate requests show --id req_01JZ...
tracegate replay --id req_01JZ... --target http://localhost:4000 --confirm-side-effects
tracegate replay --last-failed --target http://localhost:4000 --confirm-side-effects
tracegate plugins inspect ./plugins/api_key_guard.wasm
tracegate storage migrate --config tracegate.toml
tracegate storage prune --config tracegate.toml
```

## Demo Contract

The local demo must be understandable in under five minutes:

```bash
docker compose up --build
curl http://localhost:8080/api/users/123
curl http://localhost:8080/api/payments/fail
curl http://localhost:8080/api/payments/charge
tracegate requests list --failed
tracegate replay --last-failed --target http://localhost:4000 --confirm-side-effects
```

The live GCP demo uses the same behavior through the deployed VM:

```bash
deployments/gcp/scripts/status.ps1
deployments/gcp/scripts/smoke.ps1
curl http://<gcp-external-ip>:8080/api/users/123
curl http://<gcp-external-ip>:8080/api/payments/fail
tracegate requests list --failed --server http://<gcp-external-ip>:9090
tracegate replay --last-failed --target http://<gcp-external-ip>:4000 --confirm-side-effects
gcloud compute ssh <tracegate-vm> --zone <zone>
sudo systemctl status tracegate
docker ps
docker logs tracegate --tail 200
```

The demo proves:

- `/api/users/*` routes to users backend.
- `/api/payments/*` routes to payments backend.
- Failed payment request is recorded.
- Trace is visible in the trace viewer.
- Metrics are visible in Prometheus.
- Replay sends the captured failing request to a safe target.
- WASM plugin blocks a request missing the configured API key.
- Console shows recent requests, route health, replay runs, and plugin decisions.
- The same proof works against the live GCP endpoint after the deployed GitHub commit is running on the VM.

## Implementation Milestones

Every milestone inherits this completion gate:

- The implementation is committed and pushed to GitHub.
- Terraform state is applied or verified for the milestone.
- The pushed commit is deployed to the GCP VM.
- The live GCP endpoint is called from outside the VM.
- The VM is inspected over SSH after the live call.
- Logs, storage, process status, and feature-specific evidence match the milestone expectation.
- A concise stage proof artifact records commands, commit SHA, endpoint, and observed results.

### v0.1 Gateway Foundation

Deliver:

- Cargo workspace.
- `tracegate serve --config tracegate.toml`.
- TOML config parsing and validation.
- Host and path-prefix route matching.
- Hyper-based reverse proxy.
- Request ID generation with UUID v7.
- Structured JSON logs.
- Per-route timeout and retry.
- Docker Compose with two fake backend services.
- `deployments/gcp/terraform` for the initial VM, firewall, service account, and budget-conscious variables.
- `deployments/gcp/scripts` for status, deploy, smoke, logs, and rollback commands.
- `deployments/gcp/systemd` for the TraceGate service unit.
- GCP Compose deployment for TraceGate and the two fake backend services.

Done when:

- `curl http://localhost:8080/api/users/123` reaches users backend.
- `curl http://localhost:8080/api/payments/fail` reaches payments backend.
- Logs include request ID, route ID, upstream, status, and latency.
- Integration tests cover route matching, proxy success, proxy error, and timeout.
- The same users and payments calls work against the GCP external endpoint.
- SSH log inspection proves the live VM routed both calls and emitted the expected structured logs.

### v0.2 Observability

Deliver:

- `tracing` span model.
- W3C trace context propagation.
- OTLP trace export.
- Prometheus metrics endpoint.
- Compose OpenTelemetry Collector and Prometheus.
- `/health/live` and `/health/ready`.
- GCP deployment of collector and Prometheus in the same VM Compose topology.

Done when:

- A request has matching request ID in logs and trace attributes.
- Trace viewer shows the TraceGate span and upstream timing.
- Prometheus exposes request count, latency, and upstream error metrics.
- Tests cover trace header propagation and metrics increments.
- Live GCP call produces a trace and Prometheus metric visible on the VM.
- SSH inspection verifies collector, TraceGate, and Prometheus logs for the live request.

### v0.3 Capture Store

Deliver:

- SQLx repository layer.
- SQLite migrations.
- Request metadata storage.
- Redacted header storage.
- Bounded request and response body capture.
- Slow/error marking.
- Retention cleanup job.
- `tracegate requests list` and `tracegate requests show`.
- GCP storage path and backup/inspect commands for the live SQLite database.

Done when:

- Failed and slow requests persist to SQLite.
- Sensitive headers and query params are absent from logs and storage.
- Large bodies are truncated with truncation flags.
- Retention cleanup deletes old capture records.
- Tests cover redaction, truncation, slow classification, and storage migration.
- Live GCP failed and slow requests persist on the VM.
- SSH inspection verifies redacted records, capture truncation flags, and retention state in the live database.

### v0.4 Replay Engine

Deliver:

- `tracegate replay --id`.
- `tracegate replay --last-failed`.
- Replay target validation.
- Side-effect confirmation guard.
- Replay result persistence.
- Replay metadata headers.
- Replay target service in local and GCP Compose.

Done when:

- A captured failed request replays successfully against the local target.
- Mutating methods require `--confirm-side-effects`.
- Replay records status, latency, and target.
- Tests cover GET replay, POST replay, missing body handling, target validation, and replay audit records.
- A captured failed request from the live GCP gateway replays successfully against the live GCP replay target.
- SSH inspection verifies the original request record, replay run record, and replay target logs.

### v0.5 WASM Policy Hook

Deliver:

- Wasmtime integration.
- WIT contract for `before_request`.
- Plugin config loading.
- Timeout and memory enforcement.
- Plugin allow/deny/header mutation decisions.
- `api-key-guard` example plugin.
- `header-normalizer` example plugin.
- Plugin decision metrics and storage.
- GCP deployment path for compiled WASM plugins.

Done when:

- Missing API key request is denied before upstream proxying.
- Valid API key request reaches upstream.
- Plugin timeout produces a deny decision on protected route.
- Plugin cannot access filesystem, network, or environment variables.
- Tests cover contract compatibility, deny path, mutation path, timeout, and sandbox restrictions.
- Live GCP request without the API key is denied before upstream proxying.
- SSH inspection verifies plugin load, plugin decision logs, plugin metrics, and absence of upstream traffic for the denied request.

### v0.6 Production Storage and Hardening

Delivered scope:

- PostgreSQL migrations and repository implementation.
- Production config mode.
- rustls incoming TLS.
- Upstream HTTPS verification.
- Admin bearer token enforcement.
- Hot config reload with atomic activation.
- Per-route concurrency limits.
- Passive upstream health and round-robin balancing.
- Capture writer backpressure.
- Production readiness validation.
- GCP production-mode config and TLS deployment on the VM.
- Production Compose keeps Postgres, admin, telemetry, and replay target internal to the VM network.

Done when:

- Same integration test suite passes against SQLite and PostgreSQL.
- Gateway refuses production mode without admin token, retention caps, and plugin limits.
- TLS smoke test passes.
- Hot reload updates route/plugin/redaction/capture state without dropping in-flight requests.
- Backpressure proof shows capture drops are recorded without breaking proxy traffic.
- Live GCP production-mode deployment refuses unsafe config and accepts the valid deployed config.
- SSH inspection verifies TLS listener, admin token enforcement, hot reload activation, and capture backpressure logs.

### v0.7 TraceGate Console and Full Demo

Deliver:

- Axum-served Console on admin port.
- Recent requests view.
- Request detail view.
- Replay run view.
- Route health view.
- Plugin decisions view.
- Telemetry status view.
- Grafana dashboard provisioning.
- Full Compose demo script.
- Full GCP demo script using the live external endpoint and SSH verification.

Done when:

- Console shows recent failed request.
- Console shows replay result for that request.
- Console shows plugin deny decision.
- Demo can be completed from a clean checkout with one Compose command and documented curls.
- The same demo can be completed against the GCP VM from the pushed commit.
- SSH inspection verifies console/admin logs and live telemetry service health.

### v1.0 Release Quality

Deliver:

- GitHub Actions CI for fmt, clippy, tests, coverage artifact, and benchmark smoke.
- Criterion benchmarks for proxy overhead, capture overhead, replay throughput, and plugin overhead.
- Load test script using generated HTTP traffic.
- Security review checklist.
- Terraform repeatability checks for the GCP deployment.
- GCP live deployment scripts with rollback.
- Docker image build.
- Architecture diagram.
- Release documentation: README, SECURITY, BENCHMARKS, plugin authoring guide.

Done when:

- CI passes on clean checkout.
- Benchmarks compare direct upstream, proxy only, proxy with capture, and proxy with plugin.
- Fresh Terraform-managed GCP deployment routes traffic and exposes metrics.
- Rollback script restores the previous known-good VM deployment.
- Documentation walks a new user from demo to replay to plugin policy.
- v1.0 tag can be cut from the repository with repeatable commands.

## Testing Strategy

Test layers:

- Unit tests for route matching, config validation, redaction, replay building, and plugin decisions.
- Integration tests with fake upstream services.
- Storage tests against SQLite and PostgreSQL.
- Replay tests for idempotent and mutating methods.
- Plugin contract tests using compiled WASM examples.
- Security tests for admin auth, redaction, blocked targets, and sandbox capabilities.
- Load tests for proxy latency, capture overhead, and plugin overhead.

Quality gates:

- `cargo fmt --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- SQLx migration checks for SQLite and PostgreSQL.
- Docker Compose smoke test.
- GCP Terraform validation.
- GCP live smoke test against the deployed commit.
- SSH log and storage inspection for the live feature under test.
- Benchmark smoke run.

## v1 Boundaries

TraceGate v1 includes:

- HTTP/1 and HTTP/2 API gateway proxying.
- Host and path-prefix routing.
- Multiple upstreams per route.
- Request IDs.
- Structured logs.
- OpenTelemetry traces.
- Prometheus metrics.
- SQLite and PostgreSQL storage.
- Failure and slow-request capture.
- Safe replay.
- One stable WASM `before_request` hook.
- CLI.
- Console.
- Docker Compose demo.
- Terraform-managed GCP Compute Engine deployment.
- Direct SSH deployment and verification scripts.
- systemd-managed live runtime.
- CI, tests, benchmarks, and release documentation.

TraceGate v1 excludes:

- eBPF.
- Service mesh control plane.
- Full plugin marketplace.
- Multi-tenant SaaS user management.
- Distributed Redis rate limiting.
- gRPC-specific proxy semantics beyond HTTP/2 transport.
- Managed GKE deployment.
- Cloud Run deployment.
- Cloud SQL production dependency.
- Replacing Envoy, Kong, NGINX, or Traefik as a universal gateway.

## Resume Bullet Target

> Built TraceGate, a Rust observability API gateway using Tokio, Hyper, Tower, Axum, OpenTelemetry, SQLx, SQLite, PostgreSQL, Wasmtime, Terraform, and GCP Compute Engine; implemented async reverse proxying, route matching, request tracing, failure capture, safe traffic replay, sandboxed WebAssembly request-policy plugins, production config validation, Dockerized local and live GCP demos, direct SSH operational verification, CI, integration tests, and benchmark documentation.
