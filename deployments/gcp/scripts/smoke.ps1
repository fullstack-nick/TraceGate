param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm"
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

$ip = (gcloud compute instances describe $VmName --zone $Zone --format="value(networkInterfaces[0].accessConfigs[0].natIP)").Trim()
if ([string]::IsNullOrWhiteSpace($ip)) {
    throw "no external IP found for $VmName"
}

function Invoke-Smoke($Path, $ExpectedStatus) {
    $url = "http://${ip}:8080$Path"
    $tmp = New-TemporaryFile
    $status = (curl.exe -sS -o $tmp -w "%{http_code}" $url).Trim()
    $body = Get-Content $tmp -Raw
    Remove-Item $tmp -Force
    Write-Host "$status $url"
    Write-Host $body
    if ($status -ne "$ExpectedStatus") {
        throw "expected HTTP $ExpectedStatus for $url, got $status"
    }
}

function Invoke-PostSmoke($Path, $ExpectedStatus, $Body) {
    $url = "http://${ip}:8080$Path"
    $tmp = New-TemporaryFile
    $status = (curl.exe -sS -o $tmp -w "%{http_code}" -X POST -H "content-type: application/json" -H "authorization: Bearer should-not-be-stored" --data $Body $url).Trim()
    $body = Get-Content $tmp -Raw
    Remove-Item $tmp -Force
    Write-Host "$status POST $url"
    Write-Host $body
    if ($status -ne "$ExpectedStatus") {
        throw "expected HTTP $ExpectedStatus for $url, got $status"
    }
}

Invoke-Smoke "/api/users/123" 200
Invoke-Smoke "/api/payments/fail" 500
Invoke-Smoke "/api/payments/slow?token=should-not-be-stored&visible=yes" 200
Invoke-PostSmoke "/api/payments/large-fail?api_key=should-not-be-stored&visible=yes" 500 '{"card":"4242424242424242","note":"large request body for capture truncation proof"}'

gcloud compute ssh $VmName --zone $Zone --command "docker logs tracegate --tail 100"
& "$scriptRoot\inspect-observability.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName
& "$scriptRoot\inspect-captures.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName
