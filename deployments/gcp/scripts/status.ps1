param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [switch] $ReleaseQuality
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone -ReleaseQuality:$ReleaseQuality

gcloud compute instances describe $VmName --zone $Zone --format="table(name,status,machineType.basename(),networkInterfaces[0].accessConfigs[0].natIP)"
gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command "sudo systemctl --no-pager --full status tracegate || true; docker ps || true; cat /opt/tracegate/current.env || true"
