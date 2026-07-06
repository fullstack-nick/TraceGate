param(
    [string] $ProjectId = "tracegate-r7m5o9ld",
    [string] $ProjectName = "TraceGate",
    [Parameter(Mandatory = $true)]
    [string] $BillingAccount
)

$ErrorActionPreference = "Stop"

$account = (gcloud config get-value account 2>$null).Trim()
if ($account -ne "nickaccturk@gmail.com") {
    throw "active gcloud account must be nickaccturk@gmail.com, got '$account'"
}

if ($ProjectId -notmatch '^tracegate-[a-z0-9-]+$') {
    throw "project id must begin with tracegate-"
}

gcloud projects create $ProjectId --name $ProjectName
gcloud billing projects link $ProjectId --billing-account $BillingAccount
gcloud config set project $ProjectId
gcloud services enable compute.googleapis.com iam.googleapis.com serviceusage.googleapis.com cloudresourcemanager.googleapis.com --project $ProjectId

Write-Host "TraceGate project bootstrapped: $ProjectId"
