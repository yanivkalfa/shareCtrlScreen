//! Adapters from the Windows media crates to the `platform-api` traits.

use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::time::Duration;

use platform_api::{
    Autostart, CaptureError, CapturedFrame, Clipboard, Codec, CodecError, CodecFactory,
    CursorShape, CursorSprite, CursorUpdate, EncodedUnit, EncoderConfig, FileDrop, Frame,
    FrameData, InputInjector, KeyForwarder, MoveRect, Point, Presenter, Rect, ScreenCapture,
    SecretStore, ShortcutHook, SurfaceError, VideoDecoder, VideoEncoder, VideoInput, VideoSurface,
};
use protocol::InputMsg;
use windows::Win32::Foundation::{HWND, POINT, RECT};

use crate::gpu::{D3d11Device, D3d11Texture};

// ---- conversions -------------------------------------------------------------

fn to_rect(r: &RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        w: (r.right - r.left).max(0) as u32,
        h: (r.bottom - r.top).max(0) as u32,
    }
}

fn to_point(p: &POINT) -> Point {
    Point { x: p.x, y: p.y }
}

fn to_mf_codec(c: Codec) -> codec::Codec {
    match c {
        Codec::H264 => codec::Codec::H264,
        Codec::Hevc => codec::Codec::Hevc,
        Codec::Av1 => codec::Codec::Av1,
    }
}

fn from_mf_codec(c: codec::Codec) -> Codec {
    match c {
        codec::Codec::H264 => Codec::H264,
        codec::Codec::Hevc => Codec::Hevc,
        codec::Codec::Av1 => Codec::Av1,
    }
}

fn codec_err(e: codec::Error) -> CodecError {
    match e {
        codec::Error::NoEncoder(c) => CodecError::NoEncoder(from_mf_codec(c)),
        codec::Error::NoDecoder(c) => CodecError::NoDecoder(from_mf_codec(c)),
        other => CodecError::Backend(other.to_string()),
    }
}

fn capture_err(e: capture::Error) -> CaptureError {
    match e {
        // Both are recoverable by re-acquiring the duplication, which also
        // re-attaches the capture thread to the active input desktop (see
        // [`DdaCapture::reinit`]). The host media loop recovers from
        // `CaptureError::Lost` and only from it, so a service host that starts
        // (or lands) off the input desktop now retries instead of ending the
        // session — the W8 regression. `AccessDenied` is deliberately NOT mapped
        // to the terminal `SecureDesktop`/`Backend`: a secure-desktop transition
        // and an off-desktop service host are the same recoverable denial.
        capture::Error::AccessLost | capture::Error::AccessDenied => CaptureError::Lost,
        capture::Error::NoOutput => CaptureError::NoOutput,
        capture::Error::Win(e) => CaptureError::Backend(e.to_string()),
    }
}

/// DXGI pointer shape types (`DXGI_OUTDUPL_POINTER_SHAPE_TYPE`).
const SHAPE_MONOCHROME: u32 = 1;
const SHAPE_COLOR: u32 = 2;
const SHAPE_MASKED_COLOR: u32 = 4;

