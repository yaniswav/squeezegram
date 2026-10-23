<#
.SYNOPSIS
    Creates the GitHub release that hosts the pinned FFmpeg build with its corresponding source.

.DESCRIPTION
    The app downloads ffmpeg from this project's release (src-tauri\src\sidecars\mod.rs,
    pinned_build), because BtbN deletes its builds and tags after about 14 days. Hosting the
    binary makes this project its distributor, so the same release must hold the corresponding
    source: the FFmpeg source and the BtbN build scripts at the build's commits (see section 5 of
    notices\THIRD-PARTY-NOTICES.template.txt).

    Run once by the owner with an authenticated gh. It checks the build's size and SHA-256 before
    any upload, refuses to touch a release that already exists, and afterwards checks that the
    public URL the app uses serves the expected size.

.PARAMETER ZipPath
    A local copy of the build. Without it the build is downloaded from BtbN, which only works
    while BtbN still has that release (about 14 days after 2026-10-22).

.PARAMETER DryRun
    Downloads and checks everything, uploads nothing.

.EXAMPLE
    pwsh -File scripts\mirror-ffmpeg.ps1 -DryRun
    pwsh -File scripts\mirror-ffmpeg.ps1
#>
param(
    [string]$Repository = 'yaniswav/squeezegram',
    [string]$ZipPath,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'

# Keep in step with pinned_build() and the notices template.
$Tag = 'ffmpeg-n9.0.2-33'
$BtbnTag = 'autobuild-2026-10-22-13-06'
$ZipName = 'ffmpeg-n9.0.2-33-g4cf298e66a-win64-lgpl-9.0.zip'
$ZipSize = 171482264
$ZipSha256 = 'ebc6e5d6c51856b402c347f0c4cfca70c4bc43c85f7b1148237b92742d81d16f'
$FfmpegCommit = '4cf298e66a20dd90e8b7c62333ab5812596835ec'
$BtbnCommit = '937868360a21d711ec94a33f5591d345ad9b7754'

function Invoke-Gh {
    & gh @args
    if ($LASTEXITCODE -ne 0) { throw "gh $($args[0]) $($args[1]) failed" }
}

$work = Join-Path ([IO.Path]::GetTempPath()) "squeezegram-ffmpeg-mirror-$PID"
New-Item -ItemType Directory -Force $work | Out-Null
try {
    $zip = Join-Path $work $ZipName
    if ($ZipPath) {
        Copy-Item -LiteralPath $ZipPath -Destination $zip
    } else {
        Invoke-Gh release download $BtbnTag --repo BtbN/FFmpeg-Builds --pattern $ZipName --dir $work
    }
    if ((Get-Item -LiteralPath $zip).Length -ne $ZipSize) { throw "$ZipName does not have the pinned size" }
    if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant() -ne $ZipSha256) {
        throw "$ZipName does not match the pinned SHA-256"
    }

    $ffmpegSource = Join-Path $work "ffmpeg-source-$($FfmpegCommit.Substring(0, 10)).tar.gz"
    $scriptsSource = Join-Path $work "ffmpeg-builds-scripts-$($BtbnCommit.Substring(0, 7)).tar.gz"
    Invoke-WebRequest -Uri "https://github.com/FFmpeg/FFmpeg/archive/$FfmpegCommit.tar.gz" -OutFile $ffmpegSource
    Invoke-WebRequest -Uri "https://github.com/BtbN/FFmpeg-Builds/archive/$BtbnCommit.tar.gz" -OutFile $scriptsSource

    $assets = @($zip, $ffmpegSource, $scriptsSource)
    $sums = Join-Path $work 'SHA256SUMS.txt'
    $assets | ForEach-Object {
        "$((Get-FileHash -LiteralPath $_ -Algorithm SHA256).Hash.ToLowerInvariant())  $(Split-Path $_ -Leaf)"
    } | Set-Content -LiteralPath $sums -Encoding ascii
    Get-Content -LiteralPath $sums

    $notes = @"
FFmpeg for squeezegram (not an app release). The app downloads the zip below when the user
presses "Download ffmpeg (LGPL)" and checks its SHA-256 ($ZipSha256).

Unmodified LGPL v3 build of BtbN FFmpeg-Builds, release $BtbnTag.
Corresponding source, attached here:
- FFmpeg commit $FfmpegCommit
- BtbN FFmpeg-Builds commit $BtbnCommit (build scripts, patches, bundled library versions)
"@

    & gh release view $Tag --repo $Repository *> $null
    if ($LASTEXITCODE -eq 0) { throw "release $Tag already exists in ${Repository}: nothing changed" }
    if ($DryRun) {
        Write-Output "Dry run: would create $Tag in $Repository with $($assets.Count + 1) assets."
        return
    }
    Invoke-Gh release create $Tag --repo $Repository --prerelease --latest=false `
        --title "FFmpeg n9.0.2-33 for squeezegram (not an app release)" --notes $notes `
        @assets $sums

    $url = "https://github.com/$Repository/releases/download/$Tag/$ZipName"
    $head = Invoke-WebRequest -Uri $url -Method Head
    if ([int64]$head.Headers['Content-Length'][0] -ne $ZipSize) { throw "$url does not serve the pinned size" }
    Write-Output "OK: $url"
} finally {
    Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue
}
