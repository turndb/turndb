param(
    [Parameter(Mandatory = $true)][string]$ArtifactDirectory,
    [Parameter(Mandatory = $true)][string]$WorkDirectory
)

$ErrorActionPreference = 'Stop'
$artifacts = (Resolve-Path $ArtifactDirectory).Path
New-Item -ItemType Directory -Force -Path $WorkDirectory | Out-Null
$work = (Resolve-Path $WorkDirectory).Path
# Bind, probe and install through the same literal IPv4 endpoint. On hosted Windows, `localhost`
# resolves to ::1 first; mixing the two spellings can make a healthy registry look unavailable.
$registry = 'http://127.0.0.1:4873'

$expectedManifestNames = @(
    'prebuild-manifest-win32-x64-msvc.json',
    'cli-manifest-win32-x64-msvc.json',
    'python-manifest-3.9.json',
    'python-manifest-3.10.json',
    'python-manifest-3.11.json',
    'python-manifest-3.12.json',
    'python-manifest-3.13.json'
)
$manifests = @(Get-ChildItem $artifacts -Filter '*manifest*.json' | Sort-Object Name)
$actualManifestNames = @($manifests | ForEach-Object { $_.Name })
if (Compare-Object $expectedManifestNames $actualManifestNames) {
    throw "artifact manifest set differs: expected $($expectedManifestNames -join ', '); " +
        "got $($actualManifestNames -join ', ')"
}
foreach ($manifest in $manifests) {
    python scripts/release-artifacts.py verify `
        --manifest $manifest.FullName --directory $artifacts
    if ($LASTEXITCODE -ne 0) { throw "digest verification failed: $($manifest.Name)" }
}
$versions = @($manifests |
    ForEach-Object { (Get-Content -Raw $_.FullName | ConvertFrom-Json).version } |
    Sort-Object -Unique)
if ($versions.Count -ne 1) { throw "artifact versions disagree: $($versions -join ', ')" }
$version = $versions[0]

$verdaccioRoot = Join-Path $work 'verdaccio'
New-Item -ItemType Directory -Force -Path $verdaccioRoot | Out-Null
$config = Join-Path $verdaccioRoot 'config.yml'
@"
storage: ./storage
auth:
  htpasswd:
    file: ./htpasswd
    max_users: 10
uplinks:
packages:
  '@*/*':
    access: `$all
    publish: `$all
  '**':
    access: `$all
    publish: `$all
log: { type: stdout, format: pretty, level: warn }
"@ | Set-Content -Encoding utf8 $config

$npmRoot = (npm root --global).Trim()
if ($LASTEXITCODE -ne 0) { throw 'could not locate global npm packages' }
$verdaccioEntry = Join-Path $npmRoot 'verdaccio/bin/verdaccio'
if (-not (Test-Path $verdaccioEntry)) { throw "Verdaccio entry point is absent: $verdaccioEntry" }
$server = Start-Process -FilePath (Get-Command node.exe).Source `
    -ArgumentList @($verdaccioEntry, '--config', $config, '--listen', '127.0.0.1:4873') `
    -WorkingDirectory $verdaccioRoot -PassThru
try {
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        try {
            Invoke-WebRequest -UseBasicParsing -Uri $registry -TimeoutSec 1 | Out-Null
            break
        } catch {
            if ($attempt -eq 59) { throw 'Verdaccio did not become ready' }
            Start-Sleep -Milliseconds 500
        }
    }

    $credentials = @{
        name = 'turndb-ci'
        password = 'local-registry-only'
        email = 'ci@invalid.example'
        type = 'user'
        roles = @()
    } | ConvertTo-Json
    $login = Invoke-RestMethod -Method Put `
        -Uri "$registry/-/user/org.couchdb.user:turndb-ci" `
        -ContentType 'application/json' -Body $credentials
    if (-not $login.token) { throw 'Verdaccio registration returned no token' }
    $npmrc = Join-Path $work '.npmrc'
    @"
registry=$registry/
//127.0.0.1:4873/:_authToken=$($login.token)
audit=false
fund=false
"@ | Set-Content -Encoding ascii $npmrc
    $env:NPM_CONFIG_USERCONFIG = $npmrc

    # Prove the registry cannot satisfy a miss from an uplink before trusting any install.
    npm view '@turndb/known-absent-package' version --registry $registry 2>$null
    if ($LASTEXITCODE -eq 0) { throw 'offline registry resolved a package that was never published' }
    $global:LASTEXITCODE = 0

    $tarballs = @(Get-ChildItem $artifacts -Filter '*.tgz')
    $platform = @($tarballs | Where-Object {
        $_.Name -match '^turndb-(native|cli)-(linux|win32|darwin)-'
    } | Sort-Object Name)
    $selectors = @($tarballs | Where-Object {
        $_.Name -match '^turndb-(native|cli)-\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?\.tgz$'
    } | Sort-Object Name)
    if ($platform.Count -lt 2 -or $selectors.Count -ne 2) {
        throw "expected platform and selector tarballs; got $($tarballs.Name -join ', ')"
    }
    foreach ($tarball in @($platform) + @($selectors)) {
        npm publish $tarball.FullName --registry $registry --ignore-scripts
        if ($LASTEXITCODE -ne 0) { throw "local publish failed: $($tarball.Name)" }
    }

    $nodeConsumer = Join-Path $work 'node-consumer'
    New-Item -ItemType Directory -Force -Path $nodeConsumer | Out-Null
    npm init --yes --prefix $nodeConsumer | Out-Null
    npm install --prefix $nodeConsumer --registry $registry --ignore-scripts `
        --no-audit --no-fund '@turndb/native' '@turndb/cli'
    if ($LASTEXITCODE -ne 0) { throw 'clean npm registry install failed' }

    $wheels = @(Get-ChildItem $artifacts -Filter '*win_amd64.whl' |
        Where-Object { $_.Name -match 'cp312' })
    if ($wheels.Count -ne 1) { throw "expected one CPython 3.12 wheel, got $($wheels.Count)" }
    $wheel = $wheels[0]
    python scripts/check-python-wheel.py $wheel.FullName --version $version --platform win_amd64
    if ($LASTEXITCODE -ne 0) { throw 'wheel metadata audit failed' }
    $simple = Join-Path $work 'python-index/simple/turndb'
    New-Item -ItemType Directory -Force -Path $simple | Out-Null
    Copy-Item $wheel.FullName $simple
    "<a href='$($wheel.Name)'>$($wheel.Name)</a>" |
        Set-Content -Encoding ascii (Join-Path $simple 'index.html')
    $indexRoot = Join-Path $work 'python-index'
    $pythonServer = Start-Process -FilePath 'python' `
        -ArgumentList @('-m', 'http.server', '4874', '--bind', '127.0.0.1') `
        -WorkingDirectory $indexRoot -PassThru
    try {
        for ($attempt = 0; $attempt -lt 30; $attempt++) {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri 'http://127.0.0.1:4874/simple/turndb/' `
                    -TimeoutSec 1 | Out-Null
                break
            } catch {
                if ($attempt -eq 29) { throw 'local Python index did not become ready' }
                Start-Sleep -Milliseconds 500
            }
        }
        $venv = Join-Path $work 'python-consumer'
        python -m venv $venv
        $python = Join-Path $venv 'Scripts/python.exe'
        & $python -m pip install --disable-pip-version-check --no-deps `
            --index-url 'http://127.0.0.1:4874/simple/' "turndb==$version"
        if ($LASTEXITCODE -ne 0) { throw 'clean local-index wheel install failed' }
        & $python -m pip install --disable-pip-version-check --no-deps `
            --index-url 'http://127.0.0.1:4874/simple/' 'turndb-known-absent'
        if ($LASTEXITCODE -eq 0) { throw 'closed Python index resolved an absent distribution' }
        $global:LASTEXITCODE = 0
    } finally {
        if ($pythonServer -and -not $pythonServer.HasExited) { Stop-Process $pythonServer.Id -Force }
    }

    @{
        NodeConsumer = $nodeConsumer
        Python = $python
        Registry = $registry
        Version = $version
    } | ConvertTo-Json | Set-Content -Encoding utf8 (Join-Path $work 'installed.json')
} finally {
    if ($server -and -not $server.HasExited) { Stop-Process $server.Id -Force }
}
$global:LASTEXITCODE = 0
