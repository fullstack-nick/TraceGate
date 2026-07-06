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

Invoke-Smoke "/api/users/123" 200
Invoke-Smoke "/api/payments/fail" 500

gcloud compute ssh $VmName --zone $Zone --command "docker logs tracegate --tail 100"
& "$scriptRoot\inspect-observability.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName
