param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $Zone = "us-central1-a",
    [string] $VmName = "tracegate-vm",
    [string] $ImageTag = "",
    [switch] $AllowDirty
)

$ErrorActionPreference = "Stop"
$scriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$repo = Resolve-Path (Join-Path $scriptRoot "..\..\..")
$scratch = Join-Path $repo "deployments\gcp\.scratch"
New-Item -ItemType Directory -Force -Path $scratch | Out-Null

function Invoke-Checked {
    param(
        [scriptblock] $Command,
        [string] $Description
    )

    & $Command
    if ($LASTEXITCODE -ne 0) {
        throw "$Description failed with exit code $LASTEXITCODE"
    }
}

& "$scriptRoot\guard.ps1" -ProjectId $ProjectId -Zone $Zone

$explicitImageTag = -not [string]::IsNullOrWhiteSpace($ImageTag)
$dirtyStatus = (@(git -C $repo status --porcelain) -join "`n").Trim()
if (-not [string]::IsNullOrWhiteSpace($dirtyStatus) -and -not $AllowDirty -and -not $explicitImageTag) {
    throw "refusing to deploy a dirty worktree with an implicit Git SHA image tag. Commit changes first, pass -ImageTag explicitly, or pass -AllowDirty."
}

if ([string]::IsNullOrWhiteSpace($ImageTag)) {
    $ImageTag = (git -C $repo rev-parse --short=12 HEAD).Trim()
}

if (-not [string]::IsNullOrWhiteSpace($dirtyStatus)) {
    Write-Warning "deploying with a dirty worktree because an explicit override was provided"
}

& "$scriptRoot\build-image.ps1" -ImageTag $ImageTag

$ip = (gcloud compute instances describe $VmName --zone $Zone --format="value(networkInterfaces[0].accessConfigs[0].natIP)").Trim()
if ([string]::IsNullOrWhiteSpace($ip)) {
    throw "no external IP found for $VmName"
}

$tarPath = Join-Path $scratch "tracegate-$ImageTag.tar"
$envPath = Join-Path $scratch "current.env"
$remoteScriptPath = Join-Path $scratch "deploy-remote-$ImageTag.sh"
"TRACEGATE_IMAGE=tracegate:$ImageTag`nTRACEGATE_GIT_SHA=$ImageTag" | Set-Content -NoNewline -Encoding ascii $envPath

docker save "tracegate:$ImageTag" -o $tarPath

$prepareCommand = 'sudo mkdir -p /opt/tracegate/data/backups /opt/tracegate/postgres /opt/tracegate/tls && sudo chown -R "$USER:$USER" /opt/tracegate && sudo chown -R 10001:10001 /opt/tracegate/data || true'
Invoke-Checked { gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command $prepareCommand } "prepare VM directories"
Invoke-Checked { gcloud compute scp $tarPath "${VmName}:/opt/tracegate/tracegate.tar" --zone $Zone --strict-host-key-checking=no --quiet } "upload image tar"
Invoke-Checked { gcloud compute scp "$repo\deployments\gcp\compose\docker-compose.production.yml" "${VmName}:/opt/tracegate/docker-compose.yml" --zone $Zone --strict-host-key-checking=no --quiet } "upload production compose"
Invoke-Checked { gcloud compute scp "$repo\deployments\gcp\compose\tracegate.production.toml" "${VmName}:/opt/tracegate/tracegate.toml" --zone $Zone --strict-host-key-checking=no --quiet } "upload production config"
Invoke-Checked { gcloud compute scp "$repo\deployments\gcp\compose\otel-collector.yaml" "${VmName}:/opt/tracegate/otel-collector.yaml" --zone $Zone --strict-host-key-checking=no --quiet } "upload otel collector config"
Invoke-Checked { gcloud compute scp "$repo\deployments\gcp\compose\prometheus.production.yml" "${VmName}:/opt/tracegate/prometheus.yml" --zone $Zone --strict-host-key-checking=no --quiet } "upload prometheus config"
Invoke-Checked { gcloud compute scp "$repo\deployments\gcp\systemd\tracegate.service" "${VmName}:/tmp/tracegate.service" --zone $Zone --strict-host-key-checking=no --quiet } "upload systemd unit"
Invoke-Checked { gcloud compute scp $envPath "${VmName}:/opt/tracegate/current.env.next" --zone $Zone --strict-host-key-checking=no --quiet } "upload image env"

$remoteScript = @'
#!/usr/bin/env bash
set -euo pipefail
cd /opt/tracegate
sudo mkdir -p /opt/tracegate/data/backups /opt/tracegate/postgres /opt/tracegate/tls
sudo chown -R 10001:10001 /opt/tracegate/data || true
if ! command -v openssl >/dev/null 2>&1; then
  sudo apt-get update
  sudo DEBIAN_FRONTEND=noninteractive apt-get install -y openssl
