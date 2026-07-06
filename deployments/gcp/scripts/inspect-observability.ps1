param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm"
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

$remoteCommand = @'
set -euo pipefail
cd /opt/tracegate
. /opt/tracegate/secrets.env
AUTH_HEADER="Authorization: Bearer ${TRACEGATE_ADMIN_TOKEN}"
docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
docker logs tracegate --tail 80
docker logs tracegate-otel-collector --tail 80
docker logs tracegate-prometheus --tail 80
docker logs tracegate-jaeger --tail 80

docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS -H "$AUTH_HEADER" http://tracegate:9090/health/live
docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS -H "$AUTH_HEADER" http://tracegate:9090/health/ready
docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS -H "$AUTH_HEADER" http://tracegate:9090/metrics | grep -E 'tracegate_requests_total|tracegate_request_duration_seconds|tracegate_upstream_errors_total|tracegate_plugin_decisions_total|tracegate_plugin_duration_seconds|tracegate_plugin_timeouts_total|tracegate_plugin_errors_total'
docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS 'http://prometheus:9090/api/v1/query?query=tracegate_requests_total'
docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS 'http://jaeger:16686/api/services' | grep tracegate
docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS 'http://jaeger:16686/api/traces?service=tracegate&limit=5'
'@

gcloud compute ssh $VmName --zone $Zone --command $remoteCommand