/// Convert a Desktop Duplication pointer shape to straight-alpha BGRA.
///
/// * Color: 32-bpp BGRA as-is.
/// * Masked color: the alpha byte is a mask — 0 replaces the screen pixel
///   (opaque), 0xFF XORs it. XOR pixels can't be reproduced on another screen, so
///   non-black XOR pixels are drawn opaque in their colour with a 1-px
///   contrasting rim (see [`outline_xor`]) and black ones (XOR no-ops)
///   transparent.
/// * Monochrome: two stacked 1-bpp masks (AND above XOR), so the real height is
///   half. AND=1,XOR=0 is transparent; AND=0 is black/white by XOR; AND=1,XOR=1
///   (invert) is drawn black with a 1-px white rim (see [`outline_xor`]).
///
/// ## References
/// * `DXGI_OUTDUPL_POINTER_SHAPE_TYPE`:
///   <https://learn.microsoft.com/windows/win32/api/dxgi1_2/ne-dxgi1_2-dxgi_outdupl_pointer_shape_type>
/// * WebRTC `modules/desktop_capture/win/cursor.cc` (`AddCursorOutline`): the same
///   black-plus-white-outline stand-in for inverting pixels.
pub(crate) fn pointer_shape_to_bgra(
    kind: u32,
    width: u32,
    height: u32,
    pitch: u32,
    data: &[u8],
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, pitch) = (width as usize, pitch as usize);
    match kind {
        SHAPE_COLOR | SHAPE_MASKED_COLOR => {
            let h = height as usize;
            if pitch < w * 4 || data.len() < pitch * h {
                return None;
            }
            let mut out = Vec::with_capacity(w * h * 4);
            let mut xor = vec![false; w * h];
            for y in 0..h {
                for (x, px) in data[y * pitch..y * pitch + w * 4]
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .enumerate()
                {
                    let (b, g, r, a) = (px[0], px[1], px[2], px[3]);
                    let alpha = if kind == SHAPE_COLOR {
                        a
                    } else if a == 0 || (b | g | r) != 0 {
                        255
                    } else {
                        0
                    };
                    xor[y * w + x] = kind == SHAPE_MASKED_COLOR && a == 0xff && (b | g | r) != 0;
                    out.extend_from_slice(&[b, g, r, alpha]);
                }
            }
            if kind == SHAPE_MASKED_COLOR {
                outline_xor(&mut out, &xor, w, h);
            }
            Some((width, height, out))
        }
        SHAPE_MONOCHROME => {
            let h = height as usize / 2;
            if h == 0 || pitch * 8 < w || data.len() < pitch * h * 2 {
                return None;
            }
            let bit = |row: usize, x: usize| (data[row * pitch + x / 8] >> (7 - (x % 8))) & 1;
            let mut out = Vec::with_capacity(w * h * 4);
            let mut inverting = vec![false; w * h];
            for y in 0..h {
                for x in 0..w {
                    let and = bit(y, x);
                    let xor = bit(y + h, x);
                    inverting[y * w + x] = and == 1 && xor == 1;
                    out.extend_from_slice(match (and, xor) {
                        (0, 0) => &[0, 0, 0, 255],
                        (0, _) => &[255, 255, 255, 255],
                        (_, 0) => &[0, 0, 0, 0],
                        _ => &[0, 0, 0, 255],
                    });
                }
            }
            outline_xor(&mut out, &inverting, w, h);
            Some((width, h as u32, out))
        }
        _ => None,
    }
}

/// XOR pixels (monochrome "invert", masked-colour XOR) cannot be reproduced on
/// the viewer, which only blends, so they are drawn in one fixed colour — and
/// one fixed colour vanishes on a background of that colour (the standard
/// Windows I-beam, an inverting monochrome cursor drawn black, on a dark
/// terminal; a masked-colour I-beam with white XOR pixels on a white page).
/// Every transparent pixel touching an XOR pixel (8-neighbourhood) therefore
/// becomes an opaque rim in the contrasting colour: white beside a dark XOR
/// pixel, black beside a light one. A line with a contrasting rim reads on any
/// background. `out` is straight BGRA, `xor` one flag per pixel, both `w * h`
/// row-major; XOR pixels are opaque, so the rim never overwrites one.
fn outline_xor(out: &mut [u8], xor: &[bool], w: usize, h: usize) {
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if out[i * 4 + 3] != 0 {
                continue; // only transparent pixels become rim
            }
            let neighbour = (y.saturating_sub(1)..(y + 2).min(h))
                .flat_map(|ny| (x.saturating_sub(1)..(x + 2).min(w)).map(move |nx| ny * w + nx))
                .find(|&n| xor[n]);
            let Some(n) = neighbour else { continue };
            // BT.601 luma of the XOR pixel decides the rim's colour.
            let (b, g, r) = (
                u32::from(out[n * 4]),
                u32::from(out[n * 4 + 1]),
                u32::from(out[n * 4 + 2]),
            );
            let luma = (299 * r + 587 * g + 114 * b) / 1000;
            let rim = if luma < 128 {
                [255, 255, 255, 255]
            } else {
                [0, 0, 0, 255]
            };
            out[i * 4..i * 4 + 4].copy_from_slice(&rim);
        }
    }
}

// ---- capture -----------------------------------------------------------------

