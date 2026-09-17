param(
    [string]$TargetDir = "D:\Jianwei_Li\rust\targets",
    [string]$InnoCompiler
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
    (Join-Path $distRoot "Rust-AI-Agent-windows-x64")
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
