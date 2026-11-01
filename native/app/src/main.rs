//! Tauri shell for ShareCtrlScreen. The Rust engine owns config, signaling,
//! WebRTC and the session; the OS backend (`platform-windows`/`-macos`/`-linux`)
//! owns capture, codecs, the video surface, input, clipboard and secrets. This
//! binary is the thin bridge:
//!   * `#[tauri::command]`s replace each Electron `ipcMain.handle` (config,
//!     connect, permission, password, approval),
//!   * engine `UiEvent`s are forwarded to the WebView2 UI via Tauri events,
//!     replacing `ipcRenderer.on(...)`,
//!   * the video is a native child surface the platform creates under this
//!     window; the web UI frames it (the toolbar slides over it on hover).
//!
//! The old renderer's WebRTC/signaling JS is obsolete here — that logic moved to
//! the Rust engine. `withGlobalTauri` lets the ported vanilla UI call
//! `window.__TAURI__.core.invoke` with no bundler.

// On Windows release builds, don't spawn a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use codec_openh264::{Availability, OpenH264};
use engine::handshake::{ApprovalDecision, Decider};
use engine::{Engine, Role, UiEvent};
use platform_api::{OsPermission, PermissionReport, PermissionState, Platform, AUTOSTART_FLAG};
use protocol::Permission;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use tauri::{Emitter, Manager};
use tokio::sync::{oneshot, Mutex};

/// Approve-mode decider backed by the WebView2 modal: `decide` parks on a
/// oneshot channel that the `approve` command resolves when the human clicks.
#[derive(Default)]
struct AppDecider {
    pending: Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>>,
}

impl Decider for AppDecider {
    fn decide<'a>(
        &'a self,
        from: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalDecision> + Send + 'a>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.pending.lock().await.insert(from.to_string(), tx);
            // 30 s auto-deny (contract §3.3): whichever fires first wins.
            match tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                Ok(Ok(decision)) => decision,
                _ => ApprovalDecision::Deny,
            }
        })
    }
}

impl AppDecider {
    async fn resolve(&self, from: &str, decision: ApprovalDecision) {
        if let Some(tx) = self.pending.lock().await.remove(from) {
            let _ = tx.send(decision);
        }
    }
}

/// Shared Tauri state.
struct AppState {
    engine: Arc<Engine>,
    decider: Arc<AppDecider>,
    /// The Cisco OpenH264 software codec (Settings → Advanced).
    openh264: Arc<OpenH264>,
    /// `Some(advice)` when this computer has no H.264 codec at all; shown next
    /// to the OpenH264 toggle.
    h264_problem: H264Problem,
}

/// Asks the platform whether H.264 is missing altogether (it can change when
/// the user toggles OpenH264, so it is asked each time, not cached).
type H264Problem = Arc<dyn Fn() -> Option<String> + Send + Sync>;

// ---- commands (one per old ipcMain.handle) ----------------------------------

/// UI-shaped config (camelCase, as the ported renderer expects). Deliberately
/// omits `passwordHash` — the webview never needs it — and exposes a
/// `hasPassword` boolean instead, which is all the settings modal uses.
#[tauri::command]
fn get_config(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let c = state.engine.config();
    serde_json::json!({
        "uuid": c.uuid,
        "serverUrl": c.server_url,
        "mode": match c.mode { protocol::Mode::Password => "password", _ => "approve" },
        "hasPassword": c.password_hash.is_some(),
        "passwordPermission": c.password_permission.as_str(),
        "shareAudio": c.share_audio,
        "shareDisplayId": c.share_display_id,
        "recentIds": c.recent_ids,
        "captureShortcuts": c.capture_shortcuts,
        "autoLogin": c.auto_login,
        "clipboardSync": c.clipboard_sync,
        "pasteDroppedFiles": c.paste_dropped_files,
        "startWithWindows": c.start_with_windows,
        "useOpenH264": c.use_openh264,
        "translateModifiers": c.translate_modifiers,
        // Count only — the saved passwords themselves never reach the WebView.
        "savedLoginCount": c.saved_logins.len(),
    })
}

