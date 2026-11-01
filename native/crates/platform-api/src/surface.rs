//! The viewer's native video surface and its presenter. Contract derived from
//! `render::{VideoWindow, Renderer}` and `render::window`.

use std::any::Any;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use raw_window_handle::{RawDisplayHandle, RawWindowHandle};

use crate::frame::Frame;

/// Input captured on the video surface, normalized over the **letterboxed video
/// rectangle** (not the whole surface), already mapped to DOM codes.
#[derive(Debug, Clone, PartialEq)]
pub enum VideoInput {
    /// Pointer move, `[0,1]`.
    Move { nx: f64, ny: f64 },
    /// Button 0 = left, 1 = middle, 2 = right (DOM `MouseEvent.button`).
    Button {
        button: u8,
        down: bool,
        nx: f64,
        ny: f64,
    },
    /// Wheel in Windows units: one notch = ±120. Positive `dy` = away from user.
    Wheel { dx: i32, dy: i32 },
    /// Key by DOM `KeyboardEvent.code` (physical, layout-independent). Backends
    /// map native key codes with the shared keymap; unmapped keys are dropped.
    Key { code: &'static str, down: bool },
}

/// Files dropped on the surface plus the normalized drop point (the host pastes
/// into the window under that point).
#[derive(Debug, Clone, PartialEq)]
pub struct FileDrop {
    pub paths: Vec<PathBuf>,
    pub nx: f64,
    pub ny: f64,
}

/// Native handles of a [`VideoSurface`], for backends that act on the *window*
/// rather than its pixels: the Linux shortcut hook grabs the keyboard on the
/// video window (X11) or installs a shortcuts inhibitor on the **toplevel**
/// `wl_surface` over the app's `wl_display` (Wayland — a sub-surface never has
/// keyboard focus, so the inhibitor must name the parent). Valid for the
/// surface's lifetime; the hook must not hold them past `uninstall`.
#[derive(Debug, Clone, Copy)]
pub struct SurfaceHandles {
    /// The video surface itself (X11 child window, Wayland video `wl_surface`,
    /// NSView, HWND).
    pub video: RawWindowHandle,
    /// The app window it was created in — what
    /// [`crate::Platform::create_surface`] received. Keyboard focus lives here.
    pub parent: RawWindowHandle,
    /// The display connection both windows belong to.
    pub display: RawDisplayHandle,
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error("unsupported window handle for this backend")]
    UnsupportedHandle,
    #[error("surface backend: {0}")]
    Backend(String),
}

/// The native view inside the app window that shows the remote screen (the
/// AnyDesk layout: video fills the window, the web toolbar slides over it).
///
/// Created on the UI thread by [`crate::Platform::create_surface`]; every other
/// method may be called from any thread (render thread, transport thread).
///
/// Invariants:
/// * Sits **above** the web view when shown, hidden (and not hit-testable) when
///   hidden; hiding never destroys it and never stops the stream.
/// * [`hide`](Self::hide) must take effect before the UI paints a modal (the UI
///   awaits `set_video_visible(false)` then opens the dialog).
/// * **Toolbar reveal is the surface's own job** (no engine/UI call drives it):
///   when the pointer reaches the top 2 px of the surface, remove the top
///   40 CSS px (scaled to physical pixels) from paint and hit-testing so the web
///   toolbar underneath shows and is clickable; restore when the pointer goes
///   deeper than 56 px. Reset on `hide`.
/// * [`client_size`](Self::client_size) is unaffected by the reveal — the host's
///   encode size must not change while the toolbar is shown.
/// * Input is normalized over the letterboxed video rectangle, using the size of
///   the last presented frame (so clicks on black bars clamp or are dropped).
/// * When the surface loses keyboard focus mid-combo, synthesize key-up /
///   button-up for everything held (the real releases land in another window).
/// * Clearing the input sink (`None`) also first emits those releases, so the
///   host never ends up with stuck input.
/// * On X11, [`hide`](Self::hide) **unmaps** the video window and
///   [`show`](Self::show) maps it again. The shortcut hook's keyboard grab on
///   that window ends when the server sees it unviewable, which is what keeps a
///   modal's typing out of the remote session; a hide that merely moved or
///   resized the window would leave the grab in place.
pub trait VideoSurface: Send + Sync {
    fn show(&self);
    fn hide(&self);
    /// Follow the parent's size. Called every ~100 ms from the render loop, so it
    /// must be cheap and a no-op when nothing changed.
    fn fit(&self);
    /// Surface size in physical pixels (what the viewer reports as `ViewSize`).
    fn client_size(&self) -> (u32, u32);
    /// Install or clear the input sink. `None` releases held input first.
    fn set_input_sink(&self, sink: Option<Sender<VideoInput>>);
    /// Install or clear the file-drop sink. `None` refuses drops.
    fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>);
    /// Create the presenter for this surface. Called on the viewer media thread.
    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError>;
    /// Backend escape hatch (e.g. the Windows shortcut hook needs the HWND).
    fn as_any(&self) -> &dyn Any;
    /// Native handles for window-level backends (see [`SurfaceHandles`]).
    /// `None` means this backend does not expose them, and a hook that needs
    /// them reports `install == false` instead of guessing.
    fn native_handles(&self) -> Option<SurfaceHandles> {
        None
    }
}

/// Draws decoded frames onto a [`VideoSurface`]: aspect-preserving letterbox
/// (black bars), NV12 BT.709 limited-range → RGB, then the cursor sprite. One
/// frame of queue depth, no vsync wait when the OS allows it (low latency).
pub trait Presenter {
    /// `cursor` is normalized `[0,1]` over the video; `None` = no sprite.
    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError>;
    /// The GPU device decoders should output to (`None` for CPU presenters).
    fn device(&self) -> Option<&dyn Any>;
}
