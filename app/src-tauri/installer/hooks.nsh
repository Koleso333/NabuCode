; "NabuCode v1.2.3" in place of the NSIS branding at the bottom; the version comes from the setup's own file
!define MUI_CUSTOMFUNCTION_GUIINIT NabuBranding
!define MUI_CUSTOMFUNCTION_UNGUIINIT un.NabuBranding
!macro NABU_BRANDING
  GetDLLVersion "$EXEPATH" $R0 $R1
  IntOp $R2 $R0 >> 16
  IntOp $R3 $R0 & 0xFFFF
  IntOp $R4 $R1 >> 16
  StrCpy $R5 "$(^Name) v$R2.$R3.$R4"
  GetDlgItem $R6 $HWNDPARENT 1028
  SendMessage $R6 ${WM_SETTEXT} 0 "STR:$R5"
  GetDlgItem $R6 $HWNDPARENT 1256
  SendMessage $R6 ${WM_SETTEXT} 0 "STR:$R5"
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