/// What the UI needs to word itself for this OS and to list the monitors.
/// Built from the platform alone so it can be tested against the mock.
fn platform_info_json(platform: &dyn Platform) -> serde_json::Value {
    let info = platform.info();
    let displays: Vec<_> = platform
        .displays()
        .into_iter()
        .map(|d| {
            serde_json::json!({
                "id": d.id,
                "name": d.name,
                "x": d.x,
                "y": d.y,
                "width": d.width,
                "height": d.height,
                "scale": d.scale,
                "primary": d.primary,
            })
        })
        .collect();
    serde_json::json!({
        "name": info.name,
        "startAtLoginLabel": info.start_at_login_label,
        "shortcutCaptureHint": info.shortcut_capture_hint,
        "loginScreenSupported": info.login_screen_supported,
        "downloadsDir": platform.paths().downloads_dir.display().to_string(),
        "displays": displays,
    })
}

#[tauri::command]
fn get_platform_info(state: tauri::State<'_, AppState>) -> serde_json::Value {
    platform_info_json(state.engine.platform().as_ref())
}

#[tauri::command]
fn clear_recents(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.engine.clear_recents()
}

/// Current signaling status, queried by the UI on boot. The one-shot
/// `server-status` event can fire before the WebView subscribes, so the boot
/// path pulls the live value instead of relying solely on that event.
#[tauri::command]
fn get_server_status(state: tauri::State<'_, AppState>) -> String {
    if state.engine.server_connected() {
        "connected".into()
    } else {
        "disconnected".into()
    }
}

#[tauri::command]
fn get_role(state: tauri::State<'_, AppState>) -> String {
    match state.engine.role() {
        Role::Idle => "idle".into(),
        Role::Host { .. } => "host".into(),
        Role::Viewer { .. } => "viewer".into(),
    }
}

#[tauri::command]
fn connect_to(state: tauri::State<'_, AppState>, id: String) {
    state.engine.connect_to(id);
}

#[tauri::command]
fn submit_password(
    state: tauri::State<'_, AppState>,
    host: String,
    password: String,
    remember: Option<bool>,
) {
    state
        .engine
        .submit_password(host, password, remember.unwrap_or(false));
}

#[tauri::command]
fn set_permission(state: tauri::State<'_, AppState>, value: String) {
    let perm = if value == "control" {
        Permission::Control
    } else {
        Permission::View
    };
    state.engine.set_permission(perm);
}

#[tauri::command]
fn end_session(state: tauri::State<'_, AppState>) {
    state.engine.end_session();
}

/// Viewer: ask the host for a fresh keyframe (clears smear after packet loss).
#[tauri::command]
fn request_refresh(state: tauri::State<'_, AppState>) {
    state.engine.request_refresh();
}

/// Viewer: hide/show the native video surface so web modals (settings) are
/// visible mid-session — the native surface otherwise covers them.
#[tauri::command]
fn set_video_visible(state: tauri::State<'_, AppState>, visible: bool) {
    state.engine.set_video_visible(visible);
}

#[tauri::command]
async fn approve(
    state: tauri::State<'_, AppState>,
    from: String,
    decision: String,
) -> Result<(), ()> {
    let d = match decision.as_str() {
        "control" => ApprovalDecision::Allow(Permission::Control),
        "view" => ApprovalDecision::Allow(Permission::View),
        _ => ApprovalDecision::Deny,
    };
    state.decider.resolve(&from, d).await;
    Ok(())
}

