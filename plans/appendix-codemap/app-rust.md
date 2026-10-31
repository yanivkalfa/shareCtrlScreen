<!-- Generated 2026-10-31 by a parallel code-reading sweep (20 haiku readers, one per subsystem). Line numbers refer to the tree at commit 5a0d195. Facts were spot-checked but not every line was re-verified; treat as a map, not a spec. -->

# app-rust

## summary
The `app` crate (binary `sharectrl`, native/app/Cargo.toml:9-11) is the thin Tauri 2 shell over `engine` and `protocol`. It has one source file, native/app/src/main.rs (407 lines), plus build.rs (`tauri_build::build()` only), tauri.conf.json, capabilities/default.json and sharectrl.manifest. main.rs does four things: (1) initialises file logging (init_logging, main.rs:266-292); (2) builds a multi-thread tokio runtime, hands it to Tauri and enters it before Engine::start (main.rs:301-303, 306-307); (3) exposes 12 `#[tauri::command]`s that map UI calls onto Engine methods (main.rs:71-254 and 146-165, registered at 323-336); (4) in `setup` (main.rs:337-404) starts two forwarding tasks (engine UiEvent -> Tauri events at 341-379; signaling events -> engine.handle_signaling at 383-387) and, on Windows only, creates a native D3D11 child HWND under the main window (main.rs:391-395). The web UI in native/app/ui frames that video surface (main.rs:1-9).

Almost all of the crate is OS-agnostic: commands, config mapping, approve/deny logic (AppDecider, main.rs:28-58), event bridge, logging and the tokio/Tauri wiring. Windows-specific code is confined to five cfg(windows) sites: the windows_subsystem attribute (main.rs:16), video child window create/show/hide (main.rs:160-164, 391-395), the autostart registry reconcile and set_enabled (main.rs:246-252, 313-314), the --autostart minimise branch (main.rs:396-401), and the config base path that depends on APPDATA (main.rs:256-262, which is not cfg-gated but falls back to the temp dir on other OSes). The Windows-only integration is the video surface: on macOS/Linux no surface is created at all, and set_video_visible is a no-op (main.rs:163-164). The manifest (sharectrl.manifest) requests PerMonitorV2 DPI and asInvoker/uiAccess=false, but nothing in the repo references it (build.rs has no WindowsAttributes/manifest call), so it is not embedded by this build.

