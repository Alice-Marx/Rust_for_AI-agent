$ErrorActionPreference = 'Stop'
$installDir = $PSScriptRoot
$backend = Join-Path $installDir 'wonderland.exe'
$desktop = Join-Path $installDir 'wonderland-desktop.exe'

function Fail($message) {
    Write-Host ""
    Write-Host "[错误] $message" -ForegroundColor Red
    Write-Host ""
    Write-Host "日志位置: $dataDir\wonderland.err.log / wonderland.out.log"
    Write-Host "本窗口 15 秒后自动关闭。" -ForegroundColor DarkGray
    Start-Sleep -Seconds 15
    exit 1
}

Write-Host "Wonderland 正在启动..." -ForegroundColor Cyan

# 数据目录优先级：安装器写入的 data-location.txt > 环境变量 AGENT_DATA_DIR > 默认 %LOCALAPPDATA%
$dataLocationFile = Join-Path $installDir 'data-location.txt'
if (Test-Path -LiteralPath $dataLocationFile) {
    $configured = (Get-Content -LiteralPath $dataLocationFile -Raw).Trim()
    if ($configured) { $dataDir = $configured }
}
if (-not $dataDir) { $dataDir = if ($env:AGENT_DATA_DIR) { $env:AGENT_DATA_DIR } else { Join-Path $env:LOCALAPPDATA 'WonderlandData' } }
New-Item -ItemType Directory -Force -Path $dataDir | Out-Null
Write-Host "数据目录: $dataDir"
$env:AGENT_DATA_DIR = $dataDir
$env:CLIPROXYAPI_BIN = Join-Path $installDir 'cliproxyapi\cli-proxy-api.exe'
if (-not $env:AGENT_PROVIDER) { $env:AGENT_PROVIDER = 'subscription' }
if (-not $env:AGENT_SERVER_URL) { $env:AGENT_SERVER_URL = 'http://127.0.0.1:8080' }
if (-not $env:AGENT_ADDR) { $env:AGENT_ADDR = ([Uri]$env:AGENT_SERVER_URL).Authority }

if (-not (Test-Path -LiteralPath $backend) -or -not (Test-Path -LiteralPath $desktop)) {
    Fail '安装不完整：缺少 wonderland.exe 或 wonderland-desktop.exe，请重新安装。'
}

function Get-Health {
    try { Invoke-RestMethod "$($env:AGENT_SERVER_URL.TrimEnd('/'))/health" -TimeoutSec 2 } catch { $null }
}

$health = Get-Health
if ($health -and ($health.status -ne 'ok' -or -not $health.provider)) {
    Fail "端口 $($env:AGENT_ADDR) 被其他程序占用。请关闭占用程序，或设置环境变量 AGENT_ADDR / AGENT_SERVER_URL 换一个端口。"
}

if (-not $health) {
    Write-Host "正在启动本地服务（$($env:AGENT_ADDR)）..."
    $process = Start-Process -FilePath $backend -WorkingDirectory $installDir -WindowStyle Hidden -PassThru -RedirectStandardOutput (Join-Path $dataDir 'wonderland.out.log') -RedirectStandardError (Join-Path $dataDir 'wonderland.err.log')
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        $health = Get-Health
        if ($health) { break }
        if ($process.HasExited) {
            Fail "本地服务启动失败（进程已退出）。日志: $dataDir\wonderland.err.log"
        }
        Start-Sleep -Milliseconds 500
    }
    if (-not $health) {
        Fail "本地服务 30 秒内未就绪。日志: $dataDir\wonderland.err.log"
    }
} else {
    Write-Host "本地服务已在运行。"
}

Write-Host "正在打开桌面窗口..." -ForegroundColor Cyan
# The Rust service owns provider configuration and sidecar lifecycle.
# cmd start detaches the desktop so this console can close immediately.
Start-Process -FilePath 'cmd.exe' -ArgumentList '/c', 'start', '""', ('"{0}"' -f $desktop) -WorkingDirectory ([Environment]::GetFolderPath('UserProfile')) -WindowStyle Hidden
Write-Host "完成。桌面窗口应已打开；若没有出现，请直接运行安装目录下的 wonderland-desktop.exe。" -ForegroundColor Green
Start-Sleep -Seconds 2
