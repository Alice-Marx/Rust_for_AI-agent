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
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 7\ISCC.exe"),
        (Join-Path $env:LOCALAPPDATA "Programs\Inno Setup 6\ISCC.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 7\ISCC.exe"),
        (Join-Path ${env:ProgramFiles(x86)} "Inno Setup 6\ISCC.exe"),
        (Join-Path "D:\Program Files (x86)" "Inno Setup 7\ISCC.exe"),
        (Join-Path "D:\Program Files (x86)" "Inno Setup 6\ISCC.exe"),
        (Join-Path "D:\Program Files" "Inno Setup 7\ISCC.exe"),
        (Join-Path "D:\Program Files" "Inno Setup 6\ISCC.exe")
    ) | Where-Object { $_ }

    # 以上位置都没有时，再在常见根目录下浅层搜索一次。
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return (Resolve-Path -LiteralPath $candidate).Path
        }
    }
    foreach ($root in @("D:\", "C:\")) {
        if (-not (Test-Path -LiteralPath $root)) { continue }
        $found = Get-ChildItem -LiteralPath $root -Directory -ErrorAction SilentlyContinue |
            Where-Object { $_.Name -like "Program Files*" } |
            ForEach-Object {
                Get-ChildItem -LiteralPath $_.FullName -Directory -Filter "Inno Setup*" -ErrorAction SilentlyContinue
            } |
            ForEach-Object {
                Get-ChildItem -LiteralPath $_.FullName -Filter "ISCC.exe" -File -ErrorAction SilentlyContinue
            } |
            Select-Object -First 1
        if ($found) {
            return $found.FullName
        }
    }
    throw "未找到 Inno Setup 的 ISCC.exe。请安装 Inno Setup 6/7，或用 -InnoCompiler 指定 ISCC.exe 的完整路径。"
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
    (Join-Path $distRoot "Rust-AI-Agent-Setup-0.1.0-x64.exe"),
    (Join-Path $distRoot "Rust-AI-Agent-Setup-0.1.1-x64.exe"),
    (Join-Path $distRoot "Rust-AI-Agent-Setup-0.2.0-x64.exe"),
    (Join-Path $distRoot "rust-ai-agent-cli-0.2.0.tgz"),
    (Join-Path $distRoot "wonderland-cli-0.2.0.tgz"),
    (Join-Path $distRoot "wonderland-cli-0.2.1.tgz"),
    (Join-Path $distRoot "Wonderland-Setup-0.2.0-x64.exe"),
    (Join-Path $distRoot "Wonderland-Setup-0.2.1-x64.exe"),
    (Join-Path $distRoot "Wonderland-Setup-0.3.0-x64.exe"),
    (Join-Path $distRoot "wonderland-cli-0.3.0.tgz")
    (Join-Path $distRoot "Wonderland-Setup-0.4.0-x64.exe"),
    (Join-Path $distRoot "wonderland-cli-0.4.0.tgz")
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

foreach ($binary in @("wonderland.exe", "wonderland-desktop.exe", "wonderland-cli.exe")) {
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
$script = Join-Path $repoRoot "packaging\windows\wonderland.iss"

# 启动器脚本含中文，必须带 UTF-8 BOM 才能被 Windows PowerShell 5.1 正确解析。
$launcherScript = Join-Path $repoRoot "packaging\windows\Start-Wonderland.ps1"
$launcherBytes = [System.IO.File]::ReadAllBytes($launcherScript)
if ($launcherBytes.Length -lt 3 -or $launcherBytes[0] -ne 0xEF -or $launcherBytes[1] -ne 0xBB -or $launcherBytes[2] -ne 0xBF) {
    throw "Start-Wonderland.ps1 缺少 UTF-8 BOM，Windows PowerShell 5.1 会解析失败。请为该文件添加 BOM 后重新构建。"
}

& $compiler "/DMyAppVersion=$version" $script
if ($LASTEXITCODE -ne 0) {
    throw "Inno Setup 编译失败，退出码：$LASTEXITCODE"
}

$installer = Join-Path $distRoot "Wonderland-Setup-$version-x64.exe"
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "Inno Setup 没有生成预期安装包：$installer"
}

Write-Host "Inno Setup 安装包已生成：$installer"
Write-Host "安装时会同时安装桌面版、后端服务和 wonderland-cli.exe。"

# npm CLI 包：与安装包同版本，方便只用命令行、或没有安装 Rust 的用户。
$npmRoot = Join-Path $repoRoot "packaging\npm\wonderland-cli"
$npmManifest = Join-Path $npmRoot "package.json"
$npmVersion = (Get-Content -LiteralPath $npmManifest -Raw | ConvertFrom-Json).version
if ($npmVersion -ne $version) {
    throw "npm 包版本 $npmVersion 与 Cargo.toml 的 $version 不一致，请先同步版本号。"
}
$npmCommand = Get-Command npm.cmd -ErrorAction SilentlyContinue
if (-not $npmCommand) {
    Write-Warning "未找到 npm.cmd，跳过 npm CLI 包构建。"
} else {
    & $npmCommand.Source pack $npmRoot --pack-destination $distRoot
    if ($LASTEXITCODE -ne 0) {
        throw "npm pack 失败，退出码：$LASTEXITCODE"
    }
    $npmPackage = Join-Path $distRoot "wonderland-cli-$version.tgz"
    if (-not (Test-Path -LiteralPath $npmPackage -PathType Leaf)) {
        throw "npm pack 没有生成预期产物：$npmPackage"
    }
    Write-Host "npm CLI 包已生成：$npmPackage"
}
