$ErrorActionPreference = 'Stop'
$installDir = $PSScriptRoot
$backend = Join-Path $installDir 'wonderland.exe'
$desktop = Join-Path $installDir 'wonderland-desktop.exe'
$dataDir = if ($env:AGENT_DATA_DIR) { $env:AGENT_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'WonderlandData' }
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
$env:AGENT_DATA_DIR=$dataDir
$env:CLIPROXYAPI_BIN=Join-Path $installDir 'cliproxyapi\cli-proxy-api.exe'
if (-not $env:AGENT_PROVIDER) { $env:AGENT_PROVIDER='subscription' }
if (-not $env:AGENT_SERVER_URL) { $env:AGENT_SERVER_URL='http://127.0.0.1:8080' }
if (-not $env:AGENT_ADDR) { $env:AGENT_ADDR=([Uri]$env:AGENT_SERVER_URL).Authority }
if (-not (Test-Path -LiteralPath $backend) -or -not (Test-Path -LiteralPath $desktop)) { throw 'Wonderland installation is incomplete.' }
function Get-Health {
    try { Invoke-RestMethod "$($env:AGENT_SERVER_URL.TrimEnd('/'))/health" -TimeoutSec 2 } catch { $null }
}
$health=Get-Health
if ($health -and ($health.status -ne 'ok' -or -not $health.provider)) { throw 'Service port is occupied. Set AGENT_ADDR and AGENT_SERVER_URL.' }
if (-not $health) {
    $process=Start-Process -FilePath $backend -WorkingDirectory $installDir -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $dataDir 'wonderland.out.log') -RedirectStandardError (Join-Path $dataDir 'wonderland.err.log')
    for ($attempt=0; $attempt -lt 60; $attempt++) {
        $health=Get-Health
        if ($health) { break }
        if ($process.HasExited) { throw "Backend failed. See $dataDir\wonderland.err.log" }
        Start-Sleep -Milliseconds 500
    }
    if (-not $health) { throw "Backend timeout. See $dataDir\wonderland.err.log" }
}
# The Rust service owns provider configuration and sidecar lifecycle.
Start-Process -FilePath $desktop -WorkingDirectory ([Environment]::GetFolderPath('UserProfile'))
