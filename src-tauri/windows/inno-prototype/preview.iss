#ifndef ArabicMessages
  #define ArabicMessages "compiler:Languages\Arabic.isl"
#endif
#ifndef IndonesianMessages
  #define IndonesianMessages "compiler:Languages\Indonesian.isl"
#endif

[Setup]
AppName=squeezegram preview
AppVersion=0.1.0
AppId=squeezegram-inno-preview-only
DefaultDirName={localappdata}\Programs\squeezegram-preview
CreateAppDir=no
Uninstallable=no
CreateUninstallRegKey=no
PrivilegesRequired=lowest
UsePreviousLanguage=no
UsePreviousAppDir=no
UsePreviousTasks=no
UsePreviousSetupType=no
LanguageDetectionMethod=uilanguage
ShowLanguageDialog=no
DisableWelcomePage=no
DisableDirPage=yes
DisableProgramGroupPage=yes
DisableReadyPage=yes
CloseApplications=no
RestartApplications=no
WizardStyle=modern dark
WizardSizePercent=120
SetupIconFile=..\..\icons\icon.ico
WizardSmallImageFile=..\..\icons\128x128@2x.png
OutputBaseFilename=squeezegram-installer-inno-preview
OutputDir=output
Compression=lzma2
SolidCompression=yes

[Languages]
Name: "en"; MessagesFile: "compiler:Default.isl"
Name: "fr"; MessagesFile: "compiler:Languages\French.isl"
Name: "ru"; MessagesFile: "compiler:Languages\Russian.isl"
Name: "es"; MessagesFile: "compiler:Languages\Spanish.isl"
Name: "ar"; MessagesFile: "{#ArabicMessages}"
Name: "ptBR"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
Name: "id"; MessagesFile: "{#IndonesianMessages}"

#include "strings.iss"

[Code]
var
  ControlsPage: TWizardPage;
  PathEdit: TNewPathEdit;
  DemoButton: TNewButton;
  DemoProgress: TNewProgressBar;

function InitializeSetup: Boolean;
begin
  Result := not WizardSilent;
end;

procedure PathChanged(Sender: TObject);
begin
  WizardForm.NextButton.Enabled := Trim(PathEdit.Text) <> '';
end;

procedure BrowseClicked(Sender: TObject);
var
  Folder: String;
begin
  Folder := PathEdit.Text;
  if BrowseForFolder(CustomMessage('folder'), Folder, False) then
    PathEdit.Text := Folder;
end;

procedure AdvanceDemo(Sender: TObject);
begin
  if DemoProgress.Position < 100 then
    DemoProgress.Position := DemoProgress.Position + 25;
  DemoButton.Enabled := DemoProgress.Position < 100;
end;

procedure AddLabel(const Text: String; Top: Integer);
var
  LabelControl: TNewStaticText;
begin
  LabelControl := TNewStaticText.Create(WizardForm);
  LabelControl.Parent := ControlsPage.Surface;
  LabelControl.SetBounds(0, ScaleY(Top), ControlsPage.SurfaceWidth, ScaleY(28));
  LabelControl.AutoSize := False;
  LabelControl.WordWrap := True;
  LabelControl.Caption := Text;
end;

procedure InitializeWizard;
var
  BrowseButton: TNewButton;
  ShortcutCheck: TNewCheckBox;
  UserRadio, DisabledRadio: TNewRadioButton;
  WelcomeLogo: TBitmapImage;
