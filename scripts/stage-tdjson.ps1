<#
.SYNOPSIS
    Copies the pinned tdjson.dll into a git-ignored staging folder, for the installer and the portable zip.

.DESCRIPTION
    The source (default %LOCALAPPDATA%\squeezegram\tdlib\bin\tdjson.dll, where
    scripts\install-tdjson.ps1 puts it) is accepted only if its SHA-256 matches the pinned value
    (src-tauri\src\transport\tdlib\library.rs, PINNED_SHA256). The copy is hashed again.
    The default staging folder, src-tauri\target\release-stage, is under target\, which Git ignores;
    tdjson*.dll is ignored as well. The DLL is never committed.

    The staged path is written to the pipeline.

.EXAMPLE
    pwsh -File scripts\stage-tdjson.ps1
#>
[CmdletBinding()]
param(
    # The tdjson.dll to stage.
    [string] $Source,
    # Staging folder (default: src-tauri\target\release-stage).
    [string] $Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
# Same pin as library.rs (PINNED_SHA256) and scripts\install-tdjson.ps1.
$DllSha256 = '048AF1D1C97EF9B860DD5DE132C4D2D901D444691EC86FC15012E5896DBDE6B8'

if (-not $Source) {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set' }
    $Source = Join-Path $env:LOCALAPPDATA 'squeezegram\tdlib\bin\tdjson.dll'
}
if (-not $Destination) { $Destination = Join-Path $root 'src-tauri\target\release-stage' }

if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
    throw "tdjson.dll not found at $Source (see scripts\install-tdjson.ps1)"
}
$actual = (Get-FileHash -LiteralPath $Source -Algorithm SHA256).Hash.ToUpperInvariant()
if ($actual -ne $DllSha256) {
    throw "tdjson.dll (source): SHA-256 $actual does not match the pinned $DllSha256"
}

New-Item -ItemType Directory -Force -Path $Destination | Out-Null
$target = Join-Path (Resolve-Path -LiteralPath $Destination).ProviderPath 'tdjson.dll'
$temp = "$target.tmp"
Copy-Item -LiteralPath $Source -Destination $temp -Force
try {
    $copied = (Get-FileHash -LiteralPath $temp -Algorithm SHA256).Hash.ToUpperInvariant()
    if ($copied -ne $DllSha256) { throw "Staged copy has SHA-256 $copied, expected $DllSha256" }
    Move-Item -LiteralPath $temp -Destination $target -Force
}
finally {
    if (Test-Path -LiteralPath $temp) { Remove-Item -LiteralPath $temp -Force }
}

Write-Host "tdjson.dll staged, SHA-256 OK ($DllSha256): $target"
Write-Output $target
