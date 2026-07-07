param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $RequestId = "",
    [switch] $ReleaseQuality
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone -ReleaseQuality:$ReleaseQuality

$showCommand = ""
if (-not [string]::IsNullOrWhiteSpace($RequestId)) {
    if ($RequestId -notmatch '^[0-9A-Fa-f-]+$') {
        throw "request id contains unsupported characters"
    }
    $showCommand = "docker exec tracegate tracegate requests show --config /etc/tracegate/tracegate.toml --id '$RequestId'"
}

$remoteCommandTemplate = @'
set -euxo pipefail
cd /opt/tracegate
docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
ls -lah /opt/tracegate/data
docker exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --failed --limit 20
docker exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --slow --limit 20
__SHOW_COMMAND__
docker exec tracegate tracegate storage prune --config /etc/tracegate/tracegate.toml
sudo mkdir -p /opt/tracegate/data/backups
docker exec tracegate-postgres sh -c 'pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB"' | sudo tee /opt/tracegate/data/backups/tracegate-inspect-backup.sql >/dev/null
sudo chown 10001:10001 /opt/tracegate/data/backups/tracegate-inspect-backup.sql
ls -lah /opt/tracegate/data/backups
'@

$remoteCommand = $remoteCommandTemplate.Replace("__SHOW_COMMAND__", $showCommand)
$encodedRemoteCommand = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($remoteCommand))
$remoteLauncher = "printf '%s' '$encodedRemoteCommand' | base64 -d | bash"
gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command $remoteLauncher
