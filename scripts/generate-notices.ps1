<#
.SYNOPSIS
    Generates THIRD-PARTY-NOTICES.txt, the licence notices shipped with squeezegram.

.DESCRIPTION
    Merges three sources into one text file:
      1. notices\THIRD-PARTY-NOTICES.template.txt   hand-written sections (TDLib, OpenSSL, zlib,
         SQLCipher, SQLite, WebView2 loader, FFmpeg) with the licence texts of notices\licenses\
      2. the Rust crates of the Windows build, by cargo-about (src-tauri\about.toml and
         about.hbs). The pinned version is checked, see scripts\install-cargo-about.ps1.
      3. the npm production packages that end up in the user interface, from
         `pnpm licenses list --prod` (notices\npm.json holds the exclusions, the additions
         and the copyright holders of two packages that ship no licence file)

    The result is checked, and the script fails instead of writing an incomplete file:
      - every crate of the Windows dependency graph that is not dev-only (cargo metadata) is in
        the Rust section, and nothing else is (compared with cargo-about's own resolution);
      - no crate ships a NOTICE file that would have to be reproduced (Apache-2.0, 4(d));
      - cargo-about found a licence for every crate (--fail), under the licences of deny.toml;
      - every npm package has a licence text or a configured copyright holder (MIT only);
      - no placeholder of the template is left.

    Releases call it from scripts\build-release.ps1, the build workflow calls it to catch
    breakage early. Nothing is downloaded by this script except the crates cargo needs
    (`cargo fetch --locked`), after which cargo-about runs offline (--frozen). Requires
    `pnpm install` to have been run.

.PARAMETER OutFile
    Output path. Default: src-tauri\target\notices\THIRD-PARTY-NOTICES.txt (ignored by Git).

.PARAMETER Version
    Version written in the title. Defaults to the version shared by package.json, Cargo.toml and
    tauri.conf.json.

.EXAMPLE
    pwsh -File scripts\generate-notices.ps1
#>
[CmdletBinding()]
param(
    [string] $OutFile,
    [string] $Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$tauriDir = Join-Path $root 'src-tauri'
$noticesDir = Join-Path $root 'notices'
if (-not $OutFile) { $OutFile = Join-Path $tauriDir 'target\notices\THIRD-PARTY-NOTICES.txt' }
if (-not $Version) { $Version = (& (Join-Path $PSScriptRoot 'check-versions.ps1') | Select-Object -Last 1) }
$target = 'x86_64-pc-windows-msvc'
$rule = '-' * 90
$utf8 = [System.Text.UTF8Encoding]::new($false)

function Assert-Native([string] $What) {
    if ($LASTEXITCODE -ne 0) { throw "$What failed with exit code $LASTEXITCODE" }
}

function Read-Text([string] $Path) {
    # UTF-8, no BOM, LF line endings, no trailing blank lines.
    ([IO.File]::ReadAllText($Path, $utf8)).TrimStart([char]0xFEFF).Replace("`r`n", "`n").Replace("`r", "`n").TrimEnd()
}

function Get-TextHash([string] $Text) {
    $bytes = $utf8.GetBytes($Text)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { [BitConverter]::ToString($sha.ComputeHash($bytes)) } finally { $sha.Dispose() }
}

# ---------------------------------------------------------------------------------------------
# Rust crates
# ---------------------------------------------------------------------------------------------
& (Join-Path $PSScriptRoot 'install-cargo-about.ps1') -CheckOnly | Out-Null

Push-Location $tauriDir
try {
    # Download the crates first: cargo-about then runs offline and never reaches the network
    # itself (it would otherwise fetch missing licence files from the crates' repositories).
    # All platforms: cargo-about's own `cargo metadata` resolves the whole graph before it
    # filters on about.toml's targets, and fails offline on a crate it cannot find.
    & cargo fetch --locked
    Assert-Native 'cargo fetch'
    $metaJson = & cargo metadata --format-version 1 --locked --filter-platform $target
    Assert-Native 'cargo metadata'
    # cargo's own view of what the Windows build compiles: normal and build dependencies for the
    # target, dev-dependencies left out. (cargo metadata alone also lists edges for other
    # platforms.) One line per crate: name, version, licence expression, repository.
    $treeLines = & cargo tree --locked --target $target --edges normal,build --prefix none --format '{p}|{l}|{r}'
    Assert-Native 'cargo tree'
    $aboutOut = Join-Path ([IO.Path]::GetTempPath()) ("sqz-about-" + [guid]::NewGuid().ToString('N') + '.txt')
    try {
        & cargo about generate --frozen --fail --config about.toml --output-file $aboutOut about.hbs
        Assert-Native 'cargo about generate'
        $aboutText = Read-Text $aboutOut
    } finally {
        Remove-Item -LiteralPath $aboutOut -Force -ErrorAction SilentlyContinue
    }
} finally {
    Pop-Location
}

$meta = $metaJson | ConvertFrom-Json
$rootName = ($meta.packages | Where-Object { $_.id -eq $meta.resolve.root }).name
$dirs = @{}
foreach ($p in $meta.packages) { $dirs["$($p.name) $($p.version)"] = Split-Path -Parent $p.manifest_path }
$crateMap = @{}
foreach ($line in $treeLines) {
    if ($line.StartsWith("$rootName v")) { continue }
    if ($line -notmatch '^(\S+) v(\S+?)( \(proc-macro\))?\|(.*)\|(.*?)( \(\*\))?$') { throw "Unexpected cargo tree line: $line" }
    $crateMap["$($Matches[1]) $($Matches[2])"] = [pscustomobject]@{
        name = $Matches[1]; version = $Matches[2]; license = $Matches[4]; repository = $Matches[5]
    }
}
$crates = @($crateMap.Values | Sort-Object { $_.name }, { $_.version })

# The Rust section must list exactly these crates.
$listed = [System.Collections.Generic.HashSet[string]]::new()
$inUsedBy = $false
foreach ($line in $aboutText.Split("`n")) {
    if ($line -eq 'Used by:') { $inUsedBy = $true; continue }
    if ($inUsedBy) {
        if ($line -match '^  - (\S+) (\S+)') { [void]$listed.Add("$($Matches[1]) $($Matches[2])") } else { $inUsedBy = $false }
    }
}
$expected = [System.Collections.Generic.HashSet[string]]::new()
foreach ($c in $crates) { [void]$expected.Add("$($c.name) $($c.version)") }
$missing = @($expected | Where-Object { -not $listed.Contains($_) } | Sort-Object)
$extra = @($listed | Where-Object { -not $expected.Contains($_) } | Sort-Object)
if ($missing) { throw "Crates of the Windows build missing from the notices: $($missing -join ', ')" }
if ($extra) { throw "Crates in the notices that are not in the Windows build: $($extra -join ', ')" }

# Apache-2.0 4(d): a NOTICE file shipped by a crate has to be reproduced. None does today; if
# one appears, extend the Rust section by hand rather than ship without it.
foreach ($c in $crates) {
    $dir = $dirs["$($c.name) $($c.version)"]
    $notice = Get-ChildItem -LiteralPath $dir -File -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^NOTICE' }
    if ($notice) { throw "$($c.name) $($c.version) ships $($notice.Name): reproduce it in the notices (Apache-2.0, section 4(d))" }
}

$rustSection = "Number of crates: $($crates.Count)`n`n$aboutText"

$mplLines = @($crates | Where-Object { $_.license -match 'MPL-2\.0' } | ForEach-Object {
    $src = if ($_.repository) { $_.repository } else { "https://crates.io/crates/$($_.name)/$($_.version)" }
    "  - $($_.name) $($_.version) ($($_.license)), $src"
})
if (-not $mplLines) { $mplLines = @('  None.') }
$mplSection = $mplLines -join "`n"

# ---------------------------------------------------------------------------------------------
# npm packages
# ---------------------------------------------------------------------------------------------
$npmConfig = Get-Content -LiteralPath (Join-Path $noticesDir 'npm.json') -Raw | ConvertFrom-Json

function Get-NpmPackages([string[]] $PnpmArgs) {
    Push-Location $root
    try {
        $json = & pnpm licenses list @PnpmArgs --json
        Assert-Native 'pnpm licenses list (run pnpm install first)'
    } finally {
        Pop-Location
    }
    $byLicense = $json | ConvertFrom-Json
    foreach ($prop in $byLicense.PSObject.Properties) {
        foreach ($pkg in $prop.Value) {
            for ($i = 0; $i -lt @($pkg.versions).Count; $i++) {
                [pscustomobject]@{
                    Name     = $pkg.name
                    Version  = @($pkg.versions)[$i]
                    License  = $prop.Name
                    Homepage = $pkg.homepage
                    Dir      = @($pkg.paths)[$i]
                }
            }
        }
    }
}

$excluded = @($npmConfig.exclude.PSObject.Properties.Name)
$npm = @(Get-NpmPackages @('--prod') | Where-Object { $excluded -notcontains $_.Name })
foreach ($name in $npmConfig.include.PSObject.Properties.Name) {
    if ($npm | Where-Object { $_.Name -eq $name }) { continue }
    $extraPkg = @(Get-NpmPackages @('--dev') | Where-Object { $_.Name -eq $name })
    if (-not $extraPkg) { throw "npm.json asks to include '$name', but pnpm does not list it" }
    $npm += $extraPkg
}
$npm = @($npm | Sort-Object { $_.Name }, { $_.Version })
if (-not $npm) { throw 'No npm package found (run pnpm install first)' }

$holders = $npmConfig.copyrightHolders
$mitText = @'
MIT License

Copyright (c) {HOLDER}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
'@

$groups = [ordered]@{}
foreach ($p in $npm) {
    # .spdx files are SPDX metadata, not licence texts.
    $files = @(Get-ChildItem -LiteralPath $p.Dir -File | Where-Object {
            $_.Name -match '^(LICEN[CS]E|COPYING|NOTICE)' -and $_.Extension -ne '.spdx'
        } | Sort-Object Name)
    if ($files) {
        if ($files.Count -eq 1) { $text = Read-Text $files[0].FullName }
        else { $text = ($files | ForEach-Object { "[$($_.Name)]`n`n$(Read-Text $_.FullName)" }) -join "`n`n" }
    } elseif ($p.License -match '\bMIT\b' -and $holders.PSObject.Properties.Name -contains $p.Name) {
        # No licence file in the package: the standard MIT text with the holder configured in
        # notices\npm.json, taken from the package's own metadata.
        $text = $mitText.Replace("`r`n", "`n").Replace('{HOLDER}', $holders.($p.Name)).TrimEnd()
        Write-Warning "$($p.Name) $($p.Version) ships no licence file: standard MIT text with the holder from notices\npm.json."
    } else {
        throw "$($p.Name) $($p.Version) ($($p.License)) ships no licence file and has no copyright holder in notices\npm.json"
    }
    $key = Get-TextHash $text
    if (-not $groups.Contains($key)) { $groups[$key] = [pscustomobject]@{ Text = $text; Packages = [System.Collections.Generic.List[object]]::new() } }
    $groups[$key].Packages.Add($p)
}
$sb = [System.Text.StringBuilder]::new()
[void]$sb.Append("Number of packages: $($npm.Count)`n`n")
[void]$sb.Append("The production dependencies of pnpm-lock.yaml and the packages they pull in (the list`n")
[void]$sb.Append("can include a few that tree-shaking leaves out of the bundle), plus Tailwind CSS, whose`n")
[void]$sb.Append("styles are compiled into the stylesheet. Each licence text is given once, followed by`n")
[void]$sb.Append("the packages distributed under exactly that text. Packages offered under a choice of`n")
[void]$sb.Append("licences ('Apache-2.0 OR MIT') are covered by the texts shipped in their package.`n`n")
foreach ($g in ($groups.Values | Sort-Object { $_.Packages[0].Name })) {
    [void]$sb.Append("$rule`n")
    [void]$sb.Append("Used by:`n")
    foreach ($p in $g.Packages) {
        $homepage = if ($p.Homepage) { " $($p.Homepage)" } else { '' }
        [void]$sb.Append("  - $($p.Name) $($p.Version) [$($p.License)]$homepage`n")
    }
    [void]$sb.Append("$rule`n`n$($g.Text)`n`n")
}
$npmSection = $sb.ToString().TrimEnd()

# ---------------------------------------------------------------------------------------------
# Merge
# ---------------------------------------------------------------------------------------------
$out = Read-Text (Join-Path $noticesDir 'THIRD-PARTY-NOTICES.template.txt')
$out = $out.Replace('@@VERSION@@', $Version)
foreach ($m in [regex]::Matches($out, '(?m)^@@LICENSE:([A-Za-z0-9._-]+)@@$') | ForEach-Object { $_ }) {
    $file = Join-Path $noticesDir "licenses\$($m.Groups[1].Value)"
    if (-not (Test-Path -LiteralPath $file -PathType Leaf)) { throw "Licence text not found: $file" }
    $out = $out.Replace($m.Value, (Read-Text $file))
}
$out = $out.Replace('@@RUST_CRATES@@', $rustSection).Replace('@@MPL_CRATES@@', $mplSection).Replace('@@NPM_PACKAGES@@', $npmSection)
if ($out -match '(?m)^@@[A-Z_:]+.*@@$|@@(VERSION|RUST_CRATES|MPL_CRATES|NPM_PACKAGES)@@') {
    throw 'A placeholder of the template was not replaced'
}

$outDir = Split-Path -Parent $OutFile
if ($outDir) { New-Item -ItemType Directory -Force -Path $outDir | Out-Null }
# CRLF: the file is opened with Notepad on Windows.
[IO.File]::WriteAllText($OutFile, ($out.TrimEnd() + "`n").Replace("`n", "`r`n"), $utf8)
Write-Host "Wrote $OutFile ($($crates.Count) Rust crates, $($npm.Count) npm packages, $([math]::Round((Get-Item -LiteralPath $OutFile).Length / 1KB)) KB)"
Write-Output $OutFile
