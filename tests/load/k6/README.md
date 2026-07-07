# TraceGate v1 k6 Stress Gate

This scenario is intended to run from the temporary `tracegate-v1-loadgen` VM against the release-quality app VM.

```powershell
deployments/gcp/scripts/v1-infra.ps1 -Action LoadGenUp -AutoApprove
deployments/gcp/scripts/v1-stress.ps1 -Duration 1h
```

The script drives routing, plugin deny, plugin timeout, failed capture, slow capture, and large-body capture paths. It treats the expected `403` and `500` responses as successful feature outcomes and fails on missing `x-request-id`, wrong status codes, high latency thresholds, or failed post-load readback.
