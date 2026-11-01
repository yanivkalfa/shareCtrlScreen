//! VA-API H.264 encoder (feature `vaapi`) for Intel (iHD/i965) and AMD
//! (Mesa radeonsi) GPUs, with libva loaded at run time. The decoder (task
//! L4) is [`decode`]; it shares this module's display, node probing and
//! object wrappers.
//!
//! ## Why an in-tree binding, not `cros-libva` / `cros-codecs`
//! Both were evaluated (task L3, first step):
//! * `cros-libva` 0.0.13 generates its bindings with bindgen from the system's
//!   `libva-dev` headers at build time and links `va` and `va-drm`
//!   (`cargo:rustc-link-lib=dylib=…`). That puts a hard `DT_NEEDED` on
//!   `libva.so.2` in the binary — the app would fail to *start* on a machine
//!   without libva, which decision
//!   #7 rules out — and it needs libclang + libva headers on every build host.
//!   A runtime probe cannot wrap a load-time dependency.
//! * `cros-codecs` 0.0.6 has a capable stateless H.264 VA-API encoder (CBR,
//!   low-power entrypoint, forced keyframes, a max-QP tuning), but sits on
//!   `cros-libva` 0.0.12 (same linking), pulls `nix`/`drm`/`gbm` and a
//!   ChromeOS-shaped frame-pool API, and hard-codes rate-control choices this
//!   recipe must change: bit stuffing on (a static desktop would burn the whole
//!   CBR budget on filler), a 1.5 s RC window and no HRD buffer (keyframe
//!   bursts queue on the link).
//!
//! So this is a `dlopen` binding of the ~30 libva calls needed (`ffi`), the
//! same shape as the NVENC one, with the stream headers written in-tree
//! (`super::h264`). `cros-codecs`' `encoder/stateless/h264/vaapi.rs` and
//! Chromium's `VaapiVideoEncodeAccelerator` were the references for the
//! parameter-buffer flow.
//!
//! ## Recipe (mirrors `platform_api::VideoEncoder`)
//! * `VAEntrypointEncSliceLP` (Intel VDEnc / fixed-function) preferred, then
//!   `VAEntrypointEncSlice`; H.264 Main (CABAC) preferred, Constrained
//!   Baseline otherwise.
//! * CBR (`VAConfigAttribRateControl`), live bitrate through a
//!   `VAEncMiscParameterRateControl` on every picture (Chromium does the same;
//!   drivers reset their BRC when the value changes), single-frame
//!   `VAEncMiscParameterHRD`, max QP 32, no frame skipping, no bit stuffing.
//! * Every frame is P except IDRs the engine asks for; one reference, one
//!   slice, `ip_period` 1 (no B-frames).
//! * Input v1: CPU frames uploaded with `vaDeriveImage` (or `vaCreateImage` +
//!   `vaPutImage` where deriving is refused), then the video processor (VPP)
//!   converts BGRA → NV12 BT.709 limited range and scales to the encode size.
//!   An NV12 frame of the right size skips VPP. DMA-BUF import
//!   (`VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2`) is task L3b: it plugs in as
//!   another [`Source`] kind fed from `FrameData::Native`.
//!
//! ## References
//! * libva 2.20 `va.h`, `va_enc_h264.h`, `va_vpp.h`, `va_drm.h` (API docs are
//!   the header comments; intel.github.io/libva).
//! * Intel media-driver `media_driver/linux/common/codec/ddi/media_ddi_encode_avc.cpp`
//!   (packed-header expectations, LP entrypoint) and Mesa
//!   `src/gallium/frontends/va/picture_h264_enc.c` (radeonsi).
//! * FFmpeg `libavcodec/vaapi_encode.c`, `vaapi_encode_h264.c` (buffer order,
//!   HRD window, derive-or-put upload in `hwcontext_vaapi.c`).
//! * ITU-T H.264 (see `super::h264`).

mod decode;
mod ffi;

pub(crate) use decode::{decode_probe, DecodeProbe, VaapiDecoder};

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use platform_api::{
    Codec, CodecError, EncodedUnit, EncoderConfig, Frame, FrameData, PixelFormat, VideoEncoder,
};

use self::ffi::*;
use super::h264::{Profile, SeqHeader, LOG2_MAX_FRAME_NUM};
use super::zeroed::Zeroed;
use super::{annexb, validate, vbv_bits, MAX_QP};

/// Decision #7: libva older than 2.20 (VA-API 1.20) keeps hardware encode off
/// — Ubuntu 22.04's 2.14 included — and the software codec is used instead.
/// `vaInitialize` reports the VA-API version; libva 2.N implements VA-API 1.N.
const MIN_VA_API: (c_int, c_int) = (1, 20);

/// `intra_period` / `intra_idr_period` hint. Every picture type is decided by
/// this encoder (IDR only on request), so these only shape the driver's rate
/// planning. Not 0 — VA-API gives 0 no meaning and drivers divide by it — and
/// within 16 bits, which is how iHD stores it.
const GOP_HINT: u32 = 0xffff;

/// Where the H.264 frame number wraps.
const MAX_FRAME_NUM: u32 = 1 << LOG2_MAX_FRAME_NUM;

pub(crate) fn version_supported(major: c_int, minor: c_int) -> bool {
    (major, minor) >= MIN_VA_API
}

/// The encode entrypoint to use: low-power (fixed-function) first.
pub(crate) fn pick_entrypoint(entrypoints: &[VAEntrypoint]) -> Option<VAEntrypoint> {
    [VA_ENTRYPOINT_ENC_SLICE_LP, VA_ENTRYPOINT_ENC_SLICE]
        .into_iter()
        .find(|e| entrypoints.contains(e))
}

/// The H.264 profiles the driver lists, in preference order.
pub(crate) fn h264_profiles(profiles: &[VAProfile]) -> Vec<(Profile, VAProfile)> {
    [
        (Profile::Main, VA_PROFILE_H264_MAIN),
        (
            Profile::ConstrainedBaseline,
            VA_PROFILE_H264_CONSTRAINED_BASELINE,
        ),
    ]
    .into_iter()
    .filter(|(_, p)| profiles.contains(p))
    .collect()
}

/// `/dev/dri` entries that are render nodes, in card order (`renderD128` …).
pub(crate) fn render_nodes<I: IntoIterator<Item = String>>(names: I) -> Vec<String> {
    let mut nodes: Vec<(u32, String)> = names
        .into_iter()
        .filter_map(|n| Some((n.strip_prefix("renderD")?.parse().ok()?, n)))
        .collect();
    nodes.sort();
    nodes.into_iter().map(|(_, n)| n).collect()
}

/// `VAEncMiscParameterFrameRate::framerate`: numerator in the low 16 bits,
/// denominator in the high 16. Rates that do not fit are halved together
/// (e.g. 120000/1001 → 60000/500), which keeps the ratio to < 0.1%.
pub(crate) fn pack_framerate(num: u32, den: u32) -> u32 {
    let (mut num, mut den) = (num.max(1), den.max(1));
    while num > 0xffff || den > 0xffff {
        num = (num / 2).max(1);
        den = (den / 2).max(1);
    }
    num | (den << 16)
}

