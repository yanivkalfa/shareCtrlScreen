//! Native D3D11 video child window (Plan 04 §7, Option A). Tauri's `wry` uses
//! the *windowed* WebView2 controller, so a native child HWND sits as an opaque
//! rectangle over/under the web content ("airspace" limit). We create a child
//! window parented to the Tauri window; the web UI frames it (chrome in the top
//! bar, not over the pixels). The swapchain ([`crate::Renderer`]) is created on
//! this child HWND, not the WebView2 window, so the two never fight for a surface.
//!
//! The window is created on the caller's thread (the app's UI thread, which owns
//! the message pump) so `WM_SIZE`/paint are serviced by the parent's loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetClientRect, LoadCursorW, MoveWindow, PostMessageW,
    RegisterClassW, SetCursor, ShowWindow, CS_HREDRAW, CS_VREDRAW, HMENU, HTCLIENT, IDC_ARROW,
    SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WM_APP, WM_DROPFILES, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SETFOCUS, WM_SYSKEYDOWN,
    WM_SYSKEYUP, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS,
};

const CLASS_NAME: PCWSTR = w!("ShareCtrlVideoWindow");
static REGISTER: std::sync::Once = std::sync::Once::new();

pub use platform_api::{FileDrop, VideoInput};

/// The engine installs a sink while a viewer session with control is active; the
/// wndproc forwards captured input to it. `None` ⇒ input is dropped (view-only
/// or no session), so we never capture when we shouldn't.
static INPUT_SINK: Mutex<Option<Sender<VideoInput>>> = Mutex::new(None);

/// Files dropped onto the video surface, for push-to-host transfer. Installed by
/// the viewer session; `None` ⇒ drops are ignored.
static FILE_DROP_SINK: Mutex<Option<Sender<FileDrop>>> = Mutex::new(None);

/// Start accepting dropped files on the video window and route them to `tx`.
pub fn set_file_drop_sink(tx: Sender<FileDrop>) {
    if let Ok(mut g) = FILE_DROP_SINK.lock() {
        *g = Some(tx);
    }
    let hwnd_raw = crate::window::drop_target_hwnd();
    if hwnd_raw != 0 {
        // SAFETY: valid child HWND owned by this process.
        unsafe {
            windows::Win32::UI::Shell::DragAcceptFiles(HWND(hwnd_raw as *mut _), true);
        }
    }
}

/// Stop accepting dropped files (session end).
pub fn clear_file_drop_sink() {
    if let Ok(mut g) = FILE_DROP_SINK.lock() {
        *g = None;
    }
    let hwnd_raw = drop_target_hwnd();
    if hwnd_raw != 0 {
        // SAFETY: valid child HWND owned by this process.
        unsafe {
            windows::Win32::UI::Shell::DragAcceptFiles(HWND(hwnd_raw as *mut _), false);
        }
    }
}

/// The video window that accepts drops — remembered at creation so the engine
/// can enable drag-and-drop without threading the HWND through.
static DROP_TARGET: AtomicU64 = AtomicU64::new(0);

fn drop_target_hwnd() -> isize {
    DROP_TARGET.load(Ordering::Relaxed) as isize
}

/// Pull the dropped paths out of a `WM_DROPFILES` handle and hand them to the
/// sink. Always frees the drop handle, including when there is no sink.
fn deliver_dropped_files(hwnd: HWND, wparam: WPARAM) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Shell::{DragFinish, DragQueryFileW, DragQueryPoint, HDROP};
    let hdrop = HDROP(wparam.0 as *mut _);
    let mut paths = Vec::new();
    // Where in the video the user let go — this is what lets the host paste into
    // the window the file was aimed at, instead of just filing it away.
    let mut pt = POINT::default();
    // SAFETY: hdrop is valid until DragFinish; pt is an out-param.
    let drop_at = unsafe {
        let _ = DragQueryPoint(hdrop, &mut pt);
        normalize_client(hwnd, pt.x as f64, pt.y as f64)
    };
    // SAFETY: hdrop comes straight from WM_DROPFILES and is freed below.
    unsafe {
        // Passing u32::MAX as the index asks for the count.
        let count = DragQueryFileW(hdrop, u32::MAX, None);
        for i in 0..count {
            let needed = DragQueryFileW(hdrop, i, None) as usize;
            if needed == 0 {
                continue;
            }
            let mut buf = vec![0u16; needed + 1];
            let written = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
            if written > 0 {
                paths.push(std::path::PathBuf::from(String::from_utf16_lossy(
                    &buf[..written],
                )));
            }
        }
        DragFinish(hdrop);
    }
    if paths.is_empty() {
        return;
    }
    if let Ok(guard) = FILE_DROP_SINK.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(FileDrop {
                paths,
                nx: drop_at.0,
                ny: drop_at.1,
            });
        }
    }
}

