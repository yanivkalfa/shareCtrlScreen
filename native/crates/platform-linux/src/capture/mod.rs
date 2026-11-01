//! `ScreenCapture` on X11 (plan task L2): MIT-SHM + DAMAGE + XFixes + RandR.
//!
//! * **Change tracking.** A Damage object on the root window at report level
//!   `NonEmpty` wakes the capture once when something is drawn; at grab time
//!   `DamageSubtract` moves the accumulated damage into an XFixes region,
//!   `FetchRegion` returns its rectangles, and only those are read — clipped
//!   to the shared monitor and merged into at most 16 disjoint dirty rects.
//!   No damage within the timeout → `Ok(None)`, so a static screen costs
//!   nothing (trait invariant 2).
//! * **Pixels.** `ShmGetImage` of each dirty rectangle into a server-created
//!   MIT-SHM 1.2 segment sized for the monitor, read back through its fd into
//!   a persistent picture that is lent as `FrameData::Cpu(Bgra8)`. Core
//!   `GetImage` when MIT-SHM is unavailable.
//! * **Cursor.** Never part of the picture (X11 does not draw the pointer
//!   into the root window): position from `QueryPointer`, image from XFixes
//!   `GetCursorImage` on each `CursorNotify`, reported out of band like DXGI
//!   pointer metadata and macOS. A pointer move on a static screen is a
//!   `pointer_only` frame.
//! * **Monitors.** One X11 screen spans every monitor; RandR 1.5 monitors say
//!   which rectangle is which ([`monitors`] has the id scheme).
//! * **Loss.** A RandR / root `ConfigureNotify` that changes the captured
//!   monitor, the X server going away, or a failing request → `Lost`;
//!   `reinit` reconnects from scratch with the new geometry.
//! * **Refusal.** Under Xwayland the root window shows only X11 clients, not
//!   the desktop (plans/02 §1), so the capture refuses to start there.
//!
//! ## References
//! * DAMAGE extension 1.1 (`DamageReportNonEmpty`, `DamageSubtract` with
//!   repair `None` moving the damage into `parts`):
//!   <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/blob/master/damageproto.txt>
//! * XFIXES extension (regions, `FetchRegion` "YX-banded", `SelectCursorInput`,
//!   `GetCursorImage` premultiplied ARGB):
//!   <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/blob/master/fixesproto.txt>
//! * RandR 1.5 (`RRGetMonitors` with `get_active`, `RRSelectInput` masks and
//!   the notification sent on selection):
//!   <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/blob/master/randrproto.txt>
//! * MIT-SHM 1.2 (`ShmCreateSegment` returning an fd, `ShmGetImage` offsets):
//!   <https://gitlab.freedesktop.org/xorg/proto/xcbproto/-/blob/master/src/shm.xml>
//! * XWAYLAND extension ("if the extension is present, the X server is
//!   Xwayland"): <https://gitlab.freedesktop.org/xorg/proto/xorgproto/-/blob/master/xwaylandproto.txt>
//! * X11 core protocol: image formats and scanline padding, `GetImage`,
//!   `QueryPointer`, `SendEvent` with an empty mask, connection close:
//!   <https://www.x.org/releases/X11R7.7/doc/xproto/x11protocol.html>
//! * x11rb 0.14 — `RustConnection` (one thread waiting for events while others
//!   send requests; fd passing), extension modules:
//!   <https://docs.rs/x11rb/0.14.0/x11rb/rust_connection/struct.RustConnection.html>
//! * plans/02-LINUX-FINDINGS.md §1 (X11), plans/04-TARGET-ARCHITECTURE.md §3.

mod events;
mod geometry;
mod monitors;
mod pixels;
mod session;

use std::any::Any;
use std::ffi::OsString;
use std::time::{Duration, Instant};

use platform_api::{CaptureError, CapturedFrame, DisplayInfo, Frame, ScreenCapture};
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::randr::{self, ConnectionExt as _};

use crate::x11_connect::x11_connect;
use session::Session;

/// Longest single wait inside `acquire`, so a pointer move on a static screen
/// is reported within one 60 Hz frame even when the caller passes a long
/// timeout (the same bound the macOS capture uses).
const CURSOR_POLL: Duration = Duration::from_millis(16);

/// Consecutive sessions that failed a request before delivering a frame
/// after which the capture gives up with the error instead of `Lost`. A
/// screen reconfiguration racing a grab fails once and recovers on reinit;
/// failing every time means something reinit cannot fix, and looping on it
/// would hide the reason from the user.
const MAX_FAILED_SESSIONS: u32 = 3;

/// Back-off between reconnection attempts after `reinit` failed: 250 ms,
/// doubling, at most 5 s — the X server may be restarting (or the user logging
/// out), and the host's retry loop must not hammer it or fill the log.
fn retry_delay(failures: u32) -> Duration {
    let ms = 250u64.saturating_mul(1 << failures.saturating_sub(1).min(5));
    Duration::from_millis(ms.min(5_000))
}

