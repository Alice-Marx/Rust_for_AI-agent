param(
    [string]$TargetDir = $env:CARGO_TARGET_DIR,
    [string]$InnoCompiler,
    [string]$CliProxyApiExecutable,
    [switch]$SkipBuild
)
$ErrorActionPreference = "Stop"
$repoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
if (-not $TargetDir) { $TargetDir = Join-Path $repoRoot 'target' }
$TargetDir = [IO.Path]::GetFullPath($TargetDir)
$env:CARGO_TARGET_DIR = $TargetDir
$distRoot = Join-Path $repoRoot 'dist'
$staging = Join-Path $distRoot 'staging\windows-x64'
$version = [regex]::Match((Get-Content -LiteralPath (Join-Path $repoRoot 'Cargo.toml') -Raw), '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
if (-not $version) { throw 'Cargo version missing' }
if (-not $InnoCompiler) {
    $command = Get-Command ISCC.exe -ErrorAction SilentlyContinue
    if ($command) { $InnoCompiler = $command.Source }
    foreach ($root in @([Environment]::GetEnvironmentVariable('ProgramFiles(x86)'), $env:ProgramFiles, "$env:LOCALAPPDATA\Programs")) {
        if ($InnoCompiler -or -not $root) { continue }
        foreach ($major in @(6,7)) {
            $candidate=Join-Path $root "Inno Setup $major\ISCC.exe"
            if (Test-Path -LiteralPath $candidate) { $InnoCompiler=$candidate; break }
        }
    }
}
if (-not $InnoCompiler -or -not (Test-Path -LiteralPath $InnoCompiler)) { throw 'Specify -InnoCompiler <ISCC.exe>' }
Push-Location $repoRoot
try {
    if (-not $SkipBuild) {
        cargo build --locked --release --bins
        if ($LASTEXITCODE -ne 0) { throw 'Cargo release build failed' }
    }
    # Retain older release artifacts; remove only this known staging directory.
    $resolvedStage=[IO.Path]::GetFullPath($staging)
    if (-not $resolvedStage.StartsWith([IO.Path]::GetFullPath($distRoot)+[IO.Path]::DirectorySeparatorChar,[StringComparison]::OrdinalIgnoreCase)) { throw 'Unsafe staging path' }
    if (Test-Path -LiteralPath $resolvedStage) { Remove-Item -LiteralPath $resolvedStage -Recurse -Force }
    New-Item -ItemType Directory -Force -Path $staging | Out-Null
    foreach ($name in @('wonderland.exe','wonderland-desktop.exe','wonderland-cli.exe')) {
        Copy-Item -LiteralPath (Join-Path $TargetDir "release\$name") -Destination $staging
    }
    # GNU builds may need these runtime libraries. MSVC builds do not.
    foreach ($dll in @('libgcc_s_seh-1.dll','libstdc++-6.dll','libwinpthread-1.dll')) {
        $runtime=Get-Command $dll -ErrorAction SilentlyContinue
        if ($runtime) { Copy-Item -LiteralPath $runtime.Source -Destination $staging }
    }
    $proxyVersion='7.3.7'
    if (-not $CliProxyApiExecutable) {
        $cache=Join-Path $TargetDir "downloads\cliproxyapi-$proxyVersion"
        New-Item -ItemType Directory -Force -Path $cache | Out-Null
        $asset="CLIProxyAPI_$($proxyVersion)_windows_amd64.zip"
        $base="https://github.com/router-for-me/CLIProxyAPI/releases/download/v$proxyVersion"
        Invoke-WebRequest "$base/checksums.txt" -OutFile (Join-Path $cache 'checksums.txt')
        if (-not (Test-Path -LiteralPath (Join-Path $cache $asset))) { Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $cache $asset) }
        $hash=(Get-FileHash -LiteralPath (Join-Path $cache $asset) -Algorithm SHA256).Hash
        if (-not (Select-String -LiteralPath (Join-Path $cache 'checksums.txt') -Pattern "(?i)^$hash\s+\*?$([regex]::Escape($asset))$" -Quiet)) { throw 'CLIProxyAPI checksum mismatch' }
        Expand-Archive -LiteralPath (Join-Path $cache $asset) -DestinationPath (Join-Path $cache 'extracted') -Force
        $CliProxyApiExecutable=(Get-ChildItem -LiteralPath (Join-Path $cache 'extracted') -Filter 'cli-proxy-api.exe' -Recurse -File | Select-Object -First 1).FullName
    }
    $proxyStage=Join-Path $staging 'cliproxyapi'
    New-Item -ItemType Directory -Force -Path $proxyStage | Out-Null
    # Explicit allowlist: never copy auth folders, config, logs or credentials.
    Copy-Item -LiteralPath $CliProxyApiExecutable -Destination (Join-Path $proxyStage 'cli-proxy-api.exe')
    $license=Join-Path (Split-Path -Parent $CliProxyApiExecutable) 'LICENSE'
    if (-not (Test-Path -LiteralPath $license)) { $license=Join-Path $repoRoot 'packaging\third-party\CLIProxyAPI-LICENSE.txt' }
    Copy-Item -LiteralPath $license -Destination (Join-Path $proxyStage 'LICENSE')
    Copy-Item -LiteralPath (Join-Path $repoRoot 'packaging\third-party') -Destination (Join-Path $staging 'THIRD-PARTY-NOTICES') -Recurse
    foreach ($file in @('README.md','LICENSE')) { Copy-Item -LiteralPath (Join-Path $repoRoot $file) -Destination $staging }
    Copy-Item -LiteralPath (Join-Path $repoRoot 'docs') -Destination (Join-Path $staging 'docs') -Recurse
    $launcher=Join-Path $repoRoot 'packaging\windows\Start-Wonderland.ps1'
    $bytes=[IO.File]::ReadAllBytes($launcher)
    if ($bytes.Length -lt 3 -or $bytes[0] -ne 0xEF -or $bytes[1] -ne 0xBB -or $bytes[2] -ne 0xBF) { throw 'Launcher must use UTF-8 BOM for Windows PowerShell 5.1' }
    Copy-Item -LiteralPath $launcher -Destination $staging
    & $InnoCompiler "/DMyAppVersion=$version" (Join-Path $PSScriptRoot 'wonderland.iss')
    if ($LASTEXITCODE -ne 0) { throw 'Inno Setup compilation failed' }
    $zip=Join-Path $distRoot "Wonderland-$version-windows-x64.zip"
    Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -Force
    $npmRoot=Join-Path $repoRoot 'packaging\npm\wonderland-cli'
    Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE') -Destination (Join-Path $npmRoot 'LICENSE')
    if ((Get-Content -LiteralPath (Join-Path $npmRoot 'package.json') -Raw | ConvertFrom-Json).version -ne $version) { throw 'npm/Cargo version mismatch' }
    npm.cmd pack $npmRoot --pack-destination $distRoot
    if ($LASTEXITCODE -ne 0) { throw 'npm pack failed' }
    $artifacts=@("Wonderland-Setup-$version-x64.exe","Wonderland-$version-windows-x64.zip","rust-ai-wonderland-cli-$version.tgz")
    $checksums=foreach ($file in $artifacts) { "$((Get-FileHash -LiteralPath (Join-Path $distRoot $file) -Algorithm SHA256).Hash.ToLowerInvariant())  $file" }
    [IO.File]::WriteAllLines((Join-Path $distRoot 'SHA256SUMS.txt'),$checksums,[Text.UTF8Encoding]::new($false))
    Write-Host "Release $version artifacts: $distRoot"
} finally { Pop-Location }