/// The rendered video's pixel size (`w<<32 | h`), published by the renderer each
/// frame. Input normalization MUST use it: the video is drawn in an
/// aspect-preserving letterbox rect inside the window, so a click's position has
/// to be normalized over that rect — not the whole window — or it lands offset by
/// the black-bar size (the "clicks 1cm off" bug). 0 ⇒ not yet known (full window).
static VIDEO_DIMS: AtomicU64 = AtomicU64::new(0);

/// Renderer publishes the current video size so input maps to the letterbox rect.
pub fn set_video_size(w: u32, h: u32) {
    VIDEO_DIMS.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
}

fn video_dims() -> (f64, f64) {
    let v = VIDEO_DIMS.load(Ordering::Relaxed);
    ((v >> 32) as u32 as f64, (v & 0xffff_ffff) as u32 as f64)
}

/// Map a client-area pixel position to a normalized `[0,1]` position over the
/// LETTERBOX rect — the same math the renderer uses to place the video, so a
/// click (or a file drop) lands where the cursor actually is on the remote,
/// rather than offset by the black bars.
fn normalize_client(hwnd: HWND, px: f64, py: f64) -> (f64, f64) {
    let mut rc = RECT::default();
    // SAFETY: valid HWND.
    let _ = unsafe { GetClientRect(hwnd, &mut rc) };
    let cw = (rc.right - rc.left).max(1) as f64;
    let ch = (rc.bottom - rc.top).max(1) as f64;
    let (vw, vh) = video_dims();
    if vw <= 0.0 || vh <= 0.0 {
        return ((px / cw).clamp(0.0, 1.0), (py / ch).clamp(0.0, 1.0));
    }
    // Same letterbox math as the renderer: scale to fit, center.
    let scale = (cw / vw).min(ch / vh);
    let (dispw, disph) = (vw * scale, vh * scale);
    let offx = (cw - dispw) * 0.5;
    let offy = (ch - disph) * 0.5;
    (
        ((px - offx) / dispw).clamp(0.0, 1.0),
        ((py - offy) / disph).clamp(0.0, 1.0),
    )
}

/// Hover-reveal state for the session menu bar (§7): the native video surface
/// covers the whole client area *including* the web UI's session bar, so pushing
/// the cursor to the top edge reveals the bar (with its Disconnect/settings
/// buttons), and moving back into the video hides it again.
///
/// The bar OVERLAYS the video: we clip the top strip out of the video window with
/// a window region rather than moving/resizing the window. The window rect is
/// therefore constant, which matters twice over — the picture doesn't jump, and
/// the viewer's reported display size (which drives the host's encode resolution)
/// doesn't change every time the cursor brushes the top edge.
/// 0 = bar hidden (video unclipped); >0 = top strip clipped by that many px.
static REVEAL_OFFSET: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
/// Session menu bar height in CSS px (the web `.view-bar`); scaled by DPI.
const BAR_CSS_PX: i32 = 40;
/// Cursor deeper than this many physical px into the video re-hides the bar.
const UNREVEAL_DEPTH_PX: i32 = 56;

/// The bar height in physical pixels for this window's monitor DPI.
fn bar_phys(hwnd: HWND) -> i32 {
    // SAFETY: valid HWND; returns 96 on failure paths.
    let dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { 96 } else { dpi as i32 };
    BAR_CSS_PX * dpi / 96
}

