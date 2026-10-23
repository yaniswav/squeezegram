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
                if ($asset -eq $name) {
                    if (-not $missingInstaller) { Set-Content -LiteralPath (Join-Path $dir $asset) -Value 'installer bytes' }
                    return
                }
            }
            throw "Unexpected gh call: $($a -join ' ')"
        }
        function Invoke-TestPublish([switch] $DryRun) {
            & (Join-Path $script:scripts 'publish-channel.ps1') -Tag 'v1.2.3' -Repository 'yaniswav/squeezegram' -Minisign $script:fakeMinisign -DryRun:$DryRun
        }
        # A fake verifier: records its arguments and exits with the code the test chose.
        $script:fakeMinisign = Join-Path $TestDrive 'fake-minisign.ps1'
        Set-Content -LiteralPath $script:fakeMinisign -Value '$env:FAKE_MINISIGN_ARGS = $args -join " "; exit [int] $env:FAKE_MINISIGN_EXIT'
    }
    BeforeEach {
        $script:manifestSignature = New-SyntheticSignature
        $script:assetSignature = $script:manifestSignature
        $script:calls = New-Object 'System.Collections.Generic.List[string]'
        $script:missingInstaller = $false
        $env:FAKE_MINISIGN_EXIT = '0'
        $env:FAKE_MINISIGN_ARGS = ''
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
        if ($script:calls.Count -ne 5 -or $script:calls[4] -notmatch '^release view channel-v1 ') { throw 'Expected all pre-upload checks' }
        if ($env:FAKE_MINISIGN_ARGS -notmatch '^-V -m .*squeezegram-1\.2\.3-setup-x64\.exe -x .*installer\.minisig -p .*updater\.pub$') { throw "Unexpected minisign arguments: $env:FAKE_MINISIGN_ARGS" }
    }
    It 'refuses a failed cryptographic verification before accessing the channel' {
        $env:FAKE_MINISIGN_EXIT = '1'
        Assert-Refused { Invoke-TestPublish } 'minisign rejected'
        if ($script:calls.Count -ne 4 -or ($script:calls -join ';') -match 'channel-v1') { throw 'Expected refusal before channel operations' }
    }
    It 'refuses a missing installer before accessing the channel' {
        $script:missingInstaller = $true
        Assert-Refused { Invoke-TestPublish } 'did not download'
        if ($script:calls.Count -ne 4 -or ($script:calls -join ';') -match 'channel-v1') { throw 'Expected refusal before channel operations' }
        if ($env:FAKE_MINISIGN_ARGS) { throw 'The verifier ran without an installer' }
    }
    It 'refuses a missing verifier binary before accessing the channel' {
        Assert-Refused { & (Join-Path $script:scripts 'publish-channel.ps1') -Tag 'v1.2.3' -Repository 'yaniswav/squeezegram' -Minisign (Join-Path $TestDrive 'absent') } 'binary not found'
        if ($script:calls.Count -ne 4) { throw 'Expected refusal before channel operations' }
    }
    It 'verifies with the real minisign binary when MINISIGN_BIN is set' -Skip:(-not $env:MINISIGN_BIN) {
        $dir = Join-Path $TestDrive 'real'
        New-Item -ItemType Directory -Force -Path $dir | Out-Null
        $file = Join-Path $dir 'installer.exe'
        Set-Content -LiteralPath $file -Value 'installer bytes'
        # Throwaway key pair, no password; it never leaves the test drive.
        & $env:MINISIGN_BIN -G -W -p (Join-Path $dir 'k.pub') -s (Join-Path $dir 'k.key') | Out-Null
        & $env:MINISIGN_BIN -S -s (Join-Path $dir 'k.key') -x (Join-Path $dir 'i.minisig') -t "timestamp:1`tfile:installer.exe`tversion:1.2.3" -m $file | Out-Null
        $sig = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $dir 'i.minisig')))
        $pub = [Convert]::ToBase64String([IO.File]::ReadAllBytes((Join-Path $dir 'k.pub')))
        Assert-UpdaterSignatureValid -Minisign $env:MINISIGN_BIN -Installer $file -Signature $sig -PublicKey $pub -Work (Join-Path $dir 'w1')
        Set-Content -LiteralPath $file -Value 'tampered bytes'
        Assert-Refused { Assert-UpdaterSignatureValid -Minisign $env:MINISIGN_BIN -Installer $file -Signature $sig -PublicKey $pub -Work (Join-Path $dir 'w2') } 'minisign rejected'
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
    # One release-version rule in every script: X.Y.Z or X.Y.Z-prerelease; no build metadata, no '+' in installer names.
    It 'writes a manifest for plain and prerelease versions' {
        foreach ($v in '1.2.3', '1.2.3-rc.1') {
            $name = "squeezegram-$v-setup-x64.exe"
            $sig = Join-Path $TestDrive "$v.sig"
            $out = Join-Path $TestDrive "$v.json"
            Set-Content -LiteralPath $sig -Value (New-SyntheticSignature "trusted comment: timestamp:1234567890`tfile:$name`tversion:$v")
            & (Join-Path $script:scripts 'make-latest-json.ps1') -Version $v -SignatureFile $sig -OutFile $out
            $manifest = Get-Content -LiteralPath $out -Raw | ConvertFrom-Json
            if ($manifest.version -cne $v -or $manifest.platforms.'windows-x86_64'.url -cne "https://github.com/yaniswav/squeezegram/releases/download/v$v/$name") { throw "Manifest changed the version or URL for $v" }
        }
    }
    It 'refuses build metadata and malformed versions' {
        foreach ($v in '1.2.3+b', '1.2.3-rc.1+b', '1.2', 'v1.2.3', '1.2.3-') {
            $out = Join-Path $TestDrive 'malformed.json'
            Assert-Refused { & (Join-Path $script:scripts 'make-latest-json.ps1') -Version $v -SignatureFile (Join-Path $TestDrive 'none.sig') -OutFile $out } 'must be X\.Y\.Z'
            if (Test-Path -LiteralPath $out) { throw "A rejected version produced a manifest: $v" }
        }
    }
    It 'refuses a plus sign in the installer name' {
        $out = Join-Path $TestDrive 'plus.json'
        Assert-Refused { & (Join-Path $script:scripts 'make-latest-json.ps1') -Version '1.2.3' -AssetName 'squeezegram-1.2.3+b-setup-x64.exe' -SignatureFile (Join-Path $TestDrive 'none.sig') -OutFile $out } 'URL encoding'
        if (Test-Path -LiteralPath $out) { throw 'A rejected installer name produced a manifest' }
    }
}

