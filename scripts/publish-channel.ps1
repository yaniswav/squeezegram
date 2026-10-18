<#
.SYNOPSIS
    Copies the latest.json of a published release to the update channel release of its major.

.DESCRIPTION
    The app reads releases/download/channel-v<N>/latest.json (see scripts\lib\channel.ps1 and
    src-tauri\src\update\mod.rs), so 1.x builds keep seeing 1.x releases after a v2 is
    published. channel-v<N> is a fixed pre-release, never "latest", created once by the owner.
    This script replaces its latest.json asset. .github\workflows\publish-channel.yml runs it
    when the owner publishes a release; it can also be run by hand with an authenticated gh.

    It refuses anything it cannot prove correct, and never rebuilds the manifest: the file that
    release.yml built and the owner reviewed is copied as it is.
      - the tag must be vX.Y.Z (a suffix such as -rc.1 is never copied to a channel);
      - the release must be published, not a pre-release, with latest.json, the installer and its
        .sig as assets;
      - latest.json must name this version, point at this release's installer and carry a
        signature (the updater itself verifies that signature against the app's public key);
      - the channel release must exist and be a pre-release, not a draft;
      - the manifest already on the channel must belong to the same channel and must not be newer
        (an older release published late never rolls the channel back).
    Re-running it for the version already on the channel does nothing. After the upload it reads
    the public URL the app uses, retrying while GitHub's CDN catches up, and fails unless that
    URL now serves this version.

    One-time setup, by the owner (the script prints it again if the release is missing):
      gh release create channel-v1 --prerelease --latest=false --title "Update channel 0.x/1.x (not a download)" --notes "..."

.PARAMETER Tag
    Tag of the published release, vX.Y.Z.

.PARAMETER Repository
    GitHub repository as owner/name.

.PARAMETER DryRun
    Runs every check, then stops before the upload.

.EXAMPLE
    pwsh -File scripts\publish-channel.ps1 -Tag v1.0.1 -Repository yaniswav/squeezegram -DryRun
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string] $Tag,
    [Parameter(Mandatory)][string] $Repository,
    [switch] $DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Forward slash: this runs on a Linux runner, where a backslash is not a path separator.
. (Join-Path $PSScriptRoot 'lib/channel.ps1')

# Runs gh and returns its standard output. A non-zero exit code is an error that carries gh's
# own message (stderr), so a failed call can never be taken for an empty answer.
function Invoke-Gh([string[]] $Arguments) {
    $output = & gh @Arguments 2>&1
    $code = $LASTEXITCODE
    $messages = @($output | Where-Object { $_ -is [System.Management.Automation.ErrorRecord] } | ForEach-Object { $_.ToString() })
    if ($code -ne 0) { throw "gh $($Arguments[0]) $($Arguments[1]) failed with exit code ${code}: $($messages -join ' ')" }
    return @($output | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] })
}

function Get-ReleaseInfo([string] $ReleaseTag) {
    $json = (Invoke-Gh @('release', 'view', $ReleaseTag, '--repo', $Repository, '--json', 'isDraft,isPrerelease,assets')) -join "`n"
    return $json | ConvertFrom-Json
}

function Read-Manifest([string] $Path) {
    return Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
}

function Get-ManifestVersion($Manifest, [string] $What) {
    if (-not ($Manifest.PSObject.Properties.Name -contains 'version') -or [string]::IsNullOrWhiteSpace($Manifest.version)) {
        throw "$What has no version"
    }
    return [string] $Manifest.version
}

