param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [int] $BackpressureRequests = 1300,
    [switch] $SkipBackpressure,
    [switch] $IncludeRollback,
    [string] $CurrentImageTag = ""
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptRoot "..\..\..")
$scratch = Join-Path $repo "deployments\gcp\.scratch"
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

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

& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone -ReleaseQuality

$ip = (gcloud compute instances describe $VmName --zone $Zone --format="value(networkInterfaces[0].accessConfigs[0].natIP)").Trim()
if ([string]::IsNullOrWhiteSpace($ip)) {
    throw "no external IP found for $VmName"
}

$machineType = (gcloud compute instances describe $VmName --zone $Zone --format="value(machineType.basename())").Trim()
Write-Host "TraceGate v1 live verification target: $VmName $machineType https://<redacted-external-ip>:8080"

$caPath = Join-Path $scratch "tracegate-ca.crt"
Invoke-Checked {
    gcloud compute scp "${VmName}:/opt/tracegate/tls/ca.crt" $caPath --zone $Zone --strict-host-key-checking=no --quiet
} "download TraceGate CA"

$curlTlsArgs = @("--cacert", $caPath)
if ((curl.exe -V) -match "Schannel") {
    $curlTlsArgs += "--ssl-no-revoke"
}

function Invoke-PublicCall {
    param(
        [string] $Path,
        [int] $ExpectedStatus,
        [string[]] $Headers = @(),
        [string] $Method = "GET",
        [string] $Body = ""
    )

    $url = "https://${ip}:8080$Path"
    $bodyPath = New-TemporaryFile
    $headersPath = New-TemporaryFile
    $args = @("-sS") + $curlTlsArgs + @("-D", $headersPath, "-o", $bodyPath, "-w", "%{http_code}", "-X", $Method)
    foreach ($header in $Headers) {
        $args += @("-H", $header)
    }
    if (-not [string]::IsNullOrWhiteSpace($Body)) {
        $args += @("--data", $Body)
    }
    $args += $url

    $status = (curl.exe @args).Trim()
    $responseBody = Get-Content $bodyPath -Raw
    $responseHeaders = Get-Content $headersPath -Raw
    Remove-Item $bodyPath, $headersPath -Force

    Write-Host "$status $Method $url"
    if ($status -ne "$ExpectedStatus") {
        Write-Host $responseBody
        throw "expected HTTP $ExpectedStatus for $url, got $status"
    }

    $requestId = [regex]::Match($responseHeaders, '(?im)^x-request-id:\s*([0-9A-Fa-f-]+)\s*$').Groups[1].Value
    if ([string]::IsNullOrWhiteSpace($requestId)) {
        throw "no x-request-id returned for $url"
    }

    [pscustomobject]@{
        Status = [int] $status
        Body = $responseBody
        RequestId = $requestId
    }
}

function Invoke-Remote {
    param([string] $RemoteCommand)

    $encodedRemoteCommand = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($RemoteCommand))
    $remoteLauncher = "printf '%s' '$encodedRemoteCommand' | base64 -d | bash"
    gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command $remoteLauncher
    if ($LASTEXITCODE -ne 0) {
        throw "remote command failed"
    }
}

$users = Invoke-PublicCall "/api/users/123" 200
$denied = Invoke-PublicCall "/api/payments/fail" 403
$timeout = Invoke-PublicCall "/api/plugin-timeout/proof" 403
$failed = Invoke-PublicCall "/api/payments/fail" 500 @("x-api-key: tracegate-demo-key")
$slow = Invoke-PublicCall "/api/payments/slow?token=should-not-be-stored&visible=yes" 200 @("x-api-key: tracegate-demo-key")
$large = Invoke-PublicCall "/api/payments/large-fail?api_key=should-not-be-stored&visible=yes" 500 @("x-api-key: tracegate-demo-key", "content-type: application/json", "authorization: Bearer should-not-be-stored", "x-remove-me: remove-this") "POST" '{"note":"v1 live verification capture proof"}'

$remoteTemplate = @'
set -euo pipefail
cd /opt/tracegate
. /opt/tracegate/secrets.env
AUTH_HEADER="Authorization: Bearer ${TRACEGATE_ADMIN_TOKEN}"

curl_admin() {
  docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS -H "${AUTH_HEADER}" "$@"
}

curl_internal() {
  docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS "$@"
}

