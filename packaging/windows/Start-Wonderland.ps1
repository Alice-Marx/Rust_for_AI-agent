$ErrorActionPreference = "Stop"

$installDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$backend = Join-Path $installDir "wonderland.exe"
$desktop = Join-Path $installDir "wonderland-desktop.exe"
$sidecar = Join-Path $installDir "cliproxyapi\cli-proxy-api.exe"
$dataDir = Join-Path $env:LOCALAPPDATA "WonderlandData"
$legacyDataDir = Join-Path $env:LOCALAPPDATA "RustAIAgentData"
$proxyDataDir = Join-Path $dataDir "CLIProxyAPI"
$proxyConfig = Join-Path $proxyDataDir "config.yaml"
$launcherSettings = Join-Path $proxyDataDir "launcher-settings.json"
$launcherLog = Join-Path $dataDir "launcher.log"
$proxyPort = 18317
$proxyBaseUrl = "http://127.0.0.1:$proxyPort"

# 数据目录迁移：旧版 RustAIAgentData → WonderlandData（如果新版目录不存在）
if (-not (Test-Path -LiteralPath $dataDir) -and (Test-Path -LiteralPath $legacyDataDir)) {
    try {
        Move-Item -LiteralPath $legacyDataDir -Destination $dataDir -ErrorAction Stop
        Write-Host "已迁移数据目录：$legacyDataDir → $dataDir"
    } catch {
        Write-Warning "迁移失败，将使用旧版目录：$_"
        $dataDir = $legacyDataDir
        $proxyDataDir = Join-Path $dataDir "CLIProxyAPI"
        $proxyConfig = Join-Path $proxyDataDir "config.yaml"
        $launcherSettings = Join-Path $proxyDataDir "launcher-settings.json"
        $launcherLog = Join-Path $dataDir "launcher.log"
    }
}

New-Item -ItemType Directory -Force -Path $dataDir, $proxyDataDir | Out-Null

function Write-LauncherLog {
    param([string]$Message)

    "$(Get-Date -Format o)  $Message" | Out-File -LiteralPath $launcherLog -Append -Encoding utf8
}

function New-LauncherSecret {
    $bytes = New-Object byte[] 32
    $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $rng.GetBytes($bytes)
    } finally {
        $rng.Dispose()
    }
    return [Convert]::ToBase64String($bytes).TrimEnd("=").Replace("+", "-").Replace("/", "_")
}

function Get-LauncherSettings {
    $settings = $null
    if (Test-Path -LiteralPath $launcherSettings -PathType Leaf) {
        try {
            $settings = Get-Content -LiteralPath $launcherSettings -Raw | ConvertFrom-Json
        } catch {
            Write-LauncherLog "无法读取 launcher-settings.json，将生成新的本机访问密钥。"
        }
    }

    $apiKey = if ($settings) { [string]$settings.api_key } else { "" }
    $managementKey = if ($settings) { [string]$settings.management_key } else { "" }
    if ([string]::IsNullOrWhiteSpace($apiKey) -or [string]::IsNullOrWhiteSpace($managementKey)) {
        $settings = [ordered]@{
            schema_version = 1
            api_key = New-LauncherSecret
            management_key = New-LauncherSecret
        }
        $settings | ConvertTo-Json | Set-Content -LiteralPath $launcherSettings -Encoding utf8
    }
    return $settings
}

function Test-LocalHttpEndpoint {
    param([string]$Url)

    try {
        $request = [System.Net.HttpWebRequest]::Create($Url)
        $request.Method = "GET"
        $request.Timeout = 2000
        $request.ReadWriteTimeout = 2000
        $response = $request.GetResponse()
        $response.Close()
        return $true
    } catch [System.Net.WebException] {
        if ($_.Exception.Response) {
            $_.Exception.Response.Close()
            return $true
        }
        return $false
    } catch {
        return $false
    }
}

function Wait-ForLocalHttpEndpoint {
    param(
        [string]$Url,
        [int]$Attempts = 15
    )

    for ($attempt = 0; $attempt -lt $Attempts; $attempt++) {
        if (Test-LocalHttpEndpoint $Url) {
            return $true
        }
        Start-Sleep -Seconds 1
    }
    return $false
}

