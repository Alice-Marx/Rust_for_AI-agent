param(
    [string]$TargetDir = "D:\Jianwei_Li\rust\targets"
)

$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path))
$cargoRoot = "D:\Jianwei_Li\rust"
$env:CARGO_HOME = "$cargoRoot\cargo"
$env:RUSTUP_HOME = "$cargoRoot\rustup"
$env:CARGO_TARGET_DIR = $TargetDir
$env:Path = "$cargoRoot\cargo\bin;$env:Path"

Set-Location $repoRoot
cargo build --release --bins

$distRoot = Join-Path $repoRoot "dist"
$staging = Join-Path $distRoot "Rust-AI-Agent-windows-x64"
$archive = Join-Path $distRoot "Rust-AI-Agent-windows-x64.zip"
if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
if (Test-Path -LiteralPath $archive) { Remove-Item -LiteralPath $archive -Force }
New-Item -ItemType Directory -Force -Path $staging | Out-Null

foreach ($binary in @("rust-ai-agent.exe", "agent-desktop.exe", "agent-cli.exe")) {
    Copy-Item -LiteralPath (Join-Path $TargetDir "release\$binary") -Destination (Join-Path $staging $binary)
}
Copy-Item -LiteralPath (Join-Path $repoRoot "packaging\windows\Install-RustAIAgent.ps1") -Destination $staging
Copy-Item -LiteralPath (Join-Path $repoRoot "packaging\windows\Install-RustAIAgent.cmd") -Destination $staging
Copy-Item -LiteralPath (Join-Path $repoRoot "packaging\windows\Start-RustAIAgent.ps1") -Destination $staging
Copy-Item -LiteralPath (Join-Path $repoRoot "packaging\windows\Uninstall-RustAIAgent.ps1") -Destination $staging
Copy-Item -LiteralPath (Join-Path $repoRoot "README.md") -Destination (Join-Path $staging "README.md")

Compress-Archive -Path (Join-Path $staging "*") -DestinationPath $archive -CompressionLevel Optimal
Write-Host "Windows 安装包已生成：$archive"
Write-Host "解压后运行：powershell -ExecutionPolicy Bypass -File .\Install-RustAIAgent.ps1"
