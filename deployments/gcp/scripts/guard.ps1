param(
    [Parameter(Mandatory = $true)]
    [string] $ProjectId,
    [string] $Region = "us-central1",
    [string] $Zone = "us-central1-a",
    [string] $MachineType = "e2-micro",
    [int] $DiskSizeGb = 30
)

$ErrorActionPreference = "Stop"

function Fail($Message) {
    throw "TraceGate GCP guard failed: $Message"
}

$account = (gcloud config get-value account 2>$null).Trim()
$activeProject = (gcloud config get-value project 2>$null).Trim()

if ($account -ne "nickaccturk@gmail.com") {
    Fail "active gcloud account must be nickaccturk@gmail.com, got '$account'"
}

if ($ProjectId -notmatch '^tracegate-[a-z0-9-]+$') {
    Fail "project id must be dedicated to TraceGate and begin with tracegate-, got '$ProjectId'"
}

if ($activeProject -ne $ProjectId) {
    Fail "active gcloud project is '$activeProject'. Run: gcloud config set project $ProjectId"
}

if ($ProjectId -eq "pulsequeue-r7m5o9ld" -or $ProjectId -eq "devcontrol-r7m5o9ld") {
    Fail "refusing to deploy TraceGate into another project: $ProjectId"
}

if ($Region -notin @("us-central1", "us-east1", "us-west1")) {
    Fail "region '$Region' is not in the Compute Engine free-tier region set"
}

if ($Zone -ne "us-central1-a") {
    Fail "v0.1 is locked to us-central1-a, got '$Zone'"
}

if ($MachineType -ne "e2-micro") {
    Fail "v0.1 is locked to e2-micro, got '$MachineType'"
}

if ($DiskSizeGb -ne 30) {
    Fail "v0.1 is locked to 30 GB standard persistent disk, got '$DiskSizeGb'"
}

gcloud projects describe $ProjectId --format="value(projectId)" | Out-Null

Write-Host "TraceGate GCP guard ok"
Write-Host "  account: $account"
Write-Host "  project: $ProjectId"
Write-Host "  region:  $Region"
Write-Host "  zone:    $Zone"
Write-Host "  vm:      $MachineType / ${DiskSizeGb}GB pd-standard"
