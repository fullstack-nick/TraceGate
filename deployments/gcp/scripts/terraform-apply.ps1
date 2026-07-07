param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Region = "us-central1",
    [string] $Zone = "us-central1-a",
    [string] $MachineType = "e2-micro",
    [int] $DiskSizeGb = 30,
    [string] $OperatorCidr = "",
    [switch] $ReleaseQuality,
    [switch] $AllowFallbackZone,
    [switch] $LoadGeneratorEnabled,
    [string] $LoadGeneratorMachineType = "n2-standard-8",
    [switch] $AutoApprove
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$terraformDir = Resolve-Path (Join-Path $scriptRoot "..\terraform")

if ([string]::IsNullOrWhiteSpace($OperatorCidr)) {
    $ip = (Invoke-RestMethod -Uri "https://api.ipify.org").Trim()
    $OperatorCidr = "$ip/32"
}

& "$scriptRoot\guard.ps1" `
    -ProjectId $ProjectId `
    -Region $Region `
    -Zone $Zone `
    -MachineType $MachineType `
    -DiskSizeGb $DiskSizeGb `
    -ReleaseQuality:$ReleaseQuality `
    -AllowFallbackZone:$AllowFallbackZone `
    -LoadGeneratorEnabled:$LoadGeneratorEnabled `
    -LoadGeneratorMachineType $LoadGeneratorMachineType

Push-Location $terraformDir
try {
    terraform init
    $releaseQualityValue = if ($ReleaseQuality) { "true" } else { "false" }
    $loadGeneratorValue = if ($LoadGeneratorEnabled) { "true" } else { "false" }
    $applyArgs = @(
        "apply",
        "-var", "project_id=$ProjectId",
        "-var", "region=$Region",
        "-var", "zone=$Zone",
        "-var", "machine_type=$MachineType",
        "-var", "disk_size_gb=$DiskSizeGb",
        "-var", "ssh_source_cidr=$OperatorCidr",
        "-var", "release_quality_mode=$releaseQualityValue",
        "-var", "load_generator_enabled=$loadGeneratorValue",
        "-var", "load_generator_machine_type=$LoadGeneratorMachineType"
    )
    if ($AutoApprove) {
        $applyArgs += "-auto-approve"
    }
    terraform @applyArgs
    if ($LASTEXITCODE -ne 0) {
        throw "terraform apply failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}
