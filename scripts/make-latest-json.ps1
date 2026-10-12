<#
.SYNOPSIS
    Writes latest.json, the update manifest read by the Tauri 2 updater plugin.

.DESCRIPTION
    The app asks https://github.com/<repository>/releases/latest/download/latest.json
    (plugins.updater.endpoints in src-tauri\tauri.conf.json). The file has the static JSON
    format of the plugin:

      {
        "version":  "<version>",
        "notes":    "<text>",
        "pub_date": "<RFC 3339, UTC>",
        "platforms": {
          "windows-x86_64": { "signature": "<content of the .sig file>", "url": "<asset URL>" }
        }
      }

    "signature" is the content of the installer's .sig file, written by `pnpm tauri build`
    when TAURI_SIGNING_PRIVATE_KEY is set (see scripts\build-release.ps1). It is a public
    value (a signature, not a key). "url" is the download URL of the installer as an asset of
    the GitHub release for the tag (default v<version>); that URL only works once the release
    is published. The installer's name is the one scripts\build-release.ps1 gives it:
    squeezegram-<version>-setup-x64.exe.

    Nothing is read from the environment and no key is needed.

.EXAMPLE
    pwsh -File scripts\make-latest-json.ps1
.EXAMPLE
    pwsh -File scripts\make-latest-json.ps1 -Version 0.1.0 -SignatureFile C:\tmp\fake.sig -OutFile C:\tmp\latest.json
#>
[CmdletBinding()]
param(
    # Defaults to the version shared by package.json, Cargo.toml and tauri.conf.json.
    [string] $Version,
    # The installer's .sig (default: dist-release\squeezegram-<version>-setup-x64.exe.sig).
    [string] $SignatureFile,
    # GitHub repository that hosts the release, as owner/name.
    [string] $Repository = 'yaniswav/squeezegram',
    # Release tag (default: v<version>).
    [string] $Tag,
    # Installer file name as uploaded to the release (default: squeezegram-<version>-setup-x64.exe).
    [string] $AssetName,
    # Text shown by the app next to the update (default: a pointer to the release page).
    [string] $Notes,
    # Publication date; defaults to now, UTC.
    [datetime] $PubDate,
    # Output file (default: dist-release\latest.json).
    [string] $OutFile
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
if (-not $Version) { $Version = (& (Join-Path $PSScriptRoot 'check-versions.ps1') | Select-Object -Last 1) }
if ($Version -notmatch '^\d+\.\d+\.\d+([-+][0-9A-Za-z.-]+)?$') { throw "Version '$Version' is not a semantic version" }
if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') { throw "Repository '$Repository' is not owner/name" }
if (-not $Tag) { $Tag = "v$Version" }
if (-not $AssetName) { $AssetName = "squeezegram-$Version-setup-x64.exe" }
if ($AssetName -notmatch '^[A-Za-z0-9_.-]+$') { throw "Asset name '$AssetName' has characters that would need URL encoding" }
if ($Tag -notmatch '^[A-Za-z0-9_.+-]+$') { throw "Tag '$Tag' has characters that would need URL encoding" }
$dist = Join-Path $root 'dist-release'
if (-not $SignatureFile) { $SignatureFile = Join-Path $dist "$AssetName.sig" }
if (-not $OutFile) { $OutFile = Join-Path $dist 'latest.json' }
if (-not $Notes) { $Notes = "squeezegram $Version. Release notes: https://github.com/$Repository/releases/tag/$Tag" }
if (-not $PSBoundParameters.ContainsKey('PubDate')) { $PubDate = [datetime]::UtcNow }

if (-not (Test-Path -LiteralPath $SignatureFile -PathType Leaf)) {
    throw "Signature file not found: $SignatureFile (was the build made with TAURI_SIGNING_PRIVATE_KEY set?)"
}
$signature = (Get-Content -LiteralPath $SignatureFile -Raw).Trim()
# A Tauri .sig is one base64 line (the base64 of a minisign signature). Anything else is not one.
if ($signature -notmatch '^[A-Za-z0-9+/=]+$') { throw "$SignatureFile does not look like a Tauri signature (expected one base64 line)" }

$manifest = [ordered]@{
    version   = $Version
    notes     = $Notes
    pub_date  = $PubDate.ToUniversalTime().ToString("yyyy-MM-dd'T'HH:mm:ss'Z'", [Globalization.CultureInfo]::InvariantCulture)
    platforms = [ordered]@{
        'windows-x86_64' = [ordered]@{
            signature = $signature
            url       = "https://github.com/$Repository/releases/download/$Tag/$AssetName"
        }
    }
}
$json = $manifest | ConvertTo-Json -Depth 5

$outDir = Split-Path -Parent ([IO.Path]::GetFullPath($OutFile))
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
[IO.File]::WriteAllText([IO.Path]::GetFullPath($OutFile), $json + "`n", (New-Object System.Text.UTF8Encoding($false)))

Write-Host "Wrote $OutFile"
Write-Host "  version $Version, url https://github.com/$Repository/releases/download/$Tag/$AssetName"
Write-Output ([IO.Path]::GetFullPath($OutFile))
