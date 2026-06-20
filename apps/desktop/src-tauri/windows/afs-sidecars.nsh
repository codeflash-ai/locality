!macro NSIS_HOOK_POSTINSTALL
  SetOutPath "$INSTDIR"
  File /oname=afs.exe "${__FILEDIR__}\afs.exe"
  File /oname=afsd.exe "${__FILEDIR__}\afsd.exe"
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  Delete "$INSTDIR\afs.exe"
  Delete "$INSTDIR\afsd.exe"
!macroend
