param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $OutputName = "tracegate-capture-backup.db"
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptRoot "..\..\..")
$scratch = Join-Path $repo "deployments\gcp\.scratch"
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

$remotePath = "/opt/tracegate/data/backups/$OutputName"
$containerPath = "/var/lib/tracegate/backups/$OutputName"

$remoteCommand = @"
set -euxo pipefail
sudo mkdir -p /opt/tracegate/data/backups
sudo chown -R 10001:10001 /opt/tracegate/data
docker exec tracegate tracegate storage backup --config /etc/tracegate/tracegate.toml --output $containerPath
ls -lah $remotePath
"@

gcloud compute ssh $VmName --zone $Zone --command $remoteCommand
gcloud compute scp "${VmName}:${remotePath}" (Join-Path $scratch $OutputName) --zone $Zone

Write-Host "Downloaded capture-store backup to $(Join-Path $scratch $OutputName)"