pub struct DdaCapture {
    dup: capture::Duplicator,
    device: D3d11Device,
    current: Option<D3d11Texture>,
    /// Hotspot of the last pointer shape. DDA reports the shape only when it
    /// changes but the position on every move, and the position is the
    /// shape's top-left, so the hotspot must be remembered to convert it.
    hotspot: Point,
    /// Attaches THIS (host-media / capture) thread to the active input desktop
    /// before the duplication is (re)acquired. See [`attach_capture_desktop`].
    desktop: elevation::InputDesktopFollower,
}

impl DdaCapture {
    pub fn new(output: u32) -> Result<Self, CaptureError> {
        // Attach the capture thread to the active input desktop BEFORE creating
        // the duplication (and the shared D3D11 device). A host the SYSTEM
        // service launched with CreateProcessAsUserW is not reliably on that
        // desktop, so DuplicateOutput1 would return E_ACCESSDENIED; a
        // self-launched interactive host inherits it and succeeds. The attach is
        // sticky for this thread, so the shared device the HW H264 MFT is built
        // on (host_media_loop creates the encoder from `cap.device()` on this
        // same thread, right after this) is also created in the right desktop
        // context — restoring the full fast path, not just capture.
        let mut desktop = elevation::InputDesktopFollower::new();
        attach_capture_desktop(&mut desktop);
        let dup = capture::Duplicator::new(0, output).map_err(capture_err)?;
        let device = D3d11Device(dup.device().clone());
        Ok(Self {
            dup,
            device,
            current: None,
            hotspot: Point::default(),
            desktop,
        })
    }
}

/// Re-attach the calling (capture) thread to the active input desktop via
/// `OpenInputDesktop` + `SetThreadDesktop` (through [`elevation::InputDesktopFollower`],
/// the same primitive the input injector uses on ITS thread). `SetThreadDesktop`
/// is per-thread, so this never touches the injector thread (UIAccess control
/// keeps working) or the UI window's thread; it is scoped to the capture thread.
///
/// Logs the desktop it moved onto (so a future lag report says which desktop the
/// capture is reading — `Default`, `Winlogon`, `Screen-saver`), and warns, without
/// failing, when the input desktop cannot be opened (the duplication attempt that
/// follows then reports the denial, which the host loop retries).
///
/// ## References
/// * `SetThreadDesktop` affects only the calling thread and fails if that thread
///   already owns windows or hooks (the capture thread owns neither):
///   <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-setthreaddesktop>
/// * `OpenInputDesktop` returns the desktop currently receiving user input:
///   <https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-openinputdesktop>
fn attach_capture_desktop(desktop: &mut elevation::InputDesktopFollower) {
    match desktop.follow() {
        Ok(true) => tracing::info!(
            "capture: host thread attached to input desktop {:?}",
            desktop.current()
        ),
        // Already on the current input desktop (self-launched host, or no switch
        // since the last attach) — nothing to log.
        Ok(false) => {}
        Err(e) => tracing::warn!(
            "capture: could not attach to the input desktop ({e}); \
             Desktop Duplication may be denied until it is reachable — will retry"
        ),
    }
}

/// DDA's pointer position → the trait's: `DXGI_OUTDUPL_POINTER_POSITION`
/// is where the shape's **top-left** goes (Microsoft's DesktopDuplication
/// sample draws the shape at it unchanged), while `CursorUpdate::position`
/// is the hotspot on every backend. For the arrow (hotspot 0,0) they agree,
/// which is why the difference only showed with the I-beam and resize cursors.
pub(crate) fn dda_hotspot_position(top_left: Point, hotspot: Point) -> Point {
    Point {
        x: top_left.x.saturating_add(hotspot.x),
        y: top_left.y.saturating_add(hotspot.y),
    }
}

impl ScreenCapture for DdaCapture {
    fn dimensions(&self) -> (u32, u32) {
        self.dup.dimensions()
    }

