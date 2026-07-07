param(
    [string] $AdminToken = "tracegate-local-admin",
    [switch] $SkipStart
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $repo

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

function Invoke-TraceGateCall {
    param(
        [string] $Path,
        [int] $ExpectedStatus,
        [string[]] $Headers = @(),
        [string] $Method = "GET",
        [string] $Body = ""
    )

    $url = "http://localhost:8080$Path"
    $bodyPath = New-TemporaryFile
    $headersPath = New-TemporaryFile
    $args = @("-sS", "-D", $headersPath, "-o", $bodyPath, "-w", "%{http_code}", "-X", $Method)
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
    [pscustomobject]@{
        Status = [int] $status
        Body = $responseBody
        RequestId = $requestId
    }
}

function Invoke-AdminJson {
    param([string] $Path)

    $url = "http://localhost:9090$Path"
    $json = curl.exe -fsS -H "Authorization: Bearer $AdminToken" $url
    if ($LASTEXITCODE -ne 0) {
        throw "admin API call failed: $url"
    }
    $json | ConvertFrom-Json
}

function Wait-For {
    param(
        [scriptblock] $Condition,
        [string] $Description,
        [int] $Attempts = 30
    )

    for ($i = 1; $i -le $Attempts; $i++) {
        try {
            $result = & $Condition
            if ($result) {
                return $result
            }
        } catch {
            if ($i -eq $Attempts) {
                throw
            }
        }
        Start-Sleep -Seconds 2
    }
    throw "timed out waiting for $Description"
}

if (-not $SkipStart) {
    Invoke-Checked { docker compose up -d --build --remove-orphans } "local compose startup"
}

Wait-For {
    $status = (curl.exe -sS -o NUL -w "%{http_code}" -H "Authorization: Bearer $AdminToken" http://localhost:9090/health/ready).Trim()
    $status -eq "200"
} "TraceGate readiness" | Out-Null

$users = Invoke-TraceGateCall "/api/users/123" 200
$denied = Invoke-TraceGateCall "/api/payments/fail" 403
$timeout = Invoke-TraceGateCall "/api/plugin-timeout/proof" 403
$failed = Invoke-TraceGateCall "/api/payments/fail" 500 @("x-api-key: tracegate-demo-key")
$slow = Invoke-TraceGateCall "/api/payments/slow?token=should-not-be-stored&visible=yes" 200 @("x-api-key: tracegate-demo-key")
$large = Invoke-TraceGateCall "/api/payments/large-fail?api_key=should-not-be-stored&visible=yes" 500 @("x-api-key: tracegate-demo-key", "content-type: application/json", "authorization: Bearer should-not-be-stored") "POST" '{"card":"4242424242424242","note":"large request body for capture proof"}'

Invoke-Checked {
    docker compose exec -T tracegate tracegate replay --config /etc/tracegate/tracegate.toml --last-failed --target http://replay-target:4000 --confirm-side-effects
} "local replay"

$overview = Wait-For {
    $value = Invoke-AdminJson "/admin/api/overview"
    if ($value.route_count -eq 3 -and $value.plugin_count -eq 3 -and $value.storage_ready) {
        $value
    }
} "console overview"

$requests = Wait-For {
    $value = Invoke-AdminJson "/admin/api/requests?failed=true&limit=10"
    if (($value.requests | Where-Object { $_.request_id -eq $large.RequestId })) {
        $value
    }
} "failed request in console API"

$failedDetail = Wait-For {
    $value = Invoke-AdminJson "/admin/api/requests/$($large.RequestId)"
    if ($value.details.replay_runs.Count -gt 0) {
        $value
    }
} "replay run in request detail"

$denyDetail = Wait-For {
    $value = Invoke-AdminJson "/admin/api/requests/$($denied.RequestId)"
    $deny = $value.details.plugin_decisions | Where-Object { $_.plugin_id -eq "api-key-guard" -and $_.action -eq "deny" }
    if ($deny) {
        $value
    }
} "plugin deny decision"

$routes = Invoke-AdminJson "/admin/api/routes"
if (-not ($routes.routes | Where-Object { $_.id -eq "payments" -and $_.upstreams.Count -ge 1 })) {
    throw "console route API did not show payments route health"
}

$plugins = Invoke-AdminJson "/admin/api/plugins"
if (-not ($plugins.plugins | Where-Object { $_.id -eq "api-key-guard" -and ($_.config_keys -contains "expected") })) {
    throw "console plugin API did not show api-key-guard config keys"
}
$pluginJson = $plugins | ConvertTo-Json -Depth 10
if ($pluginJson.Contains("tracegate-demo-key")) {
    throw "console plugin API leaked a plugin config value"
}

$telemetry = Invoke-AdminJson "/admin/api/telemetry"
foreach ($seriesName in @("tracegate_requests_total", "tracegate_plugin_decisions_total", "tracegate_plugin_duration_seconds")) {
    $series = $telemetry.series | Where-Object { $_.name -eq $seriesName }
    if (-not $series -or -not $series.present) {
        throw "console telemetry API did not show $seriesName"
    }
}

Wait-For {
    $health = curl.exe -fsS http://localhost:3000/api/health
    if ($LASTEXITCODE -ne 0) {
        return $false
    }
    $health -match '"database"\s*:\s*"ok"'
} "Grafana health" | Out-Null

$dashboardSearch = curl.exe -fsS "http://localhost:3000/api/search?query=TraceGate%20Overview"
if ($LASTEXITCODE -ne 0 -or ($dashboardSearch -notmatch "tracegate-overview")) {
    throw "Grafana dashboard provisioning was not visible"
}

Write-Host "TraceGate v0.7 local full demo passed"
Write-Host "overview mode=$($overview.mode) git_sha=$($overview.git_sha)"
Write-Host "users_request=$($users.RequestId)"
Write-Host "denied_request=$($denied.RequestId)"
Write-Host "timeout_request=$($timeout.RequestId)"
Write-Host "failed_request=$($failed.RequestId)"
Write-Host "slow_request=$($slow.RequestId)"
Write-Host "large_failed_request=$($large.RequestId)"
Write-Host "failed_api_rows=$($requests.requests.Count)"
Write-Host "replay_runs=$($failedDetail.details.replay_runs.Count)"
Write-Host "deny_decisions=$($denyDetail.details.plugin_decisions.Count)"
