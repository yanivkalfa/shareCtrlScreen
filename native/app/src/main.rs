//! Tauri shell for ShareCtrlScreen (Plan 04 §7). Native WebView2 UI host, ~2.5 MB
//! vs Electron ~85 MB. The Rust engine owns config, signaling, WebRTC, capture,
//! encode/decode, input and elevation; this binary is the thin bridge:
//!   * `#[tauri::command]`s replace each Electron `ipcMain.handle` (config,
//!     connect, permission, password, approval),
//!   * engine `UiEvent`s are forwarded to the WebView2 UI via Tauri events,
//!     replacing `ipcRenderer.on(...)`,
//!   * the video is composited as a native D3D11 child HWND (Option A, §7); the
//!     web UI frames it (chrome in the top bar, not over the pixels).
//!
//! The old renderer's WebRTC/signaling JS is obsolete here — that logic moved to
//! the Rust engine. `withGlobalTauri` lets the ported vanilla UI call
//! `window.__TAURI__.core.invoke` with no bundler.

// On Windows release builds, don't spawn a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use engine::handshake::{ApprovalDecision, Decider};
use engine::{Engine, Role, UiEvent};
use protocol::Permission;
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
}

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
    })
}

#[tauri::command]
fn clear_recents(state: tauri::State<'_, AppState>) -> Vec<String> {
    state.engine.clear_recents()
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
fn submit_password(state: tauri::State<'_, AppState>, host: String, password: String) {
    state.engine.submit_password(host, password);
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
fn save_settings(state: tauri::State<'_, AppState>, patch: serde_json::Value) {
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
    });
}

fn config_path() -> PathBuf {
    // %APPDATA%\ShareCtrlScreen\config.json (matches the old userData location).
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("ShareCtrlScreen").join("config.json")
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // The engine spawns its signaling task with `tokio::spawn`, so a Tokio
    // runtime must exist *and be entered* before `Engine::start`. Hand the same
    // runtime to Tauri so `tauri::async_runtime::spawn` shares it rather than
    // standing up a second one.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    tauri::async_runtime::set(runtime.handle().clone());
    let _runtime_guard = runtime.enter();

    let decider = Arc::new(AppDecider::default());
    let (engine, mut ui_rx, mut sig_rx) =
        Engine::start(config_path(), decider.clone() as Arc<dyn Decider>);

    let state = AppState {
        engine: engine.clone(),
        decider,
    };

    tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            get_config,
            clear_recents,
            get_role,
            connect_to,
            submit_password,
            set_permission,
            end_session,
            approve,
            save_settings,
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
                    };
                    let _ = handle.emit(name, payload);
                }
            });

            // Drive the signaling event loop into the engine.
            let engine = engine.clone();
            tauri::async_runtime::spawn(async move {
                while let Some(event) = sig_rx.recv().await {
                    engine.handle_signaling(event).await;
                }
            });

            // Create the native D3D11 video child window under the Tauri window
            // (Option A, §7). Must run on the UI thread — it is, inside setup.
            #[cfg(windows)]
            if let Some(win) = app.get_webview_window("main") {
                if let Ok(hwnd) = win.hwnd() {
                    engine::pipeline::create_video_window(hwnd.0 as isize);
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ShareCtrlScreen");
}
