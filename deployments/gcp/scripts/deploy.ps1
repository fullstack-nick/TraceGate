param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $ImageTag = ""
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptRoot "..\..\..")
$scratch = Join-Path $repo "deployments\gcp\.scratch"
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

if ([string]::IsNullOrWhiteSpace($ImageTag)) {
    $ImageTag = (git -C $repo rev-parse --short=12 HEAD).Trim()
}

& "$scriptRoot\build-image.ps1" -ImageTag $ImageTag

$tarPath = Join-Path $scratch "tracegate-$ImageTag.tar"
$envPath = Join-Path $scratch "current.env"
"TRACEGATE_IMAGE=tracegate:$ImageTag`nTRACEGATE_GIT_SHA=$ImageTag" | Set-Content -NoNewline -Encoding ascii $envPath

docker save "tracegate:$ImageTag" -o $tarPath

gcloud compute ssh $VmName --zone $Zone --command "sudo mkdir -p /opt/tracegate && sudo chown `$USER:`$USER /opt/tracegate"
gcloud compute scp $tarPath "${VmName}:/opt/tracegate/tracegate.tar" --zone $Zone
gcloud compute scp "$repo\deployments\gcp\compose\docker-compose.yml" "${VmName}:/opt/tracegate/docker-compose.yml" --zone $Zone
gcloud compute scp "$repo\deployments\gcp\compose\tracegate.toml" "${VmName}:/opt/tracegate/tracegate.toml" --zone $Zone
gcloud compute scp "$repo\deployments\gcp\systemd\tracegate.service" "${VmName}:/tmp/tracegate.service" --zone $Zone
gcloud compute scp $envPath "${VmName}:/opt/tracegate/current.env.next" --zone $Zone

$remoteCommand = @'
set -euxo pipefail
cd /opt/tracegate
if [ -f current.env ]; then cp current.env previous.env; fi
mv current.env.next current.env
docker load -i tracegate.tar
sudo mv /tmp/tracegate.service /etc/systemd/system/tracegate.service
sudo systemctl daemon-reload
sudo systemctl enable tracegate
sudo systemctl restart tracegate
sudo systemctl --no-pager --full status tracegate
'@

gcloud compute ssh $VmName --zone $Zone --command $remoteCommand

Write-Host "Deployed tracegate:$ImageTag to $VmName"
