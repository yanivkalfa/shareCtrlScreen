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
    ///
    /// `cursor` is the pointer's **hotspot**. With a sprite installed
    /// ([`set_cursor_sprite`](Self::set_cursor_sprite)) the image is drawn at
    /// [`CursorSprite::placement`]; without one the presenter draws its
    /// built-in arrow with the tip on the hotspot.
    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError>;
    /// The GPU device decoders should output to (`None` for CPU presenters).
    fn device(&self) -> Option<&dyn Any>;
    /// Draw `sprite` (the host's real cursor image) at the cursor position from
    /// the next `present` on; `None` returns to the built-in arrow. Called only
    /// when the image changes, so a presenter may upload it to a texture here.
    ///
    /// Default: ignored, so a presenter that cannot draw images keeps the
    /// arrow (the same picture a viewer of an older host gets).
    fn set_cursor_sprite(&mut self, _sprite: Option<&CursorSprite>) -> Result<(), SurfaceError> {
        Ok(())
    }
}

/// Largest cursor image side the viewer accepts and the host sends, in pixels
/// (the wire limit, [`protocol::cursor::MAX_SIDE`]).
pub const MAX_CURSOR_SIDE: u32 = protocol::cursor::MAX_SIDE;

/// The host's cursor image as the viewer draws it.
///
/// Unlike [`crate::CursorShape`] (straight alpha, what capture backends
/// produce) the pixels here are **premultiplied** BGRA: the host converts once,
/// so the wire and every presenter hold one format, and premultiplied is what
/// GPU blending (`ONE, ONE_MINUS_SRC_ALPHA`) and bilinear scaling need to keep
/// anti-aliased edges free of dark fringes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorSprite {
    pub width: u32,
    pub height: u32,
    /// The image pixel that sits on the pointer position, from its top-left.
    pub hotspot: crate::Point,
    /// `width * height * 4` bytes: B, G, R, A, rows top-down, no padding,
    /// premultiplied alpha (every colour byte ≤ its alpha byte).
    pub bgra: Vec<u8>,
    /// Size of the host's captured frame the image's pixels are measured in.
    /// The sprite scales with the video: `width / frame.0` of the picture's
    /// width, whatever size the viewer displays it at (HiDPI on either side
    /// is therefore already accounted for).
    pub frame: (u32, u32),
}

impl CursorSprite {
    /// Whether the fields are consistent and within [`MAX_CURSOR_SIDE`]. A
    /// presenter may assume a sprite it receives from the engine is valid; the
    /// engine checks with this before handing one over.
    pub fn is_valid(&self) -> bool {
        (1..=MAX_CURSOR_SIDE).contains(&self.width)
            && (1..=MAX_CURSOR_SIDE).contains(&self.height)
            && self.frame.0 > 0
            && self.frame.1 > 0
            && self.bgra.len() == self.width as usize * self.height as usize * 4
    }

    /// Where the image goes for a pointer at `cursor` (normalized `[0,1]` over
    /// the video): `[x, y, width, height]` as fractions of the **video
    /// rectangle**, top-left origin. Multiply by the letterboxed video
    /// rectangle's size and add its origin to get presenter pixels.
    ///
    /// The image's top-left is `position - hotspot`; both the image and the
    /// position are in the host's frame pixels, so dividing by the frame size
    /// is the whole HiDPI story. `None` for an invalid sprite.
    pub fn placement(&self, cursor: (f64, f64)) -> Option<[f64; 4]> {
        if !self.is_valid() {
            return None;
        }
        let (fw, fh) = (f64::from(self.frame.0), f64::from(self.frame.1));
        let w = f64::from(self.width) / fw;
        let h = f64::from(self.height) / fh;
        let x = cursor.0.clamp(0.0, 1.0) - f64::from(self.hotspot.x) / fw;
        let y = cursor.1.clamp(0.0, 1.0) - f64::from(self.hotspot.y) / fh;
        Some([x, y, w, h])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Point;

    fn sprite(w: u32, h: u32, hot: (i32, i32), frame: (u32, u32)) -> CursorSprite {
        CursorSprite {
            width: w,
            height: h,
            hotspot: Point { x: hot.0, y: hot.1 },
            bgra: vec![0; (w * h * 4) as usize],
            frame,
        }
    }

    #[test]
    fn placement_puts_the_hotspot_on_the_pointer() {
        // 1000x500 frame, pointer at pixel (500, 250), I-beam hotspot (4, 8).
        let s = sprite(9, 16, (4, 8), (1000, 500));
        let [x, y, w, h] = s.placement((0.5, 0.5)).unwrap();
        assert!((x * 1000.0 - 496.0).abs() < 1e-9);
        assert!((y * 500.0 - 242.0).abs() < 1e-9);
        assert!((w * 1000.0 - 9.0).abs() < 1e-9);
        assert!((h * 500.0 - 16.0).abs() < 1e-9);
    }

    #[test]
    fn placement_scales_with_the_video_not_the_viewer() {
        // A Retina host (2880-px-wide frame) sends a 64-px arrow: it covers
        // 64/2880 of the picture, whatever size the viewer shows it at.
        let s = sprite(64, 64, (0, 0), (2880, 1800));
        let [_, _, w, h] = s.placement((0.0, 0.0)).unwrap();
        // On a 1440-px-wide video rectangle it is 32 px wide.
        assert!((w * 1440.0 - 32.0).abs() < 1e-9);
        assert!((h * 900.0 - 32.0).abs() < 1e-9);
    }

    #[test]
    fn placement_clamps_the_pointer_and_may_hang_off_the_edge() {
        let s = sprite(10, 10, (5, 5), (100, 100));
        let [x, y, _, _] = s.placement((-1.0, 2.0)).unwrap();
        assert!((x - -0.05).abs() < 1e-9);
        assert!((y - 0.95).abs() < 1e-9);
    }

    #[test]
    fn invalid_sprites_have_no_placement() {
        assert!(sprite(0, 4, (0, 0), (10, 10))
            .placement((0.5, 0.5))
            .is_none());
        assert!(sprite(4, 4, (0, 0), (0, 10))
            .placement((0.5, 0.5))
            .is_none());
        assert!(sprite(257, 4, (0, 0), (1000, 10))
            .placement((0.5, 0.5))
            .is_none());
        let mut short = sprite(4, 4, (0, 0), (10, 10));
        short.bgra.pop();
        assert!(!short.is_valid());
    }
}
