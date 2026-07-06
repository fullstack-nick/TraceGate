param(
    [string] $ImageTag = ""
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "..\..\..")

if ([string]::IsNullOrWhiteSpace($ImageTag)) {
    $ImageTag = (git -C $repo rev-parse --short=12 HEAD).Trim()
}

docker build `
    --build-arg "TRACEGATE_GIT_SHA=$ImageTag" `
    -t "tracegate:$ImageTag" `
    $repo

Write-Host "tracegate:$ImageTag"
