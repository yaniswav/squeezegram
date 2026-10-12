<#
.SYNOPSIS
    Installs the pinned tdjson.dll (TDLib 1.8.68, built from source, Windows x86_64) for squeezegram.

.DESCRIPTION
    Puts tdjson.dll in %LOCALAPPDATA%\squeezegram\tdlib\bin\, where the app looks for it
    (after SQUEEZEGRAM_TDJSON, before the folder of the executable). The DLL is accepted only
    if its SHA-256 matches the pinned value, the same one the app checks at startup.

    -FromFile <path> is required: it copies a tdjson.dll already on disk. The script never
    downloads anything (no network access).

    The copy goes through a temporary file renamed at the end: a failed run never leaves a
    half-written DLL. Nothing else is installed (OpenSSL and zlib are linked statically in this
    build; the Visual C++ runtime is required).

.EXAMPLE
    pwsh -File scripts\install-tdjson.ps1 -FromFile C:\Downloads\tdlib\bin\tdjson.dll
#>
[CmdletBinding()]
param(
    # The tdjson.dll to copy.
    [Parameter(Mandatory = $true)]
    [string] $FromFile,
    # Target folder (default: %LOCALAPPDATA%\squeezegram\tdlib\bin).
    [string] $Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Same pins as src-tauri/src/transport/tdlib/library.rs (PINNED_SHA256).
$DllSha256 = '048AF1D1C97EF9B860DD5DE132C4D2D901D444691EC86FC15012E5896DBDE6B8'

function Get-Sha256([string] $Path) {
    (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToUpperInvariant()
}

function Assert-Sha256([string] $Path, [string] $Expected, [string] $What) {
    $actual = Get-Sha256 $Path
    if ($actual -ne $Expected) {
        throw "$What`: SHA-256 $actual does not match the pinned $Expected"
    }
    Write-Host "$What`: SHA-256 OK ($actual)"
}

if (-not $Destination) {
    if (-not $env:LOCALAPPDATA) { throw 'LOCALAPPDATA is not set' }
    $Destination = Join-Path $env:LOCALAPPDATA 'squeezegram\tdlib\bin'
}
$target = Join-Path $Destination 'tdjson.dll'
$work = Join-Path ([IO.Path]::GetTempPath()) ("squeezegram-tdjson-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $work | Out-Null

try {
    $source = (Resolve-Path -LiteralPath $FromFile).ProviderPath
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "$FromFile is not a file" }
    # The source is checked before anything is written to the destination.
    Assert-Sha256 $source $DllSha256 'tdjson.dll (source)'

    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    $partial = "$target.part"
    Copy-Item -LiteralPath $source -Destination $partial -Force
    Assert-Sha256 $partial $DllSha256 'tdjson.dll (copy)'
    Move-Item -LiteralPath $partial -Destination $target -Force
    Assert-Sha256 $target $DllSha256 "installed $target"
}
finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
    if ((Test-Path variable:partial) -and (Test-Path -LiteralPath $partial)) {
        Remove-Item -LiteralPath $partial -Force -ErrorAction SilentlyContinue
    }
}