function Get-AgentHealth {
    try {
        return Invoke-RestMethod "http://127.0.0.1:8080/health" -TimeoutSec 2
    } catch {
        return $null
    }
}

function Stop-ManagedBackend {
    param([string]$ExecutablePath)

    Get-CimInstance Win32_Process -Filter "Name = 'wonderland.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.ExecutablePath -and $_.ExecutablePath -ieq $ExecutablePath } |
        ForEach-Object {
            Write-LauncherLog "停止旧的受管 Wonderland 进程：$($_.ProcessId)"
            Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
        }
}

if (-not (Test-Path -LiteralPath $backend -PathType Leaf) -or -not (Test-Path -LiteralPath $desktop -PathType Leaf)) {
    throw "Wonderland 安装不完整。请重新运行安装包。"
}
if (-not (Test-Path -LiteralPath $sidecar -PathType Leaf)) {
    throw "CLIProxyAPI sidecar 缺失。请重新运行 Wonderland 安装包。"
}

$settings = Get-LauncherSettings
if (-not (Test-Path -LiteralPath $proxyConfig -PathType Leaf)) {
    $authDir = (Join-Path $proxyDataDir "auth").Replace("\", "/")
    @"
host: "127.0.0.1"
port: $proxyPort
auth-dir: "$authDir"
api-keys:
  - "$($settings.api_key)"
remote-management:
  allow-remote: false
  secret-key: "$($settings.management_key)"
  disable-control-panel: true
  disable-auto-update-panel: true
"@ | Set-Content -LiteralPath $proxyConfig -Encoding utf8
    Write-LauncherLog "已创建仅本机使用的 CLIProxyAPI 配置。"
}

$proxyReady = Test-LocalHttpEndpoint "$proxyBaseUrl/v1/models"
if (-not $proxyReady) {
    Write-LauncherLog "启动 CLIProxyAPI sidecar。"
    Start-Process -FilePath $sidecar `
        -ArgumentList @("-config", $proxyConfig) `
        -WorkingDirectory $proxyDataDir `
        -WindowStyle Hidden `
        -RedirectStandardOutput (Join-Path $proxyDataDir "cliproxyapi.out.log") `
        -RedirectStandardError (Join-Path $proxyDataDir "cliproxyapi.err.log")
    $proxyReady = Wait-ForLocalHttpEndpoint "$proxyBaseUrl/v1/models"
}
if ($proxyReady) {
    Write-LauncherLog "CLIProxyAPI 已就绪：$proxyBaseUrl"
} else {
    Write-LauncherLog "CLIProxyAPI 未能在 15 秒内启动；桌面端会显示可读的诊断信息。"
}

$env:AGENT_DATA_DIR = $dataDir
$env:AGENT_PROVIDER = "cliproxyapi"
$env:CLIPROXYAPI_ENABLED = "true"
$env:CLIPROXYAPI_BASE_URL = "$proxyBaseUrl/v1"
$env:CLIPROXYAPI_API_KEY = [string]$settings.api_key
$env:CLIPROXYAPI_MANAGEMENT_URL = "$proxyBaseUrl/v0/management"
$env:CLIPROXYAPI_MANAGEMENT_KEY = [string]$settings.management_key

$health = Get-AgentHealth
if ($null -eq $health -or -not [bool]$health.cliproxyapi_configured) {
    Stop-ManagedBackend -ExecutablePath $backend
    Start-Sleep -Milliseconds 300
    if ($null -eq (Get-AgentHealth)) {
        Write-LauncherLog "启动 Wonderland 后端。"
        Start-Process -FilePath $backend `
            -WorkingDirectory $installDir `
            -WindowStyle Hidden `
            -RedirectStandardOutput (Join-Path $dataDir "wonderland.out.log") `
            -RedirectStandardError (Join-Path $dataDir "wonderland.err.log")
        if (-not (Wait-ForLocalHttpEndpoint "http://127.0.0.1:8080/health")) {
            Write-LauncherLog "Wonderland 后端未能在 15 秒内启动。"
        }
    } else {
        Write-LauncherLog "8080 端口已由其他 Wonderland 占用，未替换该进程。"
    }
}

Start-Process -FilePath $desktop -WorkingDirectory $installDir