/// What the probe found on the first usable render node.
#[derive(Debug, Clone)]
pub(crate) struct Probe {
    pub node: PathBuf,
    pub vendor: String,
    pub version: (c_int, c_int),
    pub profile: Profile,
    pub va_profile: VAProfile,
    pub entrypoint: VAEntrypoint,
    /// `VA_ENC_PACKED_HEADER_SEQUENCE|PICTURE` bits the driver accepts.
    pub packed_headers: u32,
}

impl Probe {
    pub(crate) fn describe(&self) -> String {
        format!(
            "VA-API on {} — {} (VA-API {}.{}; H.264 {:?}, {})",
            self.node.display(),
            self.vendor,
            self.version.0,
            self.version.1,
            self.profile,
            if self.entrypoint == VA_ENTRYPOINT_ENC_SLICE_LP {
                "low-power EncSliceLP"
            } else {
                "EncSlice"
            }
        )
    }
}

/// Probe every render node; the first with a usable H.264 encoder wins.
pub(crate) fn probe() -> Result<Probe, String> {
    first_node(probe_node)
}

/// Run `probe_node` on each DRM render node in card order; the first success
/// wins, otherwise every node's reason is reported.
fn first_node<T>(probe_node: impl Fn(&Arc<Va>, &Path) -> Result<T, String>) -> Result<T, String> {
    let va = Va::shared()?;
    let names = std::fs::read_dir("/dev/dri")
        .map_err(|e| format!("no /dev/dri ({e}) — no GPU driver loaded"))?
        .filter_map(|e| e.ok()?.file_name().into_string().ok());
    let nodes = render_nodes(names);
    if nodes.is_empty() {
        return Err("no DRM render node in /dev/dri".into());
    }
    let mut reasons = Vec::new();
    for name in nodes {
        let node = Path::new("/dev/dri").join(&name);
        match probe_node(&va, &node) {
            Ok(p) => return Ok(p),
            Err(e) => reasons.push(format!("{name}: {e}")),
        }
    }
    Err(reasons.join("; "))
}

fn probe_node(va: &Arc<Va>, node: &Path) -> Result<Probe, String> {
    let display = Display::open(va, node)?;
    let profiles = display.profiles()?;
    let candidates = h264_profiles(&profiles);
    if candidates.is_empty() {
        return Err(format!(
            "{} decodes only — no H.264 encode profile",
            display.vendor()
        ));
    }
    for (profile, va_profile) in candidates {
        let Some(entrypoint) = pick_entrypoint(&display.entrypoints(va_profile)?) else {
            continue;
        };
        let attrs = display.attributes(
            va_profile,
            entrypoint,
            &[
                VA_CONFIG_ATTRIB_RT_FORMAT,
                VA_CONFIG_ATTRIB_RATE_CONTROL,
                VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
            ],
        )?;
        let supported = |v: u32| v != VA_ATTRIB_NOT_SUPPORTED;
        if !(supported(attrs[0]) && attrs[0] & VA_RT_FORMAT_YUV420 != 0) {
            continue;
        }
        if !(supported(attrs[1]) && attrs[1] & VA_RC_CBR != 0) {
            continue;
        }
        let packed_headers = if supported(attrs[2]) {
            attrs[2] & (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE)
        } else {
            0
        };
        return Ok(Probe {
            node: node.to_path_buf(),
            vendor: display.vendor(),
            version: display.version,
            profile,
            va_profile,
            entrypoint,
            packed_headers,
        });
    }
    Err(format!(
        "{} has no H.264 encode entrypoint with 4:2:0 and CBR",
        display.vendor()
    ))
}

unsafe extern "C" fn log_error(_ctx: *mut c_void, message: *const c_char) {
    if !message.is_null() {
        // SAFETY: libva passes a NUL-terminated message valid for the call.
        let m = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        tracing::warn!("libva: {}", m.trim_end());
    }
}

unsafe extern "C" fn log_info(_ctx: *mut c_void, message: *const c_char) {
    if !message.is_null() {
        // SAFETY: as above.
        let m = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        tracing::debug!("libva: {}", m.trim_end());
    }
}

/// An initialised VA display on one DRM render node; terminated on drop.
struct Display {
    va: Arc<Va>,
    dpy: VADisplay,
    version: (c_int, c_int),
    /// Must outlive `dpy` (libva does not dup it).
    _fd: File,
}

impl Display {
    fn open(va: &Arc<Va>, node: &Path) -> Result<Self, String> {
        let fd = File::options()
            .read(true)
            .write(true)
            .open(node)
            .map_err(|e| format!("cannot open ({e}) — is the user in the `render` group?"))?;
        // SAFETY: valid DRM fd, kept open for the display's lifetime.
        let dpy = unsafe { (va.get_display_drm)(fd.as_raw_fd()) };
        if dpy.is_null() {
            return Err("vaGetDisplayDRM returned no display".into());
        }
        // Route libva's own messages (it prints driver load info to stderr by
        // default) into our log. Set before vaInitialize, which emits them.
        // SAFETY: valid display; the callbacks are 'static functions.
        unsafe {
            (va.set_error_callback)(dpy, Some(log_error), std::ptr::null_mut());
            (va.set_info_callback)(dpy, Some(log_info), std::ptr::null_mut());
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: valid display; out-pointers to locals.
        let status = unsafe { (va.initialize)(dpy, &mut major, &mut minor) };
        // Built before checking `status` so that drop terminates the display
        // either way: vaInitialize can fail after loading a driver.
        let display = Self {
            va: va.clone(),
            dpy,
            version: (major, minor),
            _fd: fd,
        };
        if status != VA_STATUS_SUCCESS {
            return Err(display.error(status, "vaInitialize (no VA-API driver for this GPU?)"));
        }
        if !version_supported(major, minor) {
            return Err(format!(
                "libva 2.{minor} (VA-API {major}.{minor}) is older than 2.20 — hardware video \
                 stays off on this version (decision #7); the software codec is used"
            ));
        }
        Ok(display)
    }

    fn error(&self, status: VAStatus, what: &str) -> String {
        // SAFETY: vaErrorStr returns a static NUL-terminated string.
        let text = unsafe { CStr::from_ptr((self.va.error_str)(status)) }.to_string_lossy();
        format!("{what}: {text} ({status:#x})")
    }

    fn check(&self, status: VAStatus, what: &str) -> Result<(), String> {
        if status == VA_STATUS_SUCCESS {
            Ok(())
        } else {
            Err(self.error(status, what))
        }
    }

    fn vendor(&self) -> String {
        // SAFETY: valid, initialised display; libva owns the string.
        let s = unsafe { (self.va.query_vendor_string)(self.dpy) };
        if s.is_null() {
            return "unknown VA-API driver".into();
        }
        // SAFETY: non-null, NUL-terminated, valid while the display lives.
        unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned()
    }

    fn profiles(&self) -> Result<Vec<VAProfile>, String> {
        // SAFETY: valid display.
        let max = unsafe { (self.va.max_num_profiles)(self.dpy) }.max(0);
        let mut out = vec![0; max as usize];
        let mut n = 0;
        // SAFETY: buffer of vaMaxNumProfiles entries, as the API requires.
        self.check(
            unsafe { (self.va.query_config_profiles)(self.dpy, out.as_mut_ptr(), &mut n) },
            "vaQueryConfigProfiles",
        )?;
        out.truncate(n.max(0) as usize);
        Ok(out)
    }

    fn entrypoints(&self, profile: VAProfile) -> Result<Vec<VAEntrypoint>, String> {
        // SAFETY: valid display.
        let max = unsafe { (self.va.max_num_entrypoints)(self.dpy) }.max(0);
        let mut out = vec![0; max as usize];
        let mut n = 0;
        // SAFETY: buffer of vaMaxNumEntrypoints entries.
        self.check(
            unsafe {
                (self.va.query_config_entrypoints)(self.dpy, profile, out.as_mut_ptr(), &mut n)
            },
            "vaQueryConfigEntrypoints",
        )?;
        out.truncate(n.max(0) as usize);
        Ok(out)
    }

    /// The values of `types` (VA_ATTRIB_NOT_SUPPORTED where unsupported).
    fn attributes(
        &self,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        types: &[c_int],
    ) -> Result<Vec<u32>, String> {
        let mut attrs: Vec<VAConfigAttrib> = types
            .iter()
            .map(|&type_| VAConfigAttrib { type_, value: 0 })
            .collect();
        // SAFETY: array of `attrs.len()` attributes the driver fills in.
        self.check(
            unsafe {
                (self.va.get_config_attributes)(
                    self.dpy,
                    profile,
                    entrypoint,
                    attrs.as_mut_ptr(),
                    attrs.len() as c_int,
                )
            },
            "vaGetConfigAttributes",
        )?;
        Ok(attrs.into_iter().map(|a| a.value).collect())
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: we own the display; every object made on it is dropped
        // first (they hold an `Rc<Display>`).
        unsafe { (self.va.terminate)(self.dpy) };
    }
}

/// `VAConfigID`, destroyed on drop.
struct Config {
    d: Rc<Display>,
    id: VAConfigID,
}

impl Drop for Config {
    fn drop(&mut self) {
        // SAFETY: created on this display; its contexts are dropped first.
        unsafe { (self.d.va.destroy_config)(self.d.dpy, self.id) };
    }
}

/// `VAContextID`, destroyed on drop.
struct Context {
    d: Rc<Display>,
    id: VAContextID,
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: created on this display; its buffers are dropped first.
        unsafe { (self.d.va.destroy_context)(self.d.dpy, self.id) };
    }
}