    fn acquire(&mut self, timeout: Duration) -> Result<Option<CapturedFrame<'_>>, CaptureError> {
        self.current = None;
        let Some(f) = self.dup.acquire(timeout).map_err(capture_err)? else {
            return Ok(None);
        };
        let cursor = f.cursor.map(|c| {
            let shape = c.shape.and_then(|s| {
                let i = s.info;
                pointer_shape_to_bgra(i.Type, i.Width, i.Height, i.Pitch, &s.data).map(
                    |(width, height, bgra)| CursorShape {
                        width,
                        height,
                        hotspot: to_point(&i.HotSpot),
                        bgra,
                    },
                )
            });
            // A shape DXGI reported but we could not convert (unknown type,
            // short buffer) leaves the previous hotspot and the viewer's
            // previous image in place: the best guess we have.
            if let Some(s) = &shape {
                self.hotspot = s.hotspot;
            }
            CursorUpdate {
                visible: c.visible,
                position: dda_hotspot_position(to_point(&c.position), self.hotspot),
                shape,
            }
        });
        let dirty = f.dirty_rects.iter().map(to_rect).collect();
        let moves = f
            .move_rects
            .iter()
            .map(|m| MoveRect {
                src: to_point(&m.src),
                dst: to_rect(&m.dst),
            })
            .collect();
        let pointer_only = f.pointer_only;
        let (width, height) = self.dup.dimensions();
        let tex: &D3d11Texture = self.current.insert(D3d11Texture {
            texture: f.texture,
            array_index: 0,
        });
        Ok(Some(CapturedFrame {
            frame: Frame {
                width,
                height,
                data: FrameData::Native(tex),
            },
            dirty,
            moves,
            pointer_only,
            cursor,
        }))
    }

    fn release(&mut self) {
        self.current = None;
        self.dup.release();
    }

    fn reinit(&mut self) -> Result<(), CaptureError> {
        self.current = None;
        // Re-attach to whatever desktop now has input focus before re-acquiring:
        // a `Lost`/`AccessDenied` is often a desktop switch (secure desktop up,
        // lock, fast-user-switch), so the new duplication must be taken on the
        // new input desktop. Logs the move; a no-op switch logs nothing.
        attach_capture_desktop(&mut self.desktop);
        self.dup.reinit().map_err(capture_err)
    }

    fn device(&self) -> Option<&dyn Any> {
        Some(&self.device)
    }
}

// ---- codecs ------------------------------------------------------------------

fn device_of(device: Option<&dyn Any>) -> Result<&D3d11Device, CodecError> {
    device
        .and_then(|d| d.downcast_ref::<D3d11Device>())
        .ok_or_else(|| CodecError::Unsupported("Media Foundation needs a D3D11 device".into()))
}

pub struct MfCodecFactory;

impl CodecFactory for MfCodecFactory {
    fn host_encodable(&self) -> Vec<Codec> {
        codec::encode::host_encodable()
            .into_iter()
            .map(from_mf_codec)
            .collect()
    }

    fn viewer_decodable(&self) -> Vec<Codec> {
        codec::decode::viewer_decodable()
            .into_iter()
            .map(from_mf_codec)
            .collect()
    }

    fn new_encoder(
        &self,
        device: Option<&dyn Any>,
        cfg: EncoderConfig,
    ) -> Result<Box<dyn VideoEncoder>, CodecError> {
        Ok(Box::new(mf_encoder(device_of(device)?, cfg)?))
    }

    fn new_decoder(
        &self,
        device: Option<&dyn Any>,
        codec: Codec,
        width: u32,
        height: u32,
    ) -> Result<Box<dyn VideoDecoder>, CodecError> {
        let dev = device_of(device)?;
        let inner =
            codec::Decoder::new(&dev.0, to_mf_codec(codec), width, height).map_err(codec_err)?;
        Ok(Box::new(MfDecoder { inner, last: None }))
    }
}

/// The Media Foundation encoder behind [`MfCodecFactory::new_encoder`],
/// concrete so tests can ask whether it is the hardware MFT.
pub(crate) fn mf_encoder(dev: &D3d11Device, cfg: EncoderConfig) -> Result<MfEncoder, CodecError> {
    let inner = codec::Encoder::new(
        &dev.0,
        codec::EncoderConfig {
            width: cfg.width,
            height: cfg.height,
            fps_num: cfg.fps_num,
            fps_den: cfg.fps_den,
            bitrate_bps: cfg.bitrate_bps,
            codec: to_mf_codec(cfg.codec),
        },
    )
    .map_err(codec_err)?;
    Ok(MfEncoder { inner })
}

pub(crate) struct MfEncoder {
    inner: codec::Encoder,
}

