<#
.SYNOPSIS
    Installs the pinned cargo-about, the tool that lists the licences of the Rust crates.

.DESCRIPTION
    scripts\generate-notices.ps1 needs exactly this version of cargo-about, so that the notice
    file of a release does not change because a newer tool reads licences differently. The pin
    lives here and nowhere else. The tool is compiled from crates.io with the lock file of
    cargo-about itself (`--locked`): cargo checks the checksum of every crate it downloads, so
    no unreviewed binary is run. CI and developers use this same script.

    Nothing is reinstalled when the right version is already there. Updating the pin: change
    $Version, run this script and then scripts\generate-notices.ps1, and compare the result.

.PARAMETER CheckOnly
    Do not install: fail with a message when the pinned version is not installed. Used by
    generate-notices.ps1.

.EXAMPLE
    pwsh -File scripts\install-cargo-about.ps1
#>
[CmdletBinding()]
param(
    [switch] $CheckOnly
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Version = '0.9.2'

function Get-InstalledVersion {
    # `cargo about` is a cargo subcommand: absent, cargo exits with an error.
    $out = & cargo about --version 2>$null
    if ($LASTEXITCODE -ne 0 -or -not $out) { return $null }
    if (($out | Select-Object -First 1) -match '^cargo-about (\S+)') { return $Matches[1] }
    return $null
}

$installed = Get-InstalledVersion
if ($installed -eq $Version) {
    Write-Host "cargo-about $Version is installed."
    return
}
$have = if ($installed) { $installed } else { 'none' }
if ($CheckOnly) {
    throw "cargo-about $Version is required (installed: $have). Run: pwsh -File scripts\install-cargo-about.ps1"
}

Write-Host "Installing cargo-about $Version (installed: $have)..."
# The binary is gated behind the `cli` feature in cargo-about 0.9.
& cargo install --locked cargo-about --version $Version --features cli --force
if ($LASTEXITCODE -ne 0) { throw "cargo install cargo-about failed with exit code $LASTEXITCODE" }
$installed = Get-InstalledVersion
if ($installed -ne $Version) { throw "cargo-about $Version was installed but reports version '$installed'" }
Write-Host "cargo-about $Version installed."