/// A group of `VASurfaceID`s, destroyed together on drop.
struct Surfaces {
    d: Rc<Display>,
    ids: Vec<VASurfaceID>,
}

impl Drop for Surfaces {
    fn drop(&mut self) {
        // SAFETY: created on this display; contexts using them are gone.
        unsafe {
            (self.d.va.destroy_surfaces)(self.d.dpy, self.ids.as_mut_ptr(), self.ids.len() as c_int)
        };
    }
}

/// `VABufferID`, destroyed on drop. libva ≥ 2.0 never destroys parameter
/// buffers on `vaRenderPicture`, so every per-picture buffer is one of these.
struct Buffer {
    d: Rc<Display>,
    id: VABufferID,
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: created on this display.
        unsafe { (self.d.va.destroy_buffer)(self.d.dpy, self.id) };
    }
}

fn create_config(
    d: &Rc<Display>,
    profile: VAProfile,
    entrypoint: VAEntrypoint,
    attrs: &mut [VAConfigAttrib],
) -> Result<Config, String> {
    let mut id = VA_INVALID_ID;
    // SAFETY: attribute array and out-pointer are valid for the call.
    d.check(
        unsafe {
            (d.va.create_config)(
                d.dpy,
                profile,
                entrypoint,
                attrs.as_mut_ptr(),
                attrs.len() as c_int,
                &mut id,
            )
        },
        "vaCreateConfig",
    )?;
    Ok(Config { d: d.clone(), id })
}

fn create_surfaces(
    d: &Rc<Display>,
    rt_format: u32,
    fourcc: u32,
    width: u32,
    height: u32,
    count: usize,
) -> Result<Surfaces, String> {
    let mut ids = vec![VA_INVALID_SURFACE; count];
    let mut attr = [VASurfaceAttrib::pixel_format(fourcc)];
    // SAFETY: `ids` holds `count` slots; one attribute.
    d.check(
        unsafe {
            (d.va.create_surfaces)(
                d.dpy,
                rt_format,
                width,
                height,
                ids.as_mut_ptr(),
                count as c_uint,
                attr.as_mut_ptr(),
                1,
            )
        },
        "vaCreateSurfaces",
    )?;
    Ok(Surfaces { d: d.clone(), ids })
}

fn create_context(
    d: &Rc<Display>,
    config: &Config,
    width: u32,
    height: u32,
    targets: &[VASurfaceID],
) -> Result<Context, String> {
    let mut targets = targets.to_vec();
    let mut id = VA_INVALID_ID;
    // SAFETY: target list and out-pointer valid for the call.
    d.check(
        unsafe {
            (d.va.create_context)(
                d.dpy,
                config.id,
                width as c_int,
                height as c_int,
                VA_PROGRESSIVE,
                targets.as_mut_ptr(),
                targets.len() as c_int,
                &mut id,
            )
        },
        "vaCreateContext",
    )?;
    Ok(Context { d: d.clone(), id })
}

/// A buffer initialised from `size` bytes at `data` (copied by libva), or
/// uninitialised when `data` is null (the coded buffer).
fn create_buffer_raw(
    ctx: &Context,
    type_: VABufferType,
    size: usize,
    data: *const c_void,
) -> Result<Buffer, String> {
    let d = &ctx.d;
    let mut id = VA_INVALID_ID;
    // SAFETY: `data` is null or points at `size` readable bytes (callers);
    // vaCreateBuffer copies them before returning.
    d.check(
        unsafe {
            (d.va.create_buffer)(
                d.dpy,
                ctx.id,
                type_,
                size as c_uint,
                1,
                data as *mut c_void,
                &mut id,
            )
        },
        "vaCreateBuffer",
    )?;
    Ok(Buffer { d: d.clone(), id })
}

fn create_buffer<T>(ctx: &Context, type_: VABufferType, value: &T) -> Result<Buffer, String> {
    create_buffer_raw(
        ctx,
        type_,
        std::mem::size_of::<T>(),
        (value as *const T).cast(),
    )
}

/// `vaBeginPicture` / `vaRenderPicture` / `vaEndPicture` on `target`.
fn render(ctx: &Context, target: VASurfaceID, buffers: &[Buffer]) -> Result<(), String> {
    let d = &ctx.d;
    let mut ids: Vec<VABufferID> = buffers.iter().map(|b| b.id).collect();
    // SAFETY: context, target and buffers all belong to this display.
    unsafe {
        d.check(
            (d.va.begin_picture)(d.dpy, ctx.id, target),
            "vaBeginPicture",
        )?;
        let rendered = d.check(
            (d.va.render_picture)(d.dpy, ctx.id, ids.as_mut_ptr(), ids.len() as c_int),
            "vaRenderPicture",
        );
        // End the picture even when rendering failed, or the context stays
        // mid-picture and every later frame fails too.
        let ended = d.check((d.va.end_picture)(d.dpy, ctx.id), "vaEndPicture");
        rendered.and(ended)
    }
}