echo "== runtime =="
cat current.env
docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
sudo systemctl --no-pager --full status tracegate
docker exec tracegate tracegate config check --config /etc/tracegate/tracegate.toml
grep -F 'https://users-service:3443' /opt/tracegate/tracegate.toml
grep -F 'https://payments-service:4443' /opt/tracegate/tracegate.toml

echo "== public-request log readback =="
docker logs tracegate --tail 300 | grep -F '__USERS_REQUEST_ID__'
docker logs tracegate --tail 300 | grep -F '__DENIED_REQUEST_ID__'
docker logs tracegate --tail 300 | grep -F '__LARGE_REQUEST_ID__'

echo "== admin health and metrics =="
curl_admin http://tracegate:9090/health/live
curl_admin http://tracegate:9090/health/ready
curl_admin http://tracegate:9090/metrics | tee /tmp/tracegate-v1-metrics.txt
for series in \
  tracegate_requests_total \
  tracegate_request_duration_seconds \
  tracegate_upstream_errors_total \
  tracegate_captures_total \
  tracegate_capture_dropped_total \
  tracegate_storage_retention_runs_total \
  tracegate_replay_runs_total \
  tracegate_plugin_decisions_total \
  tracegate_plugin_duration_seconds \
  tracegate_plugin_timeouts_total \
  tracegate_plugin_errors_total
do
  grep -F "${series}" /tmp/tracegate-v1-metrics.txt
done

echo "== hot reload =="
curl_admin -X POST http://tracegate:9090/admin/reload | tee /tmp/tracegate-v1-reload.json
grep -F '"status":"reloaded"' /tmp/tracegate-v1-reload.json

echo "== console and admin APIs =="
curl_internal http://tracegate:9090/console/ | grep -F 'TraceGate Console'
curl_admin http://tracegate:9090/admin/api/overview | tee /tmp/tracegate-v1-overview.json
grep -F '"mode":"production"' /tmp/tracegate-v1-overview.json
grep -F '"storage_ready":true' /tmp/tracegate-v1-overview.json
grep -F '"route_count":3' /tmp/tracegate-v1-overview.json
grep -F '"plugin_count":3' /tmp/tracegate-v1-overview.json

curl_admin 'http://tracegate:9090/admin/api/requests?failed=true&limit=30' | tee /tmp/tracegate-v1-requests.json
grep -F '__LARGE_REQUEST_ID__' /tmp/tracegate-v1-requests.json

curl_admin 'http://tracegate:9090/admin/api/requests/__LARGE_REQUEST_ID__' | tee /tmp/tracegate-v1-large-detail.json
grep -F '"request_id":"__LARGE_REQUEST_ID__"' /tmp/tracegate-v1-large-detail.json
grep -F '"status":500' /tmp/tracegate-v1-large-detail.json
grep -F '"capture":' /tmp/tracegate-v1-large-detail.json
grep -F '"plugin_id":"api-key-guard"' /tmp/tracegate-v1-large-detail.json
grep -F '"plugin_id":"header-normalizer"' /tmp/tracegate-v1-large-detail.json
if grep -E 'should-not-be-stored|api_key=should-not-be-stored|token=should-not-be-stored' /tmp/tracegate-v1-large-detail.json; then
  echo 'sensitive value leaked through request detail API' >&2
  exit 1
fi

curl_admin 'http://tracegate:9090/admin/api/requests/__DENIED_REQUEST_ID__' | tee /tmp/tracegate-v1-deny-detail.json
grep -F '"plugin_id":"api-key-guard"' /tmp/tracegate-v1-deny-detail.json
grep -F '"action":"deny"' /tmp/tracegate-v1-deny-detail.json
if docker logs tracegate-payments-service --tail 300 | grep -F '__DENIED_REQUEST_ID__'; then
  echo 'denied request reached payments upstream' >&2
  exit 1
fi

curl_admin http://tracegate:9090/admin/api/routes | tee /tmp/tracegate-v1-routes.json
grep -F '"id":"users"' /tmp/tracegate-v1-routes.json
grep -F '"id":"payments"' /tmp/tracegate-v1-routes.json
grep -F '"upstreams":[' /tmp/tracegate-v1-routes.json

