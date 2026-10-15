<#
.SYNOPSIS
    Builds the Windows installer and the portable zip locally, into dist-release\.

.DESCRIPTION
    Steps, stopping at the first error:
      1. scripts\check-versions.ps1   package.json, Cargo.toml and tauri.conf.json agree
      2. scripts\stage-tdjson.ps1     pinned tdjson.dll (SHA-256 checked) into the staging folder
      3. scripts\generate-notices.ps1 THIRD-PARTY-NOTICES.txt (needs the pinned cargo-about, see
                                      scripts\install-cargo-about.ps1, and `pnpm install`)
      4. pnpm tauri build             Inno installer (per-user, no admin rights)
      5. scripts\make-portable.ps1    dist-release\squeezegram-<version>-portable-x64.zip
      6. scripts\make-checksums.ps1   dist-release\SHA256SUMS.txt
    Then it prints the size and SHA-256 of each artefact.

    DO NOT PUBLISH THE OUTPUT OF THIS SCRIPT. A local build compiles the Telegram api_id and
    api_hash read from .env INTO the executable (src-tauri\build.rs), so anyone who gets the
    installer or the zip can extract them. Only builds made by CI with the intended secrets
    may be published. Checksums written here describe local test artefacts only.

    tdjson.dll and THIRD-PARTY-NOTICES.txt are shipped as Tauri resources mapped to the install
    root, so they land next to squeezegram.exe (library.rs looks for the DLL there). The
    resources are declared in a config overlay generated here rather than in tauri.conf.json:
    tauri-build fails when a declared resource is missing, which would break `cargo check`,
    `tauri dev` and the CI dry-run (none of them stage the DLL or generate the notices).

    Update artefacts: when the environment variable TAURI_SIGNING_PRIVATE_KEY is set (the
    private updater key, as a path or as its content; TAURI_SIGNING_PRIVATE_KEY_PASSWORD
    carries its password if it has one), Tauri signs the Inno installer with the app version.
    The optional -InstallerEngine Nsis keeps Tauri's bundle.createUpdaterArtifacts signing.
    Without the variable the build is the same as before and has no update signature. The key
    is consumed by Tauri from the environment only: this script only tests its presence,
    and it must never be placed in a file of the repository. latest.json (the update manifest)
    is not written here.

    A release build is long and heavy on the CPU. To keep the PC usable:
      cmd /c "start /low /wait /b pwsh -NoProfile -File scripts\build-release.ps1"

.EXAMPLE
    pwsh -File scripts\build-release.ps1
#>
[CmdletBinding()]
param([ValidateSet('Inno', 'Nsis')][string] $InstallerEngine = 'Inno')

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist-release'

