//! `ScreenCapture` on Wayland: xdg-desktop-portal ScreenCast + PipeWire (plan task L1).
//!
//! The portal handshake ([`crate::portal_session`], shared with the L12 consent flow)
//! restores the stored token, so after the first consent no dialog appears. With a
//! combined (remote-control) token, or while remote control already holds it, the
//! capture joins the process's shared combined session ([`crate::portal_shared`]) instead
//! of starting its own: one dialog, one single-use token, and the session closes when
//! the last of capture and injector lets go. The portal
//! hands back a PipeWire node and a remote fd; [`stream`] runs that stream on its own
//! thread and copies every new buffer into a depth-1 newest-wins [`slot`], which
//! [`ScreenCapture::acquire`] waits on, so the host keeps the pull model it has on
//! Windows and macOS. The pointer comes from `spa_meta_cursor` (`cursor_mode =
//! Metadata`), reported out-of-band like DXGI's pointer shape; changed regions from
//! `spa_meta_video_damage` when the compositor attaches it.
//!
//! Recovery: a pause or size change keeps the session and only waits for the stream
//! to stream again. A stream-level failure (PipeWire restarted, core error, the node
//! gone without the portal closing the session) is rebuilt from the restore token on a
//! thread of its own, so a dialog never blocks the host media thread, with back-off
//! while the rebuild keeps failing. The desktop closing the session ("Stop sharing" in
//! its indicator), a declined dialog and an unanswered one are terminal: the host ends
//! the session, as AnyDesk does, rather than silently overriding the user.
//!
//! ## References
//! * ScreenCast portal (`SelectSources`, `Start`, `OpenPipeWireRemote`, restore
//!   tokens are single-use, an unrestorable token falls back to the dialog):
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.ScreenCast.html>
//! * Session `Closed` signal:
//!   <https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Session.html>
//! * PipeWire stream API (`pw_stream_connect` flags `AUTOCONNECT`, `MAP_BUFFERS`,
//!   `DONT_RECONNECT`; states; `param_changed`; `update_params`; `dequeue_buffer`):
//!   <https://docs.pipewire.org/group__pw__stream.html>
//! * PipeWire video capture tutorial (format negotiation, `Buffers`/`Meta` params):
//!   <https://docs.pipewire.org/page_tutorial5.html>
//! * DMA-BUF negotiation (a producer only uses DMA-BUF when the consumer lists
//!   modifiers): <https://docs.pipewire.org/page_dma_buf.html>
//! * SPA buffer and metadata layouts — `spa_chunk` (offset modulo maxsize, stride),
//!   `spa_meta_header` flags, `spa_meta_cursor` / `spa_meta_bitmap` (id 0 = no news,
//!   bitmap offset 0 = invisible), `spa_meta_region` damage arrays:
//!   spa/buffer/buffer.h and spa/buffer/meta.h in PipeWire
//!   (<https://gitlab.freedesktop.org/pipewire/pipewire/-/tree/master/spa/include/spa/buffer>)
//! * pipewire-rs 0.10: <https://docs.rs/pipewire/0.10.1/pipewire/>
//! * Reference consumer: OBS `plugins/linux-pipewire/pipewire.c` (cursor metadata,
//!   size-0 cursor-only buffers).
//! * plans/02-LINUX-FINDINGS.md §1; plans/05-TASKS.md L1.

mod damage;
mod params;
mod pixels;
mod slot;
mod stream;

use std::any::Any;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use platform_api::{CaptureError, CapturedFrame, CursorUpdate, Frame, Point, Rect, ScreenCapture};
use tokio::sync::oneshot;

use crate::portal_session::{self, Failure, PortalSession, Scope};
use crate::portal_shared::{self, SharedError, SharedSession};
use crate::portal_token::{self, StoredToken};

use slot::{Ended, Picture, Ready, Slot, Wait};
use stream::PipeWireStream;

/// How long the consent dialog may stay unanswered (it only appears when no restore
/// token works). The first open waits this long on the host media thread; the bound is
/// what lets a session nobody attends end with an error instead of hanging.
const DIALOG_TIMEOUT: Duration = Duration::from_secs(120);

/// Extra time for the portal calls around the dialog (each is a quick D-Bus round
/// trip) before the host thread stops waiting for the portal runtime.
const PORTAL_SLACK: Duration = Duration::from_secs(10);

/// How long a fresh stream may take to fix a format and start streaming.
const FORMAT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long `reinit` waits for a paused or resized stream to stream again before
/// rebuilding the whole session.
const RESUME_TIMEOUT: Duration = Duration::from_secs(2);