impl MfEncoder {
    /// False when Media Foundation fell back to Microsoft's software encoder.
    #[cfg(test)]
    pub(crate) fn is_hardware(&self) -> bool {
        self.inner.is_hardware()
    }
}

impl VideoEncoder for MfEncoder {
    fn encode(&mut self, frame: &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError> {
        let FrameData::Native(any) = frame.data else {
            return Err(CodecError::Unsupported(
                "the Media Foundation encoder takes D3D11 frames".into(),
            ));
        };
        let tex = any
            .downcast_ref::<D3d11Texture>()
            .ok_or_else(|| CodecError::Unsupported("not a D3D11 texture".into()))?;
        let units = self.inner.encode(&tex.texture).map_err(codec_err)?;
        Ok(units
            .into_iter()
            .map(|u| EncodedUnit {
                data: u.data,
                keyframe: u.keyframe,
            })
            .collect())
    }

    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        self.inner.set_bitrate(bps).map_err(codec_err)
    }

    fn force_keyframe(&mut self) {
        self.inner.force_keyframe();
    }
}

struct MfDecoder {
    inner: codec::Decoder,
    /// The last decoded surface, lent to the caller until the next `decode`.
    last: Option<D3d11Texture>,
}

impl VideoDecoder for MfDecoder {
    fn decode(
        &mut self,
        au: &[u8],
        keyframe: bool,
        pts_100ns: i64,
    ) -> Result<Option<Frame<'_>>, CodecError> {
        self.last = None;
        let Some(df) = self
            .inner
            .decode(au, keyframe, pts_100ns)
            .map_err(codec_err)?
        else {
            return Ok(None);
        };
        let (width, height) = self.inner.dimensions();
        let tex: &D3d11Texture = self.last.insert(D3d11Texture {
            texture: df.texture,
            array_index: df.array_index,
        });
        Ok(Some(Frame {
            width,
            height,
            data: FrameData::Native(tex),
        }))
    }

    fn dimensions(&self) -> (u32, u32) {
        self.inner.dimensions()
    }
}

// ---- surface + presenter -------------------------------------------------------

/// The D3D11 child window under the Tauri window. The HWND is owned by the UI
/// thread; every operation here is safe from other threads (ShowWindow/MoveWindow
/// marshal to the owning thread), which is why only the raw value is stored.
pub struct ChildWindowSurface {
    pub(crate) hwnd: isize,
}

impl ChildWindowSurface {
    pub fn create(parent_hwnd: isize) -> Result<Self, SurfaceError> {
        let w = render::VideoWindow::create(HWND(parent_hwnd as *mut _))
            .map_err(|e| SurfaceError::Backend(format!("video window: {e}")))?;
        tracing::info!("native video child window created");
        // The window lives until its parent is destroyed (no destroying Drop).
        Ok(Self { hwnd: w.hwnd_raw() })
    }
}

