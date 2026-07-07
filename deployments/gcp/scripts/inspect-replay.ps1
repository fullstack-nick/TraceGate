param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $RequestId = ""
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

$remoteCommand = @'
set -euxo pipefail
cd /opt/tracegate
docker ps --format 'table {{.Names}}\t{{.Status}}\t{{.Ports}}'
request_id="__REQUEST_ID__"
if [ -z "$request_id" ]; then
  request_id="$(docker exec tracegate tracegate requests list --config /etc/tracegate/tracegate.toml --failed --limit 1 | awk 'NR==2 {print $6}')"
fi
if [ -z "$request_id" ]; then
  echo "no failed request found"
  exit 1
fi
docker exec tracegate tracegate requests show --config /etc/tracegate/tracegate.toml --id "$request_id"
docker logs tracegate-replay-target --tail 100
'@
$remoteCommand = $remoteCommand.Replace("__REQUEST_ID__", $RequestId)
$encodedCommand = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($remoteCommand))
$launcher = "printf '%s' '$encodedCommand' | base64 -d | bash"

gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command "$launcher"