const NO_PORTAL: &str = "screen sharing on Wayland needs the desktop portal: install and \
    start xdg-desktop-portal with this desktop's backend (xdg-desktop-portal-gnome, -kde or \
    -wlr). WSLg and headless sessions have none";

/// Which session to open for the stored token. A token issued with remote control
/// (L12's combined consent) only restores in a combined RemoteDesktop session, and
/// writing a screen-only replacement over it would silently drop remote control, so
/// its scope is kept.
fn plan(stored: Option<&StoredToken>) -> (Scope, Option<&str>) {
    match stored {
        Some(t) if t.covers_input => (Scope::ScreenAndInput, Some(t.token.as_str())),
        Some(t) => (Scope::Screen, Some(t.token.as_str())),
        None => (Scope::Screen, None),
    }
}

/// Why opening a session failed, before mapping to the trait's error. The portal
/// errors are boxed because `ashpd::Error` is large (over 128 bytes) and would make
/// every `Result` of the open path that size.
#[derive(Debug)]
enum OpenError {
    Portal(Box<Failure>),
    /// The shared combined session (`portal_shared`) could not start.
    Shared(SharedError),
    /// `Start` succeeded but granted no stream.
    NoStream,
    /// `OpenPipeWireRemote` failed.
    Remote(Box<ashpd::Error>),
    /// The portal runtime is missing or did not answer at all.
    NoAnswer,
}

/// A message a user can act on for every way opening can fail.
fn open_error(e: &OpenError) -> CaptureError {
    match e {
        OpenError::Portal(f) => portal_error(f),
        OpenError::Shared(e) if e.dismissed => CaptureError::PermissionDenied,
        OpenError::Shared(e) => CaptureError::Backend(e.message.clone()),
        OpenError::NoStream => CaptureError::Backend(
            "the portal granted no screen; choose a monitor in the screen-sharing dialog".into(),
        ),
        OpenError::Remote(e) => CaptureError::Backend(format!(
            "the portal could not open its PipeWire connection ({e}); is PipeWire running? \
             (`systemctl --user status pipewire`)"
        )),
        OpenError::NoAnswer => CaptureError::Backend(format!(
            "the desktop portal did not answer within {} s; {NO_PORTAL}",
            (DIALOG_TIMEOUT + PORTAL_SLACK).as_secs()
        )),
    }
}

fn portal_error(f: &Failure) -> CaptureError {
    use ashpd::desktop::ResponseError;
    match f {
        f if f.is_cancel() => CaptureError::PermissionDenied,
        Failure::Unreachable(ashpd::Error::PortalNotFound(_)) => CaptureError::Backend(
            "this desktop's portal has no screen-capture interface; install the portal \
                 backend for this desktop (xdg-desktop-portal-gnome, -kde or -wlr)"
                .into(),
        ),
        Failure::Unreachable(e) => CaptureError::Backend(format!("{NO_PORTAL} ({e})")),
        Failure::Refused(ashpd::Error::RequiresVersion(need, have)) => {
            CaptureError::Backend(format!(
                "the desktop's screen-capture portal is too old (version {have}, needs {need}); \
                 update xdg-desktop-portal and its backend"
            ))
        }
        Failure::Refused(ashpd::Error::Response(ResponseError::Other)) => CaptureError::Backend(
            "the desktop refused screen sharing; start the session again, and if it keeps \
                 failing check `journalctl --user -u xdg-desktop-portal`"
                .into(),
        ),
        Failure::Refused(e) => {
            CaptureError::Backend(format!("the screen-sharing portal failed: {e}"))
        }
        Failure::TimedOut(limit) => CaptureError::Backend(format!(
            "nobody answered the screen-sharing dialog within {} s; start the session again \
             and choose a screen",
            limit.as_secs()
        )),
        // Only the reopen in `reinit` abandons, and nobody waits for that answer.
        Failure::Abandoned => CaptureError::Backend("screen sharing was stopped".into()),
    }
}

/// Whether a failed open is worth retrying. A declined or unanswered dialog is not:
/// retrying would put the same dialog in front of a user who said no or is not there.
fn retryable(e: &OpenError) -> bool {
    match e {
        OpenError::Portal(f) => !(f.is_cancel() || matches!(**f, Failure::TimedOut(_))),
        OpenError::Shared(e) => !e.dismissed,
        _ => true,
    }
}

/// Back-off between failed rebuilds: the host's retry loop must not hammer the portal
/// or PipeWire at 4 Hz while, say, PipeWire restarts. Same curve as the macOS capture.
fn retry_delay(failures: u32) -> Duration {
    const FIRST_MS: u64 = 250;
    const MAX_MS: u64 = 4_000;
    let doublings = failures.saturating_sub(1).min(16);
    Duration::from_millis((FIRST_MS << doublings).min(MAX_MS))
}

