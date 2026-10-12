; Installer hooks (bundle.windows.nsis.installerHooks in tauri.conf.json).
;
; "Start with Windows" (src/background/autostart.rs) writes a value named after the product in
; HKCU\...\Run, and clears one in ...\Explorer\StartupApproved\Run. Tauri's own uninstaller
; removes the Run value, but only that one; this hook makes sure both are gone, so an
; uninstalled app leaves nothing that points at a missing file. An update runs the old
; uninstaller with /UPDATE ($UpdateMode = 1): the values stay then, the next version uses them.
!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $UpdateMode <> 1
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
    DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run" "${PRODUCTNAME}"
  ${EndIf}
!macroend