/// Reveal the web session bar over the video, or hide it again.
fn set_reveal(hwnd: HWND, reveal: bool) {
    use std::sync::atomic::Ordering;
    let bar = if reveal { bar_phys(hwnd) } else { 0 };
    if REVEAL_OFFSET.swap(bar, Ordering::SeqCst) == bar {
        return; // no change
    }
    apply_reveal_clip(hwnd, bar);
}

/// Clip the top `bar` physical pixels out of the video window so the WebView2
/// sibling underneath shows through there — the bar appears ON TOP of the video
/// without the video moving or resizing. `bar == 0` restores the full window.
///
/// Mouse messages over a clipped area go to the window below, so the bar's
/// buttons are clickable; moving back down into the video re-hides it
/// (`UNREVEAL_DEPTH_PX` is deeper than the bar, so the video still sees it).
fn apply_reveal_clip(hwnd: HWND, bar: i32) {
    use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, SetWindowRgn, HGDIOBJ};
    // SAFETY: valid child HWND. On success the window OWNS the region and must
    // not be deleted by us; on failure we free it ourselves.
    unsafe {
        if bar <= 0 {
            let _ = SetWindowRgn(hwnd, None, true);
            return;
        }
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return;
        }
        let w = (rc.right - rc.left).max(1);
        let h = (rc.bottom - rc.top).max(1);
        let rgn = CreateRectRgn(0, bar.min(h), w, h);
        if SetWindowRgn(hwnd, Some(rgn), true) == 0 {
            let _ = DeleteObject(HGDIOBJ(rgn.0));
        }
    }
}

/// Keys/buttons currently held down inside the video window, so a focus loss
/// (Alt+Tab away, Win key, app switch) can synthesize the matching releases.
/// Without this the host receives `Alt down` but never `Alt up` — the classic
/// remote-desktop stuck-modifier: Windows takes the focus mid-combo and the
/// key-up lands in some other window, never reaching our wndproc.
static HELD_KEYS: Mutex<Vec<(u16, bool)>> = Mutex::new(Vec::new());
static HELD_BUTTONS: Mutex<Vec<u8>> = Mutex::new(Vec::new());
/// Last normalized cursor position (for synthesized button releases, and the
/// task-D10 local-pointer prediction).
static LAST_POS: Mutex<(f64, f64)> = Mutex::new((0.5, 0.5));

/// Task D10: whether this viewer currently has control. While set (and the
/// window is focused and the pointer is over the client area) the OS cursor is
/// hidden so only the drawn sprite shows; otherwise the normal arrow shows.
static CURSOR_CONTROL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Task D10: whether the pointer is currently over the video client area, so
/// [`local_pointer`] predicts the sprite only while it is. Maintained by
/// `WM_MOUSEMOVE` / `WM_MOUSELEAVE`.
static POINTER_INSIDE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Task D10: whether the video window currently has the keyboard focus. The
/// hide is gated on it (like the macOS view): an Alt+Tab to another app must
/// bring the OS cursor back over the video even though the pointer is still
/// inside our client area. Maintained by `WM_SETFOCUS` / `WM_KILLFOCUS`.
static FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Whether a `TME_LEAVE` request is outstanding, so one is asked for per entry
/// (Windows cancels leave-tracking after it fires the one `WM_MOUSELEAVE`).
static LEAVE_TRACKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A private message posted from [`set_cursor_control`] so the OS cursor is
/// refreshed on the UI thread at once (a `SetCursor` from another thread would
/// not affect the window's cursor, and `WM_SETCURSOR` only fires on the next
/// mouse move — so a stationary pointer would keep the stale cursor).
const WM_REFRESH_CURSOR: u32 = WM_APP + 1;

