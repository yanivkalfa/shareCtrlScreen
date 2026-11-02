; ShareCtrlScreen: NSIS installer hooks (task W4).
;
; Tauri includes this file into its installer.nsi template through
; `bundle.windows.nsis.installerHooks` (set in tauri.windows-release.conf.json, so
; only release bundles carry it) and calls the four NSIS_HOOK_* macros at the end.
; Where Tauri calls them (installer.nsi of tauri-cli 2.12.3):
;   NSIS_HOOK_PREINSTALL    Section Install, after SetOutPath $INSTDIR and BEFORE the
;                           running-app check and the file copy
;   NSIS_HOOK_POSTINSTALL   after every file, registry value and shortcut is written
;   NSIS_HOOK_PREUNINSTALL  Section Uninstall, BEFORE the running-app check and any
;                           file is deleted
;   NSIS_HOOK_POSTUNINSTALL after files, registry values and shortcuts are removed
;
; Layout: one banner section per concern ("; ---- <concern> ----"), each a set of
; macros; the "; ---- hooks ----" section at the end calls each concern's macro
; once per hook. A future concern adds its own banner section and one line per
; hook, and touches nothing else.
;
; Two copies of sharectrl-service.exe ride in the installer: one as the bundle
; resource that lands in $INSTDIR, and one the PREINSTALL hook extracts with
; `File` to $PLUGINSDIR. This is deliberate. PREINSTALL runs BEFORE any file is
; copied to $INSTDIR, so the resource copy does not exist on disk yet when the
; old service must be stopped; and the stop must run the NEW binary's
; --stop-service (it waits for the service PROCESS to exit so its file can be
; overwritten, which `sc stop` does not, and a prior version's installed copy
; may predate the flag). The duplicate adds ~1-2 MB before NSIS compression,
; which is cheaper than an upgrade that corrupts a still-running executable.
;
; References
;   - Tauri v2, Windows installer, NSIS installer hooks (macro names and when each
;     runs): https://v2.tauri.app/distribute/windows-installer/#nsis-installer-hooks
;   - Tauri's NSIS template (hook call sites, CheckIfAppIsRunning, /UPDATE):
;     https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.12.3/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi
;   - NSIS nsExec plugin (ships with NSIS; ExecToLog writes the child's output to
;     the details pane and pushes its exit code, or "error"):
;     https://nsis.sourceforge.io/Docs/nsExec/nsExec.txt
;   - NSIS manual: File /oname, Abort (in a section: aborts the install; silent
;     installs then exit with code 2), DetailPrint, ${__FILEDIR__}, InitPluginsDir,
;     StrCpy (negative offset = from the end), StrLen:
;     https://nsis.sourceforge.io/Docs/Chapter4.html
;   - NSIS manual: RMDir /r (recursive directory delete; sets the error flag for
;     files it could not remove) and the standard safety warning never to
;     RMDir /r an unchecked/user-supplied path — hence the guard below:
;     https://nsis.sourceforge.io/Docs/Chapter4.html#instructions-rmdir
;   - Microsoft, Stopping / Starting a service (stop before replacing the binary,
;     wait for STOPPED; start, wait for RUNNING) — implemented in
;     native/crates/elevation/src/service.rs, which these hooks call:
;     https://learn.microsoft.com/windows/win32/services/stopping-a-service
;     https://learn.microsoft.com/windows/win32/services/starting-a-service
;   - sc.exe (the uninstall fallback; exit code 1060 = the service does not exist):
;     https://learn.microsoft.com/windows-server/administration/windows-commands/sc-delete

!include LogicLib.nsh

; ---- service ----
;
; The SYSTEM service (sharectrl-service.exe, Plan 04 §8b) is what reaches UAC
; prompts, the lock screen and the sign-in screen. It is installed next to
; sharectrl.exe (it launches the app from its own folder) and managed through its
; own command line, which waits for each state change and exits non-zero with a
; readable reason on failure (native/service/src/main.rs).
;
;   install / upgrade   PREINSTALL  stop the service if present (an upgrade: its
;                                   executable is about to be replaced), using the
;                                   NEW binary extracted to $PLUGINSDIR, because
;                                   the installed one may predate --stop-service
;                                   or be missing
;                       POSTINSTALL --install-service: register, or re-point an
;                                   existing registration at $INSTDIR, then start
;                                   and wait until RUNNING
;   uninstall           PREUNINSTALL --uninstall-service: stop (waiting until the
;                                   process has exited, so its file can be
;                                   deleted), then remove
;
; Any failing step aborts with a message naming it, before (install) or instead of
; (uninstall) touching the files; the step's own output is in the details pane.

!define SCT_SERVICE_EXE "sharectrl-service.exe"
!define SCT_SERVICE_NAME "ShareCtrlScreenHost"
; The service binary the release build staged next to this file
; (native/scripts/release-assets.sh: cargo install --root service-bin).
!define SCT_SERVICE_SRC "${__FILEDIR__}\service-bin\bin\${SCT_SERVICE_EXE}"

; Run `"EXE" ARGS` with its output in the details pane; abort naming STEP when it
; cannot start or exits non-zero. $0 is saved and restored around the call.
!macro SCT_SERVICE_STEP STEP EXE ARGS
  Push $0
  DetailPrint "ShareCtrlScreen service: ${STEP}"
  nsExec::ExecToLog '"${EXE}" ${ARGS}'
  Pop $0
  ${If} $0 != "0"
    DetailPrint "ShareCtrlScreen service: ${STEP} failed (exit code $0)"
    Abort "ShareCtrlScreen service: the step '${STEP}' failed (exit code $0). See the lines above for the reason."
  ${EndIf}
  Pop $0
