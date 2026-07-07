param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [switch] $ReleaseQuality
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone -ReleaseQuality:$ReleaseQuality

$remoteCommand = @'
set -euo pipefail
cd /opt/tracegate
test -f previous.env
cp previous.env current.env
sudo systemctl restart tracegate
sudo systemctl --no-pager --full status tracegate
'@

gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command $remoteCommand
if ($LASTEXITCODE -ne 0) {
    throw "rollback remote command failed with exit code $LASTEXITCODE"
}
