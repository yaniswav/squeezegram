<#
.SYNOPSIS
    Update-channel rules shared by check-versions.ps1, publish-channel.ps1 and the tests. Dot-source it.

.DESCRIPTION
    The app reads its update manifest from a fixed pre-release, channel-v<N>, never from GitHub's
    "latest" release: once a v2 is published, "latest" points at it and 1.x builds would stop
    seeing 1.x fixes. Channel 1 serves 0.x and 1.x (a licence sold for major 1 covers both), so
    the channel of a major is max(1, major). These functions are the single place that says so;
    src-tauri\src\update\mod.rs (channel_major) must agree with them.
#>

Set-StrictMode -Version Latest

# Channel number for an app major: 0.x builds share channel 1.
function Get-ChannelMajor([int] $Major) {
    if ($Major -lt 0) { throw "Major $Major is negative" }
    if ($Major -eq 0) { return 1 }
    return $Major
}

# Name of the channel release for a plain X.Y.Z version. A version with a suffix is refused:
# a pre-release must never be copied to a channel.
function Get-ChannelTag([string] $Version) {
    if ($Version -notmatch '^(\d+)\.\d+\.\d+$') { throw "Version '$Version' is not a plain X.Y.Z version" }
    return 'channel-v{0}' -f (Get-ChannelMajor ([int] $Matches[1]))
}

# Numeric major.minor.patch comparison of two plain versions: -1, 0 or 1 (a below, equal, above b).
function Compare-ReleaseVersion([string] $A, [string] $B) {
    $parts = foreach ($version in $A, $B) {
        if ($version -notmatch '^(\d+)\.(\d+)\.(\d+)$') { throw "Version '$version' is not a plain X.Y.Z version" }
        , @([bigint] $Matches[1], [bigint] $Matches[2], [bigint] $Matches[3])
    }
    for ($i = 0; $i -lt 3; $i++) {
        if ($parts[0][$i] -lt $parts[1][$i]) { return -1 }
        if ($parts[0][$i] -gt $parts[1][$i]) { return 1 }
    }
    return 0
}

# Whether the updater endpoint is the manifest of the channel of the app's own major. Only the
# exact URL is accepted: a query, a placeholder or another repository is not. $Version may carry
# a suffix (1.0.0-rc.1): only its major matters here.
function Test-ChannelEndpoint([string] $Url, [string] $Version) {
    if ($Url -notmatch '^https://github\.com/yaniswav/squeezegram/releases/download/channel-v(\d+)/latest\.json$') { return $false }
    $channel = [bigint] $Matches[1]
    if ($Version -notmatch '^(\d+)\.\d+\.\d+') { return $false }
    return $channel -eq (Get-ChannelMajor ([int] $Matches[1]))
}
