//! Screen capture (host side). Contract derived from `capture::Duplicator`
//! (DXGI Desktop Duplication), which every backend must reproduce.

use std::any::Any;
use std::time::Duration;

use crate::frame::{Frame, MoveRect, Point, Rect};

/// Cursor bitmap, always converted to **straight-alpha** BGRA by the backend
/// (DXGI's monochrome / masked-color shapes included; CoreGraphics, XFixes and
/// PipeWire premultiplied images are divided back out).
///
/// Units: `width`, `height` and `hotspot` are **captured-frame pixels**, the
/// same pixels as [`CursorUpdate::position`] and the frame's size. A backend
/// whose OS hands out the image in another scale (macOS points on a Retina
/// display) renders it at the frame's scale first, so the viewer can draw it
/// at `bitmap size / frame size` of the video rectangle and it matches what
/// the host's screen shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    /// The pixel of the image that sits on [`CursorUpdate::position`] (the
    /// arrow's tip, the I-beam's centre), measured from the image's top-left.
    pub hotspot: Point,
    /// `width * height * 4` bytes: B, G, R, A per pixel, rows top-down with no
    /// padding, straight (not premultiplied) alpha.
    pub bgra: Vec<u8>,
}

/// Cursor change since the previous frame. Reported **only when something
/// changed**; between updates the caller keeps the last value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorUpdate {
    pub visible: bool,
    /// Where the pointer acts, in captured-frame pixels: the **hotspot**
    /// position, not the image's top-left corner. The image is drawn with its
    /// top-left at `position - shape.hotspot`. (DXGI Desktop Duplication
    /// reports the image's top-left instead; the Windows backend adds the
    /// hotspot so every backend means the same point.)
    pub position: Point,
    /// New shape, if the shape changed.
    pub shape: Option<CursorShape>,
}

/// One captured frame. Valid until [`ScreenCapture::release`] or the next
/// [`ScreenCapture::acquire`].
#[derive(Debug)]
pub struct CapturedFrame<'a> {
    pub frame: Frame<'a>,
    /// Regions whose pixels changed. Empty with `pointer_only == false` means
    /// "assume everything changed" (backends without damage information).
    pub dirty: Vec<Rect>,
    pub moves: Vec<MoveRect>,
    /// No new pixels, only a cursor update (the caller sends no video).
    pub pointer_only: bool,
    pub cursor: Option<CursorUpdate>,
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    /// The capture session is gone (mode change, display sleep, desktop switch,
    /// portal stream closed). The caller calls [`ScreenCapture::reinit`] and then
    /// forces a keyframe. This is the only error the host loop recovers from.
    #[error("capture lost; reinit required")]
    Lost,
    /// The user has not granted (or revoked) screen capture permission.
    #[error("screen capture permission denied")]
    PermissionDenied,
    /// Windows secure desktop (UAC / Winlogon) without the SYSTEM service.
    #[error("secure desktop not capturable from this process")]
    SecureDesktop,
    /// The requested display does not exist.
    #[error("no such display")]
    NoOutput,
    #[error("capture backend: {0}")]
    Backend(String),
}

/// What the host's screen is, as far as the capture backend can tell (plans/10
/// U1, 10a §3.1). The host tells the viewer on every change, so a screen that
/// cannot be captured is explained instead of shown as a black or frozen
/// picture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenState {
    /// The ordinary desktop: what [`ScreenCapture::acquire`] delivers is what
    /// the person at the computer sees. Also the answer of every backend that
    /// cannot tell (the default).
    Normal,
    /// A secure desktop is showing and is being captured.
    Secure(SecureKind),
    /// A secure desktop is showing and cannot be captured: `acquire` returns
    /// [`CaptureError::Lost`] until it is gone.
    Blocked(BlockReason),
}

/// Which secure desktop is showing, so the viewer does not call a
/// permission prompt a lock screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureKind {
    /// The lock screen of a signed-in user.
    Lock,
    /// A prompt of the operating system (Windows: User Account Control, or
    /// the Ctrl+Alt+Del screen) in an unlocked session.
    Prompt,
    /// The sign-in screen with nobody signed in.
    SignIn,
}

/// Why a secure desktop cannot be shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// This host has no way to show it (Windows: no installed service; every
    /// Windows host before plans/10 U3).
    NotSupported,
    /// The host's own setting is off.
    Disabled,
    /// The machine's policy says no (Windows: prompts, unless an
    /// administrator allowed them).
    NotAllowed,
    /// The helper that would show it is not answering.
    AgentUnavailable,
}