curl_admin http://tracegate:9090/admin/api/plugins | tee /tmp/tracegate-v1-plugins.json
grep -F '"id":"api-key-guard"' /tmp/tracegate-v1-plugins.json
grep -F '"id":"header-normalizer"' /tmp/tracegate-v1-plugins.json
grep -F '"id":"timeout-normalizer"' /tmp/tracegate-v1-plugins.json
grep -F '"config_keys":[' /tmp/tracegate-v1-plugins.json
if grep -F 'tracegate-demo-key' /tmp/tracegate-v1-plugins.json; then
  echo 'plugin API leaked config value' >&2
  exit 1
fi

curl_admin http://tracegate:9090/admin/api/telemetry | tee /tmp/tracegate-v1-telemetry.json
grep -F '"admin_ready":true' /tmp/tracegate-v1-telemetry.json
grep -F '"storage_ready":true' /tmp/tracegate-v1-telemetry.json
grep -F '"name":"tracegate_requests_total","present":true' /tmp/tracegate-v1-telemetry.json

echo "== CLI storage and replay =="
docker exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --failed --limit 20
docker exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --slow --limit 20
docker exec tracegate tracegate requests show --config /etc/tracegate/tracegate.toml --id __LARGE_REQUEST_ID__ | tee /tmp/tracegate-v1-request-show.txt
grep -F 'request_body_truncated:' /tmp/tracegate-v1-request-show.txt
grep -F 'plugin_id: api-key-guard' /tmp/tracegate-v1-request-show.txt
docker exec tracegate tracegate storage prune --config /etc/tracegate/tracegate.toml --json | tee /tmp/tracegate-v1-prune.json
docker exec tracegate tracegate replay --config /etc/tracegate/tracegate.toml --last-failed --target http://replay-target:4000 --confirm-side-effects --json | tee /tmp/tracegate-v1-replay.json
grep -F '"status": 200' /tmp/tracegate-v1-replay.json
docker logs tracegate-replay-target --tail 120

echo "== plugin contract =="
docker exec tracegate tracegate plugins inspect /usr/local/share/tracegate/plugins/api-key-guard.wasm --json | tee /tmp/tracegate-v1-api-key-plugin.json
grep -F '"compatible": true' /tmp/tracegate-v1-api-key-plugin.json
docker exec tracegate tracegate plugins inspect /usr/local/share/tracegate/plugins/header-normalizer.wasm --json | tee /tmp/tracegate-v1-header-plugin.json
grep -F '"compatible": true' /tmp/tracegate-v1-header-plugin.json

echo "== observability =="
curl_internal 'http://prometheus:9090/api/v1/query?query=tracegate_requests_total' | tee /tmp/tracegate-v1-prometheus.json
grep -F '"status":"success"' /tmp/tracegate-v1-prometheus.json
curl_internal 'http://jaeger:16686/api/services' | tee /tmp/tracegate-v1-jaeger-services.json
grep -F 'tracegate' /tmp/tracegate-v1-jaeger-services.json
curl_internal 'http://jaeger:16686/api/traces?service=tracegate&limit=5' | tee /tmp/tracegate-v1-jaeger-traces.json
grep -F '"traceID"' /tmp/tracegate-v1-jaeger-traces.json

echo "== grafana =="
curl_internal http://grafana:3000/api/health | tee /tmp/tracegate-v1-grafana-health.json
grep -F '"database":"ok"' /tmp/tracegate-v1-grafana-health.json
curl_internal 'http://grafana:3000/api/search?query=TraceGate%20Overview' | tee /tmp/tracegate-v1-grafana-search.json
grep -F 'tracegate-overview' /tmp/tracegate-v1-grafana-search.json

echo "== storage counts =="
docker exec tracegate-postgres sh -c 'psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -tAc "select count(*) from requests; select count(*) from captures; select count(*) from plugin_decisions; select count(*) from replay_runs;"'

echo "== container log safety =="
for name in tracegate tracegate-postgres tracegate-otel-collector tracegate-prometheus tracegate-jaeger tracegate-grafana; do
  docker logs "$name" --tail 300 > "/tmp/${name}-v1-tail.log" 2>&1 || true
  if grep -Ei 'panic|thread .* panicked|fatal|segmentation fault' "/tmp/${name}-v1-tail.log"; then
    echo "fatal log marker found in $name" >&2
    exit 1
  fi
done
'@

$remoteCommand = $remoteTemplate.Replace("__USERS_REQUEST_ID__", $users.RequestId)
$remoteCommand = $remoteCommand.Replace("__DENIED_REQUEST_ID__", $denied.RequestId)
$remoteCommand = $remoteCommand.Replace("__LARGE_REQUEST_ID__", $large.RequestId)
Invoke-Remote $remoteCommand

