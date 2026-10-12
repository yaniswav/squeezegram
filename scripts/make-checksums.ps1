<#
.SYNOPSIS
    Writes dist-release\SHA256SUMS.txt for the publishable files of dist-release.

.DESCRIPTION
    Covers the installer (*-setup-x64.exe) and the portable zip (*-portable-x64.zip). The format
    is the one `sha256sum -c` reads: lowercase hash, two spaces, file name; UTF-8 without BOM,
    LF line endings. Fails when there is nothing to hash.

.EXAMPLE
    pwsh -File scripts\make-checksums.ps1
#>
[CmdletBinding()]
param(
    [string] $Directory
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

if (-not $Directory) { $Directory = Join-Path (Split-Path -Parent $PSScriptRoot) 'dist-release' }
if (-not (Test-Path -LiteralPath $Directory -PathType Container)) { throw "$Directory does not exist" }
$Directory = (Resolve-Path -LiteralPath $Directory).ProviderPath

$files = @(Get-ChildItem -LiteralPath $Directory -File |
        Where-Object { $_.Name -like '*-setup-x64.exe' -or $_.Name -like '*-portable-x64.zip' } |
        Sort-Object Name)
if ($files.Count -eq 0) { throw "No installer or portable zip in $Directory" }

$lines = foreach ($file in $files) {
    $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    "$hash  $($file.Name)"
}
$out = Join-Path $Directory 'SHA256SUMS.txt'
[IO.File]::WriteAllText($out, (($lines -join "`n") + "`n"), (New-Object System.Text.UTF8Encoding($false)))

Write-Host "Wrote $out"
$lines | ForEach-Object { Write-Host "  $_" }
