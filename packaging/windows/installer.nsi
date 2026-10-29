; PMXSpan Windows installer (NSIS 3).
;
; Build:  makensis -DVERSION=0.4.0 -DEXE=path\to\pmxspan.exe -DOUTFILE=PMXSpan-Setup.exe packaging\windows\installer.nsi
; (packaging/build-installers.sh does this for you.)

Unicode true
SetCompressor /SOLID lzma

!ifndef VERSION
  !define VERSION "0.4.0"
!endif
!ifndef EXE
  !define EXE "..\..\target\release\pmxspan.exe"
!endif
!ifndef OUTFILE
  !define OUTFILE "PMXSpan-${VERSION}-Setup.exe"
!endif

!define APPNAME "PMXSpan"
!define UNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\${APPNAME}"

!include "MUI2.nsh"

Name "${APPNAME} ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "$PROGRAMFILES64\${APPNAME}"
InstallDirRegKey HKLM "Software\${APPNAME}" "InstallDir"
RequestExecutionLevel admin
BrandingText "${APPNAME} ${VERSION}"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${APPNAME}"
VIAddVersionKey "FileDescription" "${APPNAME} installer"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" ""

!define MUI_ICON "..\assets\pmxspan.ico"
!define MUI_UNICON "..\assets\pmxspan.ico"
!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\pmxspan.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Start PMXSpan"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Earlier releases were named ProxmoxPanel. Remove such an installation first
; (its shortcuts and uninstall entry); the user data moves on first start.
!define OLDUNINSTKEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ProxmoxPanel"
Section "-Remove ProxmoxPanel" SecRemoveOld
  SetShellVarContext all
  ReadRegStr $0 HKLM "${OLDUNINSTKEY}" "UninstallString"
  StrCmp $0 "" done
  ReadRegStr $1 HKLM "${OLDUNINSTKEY}" "InstallLocation"
  DetailPrint "Removing the previous ProxmoxPanel installation..."
  ExecWait '$0 /S _?=$1'
  Delete "$1\uninstall.exe"
  RMDir "$1"
  done:
SectionEnd

Section "PMXSpan (required)" SecMain
  SectionIn RO
  SetShellVarContext all
  SetOutPath "$INSTDIR"
  File "/oname=pmxspan.exe" "${EXE}"
  File "/oname=pmxspan.ico" "..\assets\pmxspan.ico"
  SetOutPath "$INSTDIR\licenses"
  File "..\..\assets\fonts\Inter-LICENSE.txt"
  File "..\..\assets\fonts\JetBrainsMono-LICENSE.txt"
  File "..\..\assets\fonts\Phosphor-LICENSE.txt"
  SetOutPath "$INSTDIR"

  WriteRegStr HKLM "Software\${APPNAME}" "InstallDir" "$INSTDIR"
  WriteUninstaller "$INSTDIR\uninstall.exe"

  CreateDirectory "$SMPROGRAMS\${APPNAME}"
  CreateShortcut "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk" "$INSTDIR\pmxspan.exe" "" "$INSTDIR\pmxspan.exe" 0
  CreateShortcut "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk" "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "${UNINSTKEY}" "DisplayName" "${APPNAME}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINSTKEY}" "Publisher" "${APPNAME}"
  WriteRegStr HKLM "${UNINSTKEY}" "DisplayIcon" "$INSTDIR\pmxspan.exe"
  WriteRegStr HKLM "${UNINSTKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINSTKEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINSTKEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINSTKEY}" "NoRepair" 1
SectionEnd

Section "Desktop shortcut" SecDesktop
  SetShellVarContext all
  CreateShortcut "$DESKTOP\${APPNAME}.lnk" "$INSTDIR\pmxspan.exe" "" "$INSTDIR\pmxspan.exe" 0
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "The PMXSpan application."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Put a PMXSpan shortcut on the desktop."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

Section "Uninstall"
  SetShellVarContext all
  Delete "$INSTDIR\pmxspan.exe"
  Delete "$INSTDIR\pmxspan.ico"
  Delete "$INSTDIR\uninstall.exe"
  Delete "$INSTDIR\licenses\*-LICENSE.txt"
  RMDir "$INSTDIR\licenses"
  RMDir "$INSTDIR"
  Delete "$SMPROGRAMS\${APPNAME}\${APPNAME}.lnk"
  Delete "$SMPROGRAMS\${APPNAME}\Uninstall ${APPNAME}.lnk"
  RMDir "$SMPROGRAMS\${APPNAME}"
  Delete "$DESKTOP\${APPNAME}.lnk"
  DeleteRegKey HKLM "${UNINSTKEY}"
  DeleteRegKey HKLM "Software\${APPNAME}"
  ; User data (servers, encrypted vault, IPAM database, logs) in %APPDATA%\PMXSpan is kept.
SectionEnd