impl VideoSurface for ChildWindowSurface {
    fn show(&self) {
        render::window::show(self.hwnd);
    }
    fn hide(&self) {
        render::window::hide(self.hwnd);
    }
    fn fit(&self) {
        render::window::fit(self.hwnd);
    }
    fn client_size(&self) -> (u32, u32) {
        render::window::client_size(self.hwnd)
    }
    fn set_input_sink(&self, sink: Option<Sender<VideoInput>>) {
        match sink {
            Some(tx) => render::window::set_input_sink(tx),
            None => render::window::clear_input_sink(),
        }
    }
    fn set_drop_sink(&self, sink: Option<Sender<FileDrop>>) {
        match sink {
            Some(tx) => render::window::set_file_drop_sink(tx),
            None => render::window::clear_file_drop_sink(),
        }
    }
    fn create_presenter(&self) -> Result<Box<dyn Presenter>, SurfaceError> {
        let dev = create_render_device()?;
        let renderer = render::Renderer::new(&dev, HWND(self.hwnd as *mut _), 1920, 1080)
            .map_err(|e| SurfaceError::Backend(format!("renderer: {e}")))?;
        Ok(Box::new(D3dPresenter {
            renderer,
            device: D3d11Device(dev),
            upload: crate::cpu::Nv12Upload::default(),
        }))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

struct D3dPresenter {
    renderer: render::Renderer,
    device: D3d11Device,
    /// CPU NV12 frames (the Cisco OpenH264 software decoder) go through this.
    upload: crate::cpu::Nv12Upload,
}

impl Presenter for D3dPresenter {
    fn present(
        &mut self,
        frame: &Frame<'_>,
        cursor: Option<(f64, f64)>,
    ) -> Result<(), SurfaceError> {
        let render = |r: &mut render::Renderer, tex: &_, slice| {
            r.render_frame(tex, slice, cursor)
                .map_err(|e| SurfaceError::Backend(e.to_string()))
        };
        match frame.data {
            FrameData::Native(any) => {
                let tex = any
                    .downcast_ref::<D3d11Texture>()
                    .ok_or_else(|| SurfaceError::Backend("not a D3D11 texture".into()))?;
                render(&mut self.renderer, &tex.texture, tex.array_index)
            }
            FrameData::Cpu { .. } => {
                let tex = self.upload.upload(&self.device.0, frame)?;
                render(&mut self.renderer, &tex, 0)
            }
        }
    }

    fn device(&self) -> Option<&dyn Any> {
        Some(&self.device)
    }

    fn set_cursor_sprite(&mut self, sprite: Option<&CursorSprite>) -> Result<(), SurfaceError> {
        self.renderer
            .set_cursor_sprite(sprite)
            .map_err(|e| SurfaceError::Backend(format!("cursor image: {e}")))
    }
}

/// A standalone hardware D3D11 device for the viewer's decode + present.
pub(crate) fn create_render_device(
) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Device, SurfaceError> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_1};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_SDK_VERSION,
    };
    let mut device = None;
    // SAFETY: standard device creation for video decode + render.
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
        .map_err(|e| SurfaceError::Backend(format!("D3D11 device: {e}")))?;
    }
    device.ok_or_else(|| SurfaceError::Backend("D3D11 device creation returned null".into()))
}

// ---- input -------------------------------------------------------------------

pub struct SendInputInjector {
    inner: input::Injector,
    /// Re-attaches the injecting thread to the current input desktop so input
    /// reaches the secure desktop / UAC prompt when running as the service.
    desktop: elevation::InputDesktopFollower,
}

impl SendInputInjector {
    pub fn new() -> Self {
        Self {
            inner: input::Injector::new(),
            desktop: elevation::InputDesktopFollower::new(),
        }
    }
}

impl InputInjector for SendInputInjector {
    fn set_target_rect(&mut self, rect: Option<Rect>) {
        self.inner.set_target_rect(rect.map(|r| input::TargetRect {
            x: r.x,
            y: r.y,
            w: r.w as i32,
            h: r.h as i32,
        }));
    }

    fn dispatch(&mut self, msg: &InputMsg) {
        let _ = self.desktop.follow();
        self.inner.dispatch(msg);
    }

    fn release_all(&mut self) {
        self.inner.release_all();
    }

    fn focus_window_at(&mut self, nx: f64, ny: f64) -> bool {
        input::focus_window_at(nx, ny)
    }
}

#[derive(Default)]
pub struct LowLevelKeyHook {
    installed: bool,
}

impl ShortcutHook for LowLevelKeyHook {
    fn install(&mut self, surface: &dyn VideoSurface, forward: KeyForwarder) -> bool {
        let Some(s) = surface.as_any().downcast_ref::<ChildWindowSurface>() else {
            return false;
        };
        // Whether the HOST acts on injected Alt+Tab depends on the host process
        // having UIAccess (set by the service that launches it, W5), not on
        // this viewer; the hook is harmless either way (the
        // stuck-key guards hold), so log the fact and install.
        tracing::info!(
            "shortcut capture on (uiaccess={})",
            elevation::process_has_uiaccess()
        );
        input::keyhook::set_focus_root(s.hwnd);
        self.installed = input::keyhook::install(forward);
        self.installed
    }

    fn run(&mut self, stop: &AtomicBool) {
        if self.installed {
            input::keyhook::message_pump(stop);
        }
    }

    fn uninstall(&mut self) {
        if std::mem::take(&mut self.installed) {
            input::keyhook::uninstall();
        }
        input::keyhook::set_focus_root(0);
    }
}

// ---- clipboard, secrets, autostart -----------------------------------------------

pub struct Win32Clipboard;

