<#
.SYNOPSIS
    Pester checks for update-channel rules and the signature binding before publication.
.EXAMPLE
    Invoke-Pester -Path ./scripts/tests/channel.tests.ps1 -EnableExit
#>

Describe 'Update-channel rules' {
    It 'preserves channel selection, version ordering and endpoint checks' {
        . (Join-Path (Split-Path -Parent $PSScriptRoot) 'lib/channel.ps1')
        $script:checks = 0
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

        Write-Host "channel.tests.ps1: OK ($script:checks checks)"
    }
}

Describe 'Updater signature structure and binding' {
    BeforeAll {
        $script:scripts = Split-Path -Parent $PSScriptRoot
        . (Join-Path $script:scripts 'lib/updater-signature.ps1')
        $script:name = 'squeezegram-1.2.3-setup-x64.exe'
        $script:comment = "trusted comment: timestamp:1234567890`tfile:$script:name`tversion:1.2.3"

        # Deliberately synthetic: no private key or cryptographic validity is claimed.
        function New-SyntheticSignature([string] $Comment = $script:comment, [string] $NewLine = "`n") {
            $packet = New-Object byte[] 74
            $packet[0] = 69; $packet[1] = 68
            $lines = @('untrusted comment: synthetic test', [Convert]::ToBase64String($packet), $Comment, [Convert]::ToBase64String((New-Object byte[] 64)))
            return [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(($lines -join $NewLine) + $NewLine))
        }
        function Assert-Refused([scriptblock] $Action, [string] $Message) {
            $caught = $null
            try { & $Action | Out-Null } catch { $caught = $_ }
            if ($null -eq $caught -or $caught.Exception.Message -notmatch $Message) {
                throw "Expected refusal matching '$Message', got '$caught'"
            }
        }
        function Assert-Fixture([string] $Signature) {
            Assert-UpdaterSignature -Signature $Signature -Version '1.2.3' -AssetName $script:name
        }
        function Replace-SignatureLine([int] $Index, [string] $Value) {
            $text = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String((New-SyntheticSignature)))
            $lines = $text.Split([char]10)
            $lines[$Index] = $Value
            return [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes(($lines -join "`n")))
        }

        # No gh executable or network is used. Unknown operations, including writes, fail.
        function gh {
            $a = @($args)
            $calls.Add(($a -join ' '))
            $global:LASTEXITCODE = 0
            if ($a[0] -eq 'release' -and $a[1] -eq 'view') {
                if ($a[2] -eq 'v1.2.3') {
                    return @{ isDraft = $false; isPrerelease = $false; assets = @('latest.json', $name, "$name.sig") | ForEach-Object { @{name = $_} } } | ConvertTo-Json -Depth 5
                }
                if ($a[2] -eq 'channel-v1') {
                    return @{ isDraft = $false; isPrerelease = $true; assets = @() } | ConvertTo-Json
                }
            }
            if ($a[0] -eq 'release' -and $a[1] -eq 'download' -and $a[2] -eq 'v1.2.3') {
                $dir = $a[([array]::IndexOf($a, '--dir') + 1)]
                $asset = $a[([array]::IndexOf($a, '--pattern') + 1)]
                New-Item -ItemType Directory -Force -Path $dir | Out-Null
                if ($asset -eq 'latest.json') {
                    @{ version = '1.2.3'; platforms = @{ 'windows-x86_64' = @{ signature = $manifestSignature; url = "https://github.com/yaniswav/squeezegram/releases/download/v1.2.3/$name" } } } | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $dir $asset)
                    return
                }
                if ($asset -eq "$name.sig") {
                    Set-Content -LiteralPath (Join-Path $dir $asset) -Value $assetSignature
                    return
                }
            }
            throw "Unexpected gh call: $($a -join ' ')"
        }
        function Invoke-TestPublish([switch] $DryRun) {
            & (Join-Path $script:scripts 'publish-channel.ps1') -Tag 'v1.2.3' -Repository 'yaniswav/squeezegram' -DryRun:$DryRun
        }
    }
    BeforeEach {
        $script:manifestSignature = New-SyntheticSignature
        $script:assetSignature = $script:manifestSignature
        $script:calls = New-Object 'System.Collections.Generic.List[string]'
    }

    It 'accepts a matching minisign envelope' { Assert-Fixture (New-SyntheticSignature) }
    It 'accepts CRLF minisign lines' { Assert-Fixture (New-SyntheticSignature -NewLine "`r`n") }
    It 'accepts a minisign envelope without its final newline' {
        $text = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String((New-SyntheticSignature))).TrimEnd([char]10)
        Assert-Fixture ([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($text)))
    }
    It 'rejects a wrong version' {
        Assert-Refused { Assert-Fixture (New-SyntheticSignature ($script:comment.Replace('version:1.2.3', 'version:1.2.4'))) } 'trusted comment'
    }
    It 'rejects a wrong file' {
        Assert-Refused { Assert-Fixture (New-SyntheticSignature ($script:comment.Replace($script:name, 'other.exe'))) } 'trusted comment'
    }
    It 'rejects a missing version' {
        Assert-Refused { Assert-Fixture (New-SyntheticSignature ($script:comment.Replace("`tversion:1.2.3", ''))) } 'trusted comment'
    }
    It 'rejects a tampered comment with an appended field' {
        Assert-Refused { Assert-Fixture (New-SyntheticSignature ($script:comment + "`tversion:9.9.9")) } 'trusted comment'
    }
    It 'rejects a second trusted comment' {
        Assert-Refused { Assert-Fixture (New-SyntheticSignature ($script:comment + "`n" + $script:comment)) } 'four-line'
    }
    It 'rejects file name case changes' {
        Assert-Refused { Assert-Fixture (New-SyntheticSignature ($script:comment.Replace('squeezegram', 'Squeezegram'))) } 'trusted comment'
    }
    It 'rejects invalid outer base64' { Assert-Refused { Assert-Fixture 'not-base64!' } 'base64' }
    It 'rejects non-canonical outer base64' { Assert-Refused { Assert-Fixture 'AB==' } 'Non-canonical' }
    It 'rejects a base64 blob that is not minisign' { Assert-Refused { Assert-Fixture 'YWJjZA==' } 'four-line' }
    It 'rejects invalid UTF-8' { Assert-Refused { Assert-Fixture '/w==' } 'GetString' }
    It 'rejects a short signature packet' { Assert-Refused { Assert-Fixture (Replace-SignatureLine 1 'YWJjZA==') } 'signature packet' }
    It 'rejects an unknown algorithm' {
        Assert-Refused { Assert-Fixture (Replace-SignatureLine 1 ([Convert]::ToBase64String((New-Object byte[] 74)))) } 'signature packet'
    }
    It 'rejects a short global signature' { Assert-Refused { Assert-Fixture (Replace-SignatureLine 3 'YWJjZA==') } 'global signature' }
    It 'rejects an invalid untrusted comment prefix' { Assert-Refused { Assert-Fixture (Replace-SignatureLine 0 'other comment') } 'four-line' }
    It 'rejects invalid inner base64' { Assert-Refused { Assert-Fixture (Replace-SignatureLine 1 'not-base64!') } 'base64' }

    It 'allows matching release assets through the publication dry run' {
        Invoke-TestPublish -DryRun
        if ($script:calls.Count -ne 4 -or $script:calls[3] -notmatch '^release view channel-v1 ') { throw 'Expected all pre-upload checks' }
    }
    It 'refuses a wrong version before accessing the channel' {
        $script:assetSignature = New-SyntheticSignature ($script:comment.Replace('version:1.2.3', 'version:1.999.0'))
        $script:manifestSignature = $script:assetSignature
        Assert-Refused { Invoke-TestPublish } 'trusted comment'
        if ($script:calls.Count -ne 3) { throw 'Expected refusal before channel operations' }
    }
    It 'refuses a wrong file before accessing the channel' {
        $script:assetSignature = New-SyntheticSignature ($script:comment.Replace($script:name, 'other.exe'))
        $script:manifestSignature = $script:assetSignature
        Assert-Refused { Invoke-TestPublish } 'trusted comment'
        if ($script:calls.Count -ne 3) { throw 'Expected refusal before channel operations' }
    }
    It 'refuses an appended comment field before accessing the channel' {
        $script:assetSignature = New-SyntheticSignature ($script:comment + "`tfile:other.exe")
        $script:manifestSignature = $script:assetSignature
        Assert-Refused { Invoke-TestPublish } 'trusted comment'
        if ($script:calls.Count -ne 3) { throw 'Expected refusal before channel operations' }
    }
    It 'refuses a signature array in the manifest before accessing the channel' {
        $script:manifestSignature = @($script:assetSignature, $script:assetSignature)
        Assert-Refused { Invoke-TestPublish } 'does not match.*sig'
        if ($script:calls.Count -ne 3) { throw 'Expected refusal before channel operations' }
    }
    It 'refuses different signature assets before accessing the channel' {
        $script:manifestSignature = New-SyntheticSignature ($script:comment.Replace('1234567890', '1234567891'))
        Assert-Refused { Invoke-TestPublish } 'does not match.*sig'
        if ($script:calls.Count -ne 3) { throw 'Expected refusal before channel operations' }
    }
    It 'documents that an in-format forged comment still passes without cryptography' {
        Assert-Fixture (New-SyntheticSignature ($script:comment.Replace('1234567890', '1234567891')))
    }
    It 'writes a manifest for a matching signature' {
        $sig = Join-Path $TestDrive 'matching.sig'
        $out = Join-Path $TestDrive 'latest.json'
        Set-Content -LiteralPath $sig -Value (New-SyntheticSignature)
        & (Join-Path $script:scripts 'make-latest-json.ps1') -Version '1.2.3' -SignatureFile $sig -OutFile $out
        $manifest = Get-Content -LiteralPath $out -Raw | ConvertFrom-Json
        if ($manifest.version -cne '1.2.3' -or $manifest.platforms.'windows-x86_64'.signature -cne (New-SyntheticSignature)) { throw 'Manifest changed the signature or version' }
    }
    It 'refuses to write a manifest without a signed version' {
        $sig = Join-Path $TestDrive 'missing-version.sig'
        $out = Join-Path $TestDrive 'refused.json'
        Set-Content -LiteralPath $sig -Value (New-SyntheticSignature ($script:comment.Replace("`tversion:1.2.3", '')))
        Assert-Refused { & (Join-Path $script:scripts 'make-latest-json.ps1') -Version '1.2.3' -SignatureFile $sig -OutFile $out } 'trusted comment'
        if (Test-Path -LiteralPath $out) { throw 'A rejected signature produced a manifest' }
    }
    It 'refuses to write a manifest for a different release tag' {
        $sig = Join-Path $TestDrive 'wrong-tag.sig'
        $out = Join-Path $TestDrive 'wrong-tag.json'
        Set-Content -LiteralPath $sig -Value (New-SyntheticSignature)
        Assert-Refused { & (Join-Path $script:scripts 'make-latest-json.ps1') -Version '1.2.3' -Tag 'v1.2.4' -SignatureFile $sig -OutFile $out } 'does not match version'
        if (Test-Path -LiteralPath $out) { throw 'A rejected tag produced a manifest' }
    }
}
