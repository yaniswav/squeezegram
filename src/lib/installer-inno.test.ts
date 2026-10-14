import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { spawnSync } from "node:child_process"
import { test } from "node:test"
import { fileURLToPath } from "node:url"

const root = new URL("../../", import.meta.url)

const prototype = new URL("src-tauri/windows/inno-prototype/", root)

const source = readFileSync(new URL("preview.iss", prototype), "utf8")

const strings: Record<string, { id: number; text: Record<string, string> }> =
  JSON.parse(readFileSync(new URL("strings.json", prototype), "utf8"))

test("Inno preview contains seven complete custom UI translations without em dashes", () => {
  assert.deepEqual(Object.keys(strings).sort(), [
    "ar",
    "en",
    "es",
    "fr",
    "id",
    "pt-BR",
    "ru",
  ])

  const keys = Object.keys(strings.en.text).sort()

  for (const locale of Object.values(strings)) {
    assert.deepEqual(Object.keys(locale.text).sort(), keys)

    for (const text of Object.values(locale.text)) {
      assert.ok(text.trim().length > 0)
      assert.doesNotMatch(text, /[\r\n\u2014]/u)
    }
  }

  const used = [...source.matchAll(/CustomMessage\('([^']+)'\)/g)].map(
    (match) => match[1],
  )

  assert.deepEqual([...new Set(used)].sort(), keys)
})

test("Inno generated messages match their checked-in source", () => {
  const result = spawnSync(
    process.execPath,
    [fileURLToPath(new URL("generate-strings.mjs", prototype)), "--check"],
    { encoding: "utf8" },
  )

  assert.equal(result.status, 0, result.stderr)
})

test("Inno preview requests user UI language and puts English fallback first", () => {
  assert.match(source, /^LanguageDetectionMethod=uilanguage$/m)
  assert.match(source, /^ShowLanguageDialog=no$/m)
  assert.match(source, /^UsePreviousLanguage=no$/m)

  const languages = [
    ...source.matchAll(/^Name: "([^"]+)"; MessagesFile:/gm),
  ].map((match) => match[1])

  assert.deepEqual(languages, ["en", "fr", "ru", "es", "ar", "ptBR", "id"])
})

test("Inno preview disables installation persistence and requires no administrator", () => {
  for (const directive of [
    "CreateAppDir=no",
    "Uninstallable=no",
    "CreateUninstallRegKey=no",
    "PrivilegesRequired=lowest",
    "CloseApplications=no",
    "RestartApplications=no",
    "UsePreviousAppDir=no",
    "UsePreviousTasks=no",
    "UsePreviousSetupType=no",
  ]) {
    assert.ok(source.split(/\r?\n/).includes(directive), directive)
  }

  assert.doesNotMatch(
    source,
    /^\[(Files|Dirs|Icons|Registry|Run|UninstallRun|InstallDelete|UninstallDelete)\]/im,
  )
  assert.doesNotMatch(
    source,
    /\b(Exec|ShellExec|SaveStringToFile|SaveStringsToFile|RegWrite\w+|DeleteFile|DelTree|CreateDir|ForceDirectories|DownloadTemporaryFile|ExtractTemporaryFile)\s*\(/i,
  )
  assert.doesNotMatch(source, /\bexternal\b/i)
})

test("Inno preview refuses silent mode and has two independent installation guards", () => {
  assert.match(
    source,
    /function InitializeSetup: Boolean;\s*begin\s*Result := not WizardSilent;/,
  )
  assert.match(
    source,
    /function NextButtonClick\(CurPageID: Integer\): Boolean;\s*begin\s*Result := CurPageID = wpWelcome;/,
  )
  assert.match(
    source,
    /if CurPageID = ControlsPage.ID then\s*WizardForm.Close;/,
  )
  assert.match(
    source,
    /function PrepareToInstall\(var NeedsRestart: Boolean\): String;\s*begin\s*Result := CustomMessage\('notice'\);/,
  )
})

test("Inno preview leaves drawing and navigation to the installer engine", () => {
  assert.match(source, /^WizardStyle=modern dark$/m)
  assert.match(source, /^SetupIconFile=\.\.\\\.\.\\icons\\icon.ico$/m)
  assert.match(source, /CreateCustomPage\(wpWelcome/)
  assert.match(
    source,
    /WizardForm.NextButton.Enabled := Trim\(PathEdit.Text\) <> ''/,
  )
  assert.match(source, /DisabledRadio.Enabled := False/)
  assert.match(source, /DemoProgress.Position < 100/)
  assert.match(source, /DemoProgress.Position := DemoProgress.Position \+ 25/)
  assert.doesNotMatch(
    source,
    /DrawText|CreateFont|SetWindowTheme|SetWindowLong|SendMessage|OnPaint|Canvas\./,
  )
})

test("Inno study stays outside production packaging and updater configuration", () => {
  const config = JSON.parse(
    readFileSync(new URL("src-tauri/tauri.conf.json", root), "utf8"),
  )

  assert.equal(config.bundle.windows.nsis.template, undefined)
  assert.equal(config.bundle.windows.nsis.installerHooks, "windows/hooks.nsh")

  for (const path of [
    "src-tauri/windows/hooks.nsh",
    "scripts/build-release.ps1",
    "package.json",
  ]) {
    assert.doesNotMatch(
      readFileSync(new URL(path, root), "utf8"),
      /inno-prototype|ISCC/i,
    )
  }
})

test("plain-path diagnostic changes only the path control class", () => {
  const branches = [
    ...source.matchAll(
      /#ifdef PlainPathField\s+([\s\S]*?)#else\s+([\s\S]*?)#endif/g,
    ),
  ]

  assert.equal(branches.length, 2)
  assert.equal(branches[0][1].trim(), "PathEdit: TNewEdit;")
  assert.equal(branches[0][2].trim(), "PathEdit: TNewPathEdit;")

  const withoutComments = (code: string) =>
    code.replace(/\/\/ Diagnostic[^\n]*\n/, "").trim()

  assert.equal(
    withoutComments(branches[1][1]),
    "PathEdit := TNewEdit.Create(WizardForm);",
  )
  assert.equal(
    branches[1][2].trim(),
    "PathEdit := TNewPathEdit.Create(WizardForm);",
  )
})

test("header logo has scaled top and right margins without changing the welcome logo", () => {
  assert.match(
    source,
    /WizardSmallBitmapImage.SetBounds\(\s*WizardForm.MainPanel.ClientWidth - ScaleX\(60\), ScaleY\(12\), ScaleX\(36\), ScaleY\(36\)\)/,
  )
  assert.match(source, /WizardSmallBitmapImage.Stretch := True/)
  assert.match(
    source,
    /WelcomeLogo.SetBounds\(WizardForm.ClientWidth - ScaleX\(144\), ScaleY\(52\), ScaleX\(112\), ScaleY\(112\)\)/,
  )

  for (const dpi of [96, 120, 144, 192, 288]) {
    const scale = (value: number) => Math.round((value * dpi) / 96)
    const rightMargin = scale(60) - scale(36)

    assert.ok(rightMargin >= scale(24) - 1)
    assert.ok(scale(12) > 0)
    assert.ok(scale(12) + scale(36) < scale(58))
  }
})