begin
  WizardForm.Caption := CustomMessage('caption');
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
  WizardForm.WelcomeLabel2.Left := ScaleX(32);
  WizardForm.WelcomeLabel2.Top := WizardForm.WelcomeLabel1.Top + ScaleY(112);
  WizardForm.WelcomeLabel2.Width := WizardForm.ClientWidth - ScaleX(64);
  WizardForm.WelcomeLabel2.Height := WizardForm.WelcomePage.ClientHeight - WizardForm.WelcomeLabel2.Top - ScaleY(24);
  WizardForm.WelcomeLabel2.Caption := CustomMessage('summary') + #13#10#13#10 +
    CustomMessage('version') + #13#10#13#10 + CustomMessage('notice') + #13#10 + CustomMessage('languageName');
  ControlsPage := CreateCustomPage(wpWelcome, CustomMessage('controls'), CustomMessage('notice'));
  AddLabel(CustomMessage('folder'), 4);
  PathEdit := TNewPathEdit.Create(WizardForm);
  PathEdit.Parent := ControlsPage.Surface;
  PathEdit.SetBounds(0, ScaleY(32), ControlsPage.SurfaceWidth - ScaleX(120), ScaleY(24));
  PathEdit.Text := ExpandConstant('{localappdata}\Programs\squeezegram-preview');
  PathEdit.OnChange := @PathChanged;
  BrowseButton := TNewButton.Create(WizardForm);
  BrowseButton.Parent := ControlsPage.Surface;
  BrowseButton.SetBounds(ControlsPage.SurfaceWidth - ScaleX(112), ScaleY(30), ScaleX(112), ScaleY(28));
  BrowseButton.Caption := CustomMessage('browse');
  BrowseButton.OnClick := @BrowseClicked;
  ShortcutCheck := TNewCheckBox.Create(WizardForm);
  ShortcutCheck.Parent := ControlsPage.Surface;
  ShortcutCheck.SetBounds(0, ScaleY(72), ControlsPage.SurfaceWidth, ScaleY(24));
  ShortcutCheck.Caption := CustomMessage('shortcut');
  UserRadio := TNewRadioButton.Create(WizardForm);
  UserRadio.Parent := ControlsPage.Surface;
  UserRadio.SetBounds(0, ScaleY(108), ControlsPage.SurfaceWidth, ScaleY(24));
  UserRadio.Caption := CustomMessage('currentUser');
  UserRadio.Checked := True;
  DisabledRadio := TNewRadioButton.Create(WizardForm);
  DisabledRadio.Parent := ControlsPage.Surface;
  DisabledRadio.SetBounds(0, ScaleY(140), ControlsPage.SurfaceWidth, ScaleY(24));
  DisabledRadio.Caption := CustomMessage('disabledOption');
  DisabledRadio.Enabled := False;
  DemoProgress := TNewProgressBar.Create(WizardForm);
  DemoProgress.Parent := ControlsPage.Surface;
  DemoProgress.SetBounds(0, ScaleY(180), ControlsPage.SurfaceWidth - ScaleX(160), ScaleY(16));
  DemoProgress.Max := 100;
  DemoButton := TNewButton.Create(WizardForm);
  DemoButton.Parent := ControlsPage.Surface;
  DemoButton.SetBounds(ControlsPage.SurfaceWidth - ScaleX(152), ScaleY(174), ScaleX(152), ScaleY(28));
  DemoButton.Caption := CustomMessage('demo');
  DemoButton.OnClick := @AdvanceDemo;
  AddLabel(CustomMessage('progress'), 216);
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  WizardForm.CancelButton.Caption := CustomMessage('cancel');
  WizardForm.BackButton.Caption := CustomMessage('back');
  if CurPageID = ControlsPage.ID then begin
    WizardForm.NextButton.Caption := CustomMessage('close');
    PathChanged(PathEdit);
  end else
    WizardForm.NextButton.Caption := CustomMessage('continue');
end;

function NextButtonClick(CurPageID: Integer): Boolean;
begin
  Result := CurPageID = wpWelcome;
  if CurPageID = ControlsPage.ID then
    WizardForm.Close;
end;

procedure CancelButtonClick(CurPageID: Integer; var Cancel, Confirm: Boolean);
begin
  Cancel := True;
  Confirm := False;
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := CustomMessage('notice');
end;