Describe 'Version rule in check-versions.ps1 and build-inno-installer.ps1' {
    It 'check-versions accepts plain and prerelease versions and refuses the rest' {
        $scripts = Split-Path -Parent $PSScriptRoot
        # A copy of the script in a throwaway tree, because it reads the files next to its own folder.
        $fx = Join-Path $TestDrive 'fx'
        New-Item -ItemType Directory -Force -Path (Join-Path $fx 'scripts/lib'), (Join-Path $fx 'src-tauri') | Out-Null
        Copy-Item -LiteralPath (Join-Path $scripts 'check-versions.ps1') -Destination (Join-Path $fx 'scripts')
        Copy-Item -LiteralPath (Join-Path $scripts 'lib/channel.ps1') -Destination (Join-Path $fx 'scripts/lib')
        $conf = '{"version":"VER","plugins":{"updater":{"endpoints":["https://github.com/yaniswav/squeezegram/releases/download/channel-v1/latest.json"]}}}'
        foreach ($case in @(@('1.2.3', $true), @('1.2.3-rc.1', $true), @('1.2.3+b', $false), @('1.2.3-rc.1+b', $false), @('1.2', $false), @('v1.2.3', $false), @('1.2.3-', $false))) {
            $v = $case[0]
            Set-Content -LiteralPath (Join-Path $fx 'package.json') -Value ('{"version":"' + $v + '"}')
            Set-Content -LiteralPath (Join-Path $fx 'src-tauri/Cargo.toml') -Value "[package]`nversion = `"$v`""
            Set-Content -LiteralPath (Join-Path $fx 'src-tauri/tauri.conf.json') -Value $conf.Replace('VER', $v)
            $caught = $null
            $result = $null
            try { $result = & (Join-Path $fx 'scripts/check-versions.ps1') | Select-Object -Last 1 } catch { $caught = $_ }
            if ($case[1]) {
                if ($null -ne $caught -or $result -cne $v) { throw "Version '$v' should be accepted, got '$caught'" }
            }
            elseif ($null -eq $caught -or $caught.Exception.Message -notmatch 'must be X\.Y\.Z') { throw "Version '$v' should be refused, got '$caught'" }
        }
    }
    It 'build-inno-installer checks the version and the file name before any input' {
        $inno = Join-Path (Split-Path -Parent $PSScriptRoot) 'build-inno-installer.ps1'
        $absent = Join-Path $TestDrive 'absent'
        function Invoke-Inno([string] $Version, [string] $Filename) {
            & $inno -AppSource $absent -TdjsonSource $absent -NoticesSource $absent -LicenseSource $absent -BootstrapperSource $absent -Version $Version -OutputDirectory $absent -OutputFilename $Filename
        }
        # A value that passes binding reaches the input check; a refused one stops at binding.
        foreach ($v in '1.2.3', '1.2.3-rc.1') {
            $caught = $null
            try { Invoke-Inno $v "squeezegram-$v-setup-x64" } catch { $caught = $_ }
            if ($null -eq $caught -or $caught.Exception.Message -notmatch 'Required input missing') { throw "Version '$v' should pass the version check, got '$caught'" }
        }
        foreach ($v in '1.2.3+b', '1.2.3-rc.1+b', '1.2', 'v1.2.3', '1.2.3-') {
            $caught = $null
            try { Invoke-Inno $v 'squeezegram-setup-x64' } catch { $caught = $_ }
            if ($null -eq $caught -or $caught.Exception.ParameterName -ne 'Version') { throw "Version '$v' should be refused, got '$caught'" }
        }
        $caught = $null
        try { Invoke-Inno '1.2.3' 'squeezegram-1.2.3+b-setup-x64' } catch { $caught = $_ }
        if ($null -eq $caught -or $caught.Exception.ParameterName -ne 'OutputFilename') { throw "A '+' in the installer name should be refused, got '$caught'" }
    }
}
