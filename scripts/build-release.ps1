<#
.SYNOPSIS
    Builds the Windows installer and the portable zip locally, into dist-release\.

.DESCRIPTION
    Steps, stopping at the first error:
      1. scripts\check-versions.ps1   package.json, Cargo.toml and tauri.conf.json agree
      2. scripts\stage-tdjson.ps1     pinned tdjson.dll (SHA-256 checked) into the staging folder
      3. pnpm tauri build             NSIS installer (per-user, no admin rights)
      4. scripts\make-portable.ps1    dist-release\squeezegram-<version>-portable-x64.zip
      5. scripts\make-checksums.ps1   dist-release\SHA256SUMS.txt
    Then it prints the size and SHA-256 of each artefact.

    DO NOT PUBLISH THE OUTPUT OF THIS SCRIPT. A local build compiles the Telegram api_id and
    api_hash read from .env INTO the executable (src-tauri\build.rs), so anyone who gets the
    installer or the zip can extract them. Only builds made by CI with the intended secrets
    may be published. Checksums written here describe local test artefacts only.

    tdjson.dll is shipped as a Tauri resource mapped to the install root, so it lands next to
    squeezegram.exe, where library.rs looks for it. The resource is declared in a config
    overlay generated here rather than in tauri.conf.json: tauri-build fails when a declared
    resource is missing, which would break `cargo check`, `tauri dev` and the CI dry-run
    (none of them stage the DLL).

    Update artefacts: when the environment variable TAURI_SIGNING_PRIVATE_KEY is set (the
    private updater key, as a path or as its content; TAURI_SIGNING_PRIVATE_KEY_PASSWORD
    carries its password if it has one), the same overlay turns on bundle.createUpdaterArtifacts
    so Tauri signs the installer and writes its .sig, copied next to the installer in dist-release.
    Without the variable the build is the same as before and has no update signature. The key
    is read by Tauri from the environment only: this script never reads, prints or stores it,
    and it must never be placed in a file of the repository. latest.json (the update manifest)
    is not written here.

    A release build is long and heavy on the CPU. To keep the PC usable:
      cmd /c "start /low /wait /b pwsh -NoProfile -File scripts\build-release.ps1"

.EXAMPLE
    pwsh -File scripts\build-release.ps1
#>
[CmdletBinding()]
param()

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$dist = Join-Path $root 'dist-release'

function Assert-Native([string] $What) {
    if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

Write-Host '== 1/5 versions'
$version = & (Join-Path $PSScriptRoot 'check-versions.ps1') | Select-Object -Last 1

Write-Host '== 2/5 tdjson.dll'
$staged = & (Join-Path $PSScriptRoot 'stage-tdjson.ps1') | Select-Object -Last 1

Write-Host '== 3/5 pnpm tauri build'
# Paths in "resources" are relative to src-tauri; the value is the destination in the bundle.
$overlay = Join-Path (Split-Path -Parent $staged) 'tauri.release.conf.json'
$stagedRelative = ($staged.Substring((Join-Path $root 'src-tauri').Length + 1)) -replace '\\', '/'
$bundle = @{ resources = @{ $stagedRelative = 'tdjson.dll' } }
# Only the presence of the variable is tested, never its value.
$signUpdates = -not [string]::IsNullOrEmpty($env:TAURI_SIGNING_PRIVATE_KEY)
if ($signUpdates) { $bundle.createUpdaterArtifacts = $true }
$config = @{ bundle = $bundle } | ConvertTo-Json -Depth 5
if ($signUpdates) { Write-Host 'TAURI_SIGNING_PRIVATE_KEY is set: the installer will be signed for updates.' }
else { Write-Host 'TAURI_SIGNING_PRIVATE_KEY is not set: no update signature (local test build).' }
[IO.File]::WriteAllText($overlay, $config, (New-Object System.Text.UTF8Encoding($false)))

$nsisDir = Join-Path $root 'src-tauri\target\release\bundle\nsis'
if (Test-Path -LiteralPath $nsisDir) { Remove-Item -LiteralPath $nsisDir -Recurse -Force }
Push-Location $root
try {
    & pnpm tauri build --config $overlay
    Assert-Native 'pnpm tauri build'
}
finally { Pop-Location }

$installer = Join-Path $nsisDir "squeezegram_${version}_x64-setup.exe"
if (-not (Test-Path -LiteralPath $installer -PathType Leaf)) {
    throw "Installer not found: $installer (an antivirus may have removed it)"
}
New-Item -ItemType Directory -Force -Path $dist | Out-Null
Get-ChildItem -LiteralPath $dist -File -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -like 'squeezegram-*' -or $_.Name -eq 'SHA256SUMS.txt' } |
    Remove-Item -Force
$installerOut = Join-Path $dist "squeezegram-$version-setup-x64.exe"
Copy-Item -LiteralPath $installer -Destination $installerOut -Force
if ($signUpdates) {
    # The signature covers the file content, so the renamed copy keeps it valid.
    if (-not (Test-Path -LiteralPath "$installer.sig" -PathType Leaf)) { throw "Update signature not found: $installer.sig" }
    Copy-Item -LiteralPath "$installer.sig" -Destination "$installerOut.sig" -Force
}

Write-Host '== 4/5 portable zip'
$zip = & (Join-Path $PSScriptRoot 'make-portable.ps1') -Version $version -DllPath $staged | Select-Object -Last 1

Write-Host '== 5/5 checksums'
& (Join-Path $PSScriptRoot 'make-checksums.ps1') -Directory $dist

Write-Host ''
Write-Host "== Artefacts (version $version, LOCAL builds: do not publish)"
$artefacts = @($installerOut, $zip, (Join-Path $dist 'SHA256SUMS.txt'))
if ($signUpdates) { $artefacts += "$installerOut.sig" }
foreach ($path in $artefacts) {
    $file = Get-Item -LiteralPath $path
    $hash = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash.ToLowerInvariant()
    Write-Host ("{0,-44} {1,12} bytes  {2}" -f $file.Name, $file.Length, $hash)
}
