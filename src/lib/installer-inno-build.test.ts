import assert from "node:assert/strict"
import { spawnSync } from "node:child_process"
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs"
import { tmpdir } from "node:os"
import { join } from "node:path"
import { test } from "node:test"
import { fileURLToPath } from "node:url"

const root = new URL("../../", import.meta.url)

const script = (name: string) =>
  readFileSync(new URL(`scripts/${name}.ps1`, root), "utf8")

const release = script("build-release")

const compiler = script("build-inno-installer")

const toolchain = script("install-inno")

test("installer compatibility assumptions are rechecked when Tauri tooling changes", () => {
  const cargo = readFileSync(new URL("src-tauri/Cargo.lock", root), "utf8")

  const pnpm = readFileSync(new URL("pnpm-lock.yaml", root), "utf8")

  // The EXE argument adapter and version-bound signing were reviewed against these versions.
  assert.equal(
    cargo.match(/name = "tauri-plugin-updater"\r?\nversion = "([^"]+)"/)?.[1],
    "2.13.2",
  )
  assert.equal(
    pnpm.match(
      /'@tauri-apps\/cli':\s+specifier:[^\n]*\s+version: ([^\r\n]+)/,
    )?.[1],
    "2.12.1",
  )
})

test("release defaults to Inno with an explicit NSIS rollback and version-bound signing", () => {
  assert.match(release, /ValidateSet\('Inno', 'Nsis'\)/)
  assert.match(release, /\$InstallerEngine = 'Inno'/)
  assert.match(release, /pnpm tauri build --no-bundle --config \$overlay/)
  assert.match(
    release,
    /pnpm tauri signer sign --app-version \$version \$installerOut/,
  )
  assert.match(release, /\$signUpdates -and \$InstallerEngine -eq 'Nsis'/)
  assert.match(
    release,
    /make-portable\.ps1.*-ExePath \(Join-Path \$targetDirectory/,
  )
  assert.doesNotMatch(
    release,
    /-like 'squeezegram-\*'|Remove-Item[^\n]*-Recurse/,
  )
  assert.doesNotMatch(release, /inno\.flag/)
})

test("Inno compiler validates payloads before compilation and never runs the installer", () => {
  for (const name of [
    "AppSource",
    "TdjsonSource",
    "NoticesSource",
    "LicenseSource",
    "BootstrapperSource",
  ]) {
    assert.match(
      compiler,
      new RegExp(`\\[Parameter\\(Mandatory\\)\\]\\[string\\] \\$${name}`),
    )
  }

  assert.match(compiler, /Required input missing/)
  assert.match(compiler, /ReadByte\(\) -ne 0x4D.*ReadByte\(\) -ne 0x5A/)
  assert.match(
    compiler,
    /048af1d1c97ef9b860dd5de132c4d2d901d444691ec86fc15012e5896dbde6b8/,
  )
  assert.match(
    compiler,
    /Get-AuthenticodeSignature -LiteralPath \$BootstrapperSource/,
  )
  assert.match(compiler, /CN=Microsoft Corporation/)
  assert.match(compiler, /& \$IsccPath @defines/)
  assert.doesNotMatch(compiler, /Start-Process|Invoke-Expression|& \$output/)
})

test("Inno toolchain and contributed translation have fixed hashes and signer checks", () => {
  assert.match(toolchain, /innosetup-6\.7\.3\.exe/)
  assert.match(
    toolchain,
    /9c73c3bae7ed48d44112a0f48e66742c00090bdb5bef71d9d3c056c66e97b732/,
  )
  assert.match(
    toolchain,
    /0a8757031b33777e4c9cbffee40f11a5062b36d25cbe144c1db73b6102b80ad7/,
  )
  assert.match(
    toolchain,
    /eda27ca7d4b0bd666d7ed0cc91a856842a0101d7cff12417998cfd1e12c92fba/,
  )
  assert.match(toolchain, /CN=Pyrsys/)
  assert.match(toolchain, /-WindowStyle Hidden -Wait -PassThru/)
  assert.match(toolchain, /if \(\$CheckOnly\)/)
})

test(
  "Inno build scripts parse in PowerShell without executing their contents",
  { skip: process.platform !== "win32" },
  () => {
    for (const name of [
      "build-release",
      "build-inno-installer",
      "install-inno",
      "make-checksums",
      "check-versions",
    ]) {
      const path = fileURLToPath(
        new URL(`scripts/${name}.ps1`, root),
      ).replaceAll("'", "''")

      const result = spawnSync(
        "pwsh",
        [
          "-NoProfile",
          "-Command",
          `$tokens=$null; $errors=$null; [System.Management.Automation.Language.Parser]::ParseFile('${path}',[ref]$tokens,[ref]$errors) | Out-Null; if($errors.Count){$errors | Out-String | Write-Error; exit 1}`,
        ],
        { encoding: "utf8" },
      )

      assert.equal(result.status, 0, result.stderr)
    }
  },
)

test(
  "Inno accepts release, prerelease and metadata versions but fails before compiling absent inputs",
  { skip: process.platform !== "win32" },
  () => {
    const path = fileURLToPath(
      new URL("scripts/build-inno-installer.ps1", root),
    )

    for (const version of [
      "0.1.0",
      "0.1.0-beta.1",
      "0.1.0+build.2",
      "0.1.0-beta.1+local.2",
    ]) {
      const missing = fileURLToPath(
        new URL("src-tauri/target/absent-inno-test-payload", root),
      )

      const result = spawnSync(
        "pwsh",
        [
          "-NoProfile",
          "-File",
          path,
          "-AppSource",
          missing,
          "-TdjsonSource",
          missing,
          "-NoticesSource",
          missing,
          "-LicenseSource",
          missing,
          "-BootstrapperSource",
          missing,
          "-Version",
          version,
          "-OutputDirectory",
          missing,
          "-OutputFilename",
          `squeezegram-${version}-setup-x64`,
        ],
        { encoding: "utf8" },
      )

      assert.notEqual(result.status, 0)
      assert.match(result.stderr, /Required input missing: AppSource/)
    }

    assert.match(
      compiler,
      /\$numericVersion = \$Version -replace '\[-\+\]\.\*\$', ''/,
    )
    assert.match(compiler, /\/DNumericVersion=\$numericVersion/)
  },
)

test("release version guards accept prerelease plus metadata consistently", () => {
  const check = script("check-versions")

  const expression = compiler.match(
    /ValidatePattern\('([^']+)'\).*\$Version/,
  )?.[1]

  assert.ok(expression)
  assert.ok(check.includes(`-notmatch '${expression}'`))

  const pattern = new RegExp(expression)

  for (const version of [
    "0.1.0",
    "0.1.0-beta.1",
    "0.1.0+build.2",
    "0.1.0-beta.1+local.2",
  ]) {
    assert.ok(pattern.test(version))
  }

  for (const version of [
    "0.1",
    "0.1.0-",
    "0.1.0+",
    "0.1.0+local+other",
    "0.1.0/local",
  ]) {
    assert.equal(pattern.test(version), false)
  }
})

