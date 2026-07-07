param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [switch] $ReleaseQuality
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptRoot "..\..\..")
$scratch = Join-Path $repo "deployments\gcp\.scratch"
New-Item -ItemType Directory -Force -Path $scratch | Out-Null
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone -ReleaseQuality:$ReleaseQuality

$ip = (gcloud compute instances describe $VmName --zone $Zone --format="value(networkInterfaces[0].accessConfigs[0].natIP)").Trim()
if ([string]::IsNullOrWhiteSpace($ip)) {
    throw "no external IP found for $VmName"
}

$caPath = Join-Path $scratch "tracegate-ca.crt"
gcloud compute scp "${VmName}:/opt/tracegate/tls/ca.crt" $caPath --zone $Zone --strict-host-key-checking=no --quiet
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
    Write-Host $responseBody
    if ($status -ne "$ExpectedStatus") {
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

$users = Invoke-PublicCall "/api/users/123" 200
$denied = Invoke-PublicCall "/api/payments/fail" 403
$timeout = Invoke-PublicCall "/api/plugin-timeout/proof" 403
$failed = Invoke-PublicCall "/api/payments/fail" 500 @("x-api-key: tracegate-demo-key")
$slow = Invoke-PublicCall "/api/payments/slow?token=should-not-be-stored&visible=yes" 200 @("x-api-key: tracegate-demo-key")
$large = Invoke-PublicCall "/api/payments/large-fail?api_key=should-not-be-stored&visible=yes" 500 @("x-api-key: tracegate-demo-key", "content-type: application/json", "authorization: Bearer should-not-be-stored") "POST" '{"card":"4242424242424242","note":"large request body for capture proof"}'

$requestIds = @($users.RequestId, $denied.RequestId, $timeout.RequestId, $failed.RequestId, $slow.RequestId, $large.RequestId)
foreach ($requestId in $requestIds) {
    if ($requestId -notmatch '^[0-9A-Fa-f-]+$') {
        throw "unexpected request id: $requestId"
    }
}

$remoteCommandTemplate = @'
set -euo pipefail
cd /opt/tracegate
. /opt/tracegate/secrets.env
AUTH_HEADER="Authorization: Bearer ${TRACEGATE_ADMIN_TOKEN}"

docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
docker exec tracegate tracegate replay --config /etc/tracegate/tracegate.toml --last-failed --target http://replay-target:4000 --confirm-side-effects

curl_admin() {
  docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS -H "${AUTH_HEADER}" "http://tracegate:9090$1"
}

curl_internal() {
  docker run --rm --network tracegate_default curlimages/curl:8.10.1 -fsS "$1"
}

curl_internal http://tracegate:9090/console/ | grep -F 'TraceGate Console'
curl_admin /admin/api/overview | tee /tmp/tracegate-overview.json
grep -F '"route_count":3' /tmp/tracegate-overview.json
grep -F '"plugin_count":3' /tmp/tracegate-overview.json
grep -F '"storage_ready":true' /tmp/tracegate-overview.json

curl_admin '/admin/api/requests?failed=true&limit=20' | tee /tmp/tracegate-requests.json
grep -F '__LARGE_REQUEST_ID__' /tmp/tracegate-requests.json

curl_admin '/admin/api/requests/__LARGE_REQUEST_ID__' | tee /tmp/tracegate-large-detail.json
grep -F '"replay_runs":[' /tmp/tracegate-large-detail.json
grep -F '"status":200' /tmp/tracegate-large-detail.json

curl_admin '/admin/api/requests/__DENIED_REQUEST_ID__' | tee /tmp/tracegate-deny-detail.json
grep -F '"plugin_id":"api-key-guard"' /tmp/tracegate-deny-detail.json
grep -F '"action":"deny"' /tmp/tracegate-deny-detail.json

curl_admin /admin/api/routes | tee /tmp/tracegate-routes.json
grep -F '"id":"payments"' /tmp/tracegate-routes.json
grep -F '"upstreams":[' /tmp/tracegate-routes.json

curl_admin /admin/api/plugins | tee /tmp/tracegate-plugins.json
grep -F '"id":"api-key-guard"' /tmp/tracegate-plugins.json
grep -F '"config_keys":[' /tmp/tracegate-plugins.json
if grep -F 'tracegate-demo-key' /tmp/tracegate-plugins.json; then
  echo 'plugin API leaked config value' >&2
  exit 1
fi

curl_admin /admin/api/telemetry | tee /tmp/tracegate-telemetry.json
grep -F '"name":"tracegate_requests_total","present":true' /tmp/tracegate-telemetry.json
grep -F '"name":"tracegate_plugin_decisions_total","present":true' /tmp/tracegate-telemetry.json
grep -F '"name":"tracegate_plugin_duration_seconds","present":true' /tmp/tracegate-telemetry.json

curl_internal http://grafana:3000/api/health | tee /tmp/tracegate-grafana-health.json
grep -F '"database":"ok"' /tmp/tracegate-grafana-health.json
curl_internal 'http://grafana:3000/api/search?query=TraceGate%20Overview' | tee /tmp/tracegate-grafana-search.json
grep -F 'tracegate-overview' /tmp/tracegate-grafana-search.json

docker logs tracegate --tail 80
docker logs tracegate-grafana --tail 80
docker logs tracegate-prometheus --tail 80
'@

$remoteCommand = $remoteCommandTemplate.Replace("__LARGE_REQUEST_ID__", $large.RequestId)
$remoteCommand = $remoteCommand.Replace("__DENIED_REQUEST_ID__", $denied.RequestId)

$encodedRemoteCommand = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($remoteCommand))
$remoteLauncher = "printf '%s' '$encodedRemoteCommand' | base64 -d | bash"
gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command $remoteLauncher

Write-Host "TraceGate v0.7 GCP full demo passed"
Write-Host "endpoint=https://<redacted-external-ip>:8080"
Write-Host "users_request=$($users.RequestId)"
Write-Host "denied_request=$($denied.RequestId)"
Write-Host "timeout_request=$($timeout.RequestId)"
Write-Host "failed_request=$($failed.RequestId)"
Write-Host "slow_request=$($slow.RequestId)"
Write-Host "large_failed_request=$($large.RequestId)"