/// One CPU plane to copy: bytes, their pitch, bytes per row, rows.
struct PlaneCopy<'a> {
    data: &'a [u8],
    stride: usize,
    row_bytes: usize,
    rows: usize,
}

/// Copy `planes` into a mapped `VAImage`, bounds-checked against its layout.
fn write_image(d: &Display, image: &VAImage, planes: &[PlaneCopy<'_>]) -> Result<(), String> {
    if (image.num_planes as usize) < planes.len() {
        return Err(format!(
            "image has {} planes, {} needed",
            image.num_planes,
            planes.len()
        ));
    }
    let mut base: *mut c_void = std::ptr::null_mut();
    // SAFETY: the image's buffer belongs to this display.
    d.check(
        unsafe { (d.va.map_buffer)(d.dpy, image.buf, &mut base) },
        "vaMapBuffer(image)",
    )?;
    let mut result = Ok(());
    for (i, p) in planes.iter().enumerate() {
        let (offset, pitch) = (image.offsets[i] as usize, image.pitches[i] as usize);
        let row = p.row_bytes;
        // A row wider than the image pitch (or than the source stride) is a
        // geometry mismatch: refuse it rather than copy a cut-down picture.
        if row > pitch || row > p.stride {
            result = Err(format!(
                "plane {i}: {row}-byte rows do not fit (image pitch {pitch}, source stride {})",
                p.stride
            ));
            break;
        }
        let fits_dst =
            p.rows == 0 || offset + pitch * (p.rows - 1) + row <= image.data_size as usize;
        let fits_src = p.rows == 0 || p.stride * (p.rows - 1) + row <= p.data.len();
        if !(fits_dst && fits_src) {
            result = Err(format!("plane {i} does not fit the image layout"));
            break;
        }
        for r in 0..p.rows {
            let src = &p.data[r * p.stride..r * p.stride + row];
            // SAFETY: bounds checked above against the mapped `data_size`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.as_ptr(),
                    (base as *mut u8).add(offset + r * pitch),
                    row,
                )
            };
        }
    }
    // SAFETY: mapped above.
    let unmapped = d.check(
        unsafe { (d.va.unmap_buffer)(d.dpy, image.buf) },
        "vaUnmapBuffer(image)",
    );
    result.and(unmapped)
}

/// `VAImageFormat` for `vaCreateImage` (the put-image fallback).
fn image_format(fourcc: u32) -> VAImageFormat {
    let rgb = fourcc != VA_FOURCC_NV12;
    VAImageFormat {
        fourcc,
        byte_order: 1, // VA_LSB_FIRST
        bits_per_pixel: if rgb { 32 } else { 12 },
        depth: match fourcc {
            VA_FOURCC_BGRA => 32,
            VA_FOURCC_BGRX => 24,
            _ => 0,
        },
        red_mask: if rgb { 0x00ff_0000 } else { 0 },
        green_mask: if rgb { 0x0000_ff00 } else { 0 },
        blue_mask: if rgb { 0x0000_00ff } else { 0 },
        alpha_mask: if fourcc == VA_FOURCC_BGRA {
            0xff00_0000
        } else {
            0
        },
        va_reserved: [0; 4],
    }
}