#[tauri::command]
fn save_settings(
    state: tauri::State<'_, AppState>,
    patch: serde_json::Value,
) -> Result<(), String> {
    state.engine.update_config(|cfg| {
        if let Some(mode) = patch.get("mode").and_then(|v| v.as_str()) {
            cfg.mode = if mode == "password" {
                protocol::Mode::Password
            } else {
                protocol::Mode::Approve
            };
        }
        if let Some(pw) = patch.get("password").and_then(|v| v.as_str()) {
            if pw.is_empty() {
                cfg.set_password(None);
            } else {
                cfg.set_password(Some(pw));
            }
        }
        if let Some(perm) = patch.get("passwordPermission").and_then(|v| v.as_str()) {
            cfg.password_permission = if perm == "control" {
                Permission::Control
            } else {
                Permission::View
            };
        }
        if let Some(url) = patch.get("serverUrl").and_then(|v| v.as_str()) {
            cfg.server_url = url.to_string();
        }
        if let Some(b) = patch.get("shareAudio").and_then(|v| v.as_bool()) {
            cfg.share_audio = b;
        }
        if let Some(b) = patch.get("captureShortcuts").and_then(|v| v.as_bool()) {
            cfg.capture_shortcuts = b;
        }
        if let Some(v) = patch.get("shareDisplayId") {
            cfg.share_display_id = v.as_str().filter(|s| !s.is_empty()).map(String::from);
        }
        if let Some(b) = patch.get("autoLogin").and_then(|v| v.as_bool()) {
            cfg.auto_login = b;
        }
        if let Some(b) = patch.get("clipboardSync").and_then(|v| v.as_bool()) {
            cfg.clipboard_sync = b;
        }
        if let Some(b) = patch.get("pasteDroppedFiles").and_then(|v| v.as_bool()) {
            cfg.paste_dropped_files = b;
        }
        // Explicit "forget everything" from the settings modal.
        if patch
            .get("clearSavedLogins")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            cfg.saved_logins.clear();
        }
        if let Some(b) = patch.get("startWithWindows").and_then(|v| v.as_bool()) {
            cfg.start_with_windows = b;
        }
        if let Some(b) = patch.get("useOpenH264").and_then(|v| v.as_bool()) {
            cfg.use_openh264 = b;
        }
        // Read by the running session's key translator on its next key event
        // (`Engine::update_config` publishes it); held keys release as sent.
        if let Some(b) = patch.get("translateModifiers").and_then(|v| v.as_bool()) {
            cfg.translate_modifiers = b;
        }
    });
    // Takes effect for the next codec the engine creates (Cisco's licence: the
    // user can disable and re-enable the binary at any time).
    if let Some(b) = patch.get("useOpenH264").and_then(|v| v.as_bool()) {
        state.openh264.set_enabled(b);
    }

    // Apply the startup registration AFTER the config is persisted. A failure is
    // surfaced rather than swallowed (a startup toggle that silently does nothing
    // is worse than one that says why) — and the saved setting is rolled back, so
    // the config never claims an autostart that is not registered.
    if let Some(b) = patch.get("startWithWindows").and_then(|v| v.as_bool()) {
        if let Err(e) = state.engine.platform().autostart().set_enabled(b) {
            tracing::warn!("start at sign-in: {e}");
            state
                .engine
                .update_config(|cfg| cfg.start_with_windows = !b);
            return Err(e);
        }
    }
    Ok(())
}

/// Wire name of a permission state. Spelled out (not `{:?}`) so the UI contract
/// cannot drift if the enum's Debug output ever changes.
fn permission_state_name(s: PermissionState) -> &'static str {
    match s {
        PermissionState::Granted => "granted",
        PermissionState::Denied => "denied",
        PermissionState::NotDetermined => "notDetermined",
        PermissionState::NotApplicable => "notApplicable",
    }
}

fn permissions_json(report: &PermissionReport) -> serde_json::Value {
    serde_json::json!({
        "screenCapture": permission_state_name(report.screen_capture),
        "inputInjection": permission_state_name(report.input_injection),
        "inputMonitoring": permission_state_name(report.input_monitoring),
        "notes": report.notes,
    })
}

fn parse_os_permission(name: &str) -> Result<OsPermission, String> {
    match name {
        "screenCapture" => Ok(OsPermission::ScreenCapture),
        "inputInjection" => Ok(OsPermission::InputInjection),
        "inputMonitoring" => Ok(OsPermission::InputMonitoring),
        other => Err(format!("unknown permission '{other}'")),
    }
}

