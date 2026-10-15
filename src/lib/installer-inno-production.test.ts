import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { spawnSync } from "node:child_process"
import { test } from "node:test"
import { fileURLToPath } from "node:url"

const directory = new URL("../../src-tauri/windows/inno/", import.meta.url)

const source = readFileSync(new URL("setup.iss", directory), "utf8")

const messages: Record<string, { text: Record<string, string> }> = JSON.parse(
  readFileSync(new URL("strings.json", directory), "utf8"),
)

function section(name: string): string {
  return source.split(`[${name}]`)[1]?.split(/\r?\n\[/)[0] ?? ""
}

test("production Inno is a per-user Windows 10 x64 installation with stable identity", () => {
  for (const directive of [
    "AppId=com.squeezegram.desktop",
    "PrivilegesRequired=lowest",
    "ArchitecturesAllowed=x64compatible",
    "ArchitecturesInstallIn64BitMode=x64compatible",
    "MinVersion=10.0",
    "DefaultDirName={localappdata}\\Programs\\squeezegram",
    "UninstallLogMode=append",
    "RestartApplications=no",
    "AlwaysRestart=no",
  ]) {
    assert.ok(source.split(/\r?\n/).includes(directive), directive)
  }

  assert.doesNotMatch(source, /^Uninstallable=no|^CreateUninstallRegKey=no/m)
})

test("production payload is restricted to five named installed files and an extracted dependency", () => {
  const files = section("Files")
    .split(/\r?\n/)
    .filter((line) => line.startsWith("Source:"))

  assert.equal(files.length, 6)

  for (const filename of [
    "squeezegram.exe",
    "tdjson.dll",
    "THIRD-PARTY-NOTICES.txt",
    "LICENSE",
  ]) {
    assert.ok(files.some((line) => line.includes(`DestName: "${filename}"`)))
  }

  assert.ok(files.some((line) => line.startsWith('Source: "inno.flag"')))
  assert.equal(readFileSync(new URL("inno.flag", directory)).length, 0)
  assert.match(files[5], /Flags: dontcopy$/)
  assert.doesNotMatch(files.join("\n"), /\*|recursesubdirs|external/)
})

test("production uninstaller never recursively deletes app data or runs a legacy uninstaller", () => {
  assert.doesNotMatch(
    source,
    /\[(?:UninstallDelete|InstallDelete|UninstallRun)\]|DelTree\(|RemoveDir\(|DeleteFile\(|taskkill|cmd\.exe|powershell/i,
  )
  assert.doesNotMatch(section("Files"), /\{localappdata\}|\{userappdata\}/)
  assert.match(
    source,
    /if HasLegacyInstall then begin\s+Result := CustomMessage\('legacyInstall'\);\s+Exit;/,
  )

  for (const root of ["HKCU32", "HKCU64", "HKLM32", "HKLM64"]) {
    assert.ok(source.includes(`RegKeyExists(${root}, LegacyKey)`))
  }
})

test("production updates relaunch only the installed executable with one fixed optional argument", () => {
  const runs = section("Run")
    .split(/\r?\n/)
    .filter((line) => line.startsWith("Filename:"))

  assert.equal(runs.length, 2)
  assert.ok(
    runs.every((line) => line.startsWith('Filename: "{app}\\squeezegram.exe"')),
  )
  assert.match(runs[0], /postinstall skipifsilent; Check: IsFreshInstall$/)
  assert.match(
    runs[1],
    /Parameters: "\{code:UpdateLaunchArguments\}"; Flags: nowait; Check: IsUpdate$/,
  )
  assert.match(
    source,
    /Result := ExpandConstant\('\{param:SQZUPDATE\|0\}'\) = '1';/,
  )
  assert.match(
    source,
    /if ExpandConstant\('\{param:SQZBACKGROUND\|0\}'\) = '1' then\s+Result := '--background';/,
  )
  assert.doesNotMatch(source, /GetCmdTail|\{param:ARGS|ShellExec/)
})

test("production updates require registered ownership, unchanged directory and a marker file", () => {
  assert.match(
    source,
    /RegQueryStringValue\(HKCU64, OwnKey, 'InstallLocation', ExistingDirectory\)/,
  )
  assert.match(source, /not PathSame\(Target, ExistingDirectory\)/)
  assert.match(source, /IsUpdate and \(\(ExistingDirectory = ''\)/)
  assert.ok(
    source.includes("not FileExists(AddBackslash(Target) + 'inno.flag')"),
  )
})

test("production folder selection retains the approved plain edit and rejects drive roots", () => {
  assert.match(source, /PathEdit: TNewEdit;/)
  assert.match(source, /PathEdit := TNewEdit\.Create\(WizardForm\);/)
  assert.doesNotMatch(source, /TNewPathEdit/)
  assert.match(source, /PathHasInvalidCharacters\(Target, True\)/)
  assert.match(
    source,
    /Target := RemoveBackslashUnlessRoot\(ExpandFileName\(Target\)\)/,
  )
  assert.match(source, /PathSame\(Target, ExtractFileDrive\(Target\) \+ '\\'\)/)
  assert.match(
    source,
    /WizardForm\.DirEdit\.Text := ExpandFileName\(Trim\(PathEdit\.Text\)\)/,
  )
  assert.match(source, /KeepPathLeftToRight;/)
  assert.match(
    source,
    /WizardForm.NextButton.Caption := CustomMessage\('install'\)/,
  )
})

test("production WebView2 installation precedes payload installation and fails closed", () => {
  assert.match(
    source,
    /RuntimeKey = 'Software\\Microsoft\\EdgeUpdate\\Clients\\\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5\}'/,
  )
  assert.match(
    source,
    /HasRuntimeVersion\(HKCU32\) or HasRuntimeVersion\(HKLM32\)/,
  )
  assert.match(
    source,
    /\(VersionValue <> ''\) and \(VersionValue <> '0\.0\.0\.0'\)/,
  )

  const preparation =
    source
      .split("function PrepareToInstall")[1]
      ?.split("procedure CurUninstallStepChanged")[0] ?? ""

  assert.match(
    preparation,
    /ExtractTemporaryFile\('MicrosoftEdgeWebview2Setup\.exe'\)/,
  )
  assert.match(preparation, /'\/silent \/install'/)
  assert.match(preparation, /ewWaitUntilTerminated/)
  assert.match(
    preparation,
    /if ExitCode = 3010 then begin\s+NeedsRestart := True;/,
  )
  assert.match(preparation, /\(ExitCode <> 0\) or not HasWebView2/)
  assert.match(preparation, /CustomMessage\('runtimeFailed'\)/)
})

test("production uninstall removes only its own startup entry and leaves update startup choices alone", () => {
  const cleanup = source.split("procedure CurUninstallStepChanged")[1] ?? ""
  assert.match(cleanup, /if CurUninstallStep <> usUninstall then\s+Exit;/)
  assert.match(
    cleanup,
    /CompareText\(ExistingCommand, '"' \+ ExpandConstant\('\{app\}\\squeezegram\.exe'\) \+ '" --background'\) = 0/,
  )
  assert.match(
    cleanup,
    /if RegDeleteValue\(HKCU32, RunKey, 'squeezegram'\) then\s+RegDeleteValue\(HKCU32, ApprovedKey, 'squeezegram'\)/,
  )
  assert.equal((source.match(/RegDeleteValue\(/g) ?? []).length, 2)
  assert.doesNotMatch(source, /RegDeleteKey|RegWrite/)
})

test("production retains approved logo spacing and documented dark theme without custom drawing", () => {
  assert.match(source, /^WizardStyle=modern dark$/m)
  assert.match(
    source,
    /MainPanel\.ClientWidth - ScaleX\(60\), ScaleY\(12\), ScaleX\(36\), ScaleY\(36\)/,
  )
  assert.match(
    source,
    /ClientWidth - ScaleX\(144\), ScaleY\(52\), ScaleX\(112\), ScaleY\(112\)/,
  )
  assert.doesNotMatch(source, /DrawText|DrawButton|CreateFont|NM_CUSTOMDRAW/)
})

test("production messages cover all seven locales and every custom key without em dashes", () => {
  assert.deepEqual(Object.keys(messages).sort(), [
    "ar",
    "en",
    "es",
    "fr",
    "id",
    "pt-BR",
    "ru",
  ])
  const keys = Object.keys(messages.en.text).sort()

  const used = [
    ...source.matchAll(/CustomMessage\('([^']+)'\)|\{cm:([^}]+)\}/g),
  ].map((match) => match[1] ?? match[2])

  assert.deepEqual([...new Set(used)].sort(), keys)

  for (const [locale, { text }] of Object.entries(messages)) {
    assert.deepEqual(Object.keys(text).sort(), keys)

    for (const value of Object.values(text)) {
      assert.ok(value.trim().length > 0)
      assert.doesNotMatch(value, /[\r\n\u2014%]/u)
    }

    if (locale !== "en" && locale !== "fr") {
      assert.match(text.languageName, /beta|бета|تجريبي/)
    }
  }
})

test("finish artwork uses the app logo without moving native finish controls", () => {
  assert.match(source, /WizardForm.WizardBitmapImage2.Visible := False/)
  assert.match(source, /FinishedLogo.Parent := WizardForm.FinishedPage/)
  assert.match(
    source,
    /FinishedLogo.SetBounds\(ScaleX\(26\), ScaleY\(52\), ScaleX\(112\), ScaleY\(112\)\)/,
  )
  assert.match(
    source,
    /FinishedLogo.PngImage.Assign\(WizardForm.WizardSmallBitmapImage.PngImage\)/,
  )
  assert.ok(26 + 112 < 164)
  assert.doesNotMatch(
    source,
    /WizardForm\.(?:FinishedHeadingLabel|FinishedLabel|RunList|YesRadio|NoRadio)\./,
  )
})

test("production language selection uses Windows UI language with English fallback", () => {
  for (const directive of [
    "LanguageDetectionMethod=uilanguage",
    "UsePreviousLanguage=no",
    "ShowLanguageDialog=no",
  ]) {
    assert.ok(source.split(/\r?\n/).includes(directive))
  }

  assert.deepEqual(
    [...source.matchAll(/^Name: "([^"]+)"; MessagesFile:/gm)].map(
      (match) => match[1],
    ),
    ["en", "fr", "ru", "es", "ar", "ptBR", "id"],
  )
})

test("production generated messages match the checked-in translation source", () => {
  const result = spawnSync(
    process.execPath,
    [fileURLToPath(new URL("generate-strings.mjs", directory)), "--check"],
    { encoding: "utf8" },
  )

  assert.equal(result.status, 0, result.stderr)
})

test("production display versions retain prerelease names separately from numeric PE versions", () => {
  assert.match(source, /^AppVersion=\{#Version\}$/m)
  assert.match(source, /^VersionInfoVersion=\{#NumericVersion\}$/m)
  assert.match(
    source,
    /#ifndef NumericVersion\s+#define NumericVersion Version/,
  )
})

test("production Indonesian engine messages cover 6.7.3 additions with matching placeholders", () => {
  const translation = readFileSync(new URL("Indonesian.isl", directory), "utf8")

  const license = readFileSync(
    new URL("Indonesian.LICENSE.txt", directory),
    "utf8",
  )

  const values = Object.fromEntries(
    [...translation.matchAll(/^([^;\s=]+)=(.*)$/gm)].map((match) => [
      match[1],
      match[2].trim(),
    ]),
  )

  // Placeholder lists follow Inno Setup 6.7.3 Default.isl, including any %n breaks.
  const expected = {
    ArchiveIncorrectPassword: [],
    ArchiveIsCorrupted: [],
    ArchiveUnsupportedFormat: [],
    DownloadingLabel2: [],
    ErrorDownloading: [],
    ErrorExtracting: [],
    ExtractingLabel: [],
    RetryCancelCancel: [],
    RetryCancelRetry: [],
    RetryCancelSelectAction: [],
    SourceVerificationFailed: ["%1"],
    StatusDownloadFiles: [],
    VerificationFileHashIncorrect: [],
    VerificationFileNameIncorrect: [],
    VerificationFileSizeIncorrect: [],
    VerificationFileTagIncorrect: [],
    VerificationKeyNotFound: ["%1"],
    VerificationSignatureDoesntExist: ["%1"],
    VerificationSignatureInvalid: ["%1"],
  }

  for (const [key, placeholders] of Object.entries(expected)) {
    assert.ok(values[key]?.length > 0, key)
    assert.deepEqual(values[key].match(/%(?:[1-9]\d*|n)/g) ?? [], placeholders)
  }

  for (const key of [
    "DownloadingLabel",
    "ErrorFileHash1",
    "ErrorFileHash2",
    "ExtractionLabel",
  ]) {
    assert.equal(values[key], undefined)
  }

  assert.match(source, /#define IndonesianMessages "Indonesian.isl"/)
  assert.match(translation, /Alih bahasa oleh: MozaikTM/)
  assert.match(translation, /added translations are beta/)
  assert.match(license, /Copyright \(C\) 1997-2026 Jordan Russell/)
  assert.match(license, /Copyright \(C\) 2000-2026 Martijn Laan/)
  assert.match(license, /all copyright notices/)
  assert.match(license, /Modified versions in source or binary form/)
})
