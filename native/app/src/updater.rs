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
}

/// Non-blocking check: does a newer release exist? No download. An error (offline,
/// updater not configured in this build) is returned as a string for the UI to
/// show quietly — it must never block startup or hosting.
#[tauri::command]
pub async fn updater_check(app: AppHandle) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let updater = app.updater().map_err(|e| e.to_string())?;
    match updater.check().await.map_err(|e| e.to_string())? {
        Some(update) => Ok(UpdateInfo {
            available: true,
            current_version: update.current_version,
            version: update.version,
            notes: update.body,
            date: update.date.map(|d| d.to_string()),
        }),
        None => Ok(UpdateInfo {
            available: false,
            current_version: current.clone(),
            version: current,
            notes: None,
            date: None,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
