//! Screen capture (host side). Contract derived from `capture::Duplicator`
//! (DXGI Desktop Duplication), which every backend must reproduce.

use std::any::Any;
use std::time::Duration;

use crate::frame::{Frame, MoveRect, Point, Rect};

/// Cursor bitmap, always converted to straight-alpha BGRA by the backend (DXGI's
/// monochrome / masked-color shapes included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hotspot: Point,
    pub bgra: Vec<u8>,
}

/// Cursor change since the previous frame. Reported **only when something
/// changed**; between updates the caller keeps the last value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorUpdate {
    pub visible: bool,
    /// Position in captured-frame pixels.
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
}
