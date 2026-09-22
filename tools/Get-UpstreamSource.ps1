<#
.SYNOPSIS
Fetches the Wonderland source with a Git-first, GitHub-archive fallback.

.DESCRIPTION
Some networks can reach codeload.github.com but block Git Smart HTTP on
github.com:443. The script first attempts an ordinary shallow Git clone. If
that fails, it downloads the source archive for an explicit immutable commit
SHA, records its local SHA-256 in .wonderland-source.json, and makes clear
that the result is an archive checkout rather than a Git-history clone.

.EXAMPLE
.\tools\Get-UpstreamSource.ps1 -Destination F:\work\Rust_for_AI-agent
#>
[CmdletBinding()]
param(
    [ValidatePattern('^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$')]
    [string]$Repository = 'Alice-Marx/Rust_for_AI-agent',

    [ValidatePattern('^[A-Za-z0-9._/-]+$')]
    [string]$Branch = 'main',

    [ValidatePattern('^[0-9a-fA-F]{40}$')]
    [string]$Commit = '24dbf549450c78681db231d9b5e6870f67202b41',

    [Parameter(Mandatory)]
    [string]$Destination,

    [switch]$ArchiveOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Remove-Stage {
    param([Parameter(Mandatory)][string]$Path, [Parameter(Mandatory)][string]$Parent)
    $fullPath = [IO.Path]::GetFullPath($Path)
    $fullParent = [IO.Path]::GetFullPath($Parent).TrimEnd([IO.Path]::DirectorySeparatorChar) + [IO.Path]::DirectorySeparatorChar
    if ($fullPath.StartsWith($fullParent, [StringComparison]::OrdinalIgnoreCase) -and (Test-Path -LiteralPath $fullPath)) {
        Remove-Item -LiteralPath $fullPath -Recurse -Force
    }
}

$destinationFull = [IO.Path]::GetFullPath($Destination)
if (Test-Path -LiteralPath $destinationFull) {
    throw "destination already exists: $destinationFull"
}
$parent = Split-Path -Parent $destinationFull
if (-not (Test-Path -LiteralPath $parent -PathType Container)) {
    throw "destination parent does not exist: $parent"
}
$leaf = Split-Path -Leaf $destinationFull
if ([string]::IsNullOrWhiteSpace($leaf)) {
    throw 'destination must name a directory below an existing parent'
}

$remote = "https://github.com/$Repository.git"
$cloneStage = Join-Path $parent ".${leaf}.clone-$([Guid]::NewGuid().ToString('N'))"
$archiveStage = Join-Path $parent ".${leaf}.archive-$([Guid]::NewGuid().ToString('N')).zip"
$extractStage = Join-Path $parent ".${leaf}.extract-$([Guid]::NewGuid().ToString('N'))"

try {
    if (-not $ArchiveOnly) {
        & git -c http.version=HTTP/1.1 clone --depth=1 --single-branch --branch $Branch $remote $cloneStage
        if ($LASTEXITCODE -eq 0) {
            Move-Item -LiteralPath $cloneStage -Destination $destinationFull
            $resolved = (& git -C $destinationFull rev-parse HEAD).Trim()
            [pscustomobject]@{
                acquisition = 'git_clone'
                repository = $Repository
                requested_branch = $Branch
                resolved_commit = $resolved
                destination = $destinationFull
            }
            return
        }
        Remove-Stage -Path $cloneStage -Parent $parent
    }

    $commit = $Commit.ToLowerInvariant()
    $archiveUrl = "https://codeload.github.com/$Repository/zip/$commit"
    & curl.exe --fail --location --continue-at - --retry 3 --retry-delay 2 --connect-timeout 20 --output $archiveStage $archiveUrl
    if ($LASTEXITCODE -ne 0) {
        throw "Git clone and GitHub archive download both failed for $Repository at $commit"
    }

    $archiveHash = (Get-FileHash -LiteralPath $archiveStage -Algorithm SHA256).Hash.ToLowerInvariant()
    Expand-Archive -LiteralPath $archiveStage -DestinationPath $extractStage
    $entries = @(Get-ChildItem -LiteralPath $extractStage -Force)
    if ($entries.Count -ne 1 -or -not $entries[0].PSIsContainer) {
        throw 'GitHub archive did not contain exactly one source directory'
    }
    Move-Item -LiteralPath $entries[0].FullName -Destination $destinationFull
    [ordered]@{
        acquisition = 'github_codeload_archive'
        repository = $Repository
        requested_branch = $Branch
        resolved_commit = $commit
        archive_url = $archiveUrl
        archive_sha256 = $archiveHash
        downloaded_at_utc = [DateTime]::UtcNow.ToString('O')
        note = 'Archive checkout: no Git history is present. Clone or fetch from origin before rebase, push, or history-sensitive work.'
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $destinationFull '.wonderland-source.json') -Encoding utf8NoBOM
    [pscustomobject]@{
        acquisition = 'github_codeload_archive'
        repository = $Repository
        resolved_commit = $commit
        archive_sha256 = $archiveHash
        destination = $destinationFull
    }
}
finally {
    Remove-Stage -Path $cloneStage -Parent $parent
    Remove-Stage -Path $extractStage -Parent $parent
    Remove-Stage -Path $archiveStage -Parent $parent
}
