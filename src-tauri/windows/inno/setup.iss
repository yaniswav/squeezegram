#if Ver < EncodeVer(6, 7, 3)
  #error Inno Setup 6.7.3 or newer is required.
#endif
#ifndef AppSource
  #error AppSource is required.
#endif
#ifndef TdjsonSource
  #error TdjsonSource is required.
#endif
#ifndef NoticesSource
  #error NoticesSource is required.
#endif
#ifndef LicenseSource
  #error LicenseSource is required.
#endif
#ifndef BootstrapperSource
  #error BootstrapperSource is required.
#endif
#ifndef Version
  #error Version is required.
#endif
#ifndef NumericVersion
  #define NumericVersion Version
#endif
#ifndef OutputDirectory
  #error OutputDirectory is required.
#endif
#ifndef OutputFilename
  #define OutputFilename "squeezegram-" + Version + "-setup-x64"
#endif
#ifndef ArabicMessages
  #define ArabicMessages "compiler:Languages\Arabic.isl"
#endif
#ifndef IndonesianMessages
  #define IndonesianMessages "Indonesian.isl"
#endif

[Setup]
AppId=com.squeezegram.desktop
AppName=squeezegram
AppVersion={#Version}
VersionInfoVersion={#NumericVersion}
AppPublisher=squeezegram
DefaultDirName={localappdata}\Programs\squeezegram
DefaultGroupName=squeezegram
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
UninstallDisplayIcon={app}\squeezegram.exe
UsePreviousLanguage=no
LanguageDetectionMethod=uilanguage
ShowLanguageDialog=no
DisableWelcomePage=no
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
DisableFinishedPage=no
DisableStartupPrompt=yes
CloseApplications=yes
CloseApplicationsFilter=squeezegram.exe,tdjson.dll
RestartApplications=no
RestartIfNeededByRun=no
AlwaysRestart=no
WizardStyle=modern dark
WizardSizePercent=120
SetupIconFile=..\..\icons\icon.ico
WizardSmallImageFile=..\..\icons\128x128@2x.png
OutputBaseFilename={#OutputFilename}
OutputDir={#OutputDirectory}
Compression=lzma2
SolidCompression=yes
UninstallLogMode=append

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "fr"; MessagesFile: "compiler:Languages\French.isl"
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "es"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "ar"; MessagesFile: "{#ArabicMessages}"
Name: "ptBR"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "id"; MessagesFile: "{#IndonesianMessages}"

#include "strings.iss"

[Tasks]
Name: "desktopicon"; Description: "{cm:shortcut}"; Flags: unchecked

[Files]
Source: "{#AppSource}"; DestDir: "{app}"; DestName: "squeezegram.exe"; Flags: ignoreversion
Source: "{#TdjsonSource}"; DestDir: "{app}"; DestName: "tdjson.dll"; Flags: ignoreversion
Source: "{#NoticesSource}"; DestDir: "{app}"; DestName: "THIRD-PARTY-NOTICES.txt"; Flags: ignoreversion
Source: "{#LicenseSource}"; DestDir: "{app}"; DestName: "LICENSE"; Flags: ignoreversion
Source: "inno.flag"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#BootstrapperSource}"; DestName: "MicrosoftEdgeWebview2Setup.exe"; Flags: dontcopy

[Icons]
Name: "{autoprograms}\squeezegram"; Filename: "{app}\squeezegram.exe"
Name: "{autodesktop}\squeezegram"; Filename: "{app}\squeezegram.exe"; Tasks: desktopicon

[Run]
Filename: "{app}\squeezegram.exe"; Description: "{cm:launch}"; Flags: nowait postinstall skipifsilent; Check: IsFreshInstall
Filename: "{app}\squeezegram.exe"; Parameters: "{code:UpdateLaunchArguments}"; Flags: nowait; Check: IsUpdate

[Code]
const
  LegacyKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\squeezegram';
  OwnKey = 'Software\Microsoft\Windows\CurrentVersion\Uninstall\com.squeezegram.desktop_is1';
  RuntimeKey = 'Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';
  RunKey = 'Software\Microsoft\Windows\CurrentVersion\Run';
  ApprovedKey = 'Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run';

var
  FolderPage: TWizardPage;
  PathEdit: TNewEdit;
  ShortcutCheck: TNewCheckBox;
  ExistingDirectory: String;

function GetWindowLong(Window: HWND; Index: Integer): Longint;
  external 'GetWindowLongW@user32.dll stdcall';
function SetWindowLong(Window: HWND; Index: Integer; Value: Longint): Longint;
  external 'SetWindowLongW@user32.dll stdcall';
function SetWindowPos(Window, InsertAfter: HWND; X, Y, Width, Height: Integer; Flags: Cardinal): Boolean;
  external 'SetWindowPos@user32.dll stdcall';

procedure KeepPathLeftToRight;
begin
  // Windows paths keep their reading order even when the surrounding Arabic page is mirrored.
  SetWindowLong(PathEdit.Handle, -20,
    GetWindowLong(PathEdit.Handle, -20) and not ($00400000 or $00002000 or $00001000));
  SetWindowLong(PathEdit.Handle, -16,
    GetWindowLong(PathEdit.Handle, -16) and not 3);
  SetWindowPos(PathEdit.Handle, 0, 0, 0, 0, 0, $0037);
end;

function IsUpdate: Boolean;
begin
  Result := ExpandConstant('{param:SQZUPDATE|0}') = '1';
end;

function IsFreshInstall: Boolean;
begin
  Result := not IsUpdate;
end;

function UpdateLaunchArguments(Param: String): String;
begin
  // Only this fixed argument is accepted from the updater, never an arbitrary command line.
  Result := '';
  if ExpandConstant('{param:SQZBACKGROUND|0}') = '1' then
    Result := '--background';
end;

function HasRuntimeVersion(Root: Integer): Boolean;
var
  VersionValue: String;
begin
  Result := RegQueryStringValue(Root, RuntimeKey, 'pv', VersionValue) and
    (VersionValue <> '') and (VersionValue <> '0.0.0.0');
end;

function HasWebView2: Boolean;
begin
  // Microsoft's detection contract uses the 32-bit EdgeUpdate registry view.
  Result := HasRuntimeVersion(HKCU32) or HasRuntimeVersion(HKLM32);
end;

function HasLegacyInstall: Boolean;
begin
  Result := RegKeyExists(HKCU32, LegacyKey) or RegKeyExists(HKCU64, LegacyKey) or
    RegKeyExists(HKLM32, LegacyKey) or RegKeyExists(HKLM64, LegacyKey);
end;

function TargetError(const Directory: String): String;
var
  Target: String;
begin
  Result := '';
  Target := RemoveBackslashUnlessRoot(Trim(Directory));
  if (Length(Target) < 4) or (Copy(Target, 2, 2) <> ':\') or
    PathHasInvalidCharacters(Target, True) or
    (Pos('"', Target) <> 0) then begin
    Result := CustomMessage('invalidPath');
    Exit;
  end;
  Target := RemoveBackslashUnlessRoot(ExpandFileName(Target));
  if PathSame(Target, ExtractFileDrive(Target) + '\') then begin
    Result := CustomMessage('invalidPath');
    Exit;
  end;
  // Moving an existing installation can orphan its uninstall record and shortcuts.
  if (ExistingDirectory <> '') and not PathSame(Target, ExistingDirectory) then begin
    Result := CustomMessage('keepDirectory');
    Exit;
  end;
  if FileExists(AddBackslash(Target) + 'squeezegram.exe') and
    not FileExists(AddBackslash(Target) + 'inno.flag') then begin
    Result := CustomMessage('occupiedPath');
    Exit;
  end;
  if IsUpdate and ((ExistingDirectory = '') or
    not FileExists(AddBackslash(Target) + 'inno.flag') or
    not FileExists(AddBackslash(Target) + 'squeezegram.exe')) then
    Result := CustomMessage('invalidUpdate');
end;

procedure PathChanged(Sender: TObject);
begin
  WizardForm.NextButton.Enabled := Trim(PathEdit.Text) <> '';
end;

procedure BrowseClicked(Sender: TObject);
var
  Directory: String;
begin
  Directory := PathEdit.Text;
  if BrowseForFolder(CustomMessage('folder'), Directory, False) then
    PathEdit.Text := Directory;
end;

procedure InitializeWizard;
var
  BrowseButton: TNewButton;
  FolderLabel, AccountLabel: TNewStaticText;
  WelcomeLogo, FinishedLogo: TBitmapImage;
begin
  RegQueryStringValue(HKCU64, OwnKey, 'InstallLocation', ExistingDirectory);
  ExistingDirectory := RemoveBackslashUnlessRoot(ExistingDirectory);
  WizardForm.Caption := 'squeezegram';
  WizardForm.WizardBitmapImage.Visible := False;
  WizardForm.WelcomeLabel1.Left := ScaleX(32);
  WizardForm.WelcomeLabel1.Width := WizardForm.ClientWidth - ScaleX(200);
  WizardForm.WelcomeLabel1.Caption := CustomMessage('welcome') + #13#10 + 'squeezegram.';
  WizardForm.WelcomeLabel1.Font.Size := 24;
  WizardForm.WelcomeLabel1.Height := ScaleY(96);
  WelcomeLogo := TBitmapImage.Create(WizardForm);
  WelcomeLogo.Parent := WizardForm.WelcomePage;
  WelcomeLogo.SetBounds(WizardForm.ClientWidth - ScaleX(144), ScaleY(52), ScaleX(112), ScaleY(112));
  WelcomeLogo.PngImage.Assign(WizardForm.WizardSmallBitmapImage.PngImage);
  WelcomeLogo.Stretch := True;
  WizardForm.WizardBitmapImage2.Visible := False;
  FinishedLogo := TBitmapImage.Create(WizardForm);
  FinishedLogo.Parent := WizardForm.FinishedPage;
  FinishedLogo.SetBounds(ScaleX(26), ScaleY(52), ScaleX(112), ScaleY(112));
  FinishedLogo.PngImage.Assign(WizardForm.WizardSmallBitmapImage.PngImage);
  FinishedLogo.Stretch := True;
  WizardForm.WizardSmallBitmapImage.SetBounds(
    WizardForm.MainPanel.ClientWidth - ScaleX(60), ScaleY(12), ScaleX(36), ScaleY(36));
  WizardForm.WizardSmallBitmapImage.Stretch := True;
  WizardForm.WelcomeLabel2.Left := ScaleX(32);
  WizardForm.WelcomeLabel2.Top := WizardForm.WelcomeLabel1.Top + ScaleY(112);
  WizardForm.WelcomeLabel2.Width := WizardForm.ClientWidth - ScaleX(64);
  WizardForm.WelcomeLabel2.Height := WizardForm.WelcomePage.ClientHeight - WizardForm.WelcomeLabel2.Top - ScaleY(24);
  WizardForm.WelcomeLabel2.Caption := CustomMessage('summary') + #13#10#13#10 +
    CustomMessage('versionLabel') + ' {#Version}' + #13#10#13#10 + CustomMessage('currentUser') + #13#10 +
    CustomMessage('languageName');
  FolderPage := CreateCustomPage(wpWelcome, CustomMessage('folder'), CustomMessage('currentUser'));
  FolderLabel := TNewStaticText.Create(WizardForm);
  FolderLabel.Parent := FolderPage.Surface;
  FolderLabel.SetBounds(0, ScaleY(4), FolderPage.SurfaceWidth, ScaleY(28));
  FolderLabel.Caption := CustomMessage('folder');
  // Keep the plain edit variant accepted by the owner.
  PathEdit := TNewEdit.Create(WizardForm);
  PathEdit.Parent := FolderPage.Surface;
  PathEdit.SetBounds(0, ScaleY(32), FolderPage.SurfaceWidth - ScaleX(120), ScaleY(24));
  PathEdit.Text := WizardDirValue;
  PathEdit.OnChange := @PathChanged;
  KeepPathLeftToRight;
  BrowseButton := TNewButton.Create(WizardForm);
  BrowseButton.Parent := FolderPage.Surface;
  BrowseButton.SetBounds(FolderPage.SurfaceWidth - ScaleX(112), ScaleY(30), ScaleX(112), ScaleY(28));
  BrowseButton.Caption := CustomMessage('browse');
  BrowseButton.OnClick := @BrowseClicked;
  ShortcutCheck := TNewCheckBox.Create(WizardForm);
  ShortcutCheck.Parent := FolderPage.Surface;
  ShortcutCheck.SetBounds(0, ScaleY(72), FolderPage.SurfaceWidth, ScaleY(24));
  ShortcutCheck.Caption := CustomMessage('shortcut');
  ShortcutCheck.Checked := WizardIsTaskSelected('desktopicon');
  AccountLabel := TNewStaticText.Create(WizardForm);
  AccountLabel.Parent := FolderPage.Surface;
  AccountLabel.SetBounds(0, ScaleY(108), FolderPage.SurfaceWidth, ScaleY(60));
  AccountLabel.AutoSize := False;
  AccountLabel.WordWrap := True;
  AccountLabel.Caption := CustomMessage('dataKept');
end;

function ShouldSkipPage(PageID: Integer): Boolean;
begin
  Result := (PageID = wpSelectTasks) or
    (IsUpdate and ((PageID = wpWelcome) or (PageID = FolderPage.ID)));
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  ErrorText: String;
begin
  Result := True;
  if CurPageID = FolderPage.ID then begin
    ErrorText := TargetError(PathEdit.Text);
    Result := ErrorText = '';
    if not Result then
      MsgBox(ErrorText, mbError, MB_OK)
    else begin
      WizardForm.DirEdit.Text := ExpandFileName(Trim(PathEdit.Text));
      if ShortcutCheck.Checked then
        WizardSelectTasks('desktopicon')
      else
        WizardSelectTasks('');
    end;
  end;
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = FolderPage.ID then begin
    WizardForm.NextButton.Caption := CustomMessage('install');
    KeepPathLeftToRight;
    PathChanged(PathEdit);
  end;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  ExitCode: Integer;
begin
  Result := '';
  if HasLegacyInstall then begin
    Result := CustomMessage('legacyInstall');
    Exit;
  end;
  Result := TargetError(WizardDirValue);
  if Result <> '' then
    Exit;
  if HasWebView2 then
    Exit;
  ExtractTemporaryFile('MicrosoftEdgeWebview2Setup.exe');
  if not Exec(ExpandConstant('{tmp}\MicrosoftEdgeWebview2Setup.exe'), '/silent /install',
    '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then begin
    Result := CustomMessage('runtimeFailed');
    Exit;
  end;
  if ExitCode = 3010 then begin
    NeedsRestart := True;
    Result := CustomMessage('runtimeRestart');
  end else if (ExitCode <> 0) or not HasWebView2 then
    Result := CustomMessage('runtimeFailed');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  ExistingCommand: String;
begin
  if CurUninstallStep <> usUninstall then
    Exit;
  // Never clear another copy's startup choice or its Task Manager approval state.
  if RegQueryStringValue(HKCU32, RunKey, 'squeezegram', ExistingCommand) and
    (CompareText(ExistingCommand, '"' + ExpandConstant('{app}\squeezegram.exe') + '" --background') = 0) then begin
    if RegDeleteValue(HKCU32, RunKey, 'squeezegram') then
      RegDeleteValue(HKCU32, ApprovedKey, 'squeezegram');
  end;
end;
