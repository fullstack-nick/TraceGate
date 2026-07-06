param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Region = "us-central1",
    [string] $Zone = "us-central1-a",
    [string] $MachineType = "e2-micro",
    [int] $DiskSizeGb = 30,
    [string] $OperatorCidr = "",
    [switch] $AutoApprove
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$terraformDir = Resolve-Path (Join-Path $scriptRoot "..\terraform")

if ([string]::IsNullOrWhiteSpace($OperatorCidr)) {
    $ip = (Invoke-RestMethod -Uri "https://api.ipify.org").Trim()
    $OperatorCidr = "$ip/32"
}

& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Region $Region -Zone $Zone -MachineType $MachineType -DiskSizeGb $DiskSizeGb

Push-Location $terraformDir
try {
    terraform init
    $applyArgs = @(
        "apply",
        "-var", "project_id=$ProjectId",
        "-var", "region=$Region",
        "-var", "zone=$Zone",
        "-var", "machine_type=$MachineType",
        "-var", "disk_size_gb=$DiskSizeGb",
        "-var", "ssh_source_cidr=$OperatorCidr"
    )
    if ($AutoApprove) {
        $applyArgs += "-auto-approve"
    }
    terraform @applyArgs
} finally {
    Pop-Location
}
