param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [int] $Tail = 200
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

gcloud compute ssh $VmName --zone $Zone --command "sudo journalctl -u tracegate -n $Tail --no-pager; docker logs tracegate --tail $Tail"