/// Task D10: hide the OS cursor over the video while controlling, or restore
/// it. Stores the flag, drops the inside flag when turning off, and asks the
/// UI thread to apply the change now (so a stationary pointer does not keep the
/// old cursor until its next move).
pub fn set_cursor_control(active: bool) {
    CURSOR_CONTROL.store(active, Ordering::Relaxed);
    if !active {
        POINTER_INSIDE.store(false, Ordering::Relaxed);
    }
    let hwnd = drop_target_hwnd();
    if hwnd != 0 {
        // SAFETY: valid child HWND; posting a message is safe across threads.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(hwnd as *mut _)),
                WM_REFRESH_CURSOR,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// Task D10: the local pointer over the video (normalized over the letterbox,
/// matching the input mapping), or `None` when it is not over the client area.
pub fn local_pointer() -> Option<(f64, f64)> {
    if !POINTER_INSIDE.load(Ordering::Relaxed) {
        return None;
    }
    LAST_POS.lock().ok().map(|p| *p)
}

/// Whether the OS cursor should be hidden right now: controlling, focused, and
/// the pointer over the client area (task D10).
fn cursor_should_hide() -> bool {
    POINTER_INSIDE.load(Ordering::Relaxed)
        && CURSOR_CONTROL.load(Ordering::Relaxed)
        && FOCUSED.load(Ordering::Relaxed)
}

/// Task D11: re-evaluate whether the OS pointer is physically over the video's
/// client area and, if so, mark it inside and refresh the predicted position.
///
/// `POINTER_INSIDE` is maintained by `WM_MOUSEMOVE` / `WM_MOUSELEAVE`, so after
/// a focus change or a control toggle with no mouse move since, it can be stuck
/// `false` even though the pointer sits over the video — which drops the drawn
/// sprite to the stale host position until the next move. Called on focus
/// regain and when control turns on so the sprite tracks the local pointer at
/// once. Only ever SETS inside (never clears it): leaving is `WM_MOUSELEAVE`'s job.
fn reevaluate_pointer_inside(hwnd: HWND) {
    use windows::Win32::Foundation::POINT;
    // SAFETY: valid child HWND; GetCursorPos/ScreenToClient/GetClientRect read only.
    unsafe {
        let mut pt = POINT::default();
        if windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt).is_err() {
            return;
        }
        // Screen → this window's client coordinates.
        if !windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut pt).as_bool() {
            return;
        }
        let mut rc = RECT::default();
        if GetClientRect(hwnd, &mut rc).is_err() {
            return;
        }
        // Over the revealed toolbar strip the pointer belongs to the web view
        // (its mouse messages go to the sibling), so treat that as outside.
        let bar = REVEAL_OFFSET.load(Ordering::SeqCst);
        let inside =
            pt.x >= rc.left && pt.x < rc.right && pt.y >= bar.max(rc.top) && pt.y < rc.bottom;
        if !inside {
            return;
        }
        let (nx, ny) = normalize_client(hwnd, pt.x as f64, pt.y as f64);
        if let Ok(mut p) = LAST_POS.lock() {
            *p = (nx, ny);
        }
        POINTER_INSIDE.store(true, Ordering::Relaxed);
        // Ask for one WM_MOUSELEAVE so we still learn when it next leaves,
        // exactly as WM_MOUSEMOVE does on entry.
        if !LEAVE_TRACKED.swap(true, Ordering::Relaxed) {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            if TrackMouseEvent(&mut tme).is_err() {
                LEAVE_TRACKED.store(false, Ordering::Relaxed);
            }
        }
    }
}

/// Apply the OS cursor: hidden (null) or the standard arrow. Call only on the
/// UI thread (a wndproc handler), where `SetCursor` affects our window.
fn apply_os_cursor(hidden: bool) {
    // SAFETY: `SetCursor`/`LoadCursorW` on the UI thread; a null cursor hides
    // it, the system arrow restores it. `IDC_ARROW` is a built-in resource.
    unsafe {
        if hidden {
            let _ = SetCursor(None);
        } else if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
            let _ = SetCursor(Some(arrow));
        }
    }
}