The crate has no tests (no #[cfg(test)] in main.rs, and the app crate's Cargo.toml has no test deps). Its only dependency that touches the OS directly is through `engine::pipeline` and `engine::autostart`, which were not read in this unit; their exact signatures are inferred from call sites only.

## public_api
- native/app/src/main.rs:16 - crate attribute windows_subsystem="windows" (cfg all(windows, not(debug_assertions))); no pub items exported (binary crate)
- native/app/src/main.rs:31 - struct AppDecider { pending: tokio Mutex<HashMap<String, oneshot::Sender<ApprovalDecision>>> } (private), impl Decider at 35-50, resolve() at 53-57
- native/app/src/main.rs:61 - struct AppState { engine: Arc<Engine>, decider: Arc<AppDecider> } (private, managed by Tauri at 322)
- native/app/src/main.rs:71 get_config -> serde_json::Value (camelCase keys, omits passwordHash, exposes hasPassword and savedLoginCount)
- native/app/src/main.rs:94 clear_recents -> Vec<String>
- native/app/src/main.rs:102 get_server_status -> String ('connected'|'disconnected')
- native/app/src/main.rs:111 get_role -> String ('idle'|'host'|'viewer')
- native/app/src/main.rs:120 connect_to(id: String)
- native/app/src/main.rs:125 submit_password(host, password, remember: Option<bool>) (remember defaults false)
- native/app/src/main.rs:137 set_permission(value: String) (anything other than 'control' maps to View)
- native/app/src/main.rs:147 end_session
- native/app/src/main.rs:153 request_refresh (viewer asks host for keyframe)
- native/app/src/main.rs:160 set_video_visible(visible: bool) (sync; no-op off Windows)
- native/app/src/main.rs:168 async approve(from: String, decision: String) -> Result<(),()> ('control'|'view'|other=Deny)
- native/app/src/main.rs:183 save_settings(patch: serde_json::Value) -> Result<(),String> (applies a partial JSON patch to config via engine.update_config, then Windows autostart)
- native/app/src/main.rs:256 config_path() -> PathBuf (%APPDATA%\ShareCtrlScreen\config.json, fallback temp_dir)
- native/app/src/main.rs:266 init_logging() (app.log beside config; EnvFilter default 'info,engine=debug,transport=debug,codec=debug,capture=debug,signaling=debug,str0m=warn')
- native/app/src/main.rs:294 main()

## platform_deps
- **APPDATA environment variable (std::env::var) used as the per-user config root, falling back to std::env::temp_dir()** @ native/app/src/main.rs:256-262 (also used by init_logging at 266-267)
  - purpose: location of config.json and app.log (`ShareCtrlScreen` subfolder)
  - portable: macOS needs the per-user Application Support directory (~/Library/Application Support/<id>) and Linux needs XDG config/state (~/.config or $XDG_CONFIG_HOME). Currently APPDATA is unset on those systems, so the code silently writes config and log to the system temp directory, which is not durable. The replacement must return the OS-correct per-user dir and keep the same file names. Note that tauri.conf identifier is dev.netameta.sharectrl (tauri.conf.json:5) and could be reused.
- **#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")] - GUI subsystem to suppress the console in release** @ native/app/src/main.rs:16
  - purpose: no console window on Windows release builds
  - portable: No equivalent needed on macOS (a bundled .app has no console) or Linux (a launcher starts a terminal-less process). Keep debug builds attached to a console for logs.
- **Native D3D11 child HWND created under the Tauri window's HWND: engine::pipeline::create_video_window(hwnd.0 as isize) obtained from tauri::WebviewWindow::hwnd()** @ native/app/src/main.rs:391-395 (inside setup; Windows only)
  - purpose: video surface (Option A, main.rs:8). Rendering is native; the web UI draws chrome around it and pixels are never drawn in the web layer.
  - portable: Contract: a native render surface owned by the window and parented to it. It must be created on the UI/main thread during setup, be positioned by the webview layout (the web UI frames it), be z-ordered above the webview, and be resizable as the window changes. On macOS this is an NSView/CALayer child of the window's content view; on Linux it is a native child window (X11 or Wayland subsurface) or a non-native alternative. No such surface exists on non-Windows: the app creates none (main.rs:391 is cfg(windows)), so video has no target on those OSes today.
- **engine::pipeline::set_video_visible(bool) - hides/shows the native video child HWND** @ native/app/src/main.rs:160-165 (Tauri command; cfg(windows) call, else `let _ = visible`); also called internally by pipeline.rs:1100, 1226, 1240
  - purpose: lets web modals (settings etc.) draw over video; the child HWND otherwise covers them (main.rs:157-158)
  - portable: Behavioural contract: show/hide must be immediate and ordered with respect to the UI paint; hiding must not tear down the surface (state and decoder keep running). macOS: NSView isHidden; Linux: unmap/map or a subsurface toggle. This is a sync Tauri command, so it runs on Tauri's command dispatch path, not the tokio runtime.
- **tauri::WebviewWindow::hwnd() (Windows HWND handle, hwnd.0 as isize)** @ native/app/src/main.rs:393-394
  - purpose: raw window handle passed to create_video_window
  - portable: macOS needs the NSWindow/NSView (raw-window-handle style), Linux needs the X11 window id or Wayland surface. The Tauri 2 raw handle path is cross-platform; only the hwnd() accessor is Windows-only.
- **engine::autostart::set_enabled(bool) -> Result<String>-style error, and reconcile(bool) - Windows Run-key registration with an absolute exe path plus --autostart** @ native/app/src/main.rs:246-252 (set_enabled, error returned to UI and logged with tracing::warn) and main.rs:313-314 (reconcile at startup, before the window is created)
  - purpose: start with Windows; keeps the stored Run-key path current after updates (comment at 309-312)
  - portable: macOS: LaunchAgent plist in ~/Library/LaunchAgents (or SMAppService on macOS 13+, which is a registration API, not a plist). Linux: XDG autostart .desktop in ~/.config/autostart or a systemd user unit. Contract: reconcile must rewrite the stored path when the executable moves, and set_enabled must surface failures to the UI. Currently the non-Windows build accepts the setting and persists it with no effect (main.rs:246 cfg(windows) only).
- **--autostart command-line flag (engine::autostart::AUTOSTART_FLAG = "--autostart", autostart.rs:27) checked in setup; on match the main window is minimised** @ native/app/src/main.rs:399-401 (inside the cfg(windows) block at 391-402)
  - purpose: logon start comes up minimised (main.rs:396-398)
  - portable: The flag check and minimise run only on Windows, so non-Windows autostart would open a visible window. The replacement must honour the same flag on macOS and Linux if autostart is ported.
- **Windows application manifest sharectrl.manifest: PerMonitorV2 DPI awareness (dpiAwareness and dpiAware/pm), requestedExecutionLevel asInvoker with uiAccess=false, supportedOS Windows 10/11 GUID** @ native/app/sharectrl.manifest:1-25; NOT referenced by build.rs (native/app/build.rs:1-3 only calls tauri_build::build()) or any Cargo/tauri config, so it is not embedded by this build
  - purpose: per-monitor DPI so overlays and remote-cursor math line up with physical-pixel capture (manifest comment at lines 2-5)
  - portable: macOS: Retina/backing-scale-factor handling is needed for cursor and capture coordinates (points vs pixels). Linux: X11 or Wayland fractional scaling. No manifest equivalent exists; the contract is that coordinate transforms between capture pixels, window points and remote input use the same scale factor.
- **tauri.conf.json bundle: targets ["nsis"], icon ["icons/icon.ico"], windows.nsis.installMode perMachine (tauri.conf.json:26-35)** @ native/app/tauri.conf.json:26-35
  - purpose: Windows installer only
  - portable: macOS needs an .icns icon and a .app/.dmg target, and Linux needs .png/.deb/.AppImage/.rpm targets. icons/icon.png exists (native/app/icons) but is not referenced by bundle.icon. perMachine is Windows-only.
- **Windows Win32 DLL/OS assumptions are otherwise absent from this crate; input injection is handled in other crates via a Windows SYSTEM service (described in the manifest comment, sharectrl.manifest:4-5, and main.rs:5-6 'Plan 04 §8b')** @ outside this unit (engine crate). Only the mention is here.
  - purpose: privileged injection
  - portable: This crate has no direct equivalent to a SYSTEM service on macOS or Linux. The replacement must be defined in the input/elevation unit.

## portable_parts
- native/app/src/main.rs:28-58 AppDecider (oneshot map keyed by `from`, 30 s auto-deny, resolve removes entry), OS-agnostic
- native/app/src/main.rs:61-64 AppState
- native/app/src/main.rs:71-91 get_config JSON shape and mode mapping, OS-agnostic
- native/app/src/main.rs:93-155 clear_recents, get_server_status, get_role, connect_to, submit_password, set_permission, end_session, request_refresh (thin Engine delegation)
- native/app/src/main.rs:167-180 approve (async; decision string mapping)
- native/app/src/main.rs:182-254 save_settings patch parsing (except the cfg(windows) autostart block at 246-252)
- native/app/src/main.rs:266-292 init_logging (aside from the path source from config_path)
- native/app/src/main.rs:294-308 runtime creation, tauri::async_runtime::set, runtime.enter(), Engine::start
- native/app/src/main.rs:321-336 Tauri builder, manage and invoke_handler registration
- native/app/src/main.rs:341-379 UiEvent -> Tauri event name/payload mapping, OS-agnostic
- native/app/src/main.rs:383-387 signaling-event loop
- native/app/tauri.conf.json:9-25 window and security config; native/app/capabilities/default.json:1-11 core permissions

## abstraction_points
- config root directory: a fn returning PathBuf (today config_path() at main.rs:256-262, reading APPDATA). Replace per OS for macOS (Application Support) and Linux (XDG).
- native video surface: create(parent_window_handle: isize) and set_visible(bool) (today engine::pipeline::create_video_window(isize) at main.rs:394 and engine::pipeline::set_video_visible(bool) at main.rs:162). A trait or per-OS module needs create, show/hide and resize/position from the webview layout. The non-Windows implementation must exist, not a no-op, if video is to work.
- raw window handle accessor: win.hwnd() (main.rs:393) becomes a platform-generic raw handle (Tauri raw-window-handle) per OS.
- startup registration: autostart::set_enabled(bool) and reconcile(bool) (main.rs:246-252, 313-314) become a per-OS trait; the AUTOSTART_FLAG minimise path (main.rs:399-401) must be shared.
- logging sink: init_logging (main.rs:266) already falls back to stdout, so only its path source needs abstracting (same as config root).
- console subsystem attribute (main.rs:16) is Windows-only and can stay cfg-gated.
- Tauri bundle/targets (tauri.conf.json:26-35) and the manifest (sharectrl.manifest) are per-OS packaging inputs, not Rust code abstractions.

## cross_module_contracts
- Engine::start(config_path: PathBuf, decider: Arc<dyn Decider>) -> (Arc<Engine>, ui_rx, sig_rx) (main.rs:306-307). Channel element types are defined in the engine crate, not read in this unit. Engine::start must run inside an entered tokio runtime (main.rs:297-303).
- engine::handshake::{Decider, ApprovalDecision} (main.rs:22): Decider::decide<'a>(&'a self, from: &'a str) -> Pin<Box<dyn Future<Output=ApprovalDecision>+Send+'a>>. ApprovalDecision has variants Allow(Permission) and Deny (main.rs:45-46, 174-176).
- engine::UiEvent variants consumed in main.rs:343-376: ServerStatus(String-like), ApprovalRequest{from}, PasswordRequired{from}, RoleChanged(Role), Toast(message), LinkTrouble{secs_left}, LinkRestored. Each is emitted as a Tauri event named server-status, approval-request, password-required, role-changed, toast, link-trouble, link-restored (main.rs:344-375). The UI relies on these names and payload keys (status, from, role, peer, permission, message, secsLeft).
- engine::Role variants Idle, Host{peer, permission}, Viewer{peer, permission} (main.rs:113-115, 355-369). Permission has as_str() and variants Control and View (main.rs:79, 139-141).
- Engine methods called from this crate: config() -> config with fields uuid, server_url, mode, password_hash (is_some), password_permission, share_audio, share_display_id (Option<String>), recent_ids, capture_shortcuts, auto_login, clipboard_sync, paste_dropped_files, start_with_windows, saved_logins (len, clear) (main.rs:73-89, 225-240); clear_recents() -> Vec<String>; server_connected() -> bool; role(); connect_to(String); submit_password(host, password, remember: bool); set_permission(Permission); end_session(); request_refresh(); update_config(FnOnce(&mut Config)); set_password(Option<&str>) on the config (main.rs:196-199); handle_signaling(event) async (main.rs:385).
- protocol::Mode (Password, else Approve) and protocol::Permission (main.rs:24, 77, 138-141, 188-192).
- Threading: Tauri sync commands run on Tauri's dispatch path. `approve` is async and awaits a tokio::sync::Mutex (main.rs:167-180). The decider's pending map uses tokio Mutex because the lock is held across await points (main.rs:32, 42, 54). Two long-lived tokio tasks are spawned via tauri::async_runtime::spawn: the UI event bridge (main.rs:341) and the signaling loop (main.rs:383). The runtime is owned by main (main.rs:301) and shared with Tauri (302), and its enter guard lives until main returns (303).
- Ownership: main holds an Engine clone (main.rs:317, 382). Tauri holds AppState (Arc<Engine>, Arc<AppDecider>) via manage (main.rs:322). The ui_rx and sig_rx receivers are moved into tasks (main.rs:341, 383).
- Engine::autostart::reconcile and set_enabled and AUTOSTART_FLAG (autostart.rs:27 defines the flag, which is a pub const) are called from main.rs only on Windows.
- Engine::pipeline::create_video_window(isize) and set_video_visible(bool) are Windows-only public functions (pipeline.rs:50, 1009). pipeline.rs also calls set_video_visible internally at lines 1100, 1226, 1240, so the surface lifecycle is partly owned by the engine.

## gotchas
- save_settings ordering bug vs comment: main.rs:243-245 claims the startup registration is applied AFTER persistence 'so the two can't disagree', but on a set_enabled failure (main.rs:248-251) the config has already been saved with the new start_with_windows value (main.rs:238-240 inside update_config). The UI gets Err while config and the Run key disagree. Any port must either roll back the config value or treat the Run key as the source of truth.
- Silent non-Windows autostart: on macOS/Linux, save_settings accepts startWithWindows, persists it, and returns Ok (main.rs:238-240, 246 cfg(windows)). The UI shows the setting as on but nothing starts at logon.
- Config and log location falls back to the temp directory when APPDATA is unset (main.rs:258-261), which is always the case on macOS/Linux. Config.json and app.log would then be written to temp, which is not durable. The comment at main.rs:257 documents only the %APPDATA% case.
- init_logging truncates app.log on every launch: File::create (main.rs:278) overwrites, so the previous session's log is lost. The writer is a std Mutex<File> without buffering (main.rs:282), so each log line hits disk.
- AppDecider timeout leak: decide inserts a oneshot sender (main.rs:42) and on the 30 s timeout (main.rs:44-47) does NOT remove the entry, so the stale sender stays in the map until the same `from` is decided again or resolved. A second request from the same `from` replaces the sender (HashMap::insert), which drops the old sender, and the old decide then resolves to Deny through Err (main.rs:46). Keys are peer strings, not request ids.
- approve with an unknown decision string (main.rs:176) or an unknown `from` (resolve, main.rs:54-55 does nothing) is a silent no-op or Deny. There is no error back to the UI.
- Event replay gap: the bridge (main.rs:341-379) emits once with no buffering. main.rs:98-100 documents that the one-shot server-status event can fire before the WebView subscribes, and provides get_server_status to pull it. No pull equivalent exists for approval-request, password-required, toast, link-trouble or link-restored, so those can be lost if emitted before the UI listens.
- Video surface is created only when the window handle is available (main.rs:392-395: if let Some(win) and if let Ok(hwnd)). If hwnd() fails, video is silently not created and the code does not log it.
- Minimise-on-autostart happens inside the cfg(windows) block (main.rs:391-402). If the window handle path is skipped, the minimise does not happen either.
- set_video_visible is sync and runs on Tauri's command path (main.rs:160-161), while the engine also toggles visibility internally (pipeline.rs:1100, 1226, 1240). The two callers must not fight over state. This was not verified in pipeline.rs in this unit.
- tauri.conf.json version "2.0.0" (tauri.conf.json:4) duplicates the workspace version (native/Cargo.toml:21 = 2.0.0) by hand and can drift. Cargo.toml uses version.workspace.
- CSP is strict: default-src 'self'; script-src 'self' (tauri.conf.json:23). Any inline script in ui/ or any CDN script would be blocked. style-src allows 'unsafe-inline'. withGlobalTauri true (tauri.conf.json:10) means window.__TAURI__ is exposed.
- Capability surface is minimal: capabilities/default.json grants only core:default, core:event:default and core:window:default for window 'main' (default.json:5-10). Clipboard, dialog, file system or shell plugins are not granted, so any such feature must go through engine commands or a new permission, and that changes on each platform.
- Autostart flag handling: the Run key stores an absolute exe path plus --autostart (autostart.rs:33). main.rs:309-314 says a stale path fails silently at logon, which is why reconcile runs at every start.
- Hard-coded 30 s approval timeout (main.rs:44) is cited as contract §3.3 and is not configurable.
- The manifest's asInvoker/uiAccess=false and PerMonitorV2 settings are not embedded by this build (no reference in build.rs or any config). If the Windows binary currently gets DPI behaviour from Tauri defaults, the manifest's intent is not actually applied. This was not verified against a built binary.
- Windows-only type leakage: `hwnd.0 as isize` (main.rs:394) casts a HWND to isize, so the create_video_window signature is effectively a raw pointer-sized handle. A port must keep the handle type per OS and not reuse the isize name for other handle kinds without renaming.

## tests
- No tests in native/app. main.rs has no #[cfg(test)] module and native/app/Cargo.toml defines no test targets or dev-dependencies. build.rs is only tauri_build::build(). Nothing in this unit is directly unit-tested.
- Portable: none exist here. Pure-logic candidates to cover on any OS: AppDecider decide/resolve/timeout (main.rs:28-58), get_config JSON shape (main.rs:71-91), save_settings patch parsing (main.rs:182-254 minus the autostart call), approve decision mapping (main.rs:167-180), config_path resolution (main.rs:256-262), and the UiEvent->event-name mapping (main.rs:341-379).
- Windows-only: the autostart unit test in engine/src/autostart.rs:178-182 exists but is in the engine crate, not this one. The video window path (main.rs:391-395) and set_video_visible (main.rs:160-164) have no tests; they can only be exercised by running the app on Windows.