/// Upload CPU planes into `surface` (`width`×`height`, `fourcc`). Tries
/// `vaDeriveImage` (one copy, straight into the surface) and falls back to
/// `vaCreateImage` + `vaPutImage` for drivers/surfaces that refuse to derive;
/// `derive` remembers a refusal so it is not retried every frame.
fn upload(
    d: &Display,
    surface: VASurfaceID,
    fourcc: u32,
    width: u32,
    height: u32,
    planes: &[PlaneCopy<'_>],
    derive: &mut bool,
) -> Result<(), String> {
    if *derive {
        let mut image = VAImage::zeroed();
        // SAFETY: valid surface; out-struct.
        let status = unsafe { (d.va.derive_image)(d.dpy, surface, &mut image) };
        if status == VA_STATUS_SUCCESS {
            let written = if image.format.fourcc == fourcc {
                write_image(d, &image, planes)
            } else {
                Err("derived image has another layout".into())
            };
            // SAFETY: derived above.
            unsafe { (d.va.destroy_image)(d.dpy, image.image_id) };
            if written.is_ok() {
                return written;
            }
        }
        tracing::debug!("VA-API: vaDeriveImage unusable here — uploading through vaPutImage");
        *derive = false;
    }
    let mut format = image_format(fourcc);
    let mut image = VAImage::zeroed();
    // SAFETY: format and out-struct valid for the call.
    d.check(
        unsafe {
            (d.va.create_image)(
                d.dpy,
                &mut format,
                width as c_int,
                height as c_int,
                &mut image,
            )
        },
        "vaCreateImage",
    )?;
    let result = write_image(d, &image, planes).and_then(|()| {
        // SAFETY: image and surface on this display, same size.
        d.check(
            unsafe {
                (d.va.put_image)(
                    d.dpy,
                    surface,
                    image.image_id,
                    0,
                    0,
                    width,
                    height,
                    0,
                    0,
                    width,
                    height,
                )
            },
            "vaPutImage",
        )
    });
    // SAFETY: created above.
    unsafe { (d.va.destroy_image)(d.dpy, image.image_id) };
    result
}

/// The video processor: BGRA/NV12 of any size → the NV12 encode surface.
struct Vpp {
    ctx: Context,
    _config: Config,
    /// BGRA if the driver's VPP reads it, else BGRX (alpha is unused).
    rgb_fourcc: u32,
}

/// The surface CPU frames are uploaded to when they need VPP.
struct Source {
    surface: Surfaces,
    fourcc: u32,
    width: u32,
    height: u32,
}

pub(crate) struct VaapiEncoder {
    // Field order is drop order: buffers → contexts → surfaces → configs →
    // display (each holds an `Rc<Display>`; the display goes last anyway).
    coded: Buffer,
    enc_ctx: Context,
    vpp: Option<Vpp>,
    source: Option<Source>,
    input: Surfaces,
    recon: Surfaces,
    _enc_config: Config,
    display: Rc<Display>,
    header: SeqHeader,
    /// Annex-B parameter sets used to repair a keyframe that lacks them.
    /// Ours (SPS + PPS) when the driver takes both as packed headers; else
    /// the driver writes some or all itself, and ours could duplicate or
    /// contradict them, so the ones from its first complete IDR replace ours
    /// (`capture_headers`) — ours stay only as the fallback until then.
    headers: Vec<u8>,
    capture_headers: bool,
    sps: Vec<u8>,
    pps: Vec<u8>,
    packed_headers: u32,
    cfg: EncoderConfig,
    derive: bool,
    force_idr: bool,
    have_ref: bool,
    /// Index into `recon` of the picture being reconstructed now.
    cur: usize,
    frames_since_idr: u32,
    idr_pic_id: u16,
    /// Reference picture (the previous reconstruction) for the next P frame.
    reference: VAPictureH264,
}

impl VaapiEncoder {
    pub(crate) fn new(probe: &Probe, cfg: EncoderConfig) -> Result<Self, CodecError> {
        if cfg.codec != Codec::H264 {
            return Err(CodecError::NoEncoder(cfg.codec));
        }
        validate(&cfg)?;
        Self::create(probe, cfg).map_err(|e| CodecError::Backend(format!("VA-API: {e}")))
    }

    fn create(probe: &Probe, cfg: EncoderConfig) -> Result<Self, String> {
        let va = Va::shared()?;
        let display = Rc::new(Display::open(&va, &probe.node)?);
        let header = SeqHeader::new(
            probe.profile,
            cfg.width,
            cfg.height,
            cfg.fps_num,
            cfg.fps_den,
        );
        let mut attrs = vec![
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: VA_RC_CBR,
            },
        ];
        if probe.packed_headers != 0 {
            attrs.push(VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: probe.packed_headers,
            });
        }
        let enc_config = create_config(&display, probe.va_profile, probe.entrypoint, &mut attrs)?;
        let (max_w, max_h) = max_surface_size(&display, &enc_config)?;
        if cfg.width > max_w || cfg.height > max_h {
            return Err(format!(
                "{} encodes up to {max_w}x{max_h}, not {}x{}",
                probe.vendor, cfg.width, cfg.height
            ));
        }
        let input = create_surfaces(
            &display,
            VA_RT_FORMAT_YUV420,
            VA_FOURCC_NV12,
            cfg.width,
            cfg.height,
            1,
        )?;
        let recon = create_surfaces(
            &display,
            VA_RT_FORMAT_YUV420,
            VA_FOURCC_NV12,
            cfg.width,
            cfg.height,
            2,
        )?;
        let targets: Vec<VASurfaceID> = input.ids.iter().chain(&recon.ids).copied().collect();
        let enc_ctx = create_context(&display, &enc_config, cfg.width, cfg.height, &targets)?;
        // Raw 4:2:0 size + headroom bounds any picture this encoder makes (the
        // QP floor keeps them far smaller); overflow is detected per frame.
        let coded_size = cfg.width as usize * cfg.height as usize * 3 / 2 + (64 << 10);
        let coded = create_buffer_raw(
            &enc_ctx,
            VA_ENC_CODED_BUFFER_TYPE,
            coded_size,
            std::ptr::null(),
        )?;
        let sps = header.sps_nal();
        let pps = header.pps_nal();
        tracing::info!(
            "VA-API encoder: {} — level {}.{} {}x{} @{}/{} fps, {} kbps CBR, max QP {MAX_QP}, packed headers {:#x}",
            probe.describe(),
            header.level_idc / 10,
            header.level_idc % 10,
            cfg.width,
            cfg.height,
            cfg.fps_num,
            cfg.fps_den,
            cfg.bitrate_bps / 1000,
            probe.packed_headers
        );
        Ok(Self {
            coded,
            enc_ctx,
            vpp: None,
            source: None,
            input,
            recon,
            _enc_config: enc_config,
            display,
            header,
            headers: [sps.as_slice(), pps.as_slice()].concat(),
            capture_headers: probe.packed_headers
                != (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE),
            sps,
            pps,
            packed_headers: probe.packed_headers,
            cfg,
            derive: true,
            force_idr: true,
            have_ref: false,
            cur: 0,
            frames_since_idr: 0,
            idr_pic_id: 0,
            reference: VAPictureH264::INVALID,
        })
    }

    /// The video processor, created on first need (an NV12 feed at the
    /// encode size never needs it).
    fn vpp(&mut self) -> Result<&Vpp, String> {
        if self.vpp.is_none() {
            let d = &self.display;
            let config = create_config(d, VA_PROFILE_NONE, VA_ENTRYPOINT_VIDEO_PROC, &mut [])
                .map_err(|e| format!("no video processor for BGRA → NV12 ({e})"))?;
            let formats = surface_formats(d, &config)?;
            let rgb_fourcc = [VA_FOURCC_BGRA, VA_FOURCC_BGRX]
                .into_iter()
                .find(|f| formats.contains(f))
                .ok_or("the video processor reads neither BGRA nor BGRX")?;
            let ctx = create_context(d, &config, self.cfg.width, self.cfg.height, &self.input.ids)?;
            self.vpp = Some(Vpp {
                ctx,
                _config: config,
                rgb_fourcc,
            });
        }
        self.vpp
            .as_ref()
            .ok_or_else(|| "video processor missing".into())
    }

    /// Get the frame's pixels into the NV12 encode surface.
    fn stage(&mut self, frame: &Frame<'_>) -> Result<(), CodecError> {
        let FrameData::Cpu {
            format,
            plane0,
            plane1,
        } = frame.data
        else {
            return Err(CodecError::Unsupported(
                "the VA-API encoder takes CPU frames in this version (DMA-BUF import is task L3b)"
                    .into(),
            ));
        };
        let backend = |e: String| CodecError::Backend(format!("VA-API: {e}"));
        let (w, h) = (frame.width, frame.height);
        let planes: Vec<PlaneCopy<'_>> = match format {
            PixelFormat::Bgra8 => vec![PlaneCopy {
                data: plane0.data,
                stride: plane0.stride,
                row_bytes: w as usize * 4,
                rows: h as usize,
            }],
            PixelFormat::Nv12 => {
                let uv = plane1.ok_or_else(|| {
                    CodecError::Unsupported("NV12 frame without a UV plane".into())
                })?;
                vec![
                    PlaneCopy {
                        data: plane0.data,
                        stride: plane0.stride,
                        row_bytes: w as usize,
                        rows: h as usize,
                    },
                    PlaneCopy {
                        data: uv.data,
                        stride: uv.stride,
                        row_bytes: w.div_ceil(2) as usize * 2,
                        rows: h.div_ceil(2) as usize,
                    },
                ]
            }
        };
        // `Frame` only debug-asserts its plane sizes; refuse a short plane
        // here as the caller's error, before it can look like a driver one.
        for (i, p) in planes.iter().enumerate() {
            let fits = p.stride >= p.row_bytes
                && (p.rows == 0 || p.stride * (p.rows - 1) + p.row_bytes <= p.data.len());
            if !fits {
                return Err(CodecError::Unsupported(format!(
                    "VA-API: plane {i} of a {w}x{h} frame is {} bytes at stride {} — too small",
                    p.data.len(),
                    p.stride
                )));
            }
        }
        if format == PixelFormat::Nv12 && (w, h) == (self.cfg.width, self.cfg.height) {
            let target = self.input.ids[0];
            return upload(
                &self.display,
                target,
                VA_FOURCC_NV12,
                w,
                h,
                &planes,
                &mut self.derive,
            )
            .map_err(backend);
        }

        let rgb_fourcc = self.vpp().map_err(backend)?.rgb_fourcc;
        let (rt, fourcc, standard, range) = match format {
            PixelFormat::Bgra8 => (
                VA_RT_FORMAT_RGB32,
                rgb_fourcc,
                VA_PROC_COLOR_STANDARD_SRGB,
                VA_SOURCE_RANGE_FULL,
            ),
            PixelFormat::Nv12 => (
                VA_RT_FORMAT_YUV420,
                VA_FOURCC_NV12,
                VA_PROC_COLOR_STANDARD_BT709,
                VA_SOURCE_RANGE_REDUCED,
            ),
        };
        if !self
            .source
            .as_ref()
            .is_some_and(|s| (s.fourcc, s.width, s.height) == (fourcc, w, h))
        {
            self.source = None;
            self.source = Some(Source {
                surface: create_surfaces(&self.display, rt, fourcc, w, h, 1).map_err(backend)?,
                fourcc,
                width: w,
                height: h,
            });
            // A new surface kind: give deriving another chance.
            self.derive = true;
        }
        let source = self
            .source
            .as_ref()
            .map(|s| s.surface.ids[0])
            .ok_or_else(|| backend("source surface missing".into()))?;
        upload(
            &self.display,
            source,
            fourcc,
            w,
            h,
            &planes,
            &mut self.derive,
        )
        .map_err(backend)?;

        let mut pipeline = VAProcPipelineParameterBuffer::zeroed();
        pipeline.surface = source;
        // Null regions = whole surfaces: VPP scales the full frame to the full
        // encode surface (the engine keeps the aspect ratio).
        pipeline.surface_color_standard = standard;
        pipeline.output_color_standard = VA_PROC_COLOR_STANDARD_BT709;
        pipeline.filter_flags = VA_FILTER_SCALING_HQ;
        pipeline.input_color_properties.color_range = range;
        // The presenter's shader expects BT.709 limited range (frame.rs).
        pipeline.output_color_properties.color_range = VA_SOURCE_RANGE_REDUCED;
        let vpp = self
            .vpp
            .as_ref()
            .ok_or_else(|| backend("video processor missing".into()))?;
        let buf = create_buffer(&vpp.ctx, VA_PROC_PIPELINE_PARAMETER_BUFFER_TYPE, &pipeline)
            .map_err(backend)?;
        render(&vpp.ctx, self.input.ids[0], &[buf]).map_err(backend)
    }

    /// The parameter buffers for one picture, in FFmpeg's submission order.
    fn picture_buffers(&self, idr: bool, frame_num: u32, poc: i32) -> Result<Vec<Buffer>, String> {
        let ctx = &self.enc_ctx;
        let h = &self.header;
        let bitrate = self.cfg.bitrate_bps;
        let vbv = vbv_bits(bitrate, self.cfg.fps_num, self.cfg.fps_den);
        let mut out = Vec::with_capacity(10);

        if idr {
            let mut seq = VAEncSequenceParameterBufferH264::zeroed();
            seq.seq_parameter_set_id = 0;
            seq.level_idc = h.level_idc;
            seq.intra_period = GOP_HINT;
            seq.intra_idr_period = GOP_HINT;
            seq.ip_period = 1;
            seq.bits_per_second = bitrate;
            seq.max_num_ref_frames = 1;
            seq.picture_width_in_mbs = h.width_mbs() as u16;
            seq.picture_height_in_mbs = h.height_mbs() as u16;
            seq.seq_fields = (1 << SEQ_CHROMA_FORMAT_IDC_SHIFT)
                | SEQ_FRAME_MBS_ONLY
                | SEQ_DIRECT_8X8_INFERENCE
                | ((LOG2_MAX_FRAME_NUM - 4) << SEQ_LOG2_MAX_FRAME_NUM_MINUS4_SHIFT)
                | (2 << SEQ_PIC_ORDER_CNT_TYPE_SHIFT);
            seq.frame_cropping_flag = h.cropped() as u8;
            seq.frame_crop_right_offset = h.crop_right();
            seq.frame_crop_bottom_offset = h.crop_bottom();
            seq.vui_parameters_present_flag = 1;
            seq.vui_fields = VUI_TIMING_INFO_PRESENT
                | VUI_BITSTREAM_RESTRICTION
                | (15 << VUI_LOG2_MAX_MV_LENGTH_HORIZONTAL_SHIFT)
                | (15 << VUI_LOG2_MAX_MV_LENGTH_VERTICAL_SHIFT)
                | VUI_MOTION_VECTORS_OVER_PIC_BOUNDARIES;
            seq.num_units_in_tick = h.num_units_in_tick();
            seq.time_scale = h.time_scale();
            out.push(create_buffer(
                ctx,
                VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE,
                &seq,
            )?);
        }

        let mut rc = Misc {
            type_: VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL,
            data: VAEncMiscParameterRateControl::zeroed(),
        };
        rc.data.bits_per_second = bitrate;
        rc.data.target_percentage = 100;
        // The averaging window = the HRD buffer in ms (FFmpeg's rule): the
        // tighter the window, the less a burst may run ahead of the link.
        rc.data.window_size = ((vbv as u64 * 1000) / bitrate.max(1) as u64).max(1) as u32;
        rc.data.max_qp = MAX_QP;
        // No skipped frames (the engine paces) and no filler bits on a static
        // screen (CBR stuffing would spend the whole budget on nothing).
        rc.data.rc_flags = RC_DISABLE_FRAME_SKIP | RC_DISABLE_BIT_STUFFING;
        out.push(create_buffer(ctx, VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &rc)?);

        let mut hrd = Misc {
            type_: VA_ENC_MISC_PARAMETER_TYPE_HRD,
            data: VAEncMiscParameterHRD::zeroed(),
        };
        hrd.data.buffer_size = vbv;
        hrd.data.initial_buffer_fullness = vbv;
        out.push(create_buffer(ctx, VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &hrd)?);

        let mut fr = Misc {
            type_: VA_ENC_MISC_PARAMETER_TYPE_FRAME_RATE,
            data: VAEncMiscParameterFrameRate::zeroed(),
        };
        fr.data.framerate = pack_framerate(self.cfg.fps_num, self.cfg.fps_den);
        out.push(create_buffer(ctx, VA_ENC_MISC_PARAMETER_BUFFER_TYPE, &fr)?);

        let curr = VAPictureH264 {
            picture_id: self.recon.ids[self.cur],
            frame_idx: frame_num,
            flags: 0,
            top_field_order_cnt: poc,
            bottom_field_order_cnt: poc,
            va_reserved: [0; 4],
        };
        let mut pic = VAEncPictureParameterBufferH264::zeroed();
        pic.curr_pic = curr;
        pic.reference_frames = [VAPictureH264::INVALID; 16];
        if !idr {
            pic.reference_frames[0] = self.reference;
        }
        pic.coded_buf = self.coded.id;
        pic.frame_num = frame_num as u16;
        pic.pic_init_qp = 26;
        pic.pic_fields = (idr as u32 * PIC_IDR)
            | (1 << PIC_REFERENCE_SHIFT)
            | if h.profile.cabac() {
                PIC_ENTROPY_CODING_MODE
            } else {
                0
            }
            | PIC_DEBLOCKING_FILTER_CONTROL_PRESENT;
        out.push(create_buffer(
            ctx,
            VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE,
            &pic,
        )?);

        if idr {
            for (bit, kind, nal) in [
                (
                    VA_ENC_PACKED_HEADER_SEQUENCE,
                    VA_ENC_PACKED_HEADER_TYPE_SEQUENCE,
                    &self.sps,
                ),
                (
                    VA_ENC_PACKED_HEADER_PICTURE,
                    VA_ENC_PACKED_HEADER_TYPE_PICTURE,
                    &self.pps,
                ),
            ] {
                if self.packed_headers & bit == 0 {
                    continue;
                }
                let mut param = VAEncPackedHeaderParameterBuffer::zeroed();
                param.type_ = kind;
                param.bit_length = (nal.len() * 8) as u32;
                // `h264::nal` already inserted emulation-prevention bytes.
                param.has_emulation_bytes = 1;
                out.push(create_buffer(
                    ctx,
                    VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE,
                    &param,
                )?);
                out.push(create_buffer_raw(
                    ctx,
                    VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
                    nal.len(),
                    nal.as_ptr().cast(),
                )?);
            }
        }

        let mut slice = VAEncSliceParameterBufferH264::zeroed();
        slice.macroblock_address = 0;
        slice.num_macroblocks = h.macroblocks();
        slice.macroblock_info = VA_INVALID_ID;
        // H.264 slice_type: 0 = P, 2 = I.
        slice.slice_type = if idr { 2 } else { 0 };
        slice.idr_pic_id = self.idr_pic_id;
        slice.ref_pic_list0 = [VAPictureH264::INVALID; 32];
        slice.ref_pic_list1 = [VAPictureH264::INVALID; 32];
        if !idr {
            slice.ref_pic_list0[0] = self.reference;
        }
        out.push(create_buffer(
            ctx,
            VA_ENC_SLICE_PARAMETER_BUFFER_TYPE,
            &slice,
        )?);
        Ok(out)
    }

    /// Encode the staged NV12 surface as one access unit.
    fn encode_staged(&mut self) -> Result<EncodedUnit, String> {
        let idr = self.force_idr || !self.have_ref;
        if idr {
            self.frames_since_idr = 0;
        }
        let frame_num = self.frames_since_idr % MAX_FRAME_NUM;
        // pic_order_cnt_type 2: POC = 2 × frames since the IDR. Kept below
        // 2^31; the wrap after 2^30 frames (> 200 days at 60 fps) is harmless
        // for a P-only stream, whose decoders derive POC themselves.
        let poc = (2 * (self.frames_since_idr % (1 << 30))) as i32;
        let buffers = self.picture_buffers(idr, frame_num, poc)?;
        let input = self.input.ids[0];
        render(&self.enc_ctx, input, &buffers)?;
        drop(buffers);
        let d = &self.display;
        // SAFETY: surface on this display.
        d.check(
            unsafe { (d.va.sync_surface)(d.dpy, input) },
            "vaSyncSurface",
        )?;

        let mut seg: *mut c_void = std::ptr::null_mut();
        // SAFETY: the coded buffer belongs to this display; mapping waits for
        // the encode to finish.
        d.check(
            unsafe { (d.va.map_buffer)(d.dpy, self.coded.id, &mut seg) },
            "vaMapBuffer(coded)",
        )?;
        let mut data = Vec::new();
        let mut overflow = false;
        let mut seg = seg as *const VACodedBufferSegment;
        while !seg.is_null() {
            // SAFETY: libva hands back a NULL-terminated list of segments, each
            // describing `size` valid bytes at `buf`, until the unmap below.
            let s = unsafe { &*seg };
            overflow |= s.status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0;
            if !s.buf.is_null() && s.size > 0 {
                // SAFETY: as above.
                data.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(s.buf as *const u8, s.size as usize)
                });
            }
            seg = s.next as *const VACodedBufferSegment;
        }
        // SAFETY: mapped above.
        d.check(
            unsafe { (d.va.unmap_buffer)(d.dpy, self.coded.id) },
            "vaUnmapBuffer(coded)",
        )?;
        if overflow {
            // A truncated picture would corrupt every frame predicted from it.
            self.force_idr = true;
            self.have_ref = false;
            return Err("coded buffer overflow — frame dropped, next one is an IDR".into());
        }

        // This reconstruction is the next P frame's only reference.
        self.reference = VAPictureH264 {
            picture_id: self.recon.ids[self.cur],
            frame_idx: frame_num,
            flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            top_field_order_cnt: poc,
            bottom_field_order_cnt: poc,
            va_reserved: [0; 4],
        };
        self.cur ^= 1;
        self.have_ref = true;
        self.frames_since_idr = self.frames_since_idr.wrapping_add(1);
        if idr {
            self.force_idr = false;
            // Consecutive IDRs must differ in idr_pic_id (H.264 §7.4.3).
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
        }
        // The bitstream is the truth the viewer will see; a driver that ignored
        // the IDR request would otherwise be reported as a keyframe.
        let keyframe = annexb::is_idr(Codec::H264, &data);
        if keyframe != idr && !data.is_empty() {
            tracing::warn!("VA-API: asked for idr={idr}, the driver produced idr={keyframe}");
        }
        if keyframe && self.capture_headers {
            let found = annexb::parameter_sets(Codec::H264, &data);
            if annexb::has_parameter_sets(Codec::H264, &found) {
                tracing::debug!("VA-API: using the driver's own SPS/PPS for keyframe repair");
                self.headers = found;
                self.capture_headers = false;
            } else {
                tracing::warn!(
                    "VA-API: the driver's IDR carries no complete SPS/PPS — adding ours"
                );
            }
        }
        let data = if keyframe {
            annexb::with_parameter_sets(Codec::H264, data, &self.headers)
        } else {
            data
        };
        Ok(EncodedUnit { data, keyframe })
    }
}