!macroend

!macro SCT_SERVICE_PREINSTALL
  InitPluginsDir
  File "/oname=$PLUGINSDIR\${SCT_SERVICE_EXE}" "${SCT_SERVICE_SRC}"
  !insertmacro SCT_SERVICE_STEP "stop the running service before replacing files" "$PLUGINSDIR\${SCT_SERVICE_EXE}" "--stop-service"
!macroend

!macro SCT_SERVICE_POSTINSTALL
  !insertmacro SCT_SERVICE_STEP "register and start the service" "$INSTDIR\${SCT_SERVICE_EXE}" "--install-service"
!macroend

!macro SCT_SERVICE_PREUNINSTALL
  ${If} ${FileExists} "$INSTDIR\${SCT_SERVICE_EXE}"
    !insertmacro SCT_SERVICE_STEP "stop and remove the service" "$INSTDIR\${SCT_SERVICE_EXE}" "--uninstall-service"
  ${Else}
    ; The binary was deleted by hand. Remove a registration that may point at it
    ; with the system's own tool; exit code 1060 (no such service) is the normal
    ; case, so this step logs and never aborts.
    Push $0
    DetailPrint "ShareCtrlScreen service: $INSTDIR\${SCT_SERVICE_EXE} is missing; removing any registration with sc.exe"
    nsExec::ExecToLog '"$SYSDIR\sc.exe" stop ${SCT_SERVICE_NAME}'
    Pop $0
    nsExec::ExecToLog '"$SYSDIR\sc.exe" delete ${SCT_SERVICE_NAME}'
    Pop $0
    DetailPrint "ShareCtrlScreen service: sc.exe delete exit code $0 (1060 = was not installed)"
    Pop $0
  ${EndIf}
!macroend

; ---- install directory cleanup ----
;
; WHY: Tauri's NSIS uninstaller only deletes the files, registry values and
; shortcuts it RECORDED at install time. It never records files created at
; runtime, so the service's runtime log (sharectrl-service.log, written by
; SYSTEM beside the exe under --run-service — native/service/src/main.rs) and
; any future crash dump survive the uninstall, and the install directory
; survives with them. This POSTUNINSTALL step removes $INSTDIR and whatever is
; left in it, AFTER PREUNINSTALL stopped and removed the service and AFTER Tauri
; deleted the recorded files.
;
; RMDir /r is recursive and unforgiving, so it is GUARDED and never runs on an
; unchecked path (NSIS's own RMDir /r safety warning). It deletes $INSTDIR only
; when that path is the real per-machine install dir: non-empty, ending in
; "\ShareCtrlScreen", AND under "$PROGRAMFILES64\" or "$PROGRAMFILES\" — never a
; drive root or a system path. The prefix test keeps the trailing backslash so
; "C:\Program FilesX\..." cannot masquerade as being under Program Files.
;
; Best-effort: a file another process holds open is left behind but does NOT
; abort the uninstall. RMDir /r sets the error flag for anything it could not
; remove; we report that and clear it, and never Abort.

!macro SCT_CLEANUP_POSTUNINSTALL
  Push $0
  Push $1
  Push $2
  StrCpy $0 "0"                        ; $0 = "is $INSTDIR a safe install dir?"
  ${If} $INSTDIR != ""
    StrCpy $1 $INSTDIR "" -16          ; last 16 chars = len("\ShareCtrlScreen")
    ${If} $1 == "\ShareCtrlScreen"
      StrLen $2 "$PROGRAMFILES64\"
      StrCpy $1 $INSTDIR $2            ; prefix as long as "$PROGRAMFILES64\"
      ${If} $1 == "$PROGRAMFILES64\"
        StrCpy $0 "1"
      ${Else}
        StrLen $2 "$PROGRAMFILES\"
        StrCpy $1 $INSTDIR $2          ; prefix as long as "$PROGRAMFILES\"
        ${If} $1 == "$PROGRAMFILES\"
          StrCpy $0 "1"
        ${EndIf}
      ${EndIf}
    ${EndIf}
  ${EndIf}
  ${If} $0 == "1"
    DetailPrint "ShareCtrlScreen: removing the install directory and any runtime files left in it ($INSTDIR)"
    ClearErrors
    RMDir /r "$INSTDIR"
    ${If} ${Errors}
      DetailPrint "ShareCtrlScreen: some files under $INSTDIR were in use and were left behind (uninstall continues)"
      ClearErrors
    ${Else}
      DetailPrint "ShareCtrlScreen: install directory removed"
    ${EndIf}
  ${Else}
    DetailPrint "ShareCtrlScreen: '$INSTDIR' is not the expected per-machine install path; skipping directory cleanup"
  ${EndIf}
  Pop $2
  Pop $1
  Pop $0
!macroend

; ---- hooks ----

!macro NSIS_HOOK_PREINSTALL
  !insertmacro SCT_SERVICE_PREINSTALL
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro SCT_SERVICE_POSTINSTALL
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro SCT_SERVICE_PREUNINSTALL
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro SCT_CLEANUP_POSTUNINSTALL
!macroend
