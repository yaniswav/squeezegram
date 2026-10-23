<#
.SYNOPSIS
    Fails unless package.json, src-tauri\Cargo.toml and src-tauri\tauri.conf.json carry the same version.

.DESCRIPTION
    The three files each declare the product version. A release built from files that disagree
    would name the installer after one version and report another one. It also checks that the
    updater endpoint in tauri.conf.json is the manifest of the update channel of that version's
    major (scripts\lib\channel.ps1): a bump to 2.0.0 that forgot channel-v2 would leave v2 builds
    reading the 1.x channel. On success the version is written to the pipeline, so callers can use
    it: $v = & scripts\check-versions.ps1

.EXAMPLE
    pwsh -File scripts\check-versions.ps1
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path $PSScriptRoot 'lib/channel.ps1')

$root = Split-Path -Parent $PSScriptRoot

function Read-CargoPackageVersion([string] $Path) {
    $inPackage = $false
    foreach ($line in Get-Content -LiteralPath $Path) {
        if ($line -match '^\s*\[(.+?)\]\s*$') { $inPackage = ($Matches[1] -eq 'package'); continue }
        if ($inPackage -and $line -match '^\s*version\s*=\s*"([^"]+)"') { return $Matches[1] }
    }
    throw "No version in the [package] section of $Path"
}

$versions = [ordered]@{
    'package.json'              = (Get-Content -LiteralPath (Join-Path $root 'package.json') -Raw | ConvertFrom-Json).version
    'src-tauri/Cargo.toml'      = Read-CargoPackageVersion (Join-Path $root 'src-tauri\Cargo.toml')
    'src-tauri/tauri.conf.json' = (Get-Content -LiteralPath (Join-Path $root 'src-tauri\tauri.conf.json') -Raw | ConvertFrom-Json).version
}

foreach ($entry in $versions.GetEnumerator()) {
    Write-Host ("{0,-28} {1}" -f $entry.Key, $entry.Value)
    if ([string]::IsNullOrWhiteSpace($entry.Value)) { throw "$($entry.Key) has no version" }
}

$distinct = @($versions.Values | Sort-Object -Unique)
if ($distinct.Count -ne 1) {
    throw ('Versions differ: ' + (($versions.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ', '))
}
if ($distinct[0] -notmatch '^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$') {
    throw "Version '$($distinct[0])' must be X.Y.Z or X.Y.Z-prerelease (no build metadata '+')"
}

$endpoints = @((Get-Content -LiteralPath (Join-Path $root 'src-tauri\tauri.conf.json') -Raw | ConvertFrom-Json).plugins.updater.endpoints)
if ($endpoints.Count -ne 1) { throw "plugins.updater.endpoints must hold exactly one URL, found $($endpoints.Count)" }
if (-not (Test-ChannelEndpoint $endpoints[0] $distinct[0])) {
    $expected = 'https://github.com/yaniswav/squeezegram/releases/download/channel-v{0}/latest.json' -f (Get-ChannelMajor ([int] ($distinct[0] -split '\.')[0]))
    throw "Updater endpoint '$($endpoints[0])' is not the channel manifest of version $($distinct[0]); expected $expected"
}

Write-Host "Versions agree: $($distinct[0])"
Write-Host "Updater endpoint: $($endpoints[0])"
Write-Output $distinct[0]
