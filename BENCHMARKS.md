# TraceGate Benchmarks and Stress Gate

TraceGate v1 uses two performance gates:

- Criterion benchmark smoke for repeatable local and CI regression checks.
- GCP release-quality stress testing from a separate load generator VM.

## Criterion

```powershell
scripts/cargo.ps1 bench -p tracegate-proxy --bench release_overhead -- --sample-size 10 --measurement-time 1 --warm-up-time 1
```

The benchmark compares:

- direct upstream
- TraceGate proxy only
- TraceGate proxy with capture
- TraceGate proxy with a WASM policy plugin

The benchmark is a release smoke gate, not a formal capacity claim. It catches large overhead regressions while keeping CI runtime bounded.

## GCP Stress Gate

The v1 stress gate uses:

- app VM: `n2-standard-16`
- load generator VM: `n2-standard-8`
- scenario: `tests/load/k6/v1-stress.js`
- default balanced duration: feature spikes plus a one-hour mixed soak

Run:

```powershell
deployments/gcp/scripts/v1-infra.ps1 -Action LoadGenUp -AutoApprove
deployments/gcp/scripts/v1-stress.ps1 -Duration 1h
```

The k6 scenario drives routing, plugin deny, plugin timeout, failed capture, slow capture, and large-body capture paths. The gate fails on wrong status codes, missing `x-request-id`, latency threshold failures, app container restarts, missing post-load metrics, fatal log markers, or failed storage readback.

After the release gate:

```powershell
deployments/gcp/scripts/v1-infra.ps1 -Action Cleanup -AutoApprove
```

Cleanup must delete the load generator and resize the app VM back to `e2-micro`.
