param(
    [string] $EnvPath = ".env.production",
    [string] $DataDir = "data/production"
)

$ErrorActionPreference = "Stop"
$repo = Resolve-Path (Join-Path $PSScriptRoot "..")
$envFile = Join-Path $repo $EnvPath
$dataPath = Join-Path $repo $DataDir
$tlsPath = Join-Path $dataPath "tls"
$adminTokenPath = Join-Path $dataPath "admin-token"

New-Item -ItemType Directory -Force -Path $tlsPath | Out-Null

function New-SecretHex([int] $Bytes) {
    $buffer = [byte[]]::new($Bytes)
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($buffer)
    }
    finally {
        $rng.Dispose()
    }
    -join ($buffer | ForEach-Object { $_.ToString("x2") })
}

if (-not (Test-Path $envFile)) {
    $postgresPassword = New-SecretHex 24
    $adminToken = New-SecretHex 32
    @"
POSTGRES_USER=tracegate
POSTGRES_DB=tracegate
POSTGRES_PASSWORD=$postgresPassword
TRACEGATE_ADMIN_TOKEN=$adminToken
TRACEGATE_DATABASE_URL=postgres://tracegate:$postgresPassword@postgres:5432/tracegate
"@ | Set-Content -NoNewline -Encoding ascii $envFile
}

$envValues = @{}
Get-Content $envFile | ForEach-Object {
    if ($_ -match '^\s*([^#][^=]+)=(.*)$') {
        $envValues[$matches[1].Trim()] = $matches[2]
    }
}

if (-not $envValues.ContainsKey("TRACEGATE_ADMIN_TOKEN") -or [string]::IsNullOrWhiteSpace($envValues["TRACEGATE_ADMIN_TOKEN"])) {
    throw "TRACEGATE_ADMIN_TOKEN is missing from $envFile"
}

Set-Content -NoNewline -Encoding ascii -Path $adminTokenPath -Value $envValues["TRACEGATE_ADMIN_TOKEN"]

$tracegateCnf = @"
[req]
distinguished_name=req
[ext]
subjectAltName=IP:127.0.0.1,DNS:localhost,DNS:tracegate
"@
$upstreamsCnf = @"
[req]
distinguished_name=req
[ext]
subjectAltName=DNS:users-service,DNS:payments-service,DNS:payments-service-alt,DNS:localhost
"@
Set-Content -NoNewline -Encoding ascii -Path (Join-Path $tlsPath "tracegate.cnf") -Value $tracegateCnf
Set-Content -NoNewline -Encoding ascii -Path (Join-Path $tlsPath "upstreams.cnf") -Value $upstreamsCnf

$haveCerts = (Test-Path (Join-Path $tlsPath "ca.crt")) -and
    (Test-Path (Join-Path $tlsPath "tracegate.crt")) -and
    (Test-Path (Join-Path $tlsPath "upstreams.crt"))

if (-not $haveCerts) {
    $mountPath = (Resolve-Path $tlsPath).Path
    docker run --rm -v "${mountPath}:/tls" alpine/openssl req -x509 -newkey rsa:2048 -nodes -days 3650 `
        -keyout /tls/ca.key `
        -out /tls/ca.crt `
        -subj "/CN=TraceGate Local Production CA"
    docker run --rm -v "${mountPath}:/tls" alpine/openssl req -newkey rsa:2048 -nodes `
        -keyout /tls/tracegate.key `
        -out /tls/tracegate.csr `
        -subj "/CN=tracegate"
    docker run --rm -v "${mountPath}:/tls" alpine/openssl x509 -req -days 3650 `
        -in /tls/tracegate.csr `
        -CA /tls/ca.crt `
        -CAkey /tls/ca.key `
        -CAcreateserial `
        -out /tls/tracegate.crt `
        -extfile /tls/tracegate.cnf `
        -extensions ext
    docker run --rm -v "${mountPath}:/tls" alpine/openssl req -newkey rsa:2048 -nodes `
        -keyout /tls/upstreams.key `
        -out /tls/upstreams.csr `
        -subj "/CN=tracegate-upstreams"
    docker run --rm -v "${mountPath}:/tls" alpine/openssl x509 -req -days 3650 `
        -in /tls/upstreams.csr `
        -CA /tls/ca.crt `
        -CAkey /tls/ca.key `
        -CAcreateserial `
        -out /tls/upstreams.crt `
        -extfile /tls/upstreams.cnf `
        -extensions ext
}

$mountPath = (Resolve-Path $tlsPath).Path
docker run --rm --entrypoint /bin/chmod -v "${mountPath}:/tls" alpine/openssl 644 /tls/ca.crt /tls/tracegate.crt /tls/tracegate.key /tls/upstreams.crt /tls/upstreams.key

Write-Host "Prepared local production Compose env at $envFile"
Write-Host "Prepared local production TLS material under $tlsPath"
