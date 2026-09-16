$ErrorActionPreference = "Stop"
$installDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$backend = Join-Path $installDir "rust-ai-agent.exe"
$desktop = Join-Path $installDir "agent-desktop.exe"
$dataDir = Join-Path $env:LOCALAPPDATA "RustAIAgentData"
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
$env:AGENT_DATA_DIR = $dataDir

try {
    Invoke-RestMethod "http://127.0.0.1:8080/health" -TimeoutSec 1 | Out-Null
} catch {
    Start-Process -FilePath $backend -WorkingDirectory $installDir -WindowStyle Hidden
    Start-Sleep -Milliseconds 800
}

Start-Process -FilePath $desktop -WorkingDirectory $installDir