/// One open portal session and its stream. Fields drop in order: the PipeWire stream
/// stops (thread joined) before the portal session is told to close.
struct Live {
    slot: Arc<Slot>,
    _stream: PipeWireStream,
    /// Dropping it makes the watcher task close the portal session.
    _close: oneshot::Sender<()>,
}

/// The capture's hold on its portal session.
enum Held {
    /// A screen-only session the capture started and owns.
    Own(PortalSession),
    /// The process's shared combined session (`portal_shared`), also held by
    /// remote control; the last holder closes it.
    Shared(Arc<SharedSession>),
}

impl Held {
    async fn closed(&self) -> Result<(), ashpd::Error> {
        match self {
            Held::Own(session) => session.closed().await,
            Held::Shared(shared) => match shared.granted().await {
                Ok(granted) => granted.session.closed().await,
                // Never reached: the capture only holds a granted session.
                Err(_) => std::future::pending().await,
            },
        }
    }

    async fn release(self) {
        // The monitor the capture published belongs to this session.
        crate::portal_monitor::clear();
        match self {
            Held::Own(session) => session.close().await,
            Held::Shared(shared) => drop(shared),
        }
    }
}

/// Owns the capture's hold on the portal session on the portal runtime: closes the
/// slot when the desktop ends the session, and lets go of the session when the
/// capture does (closing it, unless remote control still holds the shared one).
async fn watch(session: Held, slot: Arc<Slot>, mut close: oneshot::Receiver<()>) {
    let ended_by_desktop = tokio::select! {
        closed = session.closed() => Some(closed),
        // A message or a dropped sender: the capture is done with the session.
        _ = &mut close => None,
    };
    match ended_by_desktop {
        Some(Ok(())) => {
            tracing::info!(
                "portal: the desktop closed the screen-cast session (the user stopped sharing)"
            );
            slot.close(Ended::ByDesktop);
        }
        Some(Err(e)) => {
            // The stream's own state still reports the end; just wait for the capture.
            tracing::debug!("portal: cannot watch the session for Closed: {e}");
            let _ = close.await;
        }
        None => {}
    }
    session.release().await;
}

/// The portal half of opening: handshake, store the replacement token, open the
/// PipeWire remote, and hand the session to [`watch`].
async fn open_portal(
    token_path: PathBuf,
    slot: Arc<Slot>,
    close: oneshot::Receiver<()>,
    abandon: oneshot::Receiver<()>,
) -> Result<(OwnedFd, u32), OpenError> {
    let stored = portal_token::read(&token_path);
    let (scope, previous) = plan(stored.as_ref());
    // A dropped sender counts as abandoning too: the capture is gone.
    let abandon = async {
        let _ = abandon.await;
    };
    // Join the one shared combined session (one dialog, one single-use token,
    // one monitor pick) when the token is combined, when remote control
    // already holds it, or when remote control would use the portal at all:
    // every host session creates an injector, so on a first run the capture
    // and the injector would otherwise race into two dialogs.
    if scope == Scope::ScreenAndInput
        || portal_shared::live().is_some()
        || crate::input::injector_uses_portal().await
    {
        return open_shared(&token_path, slot, close, abandon).await;
    }
    let granted = portal_session::start(scope, previous, Some(DIALOG_TIMEOUT), abandon)
        .await
        .map_err(|f| OpenError::Portal(Box::new(f)))?;
    // A token handed to `Start` is spent whatever happens next.
    portal_token::replace(&token_path, previous.is_some(), granted.token.as_ref());
    let opened =
        match granted.streams.first() {
            None => Err(OpenError::NoStream),
            Some(stream) => {
                tracing::info!(
                "portal: granted node {} ({:?}, compositor size {:?} at {:?}), {} restore token",
                stream.pipe_wire_node_id(),
                stream.source_type(),
                stream.size(),
                stream.position(),
                if granted.token.is_some() { "with a" } else { "no" },
            );
                let node = stream.pipe_wire_node_id();
                let monitor = crate::portal_monitor::Monitor::of(stream);
                granted
                    .open_pipewire_remote()
                    .await
                    .map(|fd| {
                        // Published only once the stream is really open: a failed
                        // open must not leave a monitor that no live session shows.
                        crate::portal_monitor::publish(monitor);
                        (fd, node)
                    })
                    .map_err(|e| OpenError::Remote(Box::new(e)))
            }
        };
    match opened {
        Ok(fd_node) => {
            tokio::spawn(watch(Held::Own(granted.session), slot, close));
            Ok(fd_node)
        }
        Err(e) => {
            granted.session.close().await;
            Err(e)
        }
    }
}

