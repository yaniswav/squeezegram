<#
.SYNOPSIS
    Stages the pinned Inno Setup compiler without a machine-wide installation.
#>
[CmdletBinding()]
param([string] $Directory, [switch] $CheckOnly)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if (-not $Directory) {
    $Directory = Join-Path (Split-Path -Parent $PSScriptRoot) 'src-tauri\target\build-tools\inno-6.7.3'
}
$compiler = Join-Path $Directory 'ISCC.exe'
$compilerHash = '0a8757031b33777e4c9cbffee40f11a5062b36d25cbe144c1db73b6102b80ad7'

function Assert-Compiler {
    if (-not (Test-Path -LiteralPath $compiler -PathType Leaf)) { throw "Compiler missing: $compiler" }
    if ((Get-FileHash -LiteralPath $compiler -Algorithm SHA256).Hash.ToLowerInvariant() -ne $compilerHash) {
        throw 'The Inno compiler does not match the pinned SHA-256.'
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $compiler
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'CN=Pyrsys B\.V\.') {
        throw 'The Inno compiler does not have a valid Pyrsys signature.'
    }
}

if (-not (Test-Path -LiteralPath $compiler -PathType Leaf)) {
    if ($CheckOnly) { throw 'Pinned Inno Setup is absent. Run scripts/install-inno.ps1.' }
    New-Item -ItemType Directory -Force -Path $Directory | Out-Null
    $package = Join-Path $Directory 'innosetup-6.7.3.exe'
    Invoke-WebRequest -Uri 'https://github.com/jrsoftware/issrc/releases/download/is-6_7_3/innosetup-6.7.3.exe' -OutFile $package
    if ((Get-FileHash -LiteralPath $package -Algorithm SHA256).Hash.ToLowerInvariant() -ne '9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732') {
        throw 'The Inno toolchain download does not match the pinned SHA-256.'
    }
    $signature = Get-AuthenticodeSignature -LiteralPath $package
    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Subject -notmatch 'CN=Pyrsys B\.V\.') {
        throw 'The Inno toolchain download does not have a valid Pyrsys signature.'
    }
    # Only the verified compiler toolchain is unpacked; the application installer is never run.
    $process = Start-Process -FilePath $package -ArgumentList @('/PORTABLE=1', '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', ('/DIR="{0}"' -f $Directory)) -WindowStyle Hidden -Wait -PassThru
    if ($process.ExitCode -ne 0) { throw "Inno toolchain extraction failed: $($process.ExitCode)" }
}
Assert-Compiler

$indonesian = Join-Path $Directory 'Languages\Indonesian.isl'
if (-not (Test-Path -LiteralPath $indonesian -PathType Leaf)) {
    if ($CheckOnly) { throw 'Pinned Indonesian translation is absent.' }
    Invoke-WebRequest -Uri 'https://raw.githubusercontent.com/jrsoftware/issrc/is-6_7_3/Files/Languages/Unofficial/Indonesian.isl' -OutFile $indonesian
}
if ((Get-FileHash -LiteralPath $indonesian -Algorithm SHA256).Hash.ToLowerInvariant() -ne 'eda27ca7d4b0bd666d7ed0cc91a856842a0101d7cff12417998cfd1e12c92fba') {
    throw 'The Indonesian translation does not match the pinned SHA-256.'
}
Write-Output $compiler
