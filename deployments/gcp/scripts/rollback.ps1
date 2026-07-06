param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm"
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

$remoteCommand = @'
set -euxo pipefail
cd /opt/tracegate
test -f previous.env
cp previous.env current.env
sudo systemctl restart tracegate
sudo systemctl --no-pager --full status tracegate
'@

gcloud compute ssh $VmName --zone $Zone --command $remoteCommand
