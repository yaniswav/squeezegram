<#
.SYNOPSIS
    Fills .github\release-notes-template.md for a release and writes the body of the GitHub release.

.DESCRIPTION
    Replaces {{VERSION}}, {{TAG}}, {{REPOSITORY}}, {{SETUP_NAME}}, {{SETUP_SHA256}}, {{ZIP_NAME}}
    and {{ZIP_SHA256}}. The hashes are read from dist-release\SHA256SUMS.txt (written by
    scripts\make-checksums.ps1), never recomputed here. Fails when a file or a hash is missing,
    or when a placeholder is left over. The "What's new" section keeps its TODO line: the owner
    writes it in the draft before publishing.

.EXAMPLE
    pwsh -File scripts\render-release-notes.ps1 -Tag v0.1.0 -Repository yaniswav/squeezegram
#>
[CmdletBinding()]
param(
    # Defaults to the version shared by package.json, Cargo.toml and tauri.conf.json.
    [string] $Version,
    [string] $Tag,
    [string] $Repository = 'yaniswav/squeezegram',
    # Folder with the installer, the zip and SHA256SUMS.txt (default: dist-release).
    [string] $Directory,
    [string] $Template,
    # Output file (default: <Directory>\release-notes.md).
    [string] $OutFile
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
if (-not $Version) { $Version = (& (Join-Path $PSScriptRoot 'check-versions.ps1') | Select-Object -Last 1) }
if (-not $Tag) { $Tag = "v$Version" }
if (-not $Directory) { $Directory = Join-Path $root 'dist-release' }
if (-not $Template) { $Template = Join-Path $root '.github\release-notes-template.md' }
if (-not $OutFile) { $OutFile = Join-Path $Directory 'release-notes.md' }

$setupName = "squeezegram-$Version-setup-x64.exe"
$zipName = "squeezegram-$Version-portable-x64.zip"

$sums = Join-Path $Directory 'SHA256SUMS.txt'
foreach ($path in @($Template, $sums)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Not found: $path" }
}
$hashes = @{}
foreach ($line in Get-Content -LiteralPath $sums) {
    if ($line -match '^([0-9a-f]{64})  (.+)$') { $hashes[$Matches[2]] = $Matches[1] }
}
foreach ($name in @($setupName, $zipName)) {
    if (-not $hashes.ContainsKey($name)) { throw "$sums has no SHA-256 for $name" }
}

$values = @{
    '{{VERSION}}'      = $Version
    '{{TAG}}'          = $Tag
    '{{REPOSITORY}}'   = $Repository
    '{{SETUP_NAME}}'   = $setupName
    '{{SETUP_SHA256}}' = $hashes[$setupName]
    '{{ZIP_NAME}}'     = $zipName
    '{{ZIP_SHA256}}'   = $hashes[$zipName]
}
$text = Get-Content -LiteralPath $Template -Raw
foreach ($entry in $values.GetEnumerator()) { $text = $text.Replace($entry.Key, $entry.Value) }
if ($text -match '\{\{[A-Z0-9_]+\}\}') { throw "Unfilled placeholder in the template: $($Matches[0])" }

$full = [IO.Path]::GetFullPath($OutFile)
[IO.File]::WriteAllText($full, $text, (New-Object System.Text.UTF8Encoding($false)))
Write-Host "Wrote $full"
Write-Output $full