fi
if [ ! -f /opt/tracegate/secrets.env ]; then
  POSTGRES_PASSWORD="$(openssl rand -hex 24)"
  ADMIN_TOKEN="$(openssl rand -hex 32)"
  cat > /opt/tracegate/secrets.env <<EOF
POSTGRES_USER=tracegate
POSTGRES_DB=tracegate
POSTGRES_PASSWORD=${POSTGRES_PASSWORD}
TRACEGATE_ADMIN_TOKEN=${ADMIN_TOKEN}
TRACEGATE_DATABASE_URL=postgres://tracegate:${POSTGRES_PASSWORD}@postgres:5432/tracegate
EOF
  chmod 600 /opt/tracegate/secrets.env
fi
. /opt/tracegate/secrets.env
printf '%s' "$TRACEGATE_ADMIN_TOKEN" > /opt/tracegate/admin-token
sudo chown root:65534 /opt/tracegate/admin-token || true
sudo chmod 640 /opt/tracegate/admin-token
if [ ! -f /opt/tracegate/tls/ca.crt ] || [ ! -f /opt/tracegate/tls/tracegate.crt ] || [ ! -f /opt/tracegate/tls/upstreams.crt ]; then
  rm -f /opt/tracegate/tls/*.crt /opt/tracegate/tls/*.key /opt/tracegate/tls/*.csr /opt/tracegate/tls/*.cnf /opt/tracegate/tls/*.srl
  openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
    -keyout /opt/tracegate/tls/ca.key \
    -out /opt/tracegate/tls/ca.crt \
    -subj "/CN=TraceGate v0.6 Demo CA"
  cat > /opt/tracegate/tls/tracegate.cnf <<EOF
[req]
distinguished_name=req
[ext]
subjectAltName=IP:__TRACEGATE_PUBLIC_IP__,DNS:tracegate,DNS:localhost
EOF
  openssl req -newkey rsa:2048 -nodes \
    -keyout /opt/tracegate/tls/tracegate.key \
    -out /opt/tracegate/tls/tracegate.csr \
    -subj "/CN=tracegate"
  openssl x509 -req -days 3650 \
    -in /opt/tracegate/tls/tracegate.csr \
    -CA /opt/tracegate/tls/ca.crt \
    -CAkey /opt/tracegate/tls/ca.key \
    -CAcreateserial \
    -out /opt/tracegate/tls/tracegate.crt \
    -extfile /opt/tracegate/tls/tracegate.cnf \
    -extensions ext
  cat > /opt/tracegate/tls/upstreams.cnf <<EOF
[req]
distinguished_name=req
[ext]
subjectAltName=DNS:users-service,DNS:payments-service,DNS:payments-service-alt,DNS:localhost
EOF
  openssl req -newkey rsa:2048 -nodes \
    -keyout /opt/tracegate/tls/upstreams.key \
    -out /opt/tracegate/tls/upstreams.csr \
    -subj "/CN=tracegate-upstreams"
  openssl x509 -req -days 3650 \
    -in /opt/tracegate/tls/upstreams.csr \
    -CA /opt/tracegate/tls/ca.crt \
    -CAkey /opt/tracegate/tls/ca.key \
    -CAcreateserial \
    -out /opt/tracegate/tls/upstreams.crt \
    -extfile /opt/tracegate/tls/upstreams.cnf \
    -extensions ext
  chmod 600 /opt/tracegate/tls/*.key
fi
sudo chown -R 10001:10001 /opt/tracegate/postgres
if [ -f current.env ]; then cp current.env previous.env; fi
mv current.env.next current.env
docker load -i tracegate.tar
sudo mv /tmp/tracegate.service /etc/systemd/system/tracegate.service
sudo systemctl daemon-reload
sudo systemctl enable tracegate
sudo systemctl restart tracegate
sudo systemctl --no-pager --full status tracegate
'@

$remoteScript = $remoteScript.Replace("__TRACEGATE_PUBLIC_IP__", $ip)
$remoteScript | Set-Content -NoNewline -Encoding ascii $remoteScriptPath
Invoke-Checked { gcloud compute scp $remoteScriptPath "${VmName}:/opt/tracegate/deploy-remote.sh" --zone $Zone --strict-host-key-checking=no --quiet } "upload remote deploy script"
Invoke-Checked { gcloud compute ssh $VmName --zone $Zone --strict-host-key-checking=no --quiet --command "chmod 700 /opt/tracegate/deploy-remote.sh && /opt/tracegate/deploy-remote.sh" } "run remote deploy script"

Write-Host "Deployed tracegate:$ImageTag to $VmName"
