<#
.SYNOPSIS
    Compiles the per-user Inno installer; never executes the resulting installer.
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $AppSource,
    [Parameter(Mandatory)][string] $TdjsonSource,
    [Parameter(Mandatory)][string] $NoticesSource,
    [Parameter(Mandatory)][string] $LicenseSource,
    [Parameter(Mandatory)][string] $BootstrapperSource,
    [Parameter(Mandatory)][ValidatePattern('^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$')][string] $Version,
    [Parameter(Mandatory)][string] $OutputDirectory,
    [Parameter(Mandatory)][ValidatePattern('^[A-Za-z0-9._-]+$')][string] $OutputFilename,
    [string] $IsccPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
foreach ($name in @('AppSource', 'TdjsonSource', 'NoticesSource', 'LicenseSource', 'BootstrapperSource')) {
    $path = Get-Variable -Name $name -ValueOnly
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) { throw "Required input missing: $name" }
    Set-Variable -Name $name -Value (Resolve-Path -LiteralPath $path).ProviderPath
}
foreach ($path in @($AppSource, $BootstrapperSource)) {
    $stream = [IO.File]::OpenRead($path)
    try {
        if ($stream.ReadByte() -ne 0x4D -or $stream.ReadByte() -ne 0x5A) { throw "Input is not a Windows executable: $path" }
    }
    finally { $stream.Dispose() }
}
if ((Get-FileHash -LiteralPath $TdjsonSource -Algorithm SHA256).Hash.ToLowerInvariant() -ne '048af1d1c97ef9b860dd5de132c4d2d901d444691ec86fc15012e5896dbde6b8') {
    throw 'tdjson.dll does not match the pinned SHA-256.'
}
$bootstrapperSignature = Get-AuthenticodeSignature -LiteralPath $BootstrapperSource
if ($bootstrapperSignature.Status -ne 'Valid' -or $bootstrapperSignature.SignerCertificate.Subject -notmatch 'CN=Microsoft Corporation(?:,|$)') {
    throw 'The WebView2 bootstrapper must have a valid Microsoft signature.'
}
if (-not $IsccPath) { $IsccPath = & (Join-Path $PSScriptRoot 'install-inno.ps1') | Select-Object -Last 1 }
if (-not (Test-Path -LiteralPath $IsccPath -PathType Leaf)) { throw 'Inno compiler missing.' }
if ((Get-FileHash -LiteralPath $IsccPath -Algorithm SHA256).Hash.ToLowerInvariant() -ne '0a8757031b33777e4c9cbffee40f11a5062b36d25cbe144c1db73b6102b80ad7') {
    throw 'The Inno compiler does not match the pinned SHA-256.'
}
New-Item -ItemType Directory -Force -Path $OutputDirectory | Out-Null
$OutputDirectory = (Resolve-Path -LiteralPath $OutputDirectory).ProviderPath
$numericVersion = $Version -replace '-.*$', ''
$defines = @(
    "/DAppSource=$AppSource", "/DTdjsonSource=$TdjsonSource", "/DNoticesSource=$NoticesSource",
    "/DLicenseSource=$LicenseSource", "/DBootstrapperSource=$BootstrapperSource", "/DVersion=$Version",
    "/DNumericVersion=$numericVersion", "/DOutputDirectory=$OutputDirectory", "/DOutputFilename=$OutputFilename"
)
& $IsccPath @defines (Join-Path (Split-Path -Parent $PSScriptRoot) 'src-tauri\windows\inno\setup.iss')
if ($LASTEXITCODE -ne 0) { throw "Inno compilation failed with exit code $LASTEXITCODE" }
$output = Join-Path $OutputDirectory "$OutputFilename.exe"
if (-not (Test-Path -LiteralPath $output -PathType Leaf)) { throw 'Inno did not produce the expected installer.' }
Write-Output $output
