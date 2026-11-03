//! In-app updater (task U1): a non-blocking "Check for updates" and a consented
//! download → install → restart, driven from Rust so the web UI stays the thin
//! `invoke`/`listen` shell it is everywhere else (Plan 04 §7).
//!
//!   * [`updater_check`] reports whether a newer GitHub release exists (version +
//!     notes), without downloading anything;
//!   * [`updater_install`] ends any live session, downloads and installs the
//!     update (emitting `updater-progress` events the Settings panel renders),
//!     then hands the restart to whoever owns it on this install.
//!
//! The updater verifies every download against the public key committed in
//! `tauri.conf.json` (`plugins.updater.pubkey`) before it runs it; the matching
//! private key lives only in CI (`TAURI_SIGNING_PRIVATE_KEY`). The static feed is
//! `…/releases/latest/download/latest.json` (see `tauri.conf.json`).
//!
//! ## Service-aware restart (Windows, task W4 — the critical part)
//!
//! On a **service-managed** Windows install the host exe is owned and launched by
//! the SYSTEM service, and W4's NSIS hooks `--stop-service` in PREINSTALL and
//! `--install-service` (which re-registers and *starts* the service) in
//! POSTINSTALL. So the updater's silent NSIS run naturally stops the service,
//! replaces the files, and the fresh service start relaunches the host. On such
//! an install the app must **not** also relaunch itself: a second launch would
//! race the service's and trip W4's one-instance rule
//! ([`platform_windows::instance`]). We detect this with the `--service-host`
//! launch flag or [`elevation::service::query`]/`owns_sign_in`.
//!
//! On a **non-service / portable** Windows install the NSIS installer restarts the
//! app itself after a passive update, so again we do not self-relaunch. On
//! **macOS / Linux** the bundle is replaced in place and `download_and_install`
//! returns, so the app relaunches itself ([`tauri::AppHandle::restart`], the core
//! call `tauri-plugin-process` exposes to the webview).
//!
//! On Windows `download_and_install` spawns the installer and the process exits
//! during install (a Windows installer limitation the Tauri docs call out), so
//! control usually does not return to us at all — the branch below is belt and
//! braces.
//!
//! ## References
//!
//! * Tauri v2 Updater plugin — config (`endpoints`, `pubkey`,
//!   `windows.installMode`), `createUpdaterArtifacts`, the `latest.json` schema,
//!   signing env, and "the app exits automatically during install" on Windows:
//!   <https://v2.tauri.app/plugin/updater/>
//! * `Update::download_and_install` (`on_chunk: FnMut(usize, Option<u64>)`,
//!   `on_download_finish: FnOnce()`) and the `Update` fields used here:
//!   <https://docs.rs/tauri-plugin-updater/2.14.1/tauri_plugin_updater/struct.Update.html>
//! * tauri-plugin-process `relaunch`/`restart`, backed by core
//!   `AppHandle::restart`: <https://v2.tauri.app/plugin/process/>

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

use crate::AppState;

/// Who brings the app back after an update installs. Pure data so the decision
/// is unit-tested without an installer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPlan {
    /// Windows, service install: the installer's POSTINSTALL restarts the
    /// service, which relaunches the host. Do NOT self-relaunch (W4).
    ServiceRelaunches,
    /// Windows, portable install: the NSIS installer restarts the app itself.
    InstallerRelaunches,
    /// macOS / Linux: the bundle is replaced in place; the app relaunches itself.
    SelfRelaunch,
}

/// The restart decision, factored out of any OS calls so it can be tested.
pub fn restart_plan(is_windows: bool, service_managed: bool) -> RestartPlan {
    match (is_windows, service_managed) {
        (true, true) => RestartPlan::ServiceRelaunches,
        (true, false) => RestartPlan::InstallerRelaunches,
        (false, _) => RestartPlan::SelfRelaunch,
    }
}

