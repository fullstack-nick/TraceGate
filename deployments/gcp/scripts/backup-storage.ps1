param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $OutputName = "tracegate-capture-backup.sql"
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptRoot "..\..\..")
$scratch = Join-Path $repo "deployments\gcp\.scratch"
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

if ($OutputName -notmatch '^[A-Za-z0-9._-]+$') {
    throw "output name contains unsupported characters"
}

$remotePath = "/opt/tracegate/data/backups/$OutputName"

$remoteCommandTemplate = @'
set -euxo pipefail
sudo mkdir -p /opt/tracegate/data/backups
sudo chown -R 10001:10001 /opt/tracegate/data
docker exec tracegate-postgres sh -c 'pg_dump -U "$POSTGRES_USER" -d "$POSTGRES_DB"' | sudo tee __REMOTE_PATH__ >/dev/null
sudo chmod 644 __REMOTE_PATH__
ls -lah __REMOTE_PATH__
'@

$remoteCommand = $remoteCommandTemplate.Replace("__REMOTE_PATH__", $remotePath)
$encodedRemoteCommand = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($remoteCommand))
$remoteLauncher = "printf '%s' '$encodedRemoteCommand' | base64 -d | bash"

gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command $remoteLauncher
gcloud compute scp "${VmName}:${remotePath}" (Join-Path $scratch $OutputName) --zone $Zone

Write-Host "Downloaded capture-store backup to $(Join-Path $scratch $OutputName)"
