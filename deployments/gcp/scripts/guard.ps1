param(
    [Parameter(Mandatory = $true)]
    [string] $ProjectId,
    [string] $Region = "us-central1",
    [string] $Zone = "us-central1-a",
    [string] $MachineType = "e2-micro",
    [int] $DiskSizeGb = 30,
    [switch] $ReleaseQuality,
    [switch] $LoadGeneratorEnabled,
    [string] $LoadGeneratorMachineType = "n2-standard-8"
)

$ErrorActionPreference = "Stop"

function Fail($Message) {
    throw "TraceGate GCP guard failed: $Message"
}

$account = (gcloud config get-value account 2>$null).Trim()
$activeProject = (gcloud config get-value project 2>$null).Trim()
$zoneRegion = $Zone -replace '-[a-z]$', ''
if (-not $PSBoundParameters.ContainsKey("Region")) {
    $Region = $zoneRegion
}

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

if ($zoneRegion -ne $Region) {
    Fail "zone '$Zone' is not in region '$Region'"
}

if ($ReleaseQuality) {
    if ($Zone -notin @("us-central1-a", "us-west1-a")) {
        Fail "release-quality zone must be us-central1-a or proven-capacity fallback us-west1-a, got '$Zone'"
    }
} elseif ($Zone -ne "us-central1-a") {
    Fail "steady-state operations are locked to us-central1-a, got '$Zone'"
}

if ($ReleaseQuality) {
    if ($MachineType -notin @("e2-micro", "n2-standard-16")) {
        Fail "release-quality app VM must be e2-micro or n2-standard-16, got '$MachineType'"
    }
} elseif ($MachineType -ne "e2-micro") {
    Fail "large app VM '$MachineType' requires -ReleaseQuality"
}

if ($LoadGeneratorEnabled) {
    if (-not $ReleaseQuality) {
        Fail "load generator creation requires -ReleaseQuality"
    }
    if ($LoadGeneratorMachineType -ne "n2-standard-8") {
        Fail "v1 load generator is locked to n2-standard-8, got '$LoadGeneratorMachineType'"
    }
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
Write-Host "  release-quality: $([bool] $ReleaseQuality)"
Write-Host "  load-generator:  $([bool] $LoadGeneratorEnabled) $LoadGeneratorMachineType"