function Publish-Channel {
    param([string] $Work)

    # a. The tag decides the version and the channel.
    if ($Tag -notmatch '^v(\d+\.\d+\.\d+)$') { throw "Tag '$Tag' is not vX.Y.Z" }
    $version = $Matches[1]
    $channelTag = Get-ChannelTag $version
    $setup = "squeezegram-$version-setup-x64.exe"
    Write-Host "Release $Tag (version $version) belongs to channel $channelTag"

    # b. The release must be a published, final release with the files the app needs.
    $release = Get-ReleaseInfo $Tag
    if ($release.isDraft) { throw "$Tag is a draft" }
    if ($release.isPrerelease) { throw "$Tag is a pre-release: a pre-release is never copied to a channel" }
    $names = @($release.assets | ForEach-Object { $_.name })
    foreach ($required in 'latest.json', $setup, "$setup.sig") {
        if ($names -notcontains $required) { throw "$Tag has no asset $required (assets: $($names -join ', '))" }
    }

    # c. The manifest of the release is what gets copied; check it says what the release is.
    $newDir = Join-Path $Work 'new'
    Invoke-Gh @('release', 'download', $Tag, '--repo', $Repository, '--pattern', 'latest.json', '--dir', $newDir) | Out-Null
    $newFile = Join-Path $newDir 'latest.json'
    if (-not (Test-Path -LiteralPath $newFile -PathType Leaf)) { throw "gh did not download latest.json of $Tag" }
    $new = Read-Manifest $newFile
    $newVersion = Get-ManifestVersion $new "latest.json of $Tag"
    if ($newVersion -ne $version) { throw "latest.json of $Tag says version $newVersion, expected $version" }
    if (-not ($new.PSObject.Properties.Name -contains 'platforms') -or -not ($new.platforms.PSObject.Properties.Name -contains 'windows-x86_64')) {
        throw "latest.json of $Tag has no windows-x86_64 platform"
    }
    $platform = $new.platforms.'windows-x86_64'
    $expectedUrl = "https://github.com/$Repository/releases/download/$Tag/$setup"
    if ($platform.url -ne $expectedUrl) { throw "latest.json of $Tag points at '$($platform.url)', expected $expectedUrl" }
    if ([string]::IsNullOrWhiteSpace($platform.signature) -or $platform.signature -notmatch '^[A-Za-z0-9+/=]+$') {
        throw "latest.json of $Tag has no base64 signature"
    }

    # d. The channel release is created once by the owner; this script never creates it.
    try { $channel = Get-ReleaseInfo $channelTag }
    catch {
        $create = "gh release create $channelTag --prerelease --latest=false --title `"Update channel 0.x/1.x (not a download)`" --notes `"...`""
        throw "The channel release $channelTag cannot be read ($($_.Exception.Message)). If it does not exist, create it once: $create"
    }
    if ($channel.isDraft) { throw "$channelTag is a draft: the updater would not see it" }
    if (-not $channel.isPrerelease) { throw "$channelTag is not a pre-release: it would become GitHub's latest release" }

    # e. Never roll the channel back, never mix channels.
    if (@($channel.assets | ForEach-Object { $_.name }) -contains 'latest.json') {
        $currentDir = Join-Path $Work 'current'
        Invoke-Gh @('release', 'download', $channelTag, '--repo', $Repository, '--pattern', 'latest.json', '--dir', $currentDir) | Out-Null
        $current = Read-Manifest (Join-Path $currentDir 'latest.json')
        $currentVersion = Get-ManifestVersion $current "latest.json of $channelTag"
        if ((Get-ChannelTag $currentVersion) -ne $channelTag) { throw "latest.json of $channelTag says $currentVersion, which belongs to $(Get-ChannelTag $currentVersion)" }
        $order = Compare-ReleaseVersion $currentVersion $version
        if ($order -gt 0) { throw "${channelTag} is at ${currentVersion}: refusing to replace $currentVersion by $version" }
        if ($order -eq 0) { Write-Host "$channelTag is already at $version. Nothing to do."; return }
        Write-Host "$channelTag is at $currentVersion, will move to $version"
    }
    else { Write-Host "$channelTag has no latest.json yet" }

    # f.
    if ($DryRun) { Write-Host "Dry run: every check passed, $channelTag would be moved to $version. Nothing was uploaded."; return }
    Invoke-Gh @('release', 'upload', $channelTag, $newFile, '--repo', $Repository, '--clobber') | Out-Null

    # g. The app reads this URL: prove it serves the new version (GitHub's CDN may lag a little).
    $url = "https://github.com/$Repository/releases/download/$channelTag/latest.json"
    $served = $null
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        try {
            $response = Invoke-WebRequest -Uri $url -MaximumRedirection 5
            # Release assets come back as application/octet-stream: the body is then bytes.
            $text = if ($response.Content -is [byte[]]) { [Text.Encoding]::UTF8.GetString($response.Content) } else { [string] $response.Content }
            $served = ($text | ConvertFrom-Json).version
        }
        catch { $served = "unreadable ($($_.Exception.Message))" }
        if ($served -eq $version) { break }
        if ($attempt -lt 5) { Write-Host "Attempt ${attempt}: $url serves $served, expected $version. Waiting 10 s."; Start-Sleep -Seconds 10 }
    }
    if ($served -ne $version) { throw "$url still serves $served after the upload, expected $version" }
    Write-Host "$channelTag now serves $version at $url"
}

if ($Repository -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$') { throw "Repository '$Repository' is not owner/name" }
$base = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [IO.Path]::GetTempPath() }
$work = Join-Path $base ('publish-channel-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $work | Out-Null
try { Publish-Channel -Work $work }
finally { Remove-Item -LiteralPath $work -Recurse -Force -ErrorAction SilentlyContinue }
