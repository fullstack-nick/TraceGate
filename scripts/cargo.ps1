param(
    [Parameter(ValueFromRemainingArguments = $true)]
    [string[]] $CargoArgs
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$image = "rust:1.96.1-bookworm"
$ForwardArgs = @($CargoArgs)
if ($ForwardArgs.Count -gt 0 -and $ForwardArgs[0] -eq "clippy" -and -not ($ForwardArgs -contains "--")) {
    $denyIndex = [Array]::IndexOf($ForwardArgs, "-D")
    if ($denyIndex -gt 0) {
        $ForwardArgs = @($ForwardArgs[0..($denyIndex - 1)] + "--" + $ForwardArgs[$denyIndex..($ForwardArgs.Count - 1)])
    }
}
$script = @'
set -euo pipefail
export PATH="/usr/local/cargo/bin:$HOME/.cargo/bin:$PATH"
if [ "${1:-}" = "fmt" ] || [ "${1:-}" = "clippy" ]; then
  if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.96.1 --profile minimal --component rustfmt --component clippy
    . "$HOME/.cargo/env"
  else
    rustup component add rustfmt clippy >/dev/null
  fi
  export PATH="$HOME/.cargo/bin:/usr/local/cargo/bin:$PATH"
fi
cargo "$@"
'@

docker run --rm `
    -v "${repo}:/workspace" `
    -w /workspace `
    $image `
    bash -lc $script -- @ForwardArgs