/// Synthesize key-up / button-up for everything currently held (focus loss or
/// session end) so the host never keeps a stuck modifier.
fn release_all_held() {
    let keys: Vec<(u16, bool)> = HELD_KEYS
        .lock()
        .map(|mut v| v.drain(..).collect())
        .unwrap_or_default();
    let buttons: Vec<u8> = HELD_BUTTONS
        .lock()
        .map(|mut v| v.drain(..).collect())
        .unwrap_or_default();
    if keys.is_empty() && buttons.is_empty() {
        return;
    }
    let (nx, ny) = LAST_POS.lock().map(|p| *p).unwrap_or((0.5, 0.5));
    tracing::debug!(
        "video window: focus lost — releasing {} held key(s), {} button(s)",
        keys.len(),
        buttons.len()
    );
    for (scancode, extended) in keys {
        if let Some(code) = input::scancode::code_for(scancode, extended) {
            emit(VideoInput::Key { code, down: false });
        }
    }
    for button in buttons {
        emit(VideoInput::Button {
            button,
            down: false,
            nx,
            ny,
        });
    }
}

/// Install the input sink (viewer session start).
pub fn set_input_sink(tx: Sender<VideoInput>) {
    if let Ok(mut g) = INPUT_SINK.lock() {
        *g = Some(tx);
    }
}

/// Remove the input sink (session end / view-only).
pub fn clear_input_sink() {
    // Release everything still held BEFORE dropping the sink, so the host never
    // keeps a key or button down after the session (or control) ends. This also
    // empties the held state, so nothing leaks into the next session.
    release_all_held();
    if let Ok(mut g) = INPUT_SINK.lock() {
        *g = None;
    }
}

fn emit(ev: VideoInput) {
    if let Ok(guard) = INPUT_SINK.lock() {
        if let Some(tx) = guard.as_ref() {
            let _ = tx.send(ev);
        }
    }
}

/// A child window used purely as the D3D11 present target.
pub struct VideoWindow {
    hwnd: HWND,
    parent: HWND,
}

// The HWND is only touched from the UI thread, but we send the raw value across
// threads to the render thread as an `isize` (see engine), not this struct.
impl VideoWindow {
    /// Create a hidden child window filling the parent's client area.
    pub fn create(parent: HWND) -> Result<Self, windows::core::Error> {
        register_class();
        // SAFETY: valid parent HWND; class registered above.
        let hwnd = unsafe {
            let hinstance = GetModuleHandleW(None)?;
            let mut rc = RECT::default();
            let _ = GetClientRect(parent, &mut rc);
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS_NAME,
                PCWSTR::null(),
                WS_CHILD | WS_CLIPSIBLINGS,
                0,
                0,
                (rc.right - rc.left).max(1),
                (rc.bottom - rc.top).max(1),
                Some(parent),
                None::<HMENU>,
                Some(hinstance.into()),
                None,
            )?
        };
        // Remember the surface so the engine can toggle drag-and-drop on it.
        DROP_TARGET.store(hwnd.0 as u64, Ordering::Relaxed);
        Ok(Self { hwnd, parent })
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Raw handle value to hand across threads (to the render thread).
    pub fn hwnd_raw(&self) -> isize {
        self.hwnd.0 as isize
    }

    /// Keep a reference to the parent alive in the struct (used for fit).
    pub fn parent(&self) -> HWND {
        self.parent
    }
}

/// Show the video surface and size it to the parent's client area (session
/// start). Safe to call from the render thread — `ShowWindow`/`MoveWindow`
/// marshal to the owning thread.
///
/// The child is raised to the TOP of the sibling z-order: the WebView2 child
/// (created by Tauri *after* this window) otherwise sits above it, and the video
/// presents perfectly — invisibly — behind the web page ("airspace", §7).
pub fn show(hwnd_raw: isize) {
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, HWND_TOP, SWP_SHOWWINDOW};
    let hwnd = HWND(hwnd_raw as *mut _);
    REVEAL_OFFSET.store(0, std::sync::atomic::Ordering::SeqCst);
    // SAFETY: valid child HWND created by `VideoWindow::create`.
    unsafe {
        let parent = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd);
        let mut w = 1;
        let mut h = 1;
        if let Ok(parent) = parent {
            let mut rc = RECT::default();
            if GetClientRect(parent, &mut rc).is_ok() {
                w = (rc.right - rc.left).max(1);
                h = (rc.bottom - rc.top).max(1);
            }
        }
        tracing::info!("video window: show {w}x{h} (raise to top)");
        let _ = MoveWindow(hwnd, 0, 0, w, h, true);
        apply_reveal_clip(hwnd, 0); // drop any clip left over from a past session
                                    // Raise above the WebView2 sibling AND show in one call.
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), 0, 0, w, h, SWP_SHOWWINDOW);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

