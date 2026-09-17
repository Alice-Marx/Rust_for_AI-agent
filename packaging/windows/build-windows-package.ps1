param(
    [string]$TargetDir = "D:\Jianwei_Li\rust\targets",
    [string]$InnoCompiler,
    [string]$CliProxyApiExecutable
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path))
$cargoRoot = "D:\Jianwei_Li\rust"
$env:CARGO_HOME = "$cargoRoot\cargo"
$env:RUSTUP_HOME = "$cargoRoot\rustup"
$env:CARGO_TARGET_DIR = $TargetDir
$env:Path = "$cargoRoot\cargo\bin;$env:Path"

function Resolve-InnoCompiler {
    param([string]$RequestedPath)

    $candidates = @(
        $RequestedPath,
        (Get-Command ISCC.exe -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -ErrorAction SilentlyContinue),
        (Join-Path $env:ProgramFiles "Inno Setup 7\ISCC.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 7\ISCC.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 7\ISCC.exe")
    ) | Where-Object { $_ }

    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }

    throw "未找到 Inno Setup 7 的 ISCC.exe。请安装 Inno Setup 7，或用 -InnoCompiler 指定 ISCC.exe 的完整路径。"
}

function Resolve-CliProxyApiExecutable {
    param([string]$RequestedPath)

    if ($RequestedPath) {
        if (-not (Test-Path -LiteralPath $RequestedPath -PathType Leaf)) {
            throw "指定的 CLIProxyAPI 可执行文件不存在：$RequestedPath"
        }
        return (Resolve-Path -LiteralPath $RequestedPath).Path
    }

    $version = "7.3.6"
    $assetName = "CLIProxyAPI_${version}_windows_amd64.zip"
    $releaseBaseUrl = "https://github.com/router-for-me/CLIProxyAPI/releases/download/v$version"
    $cacheDir = Join-Path $cargoRoot "downloads\CLIProxyAPI\$version"
    $archivePath = Join-Path $cacheDir $assetName
    $checksumsPath = Join-Path $cacheDir "checksums.txt"
    $extractDir = Join-Path $cacheDir "windows-amd64"

    New-Item -ItemType Directory -Force -Path $cacheDir | Out-Null
    if (-not (Test-Path -LiteralPath $checksumsPath -PathType Leaf)) {
        Invoke-WebRequest -Uri "$releaseBaseUrl/checksums.txt" -OutFile $checksumsPath
    }
    if (-not (Test-Path -LiteralPath $archivePath -PathType Leaf)) {
        Invoke-WebRequest -Uri "$releaseBaseUrl/$assetName" -OutFile $archivePath
    }

    $escapedAssetName = [regex]::Escape($assetName)
    $checksumMatch = [regex]::Match(
        (Get-Content -LiteralPath $checksumsPath -Raw),
        "(?mi)^([a-f0-9]{64})\s+\*?$escapedAssetName\s*$"
    )
    if (-not $checksumMatch.Success) {
        throw "CLIProxyAPI checksums.txt 中没有 $assetName 的 SHA-256。"
    }
    $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash
    if ($actualHash -ne $checksumMatch.Groups[1].Value.ToUpperInvariant()) {
        throw "CLIProxyAPI 下载校验失败：$archivePath"
    }

    $existing = Get-ChildItem -LiteralPath $extractDir -Filter "cli-proxy-api.exe" -File -Recurse -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $existing) {
        if (Test-Path -LiteralPath $extractDir) {
            Remove-Item -LiteralPath $extractDir -Recurse -Force
        }
        Expand-Archive -LiteralPath $archivePath -DestinationPath $extractDir -Force
        $existing = Get-ChildItem -LiteralPath $extractDir -Filter "cli-proxy-api.exe" -File -Recurse -ErrorAction SilentlyContinue |
            Select-Object -First 1
    }
    if (-not $existing) {
        throw "CLIProxyAPI 发布包中没有 cli-proxy-api.exe。"
    }
    return $existing.FullName
}

Set-Location $repoRoot
cargo build --release --bins

$versionMatch = [regex]::Match((Get-Content -LiteralPath (Join-Path $repoRoot "Cargo.toml") -Raw), '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw "无法从 Cargo.toml 读取 package version。"
}
$version = $versionMatch.Groups[1].Value

$distRoot = Join-Path $repoRoot "dist"
$staging = Join-Path $distRoot "staging\windows-x64"
$legacyArtifacts = @(
    (Join-Path $distRoot "Rust-AI-Agent-windows-x64.zip"),
    (Join-Path $distRoot "Rust-AI-Agent-windows-x64"),
    (Join-Path $distRoot "Rust-AI-Agent-Setup-0.1.0-x64.exe")
)
foreach ($legacyArtifact in $legacyArtifacts) {
    if (Test-Path -LiteralPath $legacyArtifact) {
        Remove-Item -LiteralPath $legacyArtifact -Recurse -Force
    }
}
if (Test-Path -LiteralPath $staging) {
    Remove-Item -LiteralPath $staging -Recurse -Force
}
New-Item -ItemType Directory -Force -Path $staging | Out-Null

foreach ($binary in @("rust-ai-agent.exe", "agent-desktop.exe", "agent-cli.exe")) {
    $source = Join-Path $TargetDir "release\$binary"
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "缺少发布二进制文件：$source"
    }
    Copy-Item -LiteralPath $source -Destination (Join-Path $staging $binary) -Force
}

$proxyExecutable = Resolve-CliProxyApiExecutable -RequestedPath $CliProxyApiExecutable
$proxyStagingDir = Join-Path $staging "cliproxyapi"
New-Item -ItemType Directory -Force -Path $proxyStagingDir | Out-Null
Copy-Item -Path (Join-Path (Split-Path -Parent $proxyExecutable) "*") -Destination $proxyStagingDir -Recurse -Force

$noticesSource = Join-Path $repoRoot "packaging\third-party"
$noticesDestination = Join-Path $staging "THIRD-PARTY-NOTICES"
Copy-Item -LiteralPath $noticesSource -Destination $noticesDestination -Recurse -Force

$compiler = Resolve-InnoCompiler -RequestedPath $InnoCompiler
$script = Join-Path $repoRoot "packaging\windows\rust-ai-agent.iss"
& $compiler "/DMyAppVersion=$version" $script
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup 编译失败，退出码：$LASTEXITCODE"
}

$installer = Join-Path $distRoot "Rust-AI-Agent-Setup-$version-x64.exe"
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "Inno Setup 没有生成预期安装包：$installer"
}

Write-Host "Inno Setup 安装包已生成：$installer"
Write-Host "安装时会同时安装桌面版、后端服务和 agent-cli.exe。"
