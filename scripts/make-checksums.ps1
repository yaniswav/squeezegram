<#
.SYNOPSIS
    Writes dist-release\SHA256SUMS.txt for the publishable files of dist-release.

.DESCRIPTION
    Covers the installer (*-setup-x64.exe) and the portable zip (*-portable-x64.zip). The format
    is the one `sha256sum -c` reads: lowercase hash, two spaces, file name; UTF-8 without BOM,
    LF line endings. Fails when there is nothing to hash. FileNames restricts a release build
    to its current artifacts without deleting older versions or diagnostic previews.

.EXAMPLE
    pwsh -File scripts\make-checksums.ps1
#>
[CmdletBinding()]
param(
    [string] $Directory,
    [string[]] $FileNames
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Directory) { $Directory = Join-Path (Split-Path -Parent $PSScriptRoot) 'dist-release' }
if (-not (Test-Path -LiteralPath $Directory -PathType Container)) { throw "$Directory does not exist" }
$Directory = (Resolve-Path -LiteralPath $Directory).ProviderPath

if ($PSBoundParameters.ContainsKey('FileNames')) {
    if (-not $FileNames -or $FileNames.Count -eq 0) { throw 'FileNames must not be empty.' }
    $files = @(foreach ($name in ($FileNames | Sort-Object -Unique)) {
        if ([string]::IsNullOrWhiteSpace($name) -or $name -match '[\\/:*?"<>|]' -or $name.Contains('..') -or $name -eq '.') {
            throw "FileNames accepts leaf names only: $name"
        }
        $path = Join-Path $Directory $name
        if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Selected artifact missing: $name" }
        Get-Item -LiteralPath $path
    })
}
else {
    $files = @(Get-ChildItem -LiteralPath $Directory -File |
            Where-Object { $_.Name -like '*-setup-x64.exe' -or $_.Name -like '*-portable-x64.zip' } |
            Sort-Object Name)
}
if ($files.Count -eq 0) { throw "No installer or portable zip in $Directory" }

$lines = foreach ($file in $files) {
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $($file.Name)"
}
$out = Join-Path $Directory 'SHA256SUMS.txt'
[IO.File]::WriteAllText($out, (($lines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))

Write-Host "Wrote $out"
$lines | ForEach-Object { Write-Host "  $_" }