impl Clipboard for Win32Clipboard {
    fn change_count(&self) -> u64 {
        crate::clipboard::sequence() as u64
    }
    fn get_text(&self) -> Option<String> {
        crate::clipboard::get_text()
    }
    fn set_text(&self, text: &str) -> Option<u64> {
        crate::clipboard::set_text(text).map(u64::from)
    }
    fn set_files(&self, paths: &[PathBuf]) -> Option<u64> {
        crate::clipboard::set_files(paths).map(u64::from)
    }
}

pub struct DpapiSecretStore;

impl SecretStore for DpapiSecretStore {
    fn seal(&self, plain: &str) -> Option<String> {
        crate::secret::protect(plain)
    }
    fn open(&self, sealed: &str) -> Option<String> {
        crate::secret::unprotect(sealed)
    }
}

pub struct RunKeyAutostart;

impl Autostart for RunKeyAutostart {
    fn is_enabled(&self) -> bool {
        crate::autostart::is_enabled()
    }
    fn set_enabled(&self, on: bool) -> Result<(), String> {
        crate::autostart::set_enabled(on)
    }
    fn reconcile(&self, want: bool) {
        crate::autostart::reconcile(want)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_shape_is_copied_honouring_pitch() {
        // 2x1 color cursor with 4 bytes of row padding.
        let data = [1, 2, 3, 200, 4, 5, 6, 0, 9, 9, 9, 9];
        let (w, h, px) = pointer_shape_to_bgra(SHAPE_COLOR, 2, 1, 12, &data).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, vec![1, 2, 3, 200, 4, 5, 6, 0]);
    }

    #[test]
    fn masked_color_makes_xor_black_transparent_and_outline_opaque() {
        // replace pixel (mask 0), xor-black (transparent), xor-black (transparent,
        // touching the next one), xor-white (outline).
        let data = [
            10, 20, 30, 0, 0, 0, 0, 0xff, 0, 0, 0, 0xff, 255, 255, 255, 0xff,
        ];
        let (_, _, px) = pointer_shape_to_bgra(SHAPE_MASKED_COLOR, 4, 1, 16, &data).unwrap();
        assert_eq!(&px[0..4], &[10, 20, 30, 255]);
        assert_eq!(px[7], 0, "XOR black is a no-op: transparent");
        assert_eq!(&px[12..16], &[255, 255, 255, 255]);
        // The transparent pixel beside the light XOR pixel is its dark rim, so
        // a white XOR I-beam still shows on a white page.
        assert_eq!(&px[8..12], &[0, 0, 0, 255]);
    }

    #[test]
    fn dark_xor_pixels_in_masked_cursors_get_a_light_rim() {
        // transparent, XOR dark blue, transparent.
        let data = [0, 0, 0, 0xff, 40, 0, 0, 0xff, 0, 0, 0, 0xff];
        let (_, _, px) = pointer_shape_to_bgra(SHAPE_MASKED_COLOR, 3, 1, 12, &data).unwrap();
        assert_eq!(&px[0..4], &[255, 255, 255, 255]);
        assert_eq!(&px[4..8], &[40, 0, 0, 255]);
        assert_eq!(&px[8..12], &[255, 255, 255, 255]);
    }

    #[test]
    fn monochrome_uses_stacked_and_xor_masks() {
        // 8x1 cursor: AND row then XOR row, 1 byte pitch.
        // px0: and=0,xor=0 black; px1: and=0,xor=1 white; px2: and=1,xor=0 clear;
        // px3: and=1,xor=1 invert → black; px6: clear, far from any invert.
        let and = 0b0011_1111u8;
        let xor = 0b0101_0000u8;
        let (w, h, px) = pointer_shape_to_bgra(SHAPE_MONOCHROME, 8, 2, 1, &[and, xor]).unwrap();
        assert_eq!((w, h), (8, 1));
        assert_eq!(&px[0..4], &[0, 0, 0, 255]);
        assert_eq!(&px[4..8], &[255, 255, 255, 255]);
        assert_eq!(&px[12..16], &[0, 0, 0, 255]);
        assert_eq!(&px[24..28], &[0, 0, 0, 0]);
        // px2 and px4 are clear but touch the inverting px3: white outline.
        assert_eq!(&px[8..12], &[255, 255, 255, 255]);
        assert_eq!(&px[16..20], &[255, 255, 255, 255]);
    }

