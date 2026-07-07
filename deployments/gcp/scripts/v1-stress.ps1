param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $LoadGeneratorName = "tracegate-v1-loadgen",
    [string] $Duration = "1h",
    [string] $SpikeDuration = "5m",
    [int] $SpikeVus = 80,
    [int] $SoakVus = 40
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptRoot "..\..\..")
$k6Dir = Join-Path $repo "tests\load\k6"

function Invoke-Checked {
    param(
        [scriptblock] $Command,
        [string] $Description
    )

    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

function Invoke-RemoteApp {
    param([string] $RemoteCommand)

    $encodedRemoteCommand = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($RemoteCommand))
    $remoteLauncher = "printf '%s' '$encodedRemoteCommand' | base64 -d | bash"
    Invoke-Checked {
        gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command $remoteLauncher
    } "app VM remote command"
}

& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone -ReleaseQuality -LoadGeneratorEnabled

$appIp = (gcloud compute instances describe $VmName --zone $Zone --format="value(networkInterfaces[0].accessConfigs[0].natIP)").Trim()
if ([string]::IsNullOrWhiteSpace($appIp)) {
    throw "no external IP found for $VmName"
}

$appMachineType = (gcloud compute instances describe $VmName --zone $Zone --format="value(machineType.basename())").Trim()
$loadGenMachineType = (gcloud compute instances describe $LoadGeneratorName --zone $Zone --format="value(machineType.basename())").Trim()
if ($appMachineType -ne "n2-standard-16") {
    throw "stress gate requires app VM n2-standard-16, got $appMachineType"
}
if ($loadGenMachineType -ne "n2-standard-8") {
    throw "stress gate requires load generator n2-standard-8, got $loadGenMachineType"
}

Invoke-Checked {
    gcloud compute ssh $LoadGeneratorName --zone $Zone --strict-host-key-checking=no --quiet --command "mkdir -p /opt/tracegate-load/k6 /opt/tracegate-load/results && docker --version"
} "load generator readiness"

Invoke-Checked {
    gcloud compute scp (Join-Path $k6Dir "v1-stress.js") "${LoadGeneratorName}:/opt/tracegate-load/k6/v1-stress.js" --zone $Zone --strict-host-key-checking=no --quiet
} "upload k6 scenario"

$remoteStress = @"
set -euo pipefail
mkdir -p /opt/tracegate-load/results
docker pull grafana/k6:0.54.0
docker run --rm -e BASE_URL=https://${appIp}:8080 -e API_KEY=tracegate-demo-key -e STRESS_DURATION=${Duration} -e SPIKE_DURATION=${SpikeDuration} -e SPIKE_VUS=${SpikeVus} -e SOAK_VUS=${SoakVus} -v /opt/tracegate-load/k6:/scripts:ro -v /opt/tracegate-load/results:/results grafana/k6:0.54.0 run --summary-export /results/v1-stress-summary.json /scripts/v1-stress.js
ls -lah /opt/tracegate-load/results
"@

$encodedStress = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($remoteStress))
$stressLauncher = "printf '%s' '$encodedStress' | base64 -d | bash"
Invoke-Checked {
    gcloud compute ssh $LoadGeneratorName --zone $Zone --strict-host-key-checking=no --quiet --command $stressLauncher
} "v1 k6 stress run"

Invoke-Checked {
    gcloud compute scp "${LoadGeneratorName}:/opt/tracegate-load/results/v1-stress-summary.json" (Join-Path $repo "deployments\gcp\.scratch\v1-stress-summary.json") --zone $Zone --strict-host-key-checking=no --quiet
} "download k6 summary"

$postLoadReadback = @'
set -euo pipefail
cd /opt/tracegate
. /opt/tracegate/secrets.env
AUTH_HEADER="Authorization: Bearer ${TRACEGATE_ADMIN_TOKEN}"

curl_admin() {
  docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS -H "${AUTH_HEADER}" "$@"
}

docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
for name in tracegate tracegate-postgres tracegate-otel-collector tracegate-prometheus tracegate-jaeger tracegate-grafana; do
  restart_count="$(docker inspect -f '{{.RestartCount}}' "$name")"
  echo "restart_count ${name}=${restart_count}"
  if [ "$restart_count" != "0" ]; then
    echo "container restarted during stress: $name" >&2
    exit 1
  fi
done

curl_admin http://tracegate:9090/health/ready
curl_admin http://tracegate:9090/metrics | tee /tmp/tracegate-v1-post-stress-metrics.txt
for series in tracegate_requests_total tracegate_captures_total tracegate_capture_dropped_total tracegate_replay_runs_total tracegate_plugin_decisions_total tracegate_plugin_timeouts_total tracegate_upstream_errors_total; do
  grep -F "$series" /tmp/tracegate-v1-post-stress-metrics.txt
done

docker exec tracegate-postgres sh -c 'psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAc "select count(*) as requests from requests; select count(*) as captures from captures; select count(*) as plugin_decisions from plugin_decisions;"'
docker logs tracegate --tail 500 > /tmp/tracegate-v1-post-stress.log 2>&1 || true
if grep -Ei 'panic|thread .* panicked|fatal|segmentation fault' /tmp/tracegate-v1-post-stress.log; then
  echo 'fatal TraceGate log marker after stress' >&2
  exit 1
fi
'@

Invoke-RemoteApp $postLoadReadback

Write-Host "TraceGate v1 stress gate passed"
Write-Host "app_vm=$VmName machine=$appMachineType endpoint=https://<redacted-external-ip>:8080"
Write-Host "load_generator=$LoadGeneratorName machine=$loadGenMachineType"
Write-Host "duration=$Duration spike_duration=$SpikeDuration spike_vus=$SpikeVus soak_vus=$SoakVus"