/// [`open_portal`] through the shared combined session: the stream it granted, the
/// PipeWire remote, and the capture's hold on the session for [`watch`]. The shared
/// handshake stores the replacement token itself.
async fn open_shared(
    token_path: &Path,
    slot: Arc<Slot>,
    close: oneshot::Receiver<()>,
    abandon: impl std::future::Future<Output = ()>,
) -> Result<(OwnedFd, u32), OpenError> {
    let shared = portal_shared::acquire_with_token(token_path);
    let opened = {
        let granted = tokio::select! {
            granted = tokio::time::timeout(DIALOG_TIMEOUT, shared.granted()) => match granted {
                Ok(Ok(granted)) => granted,
                Ok(Err(e)) => return Err(OpenError::Shared(e.clone())),
                Err(_) => {
                    return Err(OpenError::Portal(Box::new(Failure::TimedOut(DIALOG_TIMEOUT))))
                }
            },
            // Returning drops this hold; with no other holder the shared session
            // abandons its dialog.
            () = abandon => return Err(OpenError::Portal(Box::new(Failure::Abandoned))),
        };
        match granted.streams.first() {
            None => Err(OpenError::NoStream),
            Some(stream) => {
                tracing::info!(
                    "portal: shared session node {} ({:?}, compositor size {:?} at {:?})",
                    stream.pipe_wire_node_id(),
                    stream.source_type(),
                    stream.size(),
                    stream.position(),
                );
                let node = stream.pipe_wire_node_id();
                let monitor = crate::portal_monitor::Monitor::of(stream);
                granted
                    .open_pipewire_remote()
                    .await
                    .map(|fd| {
                        // Published only once the stream is really open: a failed
                        // open must not leave a monitor that no live session shows.
                        crate::portal_monitor::publish(monitor);
                        (fd, node)
                    })
                    .map_err(|e| OpenError::Remote(Box::new(e)))
            }
        }
    };
    let fd_node = opened?;
    tokio::spawn(watch(Held::Shared(shared), slot, close));
    Ok(fd_node)
}

/// A failed open: the trait error, and whether trying again can help.
#[derive(Debug)]
struct OpenFailed {
    error: CaptureError,
    retry: bool,
}

impl OpenFailed {
    fn retry(error: CaptureError) -> Self {
        Self { error, retry: true }
    }
}

type Opened = Result<(Live, (u32, u32)), OpenFailed>;

/// Open a session: portal (on the portal runtime), then the PipeWire stream (its own
/// thread), then wait for the negotiated size. Blocks for as long as the dialog is up
/// (at most [`DIALOG_TIMEOUT`]), so only `create` calls it on the host thread; `reinit`
/// runs it on a thread of its own ([`Reopening`]). Resolving `abandon` (or dropping its
/// sender) ends a pending dialog.
fn open(token_path: &Path, abandon: oneshot::Receiver<()>) -> Opened {
    let slot = Arc::new(Slot::new());
    let (close_tx, close_rx) = oneshot::channel();
    // If this wait gives up, the handshake still finishes on the runtime; `close_tx` is
    // gone by then, so the watcher closes the session straight away.
    let portal = portal_session::block_on(
        open_portal(token_path.to_owned(), Arc::clone(&slot), close_rx, abandon),
        DIALOG_TIMEOUT + PORTAL_SLACK,
    );
    let (fd, node) = match portal {
        Some(Ok(fd_node)) => fd_node,
        Some(Err(e)) => {
            tracing::warn!("portal: {e:?}");
            return Err(OpenFailed {
                retry: retryable(&e),
                error: open_error(&e),
            });
        }
        None => return Err(OpenFailed::retry(open_error(&OpenError::NoAnswer))),
    };
    let stream = PipeWireStream::start(fd, node, Arc::clone(&slot)).map_err(OpenFailed::retry)?;
    let live = Live {
        slot,
        _stream: stream,
        _close: close_tx,
    };
    match live.slot.wait_ready(FORMAT_TIMEOUT) {
        Ready::Streaming(size) => Ok((live, size)),
        Ready::Closed(_) => Err(OpenFailed::retry(CaptureError::Backend(
            "the screen stream ended before its first frame (see app.log for PipeWire's reason)"
                .into(),
        ))),
        Ready::TimedOut => Err(OpenFailed::retry(CaptureError::Backend(format!(
            "the compositor started no video within {} s: it offered no format this app reads \
             (shared-memory 32-bit RGB: BGRx, BGRA, RGBx, RGBA, xRGB, ARGB, xBGR or ABGR)",
            FORMAT_TIMEOUT.as_secs()
        )))),
    }
}