impl VideoEncoder for VaapiEncoder {
    fn encode(&mut self, frame: &Frame<'_>) -> Result<Vec<EncodedUnit>, CodecError> {
        self.stage(frame)?;
        let unit = self
            .encode_staged()
            .map_err(|e| CodecError::Backend(format!("VA-API: {e}")))?;
        Ok(if unit.data.is_empty() {
            Vec::new()
        } else {
            vec![unit]
        })
    }

    /// Takes effect on the next picture: its rate-control buffer carries the
    /// new target and the driver re-plans from there (no IDR, no reset).
    fn set_bitrate(&mut self, bps: u32) -> Result<(), CodecError> {
        if bps == 0 {
            return Err(CodecError::Unsupported("VA-API: bitrate 0".into()));
        }
        self.cfg.bitrate_bps = bps;
        Ok(())
    }

    fn force_keyframe(&mut self) {
        self.force_idr = true;
    }
}

/// `VASurfaceAttribMaxWidth/MaxHeight` of `config` (encode limits).
fn max_surface_size(d: &Display, config: &Config) -> Result<(u32, u32), String> {
    let attrs = surface_attributes(d, config)?;
    let get = |t: u32| {
        attrs
            .iter()
            .find(|a| a.type_ == t)
            // SAFETY: integer-typed attribute (VA documents these as int).
            .map(|a| unsafe { a.value.value.i }.max(0) as u32)
    };
    // A driver that reports no limit is left to refuse the size itself.
    Ok((
        get(VA_SURFACE_ATTRIB_MAX_WIDTH).unwrap_or(u32::MAX),
        get(VA_SURFACE_ATTRIB_MAX_HEIGHT).unwrap_or(u32::MAX),
    ))
}

