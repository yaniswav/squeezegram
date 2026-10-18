<#
.SYNOPSIS
    Checks scripts\lib\channel.ps1 (update-channel rules). Throws on the first failure.

.EXAMPLE
    pwsh -File scripts\tests\channel.tests.ps1
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

. (Join-Path (Split-Path -Parent $PSScriptRoot) 'lib\channel.ps1')

$checks = 0
function Assert-Equal($Actual, $Expected, [string] $What) {
    $script:checks++
    if ($Actual -ne $Expected) { throw "$What : expected '$Expected', got '$Actual'" }
}
function Assert-Throws([scriptblock] $Block, [string] $What) {
    $script:checks++
    $threw = $false
    try { & $Block | Out-Null } catch { $threw = $true }
    if (-not $threw) { throw "$What : expected an error" }
}

# A 0.x build reads channel 1, like 1.x; a new major gets its own channel.
Assert-Equal (Get-ChannelMajor 0) 1 'channel of major 0'
Assert-Equal (Get-ChannelMajor 1) 1 'channel of major 1'
Assert-Equal (Get-ChannelMajor 2) 2 'channel of major 2'
Assert-Equal (Get-ChannelMajor 10) 10 'channel of major 10'
Assert-Equal (Get-ChannelTag '0.1.0') 'channel-v1' 'tag of 0.1.0'
Assert-Equal (Get-ChannelTag '1.4.2') 'channel-v1' 'tag of 1.4.2'
Assert-Equal (Get-ChannelTag '2.0.0') 'channel-v2' 'tag of 2.0.0'
Assert-Throws { Get-ChannelTag '1.0.0-rc.1' } 'a pre-release version has no channel'
Assert-Throws { Get-ChannelTag '1.0' } 'a short version has no channel'
Assert-Throws { Get-ChannelTag 'v1.0.0' } 'a tag is not a version'

# Numeric, not textual: 0.1.10 is above 0.1.9.
Assert-Equal (Compare-ReleaseVersion '0.1.10' '0.1.9') 1 '0.1.10 vs 0.1.9'
Assert-Equal (Compare-ReleaseVersion '0.1.9' '0.1.10') -1 '0.1.9 vs 0.1.10'
Assert-Equal (Compare-ReleaseVersion '1.0.0' '0.9.9') 1 '1.0.0 vs 0.9.9'
Assert-Equal (Compare-ReleaseVersion '1.2.3' '1.2.3') 0 '1.2.3 vs 1.2.3'
Assert-Equal (Compare-ReleaseVersion '2.0.0' '10.0.0') -1 '2.0.0 vs 10.0.0'
Assert-Throws { Compare-ReleaseVersion '1.0.0-rc.1' '1.0.0' } 'compare refuses a suffix'

$ok1 = 'https://github.com/yaniswav/squeezegram/releases/download/channel-v1/latest.json'
$ok2 = 'https://github.com/yaniswav/squeezegram/releases/download/channel-v2/latest.json'
Assert-Equal (Test-ChannelEndpoint $ok1 '0.1.0') $true 'channel-v1 for 0.1.0'
Assert-Equal (Test-ChannelEndpoint $ok1 '1.4.2') $true 'channel-v1 for 1.4.2'
Assert-Equal (Test-ChannelEndpoint $ok2 '2.0.0') $true 'channel-v2 for 2.0.0'
Assert-Equal (Test-ChannelEndpoint $ok1 '2.0.0') $false 'channel-v1 for 2.0.0'
Assert-Equal (Test-ChannelEndpoint $ok2 '1.9.0') $false 'channel-v2 for 1.9.0'
Assert-Equal (Test-ChannelEndpoint $ok2 '0.1.0') $false 'channel-v2 for 0.1.0'
Assert-Equal (Test-ChannelEndpoint 'https://github.com/other/squeezegram/releases/download/channel-v1/latest.json' '1.0.0') $false 'other owner'
Assert-Equal (Test-ChannelEndpoint 'https://github.com/yaniswav/other/releases/download/channel-v1/latest.json' '1.0.0') $false 'other repository'
Assert-Equal (Test-ChannelEndpoint "${ok1}?x=1" '1.0.0') $false 'query string'
Assert-Equal (Test-ChannelEndpoint 'https://github.com/yaniswav/squeezegram/releases/latest/download/latest.json' '1.0.0') $false 'GitHub latest'
Assert-Equal (Test-ChannelEndpoint 'https://github.com/yaniswav/squeezegram/releases/download/channel-v{{current_version}}/latest.json' '1.0.0') $false 'placeholder'
Assert-Equal (Test-ChannelEndpoint 'http://github.com/yaniswav/squeezegram/releases/download/channel-v1/latest.json' '1.0.0') $false 'plain http'
Assert-Equal (Test-ChannelEndpoint $ok1 'not a version') $false 'unreadable version'

Write-Host "channel.tests.ps1: OK ($checks checks)"