/// The releases page a non-self-updatable install sends the user to. Opening it
/// is the only affordance for a `.deb`/`.rpm` install (task U3).
pub const RELEASES_LATEST_URL: &str =
    "https://github.com/yanivkalfa/shareCtrlScreen/releases/latest";

/// Whether this install can apply an update in place, or must be updated by hand.
/// Pure data so the decision is unit-tested without touching the OS (task U3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCapability {
    /// Windows, macOS, or a Linux AppImage mount: `download_and_install` replaces
    /// the running bundle, so the in-app updater works.
    SelfInstall,
    /// A Linux packaged install (`.deb`/`.rpm`): the feed serves an AppImage only,
    /// so the updater would reject the payload ("invalid updater binary format"),
    /// and a package update needs root write to `/usr` anyway. Send the user to
    /// the releases page instead (tauri-apps/plugins-workspace#3108).
    ManualDownload,
}

/// Can this install self-update? Windows and macOS always can (their installers
/// replace the app in place). On Linux only an AppImage can — a `.deb`/`.rpm`
/// cannot. `appimage` means "actually running from an AppImage mount" and is
/// decided from the real environment at the call site ([`running_from_appimage`],
/// which requires $APPIMAGE + $APPDIR + the exe under the mount), so this stays
/// OS-call-free and testable (task U3).
pub fn update_capability(is_windows: bool, is_macos: bool, appimage: bool) -> UpdateCapability {
    if is_windows || is_macos || appimage {
        UpdateCapability::SelfInstall
    } else {
        UpdateCapability::ManualDownload
    }
}

/// Should [`updater_install`] actually download and install? Factored out of the
/// command so the early-return guard is unit-tested without a Tauri app (task U3).
fn install_should_proceed(capability: UpdateCapability) -> bool {
    matches!(capability, UpdateCapability::SelfInstall)
}

/// Whether we are *running from* an AppImage mount, decided from the three env/OS
/// facts so it is unit-testable without touching the environment (task U3).
///
/// `$APPIMAGE` alone proves nothing: it is inherited by every child process, so a
/// `.deb`/`.rpm` build launched as a descendant of any AppImage (an AppImage
/// terminal, launcher, dock or file manager) would inherit it. Treating that as
/// self-updatable would offer "Update & restart", tear the session down, then hit
/// the very "invalid updater binary format" we are eliminating. So we require the
/// same containment `platform-linux`'s autostart [`choose_exe`] does: `$APPIMAGE`
/// set AND `$APPDIR` set AND our own executable living under `$APPDIR` (the mount).
// Off Linux the only non-test caller (`running_from_appimage`) is `cfg`-ed out,
// but the unit tests exercise it on every OS, so allow the lib-build dead code.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn running_from_appimage_inner(
    appimage: Option<&std::ffi::OsStr>,
    appdir: Option<&std::path::Path>,
    current_exe: Option<&std::path::Path>,
) -> bool {
    let (Some(appimage), Some(appdir), Some(exe)) = (appimage, appdir, current_exe) else {
        return false;
    };
    !appimage.is_empty() && !appdir.as_os_str().is_empty() && exe.starts_with(appdir)
}