test(
  "selected checksums exclude older artifacts and reject missing or non-leaf names",
  { skip: process.platform !== "win32" },
  () => {
    const directory = mkdtempSync(join(tmpdir(), "squeezegram-checksums-"))

    const path = fileURLToPath(
      new URL("scripts/make-checksums.ps1", root),
    ).replaceAll("'", "''")

    const quotedDirectory = directory.replaceAll("'", "''")

    const selected = [
      "squeezegram-0.1.0-setup-x64.exe",
      "squeezegram-0.1.0-portable-x64.zip",
    ]

    const old = "squeezegram-0.0.9-setup-x64.exe"

    try {
      for (const name of [...selected, old]) {
        writeFileSync(join(directory, name), `generated fixture ${name}`)
      }

      const run = (names?: string[]) =>
        spawnSync(
          "pwsh",
          [
            "-NoProfile",
            "-Command",
            `& '${path}' -Directory '${quotedDirectory}'${names ? ` -FileNames @(${names.map((name) => `'${name.replaceAll("'", "''")}'`).join(",")})` : ""}`,
          ],
          { encoding: "utf8" },
        )

      const result = run(selected)

      assert.equal(result.status, 0, result.stderr)

      const manifest = readFileSync(join(directory, "SHA256SUMS.txt"), "utf8")

      assert.equal(manifest.trim().split("\n").length, 2)

      for (const name of selected) {
        assert.ok(manifest.includes(`  ${name}\n`))
      }

      assert.ok(!manifest.includes(old))

      for (const name of [
        "missing.exe",
        "../outside.exe",
        "sub\\file.exe",
        "C:outside.exe",
        "..",
        ".",
      ]) {
        const invalid = run([name])

        assert.notEqual(invalid.status, 0)
        assert.match(
          invalid.stderr,
          /Selected artifact missing|leaf names only/,
        )
        assert.equal(
          readFileSync(join(directory, "SHA256SUMS.txt"), "utf8"),
          manifest,
        )
      }

      assert.equal(run().status, 0)
      assert.ok(
        readFileSync(join(directory, "SHA256SUMS.txt"), "utf8").includes(old),
      )
      assert.match(release, /make-checksums\.ps1.*-FileNames @\(/)
    } finally {
      rmSync(directory, { recursive: true, force: true })
    }
  },
)