function Assert-Native([string] $What) {
    if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

Write-Host '== 1/6 versions'
$version = & (Join-Path $PSScriptRoot 'check-versions.ps1') | Select-Object -Last 1

Write-Host '== 2/6 tdjson.dll'
$staged = & (Join-Path $PSScriptRoot 'stage-tdjson.ps1') | Select-Object -Last 1

Write-Host '== 3/6 third-party notices'
# Fails (and so stops the release) when a crate or package has no licence text.
$notices = & (Join-Path $PSScriptRoot 'generate-notices.ps1') -Version $version | Select-Object -Last 1
if (-not (Test-Path -LiteralPath $notices -PathType Leaf)) { throw "Notices not generated: $notices" }

Write-Host '== 4/6 pnpm tauri build'
# Paths in "resources" are relative to src-tauri; the value is the destination in the bundle.
$overlay = Join-Path (Split-Path -Parent $staged) 'tauri.release.conf.json'
$srcTauri = Join-Path $root 'src-tauri'
$stagedRelative = ($staged.Substring($srcTauri.Length + 1)) -replace '\\', '/'
$noticesRelative = ($notices.Substring($srcTauri.Length + 1)) -replace '\\', '/'
$bundle = @{ resources = @{ $stagedRelative = 'tdjson.dll'; $noticesRelative = 'THIRD-PARTY-NOTICES.txt' } }
# Only the presence of the variable is tested, never its value.
$signUpdates = -not [string]::IsNullOrEmpty($env:TAURI_SIGNING_PRIVATE_KEY)
if ($signUpdates -and $InstallerEngine -eq 'Nsis') { $bundle.createUpdaterArtifacts = $true }
$config = @{ bundle = $bundle } | ConvertTo-Json -Depth 5
if ($signUpdates) { Write-Host 'TAURI_SIGNING_PRIVATE_KEY is set: the installer will be signed for updates.' }
else { Write-Host 'TAURI_SIGNING_PRIVATE_KEY is not set: no update signature (local test build).' }
[IO.File]::WriteAllText($overlay, $config, (New-Object System.Text.UTF8Encoding($false)))

$targetDirectory = if ($env:CARGO_TARGET_DIR) { [IO.Path]::GetFullPath($env:CARGO_TARGET_DIR, $root) } else { Join-Path $srcTauri 'target' }
$nsisDir = Join-Path $targetDirectory 'release\bundle\nsis'
New-Item -ItemType Directory -Force -Path $dist | Out-Null
$installerOut = Join-Path $dist "squeezegram-$version-setup-x64.exe"
# An unsigned rebuild must not retain a signature from an older signed build.
if (Test-Path -LiteralPath "$installerOut.sig") { Remove-Item -LiteralPath "$installerOut.sig" -Force }
if ($InstallerEngine -eq 'Nsis') {
    $installer = Join-Path $nsisDir "squeezegram_${version}_x64-setup.exe"
    foreach ($old in @($installer, "$installer.sig")) {
        if (Test-Path -LiteralPath $old) { Remove-Item -LiteralPath $old -Force }
    }
}
Push-Location $root
try {
    if ($InstallerEngine -eq 'Inno') { & pnpm tauri build --no-bundle --config $overlay }
    else { & pnpm tauri build --config $overlay }
    Assert-Native 'pnpm tauri build'
}
finally { Pop-Location }

if ($InstallerEngine -eq 'Inno') {
    $bootstrapper = Join-Path (Split-Path -Parent $staged) 'MicrosoftEdgeWebview2Setup.exe'
    if (-not (Test-Path -LiteralPath $bootstrapper -PathType Leaf)) {
        Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $bootstrapper
    }
    # build-inno-installer verifies Microsoft's Authenticode signature before embedding it.
    $installerOut = & (Join-Path $PSScriptRoot 'build-inno-installer.ps1') `
        -AppSource (Join-Path $targetDirectory 'release\squeezegram.exe') `
        -TdjsonSource $staged -NoticesSource $notices -LicenseSource (Join-Path $root 'LICENSE') `
        -BootstrapperSource $bootstrapper -Version $version -OutputDirectory $dist `
        -OutputFilename "squeezegram-$version-setup-x64" | Select-Object -Last 1
    if ($signUpdates) {
        Push-Location $root
        try {
            & pnpm tauri signer sign --app-version $version $installerOut
            Assert-Native 'pnpm tauri signer sign'
        }
        finally { Pop-Location }
        if (-not (Test-Path -LiteralPath "$installerOut.sig" -PathType Leaf)) { throw 'Inno update signature missing.' }
    }
}
else {
    if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
        throw "Installer not found: $installer (an antivirus may have removed it)"
    }
    Copy-Item -LiteralPath $installer -Destination $installerOut -Force
    if ($signUpdates) {
        if (-not (Test-Path -LiteralPath "$installer.sig" -PathType Leaf)) { throw "Update signature not found: $installer.sig" }
        Copy-Item -LiteralPath "$installer.sig" -Destination "$installerOut.sig" -Force
    }
}

Write-Host '== 5/6 portable zip'
$zip = & (Join-Path $PSScriptRoot 'make-portable.ps1') -Version $version -ExePath (Join-Path $targetDirectory 'release\squeezegram.exe') -DllPath $staged -NoticesPath $notices | Select-Object -Last 1

Write-Host '== 6/6 checksums'
& (Join-Path $PSScriptRoot 'make-checksums.ps1') -Directory $dist -FileNames @([IO.Path]::GetFileName($installerOut), [IO.Path]::GetFileName($zip))

Write-Host ''
Write-Host "== Artefacts (version $version, LOCAL builds: do not publish)"
$artefacts = @($installerOut, $zip, (Join-Path $dist 'SHA256SUMS.txt'))
if ($signUpdates) { $artefacts += "$installerOut.sig" }
foreach ($path in $artefacts) {
    $file = Get-Item -LiteralPath $path
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host ("{0,-44} {1,12} bytes  {2}" -f $file.Name, $file.Length, $hash)
}
