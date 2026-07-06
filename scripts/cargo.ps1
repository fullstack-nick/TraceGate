param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $CargoArgs
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$image = "rust:1.96.1-bookworm"

docker run --rm `
    -v "${repo}:/workspace" `
    -w /workspace `
    $image `
    cargo @CargoArgs
