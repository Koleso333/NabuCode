; "NabuCode v1.2.3" in place of the NSIS branding at the bottom. Tauri defines VERSION only after this file,
; so the full version (with -beta.N) is read from tauri.conf.json at compile time
!searchparse /file "${__FILEDIR__}\..\tauri.conf.json" '"version": "' NABU_VERSION '"'
!define MUI_CUSTOMFUNCTION_GUIINIT NabuBranding
!define MUI_CUSTOMFUNCTION_UNGUIINIT un.NabuBranding
!macro NABU_BRANDING
  GetDlgItem $R6 $HWNDPARENT 1028
  SendMessage $R6 ${WM_SETTEXT} 0 "STR:$(^Name) v${NABU_VERSION}"
  GetDlgItem $R6 $HWNDPARENT 1256
  SendMessage $R6 ${WM_SETTEXT} 0 "STR:$(^Name) v${NABU_VERSION}"
!macroend
Function NabuBranding
  !insertmacro NABU_BRANDING
FunctionEnd
Function un.NabuBranding
  !insertmacro NABU_BRANDING
FunctionEnd

; The shared server (nabu.exe) leaves a few seconds after the last window closes; its file is replaceable only then
!macro NABU_STOP_APP
  !insertmacro CheckIfAppIsRunning "$INSTDIR\${MAINBINARYNAME}.exe" "${PRODUCTNAME}"
  StrCpy $R9 0
  ${DoWhile} ${FileExists} "$INSTDIR\nabu.exe"
    Delete "$INSTDIR\nabu.exe"
    IntOp $R9 $R9 + 1
    ${IfThen} $R9 > 40 ${|} ${ExitDo} ${|}
    Sleep 250
  ${Loop}
  !insertmacro CheckIfAppIsRunning "$INSTDIR\nabu.exe" "${PRODUCTNAME}"
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro NABU_STOP_APP
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro NABU_STOP_APP
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $DeleteAppDataCheckboxState = 1
  ${AndIf} $UpdateMode <> 1
    RmDir /r "$PROFILE\.nabucode"
  ${EndIf}
!macroend
