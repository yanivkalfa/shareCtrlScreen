//! The TabManager (plans/08 §3.4, 08a §4, §5.1, §6.1–§6.7): the one authority
//! for the tab strip. The web UI renders what it is told (a full
//! `tabs-changed` snapshot after every change) and calls the `tab_*`
//! commands; it holds no tab truth.
//!
//! * **Tabs are remotes.** A tab is identified by its remote ID, and its name
//!   is that remote's name. The open tabs, their order and the active one live
//!   in the active workspace (`Workspace::{tabs, active_tab}`), so they follow
//!   the account and survive a restart; what is live — the session, its phase,
//!   its surface — lives here and starts empty on every launch (nothing
//!   auto-connects, 08a §6.8).
//! * **One session per tab, the newest wins** (I4): a session's events apply to
//!   its tab only while it is that tab's current session, so late events from
//!   an ended session can never touch its successor.
//! * **Surfaces** are made for a connection before it is asked for, kept by
//!   session, and destroyed on the UI thread once that session is released —
//!   never while its threads may still draw into it (08a §6.5).
//! * **Locks.** `inner` is taken around the engine's `connect` (which emits the
//!   session's first event) and around every event, so the session id is
//!   recorded before any of its events is applied. Nothing waits on the UI
//!   thread, sends an event or awaits while holding it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use engine::{
    ConnectError, Engine, PwReason, Role, SessionEvent, SessionId, SessionPhase, SurfaceTag,
    TabStep, WindowId,
};
use platform_api::{VideoSurface, WindowActivity};
use protocol::Permission;
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

use crate::menu::update_monitor_menu;
use crate::surfaces::SurfaceFactory;
use crate::windows::{Place, Who, WindowManager, WindowReport};
use engine::WorkspaceSel;
use protocol::sync::WinState;

/// Connected tabs past which opening another asks first (D2: no cap, a
/// confirm).
pub(crate) const TAB_WARN_AT: usize = 8;

/// The gap between two starts of "Connect all" (08a §6.7): a burst of
/// simultaneous ICE gathers and host prompts helps nobody.
const CONNECT_ALL_STAGGER: Duration = Duration::from_millis(500);

/// How long a reconnect waits for the host's previous session to finish
/// tearing down (its threads joined; normally well under a second), and how
/// often it looks.
const ENDING_WAIT: Duration = Duration::from_secs(5);
const ENDING_POLL: Duration = Duration::from_millis(100);

/// How one connection attempt went.
enum Attempt {
    /// The host's previous session is still ending: try again shortly.
    Retry,
    Connected,
    /// Refused, failed or called off; the tab shows why.
    Stopped,
}

/// What a tab is doing (08a §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Phase {
    #[default]
    Disconnected,
    Connecting,
    Password(PwReason),
    Live,
}

/// A host's monitors as the native Monitor menu shows them.
#[derive(Clone, PartialEq)]
struct MonitorMenu {
    list: Vec<protocol::RemoteDisplay>,
    current: String,
    can_switch: bool,
    popouts: Vec<protocol::PopoutStream>,
}

/// The live half of one tab.
#[derive(Default)]
struct TabLive {
    /// Its current session; `None` while disconnected.
    session: Option<SessionId>,
    phase: Phase,
    permission: Option<Permission>,
    /// Seconds left of the link-trouble grace period, while it runs.
    trouble: Option<u32>,
    /// What the host says its screen is (plans/10 U1): a `host-screen`
    /// payload, `None` until the host says (an older host never does: its
    /// screen is taken as normal).
    host_screen: Option<Value>,
    /// Why its last session ended, in a sentence (cleared on reconnect).
    last_end: Option<String>,
    /// The host's monitors, for the session bar (a `tab-displays` payload)
    /// and the native Monitor menu.
    displays: Option<Value>,
    menu: Option<MonitorMenu>,
}

/// A move of a tab's live session between windows, until the engine says
/// which surface it let go of (plans/09 W2/W3, 09a §5.1, §5.2).
struct Move {
    remote: String,
    /// The surface it is moving to: released means the move failed.
    target: SurfaceTag,
    kind: MoveKind,
}

#[derive(Clone, Copy)]
enum MoveKind {
    /// Into its new window.
    Detach(WindowId),
    /// Back into the main window, out of this one (closed once the move
    /// went through).
    Dock(WindowId),
}

/// A surface and the window it is in.
type Placed = (Arc<dyn VideoSurface>, WindowId);

#[derive(Default)]
struct Inner {
    /// By remote ID. Only tabs that were ever live in this run; the strip
    /// itself is the workspace's.
    live: HashMap<String, TabLive>,
    /// Every surface a session may still draw into, until the engine lets
    /// go of it (`SurfaceReleased`, `Released`): its current one and the
    /// one a move is taking it to — each with the window it is in, so a
    /// window is never closed while a session can still draw in it (J1).
    surfaces: HashMap<SessionId, Vec<Placed>>,
    /// Remotes whose connection is being set up (its surface being made).
    starting: HashSet<String>,
    /// Of those, the ones the user disconnected or closed meanwhile: the
    /// setup stops before asking the host.
    cancelled: HashSet<String>,
    /// The tab on screen, as this computer's user last chose it (`Some(None)`:
    /// the connect page); `None` until the first choice, when the
    /// workspace's `active_tab` is shown. Kept apart from the workspace's
    /// copy because a sync may close the tab on screen elsewhere while it is
    /// connected here: it stays on screen until it disconnects (08a §6.12).
    shown: Option<Option<String>>,
    /// The workspace `shown` was chosen in: another account's choice does
    /// not carry over (each workspace has its own `active_tab`).
    shown_epoch: u64,
    /// The Monitor menu as last applied (`Some(None)`: emptied); `None`:
    /// unknown, so the next pass applies it.
    menu_applied: Option<Option<MonitorMenu>>,
    /// Tabs in a window of their own, by remote (plans/09). Seeded from the
    /// workspace only at launch and sign-in (09a J6): a tab detached or docked
    /// on another computer meanwhile stays where it is here.
    detached: HashMap<String, WindowId>,
    /// The workspace epoch `detached` was seeded for.
    seeded: Option<u64>,
    /// A workspace with tabs was seeded in this run (the first is the one a
    /// login launch opens minimised).
    seeded_any: bool,
    /// Tabs whose window is being made by Detach.
    detaching: HashSet<String>,
    /// Tabs whose Dock is being set up.
    docking: HashSet<String>,
    /// Detached windows closed while a connection was being set up in them:
    /// they go once that setup has let go of its surface.
    close_after_start: HashMap<String, WindowId>,
    /// Moves under way, by session.
    moves: HashMap<SessionId, Move>,
    /// Windows that close once a session is released (closed with X while
    /// connected, or left behind by an account switch).
    close_after: HashMap<SessionId, Vec<WindowId>>,
    /// The OS's last word on each window still open.
    activity: HashMap<Who, WindowActivity>,
    /// The window the OS says is active now (the last one reported active,
    /// until it reports otherwise or goes).
    active: Option<Who>,
    /// The window the user was in last (sticky focus for the clipboard while
    /// another app is in front, 09 §7 blocker 5); the main window until the
    /// OS says otherwise.
    last_used: Option<Who>,
    /// The last detached window's number (they are never reused in a run).
    windows_made: u32,
}

/// The result of a command that may start connections.
pub(crate) enum Started {
    Done,
    /// It would pass [`TAB_WARN_AT`] connected tabs: nothing was done; the UI
    /// asks and repeats the command confirmed.
    NeedsConfirm {
        would_be: usize,
    },
}

impl Started {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Started::Done => json!({ "done": true }),
            Started::NeedsConfirm { would_be } => json!({ "needsConfirm": would_be }),
        }
    }
}

pub(crate) struct TabManager {
    app: AppHandle,
    engine: Arc<Engine>,
    surfaces: SurfaceFactory,
    windows: Arc<WindowManager>,
    inner: Mutex<Inner>,
    /// The app is exiting: no connection starts any more (Connect all
    /// included), so the sessions being ended stay ended (I9).
    closing: AtomicBool,
    /// Bumped by [`stop_starting`](Self::stop_starting): a connection being
    /// set up, and Connect all, stop when it changed since they began.
    stops: AtomicU64,
}

