; Installer hooks (bundle.windows.nsis.installerHooks in tauri.conf.json).
;
; "Start with Windows" (src/background/autostart.rs) writes a value named after the product in
; HKCU\...\Run, and clears one in ...\Explorer\StartupApproved\Run. Tauri's own uninstaller
; removes the Run value, but only that one; this hook makes sure both are gone, so an
; uninstalled app leaves nothing that points at a missing file. An update runs the old
; uninstaller with /UPDATE ($UpdateMode = 1): the values stay then, the next version uses them.
;
; Tauri's "Delete app data" checkbox only removes $APPDATA\<identifier> and
; $LOCALAPPDATA\<identifier> (com.squeezegram.desktop), folders this app never writes to. Its
; data (TDLib session and database, logs, settings, downloaded ffmpeg) is in
; %LOCALAPPDATA%\squeezegram, so the same checkbox ($DeleteAppDataCheckboxState, set when the
; user leaves the confirmation page) also removes that folder. It is never removed during an
; update or when the box is unticked.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${PRODUCTNAME}"
  ${EndIf}

  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    RmDir /r "$LOCALAPPDATA\squeezegram"
  ${EndIf}
!macroend