/// A session being opened off the host media thread during `reinit`, so a dialog never
/// blocks that thread (the engine joins it on disconnect).
struct Reopening {
    result: mpsc::Receiver<Opened>,
    /// Dropping it (with the capture) abandons the handshake: the portal session is
    /// closed, which dismisses a dialog nobody will answer now.
    _abandon: oneshot::Sender<()>,
}

impl Reopening {
    fn start(token_path: PathBuf) -> Result<Self, CaptureError> {
        let (abandon_tx, abandon_rx) = oneshot::channel();
        let (tx, rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("sharectrl-portal-open".into())
            .spawn(move || {
                // The capture may be gone by now; then the session just closes again.
                let _ = tx.send(open(&token_path, abandon_rx));
            })
            .map_err(|e| {
                CaptureError::Backend(format!("cannot start the session-opening thread: {e}"))
            })?;
        Ok(Self {
            result: rx,
            _abandon: abandon_tx,
        })
    }
}

/// Why no session is running.
enum Down {
    /// Waiting out the back-off before the next attempt.
    Backoff,
    Reopening(Reopening),
    /// The reopen finished; `reinit` adopts the result.
    Opened(Box<Opened>),
    /// Nothing more to try: the user declined or stopped sharing, or nobody answered.
    Terminal(Terminal),
}

/// A terminal state, kept as data because `CaptureError` is not `Clone` and every
/// later `acquire` must report it again.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Terminal {
    Denied,
    Failed(String),
}

impl Terminal {
    fn from_error(e: &CaptureError) -> Self {
        match e {
            CaptureError::PermissionDenied => Terminal::Denied,
            CaptureError::Backend(m) => Terminal::Failed(m.clone()),
            other => Terminal::Failed(other.to_string()),
        }
    }

    fn error(&self) -> CaptureError {
        match self {
            Terminal::Denied => CaptureError::PermissionDenied,
            Terminal::Failed(m) => CaptureError::Backend(m.clone()),
        }
    }
}

/// How long `reinit` waits, after the stream ended, for the portal's `Closed` signal
/// that follows "Stop sharing" (the node usually disappears first).
const DESKTOP_CLOSE_GRACE: Duration = Duration::from_secs(1);

/// What the consumer last told the host about the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Reported {
    visible: bool,
    position: Point,
    shape_serial: u64,
}

pub(crate) struct PortalCapture {
    token_path: PathBuf,
    /// Negotiated frame size = `dimensions()`.
    size: (u32, u32),
    /// The newest picture. Kept after `release` while nothing newer has arrived,
    /// because a pointer-only frame must still carry a valid picture: the host encodes
    /// it when a keyframe is due on a static screen (DXGI hands out the unchanged
    /// desktop then too). Declared before `live` so it goes first.
    picture: Option<Picture>,
    live: Option<Live>,
    /// Why no session runs, while `live` is `None`.
    down: Down,
    failures: u32,
    retry_at: Instant,
    /// Slot pointer serial already looked at.
    seen_pointer: u64,
    reported: Option<Reported>,
}

/// `Platform::create_capture` on Wayland. The portal, not the app, picks the monitor
/// (the user chooses it in the dialog and the restore token remembers it), so no
/// display id names a Wayland monitor; a saved one is refused and the host falls back
/// to `None`, as for an unplugged display.
///
/// This first open blocks the calling (host media) thread while the consent dialog is
/// up, at most [`DIALOG_TIMEOUT`]: the session cannot start without an answer. Later
/// rebuilds never block it (see [`Reopening`]).
pub(crate) fn create(
    display: Option<&str>,
    state_dir: &Path,
) -> Result<PortalCapture, CaptureError> {
    if let Some(id) = display {
        tracing::warn!(
            "display id {id:?} cannot be chosen on Wayland; the portal picks the screen"
        );
        return Err(CaptureError::NoOutput);
    }
    let token_path = portal_token::path_in(state_dir);
    // Kept until `open` returns, so the first dialog is never abandoned.
    let (_keep, abandon) = oneshot::channel();
    let (live, size) = open(&token_path, abandon).map_err(|f| f.error)?;
    tracing::info!("portal: capturing {}x{} (packed BGRA)", size.0, size.1);
    Ok(PortalCapture {
        token_path,
        size,
        picture: None,
        live: Some(live),
        down: Down::Backoff,
        failures: 0,
        retry_at: Instant::now(),
        seen_pointer: 0,
        reported: None,
    })
}

impl PortalCapture {
    fn drop_picture(&mut self) {
        if let (Some(old), Some(live)) = (self.picture.take(), &self.live) {
            live.slot.recycle(old.pixels);
        }
    }

