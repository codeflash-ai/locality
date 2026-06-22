!include LogicLib.nsh

!define AFS_RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"
!define AFS_RUN_VALUE "AFS"
!define AFS_SHIM_MARKER "rem AFS_TERMINAL_CLI_SHIM"

!macro FIND_AFS_PROCESS_IMAGE IMAGE_NAME OUTVAR
  !if "${INSTALLMODE}" == "currentUser"
    nsis_tauri_utils::FindProcessCurrentUser "${IMAGE_NAME}"
  !else
    nsis_tauri_utils::FindProcess "${IMAGE_NAME}"
  !endif
  Pop ${OUTVAR}
!macroend

!macro STOP_AFS_PROCESS_IMAGE IMAGE_NAME
  !define UniqueID ${__LINE__}
  Push $0
  Push $1

  DetailPrint "Stopping ${IMAGE_NAME} if running..."
  StrCpy $1 0

  stop_process_loop_${UniqueID}:
    !insertmacro FIND_AFS_PROCESS_IMAGE "${IMAGE_NAME}" $0
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
    MessageBox MB_ICONSTOP|MB_OK "Could not stop ${IMAGE_NAME}. Close AFS and retry the installation."
    Abort

  stop_process_done_${UniqueID}:
  ClearErrors
  Pop $1
  Pop $0
  !undef UniqueID
!macroend

!macro STOP_AFS_INSTALL_PROCESSES
  !insertmacro STOP_AFS_PROCESS_IMAGE "afs-desktop.exe"
  !insertmacro STOP_AFS_PROCESS_IMAGE "AFS.exe"
  !insertmacro STOP_AFS_PROCESS_IMAGE "afs-cloud-files.exe"
  !insertmacro STOP_AFS_PROCESS_IMAGE "afsd.exe"
  !insertmacro STOP_AFS_PROCESS_IMAGE "afs.exe"
!macroend

!macro DELETE_AFS_INSTALLED_FILE FILE_NAME
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
    MessageBox MB_ICONSTOP|MB_OK "Could not replace $INSTDIR\${FILE_NAME}. Close AFS and retry the installation."
    Abort

  delete_file_done_${UniqueID}:
  ClearErrors
  Pop $0
  !undef UniqueID
!macroend

!macro PREPARE_AFS_SIDECAR_FILES
  !insertmacro STOP_AFS_INSTALL_PROCESSES
  !insertmacro DELETE_AFS_INSTALLED_FILE "afs.exe"
  !insertmacro DELETE_AFS_INSTALLED_FILE "afsd.exe"
  !insertmacro DELETE_AFS_INSTALLED_FILE "afs-cloud-files.exe"
!macroend

!macro DELETE_AFS_TERMINAL_SHIM SHIM_PATH
  ClearErrors
  FileOpen $0 "${SHIM_PATH}" r
  ${IfNot} ${Errors}
    FileRead $0 $1
    FileRead $0 $2
    FileClose $0
    ${If} $2 == "${AFS_SHIM_MARKER}$\r$\n"
    ${OrIf} $2 == "${AFS_SHIM_MARKER}$\n"
      Delete "${SHIM_PATH}"
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  !insertmacro STOP_AFS_INSTALL_PROCESSES
!macroend

!macro NSIS_HOOK_POSTINSTALL
  SetOutPath "$INSTDIR"
  !insertmacro PREPARE_AFS_SIDECAR_FILES
  File /oname=afs.exe "${__FILEDIR__}\..\..\..\..\apps\desktop\src-tauri\windows\afs.exe"
  File /oname=afsd.exe "${__FILEDIR__}\..\..\..\..\apps\desktop\src-tauri\windows\afsd.exe"
  File /oname=afs-cloud-files.exe "${__FILEDIR__}\..\..\..\..\apps\desktop\src-tauri\windows\afs-cloud-files.exe"
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro STOP_AFS_INSTALL_PROCESSES
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\afs.exe"
  Delete "$INSTDIR\afsd.exe"
  Delete "$INSTDIR\afs-cloud-files.exe"
  DeleteRegValue HKCU "${AFS_RUN_KEY}" "${AFS_RUN_VALUE}"
  !insertmacro DELETE_AFS_TERMINAL_SHIM "$LOCALAPPDATA\Microsoft\WindowsApps\afs.cmd"
  !insertmacro DELETE_AFS_TERMINAL_SHIM "$LOCALAPPDATA\AgentFS\bin\afs.cmd"
  RMDir "$LOCALAPPDATA\AgentFS\bin"
!macroend
