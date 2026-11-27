//! Engine `UiEvent`s → Tauri events for the web UI (replaces Electron's
//! `ipcRenderer.on`). Translation only: every decision lives in the engine or
//! in the module it hands off to (menus, pop-outs, the tray).
//!
//! The engine tags every session event with its [`SessionId`]. A viewer
//! session's events go to the [`TabManager`], which redraws the tab strip
//! (plans/08 P4); the host session keeps the single-session events the host
//! screen understands (`role-changed`, `displays`, ...).

use std::sync::Arc;

use engine::{Engine, SessionEvent, SessionId, SessionPhase, UiEvent};
use serde_json::{json, Value};
use tauri::Emitter;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::commands::displays_json;
use crate::menu::update_monitor_menu;
use crate::popout;
use crate::tabs::TabManager;
#[cfg(windows)]
use crate::tray;

/// Forward every engine event to the web UI until the engine's channel closes.
pub(crate) fn spawn(
    handle: tauri::AppHandle,
    engine: Arc<Engine>,
    tabs: Arc<TabManager>,
    mut ui_rx: UnboundedReceiver<UiEvent>,
) {
    tauri::async_runtime::spawn(async move {
        while let Some(ev) = ui_rx.recv().await {
            for (name, payload) in translate(&handle, &engine, &tabs, ev) {
                let _ = handle.emit(name, payload);
            }
        }
    });
}

/// The Tauri events for one engine event, after its side effects on native
/// UI (tray, menus, pop-out windows) and on the tabs.
fn translate(
    handle: &tauri::AppHandle,
    engine: &Arc<Engine>,
    tabs: &Arc<TabManager>,
    ev: UiEvent,
) -> Vec<(&'static str, Value)> {
    match ev {
        UiEvent::ServerStatus(s) => vec![("server-status", json!({ "status": s }))],
        UiEvent::ApprovalRequest { from } => {
            #[cfg(windows)]
            tray::show_for_approval(handle);
            vec![("approval-request", json!({ "from": from }))]
        }
        UiEvent::Toast(msg) => vec![("toast", json!({ "message": msg }))],
        UiEvent::WorkspaceChanged => {
            tabs.on_workspace_changed();
            vec![("workspace-changed", json!({}))]
        }
        // Ignored when it came from a detached window (plans/09 Decision 2).
        UiEvent::TabShortcut { step, session } => {
            tabs.shortcut(step, session);
            Vec::new()
        }
        UiEvent::Session {
            peer,
            host: true,
            ev,
            ..
        } => host_session(handle, peer, ev),
        UiEvent::Session {
            id,
            peer,
            host: false,
            ev,
        } => {
            viewer_side_effects(handle, engine, id, &ev);
            tabs.on_session(id, &peer, ev);
            Vec::new()
        }
    }
}

/// A viewer session's effects on its pop-out windows: they follow the host's
/// list and go when it is released. (The Monitor menu follows the tab on
/// screen: the TabManager.)
fn viewer_side_effects(
    handle: &tauri::AppHandle,
    engine: &Arc<Engine>,
    id: SessionId,
    ev: &SessionEvent,
) {
    match ev {
        SessionEvent::Displays { list, popouts, .. } => {
            popout::reconcile(handle, engine, id, list, popouts);
        }
        // Its threads are joined: the pop-out windows' surfaces are free, so
        // the windows can be destroyed (its main surface: the TabManager).
        SessionEvent::Released => popout::close_session(handle, engine, id),
        _ => {}
    }
}

/// This computer is being viewed or controlled: the single host session's
/// events, as the host screen understands them.
fn host_session(
    handle: &tauri::AppHandle,
    peer: String,
    ev: SessionEvent,
) -> Vec<(&'static str, Value)> {
    match ev {
        SessionEvent::Phase(SessionPhase::Negotiating { permission }) => vec![(
            "role-changed",
            json!({
                "role": "host",
                "peer": peer,
                "permission": permission.as_str(),
            }),
        )],
        SessionEvent::Phase(SessionPhase::Ended(reason)) => {
            update_monitor_menu(handle, &[], "", false, &[]);
            let mut out = Vec::new();
            if let Some(message) = reason.message() {
                out.push(("toast", json!({ "message": message })));
            }
            out.push(("role-changed", json!({ "role": "idle" })));
            out
        }
        SessionEvent::LinkTrouble { secs_left } => {
            vec![("link-trouble", json!({ "secsLeft": secs_left }))]
        }
        SessionEvent::LinkRestored => vec![("link-restored", json!({}))],
        SessionEvent::Toast(msg) => vec![("toast", json!({ "message": msg }))],
        SessionEvent::Displays {
            list,
            current,
            can_switch,
            popouts,
        } => {
            update_monitor_menu(handle, &list, &current, can_switch, &popouts);
            vec![(
                "displays",
                displays_json(&list, &current, can_switch, &popouts),
            )]
        }
        // `HostScreen` is the viewer's (what the remote screen is); this side
        // is the host.
        SessionEvent::Phase(_)
        | SessionEvent::Released
        | SessionEvent::SurfaceReleased { .. }
        | SessionEvent::HostScreen { .. } => Vec::new(),
    }
}