/// The X display to use: the explicit name, else `$DISPLAY`.
fn display_name(explicit: Option<&str>, env: Option<OsString>) -> Result<String, CaptureError> {
    if let Some(name) = explicit {
        return Ok(name.to_owned());
    }
    match env.map(OsString::into_string) {
        Some(Ok(name)) if !name.is_empty() => Ok(name),
        Some(Err(raw)) => Err(CaptureError::Backend(format!(
            "DISPLAY is not valid text ({raw:?}); start ShareCtrlScreen from inside your \
             desktop session"
        ))),
        _ => Err(CaptureError::Backend(
            "DISPLAY is not set, so there is no X11 desktop to capture. Start ShareCtrlScreen \
             from inside your desktop session (not over SSH or as a system service)."
                .into(),
        )),
    }
}

/// The monitors of the X display `display` (`None` = `$DISPLAY`), for
/// [`Platform::displays`](platform_api::Platform::displays).
pub fn x11_displays(display: Option<&str>) -> Result<Vec<DisplayInfo>, CaptureError> {
    let name = display_name(display, std::env::var_os("DISPLAY"))?;
    let (conn, screen) = x11_connect(&name).map_err(|e| {
        CaptureError::Backend(format!("cannot connect to the X server at {name} ({e})"))
    })?;
    let fail = |e: &dyn std::fmt::Display| {
        CaptureError::Backend(format!("listing the displays of {name}: {e}"))
    };
    let root = conn
        .setup()
        .roots
        .get(screen)
        .map(|s| s.root)
        .ok_or_else(|| fail(&"the server has no such screen"))?;
    let randr_monitors = if conn
        .extension_information(randr::X11_EXTENSION_NAME)
        .map_err(|e| fail(&e))?
        .is_some()
    {
        let v = conn
            .randr_query_version(1, 5)
            .map_err(|e| fail(&e))?
            .reply()
            .map_err(|e| fail(&e))?;
        (v.major_version, v.minor_version) >= (1, 5)
    } else {
        false
    };
    let list = monitors::query(&conn, root, randr_monitors).map_err(|e| fail(&e))?;
    let scale = monitors::desktop_scale(&conn, root);
    Ok(list
        .iter()
        .map(|m| monitors::to_display_info(m, scale))
        .collect())
}

/// What `acquire` found, before the frame is lent out.
enum Found {
    Frame {
        dirty: Vec<platform_api::Rect>,
        pointer_only: bool,
        cursor: Option<platform_api::CursorUpdate>,
    },
    Idle,
}

/// X11 screen capture of one monitor. Create it on the thread that uses it
/// (the connection and its event thread belong to this object).
pub struct X11Capture {
    display: String,
    /// The `DisplayInfo::id` asked for; `None` = the primary monitor.
    share_id: Option<String>,
    session: Option<Session>,
    size: (u32, u32),
    /// Failed `reinit`s in a row, for the back-off.
    reinit_failures: u32,
    retry_at: Instant,
    /// Sessions in a row that failed a request before delivering a frame.
    failed_sessions: u32,
}

impl X11Capture {
    /// Capture monitor `monitor` (a [`DisplayInfo::id`], `None` = primary) of
    /// X display `display` (`None` = `$DISPLAY`). An unknown id is
    /// [`CaptureError::NoOutput`]; the host then shares the primary.
    pub fn open(display: Option<&str>, monitor: Option<&str>) -> Result<Self, CaptureError> {
        let display = display_name(display, std::env::var_os("DISPLAY"))?;
        let session = Session::open(&display, monitor, false)?;
        Ok(Self {
            size: session.size(),
            display,
            share_id: monitor.map(str::to_owned),
            session: Some(session),
            reinit_failures: 0,
            retry_at: Instant::now(),
            failed_sessions: 0,
        })
    }

    /// Whether pixels come through MIT-SHM (`false`: core `GetImage`, or no
    /// session right now). Xorg, Xvfb and Xephyr reached over a Unix socket
    /// (file or abstract) offer it; TCP cannot pass the segment's fd. Logged
    /// at open; the tests assert it.
    pub fn uses_mit_shm(&self) -> bool {
        self.session.as_ref().is_some_and(Session::uses_mit_shm)
    }

    /// End the session after it broke. `failed`: a request failed (as
    /// opposed to the screen being reconfigured), which counts towards
    /// [`MAX_FAILED_SESSIONS`].
    fn lose(&mut self, why: &str, failed: bool) -> CaptureError {
        self.session = None;
        self.retry_at = Instant::now();
        if !failed {
            tracing::info!("x11 capture: {why}; rebuilding");
            return CaptureError::Lost;
        }
        self.failed_sessions += 1;
        tracing::warn!("x11 capture: {why}");
        if self.failed_sessions >= MAX_FAILED_SESSIONS {
            return CaptureError::Backend(format!("X11 capture keeps failing: {why}"));
        }
        CaptureError::Lost
    }

