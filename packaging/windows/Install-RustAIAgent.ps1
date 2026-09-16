param(
    [string]$InstallDir = "$env:LOCALAPPDATA\RustAIAgent"
)

$ErrorActionPreference = "Stop"
$sourceDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$requiredFiles = @("rust-ai-agent.exe", "agent-desktop.exe", "agent-cli.exe", "Start-RustAIAgent.ps1", "Uninstall-RustAIAgent.ps1")

foreach ($file in $requiredFiles) {
    $source = Join-Path $sourceDir $file
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "安装包缺少文件：$file"
    }
}

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
foreach ($file in $requiredFiles) {
    Copy-Item -LiteralPath (Join-Path $sourceDir $file) -Destination (Join-Path $InstallDir $file) -Force
}

$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathItems = @($userPath -split ";" | Where-Object { $_ -and $_.Trim() })
if ($pathItems -notcontains $InstallDir) {
    [Environment]::SetEnvironmentVariable("Path", (($pathItems + $InstallDir) -join ";"), "User")
}

$shell = New-Object -ComObject WScript.Shell
$desktopShortcut = $shell.CreateShortcut((Join-Path ([Environment]::GetFolderPath("Desktop")) "Rust AI Agent.lnk"))
$desktopShortcut.TargetPath = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$desktopShortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'Start-RustAIAgent.ps1')`""
$desktopShortcut.WorkingDirectory = $InstallDir
$desktopShortcut.IconLocation = (Join-Path $InstallDir "agent-desktop.exe")
$desktopShortcut.Save()

$startMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\Rust AI Agent"
New-Item -ItemType Directory -Force -Path $startMenuDir | Out-Null
$startShortcut = $shell.CreateShortcut((Join-Path $startMenuDir "Rust AI Agent.lnk"))
$startShortcut.TargetPath = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$startShortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -File `"$(Join-Path $InstallDir 'Start-RustAIAgent.ps1')`""
$startShortcut.WorkingDirectory = $InstallDir
$startShortcut.IconLocation = (Join-Path $InstallDir "agent-desktop.exe")
$startShortcut.Save()

Write-Host "Rust AI Agent 已安装到：$InstallDir"
Write-Host "桌面版：双击桌面快捷方式或运行 $InstallDir\agent-desktop.exe"
Write-Host "CLI：重新打开 PowerShell 后运行 agent-cli"