    /// The pointer update since the last report, if anything changed.
    fn pointer_update(&mut self, slot: &Slot) -> Option<CursorUpdate> {
        let (serial, p) = slot.pointer();
        self.seen_pointer = serial;
        let now = Reported {
            visible: p.visible,
            position: p.position,
            shape_serial: p.shape_serial,
        };
        if self.reported == Some(now) {
            return None;
        }
        let shape_changed = self.reported.map(|r| r.shape_serial) != Some(p.shape_serial);
        self.reported = Some(now);
        Some(CursorUpdate {
            visible: p.visible,
            position: p.position,
            shape: if shape_changed {
                p.shape.as_deref().cloned()
            } else {
                None
            },
        })
    }

    fn lend(
        &self,
        dirty: Vec<Rect>,
        pointer_only: bool,
        cursor: Option<CursorUpdate>,
    ) -> Option<CapturedFrame<'_>> {
        let p = self.picture.as_ref()?;
        Some(CapturedFrame {
            frame: Frame::bgra(p.width, p.height, &p.pixels, p.width as usize * 4),
            dirty,
            moves: Vec::new(),
            pointer_only,
            cursor,
        })
    }

    /// End the running session. `stopped_by_user` = the desktop closed it ("Stop
    /// sharing"): terminal, the host ends the session.
    fn end_session(&mut self, stopped_by_user: bool) {
        self.drop_picture();
        self.live = None;
        self.retry_at = Instant::now();
        self.down = if stopped_by_user {
            tracing::info!("portal: sharing was stopped from the desktop; not reopening it");
            Down::Terminal(Terminal::Denied)
        } else {
            Down::Backoff
        };
    }

    /// `acquire` while no session is running. Reports `Lost` when there is something
    /// for `reinit` to do (the back-off has elapsed, or a reopen finished), idles
    /// otherwise, and repeats a terminal error.
    fn acquire_while_down(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        match &self.down {
            Down::Terminal(t) => Err(t.error()),
            Down::Opened(_) => Err(CaptureError::Lost),
            Down::Reopening(r) => match r.result.recv_timeout(timeout) {
                Ok(opened) => {
                    self.down = Down::Opened(Box::new(opened));
                    Err(CaptureError::Lost)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.down = Down::Opened(Box::new(Err(OpenFailed::retry(
                        CaptureError::Backend("the session-opening thread exited".into()),
                    ))));
                    Err(CaptureError::Lost)
                }
            },
            Down::Backoff => {
                let now = Instant::now();
                if now >= self.retry_at {
                    return Err(CaptureError::Lost);
                }
                std::thread::sleep(timeout.min(self.retry_at - now));
                Ok(None)
            }
        }
    }

    fn adopt(&mut self, size: (u32, u32)) {
        if size != self.size {
            tracing::info!(
                "portal: capture size {}x{} → {}x{}",
                self.size.0,
                self.size.1,
                size.0,
                size.1
            );
        }
        self.size = size;
        // Resend the pointer (shape included) with the next frame: the host forces a
        // keyframe after `reinit` and the viewer may have reset too.
        self.reported = None;
    }

    /// Take over the result of a finished reopen.
    fn finish_reopen(&mut self, opened: Opened) -> Result<(), CaptureError> {
        match opened {
            Ok((live, size)) => {
                tracing::info!(
                    "portal: session rebuilt at {}x{} after {} failed attempt(s)",
                    size.0,
                    size.1,
                    self.failures
                );
                self.live = Some(live);
                self.seen_pointer = 0;
                self.adopt(size);
                self.failures = 0;
                self.down = Down::Backoff;
                Ok(())
            }
            Err(OpenFailed { error, retry: true }) => {
                self.failures += 1;
                self.retry_at = Instant::now() + retry_delay(self.failures);
                self.down = Down::Backoff;
                Err(error)
            }
            Err(OpenFailed {
                error,
                retry: false,
            }) => {
                self.down = Down::Terminal(Terminal::from_error(&error));
                Err(error)
            }
        }
    }
}

impl ScreenCapture for PortalCapture {
    fn dimensions(&self) -> (u32, u32) {
        self.size
    }

