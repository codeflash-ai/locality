!include LogicLib.nsh

!define LOCALITY_RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define LOCALITY_RUN_VALUE "Locality"
!define LOCALITY_SHIM_MARKER "rem LOCALITY_TERMINAL_CLI_SHIM"

!macro FIND_LOCALITY_PROCESS_IMAGE IMAGE_NAME OUTVAR
  !if "${INSTALLMODE}" == "currentUser"
    nsis_tauri_utils::FindProcessCurrentUser "${IMAGE_NAME}"
  !else
    nsis_tauri_utils::FindProcess "${IMAGE_NAME}"
  !endif
  Pop ${OUTVAR}
!macroend

!macro STOP_LOCALITY_PROCESS_IMAGE IMAGE_NAME
  !define UniqueID ${__LINE__}
  Push $0
  Push $1

  DetailPrint "Stopping ${IMAGE_NAME} if running..."
  StrCpy $1 0

  stop_process_loop_${UniqueID}:
    !insertmacro FIND_LOCALITY_PROCESS_IMAGE "${IMAGE_NAME}" $0
    ${If} $0 != 0
      Goto stop_process_done_${UniqueID}
    ${EndIf}

    ClearErrors
    nsExec::ExecToLog '"$SYSDIR\taskkill.exe" /F /T /IM "${IMAGE_NAME}"'
    Pop $0
    Sleep 500
    IntOp $1 $1 + 1
    ${If} $1 < 20
      Goto stop_process_loop_${UniqueID}
    ${EndIf}

    DetailPrint "Timed out waiting for ${IMAGE_NAME} to stop."
    MessageBox MB_ICONSTOP|MB_OK "Could not stop ${IMAGE_NAME}. Close Locality and retry the installation."
    Abort

  stop_process_done_${UniqueID}:
  ClearErrors
  Pop $1
  Pop $0
  !undef UniqueID
!macroend

!macro STOP_LOCALITY_INSTALL_PROCESSES
  !insertmacro STOP_LOCALITY_PROCESS_IMAGE "locality-desktop.exe"
  !insertmacro STOP_LOCALITY_PROCESS_IMAGE "Locality.exe"
  !insertmacro STOP_LOCALITY_PROCESS_IMAGE "locality-cloud-files.exe"
  !insertmacro STOP_LOCALITY_PROCESS_IMAGE "localityd.exe"
  !insertmacro STOP_LOCALITY_PROCESS_IMAGE "loc.exe"
!macroend

!macro DELETE_LOCALITY_INSTALLED_FILE FILE_NAME
  !define UniqueID ${__LINE__}
  Push $0

  StrCpy $0 0
  delete_file_loop_${UniqueID}:
    ${IfNot} ${FileExists} "$INSTDIR\${FILE_NAME}"
      Goto delete_file_done_${UniqueID}
    ${EndIf}

    ClearErrors
    Delete "$INSTDIR\${FILE_NAME}"
    ${IfNot} ${FileExists} "$INSTDIR\${FILE_NAME}"
      Goto delete_file_done_${UniqueID}
    ${EndIf}

    Sleep 500
    IntOp $0 $0 + 1
    ${If} $0 < 20
      Goto delete_file_loop_${UniqueID}
    ${EndIf}

    DetailPrint "Timed out waiting for $INSTDIR\${FILE_NAME} to be writable."
    MessageBox MB_ICONSTOP|MB_OK "Could not replace $INSTDIR\${FILE_NAME}. Close Locality and retry the installation."
    Abort

  delete_file_done_${UniqueID}:
  ClearErrors
  Pop $0
  !undef UniqueID
!macroend

!macro PREPARE_LOCALITY_SIDECAR_FILES
  !insertmacro STOP_LOCALITY_INSTALL_PROCESSES
  !insertmacro DELETE_LOCALITY_INSTALLED_FILE "loc.exe"
  !insertmacro DELETE_LOCALITY_INSTALLED_FILE "localityd.exe"
  !insertmacro DELETE_LOCALITY_INSTALLED_FILE "locality-cloud-files.exe"
!macroend

!macro DELETE_LOCALITY_TERMINAL_SHIM SHIM_PATH
  ClearErrors
  FileOpen $0 "${SHIM_PATH}" r
  ${IfNot} ${Errors}
    FileRead $0 $1
    FileRead $0 $2
    FileClose $0
    ${If} $2 == "${LOCALITY_SHIM_MARKER}$\r$\n"
    ${OrIf} $2 == "${LOCALITY_SHIM_MARKER}$\n"
      Delete "${SHIM_PATH}"
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro STOP_LOCALITY_INSTALL_PROCESSES
!macroend

!macro NSIS_HOOK_POSTINSTALL
  SetOutPath "$INSTDIR"
  !insertmacro PREPARE_LOCALITY_SIDECAR_FILES
  File /oname=loc.exe "${__FILEDIR__}\..\..\..\..\apps\desktop\src-tauri\windows\loc.exe"
  File /oname=localityd.exe "${__FILEDIR__}\..\..\..\..\apps\desktop\src-tauri\windows\localityd.exe"
  File /oname=locality-cloud-files.exe "${__FILEDIR__}\..\..\..\..\apps\desktop\src-tauri\windows\locality-cloud-files.exe"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro STOP_LOCALITY_INSTALL_PROCESSES
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\loc.exe"
  Delete "$INSTDIR\localityd.exe"
  Delete "$INSTDIR\locality-cloud-files.exe"
  DeleteRegValue HKCU "${LOCALITY_RUN_KEY}" "${LOCALITY_RUN_VALUE}"
  !insertmacro DELETE_LOCALITY_TERMINAL_SHIM "$LOCALAPPDATA\Microsoft\WindowsApps\loc.cmd"
  !insertmacro DELETE_LOCALITY_TERMINAL_SHIM "$LOCALAPPDATA\Locality\bin\loc.cmd"
  RMDir "$LOCALAPPDATA\Locality\bin"
!macroend