/// The rule a backend's [`ScreenCapture::screen_state`] follows for
/// [`ScreenState::Blocked`] (10a §4.1), kept in one place so every backend
/// and the mock apply it the same way: the state is decided by the **last
/// attempt to get a picture** (the constructor, a `reinit`). An attempt that
/// got one means the screen is reachable — `Normal`, whatever was refused
/// along the way (the desktop can change between a refused check and a
/// duplication that then succeeds). An attempt that failed is `Blocked` only
/// when the backend positively knows why (a refusal); any other failure is
/// `Normal` and simply retried. A lost `acquire` alone changes nothing: the
/// `reinit` that follows decides.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScreenAccess {
    blocked: Option<BlockReason>,
}

impl ScreenAccess {
    /// The last attempt got a picture.
    pub fn reached(&mut self) {
        self.blocked = None;
    }

    /// The last attempt failed; `refused` says why when the screen is one
    /// the backend cannot reach, `None` for any other failure.
    pub fn failed(&mut self, refused: Option<BlockReason>) {
        self.blocked = refused;
    }

    /// What [`ScreenCapture::screen_state`] reports.
    pub fn state(&self) -> ScreenState {
        self.blocked
            .map_or(ScreenState::Normal, ScreenState::Blocked)
    }
}

/// A display capture session.
///
/// Invariants (each one fixed a real bug on Windows — keep them on every OS):
/// 1. The encoder is sized from [`dimensions`](Self::dimensions), never from an
///    assumed 1920×1080.
/// 2. `acquire` returns `Ok(None)` when nothing changed within `timeout`; the
///    host then sends nothing (adaptive frame rate costs no encode work).
/// 3. [`release`](Self::release) is called on **every** path after a
///    successful `acquire`, including "nothing worth encoding". It is idempotent.
/// 4. After [`CaptureError::Lost`]: `reinit`, then force a keyframe.
pub trait ScreenCapture {
    /// Real pixel size of the captured output (may change after `reinit`).
    fn dimensions(&self) -> (u32, u32);

    /// Wait up to `timeout` for a new frame.
    fn acquire(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>, CaptureError>;

    /// Return the current frame to the OS. Idempotent.
    fn release(&mut self);

    /// Rebuild the session after [`CaptureError::Lost`].
    fn reinit(&mut self) -> Result<(), CaptureError>;

    /// The GPU device frames live on, for zero-copy encoder creation
    /// (`None` for CPU-only backends).
    fn device(&self) -> Option<&dyn Any>;

    /// Ask the backend to deliver frames scaled to fit `size` (the viewer's
    /// display size). Backends whose OS compositor scales for free (macOS SCK,
    /// some portals) honour it; others return `false` and the host scales in the
    /// encoder path as today. Default: unsupported.
    fn request_output_size(&mut self, _size: (u32, u32)) -> bool {
        false
    }

    /// What the screen is right now, as far as this backend can tell. Read by
    /// the host loop after every [`acquire`](Self::acquire) and
    /// [`reinit`](Self::reinit) (and after construction), so it must be cheap
    /// (a field read, no OS call) and say what those calls last found out.
    ///
    /// [`ScreenState::Blocked`] goes with [`CaptureError::Lost`]: a backend
    /// reports it while the screen the person at the computer sees cannot be
    /// captured, and returns to [`ScreenState::Normal`] by itself once a
    /// `reinit` reaches the ordinary desktop again — nothing outside the
    /// backend resets it. Being off the desktop for another reason (a startup
    /// race, a mode change, a disconnected session) is still `Normal`: only a
    /// screen the backend positively knows it cannot reach is `Blocked`.
    ///
    /// Default: [`ScreenState::Normal`] (a backend that cannot tell).
    fn screen_state(&self) -> ScreenState {
        ScreenState::Normal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// plans/10 U1 review: a refusal followed by an attempt that got a
    /// picture is `Normal` — the picture is the proof the screen is
    /// reachable — and only a refused failed attempt is `Blocked`.
    #[test]
    fn the_last_attempt_to_get_a_picture_decides() {
        let mut a = ScreenAccess::default();
        assert_eq!(a.state(), ScreenState::Normal);
        a.failed(Some(BlockReason::NotSupported));
        assert_eq!(a.state(), ScreenState::Blocked(BlockReason::NotSupported));
        // Unlocked between the refused check and the duplication.
        a.reached();
        assert_eq!(a.state(), ScreenState::Normal);
        // A failure that is not a refusal (a mode change) is retried, not
        // reported.
        a.failed(Some(BlockReason::NotSupported));
        a.failed(None);
        assert_eq!(a.state(), ScreenState::Normal);
    }
}