    /// One step of `acquire` against a live session.
    fn poll(&mut self, timeout: Duration) -> Result<Found, CaptureError> {
        let start = Instant::now();
        loop {
            let Some(session) = self.session.as_mut() else {
                return Ok(Found::Idle);
            };
            let left = timeout.saturating_sub(start.elapsed());
            let signals = session.wait(left.min(CURSOR_POLL));
            if let Some(reason) = signals.closed {
                return Err(self.lose(
                    &format!("connection to {} closed: {reason}", self.display),
                    false,
                ));
            }
            if signals.screen {
                match session.reconfigured() {
                    Ok(false) => {}
                    Ok(true) => return Err(self.lose("screen configuration changed", false)),
                    Err(e) => return Err(self.lose(&e, true)),
                }
            }
            let cursor = match session.poll_cursor(signals.cursor) {
                Ok(c) => c,
                Err(e) => return Err(self.lose(&e, true)),
            };
            if signals.damaged || !session.filled() {
                let grab = match session.grab() {
                    Ok(g) => g,
                    Err(e) => return Err(self.lose(&e, true)),
                };
                // Damage elsewhere on the screen (another monitor) leaves
                // nothing to send for this one.
                if !grab.dirty.is_empty() {
                    self.failed_sessions = 0;
                    return Ok(Found::Frame {
                        dirty: grab.dirty,
                        pointer_only: false,
                        cursor,
                    });
                }
            }
            // A cursor-only frame needs a picture to point at; before the
            // first grab there is nothing to report against.
            if cursor.is_some() && session.filled() {
                return Ok(Found::Frame {
                    dirty: Vec::new(),
                    pointer_only: true,
                    cursor,
                });
            }
            if start.elapsed() >= timeout {
                return Ok(Found::Idle);
            }
        }
    }

    /// `acquire` while no session is running: report the loss again once the
    /// back-off has elapsed (the host then calls `reinit`), idle until then.
    fn acquire_while_down(
        &self,
        timeout: Duration,
    ) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        let now = Instant::now();
        if now >= self.retry_at {
            return Err(CaptureError::Lost);
        }
        std::thread::sleep(timeout.min(self.retry_at - now));
        Ok(None)
    }
}

impl ScreenCapture for X11Capture {
    fn dimensions(&self) -> (u32, u32) {
        self.size
    }

    fn acquire(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        if self.session.is_none() {
            return self.acquire_while_down(timeout);
        }
        let Found::Frame {
            dirty,
            pointer_only,
            cursor,
        } = self.poll(timeout)?
        else {
            return Ok(None);
        };
        let Some(session) = self.session.as_ref() else {
            return Ok(None);
        };
        let (pixels, stride) = session.picture();
        let (width, height) = session.size();
        Ok(Some(CapturedFrame {
            frame: Frame::bgra(width, height, pixels, stride),
            dirty,
            moves: Vec::new(),
            pointer_only,
            cursor,
        }))
    }

    fn release(&mut self) {
        // Nothing to hand back: the picture is our own buffer, kept so a
        // partial update or a cursor-only frame always has a whole screen to
        // show, and the borrow checker already ends the lent frame before the
        // next `acquire` can write into it.
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        self.session = None;
        match Session::open(&self.display, self.share_id.as_deref(), true) {
            Ok(session) => {
                tracing::info!(
                    "x11 capture: rebuilt on {} at {}x{} after {} failed attempt(s)",
                    session.monitor_id(),
                    session.size().0,
                    session.size().1,
                    self.reinit_failures
                );
                self.size = session.size();
                self.session = Some(session);
                self.reinit_failures = 0;
                Ok(())
            }
            Err(e) => {
                self.reinit_failures += 1;
                self.retry_at = Instant::now() + retry_delay(self.reinit_failures);
                Err(e)
            }
        }
    }

    fn device(&self) -> Option<&dyn Any> {
        // CPU frames: every encoder accepts `FrameData::Cpu`.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_comes_from_the_argument_then_the_environment() {
        assert_eq!(
            display_name(Some(":3"), Some(":0".into())).ok().as_deref(),
            Some(":3")
        );
        assert_eq!(
            display_name(None, Some(":0".into())).ok().as_deref(),
            Some(":0")
        );
    }

    #[test]
    fn missing_display_is_an_actionable_error() {
        for env in [None, Some(OsString::new())] {
            match display_name(None, env) {
                Err(CaptureError::Backend(m)) => {
                    assert!(
                        m.contains("DISPLAY is not set") && m.contains("desktop session"),
                        "{m}"
                    )
                }
                other => panic!("expected a Backend error, got {other:?}"),
            }
        }
    }

    #[test]
    fn non_utf8_display_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(vec![b':', 0xff]);
        assert!(matches!(
            display_name(None, Some(raw)),
            Err(CaptureError::Backend(_))
        ));
    }

    #[test]
    fn retry_backs_off_to_five_seconds() {
        assert_eq!(retry_delay(1), Duration::from_millis(250));
        assert_eq!(retry_delay(2), Duration::from_millis(500));
        assert_eq!(retry_delay(4), Duration::from_millis(2_000));
        assert_eq!(retry_delay(6), Duration::from_millis(5_000));
        assert_eq!(retry_delay(1_000), Duration::from_millis(5_000));
    }
}
