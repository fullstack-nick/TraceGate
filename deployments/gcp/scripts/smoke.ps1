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
$curlTimeoutSeconds = 120
$script:replayRequestId = ""

function Get-HeaderRequestId($HeaderPath) {
    foreach ($line in (Get-Content $HeaderPath)) {
        if ($line -match '^x-request-id:\s*(.+)$') {
            return $Matches[1].Trim()
        }
    }
    return ""
}

function Update-ReplayCandidate($ExpectedStatus, $RequestId) {
    if ($ExpectedStatus -ge 500 -and -not [string]::IsNullOrWhiteSpace($RequestId)) {
        $script:replayRequestId = $RequestId
    }
}

function Invoke-Smoke($Path, $ExpectedStatus) {
    $url = "https://${ip}:8080$Path"
    $tmp = New-TemporaryFile
    $headers = New-TemporaryFile
    $status = (curl.exe -sS --max-time $curlTimeoutSeconds @curlTlsArgs -D $headers -o $tmp -w "%{http_code}" $url).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "curl failed for $url with exit code $LASTEXITCODE"
    }
    $requestId = Get-HeaderRequestId $headers
    $body = Get-Content $tmp -Raw
    Remove-Item $tmp, $headers -Force
    Write-Host "$status $url"
    if (-not [string]::IsNullOrWhiteSpace($requestId)) {
        Write-Host "x-request-id: $requestId"
    }
    Write-Host $body
    if ($status -ne "$ExpectedStatus") {
        throw "expected HTTP $ExpectedStatus for $url, got $status"
    }
    Update-ReplayCandidate $ExpectedStatus $requestId
}

function Invoke-KeyedSmoke($Path, $ExpectedStatus) {
    $url = "https://${ip}:8080$Path"
    $tmp = New-TemporaryFile
    $headers = New-TemporaryFile
    $status = (curl.exe -sS --max-time $curlTimeoutSeconds @curlTlsArgs -D $headers -o $tmp -w "%{http_code}" -H "x-api-key: tracegate-demo-key" $url).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "curl failed for $url with exit code $LASTEXITCODE"
    }
    $requestId = Get-HeaderRequestId $headers
    $body = Get-Content $tmp -Raw
    Remove-Item $tmp, $headers -Force
    Write-Host "$status $url"
    if (-not [string]::IsNullOrWhiteSpace($requestId)) {
        Write-Host "x-request-id: $requestId"
    }
    Write-Host $body
    if ($status -ne "$ExpectedStatus") {
        throw "expected HTTP $ExpectedStatus for $url, got $status"
    }
    Update-ReplayCandidate $ExpectedStatus $requestId
}

function Invoke-PostSmoke($Path, $ExpectedStatus, $Body) {
    $url = "https://${ip}:8080$Path"
    $tmp = New-TemporaryFile
    $headers = New-TemporaryFile
    $status = (curl.exe -sS --max-time $curlTimeoutSeconds @curlTlsArgs -D $headers -o $tmp -w "%{http_code}" -X POST -H "content-type: application/json" -H "authorization: Bearer should-not-be-stored" -H "x-api-key: tracegate-demo-key" --data $Body $url).Trim()
    if ($LASTEXITCODE -ne 0) {
        throw "curl failed for $url with exit code $LASTEXITCODE"
    }
    $requestId = Get-HeaderRequestId $headers
    $body = Get-Content $tmp -Raw
    Remove-Item $tmp, $headers -Force
    Write-Host "$status POST $url"
    if (-not [string]::IsNullOrWhiteSpace($requestId)) {
        Write-Host "x-request-id: $requestId"
    }
    Write-Host $body
    if ($status -ne "$ExpectedStatus") {
        throw "expected HTTP $ExpectedStatus for $url, got $status"
    }
    Update-ReplayCandidate $ExpectedStatus $requestId
}

Invoke-Smoke "/api/users/123" 200
Invoke-Smoke "/api/payments/fail" 403
Invoke-Smoke "/api/plugin-timeout/proof" 403
Invoke-KeyedSmoke "/api/payments/fail" 500
Invoke-KeyedSmoke "/api/payments/slow?token=should-not-be-stored&visible=yes" 200
Invoke-PostSmoke "/api/payments/large-fail?api_key=should-not-be-stored&visible=yes" 500 '{"card":"4242424242424242","note":"large request body for capture truncation proof"}'

$replayRequestId = $script:replayRequestId
if ([string]::IsNullOrWhiteSpace($replayRequestId)) {
    throw "no failed request id captured for replay smoke"
}

$replayCommandTemplate = @'
set -euo pipefail
request_id="__REQUEST_ID__"
docker exec tracegate tracegate replay --config /etc/tracegate/tracegate.toml --id "$request_id" --target http://replay-target:4000 --confirm-side-effects
docker exec tracegate tracegate requests show --config /etc/tracegate/tracegate.toml --id "$request_id"
docker logs tracegate-replay-target --tail 100
'@

$replayCommand = $replayCommandTemplate.Replace("__REQUEST_ID__", $replayRequestId)
$encodedReplayCommand = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($replayCommand))
$replayLauncher = "printf '%s' '$encodedReplayCommand' | base64 -d | bash"
gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command "$replayLauncher"
if ($LASTEXITCODE -ne 0) {
    throw "remote replay smoke failed with exit code $LASTEXITCODE"
}
gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command "docker logs tracegate --tail 100"
if ($LASTEXITCODE -ne 0) {
    throw "remote log smoke failed with exit code $LASTEXITCODE"
}
& "$scriptRoot\inspect-observability.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName -ReleaseQuality:$ReleaseQuality
& "$scriptRoot\inspect-captures.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName -RequestId $replayRequestId -ReleaseQuality:$ReleaseQuality