    fn acquire(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        let Some(slot) = self.live.as_ref().map(|l| Arc::clone(&l.slot)) else {
            return self.acquire_while_down(timeout);
        };
        match slot.wait(timeout, self.seen_pointer) {
            Wait::Picture(mut picture) => {
                if (picture.width, picture.height) != self.size {
                    // The slot only accepts pictures of the negotiated size and reports
                    // a change first, so this cannot happen; if it ever did, encoding it
                    // would feed the encoder a size it was not built for (invariant 1).
                    slot.recycle(picture.pixels);
                    return Err(CaptureError::Lost);
                }
                let damage = std::mem::replace(&mut picture.damage, damage::Damage::Full);
                if let Some(old) = self.picture.replace(picture) {
                    slot.recycle(old.pixels);
                }
                let cursor = self.pointer_update(&slot);
                Ok(self.lend(damage.into_dirty(), false, cursor))
            }
            Wait::Pointer => {
                // Before the first picture there is nothing to report against; the
                // position goes out with that picture instead.
                if self.picture.is_none() {
                    self.seen_pointer = slot.pointer().0;
                    return Ok(None);
                }
                match self.pointer_update(&slot) {
                    Some(cursor) => Ok(self.lend(Vec::new(), true, Some(cursor))),
                    None => Ok(None),
                }
            }
            Wait::Timeout => Ok(None),
            Wait::Interrupted => {
                // The kept picture has the old size or predates the pause.
                self.drop_picture();
                Err(CaptureError::Lost)
            }
            Wait::Closed(Ended::ByDesktop) => {
                self.end_session(true);
                Err(CaptureError::PermissionDenied)
            }
            // A stream failure: `reinit` first checks whether the desktop's `Closed`
            // signal follows (then it was "Stop sharing" after all).
            Wait::Closed(Ended::Stream) => {
                self.drop_picture();
                Err(CaptureError::Lost)
            }
        }
    }

    fn release(&mut self) {
        // The borrow handed out by `acquire` ends here. The picture itself goes back to
        // the pool as soon as a newer one is waiting; on a static screen it is kept for
        // pointer-only frames.
        if self.live.as_ref().is_some_and(|l| l.slot.has_pending()) {
            self.drop_picture();
        }
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        self.drop_picture();
        if let Some(live) = &self.live {
            let stopped_by_user = match live.slot.wait_ready(RESUME_TIMEOUT) {
                Ready::Streaming(size) => {
                    self.adopt(size);
                    return Ok(());
                }
                Ready::Closed(Ended::ByDesktop) => true,
                Ready::Closed(Ended::Stream) => {
                    live.slot.wait_ended_by_desktop(DESKTOP_CLOSE_GRACE)
                }
                Ready::TimedOut => {
                    tracing::info!(
                        "portal: stream did not resume within {} s; opening a new session",
                        RESUME_TIMEOUT.as_secs()
                    );
                    false
                }
            };
            self.end_session(stopped_by_user);
        }
        match std::mem::replace(&mut self.down, Down::Backoff) {
            Down::Terminal(t) => {
                let e = t.error();
                self.down = Down::Terminal(t);
                Err(e)
            }
            Down::Opened(opened) => self.finish_reopen(*opened),
            Down::Reopening(r) => match r.result.try_recv() {
                Ok(opened) => self.finish_reopen(opened),
                Err(mpsc::TryRecvError::Empty) => {
                    self.down = Down::Reopening(r);
                    Err(CaptureError::Backend(
                        "still reopening the screen-sharing session".into(),
                    ))
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.finish_reopen(Err(OpenFailed::retry(CaptureError::Backend(
                        "the session-opening thread exited".into(),
                    ))))
                }
            },
            Down::Backoff => {
                // Off this thread: if the token no longer restores, a dialog appears,
                // and the engine joins this thread on disconnect.
                tracing::info!("portal: reopening the screen-cast session from the token");
                match Reopening::start(self.token_path.clone()) {
                    Ok(r) => {
                        self.down = Down::Reopening(r);
                        Err(CaptureError::Backend(
                            "reopening the screen-sharing session".into(),
                        ))
                    }
                    Err(e) => self.finish_reopen(Err(OpenFailed::retry(e))),
                }
            }
        }
    }