/// OS permissions the preflight modal lists. The UI polls this while the modal
/// is open, so it only reads state — it never prompts.
#[tauri::command]
fn get_permissions(state: tauri::State<'_, AppState>) -> serde_json::Value {
    permissions_json(&state.engine.platform().permissions())
}

/// Show the OS prompt (or open the matching settings pane) for one permission.
/// The outcome is not returned: the user answers out of band and the UI
/// re-polls `get_permissions`.
#[tauri::command]
fn request_permission(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    let permission = parse_os_permission(&name)?;
    state.engine.platform().request_permission(permission);
    Ok(())
}

/// What Settings → Advanced shows about the software codec: Cisco's required
/// notice and licence text, and where the download stands.
#[tauri::command]
fn openh264_info(state: tauri::State<'_, AppState>) -> serde_json::Value {
    let availability = state.openh264.availability();
    serde_json::json!({
        "notice": codec_openh264::NOTICE,
        "license": codec_openh264::LICENSE_TEXT,
        "version": codec_openh264::VERSION,
        "ready": availability == Availability::Ready,
        "status": availability.describe(),
        "problem": (state.h264_problem)(),
    })
}

/// "Download now": fetch the Cisco binary ahead of need (or retry after an
/// offline first run). Blocking network I/O, so off the async runtime.
#[tauri::command]
async fn openh264_download(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let openh264 = state.openh264.clone();
    tauri::async_runtime::spawn_blocking(move || openh264.ensure())
        .await
        .map_err(|e| format!("download task failed: {e}"))?
        .map_err(|e| e.to_string())?;
    Ok(state.openh264.availability().describe())
}

/// Log to `app.log` in the platform's log directory, since release builds have
/// no console. `RUST_LOG` still overrides the level.
fn init_logging(log_dir: &Path) {
    let path = log_dir.join("app.log");
    let _ = std::fs::create_dir_all(log_dir);
    // Verbose for our own media/transport crates so a black screen leaves a
    // trail (encode/decode errors, ICE state); quiet for noisy dependencies.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(
            "info,engine=debug,transport=debug,signaling=debug,platform_windows=debug,\
             platform_macos=debug,platform_linux=debug,codec=debug,codec_openh264=debug,\
             capture=debug,str0m=warn",
        )
    });
    match std::fs::File::create(&path) {
        Ok(file) => {
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .with_env_filter(filter)
                .init();
            tracing::info!("log file: {}", path.display());
        }
        Err(_) => {
            // Fall back to stdout (debug build with a console).
            tracing_subscriber::fmt().with_env_filter(filter).init();
        }
    }
}

/// The OS backend for this build, and its Cisco OpenH264 handle (the one
/// setting that reaches into the codec layer, so it is kept before the
/// platform is type-erased).
fn make_platform() -> (Arc<dyn Platform>, Arc<OpenH264>, H264Problem) {
    #[cfg(windows)]
    let platform = Arc::new(platform_windows::WindowsPlatform::new());
    #[cfg(target_os = "macos")]
    let platform = Arc::new(platform_macos::MacPlatform::new());
    #[cfg(target_os = "linux")]
    let platform = Arc::new(platform_linux::LinuxPlatform::new());
    let openh264 = platform.openh264();
    // Only the Windows backend can tell yet whether the OS has its own H.264;
    // elsewhere no claim is made (`None`).
    #[cfg(windows)]
    let problem: H264Problem = {
        let p = platform.clone();
        Arc::new(move || p.h264_problem())
    };
    #[cfg(not(windows))]
    let problem: H264Problem = Arc::new(|| None);
    let platform: Arc<dyn Platform> = platform;
    (platform, openh264, problem)
}