if (-not $SkipBackpressure) {
    Write-Host "Starting v1 backpressure proof with $BackpressureRequests HTTPS requests"
    $beforeCommand = @'
set -euo pipefail
cd /opt/tracegate
. /opt/tracegate/secrets.env
docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS -H "Authorization: Bearer ${TRACEGATE_ADMIN_TOKEN}" http://tracegate:9090/metrics | awk '/^tracegate_capture_dropped_total / {print $2; found=1} END {if (!found) print 0}'
'@
    $beforePath = Join-Path $scratch "v1-capture-dropped-before.txt"
    $encodedBefore = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($beforeCommand))
    gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command "printf '%s' '$encodedBefore' | base64 -d | bash" | Tee-Object -FilePath $beforePath
    $captureDroppedBefore = [double]((Get-Content $beforePath | Select-Object -Last 1).Trim())

    $lockCommand = @'
set -euo pipefail
docker exec -d tracegate-postgres sh -c 'psql -U "$POSTGRES_USER" -d "$POSTGRES_DB" -c "BEGIN; LOCK TABLE requests IN ACCESS EXCLUSIVE MODE; SELECT pg_sleep(120); COMMIT;"'
'@
    Invoke-Remote $lockCommand
    Start-Sleep -Seconds 3

    $statusCounts = @{}
    for ($i = 1; $i -le $BackpressureRequests; $i++) {
        $url = "https://${ip}:8080/api/payments/large-fail?visible=yes&seq=$i"
        $tmp = New-TemporaryFile
        $status = (curl.exe -sS @curlTlsArgs --max-time 10 -o $tmp -w "%{http_code}" -X POST -H "x-api-key: tracegate-demo-key" -H "content-type: application/json" --data '{"note":"v1 backpressure"}' $url).Trim()
        Remove-Item $tmp -Force
        if ([string]::IsNullOrWhiteSpace($status)) {
            $status = "curl-error-$LASTEXITCODE"
        }
        if (-not $statusCounts.ContainsKey($status)) {
            $statusCounts[$status] = 0
        }
        $statusCounts[$status] += 1
        if (($i % 100) -eq 0) {
            Write-Host "backpressure_requests=$i"
        }
    }

    Write-Host "Backpressure status counts:"
    $statusCounts.GetEnumerator() | Sort-Object Name | ForEach-Object { Write-Host "  $($_.Name)=$($_.Value)" }
    Start-Sleep -Seconds 10

    $afterPath = Join-Path $scratch "v1-capture-dropped-after.txt"
    gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command "printf '%s' '$encodedBefore' | base64 -d | bash" | Tee-Object -FilePath $afterPath
    $captureDroppedAfter = [double]((Get-Content $afterPath | Select-Object -Last 1).Trim())
    Write-Host "capture_dropped_before=$captureDroppedBefore"
    Write-Host "capture_dropped_after=$captureDroppedAfter"
    if ($captureDroppedAfter -le $captureDroppedBefore) {
        throw "backpressure proof did not increase tracegate_capture_dropped_total"
    }
}

& "$scriptRoot\backup-storage.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName -OutputName "tracegate-v1-release-quality-backup.sql" -ReleaseQuality

if ($IncludeRollback) {
    if ([string]::IsNullOrWhiteSpace($CurrentImageTag)) {
        $CurrentImageTag = (git -C $repo rev-parse --short=12 HEAD).Trim()
    }
    Write-Host "Running rollback proof, then restoring tracegate:$CurrentImageTag"
    & "$scriptRoot\rollback.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName -ReleaseQuality
    & "$scriptRoot\smoke.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName -ReleaseQuality
    & "$scriptRoot\deploy.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName -ImageTag $CurrentImageTag -ReleaseQuality
    & "$scriptRoot\smoke.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName -ReleaseQuality
}

Write-Host "TraceGate v1 live verification passed"
Write-Host "endpoint=https://<redacted-external-ip>:8080"
Write-Host "machine_type=$machineType"
Write-Host "users_request=$($users.RequestId)"
Write-Host "denied_request=$($denied.RequestId)"
Write-Host "timeout_request=$($timeout.RequestId)"
Write-Host "failed_request=$($failed.RequestId)"
Write-Host "slow_request=$($slow.RequestId)"
Write-Host "large_failed_request=$($large.RequestId)"