/// Pixel formats `config` accepts as surfaces.
fn surface_formats(d: &Display, config: &Config) -> Result<Vec<u32>, String> {
    Ok(surface_attributes(d, config)?
        .iter()
        .filter(|a| a.type_ == VA_SURFACE_ATTRIB_PIXEL_FORMAT)
        // SAFETY: PixelFormat attributes are integer-typed.
        .map(|a| unsafe { a.value.value.i } as u32)
        .collect())
}

fn surface_attributes(d: &Display, config: &Config) -> Result<Vec<VASurfaceAttrib>, String> {
    let mut n: c_uint = 0;
    // SAFETY: first call with a null list returns the count.
    d.check(
        unsafe { (d.va.query_surface_attributes)(d.dpy, config.id, std::ptr::null_mut(), &mut n) },
        "vaQuerySurfaceAttributes",
    )?;
    let mut attrs = vec![VASurfaceAttrib::pixel_format(0); n as usize];
    // SAFETY: buffer of `n` attributes.
    d.check(
        unsafe { (d.va.query_surface_attributes)(d.dpy, config.id, attrs.as_mut_ptr(), &mut n) },
        "vaQuerySurfaceAttributes",
    )?;
    attrs.truncate(n as usize);
    Ok(attrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_gate_is_libva_2_20() {
        assert!(!version_supported(1, 14)); // Ubuntu 22.04
        assert!(!version_supported(1, 19));
        assert!(version_supported(1, 20)); // Ubuntu 24.04
        assert!(version_supported(1, 22));
        assert!(version_supported(2, 0));
        assert!(!version_supported(0, 40));
    }

    #[test]
    fn low_power_entrypoint_preferred() {
        assert_eq!(
            pick_entrypoint(&[VA_ENTRYPOINT_ENC_SLICE, VA_ENTRYPOINT_ENC_SLICE_LP]),
            Some(VA_ENTRYPOINT_ENC_SLICE_LP)
        );
        assert_eq!(
            pick_entrypoint(&[1, VA_ENTRYPOINT_ENC_SLICE]),
            Some(VA_ENTRYPOINT_ENC_SLICE)
        );
        // VLD (1) only = a decoder.
        assert_eq!(pick_entrypoint(&[1]), None);
        assert_eq!(pick_entrypoint(&[]), None);
    }

    #[test]
    fn main_profile_preferred() {
        assert_eq!(
            h264_profiles(&[
                0,
                VA_PROFILE_H264_CONSTRAINED_BASELINE,
                VA_PROFILE_H264_MAIN,
                7
            ]),
            vec![
                (Profile::Main, VA_PROFILE_H264_MAIN),
                (
                    Profile::ConstrainedBaseline,
                    VA_PROFILE_H264_CONSTRAINED_BASELINE
                )
            ]
        );
        assert_eq!(
            h264_profiles(&[VA_PROFILE_H264_CONSTRAINED_BASELINE]),
            vec![(
                Profile::ConstrainedBaseline,
                VA_PROFILE_H264_CONSTRAINED_BASELINE
            )]
        );
        assert!(h264_profiles(&[17, 18]).is_empty()); // HEVC only
    }

    #[test]
    fn render_nodes_sorted_numerically() {
        let names = [
            "card0",
            "renderD129",
            "by-path",
            "renderD128",
            "renderDx",
            "renderD1000",
        ]
        .map(String::from);
        assert_eq!(
            render_nodes(names),
            vec!["renderD128", "renderD129", "renderD1000"]
        );
    }

    #[test]
    fn framerate_packing() {
        assert_eq!(pack_framerate(30, 1), 30 | (1 << 16));
        assert_eq!(pack_framerate(60_000, 1001), 60_000 | (1001 << 16));
        assert_eq!(pack_framerate(120_000, 1001), 60_000 | (500 << 16));
        assert_eq!(pack_framerate(0, 0), 1 | (1 << 16));
    }

    #[test]
    fn image_format_masks() {
        let f = image_format(VA_FOURCC_BGRA);
        assert_eq!(
            (f.bits_per_pixel, f.depth, f.alpha_mask),
            (32, 32, 0xff00_0000)
        );
        let f = image_format(VA_FOURCC_BGRX);
        assert_eq!((f.depth, f.alpha_mask, f.red_mask), (24, 0, 0x00ff_0000));
        let f = image_format(VA_FOURCC_NV12);
        assert_eq!((f.bits_per_pixel, f.red_mask), (12, 0));
    }

    /// Real encode on an Intel/AMD GPU. Run on the Linux test machine with
    /// `cargo test -p platform-linux -- --ignored vaapi --nocapture`
    /// (see plans/checklists/L3.md).
    #[test]
    #[ignore = "needs an Intel/AMD GPU"]
    fn vaapi_device_round_trip() {
        let probe = probe().expect("VA-API probe");
        eprintln!("VA-API probe: {}", probe.describe());
        let cfg = EncoderConfig {
            width: 1280,
            height: 720,
            fps_num: 30,
            fps_den: 1,
            bitrate_bps: 4_000_000,
            codec: Codec::H264,
        };
        let mut enc = VaapiEncoder::new(&probe, cfg).expect("encoder");
        let mut stream = Vec::new();
        let mut keys = Vec::new();
        for t in 0..10usize {
            if t == 5 {
                enc.force_keyframe();
            }
            if t == 7 {
                enc.set_bitrate(2_000_000).expect("bitrate");
            }
            // Capture-sized BGRA (larger than the encode size: VPP scales).
            let (w, h) = (1920usize, 1080usize);
            let px: Vec<u8> = (0..w * h * 4)
                .map(|i| ((i / 4 % w + t * 8) ^ (i / 4 / w)) as u8)
                .collect();
            let units = enc
                .encode(&Frame::bgra(w as u32, h as u32, &px, w * 4))
                .expect("encode");
            assert_eq!(units.len(), 1);
            if units[0].keyframe {
                keys.push(t);
                assert!(annexb::has_parameter_sets(Codec::H264, &units[0].data));
                assert!(annexb::is_idr(Codec::H264, &units[0].data));
            } else {
                assert!(!annexb::is_idr(Codec::H264, &units[0].data));
            }
            stream.extend_from_slice(&units[0].data);
        }
        assert_eq!(keys, vec![0, 5]);
        // NV12 at the encode size skips VPP.
        let y = vec![100u8; 1280 * 720];
        let uv = vec![128u8; 1280 * 360];
        let units = enc
            .encode(&Frame::nv12(1280, 720, &y, 1280, &uv, 1280))
            .expect("NV12 encode");
        stream.extend_from_slice(&units[0].data);
        if let Some(dir) = std::env::var_os("SCT_DUMP_STREAM") {
            std::fs::write(std::path::Path::new(&dir).join("vaapi.h264"), &stream).expect("dump");
        }
    }
}