    fn device(&self) -> Option<&dyn Any> {
        // CPU frames (shared-memory buffers): no GPU device to share.
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ashpd::desktop::ResponseError;

    fn shared(dismissed: bool) -> OpenError {
        OpenError::Shared(SharedError {
            dismissed,
            message: "why".into(),
        })
    }

    #[test]
    fn a_declined_shared_session_is_a_permission_error_and_never_retried() {
        assert!(matches!(
            open_error(&shared(true)),
            CaptureError::PermissionDenied
        ));
        assert!(
            !retryable(&shared(true)),
            "retrying would show the dialog again"
        );
    }

    #[test]
    fn a_failed_shared_session_is_a_backend_error_and_retried() {
        assert!(matches!(open_error(&shared(false)), CaptureError::Backend(m) if m == "why"));
        assert!(retryable(&shared(false)));
    }

    fn token(covers_input: bool) -> StoredToken {
        StoredToken {
            token: "tok".into(),
            covers_input,
        }
    }

    fn portal(f: Failure) -> OpenError {
        OpenError::Portal(Box::new(f))
    }

    #[test]
    fn plan_restores_with_a_session_of_the_tokens_scope() {
        assert_eq!(plan(None), (Scope::Screen, None));
        assert_eq!(plan(Some(&token(false))), (Scope::Screen, Some("tok")));
        assert_eq!(
            plan(Some(&token(true))),
            (Scope::ScreenAndInput, Some("tok"))
        );
    }

    #[test]
    fn a_cancelled_dialog_is_a_permission_denial() {
        let e = portal(Failure::Refused(ashpd::Error::Response(
            ResponseError::Cancelled,
        )));
        assert!(matches!(open_error(&e), CaptureError::PermissionDenied));
    }

    #[test]
    fn every_other_failure_says_what_to_do() {
        let msg = |e: OpenError| match open_error(&e) {
            CaptureError::Backend(m) => m,
            other => panic!("{other:?}"),
        };
        assert!(msg(portal(Failure::Unreachable(ashpd::Error::NoResponse)))
            .contains("xdg-desktop-portal"));
        assert!(msg(portal(Failure::Refused(ashpd::Error::Response(
            ResponseError::Other
        ))))
        .contains("journalctl"));
        assert!(msg(portal(Failure::Refused(ashpd::Error::RequiresVersion(
            4, 2
        ))))
        .contains("version 2, needs 4"));
        assert!(msg(portal(Failure::TimedOut(DIALOG_TIMEOUT))).contains("120 s"));
        assert!(msg(OpenError::NoStream).contains("choose a monitor"));
        assert!(msg(OpenError::Remote(Box::new(ashpd::Error::NoResponse))).contains("PipeWire"));
        assert!(msg(OpenError::NoAnswer).contains("xdg-desktop-portal"));
    }

    #[test]
    fn a_declined_or_unanswered_dialog_is_never_retried() {
        let cancel = portal(Failure::Refused(ashpd::Error::Response(
            ResponseError::Cancelled,
        )));
        assert!(!retryable(&cancel));
        assert!(!retryable(&portal(Failure::TimedOut(DIALOG_TIMEOUT))));
        // Everything else may be a passing fault (PipeWire restarting, a slow portal).
        assert!(retryable(&portal(Failure::Unreachable(
            ashpd::Error::NoResponse
        ))));
        assert!(retryable(&OpenError::NoStream));
        assert!(retryable(&OpenError::Remote(Box::new(
            ashpd::Error::NoResponse
        ))));
        assert!(retryable(&OpenError::NoAnswer));
    }

    #[test]
    fn terminal_states_repeat_the_same_error() {
        let denied = Terminal::from_error(&CaptureError::PermissionDenied);
        assert!(matches!(denied.error(), CaptureError::PermissionDenied));
        assert!(matches!(denied.error(), CaptureError::PermissionDenied));
        let failed = Terminal::from_error(&CaptureError::Backend("no answer".into()));
        assert!(matches!(failed.error(), CaptureError::Backend(m) if m == "no answer"));
    }

    #[test]
    fn retry_backs_off_to_a_cap() {
        let ms = |n| retry_delay(n).as_millis();
        assert_eq!(ms(0), 250);
        assert_eq!(ms(1), 250);
        assert_eq!(ms(2), 500);
        assert_eq!(ms(5), 4_000);
        assert_eq!(ms(u32::MAX), 4_000);
    }

    #[test]
    fn a_saved_display_id_is_refused_without_touching_the_portal() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            create(Some("x11:0"), dir.path()),
            Err(CaptureError::NoOutput)
        ));
    }

    /// WSLg, CI containers and headless boxes have no portal: capture must fail with
    /// the actionable message, quickly, and never hang. Skips itself where a portal
    /// answers (that path is the ignored test in tests/portal_capture.rs).
    #[test]
    fn no_portal_fails_with_an_actionable_error() {
        let probe = portal_session::block_on(
            async { ashpd::desktop::screencast::Screencast::new().await.is_ok() },
            Duration::from_secs(5),
        );
        if probe != Some(false) {
            eprintln!("skipped: a screen-cast portal is reachable here");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();
        match create(None, dir.path()) {
            Err(CaptureError::Backend(m)) => assert!(m.contains("portal"), "{m}"),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("captured without a portal"),
        }
        assert!(started.elapsed() < Duration::from_secs(30));
        // Nothing was granted, so nothing was stored.
        assert!(!portal_token::path_in(dir.path()).exists());
    }
}
