# TraceGate Security

TraceGate v1 is a single-node observability gateway for HTTP debugging and replay. It is not a multi-tenant SaaS control plane.

## Supported Security Boundaries

- Public exposure is limited to the data-plane HTTPS listener on GCP.
- Admin, Console APIs, Prometheus, Jaeger, Grafana, PostgreSQL, and the replay target stay internal to the VM network and are inspected through SSH.
- Admin endpoints require a bearer token when configured.
- Production mode refuses unsafe admin, retention, capture, TLS, upstream TLS, and plugin-limit configuration.
- Request headers and query parameters listed in `tracegate.toml` are redacted before logs, storage, replay records, and plugin input.
- WASM policy plugins run without filesystem, network, environment-variable, or inherited-handle access.
- Plugins are bounded by timeout, fuel, memory, and explicit per-plugin raw-header allowlists.
- Replay requires an explicit target and mutating methods require `--confirm-side-effects`.

## Release Checklist

Before tagging v1.0:

- Run `scripts/cargo.ps1 fmt --check`.
- Run `scripts/cargo.ps1 clippy --workspace --all-targets -- -D warnings`.
- Run `scripts/cargo.ps1 test --workspace`.
- Run PostgreSQL-backed storage tests with `TRACEGATE_TEST_POSTGRES_URL`.
- Run `scripts/full-demo.ps1`.
- Run `deployments/gcp/scripts/v1-live-verify.ps1`.
- Run `deployments/gcp/scripts/v1-stress.ps1` from the temporary load generator.
- Confirm `deployments/gcp/scripts/v1-infra.ps1 -Action Cleanup` leaves only the steady-state `e2-micro` TraceGate VM.

## Reporting

This is a portfolio/demo project. Do not send secrets, production traffic, customer data, or regulated data through a public demo deployment. Report issues through the GitHub repository.