/// True when we run from an AppImage mount. Linux-only; always false elsewhere.
/// Supplies the real environment to [`running_from_appimage_inner`].
fn running_from_appimage() -> bool {
    #[cfg(target_os = "linux")]
    {
        let appimage = std::env::var_os("APPIMAGE");
        let appdir = std::env::var_os("APPDIR");
        let exe = std::env::current_exe().ok();
        running_from_appimage_inner(
            appimage.as_deref(),
            appdir.as_deref().map(std::path::Path::new),
            exe.as_deref(),
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

/// The update capability of this running install, decided from the real OS/env.
fn current_update_capability() -> UpdateCapability {
    update_capability(
        cfg!(windows),
        cfg!(target_os = "macos"),
        running_from_appimage(),
    )
}

/// Is this a service-managed Windows install? The `--service-host` flag is the
/// surest sign (the service passes it, `service/src/main.rs`); failing that, an
/// auto-start service that owns sign-in counts too, for a host on such a machine
/// that happened to be launched another way.
#[cfg(windows)]
fn service_managed_install() -> bool {
    if std::env::args().any(|a| a == elevation::SERVICE_HOST_FLAG) {
        return true;
    }
    elevation::service::query()
        .map(|s| s.owns_sign_in())
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn service_managed_install() -> bool {
    false
}

fn current_restart_plan() -> RestartPlan {
    restart_plan(cfg!(windows), service_managed_install())
}

/// What the Settings panel renders. On "no update" the two versions are equal
/// and `available` is false, so the panel can say "you are up to date".
/// camelCase to match the UI contract the other commands use (Plan 04 §7).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub available: bool,
    pub current_version: String,
    pub version: String,
    pub notes: Option<String>,
    pub date: Option<String>,
    /// Can this install apply the update in app (vs. download by hand)? Only the
    /// install affordance changes — `available` is still the version compare.
    pub self_updatable: bool,
    /// Where a non-self-updatable install downloads the update by hand.
    pub download_url: String,
}

/// Non-blocking check: does a newer release exist? No download. An error (offline,
/// updater not configured in this build) is returned as a string for the UI to
/// show quietly — it must never block startup or hosting.
#[tauri::command]
pub async fn updater_check(app: AppHandle) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let self_updatable = install_should_proceed(current_update_capability());
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(update) => Ok(UpdateInfo {
            available: true,
            current_version: update.current_version,
            version: update.version,
            notes: update.body,
            date: update.date.map(|d| d.to_string()),
            self_updatable,
            download_url: RELEASES_LATEST_URL.to_string(),
        }),
        None => Ok(UpdateInfo {
            available: false,
            current_version: current.clone(),
            version: current,
            notes: None,
            date: None,
            self_updatable,
            download_url: RELEASES_LATEST_URL.to_string(),
        }),
    }
}

/// Consented update: end any live session, download + install with progress, then
/// restart per [`current_restart_plan`]. The UI warns about the live session
/// before calling this; ending it here as well guarantees a clean teardown
/// whatever the UI did.
#[tauri::command]
pub async fn updater_install(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    // A packaged Linux install (.deb/.rpm) cannot self-update: the feed serves an
    // AppImage, so `download_and_install` would reject it ("invalid updater binary
    // format"). The UI offers "Download update" instead and never calls this, but
    // guard here too so the raw error can never surface (task U3).
    if !install_should_proceed(current_update_capability()) {
        return Ok(());
    }

    state.engine.end_session();

    let updater = app.updater().map_err(|e| e.to_string())?;
    let Some(update) = updater.check().await.map_err(|e| e.to_string())? else {
        return Err("no update is available".into());
    };

    let progress = app.clone();
    let mut downloaded: usize = 0;
    update
        .download_and_install(
            move |chunk, total| {
                downloaded = downloaded.saturating_add(chunk);
                // Best-effort: a dropped progress event never fails the update.
                let _ = progress.emit(
                    "updater-progress",
                    serde_json::json!({ "downloaded": downloaded, "total": total }),
                );
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;

    // See the module docs: only macOS/Linux reach here needing a self-relaunch;
    // every Windows path leaves the restart to the installer or the service.
    if matches!(current_restart_plan(), RestartPlan::SelfRelaunch) {
        app.restart();
    }
    Ok(())
}

/// Open the releases page in the user's browser. The only update affordance on a
/// non-self-updatable install (task U3). The URL is fixed, so this command never
/// launches anything the UI did not intend.
#[tauri::command]
pub fn open_releases_page() -> Result<(), String> {
    open::that_detached(RELEASES_LATEST_URL).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::path::Path;

    #[test]
    fn a_windows_service_install_lets_the_service_relaunch() {
        // The service's POSTINSTALL start brings the host back; a self-relaunch
        // would race W4's one-instance rule.
        assert_eq!(restart_plan(true, true), RestartPlan::ServiceRelaunches);
    }

    #[test]
    fn a_windows_portable_install_lets_the_installer_relaunch() {
        assert_eq!(restart_plan(true, false), RestartPlan::InstallerRelaunches);
    }

    #[test]
    fn mac_and_linux_relaunch_themselves() {
        assert_eq!(restart_plan(false, false), RestartPlan::SelfRelaunch);
        // The service is Windows-only, so service_managed is never true off Windows,
        // but the decision must be SelfRelaunch regardless.
        assert_eq!(restart_plan(false, true), RestartPlan::SelfRelaunch);
    }

    #[test]
    fn a_linux_appimage_can_self_update() {
        // is_windows=false, is_macos=false, appimage=true.
        assert_eq!(
            update_capability(false, false, true),
            UpdateCapability::SelfInstall
        );
    }

    #[test]
    fn a_linux_package_must_download_by_hand() {
        // is_windows=false, is_macos=false, appimage=false — the .deb/.rpm case.
        assert_eq!(
            update_capability(false, false, false),
            UpdateCapability::ManualDownload
        );
    }

    #[test]
    fn windows_and_macos_always_self_update() {
        // The appimage signal is irrelevant off Linux, so it must not flip either.
        for appimage in [false, true] {
            assert_eq!(
                update_capability(true, false, appimage),
                UpdateCapability::SelfInstall,
                "windows self-installs (appimage={appimage})"
            );
            assert_eq!(
                update_capability(false, true, appimage),
                UpdateCapability::SelfInstall,
                "macos self-installs (appimage={appimage})"
            );
        }
    }

    #[test]
    fn install_proceeds_only_when_self_updatable() {
        // The guard updater_install uses: a ManualDownload install early-returns
        // and never calls download_and_install (no "invalid updater binary format").
        assert!(!install_should_proceed(UpdateCapability::ManualDownload));
        assert!(install_should_proceed(UpdateCapability::SelfInstall));
    }

    // AppImage detection mirrors autostart's `choose_exe` containment rule: only a
    // process whose own exe lives under $APPDIR is really running from the mount.
    // A bare inherited $APPIMAGE (a .deb launched from inside another AppImage)
    // must NOT count, or we would offer a self-update that then rejects the payload.
    fn appimage_detected(appimage: Option<&str>, appdir: Option<&str>, exe: Option<&str>) -> bool {
        running_from_appimage_inner(
            appimage.map(OsStr::new),
            appdir.map(Path::new),
            exe.map(Path::new),
        )
    }

    #[test]
    fn running_from_an_appimage_mount_is_detected() {
        assert!(appimage_detected(
            Some("/home/u/Share.AppImage"),
            Some("/tmp/.mount_ShareXYZ"),
            Some("/tmp/.mount_ShareXYZ/usr/bin/sharectrl"),
        ));
    }

    #[test]
    fn inherited_appimage_without_appdir_is_not_a_mount() {
        // $APPIMAGE inherited, but $APPDIR unset — a .deb/.rpm descendant.
        assert!(!appimage_detected(
            Some("/home/u/Other.AppImage"),
            None,
            Some("/usr/bin/sharectrl"),
        ));
    }

    #[test]
    fn appimage_and_appdir_but_exe_outside_the_mount_is_not_a_mount() {
        // Both vars inherited from another AppImage, but our exe is the packaged
        // /usr/bin binary, not under the mount — the dangerous case the reviewer hit.
        assert!(!appimage_detected(
            Some("/home/u/Other.AppImage"),
            Some("/tmp/.mount_OtherXYZ"),
            Some("/usr/bin/sharectrl"),
        ));
    }

    #[test]
    fn no_appimage_env_is_not_a_mount() {
        assert!(!appimage_detected(None, None, Some("/usr/bin/sharectrl")));
        // Empty values (set but blank) are also rejected.
        assert!(!appimage_detected(
            Some(""),
            Some(""),
            Some("/usr/bin/sharectrl")
        ));
    }
}