/// Create the native video surface under the main window and hand it to the
/// engine. Must run on the UI thread (it does, inside `setup`). Failure is not
/// fatal: the app still works as a host, and a viewer session explains why it
/// cannot show the screen.
fn attach_video_surface(engine: &Engine, win: &tauri::WebviewWindow) {
    let handles = win
        .window_handle()
        .map_err(|e| e.to_string())
        .and_then(|w| {
            win.display_handle()
                .map(|d| (w.as_raw(), d.as_raw()))
                .map_err(|e| e.to_string())
        });
    match handles {
        Ok((window, display)) => match engine.platform().create_surface(window, display) {
            Ok(surface) => engine.attach_surface(surface),
            Err(e) => tracing::warn!("no native video surface: {e}"),
        },
        Err(e) => tracing::warn!("no window handle for the video surface: {e}"),
    }
}

fn main() {
    let (platform, openh264, h264_problem) = make_platform();
    init_logging(&platform.paths().log_dir);
    tracing::info!("platform: {}", platform.info().name);

    // The engine spawns its signaling task with `tokio::spawn`, so a Tokio
    // runtime must exist *and be entered* before `Engine::start`. Hand the same
    // runtime to Tauri so `tauri::async_runtime::spawn` shares it rather than
    // standing up a second one.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    tauri::async_runtime::set(runtime.handle().clone());
    let _runtime_guard = runtime.enter();

    let decider = Arc::new(AppDecider::default());
    let (engine, mut ui_rx, mut sig_rx) =
        Engine::start(platform.clone(), decider.clone() as Arc<dyn Decider>);

    // Keep the autostart entry honest. It records an absolute path, which goes
    // stale whenever the app is updated, reinstalled or run from a different
    // build — and a stale entry fails silently at sign-in.
    platform
        .autostart()
        .reconcile(engine.config().start_with_windows);
    // The saved choice, before any session can create a codec.
    openh264.set_enabled(engine.config().use_openh264);

    let state = AppState {
        engine: engine.clone(),
        decider,
        openh264,
        h264_problem,
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_platform_info,
            clear_recents,
            get_server_status,
            get_role,
            connect_to,
            submit_password,
            set_permission,
            end_session,
            request_refresh,
            set_video_visible,
            approve,
            save_settings,
            get_permissions,
            request_permission,
            openh264_info,
            openh264_download,
        ])
        .setup(move |app| {
            let handle = app.handle().clone();

            // Bridge engine UiEvents → WebView2 events (replaces ipcRenderer.on).
            tauri::async_runtime::spawn(async move {
                while let Some(ev) = ui_rx.recv().await {
                    let (name, payload) = match ev {
                        UiEvent::ServerStatus(s) => {
                            ("server-status", serde_json::json!({ "status": s }))
                        }
                        UiEvent::ApprovalRequest { from } => {
                            ("approval-request", serde_json::json!({ "from": from }))
                        }
                        UiEvent::PasswordRequired { from } => {
                            ("password-required", serde_json::json!({ "from": from }))
                        }
                        // Structured, not Debug-formatted: the UI needs the peer
                        // id and permission as real fields.
                        UiEvent::RoleChanged(role) => (
                            "role-changed",
                            match role {
                                Role::Idle => serde_json::json!({ "role": "idle" }),
                                Role::Host { peer, permission } => serde_json::json!({
                                    "role": "host",
                                    "peer": peer,
                                    "permission": permission.as_str(),
                                }),
                                Role::Viewer { peer, permission } => serde_json::json!({
                                    "role": "viewer",
                                    "peer": peer,
                                    "permission": permission.as_str(),
                                }),
                            },
                        ),
                        UiEvent::Toast(msg) => ("toast", serde_json::json!({ "message": msg })),
                        UiEvent::LinkTrouble { secs_left } => {
                            ("link-trouble", serde_json::json!({ "secsLeft": secs_left }))
                        }
                        UiEvent::LinkRestored => ("link-restored", serde_json::json!({})),
                    };
                    let _ = handle.emit(name, payload);
                }
            });

            // Drive the signaling event loop into the engine.
            let sig_engine = engine.clone();
            tauri::async_runtime::spawn(async move {
                while let Some(event) = sig_rx.recv().await {
                    sig_engine.handle_signaling(event).await;
                }
            });

            if let Some(win) = app.get_webview_window("main") {
                attach_video_surface(&engine, &win);
                // Started at sign-in: come up minimized rather than throwing a
                // window in the user's face on every boot. The app is useful
                // purely by being reachable, so it does not need focus.
                if std::env::args().any(|a| a == AUTOSTART_FLAG) {
                    let _ = win.minimize();
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ShareCtrlScreen");
}

#[cfg(test)]
mod tests {
    use super::*;
    use platform_api::mock::MockPlatform;

    fn platform_with(report: PermissionReport) -> MockPlatform {
        let platform = MockPlatform::new(&std::env::temp_dir());
        *platform.permissions.lock().expect("mock lock") = report;
        platform
    }

    #[test]
    fn denied_report_serialises_to_the_ui_shape() {
        let platform = platform_with(PermissionReport {
            screen_capture: PermissionState::Denied,
            input_injection: PermissionState::NotDetermined,
            input_monitoring: PermissionState::NotApplicable,
            notes: vec!["a caveat".into()],
        });
        assert_eq!(
            permissions_json(&platform.permissions()),
            serde_json::json!({
                "screenCapture": "denied",
                "inputInjection": "notDetermined",
                "inputMonitoring": "notApplicable",
                "notes": ["a caveat"],
            })
        );
    }

    #[test]
    fn granted_report_serialises_to_the_ui_shape() {
        let platform = platform_with(PermissionReport {
            screen_capture: PermissionState::Granted,
            input_injection: PermissionState::Granted,
            input_monitoring: PermissionState::Granted,
            notes: Vec::new(),
        });
        assert_eq!(
            permissions_json(&platform.permissions()),
            serde_json::json!({
                "screenCapture": "granted",
                "inputInjection": "granted",
                "inputMonitoring": "granted",
                "notes": [],
            })
        );
    }

    #[test]
    fn request_names_map_to_permissions_and_unknown_is_an_error() {
        assert_eq!(
            parse_os_permission("screenCapture"),
            Ok(OsPermission::ScreenCapture)
        );
        assert_eq!(
            parse_os_permission("inputInjection"),
            Ok(OsPermission::InputInjection)
        );
        assert_eq!(
            parse_os_permission("inputMonitoring"),
            Ok(OsPermission::InputMonitoring)
        );
        assert!(parse_os_permission("camera").is_err());
    }

    #[test]
    fn a_parsed_request_reaches_the_platform() {
        let platform = platform_with(PermissionReport::all_not_applicable());
        let p = parse_os_permission("inputInjection").expect("known name");
        platform.request_permission(p);
        assert_eq!(
            *platform.permission_requests.lock().expect("mock lock"),
            vec![OsPermission::InputInjection]
        );
    }

    #[test]
    fn platform_info_has_the_shape_the_ui_reads() {
        let root = std::env::temp_dir().join("sct-app-platform-info");
        let platform = MockPlatform::new(&root);
        let v = platform_info_json(&platform);

        assert_eq!(v["name"], "mock");
        assert_eq!(v["startAtLoginLabel"], "Start at sign-in");
        assert!(v["shortcutCaptureHint"].is_string());
        assert_eq!(v["loginScreenSupported"], false);
        assert_eq!(
            v["downloadsDir"],
            root.join("downloads").display().to_string()
        );

        let displays = v["displays"].as_array().expect("displays is an array");
        assert_eq!(displays.len(), 1);
        let d = &displays[0];
        assert_eq!(d["id"], "mock-0");
        assert_eq!(d["name"], "Mock display");
        assert_eq!((d["x"].as_i64(), d["y"].as_i64()), (Some(0), Some(0)));
        assert!(d["width"].is_u64() && d["height"].is_u64());
        assert_eq!(d["scale"], 1.0);
        assert_eq!(d["primary"], true);
    }
}