impl TabManager {
    pub(crate) fn new(
        app: AppHandle,
        engine: Arc<Engine>,
        windows: Arc<WindowManager>,
    ) -> Arc<Self> {
        Arc::new(TabManager {
            surfaces: SurfaceFactory::new(app.clone(), engine.clone()),
            windows,
            app,
            engine,
            inner: Mutex::default(),
            closing: AtomicBool::new(false),
            stops: AtomicU64::new(0),
        })
    }

    /// Stop the connections being set up and a running Connect all (the
    /// window went to the tray: nothing may go live behind it). Later
    /// commands connect as usual.
    /// (Only the Windows service host hides to a tray.)
    #[cfg(windows)]
    pub(crate) fn stop_starting(&self) {
        self.stops.fetch_add(1, Ordering::SeqCst);
    }

    /// The app is about to exit: start nothing new. Call before ending the
    /// sessions.
    pub(crate) fn begin_exit(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // The data stays consistent across a panic elsewhere (plain maps).
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- commands ------------------------------------------------------------

    /// Open the tab for `remote` (or show the open one) and connect it unless
    /// it is connected already (08a §6.1).
    pub(crate) async fn open(
        self: &Arc<Self>,
        remote: &str,
        confirmed: bool,
    ) -> Result<Started, String> {
        let remote = remote.trim().to_lowercase();
        if !protocol::is_valid_uuid_v4(&remote) {
            return Err(ConnectError::InvalidId.to_string());
        }
        if remote == self.engine.config().uuid {
            return Err(ConnectError::SelfId.to_string());
        }
        // In a window of its own: that window comes forward (the strip has
        // no place for it), and it connects there if it is not connected.
        let window = self.lock().detached.get(&remote).copied();
        if let Some(w) = window {
            self.windows.raise(w);
            if self.is_connected(&remote) {
                self.emit_snapshot();
                return Ok(Started::Done);
            }
            return self.connect(&remote, confirmed).await;
        }
        let opened = self
            .engine
            .with_workspace(|w, s| {
                let now = s.now();
                let key = w.open_tab(&remote, now);
                if key.is_some() {
                    w.active_tab = key.clone();
                }
                key
            })
            .flatten();
        if opened.is_none() {
            return Err(ConnectError::SignedOut.to_string());
        }
        self.lock().shown = Some(opened);
        self.apply_active();
        if self.is_connected(&remote) {
            self.emit_snapshot();
            return Ok(Started::Done);
        }
        self.connect(&remote, confirmed).await
    }

    /// Connect the open tab for `remote` (08a §6.2). A refusal (the host is
    /// offline, this computer is being viewed, ...) is the tab's to show
    /// (`lastEnd`); only a command that cannot apply is an error.
    pub(crate) async fn connect(
        self: &Arc<Self>,
        remote: &str,
        confirmed: bool,
    ) -> Result<Started, String> {
        let remote = remote.trim().to_lowercase();
        if self.closing.load(Ordering::SeqCst) {
            return Err("The app is closing".into());
        }
        if self.engine.workspace().is_none() {
            return Err(ConnectError::SignedOut.to_string());
        }
        if !self.is_open(&remote) {
            return Err("That tab is not open".into());
        }
        // The workspace it is connected in: a switch while the surface is
        // being made stops it (it would start in the next one, unreachable).
        let epoch = self.engine.workspace_epoch();
        let stops = self.stops.load(Ordering::SeqCst);
        // The window its video goes in. Fixed while it starts: Detach and
        // Dock wait for a tab that is starting.
        let window = {
            let mut inner = self.lock();
            if inner.starting.contains(&remote) || connected(&inner, &remote) {
                return Ok(Started::Done);
            }
            if inner.detaching.contains(&remote) {
                return Err("That tab is moving: try again in a moment".into());
            }
            let would_be = connected_count(&inner) + inner.starting.len() + 1;
            if would_be > TAB_WARN_AT && !confirmed {
                return Ok(Started::NeedsConfirm { would_be });
            }
            inner.starting.insert(remote.clone());
            let tab = inner.live.entry(remote.clone()).or_default();
            tab.phase = Phase::Connecting;
            tab.last_end = None;
            inner
                .detached
                .get(&remote)
                .copied()
                .unwrap_or(WindowId::MAIN)
        };
        self.emit_snapshot();
        let made = self.surfaces.create(window).await;
        // A session that just ended with this host may still be being torn
        // down (its threads joined): a quick reconnect waits for that.
        let deadline = Instant::now() + ENDING_WAIT;
        let connected = loop {
            match self.try_connect(
                &remote,
                window,
                &made,
                (epoch, stops),
                Instant::now() < deadline,
            ) {
                Attempt::Retry => tokio::time::sleep(ENDING_POLL).await,
                Attempt::Connected => break true,
                Attempt::Stopped => break false,
            }
        };
        // An unused surface goes, outside the lock (the UI thread does it) —
        // and then a window closed meanwhile, which waited for it (J1).
        if let (false, Ok(surface)) = (connected, made) {
            self.surfaces.destroy(surface);
        }
        let closed = self.lock().close_after_start.remove(&remote);
        if let Some(w) = closed {
            self.windows.close(w);
        }
        self.apply_active();
        Ok(Started::Done)
    }

    /// One attempt of [`connect`](Self::connect), under the lock the
    /// session's first event waits on, so its id is recorded first (I4).
    fn try_connect(
        &self,
        remote: &str,
        window: WindowId,
        made: &Result<Arc<dyn VideoSurface>, String>,
        (epoch, stops): (u64, u64),
        may_retry: bool,
    ) -> Attempt {
        let mut inner = self.lock();
        let stopped = inner.cancelled.remove(remote)
            || self.closing.load(Ordering::SeqCst)
            || self.engine.workspace_epoch() != epoch
            || self.stops.load(Ordering::SeqCst) != stops
            || !self.is_open(remote);
        let refused = if stopped {
            None
        } else {
            match made {
                Err(e) => Some(e.clone()),
                Ok(surface) => match self.engine.connect_in(remote, surface.clone(), window) {
                    Ok(id) => {
                        inner.starting.remove(remote);
                        inner.surfaces.insert(id, vec![(surface.clone(), window)]);
                        let tab = inner.live.entry(remote.to_string()).or_default();
                        tab.session = Some(id);
                        tab.phase = Phase::Connecting;
                        tab.trouble = None;
                        tab.host_screen = None;
                        tab.displays = None;
                        tab.menu = None;
                        return Attempt::Connected;
                    }
                    Err(ConnectError::Ending(_)) if may_retry => return Attempt::Retry,
                    Err(e) => Some(e.to_string()),
                },
            }
        };
        inner.starting.remove(remote);
        if let Some(tab) = inner.live.get_mut(remote) {
            tab.phase = Phase::Disconnected;
            tab.last_end = refused;
        }
        Attempt::Stopped
    }

    /// End `remote`'s session (or its request); the tab stays.
    pub(crate) fn disconnect(&self, remote: &str) {
        let remote = remote.trim().to_lowercase();
        let session = {
            let mut inner = self.lock();
            if inner.starting.contains(&remote) {
                inner.cancelled.insert(remote.clone());
            }
            inner.live.get(&remote).and_then(|t| t.session)
        };
        if let Some(id) = session {
            self.engine.disconnect(id);
        }
    }

    /// Disconnect and close `remote`'s tab; its neighbour becomes active.
    pub(crate) fn close(&self, remote: &str) {
        self.disconnect(remote);
        let remote = remote.trim().to_lowercase();
        self.engine.with_workspace(|w, s| {
            let now = s.now();
            w.close_tab(&remote, now);
        });
        let close_now = {
            let mut inner = self.lock();
            if inner.live.get(&remote).is_some_and(|t| t.session.is_none()) {
                inner.live.remove(&remote);
            }
            // The neighbour the workspace chose takes its place on screen.
            if inner.shown.as_ref() == Some(&Some(remote.clone())) {
                inner.shown = None;
            }
            // Its own window goes with it: now when nothing draws in it, else
            // once what does (the session being ended above, or a connection
            // being set up there) has let go (J1).
            match inner.detached.remove(&remote) {
                Some(w) if inner.starting.contains(&remote) => {
                    inner.close_after_start.insert(remote.clone(), w);
                    None
                }
                Some(w) => close_when_free(&mut inner, w),
                None => None,
            }
        };
        if let Some(w) = close_now {
            self.windows.close(w);
        }
        self.apply_active();
    }

    /// Show `remote`'s tab (08a §6.4); `None`: the connect page.
    pub(crate) fn activate(&self, remote: Option<&str>) -> Result<(), String> {
        let remote = remote.map(|r| r.trim().to_lowercase());
        // A tab in its own window: that window comes to the front.
        let window = remote
            .as_ref()
            .and_then(|r| self.lock().detached.get(r).copied());
        if let Some(w) = window {
            self.windows.raise(w);
            return Ok(());
        }
        if let Some(r) = &remote {
            let shown = self
                .engine
                .workspace()
                .is_some_and(|w| visible(&w, &self.lock()).contains(r));
            if !shown {
                return Err("That tab is not open".into());
            }
        }
        self.engine.set_active_tab(remote.clone());
        let focus = {
            let mut inner = self.lock();
            inner.shown = Some(remote.clone());
            // Choosing a live tab (again) gives its video the keyboard, as a
            // click in it does: that is how the keys leave the web page.
            remote
                .as_ref()
                .and_then(|r| inner.live.get(r))
                .filter(|t| t.phase == Phase::Live)
                .and_then(|t| t.session)
                .and_then(|id| {
                    inner
                        .surfaces
                        .get(&id)
                        .and_then(|l| l.last().map(|(s, _)| s.clone()))
                })
        };
        self.apply_active();
        if let Some(surface) = focus {
            surface.focus();
        }
        Ok(())
    }

    /// Show the next / previous tab by strip order, wrapping (D3).
    pub(crate) fn step(&self, step: TabStep) {
        let Some(ws) = self.engine.workspace() else {
            return;
        };
        let (open, active) = {
            let inner = self.lock();
            (visible(&ws, &inner), shown_tab(&ws, &inner))
        };
        if open.is_empty() {
            return;
        }
        let at = active
            .as_ref()
            .and_then(|a| open.iter().position(|k| k == a));
        let n = open.len();
        let next = match (at, step) {
            (Some(i), TabStep::Next) => (i + 1) % n,
            (Some(i), TabStep::Prev) => (i + n - 1) % n,
            (None, TabStep::Next) => 0,
            (None, TabStep::Prev) => n - 1,
        };
        let _ = self.activate(Some(&open[next]));
    }

    /// Rename the remote (and so its tab, everywhere it is shown).
    pub(crate) fn rename(&self, remote: &str, name: &str) {
        self.engine.set_remote_name(remote, name);
        self.emit_snapshot();
    }

    /// Move `remote`'s tab before `before`'s (`None`: to the end).
    pub(crate) fn move_tab(&self, remote: &str, before: Option<&str>) -> Result<(), String> {
        let moved = self
            .engine
            .with_workspace(|w, s| {
                let now = s.now();
                w.move_tab(remote, before, now)
            })
            .unwrap_or(false);
        self.emit_snapshot();
        if moved {
            Ok(())
        } else {
            Err("That tab is not open".into())
        }
    }

    /// Connect every disconnected open tab, one every
    /// [`CONNECT_ALL_STAGGER`] (08a §6.7). Stops if the workspace changes
    /// meanwhile (account switch, sign-out).
    pub(crate) fn connect_all(self: &Arc<Self>, confirmed: bool) -> Started {
        let Some(ws) = self.engine.workspace() else {
            return Started::Done;
        };
        let pending: Vec<String> = ws
            .open_tabs()
            .into_iter()
            .filter(|r| !self.is_connected(r))
            .collect();
        let would_be = connected_count(&self.lock()) + pending.len();
        if would_be > TAB_WARN_AT && !confirmed {
            return Started::NeedsConfirm { would_be };
        }
        let epoch = self.engine.workspace_epoch();
        let stops = self.stops.load(Ordering::SeqCst);
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            for (i, remote) in pending.into_iter().enumerate() {
                if i > 0 {
                    tokio::time::sleep(CONNECT_ALL_STAGGER).await;
                }
                if me.engine.workspace_epoch() != epoch || me.stops.load(Ordering::SeqCst) != stops
                {
                    return;
                }
                if let Err(e) = me.connect(&remote, true).await {
                    tracing::info!("connect all: {remote}: {e}");
                }
            }
        });
        Started::Done
    }

    /// Answer `remote`'s password prompt.
    pub(crate) fn password(&self, remote: &str, password: String, remember: bool) {
        let session = {
            let mut inner = self.lock();
            let remote = remote.trim().to_lowercase();
            let Some(tab) = inner.live.get_mut(&remote) else {
                return;
            };
            if !matches!(tab.phase, Phase::Password(_)) {
                return;
            }
            tab.phase = Phase::Connecting;
            tab.session
        };
        if let Some(id) = session {
            self.engine.submit_password(id, password, remember);
        }
        self.emit_snapshot();
    }

    /// Forget the saved password for `remote` in this workspace.
    pub(crate) fn forget_password(&self, remote: &str) {
        self.engine.forget_saved_login(remote);
        self.emit_snapshot();
    }

    // ---- detached windows (plans/09 W3, 09a §5.1–§5.6) ---------------------------

    /// Put `remote`'s docked tab in a window of its own; a live session moves
    /// along without reconnecting (09a §5.1).
    pub(crate) async fn detach(self: &Arc<Self>, remote: &str) -> Result<(), String> {
        let remote = remote.trim().to_lowercase();
        let ws = self
            .engine
            .workspace()
            .ok_or_else(|| ConnectError::SignedOut.to_string())?;
        let (id, session, place) = {
            let mut inner = self.lock();
            if !ws.tabs.get(&remote).is_some_and(|t| t.is_open()) {
                return Err("That tab is not open".into());
            }
            // A second click: the window exists or is on its way.
            if let Some(w) = inner.detached.get(&remote).copied() {
                drop(inner);
                self.windows.raise(w);
                return Ok(());
            }
            if inner.detaching.contains(&remote) {
                return Ok(());
            }
            if inner.starting.contains(&remote) || inner.docking.contains(&remote) {
                return Err("That tab is busy: try again in a moment".into());
            }
            let session = live_session(&inner, &remote);
            if session.is_some_and(|s| inner.moves.contains_key(&s)) {
                return Err("That tab is moving: try again in a moment".into());
            }
            inner.windows_made += 1;
            let id = WindowId(inner.windows_made);
            inner.detaching.insert(remote.clone());
            let tab = &ws.tabs[&remote];
            let place = Place {
                size: tab.window_size(),
                pos: tab.window_pos(),
                state: tab.window_state(),
            };
            (id, session, place)
        };
        let title = title_of(&ws, &remote);
        let opened = self.windows.open(id, &remote, &title, place).await;
        let surface = match (&opened, session) {
            (Ok(()), Some(_)) => Some(self.surfaces.create(id).await),
            _ => None,
        };
        let outcome: Result<(), (String, Option<Arc<dyn VideoSurface>>)> = {
            let mut inner = self.lock();
            inner.detaching.remove(&remote);
            let still = live_session(&inner, &remote) == session && self.is_open(&remote);
            match (opened, surface) {
                (Err(e), _) | (Ok(()), Some(Err(e))) => Err((e, None)),
                // Closed or reconnected meanwhile: nothing to do (the user's
                // later action stands), and nothing to say.
                (Ok(()), made) if !still => Err((String::new(), made.and_then(Result::ok))),
                (Ok(()), made) => {
                    let surface = made.and_then(Result::ok);
                    let moved = match (session, &surface) {
                        (Some(s), Some(surf)) => {
                            self.start_move(&mut inner, s, &remote, surf, MoveKind::Detach(id))
                        }
                        _ => Ok(()),
                    };
                    match moved {
                        Err(e) => Err((e, surface)),
                        Ok(()) => {
                            // The tab before it in this computer's strip
                            // (else after it) takes its place (09 §2) — the
                            // strip here, not the account's: J6.
                            let neighbour = neighbour_left_of(&visible(&ws, &inner), &remote);
                            let was_shown =
                                shown_tab(&ws, &inner).as_deref() == Some(remote.as_str());
                            inner.detached.insert(remote.clone(), id);
                            if was_shown {
                                inner.shown = Some(neighbour.clone());
                            }
                            // Split out, it is the window in use (09 §2).
                            inner.last_used = Some(Who::Window(id));
                            // Lock order: the TabManager's, then the engine's.
                            self.engine.with_workspace(|w, s| {
                                let now = s.now();
                                w.set_detached(&remote, true, now);
                                if w.active_tab.as_deref() == Some(remote.as_str()) {
                                    w.active_tab = neighbour.clone();
                                }
                            });
                            Ok(())
                        }
                    }
                }
            }
        };
        match outcome {
            Ok(()) => {
                self.apply_active();
                Ok(())
            }
            Err((e, surface)) => {
                // Nothing changed: what was made for it goes.
                if let Some(s) = surface {
                    self.surfaces.destroy(s);
                }
                self.windows.close(id);
                self.emit_snapshot();
                if e.is_empty() {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// Hand session `s` of `remote` to `surface` (09a §3.2). On success the
    /// surface counts as the session's and the move is remembered until the
    /// engine says which surface it let go of.
    fn start_move(
        &self,
        inner: &mut Inner,
        s: SessionId,
        remote: &str,
        surface: &Arc<dyn VideoSurface>,
        kind: MoveKind,
    ) -> Result<(), String> {
        let window = match kind {
            MoveKind::Detach(w) => w,
            MoveKind::Dock(_) => WindowId::MAIN,
        };
        self.engine
            .move_session(s, surface.clone(), window)
            .map_err(|e| e.to_string())?;
        inner
            .surfaces
            .entry(s)
            .or_default()
            .push((surface.clone(), window));
        inner.moves.insert(
            s,
            Move {
                remote: remote.to_string(),
                target: SurfaceTag::of(surface),
                kind,
            },
        );
        Ok(())
    }

    /// Put `remote`'s detached tab back at the end of the strip as the tab on
    /// screen; a live session moves along (09a §5.2). Its window closes once
    /// the engine has let go of it.
    pub(crate) async fn dock(self: &Arc<Self>, remote: &str) -> Result<(), String> {
        let remote = remote.trim().to_lowercase();
        let (window, session) = {
            let mut inner = self.lock();
            // A second click (already docking or docked): nothing to do.
            if inner.docking.contains(&remote) {
                return Ok(());
            }
            let Some(window) = inner.detached.get(&remote).copied() else {
                return Ok(());
            };
            let session = live_session(&inner, &remote);
            if session.is_some_and(|s| inner.moves.contains_key(&s))
                || inner.starting.contains(&remote)
            {
                return Err("That tab is busy: try again in a moment".into());
            }
            inner.docking.insert(remote.clone());
            (window, session)
        };
        self.windows.raise_main();
        let surface = match session {
            Some(_) => match self.surfaces.create(WindowId::MAIN).await {
                Ok(s) => Some(s),
                Err(e) => {
                    self.lock().docking.remove(&remote);
                    return Err(e);
                }
            },
            None => None,
        };
        let outcome = {
            let mut inner = self.lock();
            inner.docking.remove(&remote);
            let still = inner.detached.get(&remote) == Some(&window)
                && live_session(&inner, &remote) == session;
            let moved = if !still {
                // Closed or reconnected meanwhile: nothing to do.
                Err(String::new())
            } else {
                match (session, &surface) {
                    (Some(s), Some(surf)) => {
                        self.start_move(&mut inner, s, &remote, surf, MoveKind::Dock(window))
                    }
                    _ => Ok(()),
                }
            };
            if moved.is_ok() {
                inner.detached.remove(&remote);
                inner.shown = Some(Some(remote.clone()));
                inner.shown_epoch = self.engine.workspace_epoch();
                inner.last_used = Some(Who::Window(WindowId::MAIN));
                // At the end of the strip, the tab on screen (09 §2) —
                // whatever the account says about its window (J6).
                self.engine.with_workspace(|w, s| {
                    let now = s.now();
                    w.set_detached(&remote, false, now);
                    w.move_tab(&remote, None, now);
                    w.active_tab = Some(remote.clone());
                });
            }
            moved
        };
        match outcome {
            Ok(()) => {
                if session.is_none() {
                    // No live session: it goes once nothing draws in it (an
                    // ended one may not be released yet, J1).
                    let now = close_when_free(&mut self.lock(), window);
                    if let Some(w) = now {
                        self.windows.close(w);
                    }
                }
                self.apply_active();
                Ok(())
            }
            Err(e) => {
                if let Some(s) = surface {
                    self.surfaces.destroy(s);
                }
                if e.is_empty() {
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }

    /// The user closed detached window `window` (X): disconnect and close its
    /// tab, synced (09a §5.6). The window goes once the session let go of it.
    pub(crate) fn close_window(&self, window: WindowId) {
        let remote = self.remote_in(window).or_else(|| {
            // Its tab is being docked (the window stays until the move ends):
            // X still means "end it and close the tab"; the window then goes
            // with the move or the release (`finish_move`, `Released`).
            let inner = self.lock();
            inner
                .moves
                .values()
                .find(|m| matches!(m.kind, MoveKind::Dock(w) if w == window))
                .map(|m| m.remote.clone())
        });
        match remote {
            Some(remote) => self.close(&remote),
            // Already on its way out (closed meanwhile).
            None => tracing::debug!("window {} closing already", window.0),
        }
    }

    /// The native menu's tab commands act on the window in use (09 §7,
    /// answer 11): its detached tab, or the strip's tab on screen; `None`
    /// in a detached window means "that window's tab".
    pub(crate) fn window_in_use(&self) -> Option<(WindowId, String)> {
        let inner = self.lock();
        match inner.active.or(inner.last_used) {
            Some(Who::Window(w)) if w != WindowId::MAIN => inner
                .detached
                .iter()
                .find(|(_, x)| **x == w)
                .map(|(r, _)| (w, r.clone())),
            _ => None,
        }
    }

    /// The strip's tab on screen in the main window, if any.
    pub(crate) fn shown_remote(&self) -> Option<String> {
        let ws = self.engine.workspace()?;
        shown_tab(&ws, &self.lock())
    }

    /// The tab in detached window `window`.
    fn remote_in(&self, window: WindowId) -> Option<String> {
        let inner = self.lock();
        inner
            .detached
            .iter()
            .find(|(_, w)| **w == window)
            .map(|(r, _)| r.clone())
    }

    /// The OS's word on a window, or a detached window's settled placement
    /// (from the WindowManager).
    pub(crate) fn on_window_report(&self, report: WindowReport) {
        match report {
            WindowReport::Activity(who, a) => {
                {
                    let mut inner = self.lock();
                    if a.active {
                        inner.active = Some(who);
                        inner.last_used = Some(who);
                    } else if inner.active == Some(who) {
                        inner.active = None;
                    }
                    inner.activity.insert(who, a);
                }
                self.apply_active();
            }
            WindowReport::Gone(who) => {
                {
                    // Its watch went before the window, so its last word may
                    // be "active": forget it, or focus would stick to it.
                    let mut inner = self.lock();
                    inner.activity.remove(&who);
                    if inner.active == Some(who) {
                        inner.active = None;
                    }
                    if inner.last_used == Some(who) {
                        inner.last_used = None;
                    }
                }
                self.apply_active();
            }
            WindowReport::Placement {
                window,
                state,
                normal,
            } => {
                let Some(remote) = self.remote_in(window) else {
                    return;
                };
                self.engine.with_workspace(|w, s| {
                    let now = s.now();
                    w.set_window_state(&remote, state, now);
                    if let Some((size, pos)) = normal {
                        w.set_placement(&remote, size, pos, now);
                    }
                });
            }
        }
    }

    /// Close the previous workspace's windows (after whatever still draws in
    /// them is released: the switch ends their sessions, J1).
    fn close_all_windows(&self) {
        let close_now: Vec<WindowId> = {
            let mut inner = self.lock();
            let old: Vec<(String, WindowId)> = inner.detached.drain().collect();
            let mut now = Vec::new();
            for (remote, w) in old {
                if inner.starting.contains(&remote) {
                    inner.close_after_start.insert(remote, w);
                } else {
                    now.extend(close_when_free(&mut inner, w));
                }
            }
            now
        };
        for w in close_now {
            self.windows.close(w);
        }
    }

    /// Open the windows of the workspace's detached tabs, disconnected (09a
    /// §5.5), once per workspace: at launch, and at each sign-in or switch
    /// once that account's first sync has settled (so another computer's
    /// detached tabs show here at sign-in, J6). The previous workspace's
    /// windows close first.
    pub(crate) fn seed_windows(self: &Arc<Self>) {
        let ws = self.engine.workspace();
        let epoch = self.engine.workspace_epoch();
        if self.lock().seeded == Some(epoch) {
            return;
        }
        self.close_all_windows();
        let open = {
            let mut inner = self.lock();
            if inner.seeded == Some(epoch) {
                return;
            }
            // Started at sign-in to the OS, the main window comes up
            // minimised (main.rs); its tabs' windows do too — the first
            // workspace that has any (an account opens after its token).
            let quietly =
                !inner.seeded_any && std::env::args().any(|a| a == platform_api::AUTOSTART_FLAG);
            inner.seeded_any |= ws.is_some();
            inner.seeded = Some(epoch);
            let mut open = Vec::new();
            let detached = ws.as_ref().map(|w| (w, w.detached_tabs()));
            for (ws, remote) in detached
                .iter()
                .flat_map(|(w, tabs)| tabs.iter().map(move |r| (*w, r.clone())))
            {
                inner.windows_made += 1;
                let id = WindowId(inner.windows_made);
                inner.detached.insert(remote.clone(), id);
                let tab = &ws.tabs[&remote];
                let place = Place {
                    size: tab.window_size(),
                    pos: tab.window_pos(),
                    state: if quietly {
                        WinState::Minimized
                    } else {
                        tab.window_state()
                    },
                };
                open.push((remote.clone(), id, title_of(ws, &remote), place));
            }
            open
        };
        if open.is_empty() {
            self.apply_active();
            return;
        }
        let me = self.clone();
        tauri::async_runtime::spawn(async move {
            for (remote, id, title, place) in open {
                if me.engine.workspace_epoch() != epoch {
                    return;
                }
                let opened = me.windows.open(id, &remote, &title, place).await;
                let wanted = me.lock().detached.get(&remote) == Some(&id);
                match opened {
                    // Closed (or switched away) while it was being made: the
                    // close ran before the window existed, so it goes now.
                    Ok(()) if !wanted => me.windows.close(id),
                    Ok(()) => {}
                    Err(e) => {
                        // Shown in the strip this run instead; the account
                        // keeps it detached (none of this persists, 09a §5.7).
                        tracing::warn!("window for {remote}: {e}");
                        let mut inner = me.lock();
                        if inner.detached.get(&remote) == Some(&id) {
                            inner.detached.remove(&remote);
                        }
                    }
                }
            }
            me.apply_active();
        });
    }

    /// The viewer session of `remote`'s tab, while it has one (the session
    /// bar's commands name their tab, 09 §7).
    pub(crate) fn session_of(&self, remote: &str) -> Option<SessionId> {
        live_session(&self.lock(), &remote.trim().to_lowercase())
    }

    /// A tab chord pressed in `session`'s video (D3): it steps through the
    /// strip, unless that video is in a detached window, where it does
    /// nothing (09 Decision 2).
    pub(crate) fn shortcut(&self, step: TabStep, session: SessionId) {
        let in_window = {
            let inner = self.lock();
            inner
                .detached
                .keys()
                .any(|r| live_session(&inner, r) == Some(session))
        };
        if !in_window {
            self.step(step);
        }
    }

    // ---- engine events (from the bridge) --------------------------------------

    /// A viewer session's event (08a §5.1). `peer` is the remote: its tab.
    pub(crate) fn on_session(&self, id: SessionId, peer: &str, ev: SessionEvent) {
        let mut out: Vec<(&'static str, Value)> = Vec::new();
        let mut forget = false;
        let mut new_active: Option<Option<String>> = None;
        let mut released: Vec<Arc<dyn VideoSurface>> = Vec::new();
        // Windows that go once the surfaces above are destroyed (a docked
        // tab's old window, one closed while connected, a failed Detach's).
        let mut close_windows: Vec<WindowId> = Vec::new();
        {
            let mut inner = self.lock();
            match &ev {
                SessionEvent::Released => {
                    // Every surface the session held goes (09a J1).
                    released = inner
                        .surfaces
                        .remove(&id)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|(s, _)| s)
                        .collect();
                    let mut free = inner.close_after.remove(&id).unwrap_or_default();
                    if let Some(Move {
                        kind: MoveKind::Dock(w),
                        ..
                    }) = inner.moves.remove(&id)
                    {
                        // Docked while it ended: its old window goes.
                        free.push(w);
                    }
                    for w in free {
                        close_windows.extend(close_when_free(&mut inner, w));
                    }
                }
                SessionEvent::SurfaceReleased { surface } => {
                    if let Some(list) = inner.surfaces.get_mut(&id) {
                        if let Some(at) =
                            list.iter().position(|(s, _)| SurfaceTag::of(s) == *surface)
                        {
                            released.push(list.remove(at).0);
                        }
                    }
                    // Only a move makes the engine let go of one surface.
                    if let Some(m) = inner.moves.remove(&id) {
                        self.finish_move(&mut inner, id, m, *surface, &mut close_windows);
                    }
                }
                _ => {}
            }
            // A tab closed on another computer while connected here stays
            // until it disconnects (08a §6.12 step 5). Read under `inner`, so a
            // sync landing meanwhile cannot leave a stale entry (lock order:
            // the TabManager's, then the engine's).
            let ws = self.engine.workspace();
            let still_open = ws
                .as_ref()
                .is_some_and(|w| w.tabs.get(peer).is_some_and(|t| t.is_open()));
            // If it is on screen and goes, its neighbour takes its place, as
            // for a close here.
            let was_shown = ws
                .as_ref()
                .is_some_and(|w| shown_tab(w, &inner).as_deref() == Some(peer));
            let neighbour = ws
                .as_ref()
                .filter(|_| was_shown)
                .and_then(|w| neighbour_of(&visible(w, &inner), peer));
            let Some(tab) = inner.live.get_mut(peer).filter(|t| t.session == Some(id)) else {
                // An earlier session of this tab (or a closed tab): only its
                // surfaces and windows (above) still matter.
                drop(inner);
                self.release(released, close_windows);
                return;
            };
            match ev {
                SessionEvent::Phase(SessionPhase::Requesting) => tab.phase = Phase::Connecting,
                SessionEvent::Phase(SessionPhase::PasswordRequired { reason }) => {
                    tab.phase = Phase::Password(reason);
                }
                SessionEvent::Phase(SessionPhase::Negotiating { permission }) => {
                    tab.phase = Phase::Live;
                    tab.permission = Some(permission);
                }
                SessionEvent::Phase(SessionPhase::Ended(reason)) => {
                    tab.phase = Phase::Disconnected;
                    tab.session = None;
                    tab.trouble = None;
                    tab.host_screen = None;
                    tab.displays = None;
                    tab.menu = None;
                    tab.permission = None;
                    tab.last_end = reason.message();
                    forget = !still_open;
                }
                SessionEvent::LinkTrouble { secs_left } => tab.trouble = Some(secs_left),
                SessionEvent::LinkRestored => tab.trouble = None,
                SessionEvent::Displays {
                    list,
                    current,
                    can_switch,
                    popouts,
                } => {
                    let mut d =
                        crate::commands::displays_json(&list, &current, can_switch, &popouts);
                    d["remoteId"] = json!(peer);
                    tab.displays = Some(d.clone());
                    tab.menu = Some(MonitorMenu {
                        list,
                        current,
                        can_switch,
                        popouts,
                    });
                    out.push(("tab-displays", d));
                }
                SessionEvent::Toast(message) => {
                    out.push(("tab-toast", json!({ "remoteId": peer, "message": message })));
                }
                SessionEvent::HostScreen { screen, kind, why } => {
                    let payload = json!({
                        "remoteId": peer,
                        "screen": screen.as_str(),
                        "kind": kind,
                        "why": why,
                    });
                    tab.host_screen = Some(payload.clone());
                    out.push(("host-screen", payload));
                }
                // Handled above: they concern surfaces and windows.
                SessionEvent::Released | SessionEvent::SurfaceReleased { .. } => {}
            }
            if forget {
                inner.live.remove(peer);
                // Its window goes once the session is released.
                if let Some(w) = inner.detached.remove(peer) {
                    inner.close_after.entry(id).or_default().push(w);
                }
                if was_shown {
                    inner.shown = Some(neighbour.clone());
                    new_active = Some(neighbour);
                }
            }
        }
        if let Some(active) = new_active {
            self.engine.set_active_tab(active);
        }
        self.release(released, close_windows);
        for (name, payload) in out {
            let _ = self.app.emit(name, payload);
        }
        self.apply_active();
    }

    /// The workspace changed (a sync, an account switch): forget live state of
    /// tabs that are no longer open and redraw.
    pub(crate) fn on_workspace_changed(self: &Arc<Self>) {
        let open: HashSet<String> = self
            .engine
            .workspace()
            .map(|w| w.open_tabs().into_iter().collect())
            .unwrap_or_default();
        let epoch = self.engine.workspace_epoch();
        let (switched, gone) = {
            let mut inner = self.lock();
            let mut freed = Vec::new();
            inner
                .live
                .retain(|remote, t| open.contains(remote) || t.session.is_some());
            // A window whose tab was closed on another computer goes with it,
            // as the strip's tab does — at once when disconnected (a connected
            // one stays until it disconnects, 08a §6.12).
            let gone: Vec<String> = inner
                .detached
                .keys()
                .filter(|r| !open.contains(*r) && live_session(&inner, r).is_none())
                .cloned()
                .collect();
            for r in &gone {
                if let Some(w) = inner.detached.remove(r) {
                    freed.extend(close_when_free(&mut inner, w));
                }
            }
            let gone = freed;
            let switched = inner.shown_epoch != epoch;
            if switched {
                inner.shown_epoch = epoch;
                inner.shown = None;
            }
            (switched, gone)
        };
        for w in gone {
            self.windows.close(w);
        }
        // Only a real switch touches the windows: an ordinary sync never does
        // (J6), however the seeding stands.
        let account = matches!(self.engine.workspace_sel(), WorkspaceSel::Account(_));
        let seeded = self.lock().seeded == Some(epoch);
        match windows_on_change(switched, account, seeded) {
            WindowsOnChange::Keep => self.apply_active(),
            WindowsOnChange::CloseOld => {
                self.close_all_windows();
                self.apply_active();
            }
            WindowsOnChange::Seed => self.seed_windows(),
        }
    }

    // ---- helpers ---------------------------------------------------------------

    /// Session `id`'s move ended (09a §4.2): the engine let go of `released`.
    /// The target released means it failed: the tab goes back where it was
    /// (a Detach's new window goes; a Dock's window stays). Otherwise a
    /// Dock's old window goes. Windows to close are added to `close`.
    fn finish_move(
        &self,
        inner: &mut Inner,
        id: SessionId,
        m: Move,
        released: SurfaceTag,
        close: &mut Vec<WindowId>,
    ) {
        let failed = m.target == released;
        match (m.kind, failed) {
            (MoveKind::Detach(w), true) => {
                if inner.detached.get(&m.remote) == Some(&w) {
                    // Back where it was in the strip (its order key never
                    // changed), on screen as before the Detach.
                    inner.detached.remove(&m.remote);
                    inner.shown = Some(Some(m.remote.clone()));
                    inner.last_used = Some(Who::Window(WindowId::MAIN));
                    self.engine.with_workspace(|ws, s| {
                        let now = s.now();
                        ws.set_detached(&m.remote, false, now);
                        ws.active_tab = Some(m.remote.clone());
                    });
                }
                close.extend(close_when_free(inner, w));
            }
            (MoveKind::Dock(w), true) => {
                if self.is_open(&m.remote) {
                    // Back in its window, which stayed open.
                    inner.detached.insert(m.remote.clone(), w);
                    if inner.shown.as_ref() == Some(&Some(m.remote.clone())) {
                        inner.shown = None;
                    }
                    inner.last_used = Some(Who::Window(w));
                    self.engine.with_workspace(|ws, s| {
                        let now = s.now();
                        ws.set_detached(&m.remote, true, now);
                    });
                } else {
                    // Closed meanwhile: the session still draws there until
                    // it is released.
                    inner.close_after.entry(id).or_default().push(w);
                }
            }
            (MoveKind::Dock(w), false) => close.extend(close_when_free(inner, w)),
            (MoveKind::Detach(_), false) => {}
        }
    }

    /// Destroy `surfaces`, then close `windows` — in that order on the UI
    /// thread, so a window never goes before a surface inside it.
    fn release(&self, surfaces: Vec<Arc<dyn VideoSurface>>, windows: Vec<WindowId>) {
        for s in surfaces {
            self.surfaces.destroy(s);
        }
        for w in windows {
            self.windows.close(w);
        }
    }

    fn is_open(&self, remote: &str) -> bool {
        self.engine
            .workspace()
            .is_some_and(|w| w.tabs.get(remote).is_some_and(|t| t.is_open()))
    }

    fn is_connected(&self, remote: &str) -> bool {
        connected(&self.lock(), remote)
    }

    /// Put the workspace's active tab on screen (its session, if connected,
    /// else none: the UI shows that tab's card), then redraw.
    fn apply_active(&self) {
        let hosting = matches!(self.engine.role(), Role::Host { .. });
        let menu = {
            let mut inner = self.lock();
            let ws = self.engine.workspace();
            // A choice that left the strip is forgotten, so it cannot come back
            // by itself if that tab is reopened later.
            let stale = matches!(
                (&ws, &inner.shown),
                (Some(w), Some(Some(choice))) if !visible(w, &inner).contains(choice)
            );
            if stale {
                inner.shown = None;
            }
            let main_tab = ws.as_ref().and_then(|ws| shown_tab(ws, &inner));
            let (on_screen, focused) = screen(&inner, main_tab.as_deref());
            // Read and applied under `inner`, so a click and a session event
            // deciding at the same time cannot cross (the later decision is
            // the one on screen). `set_screen` never blocks.
            self.engine.set_screen(&on_screen, focused);
            let menu = focused
                .and_then(|f| inner.live.values().find(|t| t.session == Some(f)))
                .and_then(|t| t.menu.clone());
            // The Monitor menu belongs to the focused tab — or, hosting, to
            // this computer's own monitors, whose events keep it. Rebuilt only
            // when it changes: it is a round trip to the UI thread.
            if hosting {
                inner.menu_applied = None;
                None
            } else if inner.menu_applied.as_ref() == Some(&menu) {
                None
            } else {
                inner.menu_applied = Some(menu.clone());
                Some(menu)
            }
        };
        if let Some(menu) = menu {
            match &menu {
                Some(m) => {
                    update_monitor_menu(&self.app, &m.list, &m.current, m.can_switch, &m.popouts)
                }
                None => update_monitor_menu(&self.app, &[], "", false, &[]),
            }
        }
        self.emit_snapshot();
    }

    /// The whole strip, as the UI draws it (08a §4.2).
    pub(crate) fn snapshot(&self) -> Value {
        let ws = self.engine.workspace();
        let hosting = matches!(self.engine.role(), Role::Host { .. });
        let Some(ws) = ws else {
            return json!({ "tabs": [], "detached": [], "active": null, "connected": 0, "hosting": hosting,
                           "signedOut": true });
        };
        let inner = self.lock();
        let tab_json = |remote: String| {
            let live = inner.live.get(&remote);
            let starting = inner.starting.contains(&remote);
            let phase = live.map(|t| t.phase).unwrap_or_default();
            let (phase, reason) = match phase {
                Phase::Disconnected if starting => ("connecting", None),
                Phase::Disconnected => ("disconnected", None),
                Phase::Connecting => ("connecting", None),
                Phase::Password(PwReason::First) => ("password", Some("first")),
                Phase::Password(PwReason::SavedRejected) => ("password", Some("saved-rejected")),
                Phase::Live => ("live", None),
            };
            json!({
                "remoteId": remote,
                "name": ws.remote_name(&remote).unwrap_or(""),
                "phase": phase,
                "passwordReason": reason,
                "permission": live.and_then(|t| t.permission).map(|p| p.as_str()),
                "troubleSecs": live.and_then(|t| t.trouble),
                // `{ screen, kind, why }` from the host (plans/10 U1), or null.
                "hostScreen": live.and_then(|t| t.host_screen.clone()),
                "lastEnd": live.and_then(|t| t.last_end.clone()),
                "autoSignIn": ws.saved_logins.contains_key(&remote),
                "displays": live.and_then(|t| t.displays.clone()),
                // Closed on another computer, still connected here: it goes
                // when it disconnects, and cannot be moved meanwhile.
                "closedElsewhere": !ws.tabs.get(&remote).is_some_and(|t| t.is_open()),
                "window": inner.detached.get(&remote).map(|w| w.0),
            })
        };
        let tabs: Vec<Value> = visible(&ws, &inner).into_iter().map(tab_json).collect();
        // The tabs in windows of their own (plans/09): each window draws its
        // own from here, by remote.
        let mut detached: Vec<(&str, u32)> = inner
            .detached
            .iter()
            .map(|(r, w)| (r.as_str(), w.0))
            .collect();
        detached.sort_unstable_by_key(|(_, w)| *w);
        let detached: Vec<Value> = detached
            .into_iter()
            .map(|(r, _)| tab_json(r.to_string()))
            .collect();
        json!({
            "tabs": tabs,
            "detached": detached,
            "active": shown_tab(&ws, &inner),
            "connected": connected_count(&inner),
            "hosting": hosting,
            "signedOut": false,
        })
    }

    fn emit_snapshot(&self) {
        let _ = self.app.emit("tabs-changed", self.snapshot());
    }
}

/// What a workspace change does to the detached windows (J6).
#[derive(Debug, PartialEq, Eq)]
enum WindowsOnChange {
    /// An ordinary sync, or this workspace's windows are open already.
    Keep,
    /// Switched to an account: the old windows go now; the account's open
    /// once its first sync has settled (the AccountManager seeds them).
    CloseOld,
    /// Switched to the local workspace (or a locked one): seed it now.
    Seed,
}

fn windows_on_change(switched: bool, account: bool, seeded: bool) -> WindowsOnChange {
    match (switched, account, seeded) {
        // Only a real switch touches the windows, and never the ones already
        // seeded for this workspace (the switch's event can come after).
        (false, _, _) | (true, _, true) => WindowsOnChange::Keep,
        (true, true, false) => WindowsOnChange::CloseOld,
        (true, false, false) => WindowsOnChange::Seed,
    }
}

/// Close detached window `w` now if nothing can still draw in it: returns it.
/// Otherwise it closes once the session drawing there is released (J1: a
/// session that ended keeps its surfaces until `Released`).
fn close_when_free(inner: &mut Inner, w: WindowId) -> Option<WindowId> {
    let holder = inner
        .surfaces
        .iter()
        .find(|(_, list)| list.iter().any(|(_, x)| *x == w))
        .map(|(s, _)| *s);
    match holder {
        Some(s) => {
            inner.close_after.entry(s).or_default().push(w);
            None
        }
        None => Some(w),
    }
}

/// The tab before `remote` in `strip`, else the one after it — the tab that
/// takes a detached tab's place (09 §7).
fn neighbour_left_of(strip: &[String], remote: &str) -> Option<String> {
    let at = strip.iter().position(|r| r == remote)?;
    at.checked_sub(1)
        .and_then(|i| strip.get(i))
        .or_else(|| strip.get(at + 1))
        .cloned()
}

/// `remote`'s session while it has one that is not over.
fn live_session(inner: &Inner, remote: &str) -> Option<SessionId> {
    inner
        .live
        .get(remote)
        .filter(|t| t.phase != Phase::Disconnected)
        .and_then(|t| t.session)
}

/// A window's title for `remote`: its name, else its ID.
fn title_of(ws: &protocol::Workspace, remote: &str) -> String {
    ws.remote_name(remote)
        .filter(|n| !n.is_empty())
        .map_or_else(|| remote.to_string(), str::to_string)
}

/// Which sessions are on screen and which is focused (09a §4.1), from the
/// tab in the main window (`main_tab`), the detached windows and what the OS
/// said about each window.
///
/// * On screen: the main window's tab unless that window is minimised, and
///   each detached window's tab unless it is minimised (on Wayland nothing
///   says so, so it counts as on screen).
/// * Focused: the session of the window the OS says is active; while none of
///   ours is, the one used last (sticky, for the clipboard). A pop-out window
///   counts as its session. The main window until the OS says anything.
fn screen(inner: &Inner, main_tab: Option<&str>) -> (Vec<SessionId>, Option<SessionId>) {
    let minimized = |who: Who| inner.activity.get(&who).and_then(|a| a.minimized) == Some(true);
    let main_session = main_tab.and_then(|r| live_session(inner, r));
    let mut on_screen = Vec::new();
    if let Some(s) = main_session.filter(|_| !minimized(Who::Window(WindowId::MAIN))) {
        on_screen.push(s);
    }
    for (remote, w) in &inner.detached {
        if let Some(s) = live_session(inner, remote).filter(|_| !minimized(Who::Window(*w))) {
            on_screen.push(s);
        }
    }
    let who = inner
        .active
        .or(inner.last_used)
        .unwrap_or(Who::Window(WindowId::MAIN));
    let focused = match who {
        Who::Window(w) if w == WindowId::MAIN => main_session,
        Who::Window(w) => inner
            .detached
            .iter()
            .find(|(_, x)| **x == w)
            .and_then(|(r, _)| live_session(inner, r)),
        Who::Popout(s) => inner
            .live
            .values()
            .any(|t| t.session == Some(s) && t.phase != Phase::Disconnected)
            .then_some(s),
    };
    (on_screen, focused)
}

fn connected(inner: &Inner, remote: &str) -> bool {
    inner
        .live
        .get(remote)
        .is_some_and(|t| t.session.is_some() && t.phase != Phase::Disconnected)
}

fn connected_count(inner: &Inner) -> usize {
    inner
        .live
        .values()
        .filter(|t| t.session.is_some() && t.phase != Phase::Disconnected)
        .count()
}

/// The tabs in the strip, in order: the open ones, and those closed on
/// another computer while connected here (they stay until they disconnect,
/// 08a §6.12 step 5) — but not those in a window of their own here (the
/// detached set is this computer's, 09a J6).
fn visible(ws: &protocol::Workspace, inner: &Inner) -> Vec<String> {
    let mut tabs: Vec<(&str, &str)> = ws
        .tabs
        .iter()
        .filter(|(remote, t)| {
            (t.is_open() || connected(inner, remote)) && !inner.detached.contains_key(*remote)
        })
        .map(|(remote, t)| (t.order.as_str(), remote.as_str()))
        .collect();
    tabs.sort_unstable();
    tabs.into_iter()
        .map(|(_, remote)| remote.to_string())
        .collect()
}

/// The tab on screen: the user's last choice while it is still in the strip,
/// else the workspace's active tab.
fn shown_tab(ws: &protocol::Workspace, inner: &Inner) -> Option<String> {
    let strip = visible(ws, inner);
    match &inner.shown {
        Some(choice) if choice.as_ref().is_none_or(|r| strip.contains(r)) => choice.clone(),
        _ => ws.active_tab.clone().filter(|r| strip.contains(r)),
    }
}

/// The tab next to `remote` in `strip` — the right-hand one, else the
/// left-hand one — as a close picks it (`Workspace::close_tab`).
fn neighbour_of(strip: &[String], remote: &str) -> Option<String> {
    let at = strip.iter().position(|r| r == remote)?;
    strip
        .get(at + 1)
        .or_else(|| at.checked_sub(1).and_then(|i| strip.get(i)))
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "11111111-1111-4111-8111-111111111111";
    const B: &str = "22222222-2222-4222-8222-222222222222";
    const C: &str = "33333333-3333-4333-8333-333333333333";

    fn workspace() -> protocol::Workspace {
        let mut w = protocol::Workspace::default();
        for (i, t) in [A, B, C].iter().enumerate() {
            w.open_tab(t, 10 + i as u64);
        }
        w
    }

    fn live(inner: &mut Inner, remote: &str) {
        let tab = inner.live.entry(remote.to_string()).or_default();
        tab.session = Some(SessionId(7));
        tab.phase = Phase::Live;
    }

    /// 08a §6.12 step 5: a tab closed elsewhere stays while connected here.
    #[test]
    fn a_tab_closed_elsewhere_stays_while_connected() {
        let mut w = workspace();
        let mut inner = Inner::default();
        w.close_tab(B, 20);
        assert_eq!(visible(&w, &inner), [A, C], "disconnected: gone at once");
        live(&mut inner, B);
        assert_eq!(
            visible(&w, &inner),
            [A, B, C],
            "connected: kept, in its place"
        );
    }

    #[test]
    fn the_tab_on_screen_is_the_users_choice_while_it_is_in_the_strip() {
        let mut w = workspace();
        let mut inner = Inner::default();
        w.active_tab = Some(A.into());
        assert_eq!(shown_tab(&w, &inner).as_deref(), Some(A), "no choice yet");
        inner.shown = Some(Some(B.into()));
        assert_eq!(shown_tab(&w, &inner).as_deref(), Some(B));
        inner.shown = Some(None);
        assert_eq!(shown_tab(&w, &inner), None, "the connect page");
        // The chosen tab was closed elsewhere and is not connected: the
        // workspace's active tab (if still in the strip) is shown instead.
        inner.shown = Some(Some(B.into()));
        w.close_tab(B, 20);
        assert_eq!(shown_tab(&w, &inner).as_deref(), Some(A));
        w.active_tab = Some(B.into());
        assert_eq!(shown_tab(&w, &inner), None);
    }

    fn session(inner: &mut Inner, remote: &str, id: u64) {
        let tab = inner.live.entry(remote.to_string()).or_default();
        tab.session = Some(SessionId(id));
        tab.phase = Phase::Live;
    }

    /// As `on_window_report` records an activity report.
    fn report(inner: &mut Inner, who: Who, active: bool, minimized: Option<bool>) {
        if active {
            inner.active = Some(who);
            inner.last_used = Some(who);
        } else if inner.active == Some(who) {
            inner.active = None;
        }
        inner
            .activity
            .insert(who, WindowActivity { active, minimized });
    }

    /// plans/09 §2, 09a §4.1: the main window's tab and every detached
    /// window's are on screen unless their window is minimised; the active
    /// window's session is focused, and stays so while another app is in
    /// front.
    #[test]
    fn the_screen_follows_the_windows() {
        let mut inner = Inner::default();
        session(&mut inner, A, 1);
        session(&mut inner, B, 2);
        session(&mut inner, C, 3);
        inner.detached.insert(B.into(), WindowId(1));
        inner.detached.insert(C.into(), WindowId(2));
        let sorted = |(mut on, f): (Vec<SessionId>, Option<SessionId>)| {
            on.sort();
            (on, f)
        };
        let s = |n| SessionId(n);
        // Nothing reported yet: the main window is the one in use.
        assert_eq!(
            sorted(screen(&inner, Some(A))),
            (vec![s(1), s(2), s(3)], Some(s(1)))
        );
        // B's window active.
        report(&mut inner, Who::Window(WindowId::MAIN), false, Some(false));
        report(&mut inner, Who::Window(WindowId(1)), true, Some(false));
        assert_eq!(screen(&inner, Some(A)).1, Some(s(2)));
        // C minimised: off screen (paused).
        report(&mut inner, Who::Window(WindowId(2)), false, Some(true));
        assert_eq!(sorted(screen(&inner, Some(A))).0, vec![s(1), s(2)]);
        // Another app in front: B stays focused (sticky).
        report(&mut inner, Who::Window(WindowId(1)), false, Some(false));
        assert_eq!(screen(&inner, Some(A)).1, Some(s(2)));
        // The main window minimised: its tab is off screen.
        report(&mut inner, Who::Window(WindowId::MAIN), false, Some(true));
        assert_eq!(sorted(screen(&inner, Some(A))).0, vec![s(2)]);
        // Wayland never says minimised: on screen.
        report(&mut inner, Who::Window(WindowId(2)), false, None);
        assert_eq!(sorted(screen(&inner, Some(A))).0, vec![s(2), s(3)]);
        // A pop-out of A active: A is focused.
        report(&mut inner, Who::Popout(s(1)), true, Some(false));
        assert_eq!(screen(&inner, Some(A)).1, Some(s(1)));
        // The connect page in the main window, the main window active.
        report(&mut inner, Who::Popout(s(1)), false, Some(false));
        report(&mut inner, Who::Window(WindowId::MAIN), true, Some(false));
        assert_eq!(screen(&inner, None).1, None);
    }

    /// A window closed while active (its watch went first, so its last
    /// word was "active") must not keep the focus (W3 review).
    #[test]
    fn a_window_that_went_does_not_keep_the_focus() {
        let mut inner = Inner::default();
        session(&mut inner, A, 1);
        session(&mut inner, B, 2);
        inner.detached.insert(B.into(), WindowId(1));
        report(&mut inner, Who::Window(WindowId(1)), true, Some(false));
        assert_eq!(screen(&inner, Some(A)).1, Some(SessionId(2)));
        // Gone, as `WindowReport::Gone` handles it; then the main window.
        inner.detached.remove(B);
        inner.activity.remove(&Who::Window(WindowId(1)));
        inner.active = None;
        inner.last_used = None;
        assert_eq!(
            screen(&inner, Some(A)).1,
            Some(SessionId(1)),
            "back to the main window"
        );
        report(&mut inner, Who::Window(WindowId::MAIN), true, Some(false));
        assert_eq!(screen(&inner, Some(A)).1, Some(SessionId(1)));
    }

    /// J1: a window closes only when no session — live or ended but not yet
    /// released — can still draw in it.
    #[test]
    fn a_window_waits_for_the_session_drawing_in_it() {
        let mut inner = Inner::default();
        let w = WindowId(3);
        assert_eq!(close_when_free(&mut inner, w), Some(w), "nothing there");
        let surface: Arc<dyn VideoSurface> = platform_api::mock::MockSurface::new((1, 1));
        inner.surfaces.insert(SessionId(9), vec![(surface, w)]);
        assert_eq!(close_when_free(&mut inner, w), None);
        assert_eq!(inner.close_after[&SessionId(9)], vec![w]);
        assert_eq!(
            close_when_free(&mut inner, WindowId(4)),
            Some(WindowId(4)),
            "another window is free"
        );
    }

    #[test]
    fn a_detached_tab_is_replaced_by_the_one_before_it() {
        let strip: Vec<String> = [A, B, C].iter().map(|s| s.to_string()).collect();
        assert_eq!(neighbour_left_of(&strip, B).as_deref(), Some(A));
        assert_eq!(neighbour_left_of(&strip, A).as_deref(), Some(B));
        assert_eq!(neighbour_left_of(&strip[..1], A), None);
    }

    /// Final review B1: a sync never closes the windows; an account opens
    /// its own after its first sync; a late switch event keeps them.
    #[test]
    fn only_a_switch_touches_the_windows() {
        use WindowsOnChange::*;
        assert_eq!(windows_on_change(false, true, false), Keep, "a sync");
        assert_eq!(windows_on_change(false, true, true), Keep);
        assert_eq!(
            windows_on_change(true, true, false),
            CloseOld,
            "to an account"
        );
        assert_eq!(windows_on_change(true, true, true), Keep, "seeded already");
        assert_eq!(windows_on_change(true, false, false), Seed, "to local");
    }

    #[test]
    fn detached_tabs_leave_the_strip() {
        let w = workspace();
        let mut inner = Inner::default();
        inner.detached.insert(B.into(), WindowId(1));
        assert_eq!(visible(&w, &inner), [A, C]);
    }

    #[test]
    fn a_neighbour_is_picked_as_a_close_picks_it() {
        let strip: Vec<String> = [A, B, C].iter().map(|s| s.to_string()).collect();
        assert_eq!(neighbour_of(&strip, B).as_deref(), Some(C));
        assert_eq!(neighbour_of(&strip, C).as_deref(), Some(B));
        assert_eq!(neighbour_of(&strip[..1], A), None);
        assert_eq!(neighbour_of(&strip, "nope"), None);
    }
}
