param(
    [Parameter(Mandatory = $true)][string]$Wheel,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$WorkDirectory
)

$ErrorActionPreference = 'Stop'
$wheelPath = (Resolve-Path $Wheel).Path
python scripts/check-python-wheel.py $wheelPath --version $Version --platform win_amd64
if ($LASTEXITCODE -ne 0) { throw 'wheel metadata audit failed' }

$simple = Join-Path $WorkDirectory 'index/simple/turndb'
New-Item -ItemType Directory -Force -Path $simple | Out-Null
Copy-Item $wheelPath $simple
$wheelName = Split-Path $wheelPath -Leaf
"<a href='$wheelName'>$wheelName</a>" | Set-Content -Encoding ascii (Join-Path $simple 'index.html')
$indexRoot = Join-Path $WorkDirectory 'index'
$server = Start-Process -FilePath 'python' `
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
    $venv = Join-Path $WorkDirectory 'venv'
    python -m venv $venv
    $consumer = Join-Path $venv 'Scripts/python.exe'
    & $consumer -m pip install --disable-pip-version-check --no-deps `
        --index-url 'http://127.0.0.1:4874/simple/' "turndb==$Version"
    if ($LASTEXITCODE -ne 0) { throw 'wheel install from the closed local index failed' }
    & $consumer -c @'
import importlib.metadata
import tempfile
import turndb
assert importlib.metadata.version("turndb")
with tempfile.TemporaryDirectory() as root:
    store = turndb.Store.open(root + "/smoke.turndb")
    store.write([{"kind": "put", "id": "smoke", "attrs": [], "contents": []}], durable=True)
    assert [row["id"] for row in store.scan({"contractVersion": 1, "limit": 10})["rows"]] == ["smoke"]
    store.close(durable=True)
'@
    if ($LASTEXITCODE -ne 0) { throw 'installed wheel engine smoke failed' }
    & $consumer -m pip install --disable-pip-version-check --no-deps `
        --index-url 'http://127.0.0.1:4874/simple/' 'turndb-known-absent'
    if ($LASTEXITCODE -eq 0) { throw 'closed local index resolved an absent distribution' }
    $global:LASTEXITCODE = 0
} finally {
    if ($server -and -not $server.HasExited) { Stop-Process $server.Id -Force }
}
$global:LASTEXITCODE = 0
