; Installer hooks (bundle.windows.nsis.installerHooks in tauri.conf.json).
;
; "Start with Windows" (src/background/autostart.rs) writes a value named after the product in
; HKCU\...\Run, and clears one in ...\Explorer\StartupApproved\Run. Tauri's own uninstaller
; removes the Run value, but only that one; this hook makes sure both are gone, so an
; uninstalled app leaves nothing that points at a missing file. An update runs the old
; uninstaller with /UPDATE ($UpdateMode = 1): the values stay then, the next version uses them.
;
; App data. Tauri's "Delete the application data" checkbox ($DeleteAppDataCheckboxState, read
; when the user leaves the confirmation page) removes $APPDATA\<identifier> and
; $LOCALAPPDATA\<identifier> (com.squeezegram.desktop: only the WebView2 profile, where the UI
; keeps its language, theme and destination filter). The app's own data is elsewhere:
;   %LOCALAPPDATA%\squeezegram  Telegram session (TDLib database and its DPAPI-protected key),
;                               logs, temporary files, downloaded ffmpeg
;   %APPDATA%\squeezegram       queue database, settings.json, license.txt
; so the same checkbox also removes both of these folders.
;
; That must only happen on an uninstall the user started (Windows Settings, Apps, or the
; uninstaller itself) with the box ticked. Tauri's installer, when it finds the app already
; installed and the user picks "uninstall before installing", runs the old uninstaller
; interactively as `"<dir>\uninstall.exe" [/P] _?=<dir>`: the box is shown and can be ticked,
; but the user is reinstalling, and deleting the data would sign them out of Telegram and empty
; their queue. NSIS strips `_?=` from $CMDLINE, so it cannot be tested there; what it changes is
; that the uninstaller runs in place, from $INSTDIR as uninstall.exe, instead of from the copy in
; $TEMP that every other launch makes of itself. That is the test below. When it matches, the
; checkbox state is cleared before Tauri's own deletion runs, so its folders are kept too.
;
; In passive (/P) and silent (/S) modes the confirmation page is skipped, the box never exists
; and its state stays empty: nothing is deleted. During an update ($UpdateMode = 1) Tauri never
; deletes; neither does this hook.

!include "${__FILEDIR__}\appearance.nsh"

; The folders deleted on a real uninstall; a test script can point them elsewhere.
!define /ifndef SQZ_LOCAL_DATA "$LOCALAPPDATA\squeezegram"
!define /ifndef SQZ_ROAMING_DATA "$APPDATA\squeezegram"

!macro NSIS_HOOK_PREUNINSTALL
  ${If} $EXEDIR == $INSTDIR
  ${OrIf} $EXEFILE == "uninstall.exe"
    ${If} $DeleteAppDataCheckboxState = 1
      DetailPrint "Started by the installer: keeping the app data."
    ${EndIf}
    StrCpy $DeleteAppDataCheckboxState 0
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${PRODUCTNAME}"
  ${EndIf}

  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    SetShellVarContext current
    RmDir /r "${SQZ_LOCAL_DATA}"
    RmDir /r "${SQZ_ROAMING_DATA}"
  ${EndIf}
!macroend
