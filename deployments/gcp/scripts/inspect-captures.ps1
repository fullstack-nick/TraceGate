param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $RequestId = ""
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

$showCommand = ""
if (-not [string]::IsNullOrWhiteSpace($RequestId)) {
    $showCommand = "docker exec tracegate tracegate requests show --config /etc/tracegate/tracegate.toml --id '$RequestId'"
}

$remoteCommand = @"
set -euxo pipefail
cd /opt/tracegate
docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
ls -lah /opt/tracegate/data
docker exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --failed --limit 20
docker exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --slow --limit 20
$showCommand
docker exec tracegate tracegate storage prune --config /etc/tracegate/tracegate.toml
docker exec tracegate tracegate storage backup --config /etc/tracegate/tracegate.toml --output /var/lib/tracegate/backups/tracegate-inspect-backup.db
ls -lah /opt/tracegate/data/backups
"@

gcloud compute ssh $VmName --zone $Zone --command $remoteCommand
