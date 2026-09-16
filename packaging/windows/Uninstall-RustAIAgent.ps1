param(
    [string]$InstallDir = "$env:LOCALAPPDATA\RustAIAgent"
)

$ErrorActionPreference = "Stop"
$desktopShortcut = Join-Path ([Environment]::GetFolderPath("Desktop")) "Rust AI Agent.lnk"
$startMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Rust AI Agent"
$startShortcut = Join-Path $startMenuDir "Rust AI Agent.lnk"

foreach ($path in @($desktopShortcut, $startShortcut)) {
    if (Test-Path -LiteralPath $path) {
        Remove-Item -LiteralPath $path -Force
    }
}
if (Test-Path -LiteralPath $startMenuDir) {
    Remove-Item -LiteralPath $startMenuDir -Force
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$remaining = @($userPath -split ";" | Where-Object { $_ -and $_ -ne $InstallDir })
[Environment]::SetEnvironmentVariable("Path", ($remaining -join ";"), "User")

if (Test-Path -LiteralPath $InstallDir) {
    Remove-Item -LiteralPath $InstallDir -Recurse -Force
}
Write-Host "Rust AI Agent 已卸载。用户数据和 CLIProxyAPI 的 auth-dir 不会被删除。"
