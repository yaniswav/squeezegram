<#
.SYNOPSIS
    Builds dist-release\squeezegram-<version>-portable-x64.zip from the release build.

.DESCRIPTION
    The zip holds one folder, squeezegram-<version>-portable-x64\, with squeezegram.exe (release
    build), tdjson.dll (staged by scripts\stage-tdjson.ps1, hash checked again here), LICENSE,
    THIRD-PARTY-NOTICES.md and a short README-portable.txt. Run `pnpm tauri build` (or
    --no-bundle) and scripts\stage-tdjson.ps1 first; scripts\build-release.ps1 does it all.

    Like every local build, the executable has the Telegram api_id/api_hash from .env compiled
    in: do not publish a zip made on this machine.

.EXAMPLE
    pwsh -File scripts\make-portable.ps1
#>
[CmdletBinding()]
param(
    # Defaults to the version shared by package.json, Cargo.toml and tauri.conf.json.
    [string] $Version,
    [string] $ExePath,
    [string] $DllPath,
    [string] $OutDir
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
if (-not $Version) { $Version = (& (Join-Path $PSScriptRoot 'check-versions.ps1') | Select-Object -Last 1) }
if (-not $ExePath) { $ExePath = Join-Path $root 'src-tauri\target\release\squeezegram.exe' }
if (-not $DllPath) { $DllPath = Join-Path $root 'src-tauri\target\release-stage\tdjson.dll' }
if (-not $OutDir) { $OutDir = Join-Path $root 'dist-release' }
$DllSha256 = '048AF1D1C97EF9B860DD5DE132C4D2D901D444691EC86FC15012E5896DBDE6B8'

$inputs = [ordered]@{
    'squeezegram.exe'        = $ExePath
    'tdjson.dll'             = $DllPath
    'LICENSE'                = Join-Path $root 'LICENSE'
    'THIRD-PARTY-NOTICES.md' = Join-Path $root 'THIRD-PARTY-NOTICES.md'
}
foreach ($entry in $inputs.GetEnumerator()) {
    if (-not (Test-Path -LiteralPath $entry.Value -PathType Leaf)) {
        throw "Missing $($entry.Key): $($entry.Value) (an antivirus may have removed a fresh executable)"
    }
}
$dllActual = (Get-FileHash -LiteralPath $DllPath -Algorithm SHA256).Hash.ToUpperInvariant()
if ($dllActual -ne $DllSha256) { throw "tdjson.dll: SHA-256 $dllActual does not match the pinned $DllSha256" }

$readme = @'
squeezegram {VERSION} - portable build (Windows x64)

WHAT THIS IS
  squeezegram compresses videos and photos and sends them to Telegram groups and topics.
  This zip is the same program as the installer, without installing it: unzip the folder
  anywhere you can write and run squeezegram.exe. Keep tdjson.dll (the Telegram library)
  next to squeezegram.exe.

REQUIREMENTS
  Windows 10 or 11, 64-bit. Microsoft Edge WebView2 Runtime (already present on Windows 11
  and on up-to-date Windows 10). tdjson.dll needs the Microsoft Visual C++ Redistributable
  (x64).

FFMPEG
  ffmpeg is not in this zip. The app downloads it on first launch and keeps it in
  %LOCALAPPDATA%\squeezegram\sidecars.

WHERE YOUR DATA GOES (EVEN IN THE PORTABLE VERSION)
  "Portable" only means no installer. The app still writes outside its own folder, in your
  Windows user profile:
    %APPDATA%\squeezegram       settings and the send-queue database
    %LOCALAPPDATA%\squeezegram  Telegram session and database, ffmpeg, temporary files, logs
  Nothing is written to the registry by the app itself. To remove every trace, delete the
  program folder and those two folders.

UNSIGNED BUILD
  This build is not code-signed. Windows SmartScreen may show "Windows protected your PC"
  (More info, then Run anyway), and some antivirus programs may flag or quarantine it.
  Check the file you downloaded before running it.

VERIFY THE DOWNLOAD (SHA-256)
  In PowerShell, in the folder of the zip:
    Get-FileHash .\squeezegram-{VERSION}-portable-x64.zip -Algorithm SHA256
  The hash must equal the line for this file in SHA256SUMS.txt, published next to the
  download. If it differs, do not run it.

LICENSE
  See LICENSE (PolyForm Noncommercial 1.0.0) and THIRD-PARTY-NOTICES.md.
'@
$readme = $readme.Replace('{VERSION}', $Version) -replace "`r?`n", "`r`n"

$name = "squeezegram-$Version-portable-x64"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$zipPath = Join-Path (Resolve-Path -LiteralPath $OutDir).ProviderPath "$name.zip"
if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }

Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [System.IO.Compression.ZipFile]::Open($zipPath, [System.IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($entry in $inputs.GetEnumerator()) {
        [void][System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
            $zip, (Resolve-Path -LiteralPath $entry.Value).ProviderPath, "$name/$($entry.Key)",
            [System.IO.Compression.CompressionLevel]::Optimal)
    }
    $readmeEntry = $zip.CreateEntry("$name/README-portable.txt", [System.IO.Compression.CompressionLevel]::Optimal)
    $stream = $readmeEntry.Open()
    try {
        $bytes = (New-Object System.Text.UTF8Encoding($false)).GetBytes($readme)
        $stream.Write($bytes, 0, $bytes.Length)
    }
    finally { $stream.Dispose() }
}
finally { $zip.Dispose() }

Write-Host "Portable zip: $zipPath"
Write-Output $zipPath
