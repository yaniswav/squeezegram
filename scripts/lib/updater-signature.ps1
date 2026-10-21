# Structural checks only: these do not authenticate the installer or the trusted comment.
# Keep the signing job, manifest generation and channel publication on the same format check.
Set-StrictMode -Version Latest

function ConvertFrom-UpdaterBase64([string] $Value) {
    if ([string]::IsNullOrEmpty($Value) -or $Value -cnotmatch '\A[A-Za-z0-9+/]+={0,2}\z') {
        throw 'Invalid updater signature base64'
    }
    $bytes = [Convert]::FromBase64String($Value)
    if ([Convert]::ToBase64String($bytes) -cne $Value) { throw 'Non-canonical updater signature base64' }
    return ,$bytes
}

function Assert-UpdaterSignature {
    param(
        [Parameter(Mandatory)][string] $Signature,
        [Parameter(Mandatory)][string] $Version,
        [Parameter(Mandatory)][string] $AssetName
    )

    # Tauri wraps a four-line minisign file in base64. Decode UTF-8 strictly so malformed
    # bytes cannot turn into replacement characters before checking the trusted comment.
    $utf8 = New-Object System.Text.UTF8Encoding($false, $true)
    $text = $utf8.GetString((ConvertFrom-UpdaterBase64 $Signature))
    $lines = [regex]::Split($text, '\r?\n')
    if ($lines.Count -eq 5 -and $lines[4] -ceq '') { $lines = $lines[0..3] }
    if ($lines.Count -ne 4 -or $lines[0] -cnotmatch '\Auntrusted comment: [^\r\n]*\z') {
        throw 'Expected a four-line minisign signature'
    }
    # minisign: two algorithm bytes, eight key-id bytes, 64 signature bytes; the final
    # line is the 64-byte global signature that would authenticate the trusted comment.
    $packet = ConvertFrom-UpdaterBase64 $lines[1]
    if ($packet.Length -ne 74 -or $packet[0] -ne 69 -or ($packet[1] -ne 68 -and $packet[1] -ne 100)) {
        throw 'Invalid minisign signature packet'
    }
    $global = ConvertFrom-UpdaterBase64 $lines[3]
    if ($global.Length -ne 64) { throw 'Invalid minisign global signature' }
    $expected = '\Atrusted comment: timestamp:[0-9]+\tfile:' + [regex]::Escape($AssetName) + '\tversion:' + [regex]::Escape($Version) + '\z'
    if ($lines[2] -cnotmatch $expected) {
        throw "The signature's trusted comment does not carry file $AssetName and version $Version"
    }
}

# Cryptographic check: the pinned minisign binary verifies Ed25519 over the installer and the
# global signature over the trusted comment, against the app's updater public key (the base64
# of a minisign .pub file, as in tauri.conf.json). Any failure throws.
function Assert-UpdaterSignatureValid {
    param(
        [Parameter(Mandatory)][string] $Minisign,
        [Parameter(Mandatory)][string] $Installer,
        [Parameter(Mandatory)][string] $Signature,
        [Parameter(Mandatory)][string] $PublicKey,
        [Parameter(Mandatory)][string] $Work
    )

    if (-not (Test-Path -LiteralPath $Minisign -PathType Leaf)) { throw "minisign binary not found: $Minisign" }
    if (-not (Test-Path -LiteralPath $Installer -PathType Leaf)) { throw "Installer not found: $Installer" }
    New-Item -ItemType Directory -Force -Path $Work | Out-Null
    $sigFile = Join-Path $Work 'installer.minisig'
    $pubFile = Join-Path $Work 'updater.pub'
    [IO.File]::WriteAllBytes($sigFile, (ConvertFrom-UpdaterBase64 $Signature))
    [IO.File]::WriteAllBytes($pubFile, (ConvertFrom-UpdaterBase64 $PublicKey))
    $output = & $Minisign -V -m $Installer -x $sigFile -p $pubFile 2>&1
    if ($LASTEXITCODE -ne 0) { throw "minisign rejected the installer signature (exit code ${LASTEXITCODE}): $($output -join ' ')" }
}