/// Keep the video child sized to the parent's client area (minus any hover
/// reveal offset). Called periodically from the render loop so mid-session
/// window resizes track without a WM_SIZE hook into the Tauri parent; no-ops
/// when the size already matches.
pub fn fit(hwnd_raw: isize) {
    use std::sync::atomic::Ordering;
    let hwnd = HWND(hwnd_raw as *mut _);
    // SAFETY: valid child HWND; reads two client rects and conditionally moves.
    unsafe {
        let Ok(parent) = windows::Win32::UI::WindowsAndMessaging::GetParent(hwnd) else {
            return;
        };
        let mut prc = RECT::default();
        let mut crc = RECT::default();
        if GetClientRect(parent, &mut prc).is_err() || GetClientRect(hwnd, &mut crc).is_err() {
            return;
        }
        // Always the FULL client area — the revealed bar is a clip, not an offset.
        let want_w = (prc.right - prc.left).max(1);
        let want_h = (prc.bottom - prc.top).max(1);
        let cur_w = crc.right - crc.left;
        let cur_h = crc.bottom - crc.top;
        if cur_w != want_w || cur_h != want_h {
            let _ = MoveWindow(hwnd, 0, 0, want_w, want_h, true);
            // A resize invalidates the old region — re-clip if the bar is up.
            let offset = REVEAL_OFFSET.load(Ordering::SeqCst);
            if offset > 0 {
                apply_reveal_clip(hwnd, offset);
            }
        }
    }
}

/// The video child window's current client size in pixels. The viewer reports
/// this to the host so the host can encode at exactly this size — presenting 1:1
/// instead of resampling every frame on display.
pub fn client_size(hwnd_raw: isize) -> (u32, u32) {
    let hwnd = HWND(hwnd_raw as *mut _);
    let mut rc = RECT::default();
    // SAFETY: valid child HWND; GetClientRect only fills the rect.
    unsafe {
        if GetClientRect(hwnd, &mut rc).is_err() {
            return (0, 0);
        }
    }
    (
        (rc.right - rc.left).max(0) as u32,
        (rc.bottom - rc.top).max(0) as u32,
    )
}

/// Hide the video surface (back to the home screen / settings overlay).
pub fn hide(hwnd_raw: isize) {
    let hwnd = HWND(hwnd_raw as *mut _);
    REVEAL_OFFSET.store(0, std::sync::atomic::Ordering::SeqCst);
    VIDEO_DIMS.store(0, Ordering::Relaxed); // avoid stale mapping next session
                                            // Stop predicting the cursor from a stale position once the surface is gone
                                            // (task D10). `set_cursor_control(false)` from teardown has usually run
                                            // already; this covers the link-trouble hide too.
    POINTER_INSIDE.store(false, Ordering::Relaxed);
    LEAVE_TRACKED.store(false, Ordering::Relaxed);
    FOCUSED.store(false, Ordering::Relaxed);

    // Clip the surface to NOTHING as well as hiding it.
    //
    // `ShowWindow` is called from whatever thread the UI command lands on, while
    // this window belongs to the UI thread, so it can be deferred. If the bar was
    // revealed when the user opened Settings, a deferred hide left the clip in
    // place — and the clip is honoured immediately — so the web page was visible
    // only through the 40px bar strip and the Settings dialog appeared sliced.
    // An empty region is applied by the same mechanism that reveals the bar, so
    // whatever happens to ShowWindow the surface covers nothing.
    clip_to_nothing(hwnd);

    // SAFETY: valid child HWND.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

/// Reduce the window's visible region to an empty rect — it stops painting over
/// anything without depending on `ShowWindow` having been processed yet.
fn clip_to_nothing(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{CreateRectRgn, DeleteObject, SetWindowRgn, HGDIOBJ};
    // SAFETY: on success the window owns the region; on failure we free it.
    unsafe {
        let rgn = CreateRectRgn(0, 0, 0, 0);
        if SetWindowRgn(hwnd, Some(rgn), true) == 0 {
            let _ = DeleteObject(HGDIOBJ(rgn.0));
        }
    }
}

fn register_class() {
    REGISTER.call_once(|| {
        // SAFETY: one-time class registration with a static class name.
        unsafe {
            let hinstance = GetModuleHandleW(None).unwrap_or_default();
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance.into(),
                lpszClassName: CLASS_NAME,
                hbrBackground: HBRUSH(std::ptr::null_mut()),
                ..Default::default()
            };
            RegisterClassW(&wc);
        }
    });
}

