param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("BaselineUp", "LoadGenUp", "Cleanup", "Inventory")]
    [string] $Action,
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Region = "us-central1",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $LoadGeneratorName = "tracegate-v1-loadgen",
    [string] $OperatorCidr = "",
    [switch] $AutoApprove
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path

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

function Invoke-TerraformApply {
    param(
        [string] $MachineType,
        [switch] $ReleaseQuality,
        [switch] $LoadGeneratorEnabled
    )

    $tfArgs = @(
        "-ProjectId", $ProjectId,
        "-Region", $Region,
        "-Zone", $Zone,
        "-MachineType", $MachineType,
        "-DiskSizeGb", 30
    )
    if (-not [string]::IsNullOrWhiteSpace($OperatorCidr)) {
        $tfArgs += @("-OperatorCidr", $OperatorCidr)
    }
    if ($ReleaseQuality) {
        $tfArgs += "-ReleaseQuality"
    }
    if ($LoadGeneratorEnabled) {
        $tfArgs += "-LoadGeneratorEnabled"
    }
    if ($AutoApprove) {
        $tfArgs += "-AutoApprove"
    }

    & "$scriptRoot\terraform-apply.ps1" @tfArgs
}

function Get-TraceGateInstances {
    gcloud compute instances list `
        --filter="(name~'^tracegate' OR labels.app=tracegate)" `
        --format="table(name,zone.basename(),machineType.basename(),status,networkInterfaces[0].accessConfigs[0].natIP,labels.role)"
}

function Show-Inventory {
    Write-Host "TraceGate instance inventory"
    Get-TraceGateInstances
    Write-Host ""
    Write-Host "TraceGate disks"
    gcloud compute disks list `
        --filter="(name~'^tracegate' OR labels.app=tracegate)" `
        --format="table(name,zone.basename(),sizeGb,type.basename(),users.basename())"
    Write-Host ""
    Write-Host "External addresses"
    gcloud compute addresses list `
        --filter="name~'tracegate' OR description~'TraceGate'" `
        --format="table(name,region.basename(),address,status)"
}

function Assert-SteadyState {
    $instances = @(gcloud compute instances list --filter="name~'^tracegate' OR labels.app=tracegate" --format="csv[no-heading](name,machineType.basename(),status)")
    foreach ($instance in $instances) {
        $parts = $instance -split ","
        if ($parts.Count -lt 2) {
            continue
        }
        $name = $parts[0]
        $machineType = $parts[1]
        if ($name -eq $LoadGeneratorName) {
            throw "load generator still exists after cleanup: $name"
        }
        if ($machineType -in @("n2-standard-16", "n2-standard-8")) {
            throw "large TraceGate VM still exists after cleanup: $name $machineType"
        }
    }
}

switch ($Action) {
    "BaselineUp" {
        Invoke-TerraformApply -MachineType "n2-standard-16" -ReleaseQuality
        & "$scriptRoot\status.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName
        & "$scriptRoot\deploy.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName
        & "$scriptRoot\full-demo.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName
    }
    "LoadGenUp" {
        Invoke-TerraformApply -MachineType "n2-standard-16" -ReleaseQuality -LoadGeneratorEnabled
        Invoke-Checked {
            gcloud compute ssh $LoadGeneratorName --zone $Zone --strict-host-key-checking=no --quiet --command "docker --version && mkdir -p /opt/tracegate-load/k6 /opt/tracegate-load/results"
        } "load generator provisioning check"
        Show-Inventory
    }
    "Cleanup" {
        Invoke-TerraformApply -MachineType "e2-micro"
        Show-Inventory
        Assert-SteadyState
        & "$scriptRoot\status.ps1" -ProjectId $ProjectId -Zone $Zone -VmName $VmName
    }
    "Inventory" {
        & "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Region $Region -Zone $Zone -ReleaseQuality
        Show-Inventory
    }
}