    #[test]
    fn inverting_i_beam_gets_a_white_rim_so_it_shows_on_dark_backgrounds() {
        // 5x5 monochrome cursor: an inverting vertical line at x = 2, rows 1..=3,
        // everything else transparent (AND=1, XOR=0). 1-byte pitch, AND rows then
        // XOR rows.
        let and = [0b1111_1000u8; 5];
        let mut xor = [0u8; 5];
        for row in &mut xor[1..=3] {
            *row = 0b0010_0000;
        }
        let data: Vec<u8> = and.iter().chain(xor.iter()).copied().collect();
        let (w, h, px) = pointer_shape_to_bgra(SHAPE_MONOCHROME, 5, 10, 1, &data).unwrap();
        assert_eq!((w, h), (5, 5));
        let at = |x: usize, y: usize| &px[(y * 5 + x) * 4..(y * 5 + x) * 4 + 4];
        for y in 1..=3 {
            assert_eq!(at(2, y), &[0, 0, 0, 255], "the line itself is black");
            assert_eq!(at(1, y), &[255, 255, 255, 255], "rim left");
            assert_eq!(at(3, y), &[255, 255, 255, 255], "rim right");
            assert_eq!(at(0, y), &[0, 0, 0, 0], "beyond the rim stays clear");
        }
        assert_eq!(at(2, 0), &[255, 255, 255, 255], "rim above");
        assert_eq!(at(2, 4), &[255, 255, 255, 255], "rim below");
        assert_eq!(at(0, 0), &[0, 0, 0, 0]);
    }

    #[test]
    fn malformed_shapes_are_rejected_not_read_out_of_bounds() {
        assert!(pointer_shape_to_bgra(SHAPE_COLOR, 4, 4, 16, &[0; 10]).is_none());
        assert!(pointer_shape_to_bgra(SHAPE_COLOR, 4, 1, 8, &[0; 16]).is_none());
        assert!(pointer_shape_to_bgra(SHAPE_MONOCHROME, 8, 1, 1, &[0; 2]).is_none());
        assert!(pointer_shape_to_bgra(99, 1, 1, 4, &[0; 4]).is_none());
    }

    #[test]
    fn dda_top_left_becomes_the_hotspot_position() {
        // An I-beam whose image top-left DDA reports at (100, 40), hotspot (4, 8):
        // the pointer acts at (104, 48), which is what the trait reports.
        let p = dda_hotspot_position(Point { x: 100, y: 40 }, Point { x: 4, y: 8 });
        assert_eq!(p, Point { x: 104, y: 48 });
        // The image may hang off the left/top edge (negative top-left).
        let p = dda_hotspot_position(Point { x: -4, y: -8 }, Point { x: 4, y: 8 });
        assert_eq!(p, Point { x: 0, y: 0 });
    }

    #[test]
    fn codec_mapping_roundtrips() {
        for c in [Codec::H264, Codec::Hevc, Codec::Av1] {
            assert_eq!(from_mf_codec(to_mf_codec(c)), c);
        }
    }

    /// W8 error classification: a desktop/access denial is RECOVERABLE (maps to
    /// `CaptureError::Lost`, the one error the host loop retries), while a genuine
    /// backend failure stays terminal. Before W8, `E_ACCESSDENIED` at creation fell
    /// through to `Backend`, which ended the host media loop — the lag regression.
    #[test]
    fn access_denials_are_recoverable_backend_errors_are_terminal() {
        // Off the input desktop / secure-desktop transition → retry.
        assert!(matches!(
            capture_err(capture::Error::AccessDenied),
            CaptureError::Lost
        ));
        // Mode change / display sleep → retry (unchanged).
        assert!(matches!(
            capture_err(capture::Error::AccessLost),
            CaptureError::Lost
        ));
        // A real backend HRESULT stays terminal (not retried forever).
        assert!(matches!(
            capture_err(capture::Error::Win(windows::core::Error::from(
                windows::Win32::Foundation::E_FAIL
            ))),
            CaptureError::Backend(_)
        ));
        assert!(matches!(
            capture_err(capture::Error::NoOutput),
            CaptureError::NoOutput
        ));
    }
}