// The present loop drives D3D; the wndproc captures input over the video area
// (§7) and forwards it to the engine, which relays it to the host. Capture is
// gated by the sink being installed — only during a viewer session with control.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Normalized cursor position from lParam (signed 16-bit lo/hi), mapped over
    // the LETTERBOX rect (must match the renderer's letterbox exactly), so clicks
    // land where the cursor is on the remote — not offset by the black bars.
    let norm_pos = |lp: LPARAM| -> (f64, f64) {
        let px = (lp.0 & 0xffff) as i16 as f64;
        let py = ((lp.0 >> 16) & 0xffff) as i16 as f64;
        normalize_client(hwnd, px, py)
    };

    match msg {
        WM_MOUSEMOVE => {
            // Hover-reveal of the session menu bar (§7): top edge → slide the
            // video down to expose the web bar; deep re-entry → slide back up.
            {
                use std::sync::atomic::Ordering;
                let y_phys = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
                let revealed = REVEAL_OFFSET.load(Ordering::SeqCst) > 0;
                if !revealed && y_phys <= 2 {
                    set_reveal(hwnd, true);
                } else if revealed && y_phys > UNREVEAL_DEPTH_PX {
                    set_reveal(hwnd, false);
                }
            }
            let (nx, ny) = norm_pos(lparam);
            if let Ok(mut p) = LAST_POS.lock() {
                *p = (nx, ny);
            }
            // Task D10: the pointer is over the video — predict the sprite here,
            // and ask for one WM_MOUSELEAVE so we learn when it leaves.
            POINTER_INSIDE.store(true, Ordering::Relaxed);
            if !LEAVE_TRACKED.swap(true, Ordering::Relaxed) {
                let mut tme = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: hwnd,
                    dwHoverTime: 0,
                };
                // SAFETY: `tme` is fully initialized; `hwnd` is our window.
                if unsafe { TrackMouseEvent(&mut tme) }.is_err() {
                    LEAVE_TRACKED.store(false, Ordering::Relaxed);
                }
            }
            emit(VideoInput::Move { nx, ny });
            return LRESULT(0);
        }
        // Task D10: the pointer left the video — stop predicting (the sprite
        // falls back to the host's out-of-band position) and let the window
        // under the pointer set its own cursor.
        WM_MOUSELEAVE => {
            POINTER_INSIDE.store(false, Ordering::Relaxed);
            LEAVE_TRACKED.store(false, Ordering::Relaxed);
            return LRESULT(0);
        }
        // Task D10: hide the OS cursor while controlling and the pointer is over
        // the client area, so only the drawn sprite shows. Over the revealed
        // toolbar strip the message goes to the web view instead, which sets its
        // own cursor — so the toolbar is never left cursorless.
        WM_SETCURSOR => {
            let hit = (lparam.0 & 0xffff) as u32;
            if hit == HTCLIENT {
                // HTCLIENT means the pointer is over our client area; hide only
                // while focused (an unfocused window must show the arrow even
                // with the pointer inside, matching the macOS view). Returning
                // TRUE stops Windows from setting the class cursor afterwards.
                let hide =
                    CURSOR_CONTROL.load(Ordering::Relaxed) && FOCUSED.load(Ordering::Relaxed);
                apply_os_cursor(hide);
                return LRESULT(1);
            }
            // Non-client area (resize borders): let Windows set that cursor.
        }
        // Task D10: refresh the cursor now that control / inside / focus
        // changed, without waiting for the next mouse move.
        WM_REFRESH_CURSOR => {
            // Task D11: control may have just turned ON while the pointer was
            // already sitting over the video (no move since), so re-evaluate
            // `POINTER_INSIDE` here or the sprite would stay at the host spot.
            if CURSOR_CONTROL.load(Ordering::Relaxed) {
                reevaluate_pointer_inside(hwnd);
            }
            apply_os_cursor(cursor_should_hide());
            return LRESULT(0);
        }
        // Task D10: regained focus — re-hide if still controlling over the video.
        WM_SETFOCUS => {
            FOCUSED.store(true, Ordering::Relaxed);
            // Task D11: on focus regain the pointer may already be over the video
            // with no move since the focus loss, leaving `POINTER_INSIDE` stuck
            // `false`; re-evaluate so the sprite doesn't drop to the host spot.
            reevaluate_pointer_inside(hwnd);
            apply_os_cursor(cursor_should_hide());
            return LRESULT(0);
        }
        WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            // SAFETY: take keyboard focus so key events flow to this window.
            let _ = unsafe { SetFocus(Some(hwnd)) };
            let (nx, ny) = norm_pos(lparam);
            let button = match msg {
                WM_LBUTTONDOWN => 0,
                WM_MBUTTONDOWN => 1,
                _ => 2,
            };
            if let Ok(mut held) = HELD_BUTTONS.lock() {
                if !held.contains(&button) {
                    held.push(button);
                }
            }
            emit(VideoInput::Button {
                button,
                down: true,
                nx,
                ny,
            });
            return LRESULT(0);
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let (nx, ny) = norm_pos(lparam);
            let button = match msg {
                WM_LBUTTONUP => 0,
                WM_MBUTTONUP => 1,
                _ => 2,
            };
            if let Ok(mut held) = HELD_BUTTONS.lock() {
                held.retain(|&b| b != button);
            }
            emit(VideoInput::Button {
                button,
                down: false,
                nx,
                ny,
            });
            return LRESULT(0);
        }
        WM_MOUSEWHEEL => {
            let dy = ((wparam.0 >> 16) & 0xffff) as i16 as i32;
            emit(VideoInput::Wheel { dx: 0, dy });
            return LRESULT(0);
        }
        WM_MOUSEHWHEEL => {
            let dx = ((wparam.0 >> 16) & 0xffff) as i16 as i32;
            emit(VideoInput::Wheel { dx, dy: 0 });
            return LRESULT(0);
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let scancode = ((lparam.0 >> 16) & 0xff) as u16;
            let extended = (lparam.0 & (1 << 24)) != 0;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            if let Ok(mut held) = HELD_KEYS.lock() {
                if down {
                    if !held.contains(&(scancode, extended)) {
                        held.push((scancode, extended));
                    }
                } else {
                    held.retain(|&k| k != (scancode, extended));
                }
            }
            // Unmapped keys (media keys, Pause, Fn) are deliberately not sent.
            if let Some(code) = input::scancode::code_for(scancode, extended) {
                emit(VideoInput::Key { code, down });
            }
            return LRESULT(0);
        }
        // Focus left the video window mid-combo (Alt+Tab away, Win key, app
        // switch): the key-ups will land elsewhere, so synthesize them NOW or
        // the host keeps a stuck modifier.
        WM_KILLFOCUS => {
            release_all_held();
            // Task D10: restore the OS cursor at once — an unfocused video
            // region must never be left cursor-less, even with the pointer
            // still inside it.
            FOCUSED.store(false, Ordering::Relaxed);
            apply_os_cursor(false);
            return LRESULT(0);
        }
        // Files dropped on the video surface: the viewer pushes them to the host.
        WM_DROPFILES => {
            deliver_dropped_files(hwnd, wparam);
            return LRESULT(0);
        }
        _ => {}
    }
    // SAFETY: default handling for everything else.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
